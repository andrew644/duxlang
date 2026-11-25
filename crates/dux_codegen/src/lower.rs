//! Lowering from the AST to LLVM IR.
//!
//! The shape follows DESIGN.md §11.2: the entry block holds every alloca,
//! other instructions go in the blocks the control flow needs, and `main` is
//! a C `int main(void)` so the C runtime can start it (D12). Numeric
//! conversions follow D25: implicit exactly when the destination holds every
//! value of the source, `as` for anything else.

use std::collections::{HashMap, HashSet};

use crate::runtime::Runtime;
use dux_ast::{
    BinaryOp, Block, ConstDecl, Decl, Expr, FnDecl, ForKind, GlobalDecl, InterpolationPart, Program,
    Stmt, Type, UnaryOp,
};
use dux_diag::{code, Diagnostic};
use dux_hir::sums::{self, Payload, Shape};
use dux_source::{FileId, SourceMap, Span};
use inkwell::basic_block::BasicBlock;
use inkwell::builder::Builder;
use inkwell::context::Context;
use inkwell::debug_info::{
    AsDIScope, DICompileUnit, DIFile, DISubprogram, DIFlags, DIFlagsConstants, DebugInfoBuilder,
    DWARFEmissionKind, DWARFSourceLanguage,
};
use inkwell::module::Module;
use inkwell::targets::TargetData;
use inkwell::types::{
    BasicMetadataTypeEnum, BasicType, BasicTypeEnum, FloatType, IntType, StructType,
};
use inkwell::values::{
    BasicMetadataValueEnum, BasicValueEnum, FloatValue, GlobalValue, IntValue, PointerValue,
    StructValue,
};
use inkwell::values::{BasicValue, FunctionValue, ValueKind};
use inkwell::{AddressSpace, FloatPredicate, IntPredicate};

