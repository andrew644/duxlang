//! The type checker. Where the resolver proves that names exist, this pass
//! proves that values fit: the D25 conversion lattice, untyped literals,
//! conditions, and the `fn` signature contract.
//!
//! The rule that keeps this pass honest is the same one the resolver lives
//! by: report only what is provably wrong *today*. Anything a later
//! milestone owns (struct fields in depth, patterns, generic instantiation,
//! C types, pointer targets) gets [`Ty::Opaque`], which never errors. A
//! checker that guesses is worse than a checker that waits.

use std::collections::HashMap;

use dux_ast::{
    AssignOp, BinaryOp, Block, Decl, Expr, ForKind, ImplDecl, Program, Stmt, Type, TypeAliasDecl,
    UnaryOp,
};
use dux_diag::code;
use dux_diag::Diagnostic;
use dux_source::Span;

/// A type as the checker sees it. Primitives mirror the codegen lattice so
/// the two passes can never disagree about `lossless`; `IntConst` and
/// `FloatConst` are the untyped literals of D17, which adopt an integer or
/// float target when the value fits and default to `i32`/`f64` otherwise.
#[derive(Clone, Debug, PartialEq)]
pub enum Ty {
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
    Str,
    Ptr,
    Unit,
    IntConst(i128),
    FloatConst(f64),
    Array(Box<Ty>),
    Struct(String),
    /// Unknown to the checker today: an imported or C type, a generic
    /// instantiation, a pointer target, a field of a shape M3 will know.
    /// Silences every rule it touches.
    Opaque,
}

impl Ty {
    fn name(&self) -> String {
        match self {
            Ty::Bool => "bool".to_string(),
            Ty::I8 => "i8".to_string(),
            Ty::I16 => "i16".to_string(),
            Ty::I32 => "i32".to_string(),
            Ty::I64 => "i64".to_string(),
            Ty::U8 => "u8".to_string(),
            Ty::U16 => "u16".to_string(),
            Ty::U32 => "u32".to_string(),
            Ty::U64 => "u64".to_string(),
            Ty::F32 => "f32".to_string(),
            Ty::F64 => "f64".to_string(),
            Ty::Str => "string".to_string(),
            Ty::Ptr => "rawptr".to_string(),
            Ty::Unit => "()".to_string(),
            Ty::IntConst(value) => format!("integer literal {value}"),
            Ty::FloatConst(value) => format!("float literal {value}"),
            Ty::Array(element) => format!("[{}]", element.name()),
            Ty::Struct(name) => name.clone(),
            Ty::Opaque => "<opaque>".to_string(),
        }
    }

    fn is_integral(&self) -> bool {
        matches!(
            self,
            Ty::I8 | Ty::I16 | Ty::I32 | Ty::I64 | Ty::U8 | Ty::U16 | Ty::U32 | Ty::U64
        )
    }

    fn is_signed(&self) -> bool {
        matches!(self, Ty::I8 | Ty::I16 | Ty::I32 | Ty::I64)
    }

    fn is_float(&self) -> bool {
        matches!(self, Ty::F32 | Ty::F64)
    }

    fn is_numeric(&self) -> bool {
        self.is_integral() || self.is_float()
    }

    fn width(&self) -> u32 {
        match self {
            Ty::Bool => 1,
            Ty::I8 | Ty::U8 => 8,
            Ty::I16 | Ty::U16 => 16,
            Ty::I32 | Ty::U32 | Ty::F32 => 32,
            _ => 64,
        }
    }

    /// An untyped value takes its type from the role it fills; with no role
    /// it defaults exactly as the codegen defaults: small integers to `i32`,
    /// bigger ones up the signed ladder, floats to `f64`.
    fn defaulted(&self) -> Ty {
        match self {
            Ty::IntConst(value) => {
                if (-i32::MAX as i128 - 1..=i32::MAX as i128).contains(value) {
                    Ty::I32
                } else {
                    Ty::I64
                }
            }
            Ty::FloatConst(_) => Ty::F64,
            other => other.clone(),
        }
    }

    /// An untyped literal next to a concrete value takes that type (D17): a
    /// float literal beside an `f32` is an `f32`, so `2.0 + width` stays in
    /// the precision the program asked for. Beside an `f64`, or beside
    /// anything but a float, the literal keeps its own default.
    fn adopt(&self, other: &Ty) -> Ty {
        match self {
            Ty::FloatConst(_) if other.is_float() => other.clone(),
            concrete => concrete.clone(),
        }
    }

    /// Does this literal value fit a target of `self`? Mirrors the signed
    /// limits exactly, so `-128: i8` is fine and `128: i8` is not.
    fn fits(&self, value: i128) -> bool {
        match self {
            Ty::Bool => value == 0 || value == 1,
            Ty::I8 => (-128..=127).contains(&value),
            Ty::I16 => (i16::MIN as i128..=i16::MAX as i128).contains(&value),
            Ty::I32 => (i32::MIN as i128..=i32::MAX as i128).contains(&value),
            Ty::I64 => (i64::MIN as i128..=i64::MAX as i128).contains(&value),
            Ty::U8 => (0..=255).contains(&value),
            Ty::U16 => (0..=u16::MAX as i128).contains(&value),
            Ty::U32 => (0..=u32::MAX as i128).contains(&value),
            Ty::U64 => (0..=i128::from(u64::MAX)).contains(&value),
            Ty::F32 | Ty::F64 => true,
            _ => false,
        }
    }

    /// The free conversions of D25: the destination's value set must hold the
    /// source's. Anything else is written `as`, and the checker says so.
    fn lossless(&self, to: &Ty) -> bool {
        if self == to {
            return true;
        }
        if self.is_integral() && to.is_integral() {
            return match (self.is_signed(), to.is_signed()) {
                (true, true) | (false, false) => to.width() >= self.width(),
                (false, true) => to.width() > self.width(),
                (true, false) => false,
            };
        }
        if self.is_integral() && to.is_float() {
            return self.width() <= 16 || (self.width() <= 32 && *to == Ty::F64);
        }
        if *self == Ty::F32 && *to == Ty::F64 {
            return true;
        }
        if *self == Ty::Ptr && *to == Ty::Str {
            return true;
        }
        false
    }
}

