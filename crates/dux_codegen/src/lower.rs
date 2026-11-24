//! Lowering from the AST to LLVM IR.
//!
//! The shape follows DESIGN.md §11.2: the entry block holds every alloca,
//! other instructions go in the blocks the control flow needs, and `main` is
//! a C `int main(void)` so the C runtime can start it (D12). Numeric
//! conversions follow D25: implicit exactly when the destination holds every
//! value of the source, `as` for anything else.

use std::collections::{HashMap, HashSet};

use dux_ast::{
    BinaryOp, Block, Decl, Expr, FnDecl, InterpolationPart, Program, Stmt, Type, UnaryOp,
};
use dux_diag::{code, Diagnostic};
use dux_source::Span;
use inkwell::basic_block::BasicBlock;
use inkwell::builder::Builder;
use inkwell::context::Context;
use inkwell::module::Module;
use inkwell::types::{BasicMetadataTypeEnum, BasicType, BasicTypeEnum, FloatType, IntType};
use inkwell::values::{
    BasicMetadataValueEnum, BasicValueEnum, FloatValue, IntValue, PointerValue,
};
use inkwell::values::{FunctionValue, ValueKind};
use inkwell::{AddressSpace, FloatPredicate, IntPredicate};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DuxType {
    Bool,
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
    F32,
    F64,
    /// A string literal: a pointer into NUL terminated static storage. The
    /// fat `{ ptr, len }` form arrives with slices at M3.
    Str,
    /// `*T` and `rawptr`, both an untyped pointer here.
    Ptr,
    /// Something we could not type; it silences follow-on diagnostics.
    Error,
}

impl DuxType {
    fn from_ast(ty: &Type) -> Option<DuxType> {
        match ty {
            Type::Named { name, .. } => DuxType::named(name),
            Type::Pointer { .. } => Some(DuxType::Ptr),
            _ => None,
        }
    }

    fn named(name: &str) -> Option<DuxType> {
        Some(match name {
            "bool" => DuxType::Bool,
            "i8" | "byte" => DuxType::I8,
            "i16" => DuxType::I16,
            "i32" | "int" | "rune" => DuxType::I32,
            "i64" | "long" | "isize" => DuxType::I64,
            "u8" | "char" => DuxType::U8,
            "u16" => DuxType::U16,
            "u32" | "uint" => DuxType::U32,
            "u64" | "usize" => DuxType::U64,
            "f32" => DuxType::F32,
            "f64" | "float" => DuxType::F64,
            "string" => DuxType::Str,
            "rawptr" => DuxType::Ptr,
            _ => return None,
        })
    }

    fn is_integral(self) -> bool {
        matches!(
            self,
            DuxType::I8
                | DuxType::I16
                | DuxType::I32
                | DuxType::I64
                | DuxType::U8
                | DuxType::U16
                | DuxType::U32
                | DuxType::U64
        )
    }

    fn is_signed(self) -> bool {
        matches!(
            self,
            DuxType::I8 | DuxType::I16 | DuxType::I32 | DuxType::I64
        )
    }

    fn is_float(self) -> bool {
        matches!(self, DuxType::F32 | DuxType::F64)
    }

    fn is_numeric(self) -> bool {
        self.is_integral() || self.is_float()
    }

    fn width(self) -> u32 {
        match self {
            DuxType::Bool => 1,
            DuxType::I8 | DuxType::U8 => 8,
            DuxType::I16 | DuxType::U16 => 16,
            DuxType::I32 | DuxType::U32 | DuxType::F32 => 32,
            _ => 64,
        }
    }

    fn llvm(self, context: &'static Context) -> BasicTypeEnum<'static> {
        match self {
            DuxType::Bool => context.bool_type().into(),
            DuxType::I8 | DuxType::U8 => context.i8_type().into(),
            DuxType::I16 | DuxType::U16 => context.i16_type().into(),
            DuxType::I32 | DuxType::U32 => context.i32_type().into(),
            DuxType::I64 | DuxType::U64 => context.i64_type().into(),
            DuxType::F32 => context.f32_type().into(),
            DuxType::F64 => context.f64_type().into(),
            DuxType::Str | DuxType::Ptr | DuxType::Error => {
                context.ptr_type(AddressSpace::default()).into()
            }
        }
    }

    fn int_type(self, context: &'static Context) -> IntType<'static> {
        match self {
            DuxType::Bool => context.bool_type(),
            DuxType::I8 | DuxType::U8 => context.i8_type(),
            DuxType::I16 | DuxType::U16 => context.i16_type(),
            DuxType::I32 | DuxType::U32 => context.i32_type(),
            _ => context.i64_type(),
        }
    }

    fn float_type(self, context: &'static Context) -> FloatType<'static> {
        match self {
            DuxType::F32 => context.f32_type(),
            _ => context.f64_type(),
        }
    }

    fn name(self) -> &'static str {
        match self {
            DuxType::Bool => "bool",
            DuxType::I8 => "i8",
            DuxType::I16 => "i16",
            DuxType::I32 => "i32",
            DuxType::I64 => "i64",
            DuxType::U8 => "u8",
            DuxType::U16 => "u16",
            DuxType::U32 => "u32",
            DuxType::U64 => "u64",
            DuxType::F32 => "f32",
            DuxType::F64 => "f64",
            DuxType::Str => "string",
            DuxType::Ptr => "rawptr",
            DuxType::Error => "<error>",
        }
    }

    fn fits(self, value: u128, negated: bool) -> bool {
        let limit: u128 = match self {
            DuxType::Bool => 1,
            DuxType::I8 => 128,
            DuxType::I16 => 32768,
            DuxType::I32 => 1 << 31,
            DuxType::I64 => 1 << 63,
            DuxType::U8 => 256,
            DuxType::U16 => 1 << 16,
            DuxType::U32 => 1 << 32,
            DuxType::U64 => 1 << 64,
            DuxType::F32 | DuxType::F64 => return true,
            _ => return false,
        };
        if self.is_signed() {
            value < limit + u128::from(negated)
        } else {
            value < limit
        }
    }
}

#[derive(Clone, Copy)]
enum Core {
    Int(IntValue<'static>),
    Float(FloatValue<'static>),
    Ptr(PointerValue<'static>),
    Void,
}

#[derive(Clone, Copy)]
struct Typed {
    core: Core,
    ty: DuxType,
}

impl Typed {
    fn int(value: IntValue<'static>, ty: DuxType) -> Self {
        Self {
            core: Core::Int(value),
            ty,
        }
    }

    fn float(value: FloatValue<'static>, ty: DuxType) -> Self {
        Self {
            core: Core::Float(value),
            ty,
        }
    }

    fn pointer(value: PointerValue<'static>, ty: DuxType) -> Self {
        Self {
            core: Core::Ptr(value),
            ty,
        }
    }

    fn void() -> Self {
        Self {
            core: Core::Void,
            ty: DuxType::Error,
        }
    }

    fn error() -> Self {
        Self {
            core: Core::Void,
            ty: DuxType::Error,
        }
    }

    fn int_value(self) -> Option<IntValue<'static>> {
        match self.core {
            Core::Int(value) => Some(value),
            _ => None,
        }
    }

    fn float_value(self) -> Option<FloatValue<'static>> {
        match self.core {
            Core::Float(value) => Some(value),
            _ => None,
        }
    }

    fn pointer_value(self) -> Option<PointerValue<'static>> {
        match self.core {
            Core::Ptr(value) => Some(value),
            _ => None,
        }
    }

    fn basic(self) -> Option<BasicValueEnum<'static>> {
        match self.core {
            Core::Int(value) => Some(value.into()),
            Core::Float(value) => Some(value.into()),
            Core::Ptr(value) => Some(value.into()),
            Core::Void => None,
        }
    }
}

struct Loop {
    continuation: BasicBlock<'static>,
    exit: BasicBlock<'static>,
}

struct Signature {
    params: Vec<DuxType>,
    return_ty: Option<DuxType>,
}

pub(crate) struct Emitter {
    context: &'static Context,
    builder: Builder<'static>,
    diagnostics: Vec<Diagnostic>,
    functions: HashMap<String, FunctionValue<'static>>,
    signatures: HashMap<String, Signature>,
    /// Names already reported as not lowerable by this build; calls to them
    /// stay quiet so the user sees one diagnostic, not a cascade.
    skipped: HashSet<String>,
    scopes: Vec<HashMap<String, (DuxType, PointerValue<'static>)>>,
    loops: Vec<Loop>,
    allocas: Option<BasicBlock<'static>>,
    result_ty: Option<DuxType>,
    is_main: bool,
    dead: bool,
    printf: Option<FunctionValue<'static>>,
    puts: Option<FunctionValue<'static>>,
    fputs: Option<FunctionValue<'static>>,
    stdout: Option<inkwell::values::GlobalValue<'static>>,
}

impl Emitter {
    pub(crate) fn new(context: &'static Context) -> Self {
        Self {
            context,
            builder: context.create_builder(),
            diagnostics: Vec::new(),
            functions: HashMap::new(),
            signatures: HashMap::new(),
            skipped: HashSet::new(),
            scopes: Vec::new(),
            loops: Vec::new(),
            allocas: None,
            result_ty: None,
            is_main: false,
            dead: false,
            printf: None,
            puts: None,
            fputs: None,
            stdout: None,
        }
    }

