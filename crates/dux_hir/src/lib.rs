//! Name resolution: the pass DESIGN 6.1 places between the AST and the typed HIR.
//!
//! The resolver answers one question per identifier use: which declaration does
//! this name refer to? It reports what it can prove wrong — undeclared names,
//! duplicate definitions, calls with the wrong number of arguments, a missing
//! or malformed `main` — and deliberately stays silent about everything a
//! later milestone owns: fields and methods (M3), patterns beyond binding
//! names (M4), generics (M5), imported C namespaces (M6) and every type
//! annotation. Silence is the contract: no normative example may gain a
//! diagnostic here, because `--emit=ast` and `check` run on the whole corpus.

use dux_ast::{Block, Decl, Expr, FnDecl, ForKind, InterpolationPart, Program, Stmt, Type};
use dux_diag::{code, Diagnostic};
use dux_source::Span;
use std::collections::HashMap;

pub mod ty;

pub use ty::check;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Callable {
    Function { params: usize },
    Opaque,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Item {
    Namespace,
    Callable(Callable),
}

#[derive(Clone, Debug)]
pub struct Resolution {
    pub diagnostics: Vec<Diagnostic>,
}

pub fn resolve(program: &Program) -> Resolution {
    Resolver::new(program).run()
}

const UNIVERSE: &[&str] = &[
    "print", "println", "new", "make", "free", "del", "context", "Some", "None", "Ok", "Err",
];

const PRIMITIVES: &[&str] = &[
    "bool", "byte", "i8", "i16", "i32", "i64", "isize", "u8", "u16", "u32", "u64", "usize",
    "f32", "f64", "int", "uint", "long", "float", "rune", "char", "string", "rawptr",
];

struct Resolver<'a> {
    program: &'a Program,
    top_level: HashMap<String, Item>,
    scopes: Vec<Vec<String>>,
    diagnostics: Vec<Diagnostic>,
}