fn primitive(name: &str) -> Option<Ty> {
    Some(match name {
        "bool" => Ty::Bool,
        "i8" | "byte" => Ty::I8,
        "i16" => Ty::I16,
        "i32" | "int" | "rune" => Ty::I32,
        "i64" | "long" | "isize" => Ty::I64,
        "u8" | "char" => Ty::U8,
        "u16" => Ty::U16,
        "u32" | "uint" => Ty::U32,
        "u64" | "usize" => Ty::U64,
        "f32" => Ty::F32,
        "f64" | "float" => Ty::F64,
        "string" => Ty::Str,
        "rawptr" => Ty::Ptr,
        "never" => return Some(Ty::Opaque),
        _ => return None,
    })
}

/// The universe of value-level builtins that never need a `use`.
fn builtin_result(name: &str) -> Option<Ty> {
    match name {
        "print" | "println" => Some(Ty::Unit),
        "new" | "make" | "free" | "del" => Some(Ty::Opaque),
        _ => None,
    }
}

#[derive(Clone)]
struct Signature {
    params: Vec<Ty>,
    result: Ty,
    /// An extern or generic function: the arguments are not checkable yet,
    /// but the call still has a result type.
    check_arguments: bool,
}

struct Context {
    name: String,
    ty: Ty,
    is_const: bool,
}

pub(crate) struct Checker<'a> {
    program: &'a Program,
    diagnostics: Vec<Diagnostic>,
    functions: HashMap<String, Signature>,
    methods: HashMap<(String, String), Signature>,
    structs: HashMap<String, Vec<(String, Type)>>,
    aliases: HashMap<String, Type>,
    enums: Vec<String>,
    globals: HashMap<String, Context>,
    scopes: Vec<Vec<Context>>,
    result: Ty,
    in_main: bool,
}

impl<'a> Checker<'a> {
    fn new(program: &'a Program) -> Self {
        let mut checker = Checker {
            program,
            diagnostics: Vec::new(),
            functions: HashMap::new(),
            methods: HashMap::new(),
            structs: HashMap::new(),
            aliases: HashMap::new(),
            enums: Vec::new(),
            globals: HashMap::new(),
            scopes: Vec::new(),
            result: Ty::Unit,
            in_main: false,
        };
        checker.collect_types();
        checker.collect_functions();
        checker
    }

    fn collect_types(&mut self) {
        let decls: &[Decl] = self.program.decls.as_slice();
        for decl in decls {
            match decl {
                Decl::Struct(decl) => {
                    let fields = decl
                        .fields
                        .iter()
                        .map(|field| (field.name.name.clone(), field.ty.clone()))
                        .collect();
                    self.structs.insert(decl.name.name.clone(), fields);
                }
                Decl::TypeAlias(TypeAliasDecl { name, type_params, aliased, .. }) => {
                    if type_params.is_empty() {
                        self.aliases.insert(name.name.clone(), aliased.clone());
                    }
                }
                Decl::Enum(decl) => self.enums.push(decl.name.name.clone()),
                _ => {}
            }
        }
    }

    fn collect_functions(&mut self) {
        let functions: Vec<&dux_ast::FnDecl> = self.program.functions().collect();
        for function in functions {
            let signature = self.signature(function, false);
            self.functions.insert(function.name.name.clone(), signature);
        }
        let decls: &[Decl] = self.program.decls.as_slice();
        for decl in decls {
            if let Decl::Extern(extern_block) = decl {
                for function in &extern_block.functions {
                    let signature = self.signature(function, true);
                    self.functions.insert(function.name.name.clone(), signature);
                }
            }
        }
    }

    fn signature(&self, function: &dux_ast::FnDecl, external: bool) -> Signature {
        let params = function
            .params
            .iter()
            .map(|param| self.resolve_type(&param.ty))
            .collect();
        let result = match &function.return_ty {
            Some(ty) => self.resolve_type(ty),
            None => Ty::Unit,
        };
        Signature {
            params,
            result,
            check_arguments: !external && function.type_params.is_empty(),
        }
    }

    fn resolve_type(&self, ty: &Type) -> Ty {
        self.resolve_type_depth(ty, 0)
    }

    fn resolve_type_depth(&self, ty: &Type, depth: u32) -> Ty {
        if depth > 8 {
            return Ty::Opaque;
        }
        match ty {
            Type::Named { name, args, .. } => {
                if !args.is_empty() {
                    return Ty::Opaque;
                }
                if let Some(primitive) = primitive(name) {
                    return primitive;
                }
                if self.structs.contains_key(name.as_str()) {
                    return Ty::Struct(name.clone());
                }
                if self.enums.iter().any(|known| known == name) {
                    return Ty::Opaque;
                }
                match self.aliases.get(name) {
                    Some(target) => self.resolve_type_depth(target, depth + 1),
                    None => Ty::Opaque,
                }
            }
            Type::Pointer { .. } => Ty::Ptr,
            Type::Array { element, .. } => Ty::Array(Box::new(self.resolve_type_depth(element, depth + 1))),
            Type::Slice { .. } => Ty::Opaque,
            Type::Tuple { .. } | Type::Function { .. } | Type::ConstArgument { .. } => Ty::Opaque,
            Type::Error { .. } => Ty::Opaque,
        }
    }

    fn report(&mut self, span: Span, diagnostic: Diagnostic) {
        self.diagnostics.push(diagnostic.with_span(span));
    }

    fn define(&mut self, name: &str, ty: Ty, is_const: bool) {
        if let Some(scope) = self.scopes.last_mut() {
            scope.push(Context { name: name.to_string(), ty, is_const });
        }
    }