    pub(crate) fn lower(
        mut self,
        module: &Module<'static>,
        program: &Program,
    ) -> Vec<Diagnostic> {
        for decl in &program.decls {
            match decl {
                Decl::Const(decl) => self.unsupported(
                    decl.span,
                    "`const` (the compile-time evaluator lands with the resolver)",
                ),
                Decl::Global(decl) => self.unsupported(
                    decl.span,
                    "`global` (module-level storage lands with the resolver)",
                ),
                Decl::Extern(block) => {
                    for function in &block.functions {
                        self.declare(module, function, true);
                    }
                }
                _ => {}
            }
        }
        let functions: Vec<&FnDecl> = program.functions().collect();
        for function in &functions {
            self.declare(module, function, false);
        }
        for function in &functions {
            self.lower_body(module, function);
        }
        self.diagnostics
            .sort_by_key(|diagnostic| diagnostic.span.map(|span| span.start));
        self.diagnostics
    }
}

impl Emitter {
    fn declare(&mut self, module: &Module<'static>, function: &FnDecl, external: bool) {
        let name = function.name.name.clone();
        if self.skipped.contains(&name) || self.functions.contains_key(&name) {
            if !external {
                self.diagnostics.push(
                    Diagnostic::error(
                        code::UNEXPECTED_TOKEN,
                        format!("`{name}` is declared more than once"),
                    )
                    .with_span(function.name.span),
                );
            }
            return;
        }
        if !external && name == "main" {
            let fn_value =
                module.add_function("main", self.context.i32_type().fn_type(&[], false), None);
            self.functions.insert(name.clone(), fn_value);
            self.signatures.insert(
                name,
                Signature {
                    params: Vec::new(),
                    return_ty: Some(DuxType::I32),
                },
            );
            return;
        }
        if !function.type_params.is_empty() {
            self.skipped.insert(name);
            self.unsupported(function.name.span, "a generic `fn` (monomorphisation lands at M5)");
            return;
        }
        let mut params = Vec::new();
        for param in &function.params {
            let Some(ty) = DuxType::from_ast(&param.ty) else {
                if !external {
                    self.skipped.insert(name);
                    self.unsupported(
                        param.ty.span(),
                        &format!(
                            "a `{}` parameter (only primitives, pointers and string literals lower today)",
                            param.name.name
                        ),
                    );
                } else {
                    self.skipped.insert(name);
                }
                return;
            };
            params.push(ty);
        }
        let return_ty = match &function.return_ty {
            None => None,
            Some(ty) => match DuxType::from_ast(ty) {
                Some(ty) => Some(ty),
                None => {
                    if !external {
                        self.skipped.insert(name);
                        self.unsupported(ty.span(), "this result type (primitives and pointers today)");
                    } else {
                        self.skipped.insert(name);
                    }
                    return;
                }
            },
        };
        let parameters = params
            .iter()
            .map(|ty| BasicMetadataTypeEnum::from(ty.llvm(self.context)))
            .collect::<Vec<BasicMetadataTypeEnum>>();
        let signature = match return_ty {
            Some(ty) => ty.llvm(self.context).fn_type(&parameters, false),
            None => self.context.void_type().fn_type(&parameters, false),
        };
        let fn_value = module.add_function(&name, signature, None);
        self.functions.insert(name.clone(), fn_value);
        self.signatures.insert(name, Signature { params, return_ty });
    }

    fn lower_body(&mut self, module: &Module<'static>, function: &FnDecl) {
        let name = function.name.name.clone();
        if self.skipped.contains(&name) || function.body.stmts.is_empty() {
            return;
        }
        let Some(fn_value) = self.functions.get(&name).copied() else {
            return;
        };
        let signature = self
            .signatures
            .get(&name)
            .expect("a lowered function was declared first");
        let entry = self.context.append_basic_block(fn_value, "entry");
        let body = self.context.append_basic_block(fn_value, "body");
        self.builder.position_at_end(entry);
        self.allocas = Some(entry);
        self.scopes.clear();
        self.scopes.push(HashMap::new());
        self.loops.clear();
        self.dead = false;
        self.is_main = name == "main";
        self.result_ty = if self.is_main {
            Some(DuxType::I32)
        } else {
            signature.return_ty
        };

        for (index, param) in function.params.iter().enumerate() {
            let Some(ty) = DuxType::from_ast(&param.ty) else {
                continue;
            };
            let Some(argument) = fn_value.get_nth_param(index as u32) else {
                continue;
            };
            let slot = self.alloca(ty, &param.name.name);
            let core = match argument {
                BasicValueEnum::IntValue(value) => Core::Int(value),
                BasicValueEnum::FloatValue(value) => Core::Float(value),
                BasicValueEnum::PointerValue(value) => Core::Ptr(value),
                _ => continue,
            };
            if let Some(basic) = (Typed { core, ty }).basic() {
                let _ = self.builder.build_store(slot, basic);
            }
            if let Some(scope) = self.scopes.last_mut() {
                scope.insert(param.name.name.clone(), (ty, slot));
            }
        }

        self.br(body);
        self.builder.position_at_end(body);
        self.block_body(module, &function.body);
        if self
            .builder
            .get_insert_block()
            .is_some_and(|block| block.get_terminator().is_none())
        {
            self.finish_function(function);
        }
        self.allocas = None;
    }

    fn finish_function(&mut self, function: &FnDecl) {
        if self.is_main {
            let zero = self.context.i32_type().const_int(0, false);
            let _ = self.builder.build_return(Some(&zero));
            return;
        }
        match self.result_ty {
            Some(ty) => {
                self.diagnostics.push(
                    Diagnostic::error(
                        code::UNEXPECTED_TOKEN,
                        format!("not all paths return a value from `{}`", function.name.name),
                    )
                    .with_span(function.name.span)
                    .with_note("the checker takes over path analysis at M2"),
                );
                let zero = ty.llvm(self.context).const_zero();
                let _ = self.builder.build_return(Some(&zero));
            }
            None => {
                let _ = self.builder.build_return(None);
            }
        }
    }