impl<'a> Resolver<'a> {
    fn new(program: &'a Program) -> Resolver<'a> {
        Resolver {
            program,
            top_level: HashMap::new(),
            scopes: Vec::new(),
            diagnostics: Vec::new(),
        }
    }

    fn run(mut self) -> Resolution {
        self.collect();
        self.check_main();
        for decl in &self.program.decls {
            match decl {
                Decl::Fn(function) => self.function(function),
                Decl::Impl(impl_block) => {
                    for method in &impl_block.methods {
                        self.function(method);
                    }
                }
                Decl::Const(decl) => self.expr(&decl.value),
                Decl::Global(decl) => {
                    if let Some(value) = &decl.value {
                        self.expr(value);
                    }
                }
                _ => {}
            }
        }
        Resolution {
            diagnostics: self.diagnostics,
        }
    }

    fn collect(&mut self) {
        for name in UNIVERSE {
            self.top_level
                .insert(name.to_string(), Item::Callable(Callable::Opaque));
        }
        for name in PRIMITIVES {
            self.top_level.insert(name.to_string(), Item::Namespace);
        }
        for import in &self.program.imports {
            if let Some(namespace) = import.path.rsplit('/').next() {
                self.declare(namespace, Item::Namespace);
            }
        }
        for decl in &self.program.decls {
            match decl {
                Decl::Fn(function) => self.declare(
                    &function.name.name,
                    Item::Callable(Callable::Function {
                        params: function.params.len(),
                    }),
                ),
                Decl::Extern(block) => {
                    for function in &block.functions {
                        self.declare(&function.name.name, Item::Callable(Callable::Opaque));
                    }
                }
                Decl::Struct(decl) => self.declare(&decl.name.name, Item::Namespace),
                Decl::Enum(decl) => self.declare(&decl.name.name, Item::Namespace),
                Decl::TypeAlias(decl) => self.declare(&decl.name.name, Item::Namespace),
                Decl::Module(decl) => self.declare(&decl.name.name, Item::Namespace),
                Decl::Const(decl) => {
                    self.declare(&decl.name.name, Item::Callable(Callable::Opaque))
                }
                Decl::Global(decl) => {
                    self.declare(&decl.name.name, Item::Callable(Callable::Opaque))
                }
                Decl::Impl(_) | Decl::Error { .. } => {}
            }
        }
    }

    fn declare(&mut self, name: &str, item: Item) {
        if name == "_" {
            return;
        }
        if self.top_level.insert(name.to_string(), item).is_some()
            && !UNIVERSE.contains(&name)
            && !PRIMITIVES.contains(&name)
        {
            self.diagnostics.push(Diagnostic::error(
                code::DUPLICATE_DEFINITION,
                format!("`{name}` is defined more than once at the top level"),
            ));
        }
    }

    fn check_main(&mut self) {
        let Some(main) = self.program.find_function("main") else {
            self.diagnostics.push(
                Diagnostic::error(code::NO_MAIN, "no `fn main()` in this program")
                    .with_note("the entry point is `fn main()` in the package named `main`"),
            );
            return;
        };
        if !main.params.is_empty() {
            self.diagnostics.push(
                Diagnostic::error(code::BAD_MAIN_SIGNATURE, "`fn main()` takes no parameters")
                    .with_span(main.name.span),
            );
        }
        if let Some(return_ty) = &main.return_ty {
            let ok = matches!(return_ty, Type::Named { name, .. } if name == "i32" || name == "i64");
            if !ok {
                self.diagnostics.push(
                    Diagnostic::error(
                        code::BAD_MAIN_SIGNATURE,
                        "`fn main()` returns nothing or `i32`",
                    )
                    .with_span(return_ty.span()),
                );
            }
        }
    }

    fn function(&mut self, function: &FnDecl) {
        self.scopes.push(Vec::new());
        for parameter in &function.type_params {
            self.bind(&parameter.name.name);
        }
        for (index, parameter) in function.params.iter().enumerate() {
            if function.params[..index]
                .iter()
                .any(|earlier| earlier.name.name == parameter.name.name)
            {
                self.diagnostics.push(
                    Diagnostic::error(
                        code::DUPLICATE_DEFINITION,
                        format!("parameter `{}` is declared twice", parameter.name.name),
                    )
                    .with_span(parameter.name.span),
                );
            }
            self.bind(&parameter.name.name);
        }
        self.block(&function.body);
        self.scopes.pop();
    }

    fn block(&mut self, block: &Block) {
        self.scopes.push(Vec::new());
        for stmt in &block.stmts {
            self.stmt(stmt);
        }
        self.scopes.pop();
    }

    fn stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Binding { name, value, .. } => {
                if let Some(value) = value {
                    self.expr(value);
                }
                self.bind(&name.name);
            }
            Stmt::Assign { target, value, .. } => {
                self.expr(target);
                self.expr(value);
            }
            Stmt::Expr { expr, .. } => self.expr(expr),
            Stmt::Return { value, .. } => {
                if let Some(value) = value {
                    self.expr(value);
                }
            }
            Stmt::Break { .. } | Stmt::Continue { .. } => {}
            Stmt::Defer { stmt, .. } => self.stmt(stmt),
            Stmt::Block { block, .. } => self.block(block),
            Stmt::If {
                cond,
                then_branch,
                else_branch,
                ..
            } => {
                self.condition(cond, then_branch);
                if let Some(other) = else_branch.as_deref() {
                    self.stmt(other);
                }
            }
            Stmt::While { cond, body, .. } => {
                self.scopes.push(Vec::new());
                let binders = self.condition_uses(cond);
                    self.bind_all(binders);
                self.block(body);
                self.scopes.pop();
            }
            Stmt::Loop { body, .. } => self.block(body),
            Stmt::Match { subject, arms, .. } => {
                self.expr(subject);
                self.arms(arms);
            }
            Stmt::For { kind, body, .. } => match kind {
                ForKind::ThreeClause { init, cond, step } => {
                    self.scopes.push(Vec::new());
                    self.stmt(init);
                    let binders = self.condition_uses(cond);
                    self.bind_all(binders);
                    self.stmt(step);
                    self.block(body);
                    self.scopes.pop();
                }
                ForKind::In {
                    index,
                    value,
                    iterable,
                } => {
                    self.expr(iterable);
                    self.scopes.push(Vec::new());
                    if let Some(index) = index {
                        self.bind(&index.name);
                    }
                    self.bind(&value.name);
                    self.block(body);
                    self.scopes.pop();
                }
            },
            Stmt::Error { .. } => {}
        }
    }

    fn condition(&mut self, cond: &Expr, then_branch: &Block) {
        self.scopes.push(Vec::new());
        let binders = self.condition_uses(cond);
                    self.bind_all(binders);
        for stmt in &then_branch.stmts {
            self.stmt(stmt);
        }
        self.scopes.pop();
    }

    fn condition_uses(&mut self, cond: &Expr) -> Vec<String> {
        match cond {
            Expr::CondBinding { pattern, value, .. } => {
                self.expr(value);
                pattern.binders().into_iter().map(|name| name.name).collect()
            }
            other => {
                self.expr(other);
                Vec::new()
            }
        }
    }

    /// Each arm sees its own scope: what its pattern binds lives only in the arm.
    fn arms(&mut self, arms: &[dux_ast::MatchArm]) {
        for arm in arms {
            self.scopes.push(Vec::new());
            self.bind_all(arm.pattern.binders().into_iter().map(|name| name.name).collect());
            if let Some(guard) = &arm.guard {
                self.expr(guard);
            }
            match &arm.body {
                dux_ast::MatchBody::Expr(expr) => self.expr(expr),
                dux_ast::MatchBody::Block(block) => self.block(block),
                dux_ast::MatchBody::Stmt(stmt) => self.stmt(stmt),
            }
            self.scopes.pop();
        }
    }

    fn expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Ident { name, span } => self.use_name(name, *span),
            Expr::Call { callee, args, .. } => {
                let arity = match callee.as_ref() {
                    Expr::Ident { name, span } => {
                        self.use_name(name, *span);
                        self.callable(name)
                    }
                    Expr::Field { base, .. } => {
                        self.expr(base);
                        Callable::Opaque
                    }
                    other => {
                        self.expr(other);
                        Callable::Opaque
                    }
                };
                for arg in args {
                    self.expr(arg);
                }
                if let Callable::Function { params } = arity {
                    if params != args.len() && !args.iter().any(|a| matches!(a, Expr::Error { .. }))
                    {
                        self.diagnostics.push(
                            Diagnostic::error(
                                code::WRONG_ARITY,
                                format!(
                                    "`{}` expects {params} argument(s), the call supplies {}",
                                    callee_name(callee.as_ref()),
                                    args.len()
                                ),
                            )
                            .with_span(expr.span()),
                        );
                    }
                }
            }
            Expr::Field { base, .. } => self.expr(base),
            Expr::Index { base, index, .. } => {
                self.expr(base);
                self.expr(index);
            }
            Expr::Range { start, end, .. } => {
                if let Some(start) = start {
                    self.expr(start);
                }
                if let Some(end) = end {
                    self.expr(end);
                }
            }
            Expr::Member { base, .. } => self.expr(base),
            Expr::ArrayLiteral { elements, .. } => {
                for element in elements {
                    self.expr(element);
                }
            }
            Expr::RepeatedArray { value, len, .. } => {
                self.expr(value);
                self.expr(len);
            }
            Expr::Path { .. } => {}
            Expr::StructLiteral { fields, .. } => {
                for (_, value) in fields {
                    self.expr(value);
                }
            }
            Expr::CondBinding { pattern, value, .. } => {
                let _ = pattern;
                self.expr(value);
            }
            Expr::Match { subject, arms, .. } => {
                self.expr(subject);
                self.arms(arms);
            }
            Expr::InterpString { parts, .. } => {
                for part in parts {
                    if let InterpolationPart::Hole {
                        written: Some(written),
                        ..
                    } = part
                    {
                        self.expr(written);
                    }
                }
            }
            Expr::If {
                cond,
                then_arm,
                else_arm,
                ..
            } => {
                self.scopes.push(Vec::new());
                let binders = self.condition_uses(cond);
                    self.bind_all(binders);
                self.expr(then_arm);
                self.scopes.pop();
                if let Some(else_arm) = else_arm {
                    self.expr(else_arm);
                }
            }
            Expr::Block { block, .. } => self.block(block),
            Expr::Unary { operand, .. } => self.expr(operand),
            Expr::Binary { lhs, rhs, .. } => {
                self.expr(lhs);
                self.expr(rhs);
            }
            Expr::Cast { value, .. } => self.expr(value),
            Expr::Paren { inner, .. } => self.expr(inner),
            Expr::Int { .. }
            | Expr::Float { .. }
            | Expr::Str { .. }
            | Expr::Char { .. }
            | Expr::Bool { .. }
            | Expr::Error { .. } => {}
        }
    }

    fn use_name(&mut self, name: &str, span: Span) {
        if name == "_" || self.find(name).is_some() {
            return;
        }
        self.diagnostics.push(
            Diagnostic::error(
                code::UNDECLARED,
                format!("`{name}` is not declared in this scope"),
            )
            .with_span(span),
        );
    }

    fn find(&self, name: &str) -> Option<Item> {
        for scope in self.scopes.iter().rev() {
            if scope.iter().any(|bound| bound == name) {
                return Some(Item::Callable(Callable::Opaque));
            }
        }
        self.top_level.get(name).copied()
    }

    fn callable(&self, name: &str) -> Callable {
        for scope in self.scopes.iter().rev() {
            if scope.iter().any(|bound| bound == name) {
                return Callable::Opaque;
            }
        }
        match self.top_level.get(name) {
            Some(Item::Callable(callable)) => *callable,
            _ => Callable::Opaque,
        }
    }

    fn bind_all(&mut self, names: Vec<String>) {
        for name in names {
            self.bind(&name);
        }
    }

    fn bind(&mut self, name: &str) {
        if name == "_" {
            return;
        }
        if let Some(scope) = self.scopes.last_mut() {
            scope.push(name.to_string());
        }
    }
}

fn callee_name(callee: &Expr) -> String {
    match callee {
        Expr::Ident { name, .. } => name.clone(),
        _ => "this expression".to_string(),
    }
}