const DW_ATE_BOOLEAN: u32 = 0x02;
const DW_ATE_FLOAT: u32 = 0x04;
const DW_ATE_SIGNED: u32 = 0x05;
const DW_ATE_UNSIGNED: u32 = 0x07;

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
    /// `*T` with a known target: `&x` makes one, `*p` reads and writes it,
    /// `p.field` auto-dereferences and `p + n` steps by whole targets.
    Pointer(&'static DuxType),
    /// A fixed array `[T; N]`. The LLVM representation is always a pointer to
    /// `[N x T]` storage; bindings and assignments that must copy do so
    /// element by element.
    Array(&'static ArrayInfo),
    /// `[]T`: a `{ data, len }` view, sixteen bytes, no capacity. It is a
    /// value, so it copies, is passed by value and lives in a slot like any
    /// other aggregate.
    Slice(&'static DuxType),
    /// A declared `struct`. The LLVM representation is the nominal struct
    /// type, so assignment is a copy of the whole record.
    Struct(&'static StructInfo),
    /// A declared `enum` at one instantiation: `Shape`, `Option<i32>`. The LLVM
    /// representation is the tagged struct of DESIGN 4.2 — a tag beside an area
    /// big enough for the largest payload — so a sum-type value is a value like
    /// a record: it copies, travels in registers and lives in a slot.
    Enum(&'static EnumInfo),
    /// Something we could not type; it silences follow-on diagnostics.
    Error,
}

/// A name a pattern binds: the name, what it holds, and the slot it holds it in.
pub(crate) type Binding = (String, DuxType, PointerValue<'static>);

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ArrayInfo {
    pub(crate) elem: DuxType,
    pub(crate) len: u64,
}

#[derive(Debug)]
pub(crate) struct StructInfo {
    pub(crate) name: &'static str,
    pub(crate) llvm: StructType<'static>,
}

/// Records are nominal: the same `struct` resolves to one leaked `StructInfo`,
/// so identity is the right equality and two look-alike records stay apart.
impl PartialEq for StructInfo {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self, other)
    }
}

impl Eq for StructInfo {}

/// One variant of a sum type at one instantiation: what it carries, and where
/// each piece of it sits inside the value.
#[derive(Debug)]
pub(crate) struct VariantInfo {
    pub(crate) name: &'static str,
    pub(crate) fields: Vec<(&'static str, DuxType)>,
    /// Each field's byte offset from the start of the value.
    pub(crate) offsets: Vec<u64>,
}

#[derive(Debug)]
pub(crate) struct EnumInfo {
    pub(crate) name: &'static str,
    /// The instantiation, spelled for diagnostics: `Option<i32>`.
    pub(crate) spelled: String,
    pub(crate) variants: Vec<VariantInfo>,
    pub(crate) llvm: BasicTypeEnum<'static>,
    /// The tag's own type, and its offset — a tagged enum keeps it at zero.
    pub(crate) tag: IntType<'static>,
    pub(crate) tag_at: u64,
    /// The variant whose pointer doubles as the tag; `None` when the enum
    /// keeps a tag of its own.
    pub(crate) niche: Option<u32>,
}

impl PartialEq for EnumInfo {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self, other)
    }
}

impl Eq for EnumInfo {}

impl EnumInfo {
    fn variant(&self, name: &str) -> Option<(u32, &VariantInfo)> {
        self.variants
            .iter()
            .position(|variant| variant.name == name)
            .map(|index| (index as u32, &self.variants[index]))
    }
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

    /// The elements a slice runs over.
    fn slice_element(self) -> Option<DuxType> {
        match self {
            DuxType::Slice(element) => Some(*element),
            _ => None,
        }
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

    /// Anything that holds an address: `*T` with a target, or a `rawptr`
    /// without one.
    fn is_pointer_like(self) -> bool {
        matches!(self, DuxType::Ptr | DuxType::Pointer(_))
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
            DuxType::Ptr | DuxType::Error => context.ptr_type(AddressSpace::default()).into(),
            DuxType::Pointer(_) => context.ptr_type(AddressSpace::default()).into(),
            DuxType::Array(info) => info.elem.llvm(context).array_type(info.len as u32).into(),
            DuxType::Slice(_) | DuxType::Str => context.struct_type(&[context.ptr_type(AddressSpace::default()).into(), context.i64_type().into()], false).into(),
            DuxType::Struct(info) => info.llvm.into(),
            DuxType::Enum(info) => info.llvm,
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
            DuxType::Pointer(target) => format!("*{}", target.name()),
            DuxType::Array(info) => format!("[{}; {}]", info.elem.name(), info.len),
            DuxType::Slice(element) => format!("[]{}", element.name()),
            DuxType::Struct(info) => info.name.to_string(),
            DuxType::Enum(info) => info.spelled.clone(),
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
            DuxType::Array(..) | DuxType::Struct(..) | DuxType::Enum(_) | DuxType::Str
            | DuxType::Ptr => return false,
            DuxType::Slice(_) => return false,
            DuxType::Pointer(_) => return false,
            DuxType::Error => return false,
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
    Aggregate(StructValue<'static>),
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
            Core::Aggregate(value) => Some(value.into()),
            Core::Void => None,
        }
    }
}

/// Where the arms of a `match` meet: the block they branch to, the type they
/// agreed on, and the values that arrive there.
struct Join<'a> {
    block: BasicBlock<'a>,
    agreed: Option<DuxType>,
    carried: Vec<(Typed, BasicBlock<'static>)>,
    broken: bool,
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
    Struct(Vec<(&'static str, ConstValue)>),
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
    /// The receiver of a method, which the call site passes first and the
    /// argument list never mentions.
    receiver: Option<DuxType>,
    /// The LLVM parameter list ends with the hidden `context` pointer, which
    /// the argument list never mentions.
    hidden_context: bool,
}

/// A `fn` or a method, named the way the lowering names it. A method's key
/// carries its record, because `Vec3.len` and `Mat4.len` are different
/// functions that share a spelling; the symbol carries a prefix so neither can
/// collide with a package-level `fn`.
struct Unit<'a> {
    key: String,
    symbol: String,
    owner: Option<&'a str>,
    external: bool,
    decl: &'a FnDecl,
}

impl<'a> Unit<'a> {
    fn function(decl: &'a FnDecl) -> Self {
        Self {
            key: decl.name.name.clone(),
            symbol: decl.name.name.clone(),
            owner: None,
            external: false,
            decl,
        }
    }

    fn imported(decl: &'a FnDecl) -> Self {
        Self {
            external: true,
            ..Self::function(decl)
        }
    }

    fn method(decl: &'a FnDecl, owner: &'a str) -> Self {
        Self {
            key: format!("{owner}.{}", decl.name.name),
            symbol: format!("dux.{owner}.{}", decl.name.name),
            owner: Some(owner),
            external: false,
            decl,
        }
    }

    fn takes_a_receiver(&self) -> bool {
        self.owner.is_some() && self.decl.params.first().is_some_and(|p| p.name.name == "self")
    }
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
        hidden_context: bool,
    },
    /// `defer free(p)` and `defer del(s)`: the memory is handed back at the
    /// end of the function, through the context of the `defer`.
    Release {
        slot: (DuxType, PointerValue<'static>),
    },
}

pub(crate) struct Emitter {
    context: &'static Context,
    builder: Builder<'static>,
    diagnostics: Vec<Diagnostic>,
    functions: HashMap<String, FunctionValue<'static>>,
    signatures: HashMap<String, Signature>,
    /// The declared `struct` shapes, by name, waiting to be resolved into a
    /// `StructInfo` (a nominal LLVM struct type plus resolved field types).
    struct_decls: Vec<dux_ast::StructDecl>,
    structs: HashMap<String, &'static StructInfo>,
    /// A declared record's fields, by record name. They live here rather than
    /// on `StructInfo` so a record can exist — and be pointed at — before its
    /// fields are known, which is what `struct Node { next: *Node }` needs.
    struct_fields: HashMap<String, Vec<(&'static str, DuxType)>>,
    /// The declared `enum` shapes — the program's own over the universe's
    /// `Option` and `Result` — waiting to be resolved at an instantiation.
    shapes: HashMap<String, Shape>,
    enums: HashMap<String, &'static EnumInfo>,
    /// Names whose payload types are being resolved right now, so an enum that
    /// contains itself by value is a diagnostic rather than a hang.
    resolving_enums: Vec<String>,
    enum_errors: HashSet<String>,
    /// The module's data layout, so a payload area is placed exactly as C
    /// would place it.
    layout: String,
    /// Names whose field types are being resolved right now, so a size cycle
    /// is a diagnostic rather than a hang.
    resolving: Vec<String>,
    /// How deep inside a `*T` the resolver stands. A pointer needs no size for
    /// what it points at, so a type still being laid out is a raw pointer from
    /// here on — not the size cycle the by-value fields must not make.
    pointer_depth: usize,
    /// Structs already reported as unlowerable, so one bad field is one
    /// diagnostic rather than one per use.
    struct_errors: HashSet<String>,
    /// Names already reported as not lowerable by this build; calls to them
    /// stay quiet so the user sees one diagnostic, not a cascade.
    skipped: HashSet<String>,
    scopes: Vec<HashMap<String, (DuxType, PointerValue<'static>)>>,
    consts: HashMap<String, Const>,
    /// Aggregate `const` values live in constant global storage; the name is
    /// remembered so an assignment to one is still reported.
    const_names: HashSet<String>,
    globals: HashMap<String, (DuxType, GlobalValue<'static>)>,
    loops: Vec<Loop>,
    defers: Vec<Deferred>,
    /// The allocator runtime, emitted when some function in the program
    /// allocates.
    runtime: Option<Runtime>,
    /// Head allocations with bookkeeping and report leaks at exit.
    pub(crate) debug_allocator: bool,
    /// Functions that carry the hidden `context` parameter: the ones that
    /// allocate, directly or through a call.
    allocating: HashSet<String>,
    /// The running function's `context` slot, where it keeps the pointer it was
    /// handed (or, for `main`, the default context it created).
    context_slot: Option<PointerValue<'static>>,
    /// Saved outer contexts of the blocks being lowered, innermost last.
    context_saves: Vec<PointerValue<'static>>,
    /// The function being lowered, for reading its hidden `context` parameter.
    running_function: Option<FunctionValue<'static>>,
    /// C library declarations the generated code leans on.
    c_functions: HashMap<String, FunctionValue<'static>>,
    allocas: Option<BasicBlock<'static>>,
    result_ty: Option<DuxType>,
    is_main: bool,
    dead: bool,
    printf: Option<FunctionValue<'static>>,
    stdout: Option<inkwell::values::GlobalValue<'static>>,
    sources: SourceMap,
    dibuilder: Option<DebugInfoBuilder<'static>>,
    compile_unit: Option<DICompileUnit<'static>>,
    scope: Option<DISubprogram<'static>>,
    di_files: HashMap<FileId, DIFile<'static>>,
}

impl Emitter {
    pub(crate) fn new(context: &'static Context, sources: SourceMap) -> Self {
        Self {
            context,
            builder: context.create_builder(),
            diagnostics: Vec::new(),
            functions: HashMap::new(),
            signatures: HashMap::new(),
            struct_decls: Vec::new(),
            structs: HashMap::new(),
            struct_fields: HashMap::new(),
            shapes: HashMap::new(),
            enums: HashMap::new(),
            resolving_enums: Vec::new(),
            enum_errors: HashSet::new(),
            layout: String::new(),
            resolving: Vec::new(),
            pointer_depth: 0,
            struct_errors: HashSet::new(),
            skipped: HashSet::new(),
            scopes: Vec::new(),
            consts: HashMap::new(),
            const_names: HashSet::new(),
            globals: HashMap::new(),
            loops: Vec::new(),
            defers: Vec::new(),
            runtime: None,
            debug_allocator: true,
            allocating: HashSet::new(),
            context_slot: None,
            context_saves: Vec::new(),
            running_function: None,
            c_functions: HashMap::new(),
            allocas: None,
            result_ty: None,
            is_main: false,
            dead: false,
            printf: None,
            stdout: None,
            sources,
            dibuilder: None,
            compile_unit: None,
            scope: None,
            di_files: HashMap::new(),
        }
    }

    pub(crate) fn lower(
        mut self,
        module: &Module<'static>,
        program: &Program,
    ) -> Vec<Diagnostic> {
        self.allocating = self.allocation_analysis(program);
        if !self.allocating.is_empty() {
            self.runtime = Some(Runtime::emit(
                self.context,
                module,
                self.debug_allocator,
            ));
        }
        if !self.sources.is_empty() {
            let file = self
                .sources
                .first_file()
                .map(|file| self.sources.name_of(file).to_string())
                .unwrap_or_else(|| "main.dux".to_string());
            let (dibuilder, compile_unit) = module.create_debug_info_builder(
                true,
                DWARFSourceLanguage::Rust,
                &file,
                "",
                "duxc",
                false,
                "",
                0,
                "",
                DWARFEmissionKind::Full,
                0,
                false,
                false,
                "",
                "",
            );
            self.dibuilder = Some(dibuilder);
            self.compile_unit = Some(compile_unit);
            let version = self
                .context
                .i32_type()
                .const_int(inkwell::debug_info::debug_metadata_version() as u64, false);
            module.add_basic_value_flag(
                "Debug Info Version",
                inkwell::module::FlagBehavior::Warning,
                version,
            );
        }
        self.struct_decls = program.structs().cloned().collect();
        self.shapes = sums::prelude()
            .into_iter()
            .map(|shape| (shape.name.clone(), shape))
            .collect();
        for decl in program.enums() {
            let shape = Shape::from_decl(decl);
            self.shapes.insert(shape.name.clone(), shape);
        }
        self.layout = module
            .get_data_layout()
            .as_str()
            .to_str()
            .unwrap_or_default()
            .to_string();
        for decl in &program.decls {
            match decl {
                Decl::Const(decl) => self.declare_const(module, decl),
                Decl::Global(decl) => self.declare_global(module, decl),
                Decl::Extern(block) => {
                    for function in &block.functions {
                        self.declare(module, &Unit::imported(function));
                    }
                }
                _ => {}
            }
        }
        let functions: Vec<&FnDecl> = program.functions().collect();
        let mut units: Vec<Unit> = functions.iter().map(|decl| Unit::function(decl)).collect();
        for decl in &program.decls {
            if let Decl::Impl(block) = decl {
                let owner = block.type_name.name.as_str();
                for method in &block.methods {
                    let unit = Unit::method(method, owner);
                    if self.functions.contains_key(&unit.key) {
                        self.diagnostics.push(
                            Diagnostic::error(
                                code::DUPLICATE_DEFINITION,
                                format!("`{owner}` has more than one `{}`", method.name.name),
                            )
                            .with_span(method.name.span),
                        );
                        continue;
                    }
                    units.push(unit);
                }
            }
        }
        for unit in &units {
            self.declare(module, unit);
        }
        for unit in &units {
            self.lower_body(module, unit);
        }
        if let Some(dibuilder) = &self.dibuilder {
            dibuilder.finalize();
        }
        self.dibuilder = None;
        self.compile_unit = None;
        self.scope = None;
        self.diagnostics
            .sort_by_key(|diagnostic| diagnostic.span.map(|span| span.start));
        self.diagnostics
    }
}

impl Emitter {
    /// A source type as the lowering sees it. Primitives, pointers and fixed
    /// arrays come straight from the shared lattice; a `struct` name resolves
    /// to its nominal record.
    fn type_of(&mut self, ty: &Type) -> Option<DuxType> {
        match ty {
            Type::Named { name, args, .. } if args.is_empty() => match DuxType::named(name) {
                Some(primitive) => Some(primitive),
                None => match self.struct_type_of(name) {
                    Some(record) => Some(record),
                    None => self.enum_type_of(name, &[]),
                },
            },
            Type::Named { name, args, .. } => {
                let mut given = Vec::new();
                for argument in args {
                    given.push(self.type_of(argument)?);
                }
                self.enum_type_of(name, &given)
            }
            Type::Slice { element, .. } => self
                .type_of(element)
                .map(|element| DuxType::Slice(Box::leak(Box::new(element)))),
            Type::Pointer { pointee, .. } => {
                self.pointer_depth += 1;
                let pointee = self.type_of(pointee);
                self.pointer_depth -= 1;
                Some(match pointee {
                    Some(target) => self.pointer_to(target),
                    None => DuxType::Ptr,
                })
            }
            Type::Array { element, len, .. } => {
                let element = self.type_of(element)?;
                let Expr::Int { value, .. } = len.as_ref() else {
                    return None;
                };
                Some(DuxType::array(element, u64::try_from(*value).ok()?))
            }
            other => DuxType::from_ast(other),
        }
    }

    /// Resolve a declared `enum` at one instantiation. `Option` and `Result`
    /// come from the universe scope and are built the same way as a program's
    /// own; each instantiation gets its own nominal LLVM struct type, so
    /// `Option<i32>` and `Option<string>` stay different types.
    fn enum_type_of(&mut self, name: &str, args: &[DuxType]) -> Option<DuxType> {
        let key = instantiation(name, args);
        if let Some(info) = self.enums.get(&key).copied() {
            return Some(DuxType::Enum(info));
        }
        if self.enum_errors.contains(&key) {
            return None;
        }
        let shape = self.shapes.get(name)?.clone();
        if !shape.takes(args.len()) {
            self.diagnostics.push(
                Diagnostic::error(
                    code::CODEGEN_FAILED,
                    format!(
                        "`{name}` takes {} argument(s), this one gives {}",
                        shape.params.len(),
                        args.len()
                    ),
                )
                .with_note("an enum without its arguments has no size"),
            );
            self.enum_errors.insert(key);
            return None;
        }
        if self.resolving_enums.iter().any(|busy| busy == &key) {
            if self.pointer_depth > 0 {
                return Some(DuxType::Ptr);
            }
            self.diagnostics.push(
                Diagnostic::error(
                    code::CODEGEN_FAILED,
                    format!(
                        "`{key}` has no size: a variant must reach itself through a pointer"
                    ),
                ),
            );
            self.enum_errors.insert(key);
            return None;
        }
        let context = self.context;
        self.resolving_enums.push(key.clone());
        let mut variants: Vec<VariantInfo> = Vec::new();
        let mut payloads = Vec::new();
        let mut broken = false;
        for (variant, payload) in &shape.clone().variants {
            let mut fields: Vec<(&'static str, DuxType)> = Vec::new();
            let mut field_types = Vec::new();
            for (field, spelled) in payload_fields(payload) {
                match self.payload_type(spelled, &shape, args) {
                    Some(ty) => {
                        fields.push((Box::leak(field.to_string().into_boxed_str()) as &str, ty));
                        field_types.push(ty.llvm(context));
                    }
                    None => {
                        self.unsupported(
                            spelled.span(),
                            &format!("the payload of `{name}::{variant}`"),
                        );
                        broken = true;
                        break;
                    }
                }
            }
            if broken {
                break;
            }
            let llvm = context.struct_type(&field_types, false);
            payloads.push(llvm);
            variants.push(VariantInfo {
                name: Box::leak(variant.to_string().into_boxed_str()) as &str,
                fields,
                offsets: Vec::new(),
            });
        }
        self.resolving_enums.pop();
        if broken {
            self.enum_errors.insert(key);
            return None;
        }
        let data = TargetData::create(&self.layout);
        // DESIGN 4.3: the niche belongs to the predeclared two — `Option` and
        // `Result` — and not to every enum that happens to hold a pointer.
        let niche = match shape.name.as_str() {
            "Option" | "Result" => niche_of(context, &data, &variants, &payloads),
            _ => None,
        };
        let (llvm, tag, tag_at) = match niche {
            Some(_) => (
                context.ptr_type(AddressSpace::default()).into(),
                context.i16_type(),
                0u64,
            ),
            None => {
                // DESIGN 4.4: the tag is a `u16` unless the enum outgrows one.
                let tag = if variants.len() <= usize::from(u16::MAX) {
                    context.i16_type()
                } else {
                    context.i32_type()
                };
                let storage = storage_area(context, &data, &payloads);
                (context.struct_type(&[tag.into(), storage], false).into(), tag, 0u64)
            }
        };
        let payload_at = if niche.is_none() {
            match llvm {
                BasicTypeEnum::StructType(shape) => data.offset_of_element(&shape, 1).unwrap_or(0),
                _ => 0,
            }
        } else {
            0
        };
        for (index, variant) in variants.iter_mut().enumerate() {
            variant.offsets = match niche {
                // DESIGN 4.5: `Option<*T>` is one pointer, and null is the
                // variant that carries nothing, so the payload sits at zero.
                Some(carrier) if index == carrier as usize => vec![0u64; variant.fields.len()],
                Some(_) => Vec::new(),
                None => variant
                    .fields
                    .iter()
                    .enumerate()
                    .map(|(field, _)| {
                        let inside = data
                            .offset_of_element(&payloads[index], u32::try_from(field).unwrap_or(0))
                            .unwrap_or(0);
                        payload_at + inside
                    })
                    .collect(),
            };
        }
        let info: &'static EnumInfo = Box::leak(Box::new(EnumInfo {
            name: Box::leak(name.to_string().into_boxed_str()) as &str,
            spelled: key.clone(),
            variants,
            llvm,
            tag,
            tag_at,
            niche,
        }));
        self.enums.insert(key.clone(), info);
        Some(DuxType::Enum(info))
    }

    /// Which enum a variant written without its enum belongs to. The universe's
    /// two answer first; a program's own enum may own the name, and where more
    /// than one does the spelling is ambiguous, so nothing is claimed.
    fn owner_of_variant(&self, variant: &str) -> Option<String> {
        for name in ["Option", "Result"] {
            if self
                .shapes
                .get(name)
                .is_some_and(|shape| shape.has_variant(variant))
            {
                return Some(name.to_string());
            }
        }
        let owners: Vec<&String> = self
            .shapes
            .iter()
            .filter(|(_, shape)| shape.has_variant(variant))
            .map(|(name, _)| name)
            .collect();
        match owners.as_slice() {
            [one] => Some((*one).clone()),
            _ => None,
        }
    }

    /// `Shape::Circle(2.0)`, `Shape::Rect { w: 1.0, h: 2.0 }`, `Some(3)`,
    /// `None`: a sum-type value, built in a slot and then loaded so it travels
    /// like any other value.
    #[allow(clippy::too_many_arguments)]
    fn variant_value(
        &mut self,
        module: &Module<'static>,
        owner: &str,
        variant: &str,
        given: &[Expr],
        named: &[(&str, &Expr)],
        span: Span,
        expected: Option<DuxType>,
    ) -> Option<Typed> {
        let shape = self.shapes.get(owner).cloned()?;
        let (index, payload) = shape.variant(variant)?;
        let written = payload_fields(payload);
        // The context usually says which instantiation this is, and then each
        // payload value is checked against the field it lands in. A generic
        // enum written with no context is the one case where the values have to
        // speak for the parameters first.
        let known = match expected {
            Some(DuxType::Enum(info)) if info.name == owner => Some(info),
            _ if shape.params.is_empty() => match self.enum_type_of(owner, &[]) {
                Some(ty) => self.enum_of(ty),
                None => None,
            },
            _ => None,
        };
        let mut values: Vec<(&str, Typed)> = Vec::new();
        for (position, argument) in given.iter().enumerate() {
            let name = written
                .get(position)
                .map(|(name, _)| *name)
                .unwrap_or_default();
            let want = payload_field_of(known, index, name);
            values.push((name, self.expr(module, argument, want)?));
        }
        for (field, argument) in named {
            let want = payload_field_of(known, index, field);
            values.push((field, self.expr(module, argument, want)?));
        }
        let info = match known {
            Some(info) => info,
            None => {
                let Some(ty) = self.made_of(&shape, &written, &values) else {
                    self.diagnostics.push(
                        Diagnostic::error(
                            code::CODEGEN_FAILED,
                            format!("what `{owner}::{variant}` holds is not written anywhere"),
                        )
                        .with_span(span)
                        .with_note("say what it holds: `Option<i32> = None`, or give it a payload"),
                    );
                    return None;
                };
                self.enum_of(ty)?
            }
        };
        let ty = DuxType::Enum(info);
        let index = index as u32;
        let slot = self.alloca(ty, "sum");
        if info.niche.is_none() {
            let zero = info
                .llvm
                .into_struct_type()
                .const_zero()
                .as_basic_value_enum();
            let _ = self.builder.build_store(slot, zero).ok()?;
        }
        let fields = &info.variants[index as usize];
        if fields.fields.len() != values.len() {
            self.diagnostics.push(
                Diagnostic::error(
                    code::WRONG_ARITY,
                    format!(
                        "`{owner}::{variant}` carries {} value(s), this one gives {}",
                        fields.fields.len(),
                        values.len()
                    ),
                )
                .with_span(span),
            );
            return None;
        }
        if let Some(carrier) = info.niche {
            if index != carrier {
                let null = self.context.ptr_type(AddressSpace::default()).const_zero();
                let _ = self.builder.build_store(self.byte_at(slot, 0), null).ok()?;
            }
        } else {
            let tag = info.tag.const_int(index as u64, false);
            let at = self.byte_at(slot, info.tag_at);
            let _ = self.builder.build_store(at, tag).ok()?;
        }
        for (field, value) in fields.fields.iter().zip(values.iter()) {
            let offset = fields.offsets.get(values.iter().position(|(name, _)| *name == field.0)?)
                .copied()
                .unwrap_or(0);
            let want = self.coerce_implicit(module, value.1, field.1, span)?;
            let at = self.byte_at(slot, offset);
            match (field.1.array_info(), want.core) {
                (Some(array), Core::Ptr(source)) => self.copy_array(at, source, array),
                (None, _) => {
                    let Some(basic) = want.basic() else {
                        self.unsupported(span, "storing this value in a sum-type payload");
                        return None;
                    };
                    let _ = self.builder.build_store(at, basic).ok()?;
                }
                (Some(_), _) => {
                    self.unsupported(span, "storing this array value in a sum-type payload");
                    return None;
                }
            }
        }
        self.load(ty, slot)
    }

    /// The instantiation a sum-type value has when nothing says: whichever
    /// parameters the payload values fill in.
    fn made_of(
        &mut self,
        shape: &Shape,
        written: &[(&str, &Type)],
        values: &[(&str, Typed)],
    ) -> Option<DuxType> {
        let mut known: Vec<(String, DuxType)> = Vec::new();
        for ((_, spelled), (_, value)) in written.iter().zip(values) {
            if value.ty == DuxType::Error {
                return None;
            }
            if let Type::Named { name, args, .. } = spelled {
                if args.is_empty()
                    && shape.params.iter().any(|param| param == name)
                    && !known.iter().any(|(param, _)| param == name)
                {
                    known.push((name.clone(), value.ty));
                }
            }
        }
        let mut arguments = Vec::new();
        for param in &shape.params {
            match known.iter().find(|(known, _)| known == param) {
                Some((_, ty)) => arguments.push(*ty),
                None => return None,
            }
        }
        self.enum_type_of(&shape.name, &arguments)
    }

    /// A byte offset inside a value, which is how a payload field is reached:
    /// the tagged layout places its fields where C would, and only byte
    /// addresses are sure of that.
    fn byte_at(&self, at: PointerValue<'static>, offset: u64) -> PointerValue<'static> {
        if offset == 0 {
            return at;
        }
        let index = self.context.i64_type().const_int(offset, false);
        unsafe {
            self.builder
                .build_gep(self.context.i8_type(), at, &[index], "piece")
                .expect("byte offset inside a value")
        }
    }

    /// Does the sum-type value at `at` hold this variant? For a niche `Option`
    /// the question is whether the pointer is null; otherwise it is the tag.
    fn variant_holds(
        &mut self,
        info: &'static EnumInfo,
        index: u32,
        at: PointerValue<'static>,
    ) -> Option<IntValue<'static>> {
        let context = self.context;
        match info.niche {
            Some(carrier) => {
                let pointer = context.ptr_type(AddressSpace::default());
                let held = self
                    .builder
                    .build_load(pointer, at, "held")
                    .ok()?
                    .into_pointer_value();
                let empty = self.builder.build_is_null(held, "empty").ok()?;
                Some(if carrier == index {
                    self.builder.build_not(empty, "holds").ok()?
                } else {
                    empty
                })
            }
            None => {
                let tag = self
                    .builder
                    .build_load(info.tag, self.byte_at(at, info.tag_at), "tag")
                    .ok()?
                    .into_int_value();
                let want = info.tag.const_int(index as u64, false);
                self.builder
                    .build_int_compare(IntPredicate::EQ, tag, want, "holds")
                    .ok()
            }
        }
    }

    /// A variant's payload type with the enum's parameters filled in.
    fn payload_type(&mut self, spelled: &Type, shape: &Shape, args: &[DuxType]) -> Option<DuxType> {
        if let Type::Named { name, args: none, .. } = spelled {
            if none.is_empty() {
                if let Some(index) = shape.params.iter().position(|param| param == name) {
                    return args.get(index).cloned();
                }
            }
        }
        self.type_of(spelled)
    }

    fn enum_of(&self, ty: DuxType) -> Option<&'static EnumInfo> {
        match ty {
            DuxType::Enum(info) => Some(info),
            _ => None,
        }
    }

    /// Resolve (once) the declared `struct` of that name into a nominal LLVM
    /// struct type. Fields are resolved on demand so a record may be declared
    /// after the one that embeds it; a cycle that does not pass through a
    /// pointer has no size and is reported.
    fn struct_type_of(&mut self, name: &str) -> Option<DuxType> {
        if let Some(info) = self.structs.get(name).copied() {
            // The handle is cached before its fields are, so a record that is
            // still being laid out is reachable from here: only a pointer may
            // aim at it.
            if self.resolving.iter().any(|busy| busy == name) {
                return self.in_progress(name);
            }
            return Some(DuxType::Struct(info));
        }
        if self.struct_errors.contains(name) {
            return None;
        }
        let decl = self
            .struct_decls
            .iter()
            .find(|decl| decl.name.name == name)
            .cloned()?;
        if self.resolving.iter().any(|busy| busy == name) {
            return self.in_progress(name);
        }
        // The record's identity and its LLVM handle exist before its fields are
        // known, so a `*Self` field points at a type that is still being laid
        // out: a pointer needs no size for what it aims at.
        let llvm = self.context.opaque_struct_type(name);
        let info: &'static StructInfo = Box::leak(Box::new(StructInfo {
            name: Box::leak(name.to_string().into_boxed_str()) as &str,
            llvm,
        }));
        self.structs.insert(name.to_string(), info);
        self.resolving.push(name.to_string());
        let mut fields = Vec::new();
        let mut field_types = Vec::new();
        let mut unresolved = false;
        for field in &decl.fields {
            let Some(ty) = self.type_of(&field.ty) else {
                self.unsupported(
                    field.ty.span(),
                    &format!("the type of the `{name}::{}` field", field.name.name),
                );
                unresolved = true;
                break;
            };
            fields.push((
                Box::leak(field.name.name.clone().into_boxed_str()) as &str,
                ty,
            ));
            field_types.push(ty.llvm(self.context));
        }
        self.resolving.pop();
        if unresolved {
            self.structs.remove(name);
            self.struct_errors.insert(name.to_string());
            return None;
        }
        llvm.set_body(&field_types, false);
        self.struct_fields.insert(name.to_string(), fields);
        Some(DuxType::Struct(info))
    }

    /// A record whose fields are still being resolved. A pointer may aim at a
    /// type with no size yet — a pointer needs none — but a field of it would
    /// be a type that contains itself, which has no size at all.
    fn in_progress(&mut self, name: &str) -> Option<DuxType> {
        if self.pointer_depth > 0 {
            return self.structs.get(name).copied().map(DuxType::Struct);
        }
        let span = self
            .struct_decls
            .iter()
            .find(|decl| decl.name.name == name)
            .map(|decl| decl.span)
            .unwrap_or_default();
        self.diagnostics.push(
            Diagnostic::error(
                code::CODEGEN_FAILED,
                format!("`{name}` has no size: a `struct` field must reach itself through a pointer"),
            )
            .with_span(span),
        );
        self.struct_errors.insert(name.to_string());
        None
    }

    /// A declared record's fields, in declaration order.
    fn struct_field_list(&self, info: &StructInfo) -> &[(&'static str, DuxType)] {
        self.struct_fields
            .get(info.name)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    fn struct_of(&self, ty: DuxType) -> Option<&'static StructInfo> {
        match ty {
            DuxType::Struct(info) => Some(info),
            _ => None,
        }
    }

    /// `*T` made of `T`.
    fn pointer_to(&self, ty: DuxType) -> DuxType {
        DuxType::Pointer(Box::leak(Box::new(ty)))
    }

    /// What a pointer points at, when the type says.
    fn pointee_of(&self, ty: DuxType) -> Option<DuxType> {
        match ty {
            DuxType::Pointer(target) => Some(*target),
            _ => None,
        }
    }

    /// The record a value reaches: a record, or a pointer to one. `p.field`
    /// auto-dereferences, so both spellings land here.
    fn record_of(&self, ty: DuxType) -> Option<&'static StructInfo> {
        match ty {
            DuxType::Struct(info) => Some(info),
            DuxType::Pointer(target) => self.struct_of(*target),
            _ => None,
        }
    }
}

impl Emitter {
    /// Which functions carry the hidden `context` parameter: the ones that
    /// allocate, read `context`, or call something that does.
    ///
    /// Calls are resolved by name, and a method call against every method of
    /// that name. Being conservative is safe — a hidden parameter is invisible
    /// in the source — while missing one would be a miscompiled call.
    fn allocation_analysis(&self, program: &Program) -> HashSet<String> {
        let mut keys: Vec<String> = Vec::new();
        let mut names: Vec<String> = Vec::new();
        let mut bodies: Vec<&Block> = Vec::new();
        let mut fixed: Vec<bool> = Vec::new();
        for decl in program.functions() {
            keys.push(decl.name.name.clone());
            names.push(decl.name.name.clone());
            bodies.push(&decl.body);
            fixed.push(has_attribute(&decl.attributes, "no_context"));
        }
        for item in program.decls.iter().filter_map(|decl| match decl {
            Decl::Impl(block) => Some(block),
            _ => None,
        }) {
            let owner = item.type_name.name.as_str();
            for method in &item.methods {
                keys.push(format!("{owner}.{}", method.name.name));
                names.push(method.name.name.clone());
                bodies.push(&method.body);
                fixed.push(has_attribute(&method.attributes, "no_context"));
            }
        }

        // What a name holds, as far as the spelling tells: every function
        // written as returning a `string`, and every field written as one.
        let mut text_calls: HashSet<String> = HashSet::new();
        let mut text: HashSet<String> = HashSet::new();
        for decl in program.functions() {
            if decl.return_ty.as_ref().is_some_and(Scan::spelled_text) {
                text_calls.insert(decl.name.name.clone());
            }
        }
        for item in program.decls.iter() {
            match item {
                Decl::Struct(record) => {
                    for field in &record.fields {
                        if Scan::spelled_text(&field.ty) {
                            text.insert(field.name.name.clone());
                        }
                    }
                }
                Decl::Impl(block) => {
                    for method in &block.methods {
                        if method.return_ty.as_ref().is_some_and(Scan::spelled_text) {
                            text_calls.insert(method.name.name.clone());
                        }
                    }
                }
                _ => {}
            }
        }

        let mut scans: Vec<Scan> = Vec::new();
        for (body, params) in bodies.iter().zip(parameter_lists(program)) {
            let mut scan = Scan {
                text: text.clone(),
                text_calls: text_calls.clone(),
                ..Scan::default()
            };
            for param in params {
                if Scan::spelled_text(&param.ty) {
                    scan.text.insert(param.name.name.clone());
                }
            }
            scan.block(body);
            scans.push(scan);
        }
        let mut by_name: HashMap<&str, Vec<usize>> = HashMap::new();
        for (index, name) in names.iter().enumerate() {
            by_name.entry(name.as_str()).or_default().push(index);
        }

        let mut needs: HashSet<usize> = HashSet::new();
        for (index, scan) in scans.iter().enumerate() {
            if scan.allocates || scan.reads_context {
                needs.insert(index);
            }
        }
        loop {
            let mut grew = false;
            for (index, scan) in scans.iter().enumerate() {
                if needs.contains(&index) {
                    continue;
                }
                let follows = scan.calls.iter().any(|call| {
                    by_name
                        .get(call.as_str())
                        .into_iter()
                        .flatten()
                        .any(|callee| *callee != index && needs.contains(callee))
                });
                if follows {
                    needs.insert(index);
                    grew = true;
                }
            }
            if !grew {
                break;
            }
        }

        needs
            .into_iter()
            .filter(|index| !fixed[*index])
            .map(|index| keys[index].clone())
            .collect()
    }

    /// The running function's `context`, which every allocation goes through.
    fn context_value(&mut self, span: Span) -> Option<PointerValue<'static>> {
        let Some(slot) = self.context_slot else {
            self.unsupported(
                span,
                "allocating in a function that has no `context`: mark it `#[no_context]` \
                 and take the allocator as an argument",
            );
            return None;
        };
        self.builder
            .build_load(self.context.ptr_type(AddressSpace::default()), slot, "context")
            .ok()
            .map(|value| value.into_pointer_value())
    }

    /// Bind `context` for the running function. `main` starts with the default
    /// context; every other allocating function is handed its caller's.
    fn bind_context(&mut self, module: &Module<'static>, argument: Option<u32>) {
        let Some(runtime) = &self.runtime else {
            return;
        };
        let default = runtime.default_context;
        let report = runtime.report;
        let pointer_type = self.context.ptr_type(AddressSpace::default());
        let slot = self.alloca_in_entry(pointer_type.into(), "context");
        let value = match argument {
            Some(index) => match self.function_argument(index) {
                Some(value) => value,
                None => return,
            },
            None => {
                if let Some(report) = report {
                    let atexit = self.declare_c(
                        module,
                        "atexit",
                        self.context
                            .i32_type()
                            .fn_type(&[pointer_type.into()], false),
                    );
                    let _ = self.builder.build_call(
                        atexit,
                        &[report.as_global_value().as_pointer_value().into()],
                        "atexit",
                    );
                }
                default
            }
        };
        let _ = self.builder.build_store(slot, value);
        self.context_slot = Some(slot);
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert("context".to_string(), (DuxType::Ptr, slot));
        }
    }

    fn function_argument(&self, index: u32) -> Option<PointerValue<'static>> {
        let function = self.running_function?;
        function
            .get_nth_param(index)
            .map(|value| value.into_pointer_value())
    }

    fn declare_c(
        &mut self,
        module: &Module<'static>,
        name: &str,
        fn_type: inkwell::types::FunctionType<'static>,
    ) -> FunctionValue<'static> {
        if let Some(function) = self.c_functions.get(name).copied() {
            return function;
        }
        let function = declared(module, name, fn_type);
        self.c_functions.insert(name.to_string(), function);
        function
    }

    /// `new`, `make`, `free` and `del` reach the allocator through the hidden
    /// context. Their first argument names a type, which is not a value the
    /// lowering can otherwise evaluate.
    /// Bytes from the current context's allocator. The count is in bytes; the
    /// block comes back uninitialized.
    fn allocate(
        &mut self,
        span: Span,
        count: IntValue<'static>,
        align: u32,
    ) -> Option<PointerValue<'static>> {
        let Some(alloc) = self.runtime.as_ref().map(|runtime| runtime.alloc) else {
            self.unsupported(span, "allocating in a program with no memory runtime");
            return None;
        };
        let Some(context) = self.context_value(span) else {
            self.unsupported(
                span,
                "allocating in a function that carries no context",
            );
            return None;
        };
        let location = self.location(span);
        self.builder
            .build_call(
                alloc,
                &[
                    context.into(),
                    count.into(),
                    self.context.i64_type().const_int(align as u64, false).into(),
                    location.into(),
                ],
                "alloc",
            )
            .ok()?
            .try_as_basic_value()
            .basic()
            .map(|value| value.into_pointer_value())
    }

    fn memory_builtin(
        &mut self,
        module: &Module<'static>,
        name: &str,
        args: &[Expr],
        span: Span,
    ) -> Option<Typed> {
        let (alloc, free) = match &self.runtime {
            Some(runtime) => (runtime.alloc, runtime.free),
            None => {
                self.unsupported(span, "allocation");
                return None;
            }
        };
        let context = self.context_value(span)?;
        let location = self.location(args.first().map_or(span, Expr::span));
        match name {
            "new" => {
                let element = self.type_argument(args.first(), name)?;
                let llvm = element.llvm(self.context);
                let size = self.size_of(llvm)?;
                let align = self.alignment_of(llvm);
                let pointer = self
                    .builder
                    .build_call(
                        alloc,
                        &[
                            context.into(),
                            size.into(),
                            self.context.i64_type().const_int(align as u64, false).into(),
                            location.into(),
                        ],
                        "new",
                    )
                    .ok()?
                    .try_as_basic_value()
                    .basic()
                    .map(|value| value.into_pointer_value())?;
                Some(Typed {
                    core: Core::Ptr(pointer),
                    ty: self.pointer_to(element),
                })
            }
            "make" => {
                if args.len() != 2 {
                    self.diagnostics.push(
                        Diagnostic::error(
                            code::WRONG_ARITY,
                            "`make` takes a type and a length".to_string(),
                        )
                        .with_span(span),
                    );
                    return None;
                }
                let element = self.type_argument(args.first(), name)?;
                let count = self.expr(module, &args[1], Some(DuxType::I64))?;
                let Some(count) = count.int_value() else {
                    self.unsupported(args[1].span(), "a non-integer length for `make`");
                    return None;
                };
                let llvm = element.llvm(self.context);
                let each = self.size_of(llvm)?;
                let align = self.alignment_of(llvm);
                let total = self
                    .builder
                    .build_int_mul(each, count, "bytes")
                    .ok()?;
                let pointer = self
                    .builder
                    .build_call(
                        alloc,
                        &[
                            context.into(),
                            total.into(),
                            self.context.i64_type().const_int(align as u64, false).into(),
                            location.into(),
                        ],
                        "make",
                    )
                    .ok()?
                    .try_as_basic_value()
                    .basic()
                    .map(|value| value.into_pointer_value())?;
                let zero = self.context.i8_type().const_zero();
                let _ = self.builder.build_memset(pointer, align, zero, total);
                Some(self.view(element, pointer, count))
            }
            "free" => {
                let value = self.expr(module, args.first()?, None)?;
                let Some(pointer) = value.pointer_value() else {
                    self.unsupported(args[0].span(), "`free` of something that is not a pointer");
                    return None;
                };
                let _ = self
                    .builder
                    .build_call(free, &[context.into(), pointer.into()], "free");
                Some(Typed::void())
            }
            "del" => {
                let value = self.expr(module, args.first()?, None)?;
                let Some((pointer, _)) = self.slice_parts(value) else {
                    self.unsupported(args[0].span(), "`del` of something that is not a slice");
                    return None;
                };
                let _ = self
                    .builder
                    .build_call(free, &[context.into(), pointer.into()], "del");
                Some(Typed::void())
            }
            _ => None,
        }
    }

    /// The type named as a builtin's first argument, as in `new(Vec3)`.
    fn type_argument(&mut self, argument: Option<&Expr>, name: &str) -> Option<DuxType> {
        let Some(Expr::Ident { name: type_name, span }) = argument else {
            self.unsupported(
                argument.map_or_else(Span::default, Expr::span),
                &format!("`{name}` wants a type"),
            );
            return None;
        };
        let written = Type::Named {
            name: type_name.clone(),
            args: Vec::new(),
            span: *span,
        };
        match self.type_of(&written) {
            Some(ty) => Some(ty),
            None => {
                self.unsupported(
                    *span,
                    &format!("`{type_name}` is not a type `{name}` knows"),
                );
                None
            }
        }
    }

    /// `sizeof` without a target data handle: one past a null element folds to
    /// a constant the moment LLVM sees it.
    fn size_of(&mut self, ty: BasicTypeEnum<'static>) -> Option<IntValue<'static>> {
        let pointer_type = self.context.ptr_type(AddressSpace::default());
        let one = self.context.i64_type().const_int(1, false);
        let past = unsafe {
            self.builder
                .build_gep(ty, pointer_type.const_null(), &[one], "elements")
                .ok()?
        };
        self.builder
            .build_ptr_to_int(past, self.context.i64_type(), "size")
            .ok()
    }

    /// The alignment the allocator must respect. A fixed shape's alignment is
    /// its first field's, and no primitive the language has asks for more than
    /// eight bytes.
    fn alignment_of(&mut self, ty: BasicTypeEnum<'static>) -> u32 {
        let size = self.size_of(ty).and_then(|size| size.get_zero_extended_constant());
        match size {
            Some(value) if value < 8 => value as u32,
            _ => 8,
        }
    }

    /// A `"file:line"` literal for the allocator's location field.
    fn location(&mut self, span: Span) -> PointerValue<'static> {
        let file = self.sources.name_of(span.file).to_string();
        let line = self.sources.line_col(span.file, span.start).0;
        self.builder
            .build_global_string_ptr(&format!("{file}:{line}"), "dux.location")
            .expect("location")
            .as_pointer_value()
    }

    fn declare(&mut self, module: &Module<'static>, unit: &Unit) {
        let key = unit.key.clone();
        let external = unit.external;
        let function = unit.decl;
        if self.skipped.contains(&key) || self.functions.contains_key(&key) {
            if !external {
                self.diagnostics.push(
                    Diagnostic::error(
                        code::DUPLICATE_DEFINITION,
                        match unit.owner {
                            Some(owner) => format!(
                                "`{owner}` has more than one `{}`",
                                function.name.name
                            ),
                            None => format!("`{key}` is declared more than once"),
                        },
                    )
                    .with_span(function.name.span),
                );
            }
            return;
        }
        if !external && unit.owner.is_none() && function.name.name == "main" {
            let fn_value =
                module.add_function("main", self.context.i32_type().fn_type(&[], false), None);
            self.functions.insert(key.clone(), fn_value);
            self.signatures.insert(
                key,
                Signature {
                    params: Vec::new(),
                    return_ty: Some(DuxType::I32),
                    receiver: None,
                    hidden_context: false,
                },
            );
            return;
        }
        if !function.type_params.is_empty() {
            self.skipped.insert(key.clone());
            self.unsupported(function.name.span, "a generic `fn` (monomorphisation lands at M5)");
            return;
        }
        let mut params = Vec::new();
        let mut receiver = None;
        for (index, param) in function.params.iter().enumerate() {
            let ty = match self.parameter_type(unit, param, index == 0) {
                Some(ty) => ty,
                None => {
                    self.refuse_parameter(&key, param, external);
                    return;
                }
            };
            if unit.takes_a_receiver() && index == 0 {
                receiver = Some(ty);
                continue;
            }
            if ty.array_info().is_some() {
                self.refuse_parameter(&key, param, external);
                return;
            }
            params.push(ty);
        }
        let return_ty = match &function.return_ty {
            None => None,
            Some(ty) => match self.type_of(ty).filter(|ty| ty.array_info().is_none()) {
                Some(ty) => Some(ty),
                None => {
                    self.skipped.insert(key.clone());
                    if !external {
                        self.unsupported(ty.span(), "this result type (primitives, pointers and records today)");
                    }
                    return;
                }
            },
        };
        // An imported function is C's, so its parameter list is exactly what
        // was written; only a Dux definition carries the hidden context.
        let hidden_context = !external && self.allocating.contains(&key);
        let mut declared = receiver
            .iter()
            .chain(params.iter())
            .map(|ty| BasicMetadataTypeEnum::from(ty.llvm(self.context)))
            .collect::<Vec<BasicMetadataTypeEnum>>();
        if hidden_context {
            declared.push(self.context.ptr_type(AddressSpace::default()).into());
        }
        let signature = match return_ty {
            Some(ty) => ty.llvm(self.context).fn_type(&declared, false),
            None => self.context.void_type().fn_type(&declared, false),
        };
        let fn_value = module.add_function(&unit.symbol, signature, None);
        self.functions.insert(key.clone(), fn_value);
        self.signatures.insert(
            key,
            Signature {
                params,
                return_ty,
                receiver,
                hidden_context,
            },
        );
    }

    /// A parameter the lowering cannot pass by value today.
    fn refuse_parameter(&mut self, key: &str, param: &dux_ast::Param, external: bool) {
        self.skipped.insert(key.to_string());
        if external {
            return;
        }
        self.unsupported(
            param.ty.span(),
            &format!(
                "a `{}` parameter (only primitives, pointers, records and string views lower today)",
                param.name.name
            ),
        );
    }

    /// The type of a parameter as the lowering sees it. Inside an `impl`
    /// block, `self` is the record itself, or a pointer to it when the method
    /// is written `fn m(self: *T, …)`; every other parameter is an annotated
    /// type.
    fn parameter_type(
        &mut self,
        unit: &Unit,
        param: &dux_ast::Param,
        first: bool,
    ) -> Option<DuxType> {
        let Some(owner) = unit.owner else {
            return self.type_of(&param.ty);
        };
        if !first || param.name.name != "self" {
            return self.type_of(&param.ty);
        }
        let record = self.struct_type_of(owner)?;
        Some(match &param.ty {
            Type::Pointer { .. } => self.pointer_to(record),
            _ => record,
        })
    }

    fn lower_body(&mut self, module: &Module<'static>, unit: &Unit) {
        let key = unit.key.as_str();
        let function = unit.decl;
        if self.skipped.contains(key) || function.body.stmts.is_empty() {
            return;
        }
        let Some(fn_value) = self.functions.get(key).copied() else {
            return;
        };
        let hidden_context = self
            .signatures
            .get(key)
            .is_some_and(|signature| signature.hidden_context);
        self.begin_function_debug(function, &unit.symbol, fn_value);
        let return_ty = self.signatures.get(key).and_then(|s| s.return_ty);
        let entry = self.context.append_basic_block(fn_value, "entry");
        let body = self.context.append_basic_block(fn_value, "body");
        self.builder.position_at_end(entry);
        self.allocas = Some(entry);
        self.scopes.clear();
        self.scopes.push(HashMap::new());
        self.loops.clear();
        self.defers.clear();
        self.dead = false;
        self.is_main = unit.owner.is_none() && function.name.name == "main";
        self.result_ty = if self.is_main {
            Some(DuxType::I32)
        } else {
            return_ty
        };

        self.running_function = Some(fn_value);
        self.context_slot = None;
        if self.allocating.contains(key) {
            let argument = if self.is_main || !hidden_context {
                None
            } else {
                Some(function.params.len() as u32)
            };
            self.bind_context(module, argument);
        }

        for (index, param) in function.params.iter().enumerate() {
            let Some(ty) = self.parameter_type(unit, param, index == 0) else {
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
                BasicValueEnum::StructValue(value) => Core::Aggregate(value),
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
        self.context_slot = None;
        self.context_saves.clear();
        self.running_function = None;
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
        self.block_body_bound(module, block, &[]);
    }

    /// A block whose scope starts with the bindings a pattern made elsewhere:
    /// `while P := e { … }` tests in one block and binds in the body.
    fn block_body_bound(&mut self, module: &Module<'static>, block: &Block, bound: &[Binding]) {
        self.scopes.push(HashMap::new());
        self.declare_bindings(bound);
        let outer = self.save_context(block);
        for stmt in &block.stmts {
            if self.dead {
                break;
            }
            self.stmt(module, stmt);
        }
        self.restore_context(outer);
        self.scopes.pop();
    }

    /// Move a value into a slot of its own type, which is how a `match` gets a
    /// subject it can take apart. An array is storage rather than a word, so it
    /// is copied element by element like a binding copies it.
    fn put_in(
        &mut self,
        slot: PointerValue<'static>,
        value: Typed,
        span: Span,
    ) -> Option<()> {
        match (value.ty.array_info(), value.core) {
            (Some(array), Core::Ptr(source)) => self.copy_array(slot, source, array),
            (None, _) => {
                let basic = value.basic().or_else(|| {
                    self.unsupported(
                        span,
                        "matching on this value (it does not travel as one yet)",
                    );
                    None
                })?;
                let _ = self.builder.build_store(slot, basic).ok()?;
            }
            (Some(_), _) => {
                self.unsupported(span, "matching on this array value");
                return None;
            }
        }
        Some(())
    }

    /// `match e { P => … }`, as a statement or (with `value`) as an expression.
    /// The subject is copied into a slot of its own, the arms test it in the
    /// order they are written, and the arm that matches runs and leaves. A
    /// guarded arm whose guard declines is not a match, so the chain moves on.
    fn matching(
        &mut self,
        module: &Module<'static>,
        subject: &Expr,
        arms: &[dux_ast::MatchArm],
        expected: Option<DuxType>,
        value: bool,
        span: Span,
    ) -> Option<Typed> {
        let held = self.expr(module, subject, None)?;
        if held.ty == DuxType::Error {
            return Some(Typed::error());
        }
        let ty = held.ty;
        let place = self.alloca(ty, "match");
        self.put_in(place, held, span)?;

        let mut join = Join {
            block: self.new_block("match.end"),
            agreed: expected,
            carried: Vec::new(),
            broken: false,
        };
        let mut rest = self.new_block("match.test");
        self.br(rest);
        for arm in arms {
            self.builder.position_at_end(rest);
            self.dead = false;
            let body = self.new_block("match.arm");
            let next = self.new_block("match.next");
            let entered = if arm.guard.is_some() {
                self.new_block("match.guard")
            } else {
                body
            };
            let mut bound = Vec::new();
            self.pattern_holds(module, &arm.pattern, ty, place, entered, next, &mut bound);
            self.builder.position_at_end(entered);
            self.dead = false;
            self.scopes.push(HashMap::new());
            self.declare_bindings(&bound);
            if let Some(guard) = &arm.guard {
                let mut bound = bound.clone();
                let test = self.condition_bound(module, guard, &mut bound);
                self.declare_bindings(&bound);
                let _ = self.builder.build_conditional_branch(test, body, next);
                self.builder.position_at_end(body);
                self.dead = false;
            }
            self.arm_arrives(module, arm, value, &mut join);
            rest = next;
        }

        // DESIGN 4.4: a `match` that is not exhaustive is an error, so an arm
        // always matches and the chain never runs out. A hole in that rule is a
        // compiler bug, and a bug should say so rather than run on.
        self.builder.position_at_end(rest);
        self.dead = false;
        self.trap(module, "dux: no `match` arm matched\n");

        self.builder.position_at_end(join.block);
        self.dead = false;
        if join.block.get_first_use().is_none() {
            // Every arm left, so nothing reaches here.
            let _ = self.builder.build_unreachable();
            self.dead = true;
            return if value { Some(Typed::error()) } else { None };
        }
        if !value {
            return None;
        }
        if join.broken || join.carried.is_empty() {
            let _ = self.builder.build_unreachable();
            self.dead = true;
            return Some(Typed::error());
        }
        let common = join.agreed.expect("an arm gave the `match` its type");
        let phi = self
            .builder
            .build_phi(common.llvm(self.context), "matchtmp")
            .ok()?;
        let mut incoming: Vec<(inkwell::values::BasicValueEnum<'static>, BasicBlock<'static>)> =
            Vec::new();
        for (one, ended) in &join.carried {
            let Some(basic) = one.basic() else {
                let _ = self.builder.build_unreachable();
                self.dead = true;
                return Some(Typed::error());
            };
            incoming.push((basic, *ended));
        }
        phi.add_incoming(
            &incoming
                .iter()
                .map(|(basic, ended)| (basic as &dyn inkwell::values::BasicValue<'static>, *ended))
                .collect::<Vec<_>>(),
        );
        Some(typed_of(phi.as_basic_value(), common))
    }

    /// Run the arm that matched, and bring whatever it produces to the place
    /// where the arms meet. The arm's scope is pushed and its names bound
    /// before this runs; an arm that leaves by `return` brings nothing.
    fn arm_arrives(
        &mut self,
        module: &Module<'static>,
        arm: &dux_ast::MatchArm,
        value: bool,
        join: &mut Join<'static>,
    ) {
        let outcome = self.match_body(module, &arm.body, join.agreed);
        let ended = self.builder.get_insert_block();
        self.scopes.pop();
        let Some(one) = outcome else {
            if value && !self.dead {
                self.unsupported(arm.span, "an arm of a value `match` that produces no value");
                join.broken = true;
            } else if !self.dead {
                self.br(join.block);
            }
            return;
        };
        if self.dead {
            return;
        }
        if one.ty == DuxType::Error {
            join.broken = true;
            self.br(join.block);
            return;
        }
        if join.agreed.is_none() {
            join.agreed = Some(one.ty);
        }
        let common = join.agreed.expect("the first arm gives the `match` its type");
        match (self.convert(one, common), ended) {
            (Some(carried), Some(ended)) => {
                join.carried.push((carried, ended));
                self.br(join.block);
            }
            _ => join.broken = true,
        }
    }

    /// What an arm does. `None` says it produces no value, which a `match` used
    /// as a statement is fine with and a `match` used as a value is not.
    fn match_body(
        &mut self,
        module: &Module<'static>,
        body: &dux_ast::MatchBody,
        expected: Option<DuxType>,
    ) -> Option<Typed> {
        match body {
            dux_ast::MatchBody::Expr(expr) => self.expr(module, expr, expected),
            dux_ast::MatchBody::Block(block) => {
                self.scopes.push(HashMap::new());
                let tail = block.stmts.len().saturating_sub(1);
                let value = match block.stmts.last() {
                    Some(Stmt::Expr { expr, .. }) => Some(expr),
                    _ => None,
                };
                for (index, stmt) in block.stmts.iter().enumerate() {
                    if self.dead {
                        break;
                    }
                    if value.is_some() && index == tail {
                        continue;
                    }
                    self.stmt(module, stmt);
                }
                let outcome = match value {
                    Some(expr) if !self.dead => self.expr(module, expr, expected),
                    _ => None,
                };
                self.scopes.pop();
                outcome
            }
            dux_ast::MatchBody::Stmt(stmt) => {
                self.stmt(module, stmt);
                None
            }
        }
    }

    /// Emit the tests that decide whether the value at `place` matches
    /// `pattern`, branching to `matched` or `declined`. The names the pattern
    /// binds are collected in `bound`: their slots live in the entry block and
    /// are written along the path that reaches `matched`, so an arm body that
    /// reads them sees what it matched.
    #[allow(clippy::too_many_arguments)]
    fn pattern_holds(
        &mut self,
        module: &Module<'static>,
        pattern: &dux_ast::Pattern,
        ty: DuxType,
        place: PointerValue<'static>,
        matched: BasicBlock<'static>,
        declined: BasicBlock<'static>,
        bound: &mut Vec<Binding>,
    ) {
        match pattern {
            dux_ast::Pattern::Wildcard { .. } | dux_ast::Pattern::Else { .. } => self.br(matched),
            dux_ast::Pattern::Error { .. } => self.br(declined),
            dux_ast::Pattern::Name { name, span } => {
                match self.variant_of(ty, None, &name.name) {
                    Some((index, _)) => {
                        self.variant_test(ty, index, place, matched, declined, *span)
                    }
                    None => {
                        self.bind_pattern(&name.name, ty, place, bound);
                        self.br(matched);
                    }
                }
            }
            dux_ast::Pattern::Path {
                qualifier,
                name,
                span,
            } => match self.variant_of(ty, Some(qualifier), name) {
                Some((index, _)) => {
                    self.variant_test(ty, index, place, matched, declined, *span)
                }
                None => {
                    self.missing_variant(ty, Some(qualifier), name, *span);
                    self.br(declined);
                }
            },
            dux_ast::Pattern::Tuple {
                qualifier,
                name,
                elements,
                span,
            } => {
                let Some((index, variant)) = self.variant_of(ty, qualifier.as_deref(), name)
                else {
                    self.missing_variant(ty, qualifier.as_deref(), name, *span);
                    self.br(declined);
                    return;
                };
                if elements.len() != variant.fields.len() {
                    self.diagnostics.push(
                        Diagnostic::error(
                            code::WRONG_ARITY,
                            format!(
                                "`{name}` carries {} value(s), the pattern takes apart {}",
                                variant.fields.len(),
                                elements.len()
                            ),
                        )
                        .with_span(*span),
                    );
                    self.br(declined);
                    return;
                }
                if !self.variant_test_to(ty, index, place, declined) {
                    self.br(declined);
                    return;
                }
                if elements.is_empty() {
                    self.br(matched);
                    return;
                }
                for (position, element) in elements.iter().enumerate() {
                    let (_, field_ty) = variant.fields[position];
                    let at = self.byte_at(place, variant.offsets[position]);
                    let last = position + 1 == elements.len();
                    let onward = if last { matched } else { self.new_block("match.element") };
                    self.pattern_holds(module, element, field_ty, at, onward, declined, bound);
                    if !last {
                        self.builder.position_at_end(onward);
                        self.dead = false;
                    }
                }
            }
            dux_ast::Pattern::Struct {
                qualifier,
                name,
                fields,
                rest,
                span,
            } => {
                let Some((index, variant)) = self.variant_of(ty, qualifier.as_deref(), name)
                else {
                    self.missing_variant(ty, qualifier.as_deref(), name, *span);
                    self.br(declined);
                    return;
                };
                if !fields.is_empty() && !*rest && fields.len() != variant.fields.len() {
                    self.diagnostics.push(
                        Diagnostic::error(
                            code::TYPE_MISMATCH,
                            format!(
                                "`{name}` carries {} value(s), the pattern names {}",
                                variant.fields.len(),
                                fields.len()
                            ),
                        )
                        .with_note("name them all, or write `..` for the rest")
                        .with_span(*span),
                    );
                    self.br(declined);
                    return;
                }
                if !self.variant_test_to(ty, index, place, declined) {
                    self.br(declined);
                    return;
                }
                if fields.is_empty() {
                    self.br(matched);
                    return;
                }
                for (position, field) in fields.iter().enumerate() {
                    let (field_name, nested) = match field {
                        dux_ast::FieldPattern::Shorthand { name, .. } => (&name.name, None),
                        dux_ast::FieldPattern::Nested { name, pattern, .. } => {
                            (&name.name, Some(&**pattern))
                        }
                    };
                    let inside = variant.fields.iter().position(|(known, _)| known == field_name);
                    let Some(inside) = inside else {
                        self.missing_variant_field(ty, name, field_name, *span);
                        self.br(declined);
                        return;
                    };
                    let (_, field_ty) = variant.fields[inside];
                    let at = self.byte_at(place, variant.offsets[inside]);
                    let last = position + 1 == fields.len();
                    let onward = if last { matched } else { self.new_block("match.field") };
                    match nested {
                        Some(nested) => {
                            self.pattern_holds(module, nested, field_ty, at, onward, declined, bound)
                        }
                        None => {
                            self.bind_pattern(field_name, field_ty, at, bound);
                            self.br(onward);
                        }
                    }
                    if !last {
                        self.builder.position_at_end(onward);
                        self.dead = false;
                    }
                }
            }
            dux_ast::Pattern::Literal { value, span } => {
                match self.literal_holds(module, value, ty, place, *span) {
                    Some(flag) => {
                        let _ = self.builder.build_conditional_branch(flag, matched, declined);
                    }
                    None => self.br(declined),
                }
            }
            dux_ast::Pattern::Range {
                start,
                end,
                inclusive,
                span,
            } => {
                let low = self.range_bound(module, start, true, *inclusive, ty, place, bound, *span);
                let high = self.range_bound(module, end, false, *inclusive, ty, place, bound, *span);
                let test = match (low, high) {
                    (Some(low), Some(high)) => self.builder.build_and(low, high, "within").ok(),
                    (Some(one), None) => Some(one),
                    (None, Some(one)) => Some(one),
                    (None, None) => None,
                };
                match test {
                    Some(flag) => {
                        let _ = self.builder.build_conditional_branch(flag, matched, declined);
                    }
                    // Nothing was written to compare against: the bound named
                    // the subject, and any subject is within it.
                    None => self.br(matched),
                }
            }
            dux_ast::Pattern::Or { alternatives, .. } => {
                // Every alternative tests the same value and binds the same
                // names, so they all end at `matched` and each writes the one
                // slot the arm body reads.
                for alternative in alternatives {
                    let onward = self.new_block("match.or");
                    self.pattern_holds(module, alternative, ty, place, matched, onward, bound);
                    self.builder.position_at_end(onward);
                    self.dead = false;
                }
                self.br(declined);
            }
        }
    }

    /// A variant test that continues into its own block, so a pattern with a
    /// payload can take it apart there. `false` when the test could not be
    /// built at all.
    fn variant_test_to(
        &mut self,
        ty: DuxType,
        index: u32,
        place: PointerValue<'static>,
        declined: BasicBlock<'static>,
    ) -> bool {
        let Some(info) = self.enum_of(ty) else {
            return false;
        };
        let Some(flag) = self.variant_holds(info, index, place) else {
            return false;
        };
        let payload = self.new_block("match.payload");
        let _ = self
            .builder
            .build_conditional_branch(flag, payload, declined);
        self.builder.position_at_end(payload);
        self.dead = false;
        true
    }

    /// A nullary variant written by name: the tag is the whole test.
    fn variant_test(
        &mut self,
        ty: DuxType,
        index: u32,
        place: PointerValue<'static>,
        matched: BasicBlock<'static>,
        declined: BasicBlock<'static>,
        span: Span,
    ) {
        if !self.variant_test_to(ty, index, place, declined) {
            self.unsupported(span, "matching a variant of a value that is not a sum type");
            self.br(declined);
            return;
        }
        self.br(matched);
    }

    /// The variant a pattern names, when the subject's type has it. A pattern
    /// may name the enum or leave it to be inferred, but a second enum in the
    /// pattern's place is a different type and matches nothing.
    fn variant_of(
        &self,
        ty: DuxType,
        qualifier: Option<&str>,
        name: &str,
    ) -> Option<(u32, &'static VariantInfo)> {
        let info = self.enum_of(ty)?;
        if qualifier.is_some_and(|owner| owner != info.name) {
            return None;
        }
        info.variant(name)
    }

    fn missing_variant(&mut self, ty: DuxType, qualifier: Option<&str>, name: &str, span: Span) {
        let spelled = qualifier.map_or_else(|| name.to_string(), |owner| format!("{owner}::{name}"));
        self.diagnostics.push(
            Diagnostic::error(
                code::TYPE_MISMATCH,
                format!("`{spelled}` is not a variant of `{}`", ty.name()),
            )
            .with_span(span)
            .with_note("a `match` arm names a variant of the type it matches"),
        );
    }

    fn missing_variant_field(
        &mut self,
        ty: DuxType,
        variant: &str,
        field: &str,
        span: Span,
    ) {
        self.diagnostics.push(
            Diagnostic::error(
                code::TYPE_MISMATCH,
                format!("`{variant}` has no field `{field}` in its payload"),
            )
            .with_span(span)
            .with_note(format!("`{}` carries it under other names", ty.name())),
        );
    }

    /// A name a pattern binds gets a slot like any other local. An `or` pattern
    /// names the same local in every alternative, so a name already bound keeps
    /// the slot it has and the alternative writes it again.
    fn bind_pattern(
        &mut self,
        name: &str,
        ty: DuxType,
        from: PointerValue<'static>,
        bound: &mut Vec<Binding>,
    ) {
        let slot = match bound.iter().find(|(known, _, _)| known == name) {
            Some((_, _, slot)) => *slot,
            None => {
                let slot = self.alloca(ty, name);
                bound.push((name.to_string(), ty, slot));
                slot
            }
        };
        if let Some(array) = ty.array_info() {
            self.copy_array(slot, from, array);
            return;
        }
        if let Some(held) = self.load(ty, from) {
            if let Some(basic) = held.basic() {
                let _ = self.builder.build_store(slot, basic);
            }
        }
    }

    /// Pattern bindings become ordinary slots in the arm's own scope.
    fn declare_bindings(&mut self, bound: &[Binding]) {
        if let Some(scope) = self.scopes.last_mut() {
            for (name, ty, slot) in bound {
                scope.insert(name.clone(), (*ty, *slot));
            }
        }
    }

    /// Is the value at `place` equal to this literal?
    fn literal_holds(
        &mut self,
        module: &Module<'static>,
        value: &Expr,
        ty: DuxType,
        place: PointerValue<'static>,
        span: Span,
    ) -> Option<IntValue<'static>> {
        let want = self.expr(module, value, Some(ty))?;
        let held = self.load(ty, place)?;
        let same = if ty == DuxType::Str || want.ty == DuxType::Str || ty.slice_element().is_some() {
            self.text_binary(module, dux_ast::BinaryOp::Eq, held, want, span)
        } else {
            self.binary_typed(module, dux_ast::BinaryOp::Eq, held, want, span)
        };
        same.and_then(|same| same.int_value())
    }

    /// One bound of a range pattern. A bound written as a literal is the
    /// comparison it implies; a bound written as a name names the subject
    /// itself, which is all the checker asks of it.
    #[allow(clippy::too_many_arguments)]
    fn range_bound(
        &mut self,
        module: &Module<'static>,
        bound: &dux_ast::Pattern,
        is_start: bool,
        inclusive: bool,
        ty: DuxType,
        place: PointerValue<'static>,
        names: &mut Vec<Binding>,
        span: Span,
    ) -> Option<IntValue<'static>> {
        match bound {
            dux_ast::Pattern::Literal { value, span } => {
                let want = self.expr(module, value, Some(ty))?;
                let held = self.load(ty, place)?;
                let op = match (is_start, inclusive) {
                    (true, true) => dux_ast::BinaryOp::Ge,
                    (true, false) => dux_ast::BinaryOp::Gt,
                    (false, true) => dux_ast::BinaryOp::Le,
                    (false, false) => dux_ast::BinaryOp::Lt,
                };
                self.binary_typed(module, op, held, want, *span)
                    .and_then(|within| within.int_value())
            }
            dux_ast::Pattern::Name { name, .. } => {
                self.bind_pattern(&name.name, ty, place, names);
                None
            }
            other => {
                self.unsupported(
                    other.span(),
                    "a range bound that is not a literal or a name",
                );
                let _ = span;
                None
            }
        }
    }

    /// Stop the program where it broke a language rule the compiler could not
    /// check for it.
    fn trap(&mut self, module: &Module<'static>, message: &str) {
        // A program that allocates nothing still has to stop when a language
        // rule is broken, so the runtime arrives on demand for the trap alone.
        if self.runtime.is_none() {
            self.runtime = Some(Runtime::emit(self.context, module, self.debug_allocator));
        }
        let Some(trap) = self.runtime.as_ref().map(|runtime| runtime.trap) else {
            let _ = self.builder.build_unreachable();
            self.dead = true;
            return;
        };
        let text = self
            .builder
            .build_global_string_ptr(message, "trap.message")
            .expect("trap message")
            .as_pointer_value();
        let _ = self.builder.build_call(trap, &[text.into()], "trap");
        let _ = self.builder.build_unreachable();
        self.dead = true;
    }

    /// A block that rebinds `context` gets the outer value back on exit, so
    /// `context = arena.ctx()` lasts exactly as long as the block that wrote
    /// it. The saved pointer lives in the entry block, where every slot of the
    /// function lives.
    fn save_context(&mut self, block: &Block) -> Option<PointerValue<'static>> {
        let slot = self.context_slot?;
        let mut scan = Scan::default();
        scan.block(block);
        if !scan.rebinds_context {
            return None;
        }
        let pointer_type = self.context.ptr_type(AddressSpace::default());
        let saved = self.alloca_in_entry(pointer_type.into(), "context.outer");
        let outer = self
            .builder
            .build_load(pointer_type, slot, "context.outer.value")
            .ok()?
            .into_pointer_value();
        let _ = self.builder.build_store(saved, outer);
        self.context_saves.push(saved);
        Some(saved)
    }

    fn restore_context(&mut self, saved: Option<PointerValue<'static>>) {
        let Some(saved) = saved else { return };
        self.context_saves.pop();
        if self.dead {
            return;
        }
        let Some(slot) = self.context_slot else {
            return;
        };
        let pointer_type = self.context.ptr_type(AddressSpace::default());
        if self
            .builder
            .get_insert_block()
            .is_some_and(|block| block.get_terminator().is_some())
        {
            return;
        }
        let outer = self
            .builder
            .build_load(pointer_type, saved, "context.outer.value")
            .ok();
        if let Some(outer) = outer {
            let _ = self.builder.build_store(slot, outer);
        }
    }

    /// Leaving a block early through `break` or `continue` unwinds every block
    /// still holding an outer context, innermost first.
    fn unwind_contexts(&mut self) {
        if self.context_saves.is_empty() || self.dead {
            return;
        }
        let Some(slot) = self.context_slot else {
            return;
        };
        let pointer_type = self.context.ptr_type(AddressSpace::default());
        if self
            .builder
            .get_insert_block()
            .is_some_and(|block| block.get_terminator().is_some())
        {
            return;
        }
        for saved in self.context_saves.iter().rev() {
            if let Ok(outer) = self
                .builder
                .build_load(pointer_type, *saved, "context.outer.value")
            {
                let _ = self.builder.build_store(slot, outer);
            }
        }
    }
}