    fn block_body(&mut self, module: &Module<'static>, block: &Block) {
        self.scopes.push(HashMap::new());
        for stmt in &block.stmts {
            if self.dead {
                break;
            }
            self.stmt(module, stmt);
        }
        self.scopes.pop();
    }
}

impl Emitter {
    fn stmt(&mut self, module: &Module<'static>, stmt: &Stmt) {
        match stmt {
            Stmt::Expr { expr, .. } => {
                self.expr(module, expr, None);
            }
            Stmt::Binding {
                name,
                ty,
                value,
                span,
            } => self.binding(module, name.name.clone(), ty.as_ref(), value.as_ref(), *span),
            Stmt::Assign {
                target,
                op,
                value,
                span,
            } => self.assignment(module, target, *op, value, *span),
            Stmt::If {
                cond,
                then_branch,
                else_branch,
                ..
            } => self.if_stmt(module, cond, then_branch, else_branch.as_deref()),
            Stmt::While { cond, body, .. } => self.while_stmt(module, cond, body),
            Stmt::Loop { body, .. } => self.loop_stmt(module, body),
            Stmt::For { kind, body, span } => {
                use dux_ast::ForKind;
                match kind {
                    ForKind::ThreeClause { init, cond, step } => {
                        self.for_stmt(module, init, cond, step, body)
                    }
                    ForKind::In { .. } => {
                        self.unsupported(*span, "`for .. in` (iteration lands at M3)");
                        self.dead = true;
                    }
                }
            }
            Stmt::Break { span } | Stmt::Continue { span } => {
                let is_break = matches!(stmt, Stmt::Break { .. });
                match self.loops.last() {
                    Some(Loop {
                        continuation,
                        exit,
                    }) => {
                        let target = if is_break { *exit } else { *continuation };
                        self.br(target);
                        self.dead = true;
                    }
                    None => {
                        self.diagnostics.push(
                            Diagnostic::error(
                                code::UNEXPECTED_TOKEN,
                                if is_break {
                                    "`break` outside a loop"
                                } else {
                                    "`continue` outside a loop"
                                },
                            )
                            .with_span(*span),
                        );
                        self.dead = true;
                    }
                }
            }
            Stmt::Return { value, span } => self.return_stmt(module, value.as_ref(), *span),
            Stmt::Defer { .. } => {
                self.unsupported(stmt.span(), "`defer` (lands at M3)");
                self.dead = true;
            }
            Stmt::Block { block, .. } => self.block_body(module, block),
            Stmt::Error { .. } => {}
        }
    }

    fn binding(
        &mut self,
        module: &Module<'static>,
        name: String,
        ty: Option<&Type>,
        value: Option<&Expr>,
        span: Span,
    ) {
        let annotated = match ty {
            Some(ty) => match DuxType::from_ast(ty) {
                Some(ty) => Some(ty),
                None => {
                    self.unsupported(ty.span(), "this type annotation (primitives and pointers today)");
                    return;
                }
            },
            None => None,
        };
        let initialized = match (value, annotated) {
            (Some(expr), Some(to)) => self.expr(module, expr, Some(to)),
            (Some(expr), None) => self.expr(module, expr, None),
            (None, _) => None,
        };
        let ty = match (annotated, initialized) {
            (Some(ty), _) => ty,
            (None, Some(initialized)) => initialized.ty,
            (None, None) => {
                if value.is_none() {
                    self.unsupported(span, "a binding with neither a type nor a value");
                }
                return;
            }
        };
        let slot = self.alloca(ty, &name);
        match initialized {
            Some(value) => {
                if value.ty == DuxType::Error {
                    return;
                }
                if let Some(value) = self.coerce_implicit(module, value, ty, span) {
                    match value.basic() {
                        Some(basic) => {
                            let _ = self.builder.build_store(slot, basic);
                        }
                        None => self.unsupported(span, "storing this value"),
                    }
                }
            }
            None => {
                let zero = ty.llvm(self.context).const_zero();
                let _ = self.builder.build_store(slot, zero);
            }
        }
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name, (ty, slot));
        }
    }

    fn assignment(
        &mut self,
        module: &Module<'static>,
        target: &Expr,
        op: dux_ast::AssignOp,
        value: &Expr,
        span: Span,
    ) {
        use dux_ast::AssignOp;
        let Expr::Ident { name, span: target_span } = target else {
            self.unsupported(target.span(), "assigning to anything but a plain local");
            return;
        };
        let Some((ty, slot)) = self.lookup(name).copied() else {
            self.diagnostics.push(
                Diagnostic::error(
                    code::UNEXPECTED_TOKEN,
                    format!("`{name}` is not declared in this scope"),
                )
                .with_span(*target_span)
                .with_note("the resolver lands at M2; until then an unknown name is reported here"),
            );
            return;
        };
        let Some(rhs) = self.expr(module, value, Some(ty)) else {
            return;
        };
        if rhs.ty == DuxType::Error {
            return;
        }
        let Some(stored) = self.coerce_implicit(module, rhs, ty, span) else {
            return;
        };
        let final_value = if op == AssignOp::Set {
            stored
        } else {
            let Some(op) = compound_op(op) else {
                self.unsupported(span, "this compound assignment");
                return;
            };
            let Some(current) = self.load(ty, slot) else {
                return;
            };
            match self.binary_typed(module, op, current, stored, span) {
                Some(value) => value,
                None => return,
            }
        };
        match final_value.basic() {
            Some(basic) => {
                let _ = self.builder.build_store(slot, basic);
            }
            None => self.unsupported(span, "storing this value"),
        }
    }

    fn return_stmt(&mut self, module: &Module<'static>, value: Option<&Expr>, span: Span) {
        match (value, self.result_ty) {
            (None, Some(ty)) => {
                if self.is_main || ty == DuxType::I32 || ty == DuxType::I64 {
                    let zero = ty.int_type(self.context).const_int(0, false);
                    let _ = self.builder.build_return(Some(&zero));
                } else {
                    self.diagnostics.push(
                        Diagnostic::error(
                            code::UNEXPECTED_TOKEN,
                            format!("`return` needs a `{}` here", ty.name()),
                        )
                        .with_span(span),
                    );
                    let zero = ty.llvm(self.context).const_zero();
                    let _ = self.builder.build_return(Some(&zero));
                }
            }
            (None, None) => {
                let _ = self.builder.build_return(None);
            }
            (Some(expr), Some(ty)) => {
                let Some(value) = self.expr(module, expr, Some(ty)) else {
                    self.dead = true;
                    return;
                };
                match self.coerce_implicit(module, value, ty, span) {
                    Some(value) => match value.basic() {
                        Some(basic) => {
                            let _ = self.builder.build_return(Some(&basic));
                        }
                        None => {
                            self.unsupported(expr.span(), "returning this value");
                            let zero = ty.llvm(self.context).const_zero();
                            let _ = self.builder.build_return(Some(&zero));
                        }
                    },
                    None => {
                        let zero = ty.llvm(self.context).const_zero();
                        let _ = self.builder.build_return(Some(&zero));
                    }
                }
            }
            (Some(expr), None) => {
                self.expr(module, expr, None);
                self.diagnostics.push(
                    Diagnostic::error(
                        code::UNEXPECTED_TOKEN,
                        "`return` with a value in a function that results in nothing",
                    )
                    .with_span(span),
                );
            }
        }
        self.dead = true;
    }