    fn lookup(&self, name: &str) -> Option<&Context> {
        self.scopes
            .iter()
            .rev()
            .flat_map(|scope| scope.iter().rev())
            .find(|context| context.name == name)
            .or_else(|| self.globals.get(name))
    }

    fn error(&mut self, c: &'static str, message: impl Into<String>, span: Span) {
        self.report(span, Diagnostic::error(c, message));
    }

    // ---------------------------------------------------------------- types

    /// Check `value` in a position that demands `expected`. Reports the two
    /// failure modes distinctly: a literal that does not reach the type
    /// (E0204), and a typed value the type will not hold for free (E0200).
    fn coerce(&mut self, value: Ty, expected: &Ty, span: Span) -> Ty {
        if *expected == Ty::Opaque || value == Ty::Opaque {
            return expected.clone();
        }
        match (&value, expected) {
            (Ty::IntConst(literal), to) if to.is_integral() || *to == Ty::Bool => {
                if to.fits(*literal) {
                    to.clone()
                } else {
                    self.error(
                        code::LITERAL_OUT_OF_RANGE,
                        format!("the literal {literal} does not fit a `{}`", to.name()),
                        span,
                    );
                    to.clone()
                }
            }
            (Ty::IntConst(literal), to) if to.is_float() => {
                let _ = literal;
                to.clone()
            }
            (Ty::IntConst(_), to) => {
                self.mismatch(span, expected, &value);
                to.clone()
            }
            (Ty::FloatConst(_), to) if to.is_float() => to.clone(),
            (Ty::FloatConst(_), to) => {
                self.report(
                    span,
                    Diagnostic::error(
                        code::TYPE_MISMATCH,
                        format!("a float literal does not fit a `{}`", to.name()),
                    )
                    .with_note("write `as` to state the conversion you mean"),
                );
                to.clone()
            }
            (_, to) => {
                let from = value.defaulted();
                if from == *to {
                    return to.clone();
                }
                let on_the_numeric_lattice = (from.is_numeric() && to.is_numeric())
                    || (from == Ty::Ptr && *to == Ty::Str);
                if !on_the_numeric_lattice {
                    self.mismatch(span, to, &value);
                    return to.clone();
                }
                if from.lossless(to) {
                    to.clone()
                } else {
                    self.report(
                        span,
                        Diagnostic::error(
                            code::LOSSY_CONVERSION,
                            format!(
                                "a `{}` does not fit a `{}` for free; write `as {}` for the explicit conversion",
                                from.name(),
                                to.name(),
                                to.name()
                            ),
                        )
                        .with_note("D25: implicit conversion is allowed only when the destination is lossless"),
                    );
                    to.clone()
                }
            }
        }
    }

    fn mismatch(&mut self, span: Span, expected: &Ty, got: &Ty) {
        let from = got.defaulted();
        self.report(
            span,
            Diagnostic::error(
                code::TYPE_MISMATCH,
                format!("expected `{}`, found `{}`", expected.name(), from.name()),
            ),
        );
    }

    /// Two arms of an `if` expression must agree on one type.
    fn unify(&mut self, a: Ty, b: Ty, span: Span) -> Ty {
        if a == Ty::Opaque || b == Ty::Opaque {
            return Ty::Opaque;
        }
        match (&a, &b) {
            (Ty::IntConst(left), Ty::IntConst(right)) => {
                if left == right {
                    Ty::IntConst(*left)
                } else {
                    Ty::I32
                }
            }
            (Ty::FloatConst(_), Ty::FloatConst(_)) => Ty::F64,
            (Ty::IntConst(_), Ty::FloatConst(_)) | (Ty::FloatConst(_), Ty::IntConst(_)) => {
                Ty::FloatConst(0.0)
            }
            (Ty::IntConst(literal), other) | (other, Ty::IntConst(literal)) => {
                let other = other.clone();
                self.coerce(Ty::IntConst(*literal), &other, span)
            }
            (Ty::FloatConst(literal), other) | (other, Ty::FloatConst(literal)) => {
                let other = other.clone();
                self.coerce(Ty::FloatConst(*literal), &other, span)
            }
            _ => {
                let a = a.defaulted();
                let b = b.defaulted();
                if a == b {
                    a
                } else {
                    self.report(
                        span,
                        Diagnostic::error(
                            code::TYPE_MISMATCH,
                            format!(
                                "the arms of an `if` value must agree: `{}` vs `{}`",
                                a.name(),
                                b.name()
                            ),
                        ),
                    );
                    Ty::Opaque
                }
            }
        }
    }

    // ------------------------------------------------------------- operators

    fn binary(&mut self, op: BinaryOp, lhs: &Expr, rhs: &Expr, span: Span) -> Ty {
        match op {
            BinaryOp::And | BinaryOp::Or => {
                let symbol = if op == BinaryOp::And { "&&" } else { "||" };
                self.condition_operand(lhs, symbol);
                self.condition_operand(rhs, symbol);
                Ty::Bool
            }
            _ => {
                let left = self.infer(lhs);
                let right = self.infer(rhs);
                if let Some(folded) = fold_constants(op, &left, &right) {
                    return folded;
                }
                self.binary_types(op, left, right, span)
            }
        }
    }

    fn condition_operand(&mut self, operand: &Expr, symbol: &str) {
        let ty = self.infer(operand);
        self.require_bool(ty, operand.span(), &format!("`{symbol}` needs `bool` operands"));
    }

    fn require_bool(&mut self, ty: Ty, span: Span, context: &str) {
        if ty == Ty::Opaque || ty == Ty::Bool {
            return;
        }
        if ty == Ty::Unit {
            self.report(
                span,
                Diagnostic::error(
                    code::NOT_BOOLEAN,
                    format!("{context}: this results in nothing"),
                ),
            );
            return;
        }
        let ty = ty.defaulted();
        self.report(
            span,
            Diagnostic::error(
                code::NOT_BOOLEAN,
                format!("a condition must be a `bool`, found `{}`", ty.name()),
            )
            .with_note(context.to_string()),
        );
    }