impl Emitter {
    fn stmt(&mut self, module: &Module<'static>, stmt: &Stmt) {
        self.set_location(stmt.span());
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
                        self.unwind_contexts();
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
            Stmt::Match { subject, arms, span } => {
                self.matching(module, subject, arms, None, false, *span);
            }
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
            Some(ty) => match self.type_of(ty) {
                Some(ty) => Some(ty),
                None => {
                    self.unsupported(ty.span(), "this type annotation (primitives, pointers and records today)");
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
                Some(target) => {
                    if self.const_names.contains(name.as_str()) {
                        self.diagnostics.push(
                            Diagnostic::error(
                                code::ASSIGN_TO_CONST,
                                format!("`{name}` is a `const`; it cannot be assigned"),
                            )
                            .with_span(*target_span),
                        );
                        return;
                    }
                    target
                },
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
                match self.place_of(module, base, index, true) {
                    Some(place) => place,
                    None => return,
                }
            }
            Expr::Field { .. } => {
                if let Some(root) = place_root(target) {
                    if self.const_names.contains(root) {
                        self.diagnostics.push(
                            Diagnostic::error(
                                code::ASSIGN_TO_CONST,
                                format!("`{root}` is a `const`; its fields cannot be assigned"),
                            )
                            .with_span(target.span()),
                        );
                        return;
                    }
                }
                match self.place_of_expr(module, target) {
                    Some(place) => place,
                    None => {
                        self.unsupported(
                            target.span(),
                            "assigning to a field of a value (only a field of a place can be stored)",
                        );
                        return;
                    }
                }
            },
            Expr::Unary {
                op: UnaryOp::Load,
                operand,
                ..
            } => match self.place_of_expr(module, target) {
                Some(place) => place,
                None => {
                    self.diagnostics.push(
                        Diagnostic::error(
                            code::TYPE_MISMATCH,
                            "`*` writes through a pointer to one value",
                        )
                        .with_span(operand.span())
                        .with_note("a `rawptr` is written through a `*T`: cast it with `as` first"),
                    );
                    return;
                }
            },
            _ => {
                self.unsupported(
                    target.span(),
                    "assigning to anything but a local, an element or a field",
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
        let mut bound = Vec::new();
        let condition = self.condition_bound(module, cond, &mut bound);
        let then_block = self.new_block("then");
        let else_block = self.new_block("else");
        let merge = self.new_block("endif");
        let _ = self
            .builder
            .build_conditional_branch(condition, then_block, else_block);

        self.builder.position_at_end(then_block);
        self.dead = false;
        self.scopes.push(HashMap::new());
        // The bindings a pattern condition made live for the `then` arm alone,
        // which is where the checker can see them too.
        self.declare_bindings(&bound);
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
        Some(typed_of(phi.as_basic_value(), common))
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
        if name == "free" || name == "del" {
            let Some(value) = self.expr(module, args.first().unwrap_or(&Expr::Error { span }), None)
            else {
                return;
            };
            if value.ty == DuxType::Error {
                return;
            }
            let Some(slot) = self.capture(module, value, "deferred", span) else {
                return;
            };
            self.defers.push(Deferred::Release { slot });
            return;
        }
        if self.skipped.contains(name) {
            return;
        }
        let Some(signature) = self.signatures.get(name).cloned() else {
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
        let hidden_context = self
            .signatures
            .get(name.as_str())
            .is_some_and(|signature| signature.hidden_context);
        self.defers.push(Deferred::Call {
            name: name.clone(),
            slots,
            hidden_context,
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
                hidden_context: bool,
            },
            Release {
                slot: (DuxType, PointerValue<'static>),
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
                Deferred::Call {
                    name,
                    slots,
                    hidden_context,
                } => Owned::Call {
                    name: name.clone(),
                    slots: slots.clone(),
                    hidden_context: *hidden_context,
                },
                Deferred::Release { slot } => Owned::Release { slot: *slot },
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
                Owned::Call {
                    name,
                    slots,
                    hidden_context,
                } => {
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
                    if ready && hidden_context {
                        if let Some(context) = self.context_slot {
                            let value = self
                                .builder
                                .build_load(
                                    self.context.ptr_type(AddressSpace::default()),
                                    context,
                                    "context",
                                )
                                .ok()
                                .map(|value| value.into_pointer_value());
                            match value {
                                Some(value) => arguments.push(value.into()),
                                None => ready = false,
                            }
                        } else {
                            ready = false;
                        }
                    }
                    if ready {
                        let _ = self
                            .builder
                            .build_call(function, &arguments, "deferred")
                            .ok();
                    }
                }
                Owned::Release { slot: (ty, slot) } => {
                    let Some(runtime) = &self.runtime else {
                        continue;
                    };
                    let free = runtime.free;
                    let Some(context) = self.context_slot else {
                        continue;
                    };
                    let pointer_type = self.context.ptr_type(AddressSpace::default());
                    let memory = match self.load(ty, slot) {
                        Some(value) => value
                            .pointer_value()
                            .or_else(|| self.slice_parts(value).map(|(data, _)| data)),
                        None => None,
                    };
                    let Some(memory) = memory else {
                        continue;
                    };
                    let context = self
                        .builder
                        .build_load(pointer_type, context, "context")
                        .ok()
                        .map(|value| value.into_pointer_value());
                    let Some(context) = context else {
                        continue;
                    };
                    let _ = self
                        .builder
                        .build_call(free, &[context.into(), memory.into()], "deferred");
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
        let mut bound = Vec::new();
        let condition = self.condition_bound(module, cond, &mut bound);
        let _ = self
            .builder
            .build_conditional_branch(condition, body_block, exit_block);

        self.builder.position_at_end(body_block);
        self.dead = false;
        self.loops.push(Loop {
            continuation: cond_block,
            exit: exit_block,
        });
        self.block_body_bound(module, body, &bound);
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
    /// `for x in xs` and `for i, x in xs`. An array knows its length and a
    /// slice carries it; past that the two walk the same way.
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
        let (element, base, limit, array_len) = match iter.ty.array_info() {
            Some(info) => {
                let Some(base) = iter.pointer_value() else {
                    self.dead = true;
                    return;
                };
                (
                    info.elem,
                    base,
                    self.context.i64_type().const_int(info.len, false),
                    Some(info),
                )
            }
            None => match self.view_elements(iter) {
                Some((element, data, len)) => (element, data, len, None),
                None => {
                    self.unsupported(
                        iterable.span(),
                        &format!("iterating a `{}`", iter.ty.name()),
                    );
                    self.dead = true;
                    return;
                }
            },
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
            let slot = self.alloca(element, &value.name);
            if let Some(scope) = self.scopes.last_mut() {
                scope.insert(value.name.clone(), (element, slot));
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
                let at = match array_len {
                    Some(info) => {
                        let array_ty = info.elem.llvm(self.context).array_type(info.len as u32);
                        let zero = self.context.i64_type().const_zero();
                        unsafe { self.builder.build_gep(array_ty, base, &[zero, i], "elem") }
                    }
                    None => unsafe { self.builder.build_gep(element.llvm(self.context), base, &[i], "elem") },
                };
                if let Ok(at) = at {
                    if let Some(element) = self.load(element, at) {
                        match (element.ty.array_info(), element.core) {
                            (Some(info), Core::Ptr(source)) => {
                                self.copy_array(slot, source, info)
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
        span: Span,
    ) -> Option<Typed> {
        if matches!(index, Expr::Range { .. }) {
            return self.subslice(module, base, index, span);
        }
        let (elem, at) = self.place_of(module, base, index, false)?;
        self.load(elem, at)
    }

    /// Resolve `base[index]` to the element's address: an element of a fixed
    /// array, or the `n`th target of a `*T`.
    fn place_of(
        &mut self,
        module: &Module<'static>,
        base: &Expr,
        index: &Expr,
        writable: bool,
    ) -> Option<(DuxType, PointerValue<'static>)> {
        let base_span = base.span();
        let base = self.expr(module, base, None)?;
        if base.ty == DuxType::Str {
            if writable {
                self.diagnostics.push(
                    Diagnostic::error(
                        code::TYPE_MISMATCH,
                        "a `string` does not write through its bytes".to_string(),
                    )
                    .with_span(base_span)
                    .with_note("strings are immutable; make a `[]u8` if the bytes are yours"),
                );
                return None;
            }
            let (_, data, _) = self.view_elements(base)?;
            let index_value = self.expr(module, index, None)?;
            if index_value.ty == DuxType::Error {
                return None;
            }
            let index64 = self.index_to_i64(index_value, index.span())?;
            let at = unsafe {
                self.builder
                    .build_gep(DuxType::U8.llvm(self.context), data, &[index64.int_value()?], "byte")
            }
            .ok()?;
            return Some((DuxType::U8, at));
        }
        if let Some(element) = base.ty.slice_element() {
            let (data, _) = self.slice_parts(base)?;
            let index_value = self.expr(module, index, None)?;
            if index_value.ty == DuxType::Error {
                return None;
            }
            let index64 = self.index_to_i64(index_value, index.span())?;
            let at = unsafe {
                self.builder
                    .build_gep(element.llvm(self.context), data, &[index64.int_value()?], "idx")
            }
            .ok()?;
            return Some((element, at));
        }
        let (element, is_array) = match base.ty.array_info() {
            Some(info) => (info.elem, true),
            None => match self.pointee_of(base.ty) {
                Some(target) => (target, false),
                None => {
                    self.unsupported(
                        base_span,
                        &format!(
                            "indexing a `{}` (strings and slices land at M3)",
                            base.ty.name()
                        ),
                    );
                    return None;
                }
            },
        };
        let Core::Ptr(array) = base.core else {
            return None;
        };
        let index_value = self.expr(module, index, None)?;
        if index_value.ty == DuxType::Error {
            return None;
        }
        let index64 = self.index_to_i64(index_value, index.span())?;
        let at = if is_array {
            let info = base.ty.array_info()?;
            let array_ty = info.elem.llvm(self.context).array_type(info.len as u32);
            let zero = self.context.i64_type().const_zero();
            unsafe {
                self.builder
                    .build_gep(array_ty, array, &[zero, index64.int_value()?], "idx")
            }
            .ok()?
        } else {
            unsafe {
                self.builder
                    .build_gep(element.llvm(self.context), array, &[index64.int_value()?], "idx")
            }
            .ok()?
        };
        Some((element, at))
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
        let mut quiet = Vec::new();
        self.condition_bound(module, cond, &mut quiet)
    }

    /// A condition as a `bool`. A pattern condition reports the names its
    /// pattern binds to whoever owns the body they are visible in, because
    /// `if P := e` binds and a plain condition does not.
    fn condition_bound(
        &mut self,
        module: &Module<'static>,
        cond: &Expr,
        bound: &mut Vec<Binding>,
    ) -> IntValue<'static> {
        match cond {
            Expr::CondBinding {
                pattern, value, ..
            } => self.pattern_condition(module, pattern, value, bound),
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

    /// `if P := e`: the value is tried against the pattern, and the answer is
    /// whether it matched. DESIGN 4.4 says a pattern condition is a `match` with
    /// a non-matching fallthrough arm, which is exactly what this builds.
    fn pattern_condition(
        &mut self,
        module: &Module<'static>,
        pattern: &dux_ast::Pattern,
        value: &Expr,
        bound: &mut Vec<Binding>,
    ) -> IntValue<'static> {
        let Some(held) = self.expr(module, value, None) else {
            return self.false_value();
        };
        if held.ty == DuxType::Error {
            return self.false_value();
        }
        let ty = held.ty;
        let place = self.alloca(ty, "pattern");
        if self.put_in(place, held, value.span()).is_none() {
            return self.false_value();
        }
        let yes = self.new_block("pat.true");
        let no = self.new_block("pat.false");
        let done = self.new_block("pat.end");
        self.pattern_holds(module, pattern, ty, place, yes, no, bound);

        self.builder.position_at_end(yes);
        self.br(done);
        self.builder.position_at_end(no);
        self.br(done);
        self.builder.position_at_end(done);
        self.dead = false;
        let flag = self
            .builder
            .build_phi(self.context.bool_type(), "matched")
            .expect("pattern condition");
        let matched = self.context.bool_type().const_all_ones();
        let missed = self.context.bool_type().const_zero();
        flag.add_incoming(&[(&matched, yes), (&missed, no)]);
        flag.as_basic_value().into_int_value()
    }

    fn false_value(&self) -> IntValue<'static> {
        self.context.bool_type().const_zero()
    }

    /// The address of a place expression: a binding, a field of a place, an
    /// array element. `None` for an expression that is a value rather than a
    /// place (a call result, a literal, an arithmetic result).
    fn place_of_expr(
        &mut self,
        module: &Module<'static>,
        expr: &Expr,
    ) -> Option<(DuxType, PointerValue<'static>)> {
        match expr {
            Expr::Paren { inner, .. } => self.place_of_expr(module, inner),
            Expr::Ident { name, .. } => self.slot_of(name),
            Expr::Field { base, name, .. } => {
                let (ty, at) = self.place_of_expr(module, base)?;
                let (info, at) = self.record_address(ty, at)?;
                let (index, field_ty) = self.field_of(info, &name.name, name.span)?;
                Some((field_ty, self.field_address(info, at, index)?))
            }
            Expr::Index { base, index, .. } => self.place_of(module, base, index, false),
            Expr::Unary {
                op: UnaryOp::Load,
                operand,
                ..
            } => {
                let value = self.expr(module, operand, None)?;
                let target = self.pointee_of(value.ty)?;
                Some((target, value.pointer_value()?))
            }
            _ => None,
        }
    }

    /// What `&` makes of an operand: a view of an array or of a range of one,
    /// the view itself when the operand already is one, and otherwise the
    /// address of the place.
    fn borrowed(&mut self, module: &Module<'static>, operand: &Expr, span: Span) -> Option<Typed> {
        let mut inner = operand;
        while let Expr::Paren { inner: next, .. } = inner {
            inner = next;
        }
        if let Expr::Index { base, index, .. } = inner {
            if matches!(&**index, Expr::Range { .. }) {
                return self.subslice(module, base, index, span);
            }
        }
        let (target, at) = self.place_of_expr(module, inner)?;
        if target.array_info().is_some() {
            let copied = self.load(target, at)?;
            return self.view_of_array(copied);
        }
        if target.slice_element().is_some() {
            return self.load(target, at);
        }
        Some(Typed::pointer(at, self.pointer_to(target)))
    }

    /// One bound of a `[a..b]`, evaluated as the position it names.
    fn view_bound(&mut self, module: &Module<'static>, bound: &Expr) -> Option<Typed> {
        let value = self.expr(module, bound, Some(DuxType::I64))?;
        self.index_to_i64(value, bound.span())
    }

    /// An index is an `i64` position, whatever integer the source wrote.
    fn index_to_i64(&mut self, value: Typed, span: Span) -> Option<Typed> {
        if value.ty == DuxType::I64 {
            return Some(value);
        }
        if !value.ty.is_integral() {
            self.diagnostics.push(
                Diagnostic::error(
                    code::TYPE_MISMATCH,
                    format!("an index must be an integer, found `{}`", value.ty.name()),
                )
                .with_span(span),
            );
            return None;
        }
        self.convert(value, DuxType::I64)
    }

    /// The `{ data, len }` shape every view shares.
    fn view_llvm(context: &'static Context) -> StructType<'static> {
        context.struct_type(
            &[
                context.ptr_type(AddressSpace::default()).into(),
                context.i64_type().into(),
            ],
            false,
        )
    }

    /// A view over `element`s: an address and a count.
    fn view(
        &mut self,
        element: DuxType,
        data: PointerValue<'static>,
        len: IntValue<'static>,
    ) -> Typed {
        self.view_as(DuxType::Slice(Box::leak(Box::new(element))), data, len)
    }

    /// Put an address and a count together as `ty`, which had better be one of
    /// the two things shaped like a view.
    fn view_as(
        &mut self,
        ty: DuxType,
        data: PointerValue<'static>,
        len: IntValue<'static>,
    ) -> Typed {
        let mut value = Self::view_llvm(self.context).get_undef();
        if let Ok(inserted) =
            self.builder
                .build_insert_value(value, data.as_basic_value_enum(), 0, "view.data")
        {
            value = inserted.into_struct_value();
        }
        if let Ok(inserted) =
            self.builder
                .build_insert_value(value, len.as_basic_value_enum(), 1, "view.len")
        {
            value = inserted.into_struct_value();
        }
        Typed {
            core: Core::Aggregate(value),
            ty,
        }
    }

    /// What a view steps over: the elements of a slice, the bytes of a string.
    fn view_elements(&mut self, value: Typed) -> Option<(DuxType, PointerValue<'static>, IntValue<'static>)> {
        let element = if value.ty == DuxType::Str {
            DuxType::U8
        } else {
            value.ty.slice_element()?
        };
        let (data, len) = self.slice_parts(value)?;
        Some((element, data, len))
    }

    /// The address and the count a view carries.
    fn slice_parts(&mut self, value: Typed) -> Option<(PointerValue<'static>, IntValue<'static>)> {
        let Core::Aggregate(value) = value.core else {
            if let Some(Core::Ptr(at)) = Some(value.core) {
                // A view that lives in a slot: read it out first.
                let loaded = self
                    .builder
                    .build_load(Self::view_llvm(self.context), at, "view")
                    .ok()?;
                return self.slice_parts(Typed {
                    core: Core::Aggregate(loaded.into_struct_value()),
                    ty: value.ty,
                });
            }
            return None;
        };
        let data = self
            .builder
            .build_extract_value(value, 0, "view.data")
            .ok()?
            .into_pointer_value();
        let len = self
            .builder
            .build_extract_value(value, 1, "view.len")
            .ok()?
            .into_int_value();
        Some((data, len))
    }

    /// The view an array makes of itself: its own storage, its own length.
    fn view_of_array(&mut self, value: Typed) -> Option<Typed> {
        let info = value.ty.array_info()?;
        let data = value.pointer_value()?;
        let len = self.context.i64_type().const_int(info.len, false);
        Some(self.view(info.elem, data, len))
    }

    /// `[a..b]`: the same elements, a shorter count. A missing bound is the
    /// end of the view.
    fn subslice(
        &mut self,
        module: &Module<'static>,
        base: &Expr,
        range: &Expr,
        span: Span,
    ) -> Option<Typed> {
        let value = self.expr(module, base, None)?;
        let is_text = value.ty == DuxType::Str;
        let view = if value.ty.array_info().is_some() {
            self.view_of_array(value)?
        } else if value.ty.slice_element().is_some() || is_text {
            value
        } else {
            self.diagnostics.push(
                Diagnostic::error(
                    code::TYPE_MISMATCH,
                    format!(
                        "there is no subslicing of `{}`",
                        if value.ty == DuxType::Error { "this value".to_string() } else { value.ty.name() }
                    ),
                )
                .with_span(span),
            );
            return None;
        };
        let element = if is_text {
            DuxType::U8
        } else {
            view.ty.slice_element()?
        };
        let (data, len) = self.slice_parts(view)?;
        let Expr::Range { start, end, .. } = range else {
            return None;
        };
        let zero = self.context.i64_type().const_zero();
        let from = match start.as_deref() {
            Some(lo) => self.view_bound(module, lo)?,
            None => Typed { core: Core::Int(zero), ty: DuxType::I64 },
        };
        let to = match end.as_deref() {
            Some(hi) => self.view_bound(module, hi)?,
            None => Typed { core: Core::Int(len), ty: DuxType::I64 },
        };
        let from = from.int_value()?;
        let to = to.int_value()?;
        let data = unsafe {
            self.builder
                .build_gep(element.llvm(self.context), data, &[from], "view.head")
        }
        .ok()?;
        let Ok(count) = self.builder.build_int_sub(to, from, "view.len") else {
            return None;
        };
        if is_text {
            return Some(self.view_as(DuxType::Str, data, count));
        }
        Some(self.view(element, data, count))
    }

    /// The record a place reaches and the address of that record. A place of
    /// record type is the record; a place of `*T` type holds the address, so
    /// `p.field` reads the pointer out before stepping into the target.
    fn record_address(
        &mut self,
        ty: DuxType,
        at: PointerValue<'static>,
    ) -> Option<(&'static StructInfo, PointerValue<'static>)> {
        match ty {
            DuxType::Struct(info) => Some((info, at)),
            DuxType::Pointer(target) => {
                let info = self.struct_of(*target)?;
                let pointer = self
                    .builder
                    .build_load(self.pointer_to(*target).llvm(self.context), at, "deref")
                    .ok()?
                    .into_pointer_value();
                Some((info, pointer))
            }
            _ => None,
        }
    }

    /// `info.field`, with the missing-field diagnostic in one place.
    fn field_of(
        &mut self,
        info: &'static StructInfo,
        name: &str,
        span: Span,
    ) -> Option<(u32, DuxType)> {
        let listed = self.struct_field_list(info).to_vec();
        if let Some((index, (_, ty))) =
            listed.iter().enumerate().find(|(_, (field, _))| *field == name)
        {
            return Some((index as u32, *ty));
        }
        let mut known: Vec<&str> = listed.iter().map(|(field, _)| *field).collect();
        known.sort_unstable();
        self.diagnostics.push(
            Diagnostic::error(
                code::TYPE_MISMATCH,
                format!("`{}` has no field `{name}`", info.name),
            )
            .with_span(span)
            .with_note(format!("the fields are {}", known.join(", "))),
        );
        None
    }

    fn field_address(
        &self,
        info: &'static StructInfo,
        base: PointerValue<'static>,
        index: u32,
    ) -> Option<PointerValue<'static>> {
        let zero = self.context.i32_type().const_zero();
        let at = self.context.i32_type().const_int(index as u64, false);
        unsafe { self.builder.build_gep(info.llvm, base, &[zero, at], "field") }.ok()
    }

    /// `base.name` where `base` is a record. A place reaches its field
    /// directly; a value is stored to a temporary first, so `f().x` works
    /// without every call having to allocate.
    fn field_expr(
        &mut self,
        module: &Module<'static>,
        base: &Expr,
        name: &str,
        name_span: Span,
        span: Span,
    ) -> Option<Typed> {
        if let Some((ty, at)) = self.place_of_expr(module, base) {
            let Some((info, at)) = self.record_address(ty, at) else {
                self.unsupported(
                    span,
                    &format!("reading `.{name}` on a `{}`", ty.name()),
                );
                return None;
            };
            let (index, field_ty) = self.field_of(info, name, name_span)?;
            let at = self.field_address(info, at, index)?;
            return self.load(field_ty, at);
        }
        let value = self.expr(module, base, None)?;
        let Some(info) = self.record_of(value.ty) else {
            self.unsupported(
                span,
                &format!("reading `.{name}` on a `{}`", value.ty.name()),
            );
            return None;
        };
        let (index, field_ty) = self.field_of(info, name, name_span)?;
        if self.pointee_of(value.ty).is_some() {
            let at = self.field_address(info, value.pointer_value()?, index)?;
            return self.load(field_ty, at);
        }
        let slot = self.alloca(value.ty, "record");
        let basic = value.basic()?;
        let _ = self.builder.build_store(slot, basic).ok()?;
        let at = self.field_address(info, slot, index)?;
        self.load(field_ty, at)
    }

    /// A named-field record literal. Fields are written in any order and may
    /// be left out, which leaves them zero-initialised.
    fn struct_literal(
        &mut self,
        module: &Module<'static>,
        name: &str,
        fields: &[(dux_ast::Ident, Expr)],
        span: Span,
    ) -> Option<Typed> {
        let Some(ty) = self.struct_type_of(name) else {
            self.unsupported(
                span,
                &format!("a value of the record type `{name}` (no such `struct` is declared)"),
            );
            return None;
        };
        let info = self.struct_of(ty)?;
        let slot = self.alloca(ty, "record");
        let _ = self.builder.build_store(slot, info.llvm.const_zero());
        for (field, value) in fields {
            let Some((index, field_ty)) = self.field_of(info, &field.name, field.span) else {
                continue;
            };
            let written = self.expr(module, value, Some(field_ty))?;
            if written.ty == DuxType::Error {
                return Some(Typed::error());
            }
            let written = self.coerce_implicit(module, written, field_ty, field.span)?;
            let at = self.field_address(info, slot, index)?;
            match (field_ty.array_info(), written.core) {
                (Some(array), Core::Ptr(source)) => self.copy_array(at, source, array),
                (None, _) => {
                    let Some(basic) = written.basic() else {
                        self.unsupported(field.span, "storing this value in a record field");
                        return None;
                    };
                    let _ = self.builder.build_store(at, basic).ok()?;
                }
                (Some(_), _) => {
                    self.unsupported(field.span, "storing this array value in a record field");
                    return None;
                }
            }
        }
        self.load(ty, slot)
    }

    /// One line of the line table: every subsequent instruction carries the
    /// span's line and column until the next statement claims one.
    fn set_location(&mut self, span: Span) {
        let (Some(dibuilder), Some(scope)) = (&self.dibuilder, self.scope) else {
            return;
        };
        let context = self.context;
        let (line, column) = self.sources.line_col(span.file, span.start);
        let location = dibuilder.create_debug_location(
            context,
            line as u32,
            column as u32,
            scope.as_debug_info_scope(),
            None,
        );
        self.builder.set_current_debug_location(location);
    }

    /// A `DISubprogram` per function. Parameter types are described for the
    /// primitives; a function that touches anything richer gets an empty
    /// signature — full DWARF type DIEs land at M7, the line table does not
    /// depend on them.
    fn begin_function_debug(&mut self, function: &FnDecl, linkage_name: &str, fn_value: FunctionValue<'static>) {
        let Some(unit) = self.compile_unit else {
            return;
        };
        if self.dibuilder.is_none() {
            return;
        }
        let file = self.di_file(function.name.span.file);
        let (line, _) = self.sources.line_col(function.name.span.file, function.name.span.start);
        let mut params = Vec::new();
        let mut exact = true;
        for param in &function.params {
            match DuxType::from_ast(&param.ty).and_then(|ty| self.di_type(ty)) {
                Some(ty) => params.push(ty),
                None => exact = false,
            }
        }
        let return_type: Option<Option<inkwell::debug_info::DIType<'static>>> =
            match function.return_ty.as_ref() {
                None => Some(None),
                Some(ty) => match DuxType::from_ast(ty) {
                    Some(ty) => self.di_type(ty).map(Some),
                    None => None,
                },
            };
        let known = exact && return_type.is_some();
        let Some(dibuilder) = &self.dibuilder else {
            return;
        };
        let signature = dibuilder.create_subroutine_type(
            file,
            if known {
                return_type.unwrap_or(None)
            } else {
                None
            },
            if known { &params } else { &[] },
            DIFlags::ZERO,
        );
        let subprogram = dibuilder.create_function(
            unit.as_debug_info_scope(),
            linkage_name,
            None,
            file,
            line as u32,
            signature,
            true,
            true,
            line as u32,
            DIFlags::ZERO,
            false,
        );
        fn_value.set_subprogram(subprogram);
        self.scope = Some(subprogram);
        self.builder.unset_current_debug_location();
        self.set_location(function.name.span);
    }

    fn di_file(&mut self, file: FileId) -> DIFile<'static> {
        if let Some(known) = self.di_files.get(&file).copied() {
            return known;
        }
        let name = self.sources.name_of(file).to_string();
        let created = match &self.dibuilder {
            Some(dibuilder) => dibuilder.create_file(&name, ""),
            None => return self.di_files.get(&file).copied().expect("checked above"),
        };
        self.di_files.insert(file, created);
        created
    }

    fn di_type(&mut self, ty: DuxType) -> Option<inkwell::debug_info::DIType<'static>> {
        let dibuilder = self.dibuilder.as_ref()?;
        let (name, bits, encoding) = match ty {
            DuxType::Bool => ("bool", 1, DW_ATE_BOOLEAN),
            DuxType::I8 => ("i8", 8, DW_ATE_SIGNED),
            DuxType::I16 => ("i16", 16, DW_ATE_SIGNED),
            DuxType::I32 => ("i32", 32, DW_ATE_SIGNED),
            DuxType::I64 => ("i64", 64, DW_ATE_SIGNED),
            DuxType::U8 => ("u8", 8, DW_ATE_UNSIGNED),
            DuxType::U16 => ("u16", 16, DW_ATE_UNSIGNED),
            DuxType::U32 => ("u32", 32, DW_ATE_UNSIGNED),
            DuxType::U64 => ("u64", 64, DW_ATE_UNSIGNED),
            DuxType::F32 => ("f32", 32, DW_ATE_FLOAT),
            DuxType::F64 => ("f64", 64, DW_ATE_FLOAT),
            _ => return None,
        };
        dibuilder
            .create_basic_type(name, bits, encoding, DIFlags::ZERO)
            .ok()
            .map(|ty| ty.as_type())
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
            Expr::Str { text, kind, span } => Some(self.string_literal(text, *kind, *span)),
            Expr::Ident { name, span } => {
                if let Some((ty, slot)) = self.slot_of(name) {
                    return self.load(ty, slot);
                }
                if let Some(c) = self.consts.get(name).cloned() {
                    return Some(self.const_reference(&c, expected, *span));
                }
                // `None` names a variant as plainly as it names a constant.
                if !self.functions.contains_key(name.as_str()) {
                    if let Some(owner) = self.owner_of_variant(name) {
                        return self.variant_value(module, &owner, name, &[], &[], *span, expected);
                    }
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
            Expr::Call { callee, args, span } => self.call(module, callee, args, *span, expected),
            Expr::Path {
                qualifier, name, span,
            } => {
                if self.shapes.contains_key(qualifier) {
                    self.variant_value(module, qualifier, name, &[], &[], *span, expected)
                } else {
                    self.unsupported(*span, expression_name(expr));
                    None
                }
            }
            Expr::Match { subject, arms, span } => {
                self.matching(module, subject, arms, expected, true, *span)
            }
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
            Expr::Field { base, name, span } => {
                self.field_expr(module, base, &name.name, name.span, *span)
            }
            Expr::StructLiteral { name, fields, span } => {
                if self.structs.contains_key(name.as_str()) || !name.contains("::") {
                    return self.struct_literal(module, name, fields, *span);
                }
                let (owner, variant) = name.split_once("::").unwrap_or((name.as_str(), ""));
                let given: Vec<(&str, &Expr)> = fields
                    .iter()
                    .map(|(field, value)| (field.name.as_str(), value))
                    .collect();
                self.variant_value(module, owner, variant, &[], &given, *span, expected)
            }
            Expr::InterpString { parts, .. } => {
                self.build_text(module, parts, &[], expr.span())
            }
            other => {
                self.unsupported(other.span(), expression_name(other));
                None
            }
        }
    }

    /// Bytes in read-only storage, and how many of them the string sees. The
    /// stored bytes keep their NUL, so the same address serves a C `const
    /// char *`; the length next to it is what makes this a `string`.
    fn string_literal(&mut self, text: &str, kind: dux_ast::StrKind, span: Span) -> Typed {
        let bytes = match self.builder.build_global_string_ptr(text, "str") {
            Ok(global) => global.as_pointer_value(),
            Err(e) => {
                self.diagnostics.push(
                    Diagnostic::error(
                        code::CODEGEN_FAILED,
                        format!("failed to emit the string literal: {e}"),
                    )
                    .with_span(span),
                );
                return Typed::error();
            }
        };
        if matches!(kind, dux_ast::StrKind::CString) {
            return Typed::pointer(bytes, DuxType::Ptr);
        }
        let length = self.context.i64_type().const_int(text.len() as u64, false);
        Typed {
            core: Core::Aggregate(self.context.const_struct(
                &[bytes.into(), length.into()],
                false,
            )),
            ty: DuxType::Str,
        }
    }

    #[allow(dead_code)]
    fn unused_string_literal(&mut self, text: &str, span: Span) -> Typed {
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
            UnaryOp::AddressOf => match self.borrowed(module, operand, span) {
                Some(view) => Some(view),
                None => {
                    self.diagnostics.push(
                        Diagnostic::error(
                            code::TYPE_MISMATCH,
                            "`&` takes the address of a binding, a field or an element",
                        )
                        .with_span(operand.span())
                        .with_note("a temporary has no address to keep: bind it first"),
                    );
                    None
                }
            },
            UnaryOp::Load => {
                let value = self.expr(module, operand, None)?;
                match self.pointee_of(value.ty) {
                    Some(target) => self.load(target, value.pointer_value()?),
                    None => {
                        let note = if value.ty == DuxType::Ptr {
                            "`rawptr` carries no type to read through: write `as *i32` first"
                        } else {
                            "`*` reads through a `*T`"
                        };
                        self.diagnostics.push(
                            Diagnostic::error(
                                code::TYPE_MISMATCH,
                                format!("`*` needs a pointer, found `{}`", value.ty.name()),
                            )
                            .with_span(span)
                            .with_note(note),
                        );
                        None
                    }
                }
            }
        }
    }

    /// `+` joins two strings into a new one from the context's allocator; the
    /// comparisons read the bytes and then the lengths, which is what `==` has
    /// always meant for a string.
    fn text_binary(
        &mut self,
        module: &Module<'static>,
        op: BinaryOp,
        left: Typed,
        right: Typed,
        span: Span,
    ) -> Option<Typed> {
        if left.ty != DuxType::Str || right.ty != DuxType::Str {
            self.unsupported(
                span,
                "mixing a string with something that is not one (there is no implicit text)",
            );
            return None;
        }
        let (head, head_len) = self.slice_parts(left)?;
        let (tail, tail_len) = self.slice_parts(right)?;
        if op != BinaryOp::Add {
            let same = self.same_text(module, head, head_len, tail, tail_len)?;
            let holds = match op {
                BinaryOp::Eq => same,
                BinaryOp::NotEq => self.builder.build_not(same, "text.ne").ok()?,
                _ => {
                    // The checker sends ordering to `strings.less`, so only
                    // equality reaches here.
                    self.unsupported(span, "this operator on strings");
                    return None;
                }
            };
            return Some(Typed::int(holds, DuxType::Bool));
        }
        let total = self
            .builder
            .build_int_add(head_len, tail_len, "text.len")
            .ok()?;
        let data = self.allocate(span, total, 1)?;
        self.builder
            .build_memcpy(data, 1, head, 1, head_len)
            .ok()?;
        let joined = unsafe {
            self.builder
                .build_gep(DuxType::U8.llvm(self.context), data, &[head_len], "text.tail")
        }
        .ok()?;
        self.builder
            .build_memcpy(joined, 1, tail, 1, tail_len)
            .ok()?;
        Some(self.view_as(DuxType::Str, data, total))
    }

    /// Equal when the common prefix matches and the lengths agree.
    fn same_text(
        &mut self,
        module: &Module<'static>,
        left: PointerValue<'static>,
        left_len: IntValue<'static>,
        right: PointerValue<'static>,
        right_len: IntValue<'static>,
    ) -> Option<IntValue<'static>> {
        let i64_type = self.context.i64_type();
        let prefix = self
            .builder
            .build_select(
                self.builder
                    .build_int_compare(IntPredicate::ULT, left_len, right_len, "text.short")
                    .ok()?,
                left_len,
                right_len,
                "text.prefix",
            )
            .ok()?
            .into_int_value();
        let memcmp = self.declare_c(
            module,
            "memcmp",
            self.context.i32_type().fn_type(
                &[
                    self.context.ptr_type(AddressSpace::default()).into(),
                    self.context.ptr_type(AddressSpace::default()).into(),
                    i64_type.into(),
                ],
                false,
            ),
        );
        let bytes = self
            .builder
            .build_call(memcmp, &[left.into(), right.into(), prefix.into()], "text.cmp")
            .ok()?
            .try_as_basic_value()
            .basic()
            .map(|value| value.into_int_value())?;
        let lengths_agree = self
            .builder
            .build_int_compare(IntPredicate::EQ, left_len, right_len, "text.same")
            .ok()?;
        let prefix_matches = self
            .builder
            .build_int_compare(
                IntPredicate::EQ,
                bytes,
                self.context.i32_type().const_zero(),
                "text.diff",
            )
            .ok()?;
        self.builder
            .build_and(lengths_agree, prefix_matches, "text.eq")
            .ok()
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
        let (left, right) = self.operands(module, lhs, rhs)?;
        if left.ty == DuxType::Error || right.ty == DuxType::Error {
            return Some(Typed::error());
        }
        if left.ty.is_pointer_like() || right.ty.is_pointer_like() {
            return self.pointer_binary(op, left, right, span);
        }
        if left.ty == DuxType::Str || right.ty == DuxType::Str {
            return self.text_binary(module, op, left, right, span);
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

    /// Lower both operands of a binary operator. A float literal beside an
    /// `f32` is an `f32` (D17: an untyped literal takes the type the program
    /// asked for), so the pair is lowered with the concrete side first.
    fn operands(
        &mut self,
        module: &Module<'static>,
        lhs: &Expr,
        rhs: &Expr,
    ) -> Option<(Typed, Typed)> {
        if matches!(lhs, Expr::Float { .. }) && !matches!(rhs, Expr::Float { .. }) {
            let right = self.expr(module, rhs, None)?;
            let hint = right.ty.is_float().then_some(right.ty);
            let left = self.expr(module, lhs, hint)?;
            return Some((left, right));
        }
        if matches!(rhs, Expr::Float { .. }) && !matches!(lhs, Expr::Float { .. }) {
            let left = self.expr(module, lhs, None)?;
            let hint = left.ty.is_float().then_some(left.ty);
            let right = self.expr(module, rhs, hint)?;
            return Some((left, right));
        }
        let left = self.expr(module, lhs, None)?;
        // A sum type beside an operator says what the other side holds, which
        // is the only way `pick == None` can know which `Option` `None` is.
        let hint = matches!(left.ty, DuxType::Enum(_)).then_some(left.ty);
        let right = self.expr(module, rhs, hint)?;
        Some((left, right))
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

    /// `p + n`, `p - n` and pointer comparisons. Arithmetic is scaled by the
    /// target, so `base + 2` is two whole values away. A `rawptr` has no
    /// target to scale by and refuses until it is cast.
    fn pointer_binary(
        &mut self,
        op: BinaryOp,
        left: Typed,
        right: Typed,
        span: Span,
    ) -> Option<Typed> {
        if op.is_comparison() && left.ty.is_pointer_like() && right.ty.is_pointer_like() {
            if left.ty != right.ty && left.ty != DuxType::Ptr && right.ty != DuxType::Ptr {
                self.diagnostics.push(
                    Diagnostic::error(
                        code::TYPE_MISMATCH,
                        format!(
                            "`{}` compares pointers to different things: `{}` and `{}`",
                            op.symbol(),
                            left.ty.name(),
                            right.ty.name()
                        ),
                    )
                    .with_span(span),
                );
                return None;
            }
            let predicate = match op {
                BinaryOp::Eq => IntPredicate::EQ,
                BinaryOp::NotEq => IntPredicate::NE,
                BinaryOp::Lt => IntPredicate::ULT,
                BinaryOp::Gt => IntPredicate::UGT,
                BinaryOp::Le => IntPredicate::ULE,
                _ => IntPredicate::UGE,
            };
            let left_address = self.pointer_as_address(left)?;
            let right_address = self.pointer_as_address(right)?;
            let result = self
                .builder
                .build_int_compare(predicate, left_address, right_address, "cmptmp")
                .ok()?;
            return Some(Typed::int(result, DuxType::Bool));
        }
        let (pointer, count, negative) = match (left.ty.is_pointer_like(), right.ty.is_pointer_like()) {
            (true, false) if op == BinaryOp::Add => (left, right, false),
            (true, false) if op == BinaryOp::Sub => (left, right, true),
            (false, true) if op == BinaryOp::Add => (right, left, false),
            _ => {
                self.diagnostics.push(
                    Diagnostic::error(
                        code::TYPE_MISMATCH,
                        format!(
                            "there is no `{}` between `{}` and `{}`",
                            op.symbol(),
                            left.ty.name(),
                            right.ty.name()
                        ),
                    )
                    .with_span(span)
                    .with_note("a pointer steps with `+`/`-` by a whole number of targets"),
                );
                return None;
            }
        };
        self.step(pointer, count, negative, span)
    }

    fn pointer_as_address(&mut self, value: Typed) -> Option<IntValue<'static>> {
        self.convert(value, DuxType::U64)?.int_value()
    }

    /// `pointer` moved by `count` targets (`count` is an integer value).
    fn step(
        &mut self,
        pointer: Typed,
        count: Typed,
        negative: bool,
        span: Span,
    ) -> Option<Typed> {
        let Some(target) = self.pointee_of(pointer.ty) else {
            self.diagnostics.push(
                Diagnostic::error(
                    code::TYPE_MISMATCH,
                    format!(
                        "a `{}` takes no arithmetic: cast it `as *T` first",
                        pointer.ty.name()
                    ),
                )
                .with_span(span),
            );
            return None;
        };
        if !count.ty.is_integral() || count.ty == DuxType::Bool {
            self.diagnostics.push(
                Diagnostic::error(
                    code::TYPE_MISMATCH,
                    format!("pointer arithmetic steps by an integer, not a `{}`", count.ty.name()),
                )
                .with_span(span),
            );
            return None;
        }
        let mut steps = self.convert(count, DuxType::I64)?;
        if negative {
            steps = self.unary_negate(steps)?;
        }
        let at = unsafe {
            self.builder.build_gep(
                target.llvm(self.context),
                pointer.pointer_value()?,
                &[steps.int_value()?],
                "next",
            )
        }
        .ok()?;
        Some(Typed::pointer(at, pointer.ty))
    }

    fn unary_negate(&mut self, value: Typed) -> Option<Typed> {
        let negated = self
            .builder
            .build_int_neg(value.int_value()?, "negtmp")
            .ok()?;
        Some(Typed::int(negated, value.ty))
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

    /// The one conversion that runs code: measuring a C string. A `string` is
    /// bytes plus a count, and a NUL-terminated address only has the first, so
    /// the count has to be found before the view can exist.
    fn bytes_to_text(
        &mut self,
        module: &Module<'static>,
        value: Typed,
        to: DuxType,
    ) -> Option<Typed> {
        if value.ty != DuxType::Ptr || to != DuxType::Str {
            return None;
        }
        let data = value.pointer_value()?;
        let strlen = self.declare_c(
            module,
            "strlen",
            self.context.i64_type().fn_type(
                &[self.context.ptr_type(AddressSpace::default()).into()],
                false,
            ),
        );
        let length = self
            .builder
            .build_call(strlen, &[data.into()], "strlen")
            .ok()?
            .try_as_basic_value()
            .basic()
            .map(|value| value.into_int_value())?;
        Some(self.view_as(DuxType::Str, data, length))
    }

    fn cast(&mut self, module: &Module<'static>, target: &Expr, ty: &Type) -> Option<Typed> {
        let value = self.expr(module, target, None)?;
        let Some(to) = self.type_of(ty) else {
            self.unsupported(ty.span(), "casting to this type");
            return None;
        };
        if value.ty == DuxType::Error {
            return Some(Typed {
                core: value.core,
                ty: DuxType::Error,
            });
        }
        if let Some(text) = self.bytes_to_text(module, value, to) {
            return Some(text);
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
        if let Some(info) = value.ty.array_info() {
            // An array is a slice that knows its own length: passing one where
            // a view is expected borrows its storage and states its count.
            if to.slice_element() == Some(info.elem) {
                if let Some(view) = self.view_of_array(value) {
                    return Some(view);
                }
            }
        }
        if self.lossless(value.ty, to) {
            if let Some(text) = self.bytes_to_text(_module, value, to) {
                return Some(text);
            }
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
        if let (Some(info), Some(want)) = (from.array_info(), to.slice_element()) {
            return info.elem == want;
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
        if from == DuxType::Str && to == DuxType::Ptr {
            // The bytes of a string are a valid C string only when they hold
            // no NUL; `as` hands the address over and trusts the program.
            let (data, _) = self.slice_parts(value)?;
            return Some(Typed::pointer(data, to));
        }

        if from.is_pointer_like() && to.is_pointer_like() {
            return Some(Typed {
                core: value.core,
                ty: to,
            });
        }
        if from.is_pointer_like() && to.is_integral() {
            let cast = self
                .builder
                .build_ptr_to_int(value.pointer_value()?, to.int_type(self.context), "ptoi")
                .ok()?;
            return Some(Typed::int(cast, to));
        }
        if from.is_integral() && to.is_pointer_like() {
            let cast = self
                .builder
                .build_int_to_ptr(
                    value.int_value()?,
                    self.context.ptr_type(AddressSpace::default()),
                    "itop",
                )
                .ok()?;
            return Some(Typed::pointer(cast, to));
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
        expected: Option<DuxType>,
    ) -> Option<Typed> {
        match callee {
            Expr::Ident { name, .. } => self.call_named(module, name, args, callee, span, expected),
            Expr::Paren { inner, .. } => self.call(module, inner, args, span, expected),
            Expr::Path { qualifier, name, .. } if self.shapes.contains_key(qualifier) => {
                self.variant_value(module, qualifier, name, args, &[], span, expected)
            }
            Expr::Field { base, name, .. } => {
                self.call_method(module, base, name, args, span)
            }
            _ => {
                self.unsupported(span, "calling anything but a named function or a method");
                None
            }
        }
    }

    fn call_named(
        &mut self,
        module: &Module<'static>,
        name: &str,
        args: &[Expr],
        callee: &Expr,
        span: Span,
        expected: Option<DuxType>,
    ) -> Option<Typed> {
        if name == "print" || name == "println" {
            return self.write_builtin(module, name, args, span);
        }
        // The allocation builtins belong to the prelude, so a program that
        // declares its own `make` wins.
        if matches!(name, "new" | "make" | "free" | "del") && !self.functions.contains_key(name) {
            return self.memory_builtin(module, name, args, span);
        }
        if self.skipped.contains(name) {
            return Some(Typed::error());
        }
        // `Some(3)` and `Err(e)` are spelled without their enum, which is how
        // the predeclared sum types are meant to read. A program's own function
        // of that name wins, as it does for `make`.
        if !self.functions.contains_key(name) {
            if let Some(owner) = self.owner_of_variant(name) {
                return self.variant_value(module, &owner, name, args, &[], span, expected);
            }
        }
        let Some(function) = self.functions.get(name).copied() else {
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
        let (params, return_ty, hidden_context) = {
            let signature = self
                .signatures
                .get(name)
                .expect("a callable function has a recorded signature");
            (
                signature.params.clone(),
                signature.return_ty,
                signature.hidden_context,
            )
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
        if hidden_context {
            let context = self.context_value(span)?;
            arguments.push(context.into());
        }
        self.invoke(function, arguments, return_ty)
    }

    /// Where the receiver of a method call comes from: a place, which a
    /// pointer receiver can borrow directly, or a value, which has no address.
    fn call_method(
        &mut self,
        module: &Module<'static>,
        base: &Expr,
        name: &dux_ast::Ident,
        args: &[Expr],
        span: Span,
    ) -> Option<Typed> {
        let place = self.place_of_expr(module, base);
        let (base_ty, base_at) = match place {
            Some((ty, at)) => (ty, Some(at)),
            None => {
                let value = self.expr(module, base, None)?;
                (value.ty, value.pointer_value())
            }
        };
        if name.name == "len" && args.is_empty() {
            if let Some(len) = self.length_of(module, base_ty, base_at, base.span()) {
                return Some(len);
            }
        }
        let Some(record) = self.record_of(base_ty) else {
            self.diagnostics.push(
                Diagnostic::error(
                    code::UNDECLARED,
                    format!(
                        "there are no methods on a `{}`",
                        if base_ty == DuxType::Error {
                            "value of an unknown type".to_string()
                        } else {
                            base_ty.name()
                        }
                    ),
                )
                .with_span(base.span()),
            );
            return None;
        };
        let key = format!("{}.{}", record.name, name.name);
        if self.skipped.contains(&key) {
            return Some(Typed::error());
        }
        let Some(function) = self.functions.get(&key).copied() else {
            self.diagnostics.push(
                Diagnostic::error(
                    code::UNDECLARED,
                    format!("`{}` has no method `{}`", record.name, name.name),
                )
                .with_span(name.span)
                .with_note(format!("methods are declared in an `impl {}` block", record.name)),
            );
            return None;
        };
        let signature = self
            .signatures
            .get(&key)
            .expect("a callable method has a recorded signature");
        let params = signature.params.clone();
        let return_ty = signature.return_ty;
        let hidden_context = signature.hidden_context;
        let want = signature.receiver.unwrap_or(DuxType::Struct(record));
        let receiver = self.receiver(module, base_ty, base_at, want, name, base.span())?;
        if args.len() != params.len() {
            self.diagnostics.push(
                Diagnostic::error(
                    code::WRONG_ARITY,
                    format!(
                        "`{}.{}` takes {} arguments, given {}",
                        record.name,
                        name.name,
                        params.len(),
                        args.len()
                    ),
                )
                .with_span(span),
            );
            return None;
        }
        let mut arguments = match receiver.basic() {
            Some(basic) => vec![BasicMetadataValueEnum::from(basic)],
            None => {
                self.unsupported(base.span(), "passing this receiver");
                return None;
            }
        };
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
        if hidden_context {
            let context = self.context_value(span)?;
            arguments.push(context.into());
        }
        self.invoke(function, arguments, return_ty)
    }

    /// `.len()`: an array knows its length, a view carries it.
    fn length_of(
        &mut self,
        module: &Module<'static>,
        base_ty: DuxType,
        base_at: Option<PointerValue<'static>>,
        span: Span,
    ) -> Option<Typed> {
        let _ = module;
        if let Some(info) = base_ty.array_info() {
            let len = self.context.i64_type().const_int(info.len, false);
            return Some(Typed {
                core: Core::Int(len),
                ty: DuxType::U64,
            });
        }
        if base_ty != DuxType::Str {
            base_ty.slice_element()?;
        }
        let at = base_at?;
        let view = self.load(base_ty, at)?;
        let (_, len) = self.slice_parts(view)?;
        let _ = span;
        Some(Typed {
            core: Core::Int(len),
            ty: DuxType::U64,
        })
    }

    /// The receiver argument, from the place (or value) the method was called
    /// on: a pointer receiver borrows the place, a value receiver copies it.
    fn receiver(
        &mut self,
        module: &Module<'static>,
        base_ty: DuxType,
        base_at: Option<PointerValue<'static>>,
        want: DuxType,
        name: &dux_ast::Ident,
        span: Span,
    ) -> Option<Typed> {
        let _ = module;
        let refusal = |this: &mut Self, why: &str| {
            this.diagnostics.push(
                Diagnostic::error(
                    code::TYPE_MISMATCH,
                    format!(
                        "`{}` cannot take `{}` this way: {why}",
                        if base_ty == DuxType::Error {
                            "this value".to_string()
                        } else {
                            base_ty.name()
                        },
                        name.name
                    ),
                )
                .with_span(span),
            );
            None
        };
        let Some(at) = base_at else {
            return refusal(self, "the receiver has no address to pass");
        };
        match want {
            DuxType::Pointer(target) => {
                if base_ty == *target {
                    Some(Typed::pointer(at, want))
                } else if base_ty == want {
                    self.load(want, at)
                } else {
                    refusal(self, "it is neither the record nor a pointer to it")
                }
            }
            want => {
                if base_ty == want {
                    self.load(want, at)
                } else if self.pointee_of(base_ty) == Some(want) {
                    let pointer = self.load(base_ty, at)?.pointer_value()?;
                    self.load(want, pointer)
                } else {
                    refusal(self, "it is not a copy of the receiver's record")
                }
            }
        }
    }

    fn invoke(
        &mut self,
        function: FunctionValue<'static>,
        arguments: Vec<BasicMetadataValueEnum<'static>>,
        return_ty: Option<DuxType>,
    ) -> Option<Typed> {
        let call = self.builder.build_call(function, &arguments, "call").ok()?;
        let core = match call.try_as_basic_value() {
            ValueKind::Basic(BasicValueEnum::IntValue(value)) => Core::Int(value),
            ValueKind::Basic(BasicValueEnum::FloatValue(value)) => Core::Float(value),
            ValueKind::Basic(BasicValueEnum::PointerValue(value)) => Core::Ptr(value),
            ValueKind::Basic(BasicValueEnum::StructValue(value)) => Core::Aggregate(value),
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
                let empty = self.string_literal("", dux_ast::StrKind::Utf8, span);
                return self
                    .write_text(module, empty, true)
                    .map(|_| Typed::void());
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
                let literal = self.string_literal(text, dux_ast::StrKind::Utf8, *arg_span);
                self.write_text(module, literal, name == "println")
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
                    self.diagnostics.push(
                        Diagnostic::error(
                            code::TYPE_MISMATCH,
                            format!(
                                "`{name}` writes text, and a `{}` is not text",
                                value.ty.name()
                            ),
                        )
                        .with_span(other.span())
                        .with_note("write `$\"{}\"` to format a value as text"),
                    );
                    return None;
                }
                self.write_text(module, value, name == "println")
                    .map(|_| Typed::void())
            }
        }
    }

    /// A string view writes the bytes it sees. A NUL terminator is not
    /// involved: it is the length that says where the string ends, and the
    /// bytes may contain anything.
    fn write_text(&mut self, module: &Module<'static>, value: Typed, newline: bool) -> Option<()> {
        let (data, len) = self.slice_parts(value)?;
        let stream = self.stdout_stream(module)?;
        let i64_type = self.context.i64_type();
        let fwrite = self.fwrite_fn(module);
        let _ = self
            .builder
            .build_call(
                fwrite,
                &[
                    data.into(),
                    i64_type.const_int(1, false).into(),
                    len.into(),
                    stream.into(),
                ],
                "write",
            )
            .ok()?;
        if newline {
            let fputc = self.fputc_fn(module);
            let newline = self.context.i32_type().const_int(10, false);
            let _ = self
                .builder
                .build_call(fputc, &[newline.into(), stream.into()], "newline")
                .ok()?;
        }
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
    /// The format string an interpolated string means, and the arguments its
    /// holes contribute. Text keeps its shape, with `%` escaped.
    fn text_format(
        &mut self,
        parts: &[InterpolationPart],
        values: &[Typed],
        span: Span,
    ) -> Option<(String, Vec<BasicMetadataValueEnum<'static>>)> {
        let mut format = String::new();
        let mut prepared: Vec<Typed> = Vec::new();
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
                    self.format_hole(*value, &mut format, &mut prepared, span)?;
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
        let mut arguments: Vec<BasicMetadataValueEnum> = Vec::new();
        for value in prepared {
            arguments.extend(self.prepare_for_format(value, span)?);
        }
        Some((format, arguments))
    }

    /// One hole of a `$".."` string: the piece of format string it stands for
    /// and the values that fill it. A record says itself field by field — the
    /// compiler knows its shape — and nests the same way the value nests.
    fn format_hole(
        &mut self,
        value: Typed,
        format: &mut String,
        prepared: &mut Vec<Typed>,
        span: Span,
    ) -> Option<()> {
        let Some(info) = self.struct_of(value.ty) else {
            format.push('%');
            let spec = spec_for(value.ty).or_else(|| {
                self.unsupported(span, "formatting a value in a $\"..\" hole");
                None
            })?;
            format.push_str(spec);
            prepared.push(value);
            return Some(());
        };
        let listed = self.struct_field_list(info).to_vec();
        let parts = self.record_parts(value)?;
        if listed.len() != parts.len() {
            self.unsupported(span, "formatting a record whose fields are not all visible");
            return None;
        }
        format.push_str("{ ");
        for (index, (name, _)) in listed.iter().enumerate() {
            if index > 0 {
                format.push_str(", ");
            }
            format.push_str(name);
            format.push_str(": ");
            self.format_hole(parts[index], format, prepared, span)?;
        }
        format.push_str(" }");
        Some(())
    }

    /// A record's fields, read one at a time out of a slot of its own.
    fn record_parts(&mut self, value: Typed) -> Option<Vec<Typed>> {
        let info = self.struct_of(value.ty)?;
        let held = value.basic()?;
        let slot = self.alloca(value.ty, "shown");
        let _ = self.builder.build_store(slot, held).ok()?;
        let listed = self.struct_field_list(info).to_vec();
        let mut parts = Vec::new();
        for (index, (_, ty)) in listed.iter().enumerate() {
            let at = self.field_address(info, slot, index as u32)?;
            parts.push(self.load(*ty, at)?);
        }
        Some(parts)
    }

    fn write_prepared(
        &mut self,
        module: &Module<'static>,
        parts: &[InterpolationPart],
        values: &[Typed],
        newline: bool,
        span: Span,
    ) -> Option<()> {
        let (mut format, mut arguments) = self.text_format(parts, values, span)?;
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
        arguments.insert(0, format_ptr.into());
        let printf = self.printf_fn(module);
        let _ = self
            .builder
            .build_call(printf, &arguments, "printf")
            .ok()?;
        Some(())
    }

    /// `$".."` where a value is expected: formatted into a block the context
    /// owns. The count comes from a measurement pass, which is what the C
    /// library does when handed a zero-length buffer.
    fn build_text(
        &mut self,
        module: &Module<'static>,
        parts: &[InterpolationPart],
        positional: &[Expr],
        span: Span,
    ) -> Option<Typed> {
        let values = self.lower_holes(module, parts, positional, span)?;
        let (format, arguments) = self.text_format(parts, &values, span)?;
        let format_ptr = match self.builder.build_global_string_ptr(&format, "text") {
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
        let snprintf = self.snprintf_fn(module);
        let null = self.context.ptr_type(AddressSpace::default()).const_null();
        let zero = self.context.i64_type().const_zero();
        let mut measuring: Vec<BasicMetadataValueEnum> =
            vec![null.into(), zero.into(), format_ptr.into()];
        measuring.extend(arguments.iter().copied());
        let needed = self
            .builder
            .build_call(snprintf, &measuring, "measure")
            .ok()?
            .try_as_basic_value()
            .basic()
            .map(|value| value.into_int_value())?;
        let i64_type = self.context.i64_type();
        let length = self
            .builder
            .build_int_cast_sign_flag(needed, i64_type, true, "text.length")
            .ok()?;
        let room = self
            .builder
            .build_int_add(length, i64_type.const_int(1, false), "text.room")
            .ok()?;
        let data = self.allocate(span, room, 1)?;
        let mut writing: Vec<BasicMetadataValueEnum> =
            vec![data.into(), room.into(), format_ptr.into()];
        writing.extend(arguments.iter().copied());
        let _ = self
            .builder
            .build_call(snprintf, &writing, "format")
            .ok()?;
        Some(self.view_as(DuxType::Str, data, length))
    }

    /// The arguments one hole contributes to the format string. A string view
    /// is two of them: `%.*s` wants the count and then the bytes.
    fn prepare_for_format(
        &mut self,
        value: Typed,
        span: Span,
    ) -> Option<Vec<BasicMetadataValueEnum<'static>>> {
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
                Some(vec![selected.into_pointer_value().into()])
            }
            DuxType::Str => {
                let (data, len) = self.slice_parts(value)?;
                let count = self
                    .builder
                    .build_int_cast(len, self.context.i32_type(), "precision")
                    .ok()?;
                Some(vec![count.into(), data.into()])
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
                Some(vec![widened.into()])
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
                Some(vec![widened.into()])
            }
            DuxType::F32 => {
                let widened = self
                    .builder
                    .build_float_ext(value.float_value()?, self.context.f64_type(), "promote")
                    .ok()?;
                Some(vec![widened.into()])
            }
            DuxType::Array(..) => {
                self.unsupported(
                    span,
                    "printing an array (array formatting lands at M3)",
                );
                None
            }
            DuxType::Struct(info) => {
                self.unsupported(
                    span,
                    &format!(
                        "printing a `{}` whole (print its fields, or give the record a `to_string`)",
                        info.name
                    ),
                );
                None
            }
            _ => value.basic().map(|one| vec![one.into()]),
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
            BasicValueEnum::StructValue(value) => Core::Aggregate(value),
            _ => return None,
        };
        Some(Typed { core, ty })
    }

    fn alloca(&mut self, ty: DuxType, name: &str) -> PointerValue<'static> {
        self.alloca_in_entry(ty.llvm(self.context), name)
    }

    /// An alloca in the entry block, whatever block the builder is in. The
    /// generated code keeps every slot in the entry block so a repeated loop
    /// iteration never stacks allocations on the stack.
    fn alloca_in_entry(
        &mut self,
        ty: BasicTypeEnum<'static>,
        name: &str,
    ) -> PointerValue<'static> {
        let Some(entry) = self.allocas else {
            return self
                .builder
                .build_alloca(ty, name)
                .expect("alloca outside a function");
        };
        let current = self.builder.get_insert_block();
        match entry.get_terminator() {
            Some(terminator) => self.builder.position_before(&terminator),
            None => self.builder.position_at_end(entry),
        }
        let slot = self
            .builder
            .build_alloca(ty, name)
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
        let fn_value = declared(module, "printf", printf);
        self.printf = Some(fn_value);
        fn_value
    }

    fn snprintf_fn(&mut self, module: &Module<'static>) -> FunctionValue<'static> {
        let c_char = self.context.ptr_type(AddressSpace::default());
        let size = self.context.i64_type();
        let fn_type =
            self.context
                .i32_type()
                .fn_type(&[c_char.into(), size.into(), c_char.into()], true);
        self.declare_c(module, "snprintf", fn_type)
    }

    fn fwrite_fn(&mut self, module: &Module<'static>) -> FunctionValue<'static> {
        let c_char = self.context.ptr_type(AddressSpace::default());
        let size = self.context.i64_type();
        let fn_type = size.fn_type(&[c_char.into(), size.into(), size.into(), c_char.into()], false);
        declared(module, "fwrite", fn_type)
    }

    fn fputc_fn(&mut self, module: &Module<'static>) -> FunctionValue<'static> {
        let c_char = self.context.ptr_type(AddressSpace::default());
        let fn_type = self
            .context
            .i32_type()
            .fn_type(&[self.context.i32_type().into(), c_char.into()], false);
        declared(module, "fputc", fn_type)
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
    fn declare_const(&mut self, module: &Module<'static>, decl: &ConstDecl) {
        let name = decl.name.name.clone();
        let Some(ty) = decl
            .ty
            .as_ref()
            .and_then(|ty| self.type_of(ty))
            .filter(|ty| ty.array_info().is_none())
        else {
            self.unsupported(
                decl.span,
                "a `const` whose type is not a primitive or a record yet",
            );
            return;
        };
        let Some(value) = self.const_expr(&decl.value) else {
            return;
        };
        let Some(value) = value.checked(ty, decl.span, self) else {
            return;
        };
        if self.struct_of(ty).is_none() {
            self.consts.insert(name, Const { ty, value });
            return;
        }
        let initializer = self.const_initializer(&value, ty);
        let global = self.module_global(module, ty, &name);
        global.set_initializer(&initializer);
        global.set_constant(true);
        self.globals.insert(name.clone(), (ty, global));
        self.const_names.insert(name);
    }

    fn module_global(
        &mut self,
        module: &Module<'static>,
        ty: DuxType,
        name: &str,
    ) -> GlobalValue<'static> {
        if let Some(existing) = module.get_global(name) {
            return existing;
        }
        module.add_global(ty.llvm(self.context), None, name)
    }

    fn declare_global(&mut self, module: &Module<'static>, decl: &GlobalDecl) {
        let name = decl.name.name.clone();
        let Some(ty) = decl
            .ty
            .as_ref()
            .and_then(|ty| self.type_of(ty))
            .filter(|ty| ty.array_info().is_none())
        else {
            self.unsupported(
                decl.span,
                "a `global` whose type is not a primitive or a record yet",
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
            ConstValue::Struct(values) => {
                let Some(info) = self.struct_of(ty) else {
                    return ty.llvm(self.context).const_zero();
                };
                let mut fields = Vec::new();
                let listed = self.struct_field_list(info).to_vec();
                for (field, field_ty) in &listed {
                    let field_value = values
                        .iter()
                        .find(|(written, _)| written == field)
                        .map(|(_, value)| value.clone());
                    fields.push(match field_value {
                        Some(field_value) => self.const_initializer(&field_value, *field_ty),
                        None => field_ty.llvm(self.context).const_zero(),
                    });
                }
                info.llvm.const_named_struct(&fields).into()
            }
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
            Expr::StructLiteral { fields, .. } => {
                let mut values = Vec::new();
                for (field, value) in fields {
                    values.push((
                        Box::leak(field.name.clone().into_boxed_str()) as &str,
                        self.const_expr(value)?,
                    ));
                }
                Some(ConstValue::Struct(values))
            }
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
            ConstValue::Str(text) => self.string_literal(text, dux_ast::StrKind::Utf8, span),
            ConstValue::Struct(_) => {
                self.diagnostics.push(
                    Diagnostic::error(
                        code::CODEGEN_FAILED,
                        "a record constant lost its storage",
                    )
                    .with_span(span),
                );
                Typed::error()
            }
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

/// The name a place expression is rooted in: `unit.min.x` is rooted in
/// `unit`. Used to refuse an assignment through a `const` record.
fn place_root(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::Ident { name, .. } => Some(name.as_str()),
        Expr::Paren { inner, .. } => place_root(inner),
        Expr::Field { base, .. } | Expr::Index { base, .. } => place_root(base),
        _ => None,
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
    if ty.slice_element().is_some() {
        return None;
    }
    Some(match ty {
        DuxType::Bool => "s",
        DuxType::I8 | DuxType::I16 | DuxType::I32 => "d",
        DuxType::U8 | DuxType::U16 | DuxType::U32 => "u",
        DuxType::I64 => "ld",
        DuxType::U64 => "lu",
        DuxType::F32 | DuxType::F64 => "g",
        DuxType::Str => ".*s",
        DuxType::Ptr | DuxType::Pointer(_) => "p",
        DuxType::Array(..) | DuxType::Struct(..) | DuxType::Enum(_) | DuxType::Slice(_)
        | DuxType::Error => return None,
    })
}

fn expression_name(expr: &Expr) -> &'static str {
    match expr {
        Expr::Member { .. } => "a tuple member (lands at M3)",
        Expr::Range { .. } => "a range (lands at M3)",
        Expr::Path { .. } => "a qualified path",
        Expr::StructLiteral { .. } => "a struct value (lands at M3)",
        Expr::CondBinding { .. } => "a pattern condition (lands at M4)",
        Expr::Error { .. } => "an erroneous expression",
        _ => "this expression",
    }
}

/// What a function body contributes to the allocation analysis.
/// The parameters of every function and method, in the order
/// `allocation_analysis` walks them.
fn parameter_lists(program: &Program) -> impl Iterator<Item = &[dux_ast::Param]> {
    let free = program.functions().map(|decl| decl.params.as_slice());
    let methods = program
        .decls
        .iter()
        .filter_map(|decl| match decl {
            Decl::Impl(block) => Some(block),
            _ => None,
        })
        .flat_map(|block| block.methods.iter())
        .map(|method| method.params.as_slice());
    free.chain(methods)
}

#[derive(Default)]
struct Scan {
    /// A call to `new`, `make`, `free` or `del`, or a `+` that joins text.
    allocates: bool,
    /// A read or rebinding of `context`.
    reads_context: bool,
    /// A write to `context`, which the enclosing block must undo.
    rebinds_context: bool,
    /// The callees written in the body, by the name they are written with.
    calls: Vec<String>,
    /// Names this body holds text in: `string` parameters, bindings that begin
    /// from text, and the fields of any record that declares a `string`.
    text: HashSet<String>,
    /// Functions and methods written as returning a `string`.
    text_calls: HashSet<String>,
    /// Inside the arguments of `print`/`println`, where an interpolated string
    /// is written out rather than built, and so allocates nothing.
    in_write: bool,
}

impl Scan {
    /// A type written as `string`. The walk that decides who carries a context
    /// runs before any types exist, so it reads the spelling.
    fn spelled_text(ty: &Type) -> bool {
        matches!(ty, Type::Named { name, .. } if name == "string")
    }
}

impl Scan {
    fn block(&mut self, block: &Block) {
        for stmt in &block.stmts {
            self.stmt(stmt);
        }
    }

    /// The value a name will hold, as far as a nameless walk can tell.
    fn is_text(&self, expr: &Expr) -> bool {
        match expr {
            Expr::Str { kind, .. } => !matches!(kind, dux_ast::StrKind::CString),
            Expr::InterpString { .. } => true,
            Expr::Ident { name, .. } => self.text.contains(name),
            Expr::Paren { inner, .. } => self.is_text(inner),
            Expr::Cast { target, .. } => Self::spelled_text(target),
            Expr::Field { name, .. } => self.text.contains(&name.name),
            Expr::Index { base, index, .. } => {
                matches!(&**index, Expr::Range { .. }) && self.is_text(base)
            }
            Expr::Block { value, .. } => self.is_text(value),
            Expr::Call { callee, .. } => matches!(
                &**callee,
                Expr::Ident { name, .. } if self.text_calls.contains(name)
            ),
            _ => false,
        }
    }

    fn stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Binding {
                name, ty, value, ..
            } => {
                if let Some(value) = value {
                    self.expr(value);
                }
                let holds_text = ty.as_ref().is_some_and(Self::spelled_text)
                    || value.as_ref().is_some_and(|value| self.is_text(value));
                if holds_text {
                    self.text.insert(name.name.clone());
                } else {
                    self.text.remove(&name.name);
                }
            }
            Stmt::Assign { target, value, .. } => {
                if matches!(target, Expr::Ident { name, .. } if name == "context") {
                    self.rebinds_context = true;
                }
                self.expr(target);
                self.expr(value);
            }
            Stmt::Expr { expr, .. } => self.expr(expr),
            Stmt::Return { value, .. } => {
                if let Some(value) = value {
                    self.expr(value);
                }
            }
            Stmt::Break { .. } | Stmt::Continue { .. } | Stmt::Error { .. } => {}
            Stmt::Defer { stmt, .. } => self.stmt(stmt),
            Stmt::Block { block, .. } => self.block(block),
            Stmt::If {
                cond,
                then_branch,
                else_branch,
                ..
            } => {
                self.expr(cond);
                self.block(then_branch);
                if let Some(other) = else_branch {
                    self.stmt(other);
                }
            }
            Stmt::While { cond, body, .. } => {
                self.expr(cond);
                self.block(body);
            }
            Stmt::Loop { body, .. } => self.block(body),
            Stmt::For { kind, body, .. } => {
                match kind {
                    ForKind::ThreeClause {
                        init,
                        cond,
                        step,
                    } => {
                        self.stmt(init);
                        self.expr(cond);
                        self.stmt(step);
                    }
                    ForKind::In { iterable, .. } => self.expr(iterable),
                }
                self.block(body);
            }
            Stmt::Match { subject, arms, .. } => {
                self.expr(subject);
                self.arms(arms);
            }
        }
    }

    fn arms(&mut self, arms: &[dux_ast::MatchArm]) {
        for arm in arms {
            if let Some(guard) = &arm.guard {
                self.expr(guard);
            }
            match &arm.body {
                dux_ast::MatchBody::Expr(expr) => self.expr(expr),
                dux_ast::MatchBody::Block(block) => self.block(block),
                dux_ast::MatchBody::Stmt(stmt) => self.stmt(stmt),
            }
        }
    }

    fn expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Call { callee, args, .. } => {
                // `print`/`println` format their holes where they stand, which
                // builds nothing; the walk must not mistake the two.
                if matches!(callee.as_ref(), Expr::Ident { name, .. } if name == "print" || name == "println")
                {
                    self.in_write = true;
                    for arg in args {
                        self.expr(arg);
                    }
                    self.in_write = false;
                    return;
                }
                match callee.as_ref() {
                    Expr::Ident { name, .. } if matches!(name.as_str(), "new" | "make" | "free" | "del") => {
                        self.allocates = true;
                    }
                    Expr::Ident { name, .. } => self.calls.push(name.clone()),
                    Expr::Field { base, name, .. } => {
                        self.calls.push(name.name.clone());
                        self.expr(base);
                    }
                    other => self.expr(other),
                }
                for arg in args {
                    self.expr(arg);
                }
            }
            Expr::Ident { name, .. } if name == "context" => self.reads_context = true,
            Expr::Binary {
                op: BinaryOp::Add,
                lhs,
                rhs,
                ..
            } => {
                if self.is_text(lhs) || self.is_text(rhs) {
                    self.allocates = true;
                }
                self.expr(lhs);
                self.expr(rhs);
            }
            Expr::Int { .. }
            | Expr::Float { .. }
            | Expr::Str { .. }
            | Expr::Char { .. }
            | Expr::Bool { .. }
            | Expr::Ident { .. }
            | Expr::Error { .. } => {}
            Expr::Field { base, .. } | Expr::Member { base, .. } => self.expr(base),
            Expr::Index { base, index, .. } => {
                self.expr(base);
                self.expr(index);
            }
            Expr::Range { start, end, .. } => {
                for bound in start.iter().chain(end.iter()) {
                    self.expr(bound);
                }
            }
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
            Expr::CondBinding { value, .. } => {
                self.expr(value);
            }
            Expr::Match { subject, arms, .. } => {
                self.expr(subject);
                for arm in arms {
                    if let Some(guard) = &arm.guard {
                        self.expr(guard);
                    }
                    match &arm.body {
                        dux_ast::MatchBody::Expr(expr) => self.expr(expr),
                        dux_ast::MatchBody::Block(block) => {
                            for stmt in &block.stmts {
                                self.stmt(stmt);
                            }
                        }
                        dux_ast::MatchBody::Stmt(stmt) => self.stmt(stmt),
                    }
                }
            }
            Expr::InterpString { parts, .. } => {
                if !self.in_write {
                    self.allocates = true;
                }
                for part in parts {
                    if let InterpolationPart::Hole {
                        written: Some(value), ..
                    } = part
                    {
                        self.expr(value);
                    }
                }
            }
            Expr::If {
                cond,
                then_arm,
                else_arm,
                ..
            } => {
                self.expr(cond);
                self.expr(then_arm);
                if let Some(other) = else_arm {
                    self.expr(other);
                }
            }
            Expr::Block { block, value, .. } => {
                self.block(block);
                self.expr(value);
            }
            Expr::Unary { operand, .. } => self.expr(operand),
            Expr::Binary { lhs, rhs, .. } => {
                self.expr(lhs);
                self.expr(rhs);
            }
            Expr::Cast { value, .. } => self.expr(value),
            Expr::Paren { inner, .. } => self.expr(inner),
        }
    }
}

/// A C library declaration, reused when the module already carries one: the
/// allocator runtime declares the same handful of names.
fn declared(
    module: &Module<'static>,
    name: &str,
    fn_type: inkwell::types::FunctionType<'static>,
) -> FunctionValue<'static> {
    module
        .get_function(name)
        .unwrap_or_else(|| module.add_function(name, fn_type, None))
}

/// Whether a function carries `#[name]`.
fn has_attribute(attributes: &[dux_ast::Attribute], name: &str) -> bool {
    attributes.iter().any(|attribute| attribute.name == name)
}

/// The fields a variant carries, named or positional. A positional payload
/// names its fields by position, which is what a pattern binds by anyway.
fn payload_fields(payload: &Payload) -> Vec<(&str, &Type)> {
    match payload {
        Payload::Empty => Vec::new(),
        Payload::Tuple(types) => types
            .iter()
            .enumerate()
            .map(|(index, ty)| (Box::leak(index.to_string().into_boxed_str()) as &str, ty))
            .collect(),
        Payload::Named(fields) => fields
            .iter()
            .map(|(name, ty)| (name.as_str(), ty))
            .collect(),
    }
}

/// The payload area: bytes aligned for the most strictly aligned payload and
/// long enough for the longest one. An array of that alignment's type does the
/// job without a union, which LLVM has no such thing as.
fn storage_area(
    context: &'static Context,
    data: &TargetData,
    payloads: &[StructType<'static>],
) -> BasicTypeEnum<'static> {
    let largest = payloads
        .iter()
        .copied()
        .map(|payload| data.get_abi_size(&payload))
        .max()
        .unwrap_or(0);
    let align_of = payloads
        .iter()
        .copied()
        .map(|payload| data.get_abi_alignment(&payload))
        .max()
        .unwrap_or(1);
    let unit = payloads
        .iter()
        .copied()
        .find(|payload| {
            data.get_abi_alignment(payload) == align_of && data.get_abi_size(payload) > 0
        })
        .unwrap_or_else(|| context.struct_type(&[], false));
    let units = largest.div_ceil(data.get_abi_size(&unit).max(1)) as u32;
    unit.array_type(units).into()
}

/// A sum type whose tag can hide in the spare value of its one pointer: two
/// variants, one carrying nothing and the other exactly one pointer.
fn niche_of(
    context: &'static Context,
    data: &TargetData,
    variants: &[VariantInfo],
    payloads: &[StructType<'static>],
) -> Option<u32> {
    if variants.len() != 2 {
        return None;
    }
    let empty = variants.iter().position(|variant| variant.fields.is_empty())?;
    let carrier = 1 - empty;
    let fields = &variants[carrier].fields;
    if !matches!(fields.first()?.1, DuxType::Ptr | DuxType::Pointer(_)) {
        return None;
    }
    let payload = payloads[carrier];
    let pointer = context.ptr_type(AddressSpace::default());
    (data.get_abi_size(&payload) == data.get_abi_size(&pointer)
        && data.get_abi_alignment(&payload) == data.get_abi_alignment(&pointer))
    .then_some(carrier as u32)
}


/// A `phi` result, said in the type the branches agreed on.
fn typed_of(value: BasicValueEnum<'static>, ty: DuxType) -> Typed {
    if ty.is_float() {
        Typed::float(value.into_float_value(), ty)
    } else if ty == DuxType::Ptr {
        Typed::pointer(value.into_pointer_value(), ty)
    } else if ty == DuxType::Str
        || ty.slice_element().is_some()
        || matches!(ty, DuxType::Struct(_) | DuxType::Enum(_))
    {
        Typed {
            core: Core::Aggregate(value.into_struct_value()),
            ty,
        }
    } else if ty.is_numeric() {
        Typed::int(value.into_int_value(), ty)
    } else {
        Typed::error()
    }
}

/// The type the payload field of a variant holds, when the instantiation is
/// already known.
fn payload_field_of(info: Option<&'static EnumInfo>, variant: usize, name: &str) -> Option<DuxType> {
    let fields = &info?.variants.get(variant)?.fields;
    fields
        .iter()
        .find(|(field, _)| *field == name)
        .map(|(_, ty)| *ty)
}

/// What an enum and its arguments are spelled as, which is also the key the
/// instantiation is cached under: `Option<i32>`.
fn instantiation(name: &str, args: &[DuxType]) -> String {
    if args.is_empty() {
        return name.to_string();
    }
    format!(
        "{name}<{}>",
        args.iter()
            .map(|arg| arg.name())
            .collect::<Vec<_>>()
            .join(", ")
    )
}