    fn if_stmt(
        &mut self,
        module: &Module<'static>,
        cond: &Expr,
        then_branch: &Block,
        else_branch: Option<&Stmt>,
    ) {
        let condition = self.condition(module, cond);
        let then_block = self.new_block("then");
        let else_block = self.new_block("else");
        let merge = self.new_block("endif");
        let _ = self
            .builder
            .build_conditional_branch(condition, then_block, else_block);

        self.builder.position_at_end(then_block);
        self.dead = false;
        self.scopes.push(HashMap::new());
        for stmt in &then_branch.stmts {
            if self.dead {
                break;
            }
            self.stmt(module, stmt);
        }
        self.scopes.pop();
        self.br(merge);

        self.builder.position_at_end(else_block);
        self.dead = false;
        match else_branch {
            None => self.br(merge),
            Some(Stmt::If { .. }) => {
                self.stmt(module, else_branch.unwrap());
                self.br(merge);
            }
            Some(Stmt::Block { block, .. }) => {
                self.block_body(module, block);
                self.br(merge);
            }
            Some(other) => {
                self.unsupported(other.span(), "this shape of `else` arm");
                self.br(merge);
            }
        }

        self.builder.position_at_end(merge);
        self.dead = false;
    }

    fn while_stmt(&mut self, module: &Module<'static>, cond: &Expr, body: &Block) {
        let cond_block = self.new_block("while.cond");
        let body_block = self.new_block("while.body");
        let exit_block = self.new_block("while.end");
        self.br(cond_block);

        self.builder.position_at_end(cond_block);
        let condition = self.condition(module, cond);
        let _ = self
            .builder
            .build_conditional_branch(condition, body_block, exit_block);

        self.builder.position_at_end(body_block);
        self.dead = false;
        self.loops.push(Loop {
            continuation: cond_block,
            exit: exit_block,
        });
        self.block_body(module, body);
        self.loops.pop();
        self.br(cond_block);

        self.builder.position_at_end(exit_block);
        self.dead = false;
    }

    fn loop_stmt(&mut self, module: &Module<'static>, body: &Block) {
        let body_block = self.new_block("loop.body");
        let exit_block = self.new_block("loop.end");
        self.br(body_block);

        self.builder.position_at_end(body_block);
        self.dead = false;
        self.loops.push(Loop {
            continuation: body_block,
            exit: exit_block,
        });
        self.block_body(module, body);
        self.loops.pop();
        self.br(body_block);

        self.builder.position_at_end(exit_block);
        self.dead = false;
    }

    fn for_stmt(
        &mut self,
        module: &Module<'static>,
        init: &Stmt,
        cond: &Expr,
        step: &Stmt,
        body: &Block,
    ) {
        self.scopes.push(HashMap::new());
        self.stmt(module, init);
        let cond_block = self.new_block("for.cond");
        let body_block = self.new_block("for.body");
        let step_block = self.new_block("for.step");
        let exit_block = self.new_block("for.end");
        self.br(cond_block);

        self.builder.position_at_end(cond_block);
        let condition = self.condition(module, cond);
        let _ = self
            .builder
            .build_conditional_branch(condition, body_block, exit_block);

        self.builder.position_at_end(body_block);
        self.dead = false;
        self.loops.push(Loop {
            continuation: step_block,
            exit: exit_block,
        });
        self.block_body(module, body);
        self.loops.pop();
        self.br(step_block);

        self.builder.position_at_end(step_block);
        self.dead = false;
        self.stmt(module, step);
        self.br(cond_block);

        self.builder.position_at_end(exit_block);
        self.dead = false;
        self.scopes.pop();
    }

    fn condition(&mut self, module: &Module<'static>, cond: &Expr) -> IntValue<'static> {
        match cond {
            Expr::CondBinding { .. } => {
                self.unsupported(
                    cond.span(),
                    "a pattern condition, `if P := e` (lands at M4)",
                );
                self.false_value()
            }
            other => {
                let Some(value) = self.expr(module, other, None) else {
                    return self.false_value();
                };
                if value.ty == DuxType::Bool {
                    return value.int_value().unwrap_or_else(|| self.false_value());
                }
                self.diagnostics.push(
                    Diagnostic::error(
                        code::UNEXPECTED_TOKEN,
                        format!("a condition must be a `bool`, found `{}`", value.ty.name()),
                    )
                    .with_span(cond.span()),
                );
                self.false_value()
            }
        }
    }

    fn false_value(&self) -> IntValue<'static> {
        self.context.bool_type().const_zero()
    }
}

impl Emitter {
    fn expr(
        &mut self,
        module: &Module<'static>,
        expr: &Expr,
        expected: Option<DuxType>,
    ) -> Option<Typed> {
        match expr {
            Expr::Int { value, span } => {
                let want = expected.filter(|ty| ty.is_integral() || *ty == DuxType::Bool);
                if let Some(to) = want {
                    if to.fits(*value, false) {
                        return Some(Typed::int(
                            to.int_type(self.context).const_int(*value as u64, false),
                            to,
                        ));
                    }
                    self.diagnostics.push(
                        Diagnostic::error(
                            code::UNEXPECTED_TOKEN,
                            format!("the literal {value} does not fit a `{}`", to.name()),
                        )
                        .with_span(*span),
                    );
                    return None;
                }
                let ty = if *value <= i32::MAX as u128 {
                    DuxType::I32
                } else if *value <= i64::MAX as u128 {
                    DuxType::I64
                } else if *value <= u64::MAX as u128 {
                    DuxType::U64
                } else {
                    self.diagnostics.push(
                        Diagnostic::error(
                            code::NUMBER_OUT_OF_RANGE,
                            "integer literal does not fit any integer type",
                        )
                        .with_span(*span),
                    );
                    return None;
                };
                Some(Typed::int(
                    ty.int_type(self.context).const_int(*value as u64, false),
                    ty,
                ))
            }
            Expr::Float { value, span: _ } => {
                let ty = if expected == Some(DuxType::F32) {
                    DuxType::F32
                } else {
                    DuxType::F64
                };
                Some(Typed::float(
                    ty.float_type(self.context).const_float(*value),
                    ty,
                ))
            }
            Expr::Bool { value, .. } => Some(Typed::int(
                self.context.bool_type().const_int(*value as u64, false),
                DuxType::Bool,
            )),
            Expr::Char { value, .. } => Some(Typed::int(
                self.context.i32_type().const_int(*value as u32 as u64, false),
                DuxType::I32,
            )),
            Expr::Str { text, span, .. } => Some(self.string_literal(text, *span)),
            Expr::Ident { name, span } => match self.lookup(name).copied() {
                Some((ty, slot)) => self.load(ty, slot),
                None => {
                    self.diagnostics.push(
                        Diagnostic::error(
                            code::UNEXPECTED_TOKEN,
                            format!("`{name}` is not declared in this scope"),
                        )
                        .with_span(*span)
                        .with_note(
                            "the resolver lands at M2; until then an unknown name is reported here",
                        ),
                    );
                    None
                }
            },
            Expr::Paren { inner, .. } => self.expr(module, inner, expected),
            Expr::Unary {
                op, operand, span, ..
            } => self.unary(module, *op, operand, *span, expected),
            Expr::Binary {
                op, lhs, rhs, span, ..
            } => {
                let _ = expected;
                self.binary(module, *op, lhs, rhs, *span)
            }
            Expr::Cast { value, target, .. } => self.cast(module, value, target),
            Expr::Call { callee, args, span } => self.call(module, callee, args, *span),
            Expr::InterpString { .. } => {
                self.unsupported(
                    expr.span(),
                    "an interpolated string outside `print`/`println` (string building lands at M3)",
                );
                None
            }
            other => {
                self.unsupported(other.span(), expression_name(other));
                None
            }
        }
    }

    fn string_literal(&mut self, text: &str, span: Span) -> Typed {
        match self.builder.build_global_string_ptr(text, "str") {
            Ok(global) => Typed::pointer(global.as_pointer_value(), DuxType::Str),
            Err(e) => {
                self.diagnostics.push(
                    Diagnostic::error(
                        code::CODEGEN_FAILED,
                        format!("failed to emit the string literal: {e}"),
                    )
                    .with_span(span),
                );
                Typed::error()
            }
        }
    }