    fn binary_types(&mut self, op: BinaryOp, left: Ty, right: Ty, span: Span) -> Ty {
        if left == Ty::Opaque || right == Ty::Opaque {
            return Ty::Opaque;
        }
        let (left, right) = (left.adopt(&right), right.adopt(&left));
        let left = left.defaulted();
        let right = right.defaulted();
        if left == Ty::Str || right == Ty::Str {
            return self.string_binary(op, left, right, span);
        }
        if left == Ty::Ptr || right == Ty::Ptr {
            return self.pointer_binary(op, left, right, span);
        }
        if left == Ty::Unit || right == Ty::Unit {
            self.error(
                code::TYPE_MISMATCH,
                format!(
                    "there is no `{}` between `{}` and `{}`",
                    symbol(op),
                    left.name(),
                    right.name()
                ),
                span,
            );
            return Ty::Opaque;
        }
        if matches!(left, Ty::Struct(_) | Ty::Array(_)) || matches!(right, Ty::Struct(_) | Ty::Array(_)) {
            if !op.is_comparison() {
                self.error(
                    code::TYPE_MISMATCH,
                    format!(
                        "there is no `{}` between `{}` and `{}`",
                        symbol(op),
                        left.name(),
                        right.name()
                    ),
                    span,
                );
                return Ty::Opaque;
            }
            if left == right {
                return Ty::Bool;
            }
            self.mismatch(span, &left, &right);
            return Ty::Opaque;
        }
        if !left.is_numeric() || !right.is_numeric() {
            let bad = if !left.is_numeric() { left } else { right };
            self.error(
                code::TYPE_MISMATCH,
                format!(
                    "there is no `{}` on `{}` values",
                    symbol(op),
                    bad.name()
                ),
                span,
            );
            return Ty::Opaque;
        }
        if (left == Ty::Bool || right == Ty::Bool) && left != right {
            self.error(
                code::TYPE_MISMATCH,
                "`bool` values do not mix with numbers; write the comparison out",
                span,
            );
            return Ty::Opaque;
        }
        let common = self.common_type(&left, &right);
        if op.is_comparison() {
            return Ty::Bool;
        }
        if left == Ty::Bool || right == Ty::Bool {
            self.error(code::TYPE_MISMATCH, "there is no arithmetic on `bool` values", span);
            return Ty::Opaque;
        }
        if op == BinaryOp::Mod && common.is_float() {
            return common;
        }
        common
    }

    fn string_binary(&mut self, op: BinaryOp, left: Ty, right: Ty, span: Span) -> Ty {
        if op == BinaryOp::Add && left == Ty::Str && right == Ty::Str {
            return Ty::Str;
        }
        if op.is_comparison() && left == Ty::Str && right == Ty::Str {
            if matches!(op, BinaryOp::Eq | BinaryOp::NotEq) {
                return Ty::Bool;
            }
            self.error(
                code::TYPE_MISMATCH,
                "`string` orders with `strings.less`, not with `<`",
                span,
            );
            return Ty::Opaque;
        }
        self.error(
            code::TYPE_MISMATCH,
            format!(
                "there is no `{}` between `{}` and `{}`",
                symbol(op),
                left.name(),
                right.name()
            ),
            span,
        );
        Ty::Opaque
    }

    fn pointer_binary(&mut self, op: BinaryOp, left: Ty, right: Ty, span: Span) -> Ty {
        let numeric = |ty: &Ty| ty.is_integral() && *ty != Ty::Bool;
        match op {
            BinaryOp::Add | BinaryOp::Sub if (left == Ty::Ptr && numeric(&right)) || (numeric(&left) && right == Ty::Ptr) => {
                Ty::Ptr
            }
            BinaryOp::Eq | BinaryOp::NotEq if left == Ty::Ptr && right == Ty::Ptr => Ty::Bool,
            _ => {
                self.error(
                    code::TYPE_MISMATCH,
                    format!(
                        "there is no `{}` between `{}` and `{}`",
                        symbol(op),
                        left.name(),
                        right.name()
                    ),
                    span,
                );
                Ty::Opaque
            }
        }
    }

    /// The same lattice the codegen uses: floats absorb ints at `f64`,
    /// mixed sign meets at the first width that holds both.
    fn common_type(&mut self, a: &Ty, b: &Ty) -> Ty {
        if a == b {
            return a.clone();
        }
        if a.is_float() || b.is_float() {
            if a.is_float() && b.is_float() {
                return if a.width() >= b.width() { a.clone() } else { b.clone() };
            }
            return Ty::F64;
        }
        if a.is_signed() == b.is_signed() {
            return if a.width() >= b.width() { a.clone() } else { b.clone() };
        }
        let (unsigned, signed) = if a.is_signed() { (b, a) } else { (a, b) };
        for width in [8u32, 16, 32, 64] {
            let candidate = if signed.is_signed() {
                match width {
                    8 => Ty::I8,
                    16 => Ty::I16,
                    32 => Ty::I32,
                    _ => Ty::I64,
                }
            } else {
                match width {
                    8 => Ty::U8,
                    16 => Ty::U16,
                    32 => Ty::U32,
                    _ => Ty::U64,
                }
            };
            if width > unsigned.width() && width >= signed.width() {
                return candidate;
            }
        }
        if a.width() >= b.width() {
            a.clone()
        } else {
            b.clone()
        }
    }

