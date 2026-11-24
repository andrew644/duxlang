//! Lowering from the AST to LLVM IR.
//!
//! The shape follows DESIGN.md §11.2: the entry block holds every alloca,
//! other instructions go in the blocks the control flow needs, and `main` is
//! a C `int main(void)` so the C runtime can start it (D12). Numeric
//! conversions follow D25: implicit exactly when the destination holds every
//! value of the source, `as` for anything else.

use std::collections::{HashMap, HashSet};

use dux_ast::{
    BinaryOp, Block, ConstDecl, Decl, Expr, FnDecl, GlobalDecl, InterpolationPart, Program, Stmt,
    Type, UnaryOp,
};
use dux_diag::{code, Diagnostic};
use dux_source::Span;
use inkwell::basic_block::BasicBlock;
use inkwell::builder::Builder;
use inkwell::context::Context;
use inkwell::module::Module;
use inkwell::types::{BasicMetadataTypeEnum, BasicType, BasicTypeEnum, FloatType, IntType};
use inkwell::values::{
    BasicMetadataValueEnum, BasicValueEnum, FloatValue, GlobalValue, IntValue, PointerValue,
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
    /// A fixed array `[T; N]`. The LLVM representation is always a pointer to
    /// `[N x T]` storage; bindings and assignments that must copy do so
    /// element by element.
    Array(&'static ArrayInfo),
    /// Something we could not type; it silences follow-on diagnostics.
    Error,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ArrayInfo {
    pub(crate) elem: DuxType,
    pub(crate) len: u64,
}

impl DuxType {
    fn from_ast(ty: &Type) -> Option<DuxType> {
        match ty {
            Type::Named { name, .. } => DuxType::named(name),
            Type::Pointer { .. } => Some(DuxType::Ptr),
            Type::Array { element, len, .. } => {
                let element = DuxType::from_ast(element)?;
                let Expr::Int { value, .. } = len.as_ref() else {
                    return None;
                };
                Some(DuxType::array(element, u64::try_from(*value).ok()?))
            }
            _ => None,
        }
    }

    fn array(elem: DuxType, len: u64) -> DuxType {
        DuxType::Array(Box::leak(Box::new(ArrayInfo { elem, len })))
    }

    fn array_info(self) -> Option<&'static ArrayInfo> {
        match self {
            DuxType::Array(info) => Some(info),
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
            DuxType::Array(info) => info.elem.llvm(context).array_type(info.len as u32).into(),
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

    fn name(self) -> String {
        match self {
            DuxType::Bool => "bool".to_string(),
            DuxType::I8 => "i8".to_string(),
            DuxType::I16 => "i16".to_string(),
            DuxType::I32 => "i32".to_string(),
            DuxType::I64 => "i64".to_string(),
            DuxType::U8 => "u8".to_string(),
            DuxType::U16 => "u16".to_string(),
            DuxType::U32 => "u32".to_string(),
            DuxType::U64 => "u64".to_string(),
            DuxType::F32 => "f32".to_string(),
            DuxType::F64 => "f64".to_string(),
            DuxType::Str => "string".to_string(),
            DuxType::Ptr => "rawptr".to_string(),
            DuxType::Array(info) => format!("[{}; {}]", info.elem.name(), info.len),
            DuxType::Error => "<error>".to_string(),
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

#[derive(Clone)]
enum ConstValue {
    Int(i128),
    Float(f64),
    Bool(bool),
    Str(String),
}

#[derive(Clone)]
struct Const {
    ty: DuxType,
    value: ConstValue,
}

impl ConstValue {
    fn checked(self, ty: DuxType, span: Span, emitter: &mut Emitter) -> Option<ConstValue> {
        match self {
            ConstValue::Int(value) => {
                if fits_i128(value, ty) {
                    Some(ConstValue::Int(value))
                } else {
                    emitter.diagnostics.push(
                        Diagnostic::error(
                            code::LITERAL_OUT_OF_RANGE,
                            format!(
                                "the constant {value} does not fit a `{}`",
                                ty.name()
                            ),
                        )
                        .with_span(span),
                    );
                    None
                }
            }
            other => Some(other),
        }
    }
}

fn fits_i128(value: i128, ty: DuxType) -> bool {
    match ty {
        DuxType::Bool => value == 0 || value == 1,
        DuxType::I8 => i8::try_from(value).is_ok(),
        DuxType::I16 => i16::try_from(value).is_ok(),
        DuxType::I32 => i32::try_from(value).is_ok(),
        DuxType::I64 => i64::try_from(value).is_ok(),
        DuxType::U8 => u8::try_from(value).is_ok(),
        DuxType::U16 => u16::try_from(value).is_ok(),
        DuxType::U32 => u32::try_from(value).is_ok(),
        DuxType::U64 => u64::try_from(value).is_ok(),
        _ => true,
    }
}

#[derive(Clone)]
struct Signature {
    params: Vec<DuxType>,
    return_ty: Option<DuxType>,
}

/// A call captured by `defer`: its arguments were evaluated at the point of
/// the `defer` statement and live in slots; the epilogue replays the call.
enum Deferred {
    Write {
        newline: bool,
        parts: Vec<InterpolationPart>,
        slots: Vec<(DuxType, PointerValue<'static>)>,
        span: Span,
    },
    Call {
        name: String,
        slots: Vec<(DuxType, PointerValue<'static>)>,
    },
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
    consts: HashMap<String, Const>,
    globals: HashMap<String, (DuxType, GlobalValue<'static>)>,
    loops: Vec<Loop>,
    defers: Vec<Deferred>,
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
            consts: HashMap::new(),
            globals: HashMap::new(),
            loops: Vec::new(),
            defers: Vec::new(),
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
                Decl::Const(decl) => self.declare_const(decl),
                Decl::Global(decl) => self.declare_global(module, decl),
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
                        code::DUPLICATE_DEFINITION,
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
            let Some(ty) = DuxType::from_ast(&param.ty).filter(|ty| ty.array_info().is_none()) else {
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
            Some(ty) => match DuxType::from_ast(ty).filter(|ty| ty.array_info().is_none()) {
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
        self.defers.clear();
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
            self.fire_defers(module);
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
                        code::TYPE_MISMATCH,
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
            Stmt::For { kind, body, .. } => {
                use dux_ast::ForKind;
                match kind {
                    ForKind::ThreeClause { init, cond, step } => {
                        self.for_stmt(module, init, cond, step, body)
                    }
                    ForKind::In {
                        index,
                        value,
                        iterable,
                    } => self.for_in_stmt(module, index.as_ref(), value, iterable, body),
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
            Stmt::Defer { stmt, span } => self.defer_stmt(module, stmt, *span),
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
                    match (ty.array_info(), value.core) {
                        (Some(info), Core::Ptr(source)) => self.copy_array(slot, source, info),
                        (None, _) => match value.basic() {
                            Some(basic) => {
                                let _ = self.builder.build_store(slot, basic);
                            }
                            None => self.unsupported(span, "storing this value"),
                        },
                        (Some(_), _) => self.unsupported(span, "storing this array value"),
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
        let (ty, slot) = match target {
            Expr::Ident { name, span: target_span } => match self.slot_of(name) {
                Some(target) => target,
                None => {
                    let message = if self.consts.contains_key(name.as_str()) {
                        format!("`{name}` is a `const`; it cannot be assigned")
                    } else {
                        format!("`{name}` is not declared in this scope")
                    };
                    self.diagnostics.push(
                        Diagnostic::error(
                            if self.consts.contains_key(name.as_str()) {
                                code::ASSIGN_TO_CONST
                            } else {
                                code::UNDECLARED
                            },
                            message,
                        )
                        .with_span(*target_span)
                        .with_note("the checker reports undeclared names; this path only fires when codegen is driven directly"),
                    );
                    return;
                }
            },
            Expr::Index { base, index, .. } => {
                match self.place_of(module, base, index) {
                    Some(place) => place,
                    None => return,
                }
            }
            _ => {
                self.unsupported(
                    target.span(),
                    "assigning to anything but a local or an array element",
                );
                return;
            }
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
        match (ty.array_info(), final_value.core) {
            (Some(info), Core::Ptr(source)) => self.copy_array(slot, source, info),
            (None, _) => match final_value.basic() {
                Some(basic) => {
                    let _ = self.builder.build_store(slot, basic);
                }
                None => self.unsupported(span, "storing this value"),
            },
            (Some(_), _) => self.unsupported(span, "storing this array value"),
        }
    }

    fn return_stmt(&mut self, module: &Module<'static>, value: Option<&Expr>, span: Span) {
        self.fire_defers(module);
        match (value, self.result_ty) {
            (None, Some(ty)) => {
                if self.is_main || ty == DuxType::I32 || ty == DuxType::I64 {
                    let zero = ty.int_type(self.context).const_int(0, false);
                    let _ = self.builder.build_return(Some(&zero));
                } else {
                    self.diagnostics.push(
                        Diagnostic::error(
                            code::TYPE_MISMATCH,
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
                        code::TYPE_MISMATCH,
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

    /// `if` used as a value: both arms lower to a typed value and meet at a
    /// phi in the merge block. An arm that ends in `return`, `break`, or
    /// `panic` produces no value; that is reported, not phantomed.
    fn if_value(
        &mut self,
        module: &Module<'static>,
        cond: &Expr,
        then_arm: &Expr,
        else_arm: &Expr,
        expected: Option<DuxType>,
        span: Span,
    ) -> Option<Typed> {
        let condition = self.condition(module, cond);
        let then_block = self.new_block("if.then");
        let else_block = self.new_block("if.else");
        let merge = self.new_block("if.end");
        let _ = self
            .builder
            .build_conditional_branch(condition, then_block, else_block);

        self.builder.position_at_end(then_block);
        let then_result = self.value_arm(module, then_arm, expected);
        self.br(merge);

        self.builder.position_at_end(else_block);
        let else_result = self.value_arm(module, else_arm, expected);
        self.br(merge);

        self.builder.position_at_end(merge);
        self.dead = false;
        let Some((then_value, then_at)) = then_result else {
            self.unsupported(span, "a value `if` arm that ends without producing a value");
            return Some(Typed::error());
        };
        let Some((else_value, else_at)) = else_result else {
            self.unsupported(span, "a value `if` arm that ends without producing a value");
            return Some(Typed::error());
        };
        if then_value.ty == DuxType::Error || else_value.ty == DuxType::Error {
            return Some(Typed::error());
        }
        let common = self.common_type(then_value.ty, else_value.ty, span)?;
        if common.array_info().is_some() {
            self.unsupported(span, "an array produced by an `if` arm (arrays do not travel as values yet)");
            return Some(Typed::error());
        }
        let then_value = self.convert(then_value, common)?;
        let else_value = self.convert(else_value, common)?;
        let phi = self
            .builder
            .build_phi(common.llvm(self.context), "iftmp")
            .ok()?;
        phi.add_incoming(&[
            (&then_value.basic()?, then_at),
            (&else_value.basic()?, else_at),
        ]);
        let value = phi.as_basic_value();
        Some(if common.is_float() {
            Typed::float(value.into_float_value(), common)
        } else if common == DuxType::Ptr || common == DuxType::Str {
            Typed::pointer(value.into_pointer_value(), common)
        } else if common.is_numeric() {
            Typed::int(value.into_int_value(), common)
        } else {
            Typed::error()
        })
    }

    /// One arm of a value `if`, positioned at the start of its own region.
    /// Returns the arm's value and the block that ends the region — the phi's
    /// incoming edge must be a real predecessor of the merge.
    fn value_arm(
        &mut self,
        module: &Module<'static>,
        arm: &Expr,
        expected: Option<DuxType>,
    ) -> Option<(Typed, BasicBlock<'static>)> {
        self.dead = false;
        self.scopes.push(HashMap::new());
        let value = match arm {
            Expr::Block { block, value, .. } => {
                self.value_block_body(module, block, value);
                if self.dead {
                    None
                } else {
                    self.expr(module, value, expected)
                }
            }
            other => self.expr(module, other, expected),
        };
        self.scopes.pop();
        let at = self.builder.get_insert_block()?;
        let value = value?;
        Some((value, at))
    }

    /// Capture a deferred call. The callee and its arguments are evaluated
    /// now, at the `defer` statement; only the call itself is postponed to
    /// every exit of the enclosing function.
    fn defer_stmt(&mut self, module: &Module<'static>, stmt: &Stmt, span: Span) {
        if self.scopes.len() != 2 {
            self.unsupported(
                span,
                "`defer` inside a nested block (per-path defer chains land at M3)",
            );
            return;
        }
        let Stmt::Expr {
            expr: Expr::Call { callee, args, .. },
            ..
        } = stmt
        else {
            self.unsupported(span, "deferring anything but a call expression");
            return;
        };
        let Expr::Ident { name, .. } = callee.as_ref() else {
            self.unsupported(span, "deferring a call to anything but a plain named function");
            return;
        };
        if name == "print" || name == "println" {
            self.defer_write(module, name, args, span);
            return;
        }
        if self.skipped.contains(name.as_str()) {
            return;
        }
        let Some(signature) = self.signatures.get(name.as_str()).cloned() else {
            self.diagnostics.push(
                Diagnostic::error(
                    code::UNDECLARED,
                    format!("`{name}` is not declared in this scope"),
                )
                .with_span(callee.span()),
            );
            return;
        };
        if args.len() != signature.params.len() {
            self.diagnostics.push(
                Diagnostic::error(
                    code::WRONG_ARITY,
                    format!(
                        "`{name}` takes {} arguments, given {}",
                        signature.params.len(),
                        args.len()
                    ),
                )
                .with_span(span),
            );
            return;
        }
        let mut slots = Vec::new();
        for (index, (parameter, arg)) in
            signature.params.iter().zip(args.iter()).enumerate()
        {
            let Some(value) = self.expr(module, arg, Some(*parameter)) else {
                return;
            };
            if value.ty == DuxType::Error {
                return;
            }
            let Some(value) = self.coerce_implicit(module, value, *parameter, arg.span()) else {
                return;
            };
            let Some(slot) = self.capture(module, value, &format!("defer{index}"), arg.span())
            else {
                return;
            };
            slots.push(slot);
        }
        self.defers.push(Deferred::Call {
            name: name.clone(),
            slots,
        });
    }

    fn defer_write(
        &mut self,
        module: &Module<'static>,
        name: &str,
        args: &[Expr],
        span: Span,
    ) {
        let newline = name == "println";
        let (parts, values) = match args.first() {
            None => {
                if name == "print" {
                    self.unsupported(span, "`print` with no argument");
                    return;
                }
                (Vec::new(), Vec::new())
            }
            Some(Expr::Str { text, .. }) if args.len() == 1 => (
                vec![dux_ast::InterpolationPart::Text(text.clone())],
                Vec::new(),
            ),
            Some(Expr::InterpString { parts, .. }) => {
                match self.lower_holes(module, parts, &args[1..], span) {
                    Some(values) => (parts.clone(), values),
                    None => return,
                }
            }
            Some(other) => {
                if args.len() > 1 {
                    self.unsupported(
                        span,
                        "extra arguments to a value that is not an interpolated string",
                    );
                    return;
                };
                let Some(value) = self.expr(module, other, None) else {
                    return;
                };
                if value.ty != DuxType::Str {
                    self.unsupported(
                        other.span(),
                        &format!(
                            "printing a `{}` directly (wrap it in a $\"..\" string)",
                            value.ty.name()
                        ),
                    );
                    return;
                }
                (
                    vec![dux_ast::InterpolationPart::Hole {
                        written: None,
                        span: other.span(),
                    }],
                    vec![value],
                )
            }
        };
        let mut slots = Vec::new();
        for (index, value) in values.into_iter().enumerate() {
            let Some(slot) = self.capture(module, value, &format!("defer{index}"), span) else {
                return;
            };
            slots.push(slot);
        }
        self.defers.push(Deferred::Write {
            newline,
            parts,
            slots,
            span,
        });
    }

    /// Store a value into a fresh slot so a deferred call can replay it.
    fn capture(
        &mut self,
        _module: &Module<'static>,
        value: Typed,
        name: &str,
        span: Span,
    ) -> Option<(DuxType, PointerValue<'static>)> {
        let slot = self.alloca(value.ty, name);
        match (value.ty.array_info(), value.core) {
            (Some(info), Core::Ptr(source)) => {
                self.copy_array(slot, source, info);
                Some((value.ty, slot))
            }
            (None, _) => {
                let Some(basic) = value.basic() else {
                    self.unsupported(span, "capturing this value for `defer`");
                    return None;
                };
                let _ = self.builder.build_store(slot, basic);
                Some((value.ty, slot))
            }
            (Some(_), _) => {
                self.unsupported(span, "capturing this array value for `defer`");
                None
            }
        }
    }

    /// Replay every deferred call at a function exit, innermost first. The
    /// list is left intact: each exit block replays the same registrations.
    fn fire_defers(&mut self, module: &Module<'static>) {
        enum Owned {
            Write {
                newline: bool,
                parts: Vec<InterpolationPart>,
                slots: Vec<(DuxType, PointerValue<'static>)>,
                span: Span,
            },
            Call {
                name: String,
                slots: Vec<(DuxType, PointerValue<'static>)>,
            },
        }
        for index in (0..self.defers.len()).rev() {
            let owned = match &self.defers[index] {
                Deferred::Write {
                    newline,
                    parts,
                    slots,
                    span,
                } => Owned::Write {
                    newline: *newline,
                    parts: parts.clone(),
                    slots: slots.clone(),
                    span: *span,
                },
                Deferred::Call { name, slots } => Owned::Call {
                    name: name.clone(),
                    slots: slots.clone(),
                },
            };
            match owned {
                Owned::Write {
                    newline,
                    parts,
                    slots,
                    span,
                } => {
                    let mut values = Vec::new();
                    for (ty, slot) in &slots {
                        let Some(value) = self.load(*ty, *slot) else {
                            continue;
                        };
                        values.push(value);
                    }
                    let _ = self.write_prepared(module, &parts, &values, newline, span);
                }
                Owned::Call { name, slots } => {
                    let Some(function) = self.functions.get(name.as_str()).copied() else {
                        continue;
                    };
                    let mut arguments = Vec::new();
                    let mut ready = true;
                    for (ty, slot) in &slots {
                        match self.load(*ty, *slot).and_then(|value| value.basic()) {
                            Some(basic) => arguments.push(BasicMetadataValueEnum::from(basic)),
                            None => {
                                ready = false;
                                break;
                            }
                        }
                    }
                    if ready {
                        let _ = self
                            .builder
                            .build_call(function, &arguments, "deferred")
                            .ok();
                    }
                }
            }
        }
    }

    /// A block in value position stores its tail expression twice: as the
    /// last statement and as the block's value. Lower the statements once.
    fn value_block_body(&mut self, module: &Module<'static>, block: &dux_ast::Block, value: &Expr) {
        self.dead = false;
        self.scopes.push(HashMap::new());
        let tail = block.stmts.len().saturating_sub(1);
        for (index, stmt) in block.stmts.iter().enumerate() {
            if self.dead {
                break;
            }
            if index == tail
                && matches!(stmt, Stmt::Expr { expr, .. } if expr.span() == value.span())
            {
                continue;
            }
            self.stmt(module, stmt);
        }
        self.scopes.pop();
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

    fn for_in_stmt(
        &mut self,
        module: &Module<'static>,
        index: Option<&dux_ast::Ident>,
        value: &dux_ast::Ident,
        iterable: &Expr,
        body: &Block,
    ) {
        self.scopes.push(HashMap::new());
        match iterable {
            Expr::Range {
                start,
                end,
                inclusive,
                ..
            } => self.for_in_range(
                module,
                index,
                value,
                start.as_deref(),
                end.as_deref(),
                *inclusive,
                iterable.span(),
                body,
            ),
            _ => self.for_in_array(module, index, value, iterable, body),
        }
        self.scopes.pop();
    }

    /// `for x in start..end` walks an `i64` counter. `break` and `continue`
    /// behave as in the three-clause loop: continue runs the step.
    #[allow(clippy::too_many_arguments)]
    fn for_in_range(
        &mut self,
        module: &Module<'static>,
        index: Option<&dux_ast::Ident>,
        value: &dux_ast::Ident,
        start: Option<&Expr>,
        end: Option<&Expr>,
        inclusive: bool,
        span: Span,
        body: &Block,
    ) {
        if index.is_some() {
            self.unsupported(
                span,
                "`for i, v in` over a range (positions belong to arrays and slices)",
            );
            self.dead = true;
            return;
        }
        let Some(end_expr) = end else {
            self.unsupported(
                span,
                "an open-ended range in `for .. in` (an unbounded loop is spelled `loop`)",
            );
            self.dead = true;
            return;
        };
        let start_value = match start {
            Some(expr) => self.expr(module, expr, Some(DuxType::I64)),
            None => Some(Typed::int(self.context.i64_type().const_zero(), DuxType::I64)),
        };
        let end_value = self.expr(module, end_expr, Some(DuxType::I64));
        let (Some(start_int), Some(end_int)) =
            (start_value.and_then(|v| v.int_value()), end_value.and_then(|v| v.int_value()))
        else {
            self.dead = true;
            return;
        };
        let counter = self.alloca(DuxType::I64, "iter");
        let limit = self.alloca(DuxType::I64, "limit");
        let _ = self.builder.build_store(counter, start_int);
        let _ = self.builder.build_store(limit, end_int);
        let value_slot = if value.name != "_" {
            let slot = self.alloca(DuxType::I64, &value.name);
            if let Some(scope) = self.scopes.last_mut() {
                scope.insert(value.name.clone(), (DuxType::I64, slot));
            }
            Some(slot)
        } else {
            None
        };

        let cond_block = self.new_block("in.cond");
        let body_block = self.new_block("in.body");
        let step_block = self.new_block("in.step");
        let exit_block = self.new_block("in.end");
        self.br(cond_block);

        self.builder.position_at_end(cond_block);
        self.dead = false;
        let Some(current) = self.load(DuxType::I64, counter).and_then(|v| v.int_value()) else {
            self.dead = true;
            return;
        };
        let Some(bound) = self.load(DuxType::I64, limit).and_then(|v| v.int_value()) else {
            self.dead = true;
            return;
        };
        let predicate = if inclusive {
            IntPredicate::SLE
        } else {
            IntPredicate::SLT
        };
        let Ok(done) = self
            .builder
            .build_int_compare(predicate, current, bound, "in.cmp")
        else {
            self.dead = true;
            return;
        };
        let _ = self
            .builder
            .build_conditional_branch(done, body_block, exit_block);

        self.builder.position_at_end(body_block);
        self.dead = false;
        if let Some(slot) = value_slot {
            if let Some(current) = self.load(DuxType::I64, counter).and_then(|v| v.basic()) {
                let _ = self.builder.build_store(slot, current);
            }
        }
        self.loops.push(Loop {
            continuation: step_block,
            exit: exit_block,
        });
        self.block_body(module, body);
        self.loops.pop();
        self.br(step_block);

        self.step_counter(step_block, counter);
        self.br(cond_block);

        self.builder.position_at_end(exit_block);
        self.dead = false;
    }

    /// `for v in xs` and `for i, v in xs` over a fixed array. The iterable is
    /// evaluated once; the counter walks `[0, len)`.
    fn for_in_array(
        &mut self,
        module: &Module<'static>,
        index: Option<&dux_ast::Ident>,
        value: &dux_ast::Ident,
        iterable: &Expr,
        body: &Block,
    ) {
        let Some(iter) = self.expr(module, iterable, None) else {
            self.dead = true;
            return;
        };
        if iter.ty == DuxType::Error {
            return;
        }
        let Some(info) = iter.ty.array_info() else {
            self.unsupported(
                iterable.span(),
                &format!(
                    "iterating a `{}` (strings and slices land at M3)",
                    iter.ty.name()
                ),
            );
            self.dead = true;
            return;
        };
        let Core::Ptr(array) = iter.core else {
            self.dead = true;
            return;
        };
        let counter = self.alloca(DuxType::I64, "iter");
        let _ = self
            .builder
            .build_store(counter, self.context.i64_type().const_zero());
        let index_slot = match index.filter(|bind| bind.name != "_") {
            Some(bind) => {
                let slot = self.alloca(DuxType::I64, &bind.name);
                if let Some(scope) = self.scopes.last_mut() {
                    scope.insert(bind.name.clone(), (DuxType::I64, slot));
                }
                Some(slot)
            }
            None => None,
        };
        let value_slot = if value.name != "_" {
            let slot = self.alloca(info.elem, &value.name);
            if let Some(scope) = self.scopes.last_mut() {
                scope.insert(value.name.clone(), (info.elem, slot));
            }
            Some(slot)
        } else {
            None
        };

        let cond_block = self.new_block("in.cond");
        let body_block = self.new_block("in.body");
        let step_block = self.new_block("in.step");
        let exit_block = self.new_block("in.end");
        self.br(cond_block);

        self.builder.position_at_end(cond_block);
        self.dead = false;
        let Some(current) = self.load(DuxType::I64, counter).and_then(|v| v.int_value()) else {
            self.dead = true;
            return;
        };
        let limit = self.context.i64_type().const_int(info.len, false);
        let Ok(more) = self
            .builder
            .build_int_compare(IntPredicate::ULT, current, limit, "in.cmp")
        else {
            self.dead = true;
            return;
        };
        let _ = self
            .builder
            .build_conditional_branch(more, body_block, exit_block);

        self.builder.position_at_end(body_block);
        self.dead = false;
        if let Some(slot) = index_slot {
            if let Some(current) = self.load(DuxType::I64, counter).and_then(|v| v.basic()) {
                let _ = self.builder.build_store(slot, current);
            }
        }
        if let Some(slot) = value_slot {
            let current = self.load(DuxType::I64, counter).and_then(|v| v.int_value());
            if let Some(i) = current {
                let array_ty = info.elem.llvm(self.context).array_type(info.len as u32);
                let zero = self.context.i64_type().const_zero();
                let element = unsafe { self.builder.build_gep(array_ty, array, &[zero, i], "elem") };
                if let Ok(at) = element {
                    if let Some(element) = self.load(info.elem, at) {
                        match (info.elem.array_info(), element.core) {
                            (Some(elem_info), Core::Ptr(source)) => {
                                self.copy_array(slot, source, elem_info)
                            }
                            (None, _) => {
                                if let Some(basic) = element.basic() {
                                    let _ = self.builder.build_store(slot, basic);
                                }
                            }
                            (Some(_), _) => {}
                        }
                    }
                }
            }
        }
        self.loops.push(Loop {
            continuation: step_block,
            exit: exit_block,
        });
        self.block_body(module, body);
        self.loops.pop();
        self.br(step_block);

        self.step_counter(step_block, counter);
        self.br(cond_block);

        self.builder.position_at_end(exit_block);
        self.dead = false;
    }

    fn step_counter(&mut self, step_block: BasicBlock<'static>, counter: PointerValue<'static>) {
        self.builder.position_at_end(step_block);
        self.dead = false;
        let Some(current) = self.load(DuxType::I64, counter).and_then(|v| v.int_value()) else {
            return;
        };
        let one = self.context.i64_type().const_int(1, false);
        if let Ok(next) = self.builder.build_int_add(current, one, "iter.next") {
            let _ = self.builder.build_store(counter, next);
        }
    }

    fn array_literal(
        &mut self,
        module: &Module<'static>,
        elements: &[Expr],
        expected: Option<DuxType>,
        span: Span,
    ) -> Option<Typed> {
        let declared = expected.and_then(DuxType::array_info);
        let Some(first) = elements.first() else {
            if let Some(info) = declared.filter(|info| info.len == 0) {
                let ty = DuxType::Array(info);
                return Some(Typed::pointer(self.alloca(ty, "arr"), ty));
            }
            let message = declared.map_or(
                "an empty array literal with no annotation to give it a type".to_string(),
                |info| {
                    format!(
                        "an empty array literal for a `[{}; {}]`",
                        info.elem.name(),
                        info.len
                    )
                },
            );
            self.unsupported(span, &message);
            return None;
        };
        if let Some(info) = declared {
            if info.len != elements.len() as u64 {
                self.diagnostics.push(
                    Diagnostic::error(
                        code::TYPE_MISMATCH,
                        format!(
                            "this array literal has {} values; the annotation wants {}",
                            elements.len(),
                            info.len
                        ),
                    )
                    .with_span(span),
                );
                return Some(Typed::error());
            }
        }
        let hint = declared.map(|info| info.elem);
        let first_value = self.expr(module, first, hint)?;
        let elem_ty = declared.map_or(first_value.ty, |info| info.elem);
        if elem_ty == DuxType::Error {
            return Some(Typed::error());
        }
        let ty = DuxType::array(elem_ty, elements.len() as u64);
        let slot = self.alloca(ty, "arr");
        let array_ty = elem_ty.llvm(self.context).array_type(elements.len() as u32);
        let zero = self.context.i64_type().const_zero();
        for (position, element) in elements.iter().enumerate() {
            let value = if position == 0 {
                first_value
            } else {
                match self.expr(module, element, Some(elem_ty)) {
                    Some(value) => value,
                    None => return Some(Typed::error()),
                }
            };
            if value.ty == DuxType::Error {
                return Some(Typed::error());
            }
            let Some(value) = self.coerce_implicit(module, value, elem_ty, span) else {
                return Some(Typed::error());
            };
            let i = self.context.i64_type().const_int(position as u64, false);
            let Ok(at) = (unsafe { self.builder.build_gep(array_ty, slot, &[zero, i], "elem") })
            else {
                return None;
            };
            match (elem_ty.array_info(), value.core) {
                (Some(info), Core::Ptr(source)) => self.copy_array(at, source, info),
                (None, _) => {
                    let Some(basic) = value.basic() else {
                        self.unsupported(span, "storing this value in an array");
                        return None;
                    };
                    let _ = self.builder.build_store(at, basic);
                }
                (Some(_), _) => {
                    self.unsupported(span, "storing this array value in an array");
                    return None;
                }
            }
        }
        Some(Typed::pointer(slot, ty))
    }

    fn repeated_array(
        &mut self,
        module: &Module<'static>,
        value: &Expr,
        len: &Expr,
        expected: Option<DuxType>,
        span: Span,
    ) -> Option<Typed> {
        let Expr::Int { value: count, .. } = len else {
            self.unsupported(
                len.span(),
                "a repeated array whose length is not a literal (constant folding lands at M3)",
            );
            return None;
        };
        let Ok(count) = u64::try_from(*count) else {
            self.unsupported(len.span(), "an array length that does not fit a `u64`");
            return None;
        };
        if count > u32::MAX as u64 {
            self.unsupported(len.span(), "an array longer than `u32::MAX` elements");
            return None;
        }
        let declared = expected.and_then(DuxType::array_info);
        if let Some(info) = declared {
            if info.len != count {
                self.diagnostics.push(
                    Diagnostic::error(
                        code::TYPE_MISMATCH,
                        format!(
                            "this repeated array has {} values; the annotation wants {}",
                            count, info.len
                        ),
                    )
                    .with_span(span),
                );
                return Some(Typed::error());
            }
        }
        let hint = declared.map(|info| info.elem);
        let fill = self.expr(module, value, hint)?;
        let elem_ty = declared.map_or(fill.ty, |info| info.elem);
        if elem_ty == DuxType::Error {
            return Some(Typed::error());
        }
        let Some(fill) = self.coerce_implicit(module, fill, elem_ty, span) else {
            return Some(Typed::error());
        };
        let ty = DuxType::array(elem_ty, count);
        let slot = self.alloca(ty, "arr");
        let array_ty = elem_ty.llvm(self.context).array_type(count as u32);
        let zero = self.context.i64_type().const_zero();
        for position in 0..count {
            let i = self.context.i64_type().const_int(position, false);
            let Ok(at) = (unsafe { self.builder.build_gep(array_ty, slot, &[zero, i], "elem") })
            else {
                return None;
            };
            match (elem_ty.array_info(), fill.core) {
                (Some(info), Core::Ptr(source)) => self.copy_array(at, source, info),
                (None, _) => {
                    let Some(basic) = fill.basic() else {
                        self.unsupported(span, "storing this value in an array");
                        return None;
                    };
                    let _ = self.builder.build_store(at, basic);
                }
                (Some(_), _) => {
                    self.unsupported(span, "storing this array value in an array");
                    return None;
                }
            }
        }
        Some(Typed::pointer(slot, ty))
    }

    fn index_expr(
        &mut self,
        module: &Module<'static>,
        base: &Expr,
        index: &Expr,
        _span: Span,
    ) -> Option<Typed> {
        let (elem, at) = self.place_of(module, base, index)?;
        self.load(elem, at)
    }

    /// Resolve `base[index]` to the element's address.
    fn place_of(
        &mut self,
        module: &Module<'static>,
        base: &Expr,
        index: &Expr,
    ) -> Option<(DuxType, PointerValue<'static>)> {
        let base_span = base.span();
        let base = self.expr(module, base, None)?;
        let Some(info) = base.ty.array_info() else {
            self.unsupported(
                base_span,
                &format!(
                    "indexing a `{}` (strings and slices land at M3)",
                    base.ty.name()
                ),
            );
            return None;
        };
        let Core::Ptr(array) = base.core else {
            return None;
        };
        let index_value = self.expr(module, index, None)?;
        if index_value.ty == DuxType::Error {
            return None;
        }
        if !index_value.ty.is_integral() {
            self.diagnostics.push(
                Diagnostic::error(
                    code::TYPE_MISMATCH,
                    format!(
                        "an array index must be an integer, found `{}`",
                        index_value.ty.name()
                    ),
                )
                .with_span(index.span()),
            );
            return None;
        }
        let index64 = if index_value.ty == DuxType::I64 {
            index_value
        } else {
            self.convert(index_value, DuxType::I64)?
        };
        let array_ty = info.elem.llvm(self.context).array_type(info.len as u32);
        let zero = self.context.i64_type().const_zero();
        let at = unsafe {
            self.builder
                .build_gep(array_ty, array, &[zero, index64.int_value()?], "idx")
        }
        .ok()?;
        Some((info.elem, at))
    }

    fn copy_array(
        &mut self,
        dest: PointerValue<'static>,
        source: PointerValue<'static>,
        info: &ArrayInfo,
    ) {
        let array_ty = info.elem.llvm(self.context).array_type(info.len as u32);
        let elem = info.elem.llvm(self.context);
        let zero = self.context.i64_type().const_zero();
        for position in 0..info.len {
            let i = self.context.i64_type().const_int(position, false);
            let Ok(from) = (unsafe { self.builder.build_gep(array_ty, source, &[zero, i], "elem") })
            else {
                return;
            };
            let Ok(into) = (unsafe { self.builder.build_gep(array_ty, dest, &[zero, i], "slot") })
            else {
                return;
            };
            if let Ok(value) = self.builder.build_load(elem, from, "") {
                let _ = self.builder.build_store(into, value);
            }
        }
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
                        code::NOT_BOOLEAN,
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
                            code::LITERAL_OUT_OF_RANGE,
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
            Expr::Ident { name, span } => {
                if let Some((ty, slot)) = self.slot_of(name) {
                    return self.load(ty, slot);
                }
                if let Some(c) = self.consts.get(name.as_str()).cloned() {
                    return Some(self.const_reference(&c, expected, *span));
                }
                self.diagnostics.push(
                    Diagnostic::error(
                        code::UNDECLARED,
                        format!("`{name}` is not declared in this scope"),
                    )
                    .with_span(*span)
                    .with_note(
                        "the resolver lands at M2; until then an unknown name is reported here",
                    ),
                );
                None
            }
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
            Expr::If {
                cond,
                then_arm,
                else_arm,
                span,
            } => match else_arm {
                Some(other) => self.if_value(module, cond, then_arm, other, expected, *span),
                None => {
                    self.unsupported(*span, "an `if` in value position without a final `else`");
                    None
                }
            },
            Expr::Block { block, value, .. } => {
                self.value_block_body(module, block, value);
                if self.dead {
                    return None;
                }
                self.expr(module, value, expected)
            }
            Expr::ArrayLiteral {
                elements, span, ..
            } => self.array_literal(module, elements, expected, *span),
            Expr::RepeatedArray { value, len, span } => {
                self.repeated_array(module, value, len, expected, *span)
            }
            Expr::Index {
                base, index, span, ..
            } => self.index_expr(module, base, index, *span),
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
                            code::NOT_BOOLEAN,
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
                            code::TYPE_MISMATCH,
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
                        code::TYPE_MISMATCH,
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
                code::LOSSY_CONVERSION,
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
                    code::UNDECLARED,
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
                    code::WRONG_ARITY,
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
        let values = self.lower_holes(module, parts, positional, span)?;
        self.write_prepared(module, parts, &values, newline, span)
    }

    /// Lower every hole of an interpolated string, in order: written holes
    /// from the string, bare `{}` holes from the trailing arguments.
    fn lower_holes(
        &mut self,
        module: &Module<'static>,
        parts: &[InterpolationPart],
        positional: &[Expr],
        span: Span,
    ) -> Option<Vec<Typed>> {
        let mut values: Vec<Typed> = Vec::new();
        let mut next_positional = 0usize;
        for part in parts {
            match part {
                InterpolationPart::Text(_) => {}
                InterpolationPart::Hole {
                    written: Some(expr), ..
                } => {
                    let value = self.expr(module, expr, None)?;
                    values.push(value);
                }
                InterpolationPart::Hole {
                    written: None, ..
                } => {
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
        Some(values)
    }

    /// Emit a printf call for an interpolated string whose hole values are
    /// already lowered — the same path `defer` replays.
    fn write_prepared(
        &mut self,
        module: &Module<'static>,
        parts: &[InterpolationPart],
        values: &[Typed],
        newline: bool,
        span: Span,
    ) -> Option<()> {
        let mut format = String::new();
        let mut next_value = 0usize;
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
                InterpolationPart::Hole { .. } => {
                    let Some(value) = values.get(next_value) else {
                        self.diagnostics.push(
                            Diagnostic::error(
                                code::CODEGEN_FAILED,
                                "an interpolated string lost one of its values",
                            )
                            .with_span(span),
                        );
                        return None;
                    };
                    next_value += 1;
                    format.push('%');
                    let spec = spec_for(value.ty).or_else(|| {
                        self.unsupported(span, "printing a value in a $\"..\" hole");
                        None
                    })?;
                    format.push_str(spec);
                }
            }
        }
        if next_value != values.len() {
            self.diagnostics.push(
                Diagnostic::error(
                    code::UNEXPECTED_TOKEN,
                    format!(
                        "{} arguments given for {} holes",
                        values.len(),
                        next_value
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
            let prepared = self.prepare_for_format(*value, span)?;
            arguments.push(prepared.into());
        }
        let printf = self.printf_fn(module);
        let _ = self
            .builder
            .build_call(printf, &arguments, "printf")
            .ok()?;
        Some(())
    }

    fn prepare_for_format(
        &mut self,
        value: Typed,
        span: Span,
    ) -> Option<BasicValueEnum<'static>> {
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
            DuxType::Array(..) => {
                self.unsupported(
                    span,
                    "printing an array (array formatting lands at M3)",
                );
                None
            }
            _ => value.basic(),
        }
    }
}

impl Emitter {
    fn load(&mut self, ty: DuxType, slot: PointerValue<'static>) -> Option<Typed> {
        if ty.array_info().is_some() {
            return Some(Typed::pointer(slot, ty));
        }
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

impl Emitter {
    fn declare_const(&mut self, decl: &ConstDecl) {
        let name = decl.name.name.clone();
        let Some(ty) = decl
            .ty
            .as_ref()
            .and_then(DuxType::from_ast)
            .filter(|ty| ty.array_info().is_none())
        else {
            self.unsupported(
                decl.span,
                "a `const` whose type is not a primitive yet",
            );
            return;
        };
        let Some(value) = self.const_expr(&decl.value) else {
            return;
        };
        let Some(value) = value.checked(ty, decl.span, self) else {
            return;
        };
        self.consts.insert(name, Const { ty, value });
    }

    fn declare_global(&mut self, module: &Module<'static>, decl: &GlobalDecl) {
        let name = decl.name.name.clone();
        let Some(ty) = decl
            .ty
            .as_ref()
            .and_then(DuxType::from_ast)
            .filter(|ty| ty.array_info().is_none())
        else {
            self.unsupported(
                decl.span,
                "a `global` whose type is not a primitive yet",
            );
            return;
        };
        let initializer = match &decl.value {
            None => ty.llvm(self.context).const_zero(),
            Some(value) => {
                let Some(value) = self.const_expr(value) else {
                    return;
                };
                if matches!(value, ConstValue::Str(_)) {
                    self.unsupported(
                        decl.span,
                        "a `global` string (it lands with slices at M3)",
                    );
                    return;
                }
                let Some(value) = value.checked(ty, decl.span, self) else {
                    return;
                };
                self.const_initializer(&value, ty)
            }
        };
        let global = module.add_global(ty.llvm(self.context), None, &name);
        global.set_initializer(&initializer);
        self.globals.insert(name, (ty, global));
    }

    fn const_initializer(&mut self, value: &ConstValue, ty: DuxType) -> BasicValueEnum<'static> {
        match value {
            ConstValue::Int(v) => match ty {
                DuxType::Bool => self.context.bool_type().const_int(u64::from(*v != 0), false).into(),
                other => other.int_type(self.context).const_int(*v as u64, false).into(),
            },
            ConstValue::Float(v) => ty
                .float_type(self.context)
                .const_float(*v)
                .into(),
            ConstValue::Bool(b) => self.context.bool_type().const_int(u64::from(*b), false).into(),
            ConstValue::Str(_) => ty.llvm(self.context).const_zero(),
        }
    }

    fn const_expr(&mut self, expr: &Expr) -> Option<ConstValue> {
        match expr {
            Expr::Int { value, span } => match i128::try_from(*value) {
                Ok(value) => Some(ConstValue::Int(value)),
                Err(_) => {
                    self.diagnostics.push(
                        Diagnostic::error(
                            code::NUMBER_OUT_OF_RANGE,
                            "integer constant does not fit i128",
                        )
                        .with_span(*span),
                    );
                    None
                }
            },
            Expr::Float { value, .. } => Some(ConstValue::Float(*value)),
            Expr::Bool { value, .. } => Some(ConstValue::Bool(*value)),
            Expr::Str { text, .. } => Some(ConstValue::Str(text.clone())),
            Expr::Paren { inner, .. } => self.const_expr(inner),
            Expr::Unary {
                op: UnaryOp::Neg,
                operand,
                ..
            } => match self.const_expr(operand)? {
                ConstValue::Int(v) => match v.checked_neg() {
                    Some(v) => Some(ConstValue::Int(v)),
                    None => {
                        self.diagnostics.push(
                            Diagnostic::error(
                                code::NUMBER_OUT_OF_RANGE,
                                "negating this constant overflows",
                            )
                            .with_span(operand.span()),
                        );
                        None
                    }
                },
                ConstValue::Float(v) => Some(ConstValue::Float(-v)),
                _ => {
                    self.unsupported(operand.span(), "negating this constant");
                    None
                }
            },
            Expr::Binary {
                op, lhs, rhs, span, ..
            } => self.const_binary(*op, lhs, rhs, *span),
            other => {
                self.unsupported(
                    other.span(),
                    "this `const` initialiser (literals and constant arithmetic today)",
                );
                None
            }
        }
    }

    fn const_binary(
        &mut self,
        op: BinaryOp,
        lhs: &Expr,
        rhs: &Expr,
        span: Span,
    ) -> Option<ConstValue> {
        let left = self.const_expr(lhs)?;
        let right = self.const_expr(rhs)?;
        enum CNum {
            I(i128),
            F(f64),
        }
        fn cnum(value: &ConstValue) -> Option<CNum> {
            match value {
                ConstValue::Int(v) => Some(CNum::I(*v)),
                ConstValue::Float(v) => Some(CNum::F(*v)),
                _ => None,
            }
        }
        let mut mismatch = || {
            self.diagnostics.push(
                Diagnostic::error(
                    code::UNEXPECTED_TOKEN,
                    "this operator is not constant-foldable here",
                )
                .with_span(span),
            );
            None
        };
        if let (ConstValue::Bool(l), ConstValue::Bool(r)) = (&left, &right) {
            return match op {
                BinaryOp::And => Some(ConstValue::Bool(*l && *r)),
                BinaryOp::Or => Some(ConstValue::Bool(*l || *r)),
                BinaryOp::Eq => Some(ConstValue::Bool(l == r)),
                BinaryOp::NotEq => Some(ConstValue::Bool(l != r)),
                _ => mismatch(),
            };
        }
        let (left, right) = match (cnum(&left), cnum(&right)) {
            (Some(l), Some(r)) => (l, r),
            _ => return mismatch(),
        };
        if let (CNum::I(l), CNum::I(r)) = (&left, &right) {
            let (l, r) = (*l, *r);
            return match op {
                BinaryOp::Add => Some(ConstValue::Int(l.checked_add(r)?)),
                BinaryOp::Sub => Some(ConstValue::Int(l.checked_sub(r)?)),
                BinaryOp::Mul => Some(ConstValue::Int(l.checked_mul(r)?)),
                BinaryOp::Div if r != 0 => Some(ConstValue::Int(l.wrapping_div(r))),
                BinaryOp::Mod if r != 0 => Some(ConstValue::Int(l.wrapping_rem(r))),
                BinaryOp::Eq => Some(ConstValue::Bool(l == r)),
                BinaryOp::NotEq => Some(ConstValue::Bool(l != r)),
                BinaryOp::Lt => Some(ConstValue::Bool(l < r)),
                BinaryOp::Gt => Some(ConstValue::Bool(l > r)),
                BinaryOp::Le => Some(ConstValue::Bool(l <= r)),
                BinaryOp::Ge => Some(ConstValue::Bool(l >= r)),
                _ => mismatch(),
            };
        }
        let as_f64 = |value: &CNum| match value {
            CNum::I(v) => *v as f64,
            CNum::F(v) => *v,
        };
        let (l, r) = (as_f64(&left), as_f64(&right));
        match op {
            BinaryOp::Add => Some(ConstValue::Float(l + r)),
            BinaryOp::Sub => Some(ConstValue::Float(l - r)),
            BinaryOp::Mul => Some(ConstValue::Float(l * r)),
            BinaryOp::Div => Some(ConstValue::Float(l / r)),
            BinaryOp::Mod => Some(ConstValue::Float(l % r)),
            BinaryOp::Eq => Some(ConstValue::Bool(l == r)),
            BinaryOp::NotEq => Some(ConstValue::Bool(l != r)),
            BinaryOp::Lt => Some(ConstValue::Bool(l < r)),
            BinaryOp::Gt => Some(ConstValue::Bool(l > r)),
            BinaryOp::Le => Some(ConstValue::Bool(l <= r)),
            BinaryOp::Ge => Some(ConstValue::Bool(l >= r)),
            _ => mismatch(),
        }
    }

    fn const_reference(&mut self, c: &Const, expected: Option<DuxType>, span: Span) -> Typed {
        match &c.value {
            ConstValue::Int(v) => {
                let want = expected.filter(|ty| ty.is_integral() || *ty == DuxType::Bool);
                let ty = match want {
                    Some(ty) if fits_i128(*v, ty) => ty,
                    _ => c.ty,
                };
                match ty {
                    DuxType::Bool => Typed::int(
                        self.context.bool_type().const_int(u64::from(*v != 0), false),
                        DuxType::Bool,
                    ),
                    other => Typed::int(
                        other.int_type(self.context).const_int(*v as u64, false),
                        other,
                    ),
                }
            }
            ConstValue::Float(v) => {
                let ty = if c.ty == DuxType::F32 { DuxType::F32 } else { DuxType::F64 };
                Typed::float(ty.float_type(self.context).const_float(*v), ty)
            }
            ConstValue::Bool(b) => Typed::int(
                self.context.bool_type().const_int(u64::from(*b), false),
                DuxType::Bool,
            ),
            ConstValue::Str(text) => self.string_literal(text, span),
        }
    }

    fn slot_of(&self, name: &str) -> Option<(DuxType, PointerValue<'static>)> {
        if let Some(&(ty, slot)) = self.scopes.iter().rev().find_map(|scope| scope.get(name)) {
            return Some((ty, slot));
        }
        self.globals
            .get(name)
            .map(|(ty, global)| (*ty, global.as_pointer_value()))
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
        DuxType::Array(..) | DuxType::Error => return None,
    })
}

fn expression_name(expr: &Expr) -> &'static str {
    match expr {
        Expr::Field { .. } => "a field access (lands at M3)",
        Expr::Member { .. } => "a tuple member (lands at M3)",
        Expr::Range { .. } => "a range (lands at M3)",
        Expr::Path { .. } => "a qualified path",
        Expr::StructLiteral { .. } => "a struct value (lands at M3)",
        Expr::CondBinding { .. } => "a pattern condition (lands at M4)",
        Expr::Error { .. } => "an erroneous expression",
        _ => "this expression",
    }
}