    fn unary(
        &mut self,
        module: &Module<'static>,
        op: UnaryOp,
        operand: &Expr,
        span: Span,
        expected: Option<DuxType>,
    ) -> Option<Typed> {
        match op {
            UnaryOp::Not => {
                let value = self.expr(module, operand, None)?;
                if value.ty != DuxType::Bool {
                    self.diagnostics.push(
                        Diagnostic::error(
                            code::UNEXPECTED_TOKEN,
                            format!("`!` needs a `bool`, found `{}`", value.ty.name()),
                        )
                        .with_span(span),
                    );
                    return None;
                }
                let flipped = self.builder.build_not(value.int_value()?, "nottmp").ok()?;
                Some(Typed::int(flipped, DuxType::Bool))
            }
            UnaryOp::Neg => {
                if let Expr::Int {
                    value, span: literal_span, ..
                } = operand
                {
                    if let Some(to) = expected.filter(|ty| ty.is_integral()) {
                        if to.fits(*value, true) {
                            let wrapped = if to == DuxType::Bool {
                                *value
                            } else {
                                (1u128 << to.width()) - *value
                            };
                            return Some(Typed::int(
                                to.int_type(self.context).const_int(wrapped as u64, false),
                                to,
                            ));
                        }
                        let _ = literal_span;
                    }
                }
                let value = self.expr(module, operand, expected)?;
                if value.ty.is_float() {
                    let negated = self
                        .builder
                        .build_float_neg(value.float_value()?, "negtmp")
                        .ok()?;
                    Some(Typed::float(negated, value.ty))
                } else if value.ty.is_integral() {
                    let negated = self
                        .builder
                        .build_int_neg(value.int_value()?, "negtmp")
                        .ok()?;
                    Some(Typed::int(negated, value.ty))
                } else {
                    self.diagnostics.push(
                        Diagnostic::error(
                            code::UNEXPECTED_TOKEN,
                            format!("`-` needs a number, found `{}`", value.ty.name()),
                        )
                        .with_span(span),
                    );
                    None
                }
            }
            UnaryOp::AddressOf => {
                self.unsupported(operand.span(), "`&x` (pointers land at M3)");
                None
            }
            UnaryOp::Load => {
                self.unsupported(operand.span(), "`^p` (pointers land at M3)");
                None
            }
        }
    }

    fn binary(
        &mut self,
        module: &Module<'static>,
        op: BinaryOp,
        lhs: &Expr,
        rhs: &Expr,
        span: Span,
    ) -> Option<Typed> {
        if matches!(op, BinaryOp::And | BinaryOp::Or) {
            return self.short_circuit(module, op, lhs, rhs);
        }
        let left = self.expr(module, lhs, None)?;
        let right = self.expr(module, rhs, None)?;
        if left.ty == DuxType::Error || right.ty == DuxType::Error {
            return Some(Typed::error());
        }
        if left.ty == DuxType::Str || right.ty == DuxType::Str {
            self.unsupported(
                span,
                "`+`/`==` on strings (string operations land with the `strings` prelude at M5)",
            );
            return None;
        }
        if !left.ty.is_numeric() || !right.ty.is_numeric() {
            let bad = if !left.ty.is_numeric() {
                left.ty
            } else {
                right.ty
            };
            self.unsupported(
                span,
                &format!("this operator on `{}` values", bad.name()),
            );
            return None;
        }
        self.binary_typed(module, op, left, right, span)
    }

    fn binary_typed(
        &mut self,
        _module: &Module<'static>,
        op: BinaryOp,
        left: Typed,
        right: Typed,
        span: Span,
    ) -> Option<Typed> {
        let common = self.common_type(left.ty, right.ty, span)?;
        let left = self.convert(left, common)?;
        let right = self.convert(right, common)?;
        if common == DuxType::Bool {
            if !op.is_comparison() {
                self.unsupported(span, "arithmetic on `bool` values");
                return None;
            }
            let predicate = if op == BinaryOp::Eq {
                IntPredicate::EQ
            } else {
                IntPredicate::NE
            };
            let result = self
                .builder
                .build_int_compare(
                    predicate,
                    left.int_value()?,
                    right.int_value()?,
                    "cmptmp",
                )
                .ok()?;
            return Some(Typed::int(result, DuxType::Bool));
        }
        if op.is_comparison() {
            let result = if common.is_float() {
                let predicate = float_predicate(op)?;
                self.builder
                    .build_float_compare(
                        predicate,
                        left.float_value()?,
                        right.float_value()?,
                        "cmptmp",
                    )
                    .ok()?
            } else {
                let predicate = int_predicate(op, common.is_signed())?;
                self.builder
                    .build_int_compare(
                        predicate,
                        left.int_value()?,
                        right.int_value()?,
                        "cmptmp",
                    )
                    .ok()?
            };
            return Some(Typed::int(result, DuxType::Bool));
        }
        if common.is_float() {
            let l = left.float_value()?;
            let r = right.float_value()?;
            let value = match op {
                BinaryOp::Add => self.builder.build_float_add(l, r, "addtmp").ok()?,
                BinaryOp::Sub => self.builder.build_float_sub(l, r, "subtmp").ok()?,
                BinaryOp::Mul => self.builder.build_float_mul(l, r, "multmp").ok()?,
                BinaryOp::Div => self.builder.build_float_div(l, r, "divtmp").ok()?,
                BinaryOp::Mod => self.builder.build_float_rem(l, r, "modtmp").ok()?,
                _ => {
                    self.unsupported(span, "this operator");
                    return None;
                }
            };
            return Some(Typed::float(value, common));
        }
        let l = left.int_value()?;
        let r = right.int_value()?;
        let signed = common.is_signed();
        let value = match op {
            BinaryOp::Add => self.builder.build_int_add(l, r, "addtmp").ok()?,
            BinaryOp::Sub => self.builder.build_int_sub(l, r, "subtmp").ok()?,
            BinaryOp::Mul => self.builder.build_int_mul(l, r, "multmp").ok()?,
            BinaryOp::Div if signed => self.builder.build_int_signed_div(l, r, "divtmp").ok()?,
            BinaryOp::Div => self.builder.build_int_unsigned_div(l, r, "divtmp").ok()?,
            BinaryOp::Mod if signed => self.builder.build_int_signed_rem(l, r, "modtmp").ok()?,
            BinaryOp::Mod => self.builder.build_int_unsigned_rem(l, r, "modtmp").ok()?,
            _ => {
                self.unsupported(span, "this operator");
                return None;
            }
        };
        Some(Typed::int(value, common))
    }

    fn common_type(&mut self, a: DuxType, b: DuxType, span: Span) -> Option<DuxType> {
        if a == b {
            return Some(a);
        }
        if a.is_float() || b.is_float() {
            if a.is_float() && b.is_float() {
                return Some(if a.width() >= b.width() { a } else { b });
            }
            let float = if a.is_float() { a } else { b };
            let other = if a.is_float() { b } else { a };
            if other == DuxType::Bool {
                self.unsupported(span, "`bool` mixed with a number");
                return None;
            }
            let _ = float;
            return Some(DuxType::F64);
        }
        if a == DuxType::Bool || b == DuxType::Bool {
            self.unsupported(span, "`bool` mixed with a number");
            return None;
        }
        if !a.is_integral() || !b.is_integral() {
            self.unsupported(span, "this operator on non-numeric values");
            return None;
        }
        if a.is_signed() == b.is_signed() {
            return Some(if a.width() >= b.width() { a } else { b });
        }
        let (unsigned, signed) = if a.is_signed() { (b, a) } else { (a, b) };
        for width in [8u32, 16, 32, 64] {
            let candidate = if signed.is_signed() {
                match width {
                    8 => DuxType::I8,
                    16 => DuxType::I16,
                    32 => DuxType::I32,
                    _ => DuxType::I64,
                }
            } else {
                match width {
                    8 => DuxType::U8,
                    16 => DuxType::U16,
                    32 => DuxType::U32,
                    _ => DuxType::U64,
                }
            };
            if width > unsigned.width() && width >= signed.width() {
                return Some(candidate);
            }
        }
        Some(if a.width() >= b.width() { a } else { b })
    }