    fn unary(&mut self, op: UnaryOp, operand: &Expr, span: Span) -> Ty {
        match op {
            UnaryOp::Not => {
                let ty = self.infer(operand);
                if ty == Ty::Opaque {
                    return Ty::Opaque;
                }
                if ty != Ty::Bool {
                    let ty = ty.defaulted();
                    self.error(
                        code::NOT_BOOLEAN,
                        format!("`!` needs a `bool`, found `{}`", ty.name()),
                        span,
                    );
                    return Ty::Bool;
                }
                Ty::Bool
            }
            UnaryOp::Neg => {
                let ty = self.infer(operand);
                match ty {
                    Ty::IntConst(value) => Ty::IntConst(value.wrapping_neg()),
                    Ty::FloatConst(value) => Ty::FloatConst(-value),
                    Ty::Opaque => Ty::Opaque,
                    concrete => {
                        let concrete = concrete.defaulted();
                        if concrete.is_numeric() {
                            concrete
                        } else {
                            self.error(
                                code::TYPE_MISMATCH,
                                format!("`-` needs a number, found `{}`", concrete.name()),
                                span,
                            );
                            Ty::Opaque
                        }
                    }
                }
            }
            UnaryOp::AddressOf => {
                let _ = self.infer(operand);
                Ty::Ptr
            }
            UnaryOp::Load => {
                let _ = self.infer(operand);
                Ty::Opaque
            }
        }
    }

    // ---------------------------------------------------------- expressions

    /// Infer the type of `expr` with no particular expectation.
    fn infer(&mut self, expr: &Expr) -> Ty {
        self.typed(expr, None)
    }

    /// Infer `expr` in a position that demands `expected`, checking the fit.
    fn expect(&mut self, expr: &Expr, expected: &Ty) -> Ty {
        if *expected == Ty::Opaque {
            let _ = self.infer(expr);
            return Ty::Opaque;
        }
        let got = self.typed(expr, Some(expected));
        self.coerce(got, expected, expr.span())
    }

    fn typed(&mut self, expr: &Expr, expected: Option<&Ty>) -> Ty {
        match expr {
            Expr::Int { value, .. } => {
                let value = *value as i128;
                match expected {
                    Some(to) if (to.is_integral() || *to == Ty::Bool) && to.fits(value) => to.clone(),
                    Some(Ty::Opaque) => Ty::Opaque,
                    _ => Ty::IntConst(value),
                }
            }
            Expr::Float { value, .. } => match expected {
                Some(Ty::F32) => Ty::F32,
                Some(Ty::Opaque) => Ty::Opaque,
                _ => Ty::FloatConst(*value),
            },
            Expr::Bool { .. } => Ty::Bool,
            Expr::Char { value, .. } => match expected {
                Some(to) if (to.is_integral() || *to == Ty::Bool) && to.fits(*value as i128) => {
                    to.clone()
                }
                Some(Ty::Opaque) => Ty::Opaque,
                _ => Ty::IntConst(*value as i128),
            },
            Expr::Str { kind, .. } => match kind {
                dux_ast::StrKind::CString => Ty::Ptr,
                _ => Ty::Str,
            },
            Expr::Ident { name, .. } => match self.lookup(name).map(|context| context.ty.clone()) {
                Some(ty) => ty,
                None => Ty::Opaque,
            },
            Expr::Paren { inner, .. } => self.typed(inner, expected),
            Expr::Unary { op, operand, span } => self.unary(*op, operand, *span),
            Expr::Binary { op, lhs, rhs, span } => {
                let _ = expected;
                self.binary(*op, lhs, rhs, *span)
            }
            Expr::Cast { value, target, .. } => {
                let _ = self.infer(value);
                self.resolve_type(target)
            }
            Expr::Call { callee, args, span } => self.call(callee, args, *span),
            Expr::Field { base, name, .. } => {
                let base = self.infer(base);
                self.field_type(&base, &name.name, name.span)
            }
            Expr::Index { base, index, span } => {
                let base = self.infer(base);
                let index = self.infer(index);
                match &base {
                    Ty::Array(element) => {
                        self.check_index(&index, *span);
                        (**element).clone()
                    }
                    Ty::Str => {
                        self.check_index(&index, *span);
                        Ty::U8
                    }
                    Ty::Opaque | Ty::Ptr => Ty::Opaque,
            _ => {
                        self.error(
                            code::TYPE_MISMATCH,
                            format!("there is no indexing on `{}`", base.name()),
                            *span,
                        );
                        Ty::Opaque
                    }
                }
            }
            Expr::Range { start, end, .. } => {
                for bound in start.iter().chain(end.iter()) {
                    let _ = self.infer(bound);
                }
                Ty::Opaque
            }
            Expr::Member { base, .. } => {
                let _ = self.infer(base);
                Ty::Opaque
            }
            Expr::ArrayLiteral { elements, span } => self.array_literal(elements, *span, expected),
            Expr::RepeatedArray { value, len, span } => {
                let _ = self.infer(len);
                let element = match expected {
                    Some(Ty::Array(inner)) => {
                        let inner = (**inner).clone();
                        self.expect(value, &inner)
                    }
                    _ => self.infer(value),
                };
                let _ = span;
                Ty::Array(Box::new(element.defaulted()))
            }
            Expr::Path { .. } => Ty::Opaque,
            Expr::StructLiteral { name, fields, span } => {
                let declared = self.structs.get(name).cloned();
                for (field, value) in fields {
                    match &declared {
                        Some(fields) => match fields.iter().find(|(known, _)| known == &field.name) {
                            Some((_, ty)) => {
                                let ty = self.resolve_type(ty);
                                self.expect(value, &ty);
                            }
                            None => {
                                let _ = self.infer(value);
                                self.missing_field(name, &field.name, field.span);
                            }
                        },
                        None => {
                            let _ = self.infer(value);
                        }
                    }
                }
                let _ = span;
                match self.structs.contains_key(name.as_str()) {
                    true => Ty::Struct(name.clone()),
                    false => Ty::Opaque,
                }
            }
            Expr::InterpString { parts, .. } => {
                for part in parts {
                    if let dux_ast::InterpolationPart::Hole { written: Some(expression), .. } = part {
                        let _ = self.infer(expression);
                    }
                }
                Ty::Str
            }
            Expr::CondBinding { pattern, .. } => {
                let _ = pattern;
                Ty::Bool
            }
            Expr::If { cond, then_arm, else_arm, span } => {
                self.condition(cond);
                let then_ty = self.expression_in(then_arm, expected);
                match else_arm {
                    Some(other) => {
                        let else_ty = self.expression_in(other, expected);
                        self.unify(then_ty, else_ty, *span)
                    }
                    None => Ty::Unit,
                }
            }
            Expr::Block { block, value, .. } => {
                self.value_block(block, value);
                self.typed(value, expected)
            }
            Expr::Error { .. } => Ty::Opaque,
        }
    }