    fn short_circuit(
        &mut self,
        module: &Module<'static>,
        op: BinaryOp,
        lhs: &Expr,
        rhs: &Expr,
    ) -> Option<Typed> {
        let start = self.builder.get_insert_block()?;
        let left = self.condition(module, lhs);
        let rhs_block = self.new_block("sc.rhs");
        let done = self.new_block("sc.done");
        if op == BinaryOp::And {
            let _ = self.builder.build_conditional_branch(left, rhs_block, done);
        } else {
            let _ = self.builder
                .build_conditional_branch(left, done, rhs_block);
        }
        self.builder.position_at_end(rhs_block);
        let right = self.condition(module, rhs);
        let right_block = self.builder.get_insert_block()?;
        self.br(done);
        self.builder.position_at_end(done);
        let phi = self.builder.build_phi(self.context.bool_type(), "sc.result").ok()?;
        phi.add_incoming(&[(&left, start), (&right, right_block)]);
        Some(Typed::int(phi.as_basic_value().into_int_value(), DuxType::Bool))
    }

    fn cast(&mut self, module: &Module<'static>, target: &Expr, ty: &Type) -> Option<Typed> {
        let value = self.expr(module, target, None)?;
        let Some(to) = DuxType::from_ast(ty) else {
            self.unsupported(ty.span(), "casting to this type");
            return None;
        };
        if value.ty == DuxType::Error {
            return Some(Typed {
                core: value.core,
                ty: DuxType::Error,
            });
        }
        match self.convert(value, to) {
            Some(cast) => Some(cast),
            None => {
                self.diagnostics.push(
                    Diagnostic::error(
                        code::UNEXPECTED_TOKEN,
                        format!("cannot `as` a `{}` into a `{}`", value.ty.name(), to.name()),
                    )
                    .with_span(ty.span()),
                );
                None
            }
        }
    }

    /// The free, provably lossless conversions of D25. Everything else is a
    /// diagnostic pointing at `as`.
    fn coerce_implicit(
        &mut self,
        _module: &Module<'static>,
        value: Typed,
        to: DuxType,
        span: Span,
    ) -> Option<Typed> {
        if value.ty == to || value.ty == DuxType::Error {
            return Some(Typed {
                core: value.core,
                ty: if value.ty == DuxType::Error {
                    to
                } else {
                    value.ty
                },
            });
        }
        if self.lossless(value.ty, to) {
            return self.convert(value, to);
        }
        self.diagnostics.push(
            Diagnostic::error(
                code::UNEXPECTED_TOKEN,
                format!(
                    "a `{}` does not fit a `{}` for free; write `as {}` for the explicit conversion",
                    value.ty.name(),
                    to.name(),
                    to.name()
                ),
            )
            .with_span(span)
            .with_note("D25: implicit conversion is allowed only when the destination is lossless"),
        );
        None
    }

    fn lossless(&self, from: DuxType, to: DuxType) -> bool {
        if from == to {
            return true;
        }
        if from.is_integral() && to.is_integral() {
            return match (from.is_signed(), to.is_signed()) {
                (true, true) | (false, false) => to.width() >= from.width(),
                (false, true) => to.width() > from.width(),
                (true, false) => false,
            };
        }
        if from.is_integral() && to.is_float() {
            return from.width() <= 16 || (from.width() <= 32 && to == DuxType::F64);
        }
        if from == DuxType::F32 && to == DuxType::F64 {
            return true;
        }
        if from == DuxType::Ptr && to == DuxType::Str {
            return true;
        }
        false
    }

    /// Any representation change the emitter knows how to emit; `as` uses it
    /// directly, implicit paths only reach it through `lossless`.
    fn convert(&mut self, value: Typed, to: DuxType) -> Option<Typed> {
        let from = value.ty;
        if from == to {
            return Some(value);
        }
        if from.is_integral() && to.is_integral() {
            let cast = self
                .builder
                .build_int_cast_sign_flag(
                    value.int_value()?,
                    to.int_type(self.context),
                    from.is_signed(),
                    "conv",
                )
                .ok()?;
            return Some(Typed::int(cast, to));
        }
        if from.is_integral() && to.is_float() {
            let cast = if from.is_signed() {
                self.builder
                    .build_signed_int_to_float(value.int_value()?, to.float_type(self.context), "itofp")
                    .ok()?
            } else {
                self.builder
                    .build_unsigned_int_to_float(value.int_value()?, to.float_type(self.context), "itofp")
                    .ok()?
            };
            return Some(Typed::float(cast, to));
        }
        if from.is_float() && to.is_integral() {
            let cast = if to.is_signed() {
                self.builder
                    .build_float_to_signed_int(value.float_value()?, to.int_type(self.context), "fptoi")
                    .ok()?
            } else {
                self.builder
                    .build_float_to_unsigned_int(value.float_value()?, to.int_type(self.context), "fptoi")
                    .ok()?
            };
            return Some(Typed::int(cast, to));
        }
        if from.is_float() && to.is_float() {
            let cast = if to.width() > from.width() {
                self.builder
                    .build_float_ext(value.float_value()?, to.float_type(self.context), "fpext")
                    .ok()?
            } else {
                self.builder
                    .build_float_trunc(value.float_value()?, to.float_type(self.context), "fptrunc")
                    .ok()?
            };
            return Some(Typed::float(cast, to));
        }
        if from == DuxType::Bool && to.is_integral() {
            let cast = self
                .builder
                .build_int_cast_sign_flag(
                    value.int_value()?,
                    to.int_type(self.context),
                    false,
                    "conv",
                )
                .ok()?;
            return Some(Typed::int(cast, to));
        }
        if from.is_integral() && to == DuxType::Bool {
            let cast = self
                .builder
                .build_int_cast_sign_flag(
                    value.int_value()?,
                    self.context.bool_type(),
                    false,
                    "conv",
                )
                .ok()?;
            return Some(Typed::int(cast, to));
        }
        if (from == DuxType::Ptr || from == DuxType::Str) && (to == DuxType::Ptr || to == DuxType::Str) {
            return Some(Typed {
                core: value.core,
                ty: to,
            });
        }
        if from == DuxType::Ptr && to == DuxType::U64 {
            let cast = self
                .builder
                .build_ptr_to_int(value.pointer_value()?, self.context.i64_type(), "ptoi")
                .ok()?;
            return Some(Typed::int(cast, to));
        }
        if from == DuxType::U64 && to == DuxType::Ptr {
            let cast = self
                .builder
                .build_int_to_ptr(value.int_value()?, self.context.ptr_type(AddressSpace::default()), "itop")
                .ok()?;
            return Some(Typed::pointer(cast, to));
        }
        None
    }
}

impl Emitter {
    fn call(
        &mut self,
        module: &Module<'static>,
        callee: &Expr,
        args: &[Expr],
        span: Span,
    ) -> Option<Typed> {
        let Expr::Ident { name, .. } = callee else {
            self.unsupported(span, "calling anything but a plain named function");
            return None;
        };
        if name == "print" || name == "println" {
            return self.write_builtin(module, name.as_str(), args, span);
        }
        if self.skipped.contains(name.as_str()) {
            return Some(Typed::error());
        }
        let Some(function) = self.functions.get(name.as_str()).copied() else {
            self.diagnostics.push(
                Diagnostic::error(
                    code::UNEXPECTED_TOKEN,
                    format!("`{name}` is not declared in this scope"),
                )
                .with_span(callee.span())
                .with_note("methods and prelude functions land at M3"),
            );
            return None;
        };
        let (params, return_ty) = {
            let signature = self
                .signatures
                .get(name.as_str())
                .expect("a callable function has a recorded signature");
            (signature.params.clone(), signature.return_ty)
        };
        if args.len() != params.len() {
            self.diagnostics.push(
                Diagnostic::error(
                    code::UNEXPECTED_TOKEN,
                    format!(
                        "`{name}` takes {} arguments, given {}",
                        params.len(),
                        args.len()
                    ),
                )
                .with_span(span),
            );
            return None;
        }
        let mut arguments = Vec::new();
        for (parameter, arg) in params.iter().zip(args) {
            let value = self.expr(module, arg, Some(*parameter))?;
            if value.ty == DuxType::Error {
                return None;
            }
            let value = self.coerce_implicit(module, value, *parameter, arg.span())?;
            match value.basic() {
                Some(basic) => arguments.push(BasicMetadataValueEnum::from(basic)),
                None => {
                    self.unsupported(arg.span(), "passing this value");
                    return None;
                }
            }
        }
        let call = self
            .builder
            .build_call(function, &arguments, "call")
            .ok()?;
        let core = match call.try_as_basic_value() {
            ValueKind::Basic(BasicValueEnum::IntValue(value)) => Core::Int(value),
            ValueKind::Basic(BasicValueEnum::FloatValue(value)) => Core::Float(value),
            ValueKind::Basic(BasicValueEnum::PointerValue(value)) => Core::Ptr(value),
            _ => return Some(Typed::error()),
        };
        Some(Typed {
            core,
            ty: return_ty?,
        })
    }

    fn write_builtin(
        &mut self,
        module: &Module<'static>,
        name: &str,
        args: &[Expr],
        span: Span,
    ) -> Option<Typed> {
        if args.is_empty() {
            if name == "println" {
                let empty = self.string_literal("", span);
                return self.puts(module, empty).map(|_| Typed::void());
            }
            self.unsupported(span, "`print` with no argument");
            return None;
        }
        match &args[0] {
            Expr::Str { text, span: arg_span, .. } => {
                if args.len() > 1 {
                    self.unsupported(
                        span,
                        "extra arguments to a plain string (holes `$\"{}\"` take them)",
                    );
                    return None;
                }
                let literal = self.string_literal(text, *arg_span);
                if name == "println" {
                    self.puts(module, literal)
                } else {
                    self.fputs(module, literal)
                }
                .map(|_| Typed::void())
            }
            Expr::InterpString { parts, .. } => {
                if name == "println" {
                    self.write_interpolation(module, parts, &args[1..], true, span)
                } else {
                    self.write_interpolation(module, parts, &args[1..], false, span)
                }
                .map(|_| Typed::void())
            }
            other => {
                if args.len() > 1 {
                    self.unsupported(
                        span,
                        "extra arguments to a value that is not an interpolated string",
                    );
                    return None;
                }
                let value = self.expr(module, other, None)?;
                if value.ty != DuxType::Str {
                    self.unsupported(
                        other.span(),
                        &format!(
                            "printing a `{}` directly (wrap it in a $\"..\" string)",
                            value.ty.name()
                        ),
                    );
                    return None;
                }
                if name == "println" {
                    self.puts(module, value)
                } else {
                    self.fputs(module, value)
                }
                .map(|_| Typed::void())
            }
        }
    }

    fn puts(&mut self, module: &Module<'static>, value: Typed) -> Option<()> {
        let puts = self.puts_fn(module);
        let _ = self
            .builder
            .build_call(puts, &[value.pointer_value()?.into()], "write")
            .ok()?;
        Some(())
    }

    fn fputs(&mut self, module: &Module<'static>, value: Typed) -> Option<()> {
        let fputs = self.fputs_fn(module);
        let stream = self.stdout_stream(module)?;
        let _ = self
            .builder
            .build_call(
                fputs,
                &[value.pointer_value()?.into(), stream.into()],
                "write",
            )
            .ok()?;
        Some(())
    }

    fn write_interpolation(
        &mut self,
        module: &Module<'static>,
        parts: &[InterpolationPart],
        positional: &[Expr],
        newline: bool,
        span: Span,
    ) -> Option<()> {
        let mut format = String::new();
        let mut values: Vec<Typed> = Vec::new();
        let mut next_positional = 0usize;
        for part in parts {
            match part {
                InterpolationPart::Text(text) => {
                    for c in text.chars() {
                        if c == '%' {
                            format.push_str("%%");
                        } else {
                            format.push(c);
                        }
                    }
                }
                InterpolationPart::Hole {
                    written: Some(expr), ..
                } => {
                    let value = self.expr(module, expr, None)?;
                    values.push(value);
                    format.push('%');
                    format.push_str(spec_for(value.ty)?);
                }
                InterpolationPart::Hole { written: None, .. } => {
                    let Some(expr) = positional.get(next_positional) else {
                        self.diagnostics.push(
                            Diagnostic::error(
                                code::UNEXPECTED_TOKEN,
                                "a `\"{}\"` hole has no argument to fill it",
                            )
                            .with_span(span),
                        );
                        return None;
                    };
                    next_positional += 1;
                    let value = self.expr(module, expr, None)?;
                    values.push(value);
                    format.push('%');
                    format.push_str(spec_for(value.ty)?);
                }
            }
        }
        if next_positional < positional.len() {
            self.diagnostics.push(
                Diagnostic::error(
                    code::UNEXPECTED_TOKEN,
                    format!(
                        "{} arguments given for {} holes",
                        positional.len(),
                        next_positional
                    ),
                )
                .with_span(span),
            );
            return None;
        }
        if newline {
            format.push('\n');
        }
        let format_ptr = match self.builder.build_global_string_ptr(&format, "fmt") {
            Ok(global) => global.as_pointer_value(),
            Err(e) => {
                self.diagnostics.push(
                    Diagnostic::error(
                        code::CODEGEN_FAILED,
                        format!("failed to emit the format string: {e}"),
                    )
                    .with_span(span),
                );
                return None;
            }
        };
        let mut arguments: Vec<BasicMetadataValueEnum> = vec![format_ptr.into()];
        for value in values {
            let prepared = self.prepare_for_format(value)?;
            arguments.push(prepared.into());
        }
        let printf = self.printf_fn(module);
        let _ = self
            .builder
            .build_call(printf, &arguments, "printf")
            .ok()?;
        Some(())
    }

    fn prepare_for_format(&mut self, value: Typed) -> Option<BasicValueEnum<'static>> {
        match value.ty {
            DuxType::Bool => {
                let true_text = self
                    .builder
                    .build_global_string_ptr("true", "tf")
                    .ok()?
                    .as_pointer_value();
                let false_text = self
                    .builder
                    .build_global_string_ptr("false", "tf")
                    .ok()?
                    .as_pointer_value();
                let selected = self
                    .builder
                    .build_select(value.int_value()?, true_text, false_text, "boolstr")
                    .ok()?;
                Some(selected.into_pointer_value().into())
            }
            DuxType::I8 | DuxType::I16 => {
                let widened = self
                    .builder
                    .build_int_cast_sign_flag(
                        value.int_value()?,
                        self.context.i32_type(),
                        true,
                        "promote",
                    )
                    .ok()?;
                Some(widened.into())
            }
            DuxType::U8 | DuxType::U16 => {
                let widened = self
                    .builder
                    .build_int_cast_sign_flag(
                        value.int_value()?,
                        self.context.i32_type(),
                        false,
                        "promote",
                    )
                    .ok()?;
                Some(widened.into())
            }
            DuxType::F32 => {
                let widened = self
                    .builder
                    .build_float_ext(value.float_value()?, self.context.f64_type(), "promote")
                    .ok()?;
                Some(widened.into())
            }
            _ => value.basic(),
        }
    }
}