    fn expression_in(&mut self, expr: &Expr, expected: Option<&Ty>) -> Ty {
        match expected {
            Some(to) => self.expect(expr, to),
            None => self.infer(expr),
        }
    }

    fn check_index(&mut self, index: &Ty, span: Span) {
        if *index == Ty::Opaque {
            return;
        }
        let index = index.defaulted();
        if !index.is_integral() {
            self.error(
                code::TYPE_MISMATCH,
                format!("indexing needs an integer, found `{}`", index.name()),
                span,
            );
        }
    }

    fn array_literal(&mut self, elements: &[Expr], span: Span, expected: Option<&Ty>) -> Ty {
        let element = match expected {
            Some(Ty::Array(inner)) => Some((**inner).clone()),
            _ => None,
        };
        let mut known: Option<Ty> = element.clone();
        for element_expr in elements {
            match &known {
                Some(to) => {
                    self.expect(element_expr, to);
                }
                None => {
                    let first = self.infer(element_expr);
                    let first = match expected {
                        Some(expected) => self.coerce(first, expected, element_expr.span()),
                        None => first.defaulted(),
                    };
                    if !matches!(first, Ty::Opaque) {
                        known = Some(first);
                    }
                }
            }
        }
        let _ = span;
        Ty::Array(Box::new(known.unwrap_or(Ty::Opaque)))
    }

    /// The type of `base.field`. A field of a declared `struct` that does not
    /// exist is reported; anything the checker does not know the shape of
    /// stays silent.
    fn field_type(&mut self, base: &Ty, field: &str, span: Span) -> Ty {
        if let Ty::Struct(name) = base {
            if let Some(fields) = self.structs.get(name) {
                if let Some((_, ty)) = fields.iter().find(|(known, _)| known == field) {
                    return self.resolve_type(ty);
                }
                self.missing_field(name, field, span);
            }
        }
        Ty::Opaque
    }

    fn missing_field(&mut self, owner: &str, field: &str, span: Span) {
        let mut known: Vec<&str> = self
            .structs
            .get(owner)
            .map(|fields| fields.iter().map(|(known, _)| known.as_str()).collect())
            .unwrap_or_default();
        known.sort_unstable();
        self.report(
            span,
            Diagnostic::error(
                code::TYPE_MISMATCH,
                format!("`{owner}` has no field `{field}`"),
            )
            .with_note(format!("the fields of `{owner}` are {}", known.join(", "))),
        );
    }

    fn call(&mut self, callee: &Expr, args: &[Expr], span: Span) -> Ty {
        let name = match callee {
            Expr::Ident { name, .. } => Some(name.clone()),
            Expr::Paren { inner, .. } => match inner.as_ref() {
                Expr::Ident { name, .. } => Some(name.clone()),
                _ => None,
            },
            _ => None,
        };
        if let Some(name) = &name {
            if let Some(result) = builtin_result(name) {
                for arg in args {
                    let _ = self.infer(arg);
                }
                return result;
            }
            if matches!(name.as_str(), "Some" | "None" | "Ok" | "Err") {
                for arg in args {
                    let _ = self.infer(arg);
                }
                return Ty::Opaque;
            }
            if let Some(signature) = self.functions.get(name).cloned() {
                return self.call_signature(&signature, args, span);
            }
        }
        if let Expr::Field { base, name, .. } = callee {
            let base = self.infer(base);
            if let Ty::Struct(owner) = &base {
                if let Some(signature) = self.methods.get(&(owner.clone(), name.name.clone())).cloned() {
                    return self.call_signature(&signature, args, span);
                }
            }
            let _ = span;
            return Ty::Opaque;
        }
        let _ = self.infer(callee);
        for arg in args {
            let _ = self.infer(arg);
        }
        Ty::Opaque
    }

    fn call_signature(&mut self, signature: &Signature, args: &[Expr], span: Span) -> Ty {
        if signature.check_arguments && signature.params.len() == args.len() {
            for (argument, param) in args.iter().zip(&signature.params) {
                self.expect(argument, param);
            }
        } else {
            for argument in args {
                let _ = self.infer(argument);
            }
        }
        let _ = span;
        signature.result.clone()
    }

    // ----------------------------------------------------------- statements

    fn function(&mut self, function: &dux_ast::FnDecl, receiver: Option<&str>) {
        self.result = match &function.return_ty {
            Some(ty) => self.resolve_type(ty),
            None => Ty::Unit,
        };
        self.in_main = receiver.is_none() && function.name.name == "main";
        self.scopes.push(Vec::new());
        for parameter in &function.params {
            let ty = if parameter.name.name == "self" {
                match receiver {
                    Some(owner) => Ty::Struct(owner.to_string()),
                    None => self.resolve_type(&parameter.ty),
                }
            } else {
                self.resolve_type(&parameter.ty)
            };
            self.define(&parameter.name.name, ty, false);
        }
        for statement in &function.body.stmts {
            self.statement(statement);
        }
        let _ = function.body.span;
        self.scopes.pop();
    }

    fn impl_block(&mut self, block: &ImplDecl) {
        for method in &block.methods {
            self.function(method, Some(&block.type_name.name));
        }
    }

    fn block(&mut self, block: &Block) {
        self.scopes.push(Vec::new());
        for statement in &block.stmts {
            self.statement(statement);
        }
        self.scopes.pop();
    }