impl Emitter {
    fn load(&mut self, ty: DuxType, slot: PointerValue<'static>) -> Option<Typed> {
        let loaded = self
            .builder
            .build_load(ty.llvm(self.context), slot, "load")
            .ok()?;
        let core = match loaded {
            BasicValueEnum::IntValue(value) => Core::Int(value),
            BasicValueEnum::FloatValue(value) => Core::Float(value),
            BasicValueEnum::PointerValue(value) => Core::Ptr(value),
            _ => return None,
        };
        Some(Typed { core, ty })
    }

    fn alloca(&mut self, ty: DuxType, name: &str) -> PointerValue<'static> {
        let Some(entry) = self.allocas else {
            return self
                .builder
                .build_alloca(ty.llvm(self.context), name)
                .expect("alloca outside a function");
        };
        let current = self.builder.get_insert_block();
        match entry.get_terminator() {
            Some(terminator) => self.builder.position_before(&terminator),
            None => self.builder.position_at_end(entry),
        }
        let slot = self
            .builder
            .build_alloca(ty.llvm(self.context), name)
            .expect("alloca in the entry block");
        if let Some(block) = current {
            self.builder.position_at_end(block);
        }
        slot
    }

    fn lookup(&self, name: &str) -> Option<&(DuxType, PointerValue<'static>)> {
        self.scopes.iter().rev().find_map(|scope| scope.get(name))
    }

    fn new_block(&self, name: &str) -> BasicBlock<'static> {
        let function = self.builder.get_insert_block().unwrap().get_parent().unwrap();
        self.context.append_basic_block(function, name)
    }

    fn br(&mut self, target: BasicBlock<'static>) {
        if self
            .builder
            .get_insert_block()
            .is_some_and(|block| block.get_terminator().is_none())
        {
            let _ = self.builder.build_unconditional_branch(target);
        }
    }

    fn printf_fn(&mut self, module: &Module<'static>) -> FunctionValue<'static> {
        if let Some(printf) = self.printf {
            return printf;
        }
        let c_char = self.context.ptr_type(AddressSpace::default());
        let printf = self
            .context
            .i32_type()
            .fn_type(&[c_char.into()], true);
        let fn_value = module.add_function("printf", printf, None);
        self.printf = Some(fn_value);
        fn_value
    }

    fn puts_fn(&mut self, module: &Module<'static>) -> FunctionValue<'static> {
        if let Some(puts) = self.puts {
            return puts;
        }
        let c_char = self.context.ptr_type(AddressSpace::default());
        let fn_type = self.context.i32_type().fn_type(&[c_char.into()], false);
        let fn_value = module.add_function("puts", fn_type, None);
        self.puts = Some(fn_value);
        fn_value
    }

    fn fputs_fn(&mut self, module: &Module<'static>) -> FunctionValue<'static> {
        if let Some(fputs) = self.fputs {
            return fputs;
        }
        let c_char = self.context.ptr_type(AddressSpace::default());
        let fn_type = self
            .context
            .i32_type()
            .fn_type(&[c_char.into(), c_char.into()], false);
        let fn_value = module.add_function("fputs", fn_type, None);
        self.fputs = Some(fn_value);
        fn_value
    }

    fn stdout_stream(&mut self, module: &Module<'static>) -> Option<PointerValue<'static>> {
        let c_char = self.context.ptr_type(AddressSpace::default());
        let stream = match self.stdout {
            Some(stream) => stream,
            None => {
                let global = module.get_global("stdout").unwrap_or_else(|| {
                    module.add_global(BasicTypeEnum::from(c_char), None, "stdout")
                });
                self.stdout = Some(global);
                global
            }
        };
        let loaded = self
            .builder
            .build_load(c_char, stream.as_pointer_value(), "stdout")
            .ok()?;
        Some(loaded.into_pointer_value())
    }

    fn unsupported(&mut self, span: Span, what: &str) {
        self.diagnostics.push(
            Diagnostic::error(
                code::UNSUPPORTED_IN_BUILD,
                format!("{what} is not supported by this compiler build yet"),
            )
            .with_span(span)
            .with_note("this is a milestone limitation, not a language decision"),
        );
    }
}

fn int_predicate(op: BinaryOp, signed: bool) -> Option<IntPredicate> {
    Some(match op {
        BinaryOp::Eq => IntPredicate::EQ,
        BinaryOp::NotEq => IntPredicate::NE,
        BinaryOp::Lt if signed => IntPredicate::SLT,
        BinaryOp::Lt => IntPredicate::ULT,
        BinaryOp::Gt if signed => IntPredicate::SGT,
        BinaryOp::Gt => IntPredicate::UGT,
        BinaryOp::Le if signed => IntPredicate::SLE,
        BinaryOp::Le => IntPredicate::ULE,
        BinaryOp::Ge if signed => IntPredicate::SGE,
        BinaryOp::Ge => IntPredicate::UGE,
        _ => return None,
    })
}

fn float_predicate(op: BinaryOp) -> Option<FloatPredicate> {
    Some(match op {
        BinaryOp::Eq => FloatPredicate::OEQ,
        BinaryOp::NotEq => FloatPredicate::ONE,
        BinaryOp::Lt => FloatPredicate::OLT,
        BinaryOp::Gt => FloatPredicate::OGT,
        BinaryOp::Le => FloatPredicate::OLE,
        BinaryOp::Ge => FloatPredicate::OGE,
        _ => return None,
    })
}

fn compound_op(op: dux_ast::AssignOp) -> Option<BinaryOp> {
    use dux_ast::AssignOp;
    Some(match op {
        AssignOp::Add => BinaryOp::Add,
        AssignOp::Sub => BinaryOp::Sub,
        AssignOp::Mul => BinaryOp::Mul,
        AssignOp::Div => BinaryOp::Div,
        AssignOp::Mod => BinaryOp::Mod,
        AssignOp::Set => return None,
    })
}

fn spec_for(ty: DuxType) -> Option<&'static str> {
    Some(match ty {
        DuxType::Bool => "s",
        DuxType::I8 | DuxType::I16 | DuxType::I32 => "d",
        DuxType::U8 | DuxType::U16 | DuxType::U32 => "u",
        DuxType::I64 => "ld",
        DuxType::U64 => "lu",
        DuxType::F32 | DuxType::F64 => "g",
        DuxType::Str => "s",
        DuxType::Ptr => "p",
        DuxType::Error => return None,
    })
}

fn expression_name(expr: &Expr) -> &'static str {
    match expr {
        Expr::Field { .. } => "a field access (lands at M3)",
        Expr::Index { .. } => "an index (lands at M3)",
        Expr::Member { .. } => "a tuple member (lands at M3)",
        Expr::Range { .. } => "a range (lands at M3)",
        Expr::Path { .. } => "a qualified path",
        Expr::StructLiteral { .. } => "a struct value (lands at M3)",
        Expr::ArrayLiteral { .. } => "an array value (lands at M3)",
        Expr::RepeatedArray { .. } => "a repeated array value (lands at M3)",
        Expr::If { .. } => "an `if` expression (value-position control flow)",
        Expr::Block { .. } => "a block in value position",
        Expr::CondBinding { .. } => "a pattern condition (lands at M4)",
        Expr::Error { .. } => "an erroneous expression",
        _ => "this expression",
    }
}