    /// A block in value position stores its tail expression twice: as the
    /// last statement and as the block's value. Check the statements once.
    fn value_block(&mut self, block: &Block, value: &Expr) {
        self.scopes.push(Vec::new());
        let tail = block.stmts.len().saturating_sub(1);
        for (index, statement) in block.stmts.iter().enumerate() {
            if index == tail
                && matches!(statement, Stmt::Expr { expr, .. } if expr.span() == value.span())
            {
                continue;
            }
            self.statement(statement);
        }
        self.scopes.pop();
    }

    fn statement(&mut self, statement: &Stmt) {
        match statement {
            Stmt::Binding { name, ty, value, span } => self.binding(&name.name, ty.as_ref(), value.as_ref(), *span),
            Stmt::Assign { target, op, value, span } => self.assignment(target, *op, value, *span),
            Stmt::Expr { expr, .. } => {
                let _ = self.infer(expr);
            }
            Stmt::Return { value, span } => self.return_stmt(value.as_ref(), *span),
            Stmt::Break { .. } | Stmt::Continue { .. } => {}
            Stmt::Defer { stmt, .. } => self.statement(stmt),
            Stmt::Block { block, .. } => self.block(block),
            Stmt::If { cond, then_branch, else_branch, .. } => {
                self.condition(cond);
                self.block(then_branch);
                if let Some(other) = else_branch {
                    match &**other {
                        Stmt::Block { block, .. } => self.block(block),
                        other => self.statement(other),
                    }
                }
            }
            Stmt::While { cond, body, .. } => {
                self.condition(cond);
                self.block(body);
            }
            Stmt::Loop { body, .. } => self.block(body),
            Stmt::For { kind, body, span } => {
                let _ = span;
                self.scopes.push(Vec::new());
                match kind {
                    ForKind::ThreeClause { init, cond, step } => {
                        self.statement(init);
                        self.condition(cond);
                        self.scopes.push(Vec::new());
                        for statement in &body.stmts {
                            self.statement(statement);
                        }
                        self.scopes.pop();
                        self.statement(step);
                    }
                    ForKind::In { index, value, iterable } => {
                        let is_range = matches!(iterable, Expr::Range { .. });
                        let iterable = self.infer(iterable);
                        let element = match &iterable {
                            Ty::Array(inner) => Some((**inner).clone()),
                            Ty::Str => Some(Ty::U8),
                            _ => None,
                        };
                        if let Some(index) = index {
                            let ty = if is_range { Ty::Opaque } else { Ty::I64 };
                            self.define(&index.name, ty, false);
                        }
                        let value_ty = element
                            .or(if is_range { Some(Ty::I64) } else { None })
                            .unwrap_or(Ty::Opaque);
                        self.define(&value.name, value_ty, false);
                        for statement in &body.stmts {
                            self.statement(statement);
                        }
                    }
                }
                self.scopes.pop();
            }
            Stmt::Error { .. } => {}
        }
    }

    fn condition(&mut self, cond: &Expr) {
        if matches!(cond, Expr::CondBinding { .. }) {
            return;
        }
        let ty = self.infer(cond);
        if ty == Ty::Opaque || ty == Ty::Bool {
            return;
        }
        self.require_bool(ty, cond.span(), "this condition needs a `bool`");
    }

    fn binding(&mut self, name: &str, ty: Option<&Type>, value: Option<&Expr>, span: Span) {
        let declared = ty.map(|ty| self.resolve_type(ty));
        let bound = match (&declared, value) {
            (Some(to), Some(value)) => {
                self.expect(value, to);
                to.clone()
            }
            (Some(to), None) => to.clone(),
            (None, Some(value)) => self.infer(value).defaulted(),
            (None, None) => {
                let _ = span;
                Ty::Opaque
            }
        };
        self.define(name, bound, false);
    }

    fn assignment(&mut self, target: &Expr, op: AssignOp, value: &Expr, span: Span) {
        if let Expr::Ident { name, span: name_span } = target {
            let existing = self.lookup(name).map(|context| (context.ty.clone(), context.is_const));
            match existing {
                Some((_, true)) => {
                    self.report(
                        *name_span,
                        Diagnostic::error(
                            code::ASSIGN_TO_CONST,
                            format!("`{name}` is a `const`; it cannot be assigned"),
                        )
                        .with_note("a `const` is a value, not a place; a variable that changes is a binding"),
                    );
                    return;
                }
                Some((to, false)) => {
                    if to == Ty::Ptr && matches!(op, AssignOp::Add | AssignOp::Sub) {
                        self.expect(value, &Ty::I64);
                    } else {
                        self.expect(value, &to);
                    }
                    if op != AssignOp::Set {
                        self.compound(op, &to, span);
                    }
                    return;
                }
                None => {}
            }
        }
        if let Expr::Field { base, name, .. } = target {
            if let Some(root) = place_root(target) {
                if self.lookup(&root).is_some_and(|context| context.is_const) {
                    self.report(
                        target.span(),
                        Diagnostic::error(
                            code::ASSIGN_TO_CONST,
                            format!("`{root}` is a `const`; its fields cannot be assigned"),
                        ),
                    );
                    return;
                }
            }
            let base = self.infer(base);
            let field = self.field_type(&base, &name.name, name.span);
            if field != Ty::Opaque {
                self.expect(value, &field);
                if op != AssignOp::Set {
                    self.compound(op, &field, span);
                }
                return;
            }
            let _ = self.infer(value);
            return;
        }
        match target {
            Expr::Index { base, .. } => {
                let _ = self.infer(base);
            }
            Expr::Unary { operand, .. } => {
                let _ = self.infer(operand);
            }
            _ => {}
        }
        let _ = self.infer(value);
    }

    fn compound(&mut self, op: AssignOp, target: &Ty, span: Span) {
        if *target == Ty::Opaque || target.is_numeric() || *target == Ty::Ptr || *target == Ty::Str {
            return;
        }
        self.error(
            code::TYPE_MISMATCH,
            format!(
                "`{}=` needs a number, not a `{}`",
                match op {
                    AssignOp::Add => "+",
                    AssignOp::Sub => "-",
                    AssignOp::Mul => "*",
                    AssignOp::Div => "/",
                    AssignOp::Mod => "%",
                    AssignOp::Set => "",
                },
                target.name()
            ),
            span,
        );
    }

    fn return_stmt(&mut self, value: Option<&Expr>, span: Span) {
        let result = self.result.clone();
        match (value, &result) {
            (Some(value), Ty::Unit) => {
                let _ = self.infer(value);
                self.error(
                    code::TYPE_MISMATCH,
                    "`return` with a value in a function that results in nothing",
                    span,
                );
            }
            (Some(value), to) => {
                self.expect(value, to);
            }
            (None, Ty::Unit) => {}
            (None, to) if self.in_main || to.is_integral() => {}
            (None, to) => {
                let _ = to;
                self.error(
                    code::TYPE_MISMATCH,
                    "`return` needs a value here",
                    span,
                );
            }
        }
    }

    // ---------------------------------------------------------------- driver

    fn run(mut self) -> Vec<Diagnostic> {
        let decls: &[Decl] = self.program.decls.as_slice();
        for decl in decls {
            match decl {
                Decl::Const(decl) => {
                    let declared = decl.ty.as_ref().map(|ty| self.resolve_type(ty));
                    let bound = match &declared {
                        Some(to) => {
                            self.expect(&decl.value, to);
                            to.clone()
                        }
                        None => self.infer(&decl.value),
                    };
                    self.globals.insert(
                        decl.name.name.clone(),
                        Context { name: decl.name.name.clone(), ty: bound, is_const: true },
                    );
                }
                Decl::Global(decl) => {
                    let declared = decl.ty.as_ref().map(|ty| self.resolve_type(ty));
                    let bound = match (&declared, &decl.value) {
                        (Some(to), Some(value)) => {
                            self.expect(value, to);
                            to.clone()
                        }
                        (Some(to), None) => to.clone(),
                        (None, Some(value)) => self.infer(value).defaulted(),
                        (None, None) => Ty::Opaque,
                    };
                    self.globals.insert(
                        decl.name.name.clone(),
                        Context { name: decl.name.name.clone(), ty: bound, is_const: false },
                    );
                }
                _ => {}
            }
        }
        for block in decls {
            if let Decl::Impl(block) = block {
                for method in &block.methods {
                    let mut signature = self.signature(method, false);
                    if !method.params.is_empty() && method.params[0].name.name == "self" {
                        signature.params.remove(0);
                    }
                    self.methods.insert(
                        (block.type_name.name.clone(), method.name.name.clone()),
                        signature,
                    );
                }
            }
        }
        for decl in decls {
            match decl {
                Decl::Fn(function) => self.function(function, None),
                Decl::Impl(block) => self.impl_block(block),
                _ => {}
            }
        }
        self.diagnostics
    }
}

/// Untyped constants keep their untyped-ness through arithmetic (D17), so a
/// `const KI = 1024` used as `page: usize = KI + 1` can still adopt `usize`.
/// Folding happens only over literal constants; a typed or opaque operand
/// drops into the ordinary lattice. Overflow falls back to `None` so the
/// concrete path reports it, never a silently wrapped value.
fn fold_constants(op: BinaryOp, left: &Ty, right: &Ty) -> Option<Ty> {
    match (left, right) {
        (Ty::IntConst(a), Ty::IntConst(b)) => {
            if op.is_comparison() {
                return Some(Ty::Bool);
            }
            let value = match op {
                BinaryOp::Add => a.checked_add(*b),
                BinaryOp::Sub => a.checked_sub(*b),
                BinaryOp::Mul => a.checked_mul(*b),
                BinaryOp::Div => a.checked_div(*b),
                BinaryOp::Mod => a.checked_rem(*b),
                _ => None,
            }?;
            Some(Ty::IntConst(value))
        }
        (Ty::FloatConst(a), Ty::FloatConst(b)) => float_fold(op, *a, *b),
        (Ty::IntConst(a), Ty::FloatConst(b)) => float_fold(op, *a as f64, *b),
        (Ty::FloatConst(a), Ty::IntConst(b)) => float_fold(op, *a, *b as f64),
        _ => None,
    }
}

fn float_fold(op: BinaryOp, a: f64, b: f64) -> Option<Ty> {
    if op.is_comparison() {
        return Some(Ty::Bool);
    }
    let value = match op {
        BinaryOp::Add => a + b,
        BinaryOp::Sub => a - b,
        BinaryOp::Mul => a * b,
        BinaryOp::Div => a / b,
        BinaryOp::Mod => a % b,
        _ => return None,
    };
    Some(Ty::FloatConst(value))
}

/// The name a place expression is rooted in: `unit.min.x` is rooted in
/// `unit`, so a `const` record keeps its fields frozen too.
fn place_root(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Ident { name, .. } => Some(name.clone()),
        Expr::Paren { inner, .. } => place_root(inner),
        Expr::Field { base, .. } | Expr::Index { base, .. } => place_root(base),
        _ => None,
    }
}

fn symbol(op: BinaryOp) -> &'static str {
    match op {
        BinaryOp::Add => "+",
        BinaryOp::Sub => "-",
        BinaryOp::Mul => "*",
        BinaryOp::Div => "/",
        BinaryOp::Mod => "%",
        BinaryOp::Eq => "==",
        BinaryOp::NotEq => "!=",
        BinaryOp::Lt => "<",
        BinaryOp::Gt => ">",
        BinaryOp::Le => "<=",
        BinaryOp::Ge => ">=",
        BinaryOp::And => "&&",
        BinaryOp::Or => "||",
    }
}

/// Check one program whose names have already resolved. Silence is the
/// correct output for anything the checker cannot prove wrong yet.
pub fn check(program: &Program) -> Vec<Diagnostic> {
    Checker::new(program).run()
}
