//! Lowering from the AST to LLVM IR.
//!
//! The shape follows DESIGN.md §11.2: the entry block holds every alloca,
//! other instructions go in the blocks the control flow needs, and `main` is
//! a C `int main(void)` so the C runtime can start it (D12). Numeric
//! conversions follow D25: implicit exactly when the destination holds every
//! value of the source, `as` for anything else.

use std::collections::{HashMap, HashSet};

use crate::runtime::{HashRuntime, Runtime};
use dux_ast::{
    AttributeArgument, BinaryOp, Block, ConstDecl, Decl, Expr, FnDecl, ForKind, GlobalDecl,
    InterpolationPart, Program, Stmt, Type, UnaryOp,
};
use dux_diag::{code, Diagnostic};
use dux_hir::sums::{self, Payload, Shape};
use dux_hir::ty::CInt;
use dux_source::{FileId, SourceMap, Span};
use inkwell::attributes::{Attribute, AttributeLoc};
use inkwell::basic_block::BasicBlock;
use inkwell::builder::Builder;
use inkwell::context::Context;
use inkwell::debug_info::{
    AsDIScope, DICompileUnit, DIFile, DIFlags, DIFlagsConstants, DISubprogram, DWARFEmissionKind,
    DWARFSourceLanguage, DebugInfoBuilder,
};
use inkwell::module::{Linkage, Module};
use inkwell::targets::TargetData;
use inkwell::types::{
    BasicMetadataTypeEnum, BasicType, BasicTypeEnum, FloatType, FunctionType, IntType, StructType,
};
use inkwell::values::{AsValueRef, BasicValue, FunctionValue, ValueKind};
use inkwell::values::{
    BasicMetadataValueEnum, BasicValueEnum, CallSiteValue, FloatValue, GlobalValue, IntValue,
    PointerValue, StructValue,
};
use inkwell::{AddressSpace, FloatPredicate, IntPredicate};

const DW_ATE_BOOLEAN: u32 = 0x02;
const DW_ATE_FLOAT: u32 = 0x04;
const DW_ATE_SIGNED: u32 = 0x05;
const DW_ATE_UNSIGNED: u32 = 0x07;
const DW_ATE_ADDRESS: u32 = 0x0f;

/// The name a debugger is shown: a module's home is how the program finds
/// a name, not how the program says it.
fn di_written_name(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
}

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
    /// A C integer (D18): an `i8`..`i64` in the machine's eyes, its own type
    /// in the program's. The classification methods below answer through the
    /// kind, so the arithmetic lattice treats it like the width it has.
    C(CInt),
    F32,
    F64,
    /// C's `long double` (DESIGN 5.2): the machine's widest C float, LLVM's
    /// `x86_fp80` on this target. It is exactly what clang hands a C
    /// function, in a struct and across a boundary; the language gives it
    /// no operators, no methods and no literals - only `as` reaches it and
    /// only `as` comes back.
    CLongDouble,
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
    /// `(A, B)`: an anonymous, positional value. The LLVM representation is
    /// a literal struct of the elements, so a tuple copies, travels in
    /// registers and lives in a slot like any other aggregate.
    Tuple(&'static TupleInfo),
    /// A declared `struct`. The LLVM representation is the nominal struct
    /// type, so assignment is a copy of the whole record.
    Struct(&'static StructInfo),
    /// A declared `enum` at one instantiation: `Shape`, `Option<i32>`. The LLVM
    /// representation is the tagged struct of DESIGN 4.2 — a tag beside an area
    /// big enough for the largest payload — so a sum-type value is a value like
    /// a record: it copies, travels in registers and lives in a slot.
    Enum(&'static EnumInfo),
    /// `fn(i32) -> i32`: the address of a function, held as one word. The
    /// shape matters only where the address is called.
    Fn(&'static FnInfo),
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

/// The shape of an anonymous tuple. Two tuples written the same are one
/// type, so the emitter interns one `TupleInfo` per element list and
/// identity follows the spelling.
#[derive(Debug)]
pub(crate) struct TupleInfo {
    pub(crate) spelled: String,
    pub(crate) elems: Vec<DuxType>,
    pub(crate) llvm: StructType<'static>,
}

impl PartialEq for TupleInfo {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self, other)
    }
}

impl Eq for TupleInfo {}

/// The shape a function pointer carries: what it takes and what it answers.
/// A `fn` type is the C ABI — `&` takes the address of a `#[no_context]`
/// function, and the hidden context stays where it belongs, out of the shape.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct FnInfo {
    pub(crate) params: Vec<DuxType>,
    pub(crate) returns: Option<DuxType>,
}

impl FnInfo {
    fn spelled(info: &FnInfo) -> String {
        let params: Vec<String> = info.params.iter().map(|ty| ty.name()).collect();
        match info.returns {
            Some(answer) => format!("fn({}) -> {}", params.join(", "), answer.name()),
            None => format!("fn({})", params.join(", ")),
        }
    }
}

#[derive(Debug)]
pub(crate) struct StructInfo {
    pub(crate) name: &'static str,
    /// What the record is called with its arguments: `Vec3`, or
    /// `Pair<i32, string>`. Two ways of filling one generic `struct` are two
    /// records, and this is the name that keeps their fields apart.
    pub(crate) spelled: String,
    /// The arguments it was made of, empty for a record that takes none. A
    /// call that passes one says what its parameters stand for.
    pub(crate) args: Vec<DuxType>,
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
    /// What it was made of, in order. A call that passes one says what its
    /// parameters stand for.
    pub(crate) args: Vec<DuxType>,
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

    fn function(params: Vec<DuxType>, returns: Option<DuxType>) -> DuxType {
        DuxType::Fn(Box::leak(Box::new(FnInfo { params, returns })))
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
            "c_char" => DuxType::C(CInt::Char),
            "c_schar" => DuxType::C(CInt::SChar),
            "c_uchar" => DuxType::C(CInt::UChar),
            "c_short" => DuxType::C(CInt::Short),
            "c_ushort" => DuxType::C(CInt::UShort),
            "c_int" => DuxType::C(CInt::Int),
            "c_uint" => DuxType::C(CInt::UInt),
            "c_long" => DuxType::C(CInt::Long),
            "c_ulong" => DuxType::C(CInt::ULong),
            "c_long_long" => DuxType::C(CInt::LongLong),
            "c_ulong_long" => DuxType::C(CInt::ULongLong),
            "c_bool" => DuxType::C(CInt::Bool),
            "c_float" => DuxType::F32,
            "c_double" => DuxType::F64,
            "c_long_double" => DuxType::CLongDouble,
            "f32" => DuxType::F32,
            "f64" | "float" => DuxType::F64,
            "string" => DuxType::Str,
            "rawptr" => DuxType::Ptr,
            _ => return None,
        })
    }

    /// Whether this type's methods are the standard library's to write: text,
    /// a view over elements, a fixed array, an address, a number. A record and
    /// a sum type carry the methods their own `impl` blocks declared.
    fn methodless(self) -> bool {
        matches!(
            self,
            DuxType::Bool
                | DuxType::Str
                | DuxType::Ptr
                | DuxType::Pointer(_)
                | DuxType::Array(_)
                | DuxType::Slice(_)
                | DuxType::Tuple(_)
                | DuxType::CLongDouble
        ) || self.is_integral()
            || self.is_float()
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
                | DuxType::C(_)
        )
    }

    fn is_signed(self) -> bool {
        match self {
            DuxType::C(kind) => kind.signed(),
            _ => matches!(
                self,
                DuxType::I8 | DuxType::I16 | DuxType::I32 | DuxType::I64
            ),
        }
    }

    fn is_float(self) -> bool {
        matches!(self, DuxType::F32 | DuxType::F64)
    }

    /// Anything that holds an address: `*T` with a target, or a `rawptr`
    /// without one.
    fn is_pointer_like(self) -> bool {
        matches!(self, DuxType::Ptr | DuxType::Pointer(_))
    }

    /// Whether the type is one machine word that either holds an address or
    /// holds nothing — a pointer, or a `Option<*T>` whose `None` is NULL.
    /// The C boundary passes these interchangeably: `Some(address)` and the
    /// address are the same word (DESIGN 5.2).
    fn is_address_word(self) -> bool {
        match self {
            DuxType::Ptr | DuxType::Pointer(_) => true,
            DuxType::Enum(info) => {
                info.name == "Option"
                    && info.niche.is_some()
                    && info.args.first().is_some_and(|arg| arg.is_pointer_like())
            }
            _ => false,
        }
    }

    fn is_numeric(self) -> bool {
        self.is_integral() || self.is_float()
    }

    fn width(self) -> u32 {
        match self {
            DuxType::Bool => 1,
            DuxType::C(kind) => kind.width(),
            DuxType::I8 | DuxType::U8 => 8,
            DuxType::I16 | DuxType::U16 => 16,
            DuxType::I32 | DuxType::U32 | DuxType::F32 => 32,
            DuxType::CLongDouble => 128,
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
            DuxType::C(kind) => match kind.width() {
                8 => context.i8_type().into(),
                16 => context.i16_type().into(),
                32 => context.i32_type().into(),
                _ => context.i64_type().into(),
            },
            DuxType::F32 => context.f32_type().into(),
            DuxType::F64 => context.f64_type().into(),
            DuxType::CLongDouble => wide_float_type(context).into(),
            DuxType::Ptr | DuxType::Error => context.ptr_type(AddressSpace::default()).into(),
            DuxType::Fn(_) => context.ptr_type(AddressSpace::default()).into(),
            DuxType::Pointer(_) => context.ptr_type(AddressSpace::default()).into(),
            DuxType::Array(info) => info.elem.llvm(context).array_type(info.len as u32).into(),
            DuxType::Slice(_) | DuxType::Str => context
                .struct_type(
                    &[
                        context.ptr_type(AddressSpace::default()).into(),
                        context.i64_type().into(),
                    ],
                    false,
                )
                .into(),
            DuxType::Struct(info) => info.llvm.into(),
            DuxType::Tuple(info) => info.llvm.into(),
            DuxType::Enum(info) => info.llvm,
        }
    }

    fn int_type(self, context: &'static Context) -> IntType<'static> {
        match self {
            DuxType::Bool => context.bool_type(),
            DuxType::C(kind) => match kind.width() {
                8 => context.i8_type(),
                16 => context.i16_type(),
                32 => context.i32_type(),
                _ => context.i64_type(),
            },
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
            DuxType::C(kind) => kind.spelled().to_string(),
            DuxType::F32 => "f32".to_string(),
            DuxType::F64 => "f64".to_string(),
            DuxType::CLongDouble => "c_long_double".to_string(),
            DuxType::Str => "string".to_string(),
            DuxType::Ptr => "rawptr".to_string(),
            DuxType::Pointer(target) => format!("*{}", target.name()),
            DuxType::Array(info) => format!("[{}; {}]", info.elem.name(), info.len),
            DuxType::Slice(element) => format!("[]{}", element.name()),
            DuxType::Tuple(info) => info.spelled.clone(),
            DuxType::Struct(info) => info.name.to_string(),
            DuxType::Enum(info) => info.spelled.clone(),
            DuxType::Fn(info) => FnInfo::spelled(info),
            DuxType::Error => "<error>".to_string(),
        }
    }

    /// The type named down to its arguments, so two fillings of one generic
    /// make two keys: `*Cell<string>` and `*Cell<i32>` are not the same type
    /// and must not be asked for under one name.
    fn key(self) -> String {
        match self {
            DuxType::Pointer(target) => format!("*{}", target.key()),
            DuxType::Array(info) => format!("[{}; {}]", info.elem.key(), info.len),
            DuxType::Slice(element) => format!("[]{}", element.key()),
            DuxType::Tuple(info) => format!(
                "({})",
                info.elems
                    .iter()
                    .map(|ty| ty.key())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            DuxType::Struct(info) => info.spelled.clone(),
            DuxType::Enum(info) => info.spelled.clone(),
            DuxType::Fn(info) => FnInfo::spelled(info),
            other => other.name(),
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
            DuxType::C(kind) => {
                if kind.signed() {
                    1u128 << (kind.width() - 1)
                } else {
                    1u128 << kind.width()
                }
            }
            DuxType::F32 | DuxType::F64 => return true,
            DuxType::CLongDouble => return false,
            DuxType::Array(..)
            | DuxType::Struct(..)
            | DuxType::Tuple(..)
            | DuxType::Enum(_)
            | DuxType::Str
            | DuxType::Ptr
            | DuxType::Fn(_) => return false,
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

/// The subject a `match` is taking apart, whether its arms make a value, and
/// where those arms meet.
struct Matching<'a> {
    ty: DuxType,
    place: PointerValue<'a>,
    value: bool,
    join: Join<'a>,
}

/// One arm a `switch` reaches: the tag that lands here, the arm it belongs to,
/// and the names its pattern binds, with where each sits in the payload.
struct Case {
    tag: u64,
    arm: usize,
    bindings: Vec<(String, DuxType, u64)>,
}

/// A whole `match` as one `switch` on the tag.
struct Dispatch {
    cases: Vec<Case>,
    /// The `_` or `else` arm, when the `match` has one.
    default: Option<usize>,
}

/// Where the arms of a `match` meet: the block they branch to, the type they
/// agreed on, and the values that arrive there.
struct Join<'a> {
    block: BasicBlock<'a>,
    agreed: Option<DuxType>,
    carried: Vec<(Typed, BasicBlock<'static>)>,
    broken: bool,
}

#[derive(Clone, Copy)]
struct Loop {
    continuation: BasicBlock<'static>,
    exit: BasicBlock<'static>,
    /// How many deferred calls existed when the loop began: leaving the body
    /// past a deferred call skips the firing the block would have done on its
    /// way out, which needs the defer chain to follow the paths.
    defer_base: usize,
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
                            format!("the constant {value} does not fit a `{}`", ty.name()),
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
        DuxType::C(kind) => {
            if kind.signed() {
                let limit = 1i128 << (kind.width() - 1);
                -limit <= value && value < limit
            } else {
                value >= 0 && value < 1i128 << kind.width()
            }
        }
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

/// What one generic parameter stands for while an instantiation is lowered:
/// a type, or the value a `const` parameter was handed.
#[derive(Clone, Copy)]
enum Stood {
    Ty(DuxType),
    Const(u64),
}

/// What a call says the code it names is made of, and where that code lives:
/// a generic `fn` has no owner and no receiver; a method of a generic `impl`
/// has the type it extends and the receiver that completed it.
struct Asking<'a> {
    decl: &'a FnDecl,
    owner: Option<&'static str>,
    receiver: Option<DuxType>,
}

/// A generic `fn` one call asked for, waiting for its body: the code is the
/// declaration's, the types are the call's.
#[derive(Clone)]
struct Instance {
    key: String,
    symbol: String,
    /// The record a method of a generic `impl` belongs to.
    owner: Option<&'static str>,
    /// The receiver as the call says it, so a method that writes `self`
    /// without a type still knows what stands there.
    receiver: Option<DuxType>,
    decl: &'static FnDecl,
    bound: Vec<(String, Stood)>,
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
    /// The declaration came from C: its parameter list is exactly what was
    /// written, and a call may bring more arguments than that.
    external: bool,
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
    /// This unit is one instantiation of a generic `fn`, not the generic `fn`
    /// as written: its types are already known and it reaches the module.
    instantiated: bool,
    /// The receiver of a method of a generic `impl`, as its receiver said it.
    receiver: Option<DuxType>,
    decl: &'a FnDecl,
}

impl<'a> Unit<'a> {
    fn function(decl: &'a FnDecl) -> Self {
        Self {
            key: decl.name.name.clone(),
            symbol: decl.name.name.clone(),
            owner: None,
            external: false,
            instantiated: false,
            receiver: None,
            decl,
        }
    }

    fn imported(decl: &'a FnDecl, home: &str) -> Self {
        // `#[c_name("real_c_symbol")]` is the in-place fix for the symbol the
        // importer — or a linking scheme — would otherwise get wrong.
        let symbol = decl
            .attributes
            .iter()
            .find(|attribute| attribute.name == "c_name")
            .and_then(|attribute| match attribute.arguments.first() {
                Some(AttributeArgument::Literal(name)) => Some(name.clone()),
                _ => None,
            })
            .unwrap_or_else(|| decl.name.name.clone());
        Self {
            key: format!("{home}{}", decl.name.name),
            symbol,
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
            instantiated: false,
            receiver: None,
            decl,
        }
    }

    fn takes_a_receiver(&self) -> bool {
        self.owner.is_some()
            && self
                .decl
                .params
                .first()
                .is_some_and(|p| p.name.name == "self")
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
        span: Span,
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
    /// The anonymous tuple shapes seen, interned by their elements: one
    /// `(i32, i64)` is one type, however many places spell it.
    tuples: HashMap<String, &'static TupleInfo>,
    /// The module whose body is being lowered, written the way a use would
    /// write it. Inside `mod math` a bare name means the module's own first.
    home: String,
    /// Where each function was declared, so lowering its body knows the module
    /// it was written in.
    home_of: HashMap<String, String>,
    /// Which flattened slot holds each module's `extern` block, and where
    /// that block was written. A bare `fopen` in two namespaces is two
    /// declarations — `cs.fopen` and `stb.fopen` — and each must be read
    /// and called as the module it lives in says.
    extern_block: HashMap<usize, String>,
    /// Every `type` alias the program wrote, under its written name and the
    /// module-qualified one: `stbi_uc` and `stb.stbi_uc` stand for the same
    /// C typedef, and the signature text may say either.
    aliases: HashMap<String, &'static dux_ast::Type>,
    alias_depth: usize,
    /// Generic `fn` declarations, by name: the code each instantiation copies.
    generic_fns: HashMap<String, &'static FnDecl>,
    /// Methods of a generic `impl`, by `Pair.first`: the code each receiver
    /// instantiation copies.
    generic_methods: HashMap<String, &'static FnDecl>,
    /// The parameters of a generic `impl`, by record name.
    impl_params: HashMap<String, Vec<String>>,
    /// Generic `struct` declarations, by name.
    generic_structs: HashMap<String, &'static dux_ast::StructDecl>,
    /// A generic record at one instantiation, by the name it spells.
    instances: HashMap<String, &'static StructInfo>,
    /// What the parameters of the code being lowered stand for. A parameter
    /// name means whatever this says it means.
    substituting: Vec<(String, Stood)>,
    /// Instantiations a call asked for, waiting for their body.
    pending: Vec<Instance>,
    /// Instantiations whose body is in the module already: one per distinct
    /// set of arguments, however many calls asked for it.
    emitted: HashSet<String>,
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
    /// The blocks that own deferred work: where `defers` stood when the block
    /// began, and how deep the scopes were while it was open. A block fires
    /// what it captured on the way out.
    defer_blocks: Vec<(usize, usize)>,
    /// The allocator runtime, emitted when some function in the program
    /// allocates.
    runtime: Option<Runtime>,
    /// The hashing runtime, emitted when the program first asks for a `hash`.
    hash: Option<HashRuntime>,
    /// Head allocations with bookkeeping and report leaks at exit.
    pub(crate) debug_allocator: bool,
    /// Stop the program where an index lands outside its array, slice, or
    /// string, naming the line the index was written on.
    pub(crate) bounds_checks: bool,
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
    /// The machine being built for answers to the C standard library by a
    /// different name: Windows keeps its streams behind `__imp_` thunks.
    windows: bool,
    sources: SourceMap,
    dibuilder: Option<DebugInfoBuilder<'static>>,
    compile_unit: Option<DICompileUnit<'static>>,
    scope: Option<DISubprogram<'static>>,
    di_files: HashMap<FileId, DIFile<'static>>,
    /// The debug types seen, interned by their key: one `Vec3<i32>` is one
    /// DIE, however many signatures name it, and a record that points at
    /// itself through a pointer finds its own name waiting.
    di_types: HashMap<String, inkwell::debug_info::DIType<'static>>,
    /// The types being built, so a walk through a record's fields stops
    /// when it comes back around instead of chasing the cycle.
    di_under_construction: HashSet<String>,
    /// Where each record and sum type was declared, so its DIE can say so.
    di_decl_sites: HashMap<String, Span>,
    /// The file the function being lowered was written in, for the
    /// variables its body declares.
    di_scope_file: Option<DIFile<'static>>,
}

impl Emitter {
    pub(crate) fn new(context: &'static Context, sources: SourceMap, windows: bool) -> Self {
        Self {
            context,
            builder: context.create_builder(),
            diagnostics: Vec::new(),
            functions: HashMap::new(),
            signatures: HashMap::new(),
            struct_decls: Vec::new(),
            structs: HashMap::new(),
            tuples: HashMap::new(),
            generic_fns: HashMap::new(),
            home: String::new(),
            home_of: HashMap::new(),
            extern_block: HashMap::new(),
            aliases: HashMap::new(),
            alias_depth: 0,
            generic_methods: HashMap::new(),
            impl_params: HashMap::new(),
            generic_structs: HashMap::new(),
            instances: HashMap::new(),
            substituting: Vec::new(),
            pending: Vec::new(),
            emitted: HashSet::new(),
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
            defer_blocks: Vec::new(),
            runtime: None,
            hash: None,
            debug_allocator: true,
            bounds_checks: true,
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
            windows,
            sources,
            dibuilder: None,
            compile_unit: None,
            scope: None,
            di_files: HashMap::new(),
            di_types: HashMap::new(),
            di_under_construction: HashSet::new(),
            di_decl_sites: HashMap::new(),
            di_scope_file: None,
        }
    }

    pub(crate) fn lower(mut self, module: &Module<'static>, program: &Program) -> Vec<Diagnostic> {
        // A declaration inside a module is known by where it lives: a `fn sqrt`
        // written in `mod math` is `math.sqrt`. The lowering sees one flat
        // program that says so, and `home_of` remembers where each function was
        // written — which is what lets a bare name inside a module mean the
        // module's own.
        let rewritten = Program {
            decls: self.scoped_decls(program),
            ..program.clone()
        };
        let program = &rewritten;
        self.allocating = self.allocation_analysis(program);
        if !self.allocating.is_empty() {
            self.runtime = Some(Runtime::emit(
                self.context,
                module,
                self.debug_allocator,
                self.windows,
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
                DWARFSourceLanguage::C99,
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
        for decl in program.structs() {
            self.di_decl_sites
                .entry(decl.name.name.clone())
                .or_insert(decl.name.span);
        }
        for decl in program.enums() {
            self.di_decl_sites
                .entry(decl.name.name.clone())
                .or_insert(decl.name.span);
        }
        for decl in program.structs() {
            if !decl.type_params.is_empty() {
                self.generic_structs
                    .insert(decl.name.name.clone(), Box::leak(Box::new(decl.clone())));
            }
        }
        for decl in &program.decls {
            if let Decl::Impl(block) = decl {
                if block.type_params.is_empty() {
                    continue;
                }
                let params = block
                    .type_params
                    .iter()
                    .map(|param| param.name.name.clone())
                    .collect();
                self.impl_params
                    .insert(block.type_name.name.clone(), params);
                for method in &block.methods {
                    self.generic_methods.insert(
                        format!("{}.{}", block.type_name.name, method.name.name),
                        Box::leak(Box::new(method.clone())),
                    );
                }
            }
        }
        for decl in program.functions() {
            if !decl.type_params.is_empty() {
                self.generic_fns
                    .insert(decl.name.name.clone(), Box::leak(Box::new(decl.clone())));
            }
        }
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
        for (at, decl) in program.decls.iter().enumerate() {
            match decl {
                Decl::Const(decl) => self.declare_const(module, decl),
                Decl::Global(decl) => self.declare_global(module, decl),
                Decl::Extern(block) => {
                    for function in &block.functions {
                        let home = self.extern_block.get(&at).cloned().unwrap_or_default();
                        // The signature is read in the module the block was
                        // written in: `Option<*stbi_uc>` means the alias the
                        // namespace holds, not a name from out here.
                        self.home = home.clone();
                        self.declare(module, &Unit::imported(function, &home));
                        self.home = String::new();
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
        // A declaration's own types are read in the module it was written in.
        for unit in &units {
            self.home = self.home_of.get(&unit.key).cloned().unwrap_or_default();
            self.declare(module, unit);
        }
        self.home.clear();
        for unit in &units {
            self.lower_body(module, unit);
        }
        // A call inside one of those bodies may have asked for an
        // instantiation; a generic body may ask for another. The queue is how
        // the walk reaches the whole tree of them, recursion included.
        while !self.pending.is_empty() {
            let instance = self.pending.remove(0);
            self.lower_instance(module, &instance);
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
    /// Every declaration, under the name it is known by from outside the module
    /// it lives in. An `extern` block keeps the names it was written with:
    /// those belong to the C symbols they bind.
    fn scoped_decls(&mut self, program: &Program) -> Vec<Decl> {
        let mut decls = Vec::new();
        for entry in program.scoped() {
            if entry.home.is_empty() {
                decls.push(entry.decl.clone());
                continue;
            }
            self.remember_home(&entry.home, entry.decl);
            if let Decl::TypeAlias(alias) = entry.decl {
                if alias.type_params.is_empty() {
                    let stood = Box::leak(Box::new(alias.aliased.clone()));
                    self.aliases
                        .entry(format!("{}{}", entry.home, alias.name.name))
                        .or_insert(stood);
                    self.aliases.entry(alias.name.name.clone()).or_insert(stood);
                }
            }
            if let Decl::Extern(_) = entry.decl {
                self.extern_block
                    .insert(decls.len(), entry.home.to_string());
            }
            decls.push(named_in(&entry.home, entry.decl));
        }
        decls
    }

    fn remember_home(&mut self, home: &str, decl: &Decl) {
        match decl {
            Decl::Fn(function) => {
                let key = format!("{home}{}", function.name.name);
                self.home_of.insert(key, home.to_string());
            }
            Decl::Impl(block) => {
                let owner = format!("{home}{}", block.type_name.name);
                for method in &block.methods {
                    let key = format!("{owner}.{}", method.name.name);
                    self.home_of.insert(key, home.to_string());
                }
            }
            Decl::Extern(block) => {
                // An extern block inside a module answers to the module
                // from outside and to the C symbol itself at the link.
                for function in &block.functions {
                    let key = format!("{home}{}", function.name.name);
                    self.home_of.insert(key, home.to_string());
                }
            }
            _ => {}
        }
    }

    /// The name a written name answers to: a module's own declaration wins over
    /// a bare name the module does not declare, and everything else — the
    /// prelude, the rest of the program — stays reachable as it was written.
    fn named_in_home(&self, name: &str) -> String {
        if self.home.is_empty() || self.known(name) {
            return name.to_string();
        }
        let named = format!("{}{name}", self.home);
        if self.known(&named) {
            return named;
        }
        name.to_string()
    }

    /// Whether a name says a record, a sum type or a function this program has.
    fn known(&self, name: &str) -> bool {
        self.consts.contains_key(name)
            || self.structs.contains_key(name)
            || self.struct_decls.iter().any(|decl| decl.name.name == name)
            || self.generic_structs.contains_key(name)
            || self.shapes.contains_key(name)
            || self.functions.contains_key(name)
            || self.generic_fns.contains_key(name)
    }

    /// A source type as the lowering sees it. Primitives, pointers and fixed
    /// arrays come straight from the shared lattice; a `struct` name resolves
    /// to its nominal record.
    fn type_of(&mut self, ty: &Type) -> Option<DuxType> {
        match ty {
            Type::Named { name, args, .. } if args.is_empty() => match self.stands_for(name) {
                Some(Stood::Ty(ty)) => Some(ty),
                _ if self.alias_depth < 32 && self.stands_for_alias(name) => {
                    let key = self.aliases_key(name);
                    match self.aliases.get(&key).copied() {
                        Some(target) => {
                            self.alias_depth += 1;
                            let stood = self.type_of(target);
                            self.alias_depth -= 1;
                            stood
                        }
                        None => None,
                    }
                }
                _ => match DuxType::named(name) {
                    Some(primitive) => Some(primitive),
                    None => {
                        let named = self.named_in_home(name);
                        match self.struct_type_of(&named) {
                            Some(record) => Some(record),
                            None => self.enum_type_of(&named, &[]),
                        }
                    }
                },
            },
            Type::Named { name, args, .. } => {
                let mut given = Vec::new();
                for argument in args {
                    given.push(self.type_of(argument)?);
                }
                let named = self.named_in_home(name);
                if self.generic_structs.contains_key(&named) {
                    return self.record_instance_of(&named, &given);
                }
                self.enum_type_of(&named, &given)
            }
            Type::Slice { element, .. } => self
                .type_of(element)
                .map(|element| DuxType::Slice(Box::leak(Box::new(element)))),
            Type::Tuple { elements, .. } => {
                let mut elems = Vec::new();
                for element in elements {
                    elems.push(self.type_of(element)?);
                }
                Some(self.tuple_type(&elems))
            }
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
                let count = self.const_length(len)?;
                Some(DuxType::array(element, count))
            }
            Type::Function {
                params, return_ty, ..
            } => {
                let mut given = Vec::new();
                for parameter in params {
                    given.push(self.type_of(parameter)?);
                }
                let answer = match return_ty {
                    Some(ty) => Some(self.type_of(ty)?),
                    None => None,
                };
                Some(DuxType::function(given, answer))
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
            self.diagnostics.push(Diagnostic::error(
                code::CODEGEN_FAILED,
                format!("`{key}` has no size: a variant must reach itself through a pointer"),
            ));
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
                (
                    context.struct_type(&[tag.into(), storage], false).into(),
                    tag,
                    0u64,
                )
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
            args: args.to_vec(),
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
            let offset = fields
                .offsets
                .get(values.iter().position(|(name, _)| *name == field.0)?)
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
        if let Type::Named {
            name, args: none, ..
        } = spelled
        {
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
            .find(|decl| decl.name.name == name && decl.type_params.is_empty())
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
            spelled: name.to_string(),
            args: Vec::new(),
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
                format!(
                    "`{name}` has no size: a `struct` field must reach itself through a pointer"
                ),
            )
            .with_span(span),
        );
        self.struct_errors.insert(name.to_string());
        None
    }

    /// A declared record's fields, in declaration order.
    fn struct_field_list(&self, info: &StructInfo) -> &[(&'static str, DuxType)] {
        self.struct_fields
            .get(info.spelled.as_str())
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
            .build_load(
                self.context.ptr_type(AddressSpace::default()),
                slot,
                "context",
            )
            .ok()
            .map(|value| value.into_pointer_value())
    }

    /// The function in one slot of the current context: the allocator at 0,
    /// the deallocator at 1. A rebound context answers with its own
    /// functions, which is what makes an arena writable as ordinary code.
    fn context_function(
        &mut self,
        index: u64,
        span: Span,
    ) -> Option<(PointerValue<'static>, PointerValue<'static>)> {
        let context = self.context_value(span)?;
        let pointer_type = self.context.ptr_type(AddressSpace::default());
        let byte = self.context.i8_type();
        let cell = unsafe {
            self.builder.build_gep(
                byte,
                context,
                &[byte.const_int(index * 8, false)],
                if index == 0 { "allocfn" } else { "freefn" },
            )
        }
        .ok()?;
        let function = self
            .builder
            .build_load(
                pointer_type,
                cell,
                if index == 0 { "alloc" } else { "free" },
            )
            .ok()?
            .into_pointer_value();
        Some((context, function))
    }

    /// Bytes from the allocator the current context names, not from one the
    /// compiler picked at the call site.
    fn call_alloc(
        &mut self,
        span: Span,
        size: IntValue<'static>,
        align: u64,
        location: PointerValue<'static>,
    ) -> Option<PointerValue<'static>> {
        if self.runtime.is_none() {
            self.unsupported(span, "allocating in a program with no memory runtime");
            return None;
        }
        let (context, function) = self.context_function(0, span)?;
        let pointer_type = self.context.ptr_type(AddressSpace::default());
        let i64_type = self.context.i64_type();
        let fn_type = pointer_type.fn_type(
            &[
                pointer_type.into(),
                i64_type.into(),
                i64_type.into(),
                pointer_type.into(),
            ],
            false,
        );
        self.builder
            .build_indirect_call(
                fn_type,
                function,
                &[
                    context.into(),
                    size.into(),
                    i64_type.const_int(align, false).into(),
                    location.into(),
                ],
                "alloc",
            )
            .ok()?
            .try_as_basic_value()
            .basic()
            .map(|value| value.into_pointer_value())
    }

    /// Hand memory back to the deallocator the current context names. Memory
    /// an arena handed out comes back through the arena that made it.
    fn call_free(&mut self, span: Span, memory: PointerValue<'static>) -> bool {
        if self.runtime.is_none() {
            self.unsupported(span, "freeing in a program with no memory runtime");
            return false;
        }
        let Some((context, function)) = self.context_function(1, span) else {
            return false;
        };
        let pointer_type = self.context.ptr_type(AddressSpace::default());
        let fn_type = self
            .context
            .void_type()
            .fn_type(&[pointer_type.into(), pointer_type.into()], false);
        self.builder
            .build_indirect_call(fn_type, function, &[context.into(), memory.into()], "free")
            .is_ok()
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

    /// `hash(key)`: the bits of a value, scattered. A builtin because a
    /// generic key is the one place bits are wanted before any type an `impl`
    /// can name exists; what is hashable is decided per concrete type here.
    fn hash_builtin(
        &mut self,
        module: &Module<'static>,
        args: &[Expr],
        span: Span,
    ) -> Option<Typed> {
        if args.len() != 1 {
            self.diagnostics.push(
                Diagnostic::error(
                    code::WRONG_ARITY,
                    format!("`hash` takes one argument, given {}", args.len()),
                )
                .with_span(span),
            );
            return None;
        }
        let value = self.expr(module, args.first()?, None)?;
        if value.ty == DuxType::Error {
            return None;
        }
        if matches!(
            value.ty,
            DuxType::Array(..)
                | DuxType::Slice(_)
                | DuxType::Struct(..)
                | DuxType::Tuple(..)
                | DuxType::Enum(_)
        ) {
            self.unsupported(span, &format!("hashing a `{}`", value.ty.name()));
            return None;
        }
        let hash = self.hash_runtime(module);
        if value.ty == DuxType::Str {
            let (data, len) = self.slice_parts(value)?;
            let mixed = self
                .builder
                .build_call(hash.bytes, &[data.into(), len.into()], "hash")
                .ok()?
                .try_as_basic_value()
                .basic()?
                .into_int_value();
            return Some(Typed {
                core: Core::Int(mixed),
                ty: DuxType::U64,
            });
        }
        let i64_type = self.context.i64_type();
        let word = match value.core {
            Core::Int(at) => {
                if at.get_type().get_bit_width() >= 64 {
                    at
                } else if matches!(value.ty, DuxType::I8 | DuxType::I16 | DuxType::I32) {
                    self.builder
                        .build_int_s_extend(at, i64_type, "whole")
                        .ok()?
                } else {
                    self.builder
                        .build_int_z_extend(at, i64_type, "whole")
                        .ok()?
                }
            }
            Core::Float(at) => {
                let bits: inkwell::types::BasicTypeEnum<'static> = if value.ty == DuxType::F32 {
                    self.context.i32_type().into()
                } else {
                    i64_type.into()
                };
                let cast = self.builder.build_bit_cast(at, bits, "bits").ok()?;
                let as_int = cast.into_int_value();
                if as_int.get_type().get_bit_width() < 64 {
                    self.builder
                        .build_int_z_extend(as_int, i64_type, "whole")
                        .ok()?
                } else {
                    as_int
                }
            }
            Core::Ptr(at) => self.builder.build_ptr_to_int(at, i64_type, "bits").ok()?,
            _ => {
                self.unsupported(span, &format!("hashing a `{}`", value.ty.name()));
                return None;
            }
        };
        let mixed = self
            .builder
            .build_call(hash.mix, &[word.into()], "hash")
            .ok()?
            .try_as_basic_value()
            .basic()?
            .into_int_value();
        Some(Typed {
            core: Core::Int(mixed),
            ty: DuxType::U64,
        })
    }

    fn hash_runtime(&mut self, module: &Module<'static>) -> HashRuntime {
        if let Some(hash) = self.hash {
            return hash;
        }
        let hash = HashRuntime::emit(self.context, module);
        self.hash = Some(hash);
        hash
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
        let location = self.location(span);
        self.call_alloc(span, count, align as u64, location)
    }

    fn memory_builtin(
        &mut self,
        module: &Module<'static>,
        name: &str,
        args: &[Expr],
        span: Span,
    ) -> Option<Typed> {
        if self.runtime.is_none() {
            self.unsupported(span, "allocation");
            return None;
        }
        let location = self.location(args.first().map_or(span, Expr::span));
        match name {
            "new" => {
                let element = self.type_argument(args.first(), name)?;
                let llvm = element.llvm(self.context);
                let size = self.size_of(llvm)?;
                let align = self.alignment_of(llvm);
                let pointer =
                    self.call_alloc(args.first()?.span(), size, align as u64, location)?;
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
                let total = self.builder.build_int_mul(each, count, "bytes").ok()?;
                let pointer = self.call_alloc(args[1].span(), total, align as u64, location)?;
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
                self.call_free(span, pointer);
                Some(Typed::void())
            }
            "del" => {
                let value = self.expr(module, args.first()?, None)?;
                let Some((pointer, _)) = self.slice_parts(value) else {
                    self.unsupported(args[0].span(), "`del` of something that is not a slice");
                    return None;
                };
                self.call_free(span, pointer);
                Some(Typed::void())
            }
            _ => None,
        }
    }

    /// The type named as a builtin's first argument, as in `new(Vec3)`.
    fn type_argument(&mut self, argument: Option<&Expr>, name: &str) -> Option<DuxType> {
        let Some(Expr::Ident {
            name: type_name,
            span,
        }) = argument
        else {
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
    /// its first field's, and the target's own layout has the last word:
    /// a field's offset in a `{ i8, T }` is `T`'s alignment, and C's widest
    /// float asks for more than any Dux primitive does.
    fn alignment_of(&mut self, ty: BasicTypeEnum<'static>) -> u32 {
        let size = self
            .size_of(ty)
            .and_then(|size| size.get_zero_extended_constant());
        let natural = match size {
            Some(value) if value < 8 => value as u32,
            _ => 8,
        };
        let i64_type = self.context.i64_type();
        let probe = self
            .context
            .struct_type(&[self.context.i8_type().into(), ty], false);
        let probe_address = unsafe {
            self.builder
                .build_gep(
                    probe,
                    self.context.ptr_type(AddressSpace::default()).const_null(),
                    &[i64_type.const_zero(), i64_type.const_int(1, false)],
                    "aligned",
                )
                .ok()
        }
        .and_then(|at| {
            self.builder
                .build_ptr_to_int(at, i64_type, "alignment")
                .ok()
        })
        .and_then(|offset| offset.get_zero_extended_constant())
        .unwrap_or(natural as u64);
        natural.max(probe_address as u32)
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
        // A method of a generic `impl` is not a function: only a receiver that
        // says its types makes one. The call naming such a receiver emits it,
        // under a key carrying the types.
        if !unit.instantiated && self.generic_methods.contains_key(&key) {
            self.skipped.insert(key);
            return;
        }
        let external = unit.external;
        let function = unit.decl;
        if self.skipped.contains(&key) || self.functions.contains_key(&key) {
            if !external {
                self.diagnostics.push(
                    Diagnostic::error(
                        code::DUPLICATE_DEFINITION,
                        match unit.owner {
                            Some(owner) => {
                                format!("`{owner}` has more than one `{}`", function.name.name)
                            }
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
                    external: false,
                },
            );
            return;
        }
        if !function.type_params.is_empty() && !unit.instantiated {
            // A generic `fn` is code that has not been asked for anything yet.
            // It reaches the module one instantiation per call, which is what
            // makes `max<i32>` a real function and `max` not one at all.
            self.skipped.insert(key.clone());
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
            Some(ty) => match self.type_of(ty) {
                Some(ty) => Some(ty),
                None => {
                    self.skipped.insert(key.clone());
                    if !external {
                        self.unsupported(
                            ty.span(),
                            "this result type (primitives, pointers and records today)",
                        );
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
            .map(|ty| match ty {
                // A record that crosses the C boundary crosses it by
                // address. The importer's wrappers take records as
                // pointers for exactly this reason: LLVM and clang do not
                // agree on the shape of a by-value record, and an address
                // is one shape everyone shares.
                DuxType::Struct(_) if external => {
                    BasicMetadataTypeEnum::from(self.context.ptr_type(AddressSpace::default()))
                }
                other => BasicMetadataTypeEnum::from(other.llvm(self.context)),
            })
            .collect::<Vec<BasicMetadataTypeEnum>>();
        if hidden_context {
            declared.push(self.context.ptr_type(AddressSpace::default()).into());
        }
        // A C function that hands back a record hands it back through a
        // slot the caller provides. LLVM spells that slot as a leading
        // `sret` pointer and a `void` result — the shape clang gives the
        // `__duxr_` wrappers the importer compiles, so the declaration
        // here and the definition in the linked bitcode are one function.
        let sret = external && matches!(return_ty, Some(DuxType::Struct(_)));
        let signature = if sret {
            let mut shaped = vec![BasicMetadataTypeEnum::from(
                self.context.ptr_type(AddressSpace::default()),
            )];
            shaped.extend(declared.iter().copied());
            self.context.void_type().fn_type(&shaped, external)
        } else {
            match return_ty {
                Some(ty) => ty.llvm(self.context).fn_type(&declared, external),
                None => self.context.void_type().fn_type(&declared, external),
            }
        };
        // Two declarations may name one C symbol — a `#[c_name]` alias and
        // the plain declaration, say. The symbol is the truth: both keys
        // answer with the same function.
        let existing = module.get_function(&unit.symbol).filter(|_| external);
        let fn_value = match existing {
            Some(found) => found,
            None => {
                let made = module.add_function(&unit.symbol, signature, None);
                if sret {
                    made.add_attribute(
                        AttributeLoc::Param(0),
                        Emitter::sret_attribute(self.context),
                    );
                }
                made
            }
        };
        self.functions.insert(key.clone(), fn_value);
        self.signatures.insert(
            key,
            Signature {
                params,
                return_ty,
                receiver,
                hidden_context,
                external,
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
        if first {
            if let Some(receiver) = unit.receiver {
                // A method of a generic `impl` reads its receiver as whatever
                // stood there and said the types: `self` with no type written
                // is that, and a `self: *T` borrows it.
                return Some(match &param.ty {
                    Type::Pointer { .. } => self.pointer_to(receiver),
                    _ => receiver,
                });
            }
        }
        let Some(owner) = unit.owner else {
            return self.type_of(&param.ty);
        };
        if !first || param.name.name != "self" || unit.instantiated {
            return self.type_of(&param.ty);
        }
        let record = self.struct_type_of(owner)?;
        Some(match &param.ty {
            Type::Pointer { .. } => self.pointer_to(record),
            _ => record,
        })
    }

    fn lower_body(&mut self, module: &Module<'static>, unit: &Unit) {
        let saved_home = std::mem::take(&mut self.home);
        let written = spelled_without_arguments(&unit.key);
        self.home = self
            .home_of
            .get(&unit.key)
            .or_else(|| self.home_of.get(&written))
            .cloned()
            .unwrap_or_else(|| saved_home.clone());
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
            self.note_slot(&param.name.name, ty, slot);
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
        self.home = saved_home;
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
        self.block_body_owning(module, block, bound, false);
    }

    /// `owns_defers` marks the blocks whose exit is their own — a plain
    /// `{ .. }` statement, a branch arm, a loop body: a `defer` written
    /// directly inside one runs when that block runs out. A block that ends
    /// mid-flight, on a `return` or a `break`, has already handed its chain
    /// to the exit that left; the paths standing where codegen resumes never
    /// registered it.
    fn block_body_owning(
        &mut self,
        module: &Module<'static>,
        block: &Block,
        bound: &[Binding],
        owns_defers: bool,
    ) {
        self.scopes.push(HashMap::new());
        self.declare_bindings(bound);
        let outer = self.save_context(block);
        let base = self.defers.len();
        let depth = self.scopes.len();
        if owns_defers {
            self.defer_blocks.push((base, depth));
        }
        for stmt in &block.stmts {
            if self.dead {
                break;
            }
            self.stmt(module, stmt);
        }
        if owns_defers {
            self.defer_blocks.pop();
        }
        // The function body's own exit is the epilogue's to fire; a block
        // that owns its defers fires what it captured on the way out.
        if owns_defers && !self.dead {
            self.fire_before(module, base, true);
        } else if owns_defers {
            self.defers.truncate(base);
        }
        self.restore_context(outer);
        self.scopes.pop();
    }

    /// Move a value into a slot of its own type, which is how a `match` gets a
    /// subject it can take apart. An array is storage rather than a word, so it
    /// is copied element by element like a binding copies it.
    fn put_in(&mut self, slot: PointerValue<'static>, value: Typed, span: Span) -> Option<()> {
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

        let mut work = Matching {
            ty,
            place,
            value,
            join: Join {
                block: self.new_block("match.end"),
                agreed: expected,
                carried: Vec::new(),
                broken: false,
            },
        };
        match self.dispatch_for(arms, ty) {
            Some(dispatch) => self.arms_by_switch(module, &dispatch, arms, &mut work)?,
            None => self.arms_by_chain(module, arms, &mut work)?,
        }
        let join = work.join;

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
        let mut incoming: Vec<(
            inkwell::values::BasicValueEnum<'static>,
            BasicBlock<'static>,
        )> = Vec::new();
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

    /// Plan the `switch` DESIGN asks the backend for, when a `switch` can say
    /// this `match`: every arm is one variant of the subject's own enum, named
    /// once, and nothing below the variant tests anything — the tag has already
    /// answered, so the arm only has names to fill. A guard, a literal, a
    /// range, an alternative, a second arm for the same variant, or a variant
    /// taken apart beyond the names it binds is beyond a `switch`, and such a
    /// `match` is the chain of tests instead.
    fn dispatch_for(&self, arms: &[dux_ast::MatchArm], ty: DuxType) -> Option<Dispatch> {
        let info = self.enum_of(ty)?;
        if info.niche.is_some() {
            return None;
        }
        let mut cases: Vec<Case> = Vec::new();
        let mut default: Option<usize> = None;
        for (position, arm) in arms.iter().enumerate() {
            if arm.guard.is_some() {
                return None;
            }
            match &arm.pattern {
                dux_ast::Pattern::Wildcard { .. } | dux_ast::Pattern::Else { .. } => {
                    if position + 1 != arms.len() {
                        return None;
                    }
                    default = Some(position);
                }
                other => {
                    let (qualifier, name) = match other {
                        dux_ast::Pattern::Name { name, .. } => (None, name.name.as_str()),
                        dux_ast::Pattern::Path {
                            qualifier, name, ..
                        } => (Some(qualifier.as_str()), name.as_str()),
                        dux_ast::Pattern::Tuple {
                            qualifier, name, ..
                        } => (qualifier.as_deref(), name.as_str()),
                        dux_ast::Pattern::Struct {
                            qualifier, name, ..
                        } => (qualifier.as_deref(), name.as_str()),
                        _ => return None,
                    };
                    let (index, variant) = self.variant_of(ty, qualifier, name)?;
                    if cases.iter().any(|case| case.tag == index as u64) {
                        return None;
                    }
                    let bindings = self.names_below(other, variant)?;
                    cases.push(Case {
                        tag: index as u64,
                        arm: position,
                        bindings,
                    });
                }
            }
        }
        Some(Dispatch { cases, default })
    }

    /// The names a `switch` arm binds, each with the type it binds and where it
    /// sits. `None` says the pattern reaches past a name — a variant inside a
    /// variant, a literal, a range — and that is a test, which a `switch` arm
    /// does not make.
    fn names_below(
        &self,
        pattern: &dux_ast::Pattern,
        variant: &VariantInfo,
    ) -> Option<Vec<(String, DuxType, u64)>> {
        let field_at = |name: &str| -> Option<(DuxType, u64)> {
            let position = variant
                .fields
                .iter()
                .position(|(known, _)| *known == name)?;
            let (_, ty) = variant.fields[position];
            Some((ty, *variant.offsets.get(position)?))
        };
        match pattern {
            dux_ast::Pattern::Wildcard { .. } | dux_ast::Pattern::Else { .. } => Some(Vec::new()),
            dux_ast::Pattern::Name { .. } | dux_ast::Pattern::Path { .. }
                if variant.fields.is_empty() =>
            {
                Some(Vec::new())
            }
            dux_ast::Pattern::Tuple { elements, .. } if elements.len() == variant.fields.len() => {
                let mut names = Vec::new();
                for (position, element) in elements.iter().enumerate() {
                    let (_, ty) = variant.fields[position];
                    let offset = *variant.offsets.get(position)?;
                    names.extend(self.names_under(element, ty, offset)?);
                }
                Some(names)
            }
            dux_ast::Pattern::Struct { fields, rest, .. } => {
                if !fields.is_empty() && !*rest && fields.len() != variant.fields.len() {
                    return None;
                }
                let mut names = Vec::new();
                for field in fields {
                    match field {
                        dux_ast::FieldPattern::Shorthand { name, .. } => {
                            let (ty, offset) = field_at(name.name.as_str())?;
                            names.push((name.name.clone(), ty, offset));
                        }
                        dux_ast::FieldPattern::Nested { name, pattern, .. } => {
                            let (ty, offset) = field_at(name.name.as_str())?;
                            names.extend(self.names_under(pattern, ty, offset)?);
                        }
                    }
                }
                Some(names)
            }
            _ => None,
        }
    }

    /// One pattern under a field: a name, or nothing.
    fn names_under(
        &self,
        pattern: &dux_ast::Pattern,
        ty: DuxType,
        offset: u64,
    ) -> Option<Vec<(String, DuxType, u64)>> {
        match pattern {
            dux_ast::Pattern::Wildcard { .. } | dux_ast::Pattern::Else { .. } => Some(Vec::new()),
            dux_ast::Pattern::Name { name, .. }
                if self.variant_of(ty, None, &name.name).is_none() =>
            {
                Some(vec![(name.name.clone(), ty, offset)])
            }
            _ => None,
        }
    }

    /// The arms, tested one after another in the order they are written. This
    /// is the general lowering: it can say whatever a pattern says.
    fn arms_by_chain(
        &mut self,
        module: &Module<'static>,
        arms: &[dux_ast::MatchArm],
        work: &mut Matching<'static>,
    ) -> Option<()> {
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
            self.pattern_holds(
                module,
                &arm.pattern,
                work.ty,
                work.place,
                entered,
                next,
                &mut bound,
            );
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
            self.arm_arrives(module, arm, work);
            rest = next;
        }

        // DESIGN 4.4: a `match` that is not exhaustive is an error, so an arm
        // always matches and the chain never runs out. A hole in that rule is a
        // compiler bug, and a bug should say so rather than run on.
        self.builder.position_at_end(rest);
        self.dead = false;
        self.trap(module, "dux: no `match` arm matched\n");
        Some(())
    }

    /// Read the tag once, jump to the arm its value names, and bind the fields
    /// that arm asked for. Where the tags are dense LLVM answers with a jump
    /// table, which is what a command loop with thirty variants is for. A tag
    /// no case names is the trap the chain ends in, because the checker said
    /// there is no such tag.
    fn arms_by_switch(
        &mut self,
        module: &Module<'static>,
        dispatch: &Dispatch,
        arms: &[dux_ast::MatchArm],
        work: &mut Matching<'static>,
    ) -> Option<()> {
        let info = self.enum_of(work.ty)?;
        let here = self.builder.get_insert_block()?;
        let mut blocks = Vec::new();
        for _ in &dispatch.cases {
            blocks.push(self.new_block("match.arm"));
        }
        let default = match dispatch.default {
            Some(arm) => (self.new_block("match.arm"), Some(arm)),
            None => {
                let block = self.new_block("match.trap");
                self.builder.position_at_end(block);
                self.dead = false;
                self.trap(module, "dux: no `match` arm matched\n");
                self.builder.position_at_end(here);
                (block, None)
            }
        };
        let tag = self
            .builder
            .build_load(info.tag, self.byte_at(work.place, info.tag_at), "tag")
            .ok()?
            .into_int_value();
        let cases: Vec<(IntValue<'static>, BasicBlock<'static>)> = dispatch
            .cases
            .iter()
            .zip(&blocks)
            .map(|(case, block)| (info.tag.const_int(case.tag, false), *block))
            .collect();
        let _ = self.builder.build_switch(tag, default.0, &cases).ok()?;
        for (case, block) in dispatch.cases.iter().zip(&blocks) {
            self.enter_arm(module, *block, &arms[case.arm], &case.bindings, work);
        }
        if let Some(arm) = default.1 {
            self.enter_arm(module, default.0, &arms[arm], &[], work);
        }
        Some(())
    }

    /// The arm a `switch` jumped to: its names, then its body.
    fn enter_arm(
        &mut self,
        module: &Module<'static>,
        block: BasicBlock<'static>,
        arm: &dux_ast::MatchArm,
        bindings: &[(String, DuxType, u64)],
        work: &mut Matching<'static>,
    ) {
        self.builder.position_at_end(block);
        self.dead = false;
        self.scopes.push(HashMap::new());
        let mut bound = Vec::new();
        for (name, field, offset) in bindings {
            let at = self.byte_at(work.place, *offset);
            self.bind_pattern(name, *field, at, &mut bound);
        }
        self.declare_bindings(&bound);
        self.arm_arrives(module, arm, work);
    }

    /// Run the arm that matched, and bring whatever it produces to the place
    /// where the arms meet. The arm's scope is pushed and its names bound
    /// before this runs; an arm that leaves by `return` brings nothing.
    fn arm_arrives(
        &mut self,
        module: &Module<'static>,
        arm: &dux_ast::MatchArm,
        work: &mut Matching<'static>,
    ) {
        let value = work.value;
        let join = &mut work.join;
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
        let common = join
            .agreed
            .expect("the first arm gives the `match` its type");
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
                let base = self.defers.len();
                self.defer_blocks.push((base, self.scopes.len()));
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
                self.defer_blocks.pop();
                if !self.dead {
                    self.fire_before(module, base, true);
                } else {
                    self.defers.truncate(base);
                }
                self.scopes.pop();
                outcome
            }
            dux_ast::MatchBody::Stmt(stmt) => {
                self.scopes.push(HashMap::new());
                let base = self.defers.len();
                self.defer_blocks.push((base, self.scopes.len()));
                self.stmt(module, stmt);
                self.defer_blocks.pop();
                if !self.dead {
                    self.fire_before(module, base, true);
                } else {
                    self.defers.truncate(base);
                }
                self.scopes.pop();
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
            dux_ast::Pattern::Name { name, span } => match self.variant_of(ty, None, &name.name) {
                Some((index, _)) => self.variant_test(ty, index, place, matched, declined, *span),
                None => {
                    self.bind_pattern(&name.name, ty, place, bound);
                    self.br(matched);
                }
            },
            dux_ast::Pattern::Path {
                qualifier,
                name,
                span,
            } => match self.variant_of(ty, Some(qualifier), name) {
                Some((index, _)) => self.variant_test(ty, index, place, matched, declined, *span),
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
                let Some((index, variant)) = self.variant_of(ty, qualifier.as_deref(), name) else {
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
                    let onward = if last {
                        matched
                    } else {
                        self.new_block("match.element")
                    };
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
                let Some((index, variant)) = self.variant_of(ty, qualifier.as_deref(), name) else {
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
                    let inside = variant
                        .fields
                        .iter()
                        .position(|(known, _)| known == field_name);
                    let Some(inside) = inside else {
                        self.missing_variant_field(ty, name, field_name, *span);
                        self.br(declined);
                        return;
                    };
                    let (_, field_ty) = variant.fields[inside];
                    let at = self.byte_at(place, variant.offsets[inside]);
                    let last = position + 1 == fields.len();
                    let onward = if last {
                        matched
                    } else {
                        self.new_block("match.field")
                    };
                    match nested {
                        Some(nested) => self
                            .pattern_holds(module, nested, field_ty, at, onward, declined, bound),
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
                        let _ = self
                            .builder
                            .build_conditional_branch(flag, matched, declined);
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
                let low =
                    self.range_bound(module, start, true, *inclusive, ty, place, bound, *span);
                let high =
                    self.range_bound(module, end, false, *inclusive, ty, place, bound, *span);
                let test = match (low, high) {
                    (Some(low), Some(high)) => self.builder.build_and(low, high, "within").ok(),
                    (Some(one), None) => Some(one),
                    (None, Some(one)) => Some(one),
                    (None, None) => None,
                };
                match test {
                    Some(flag) => {
                        let _ = self
                            .builder
                            .build_conditional_branch(flag, matched, declined);
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
        let spelled =
            qualifier.map_or_else(|| name.to_string(), |owner| format!("{owner}::{name}"));
        self.diagnostics.push(
            Diagnostic::error(
                code::TYPE_MISMATCH,
                format!("`{spelled}` is not a variant of `{}`", ty.name()),
            )
            .with_span(span)
            .with_note("a `match` arm names a variant of the type it matches"),
        );
    }

    fn missing_variant_field(&mut self, ty: DuxType, variant: &str, field: &str, span: Span) {
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
        for (name, ty, slot) in bound {
            self.note_slot(name, *ty, *slot);
        }
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
        let same = if ty == DuxType::Str || want.ty == DuxType::Str || ty.slice_element().is_some()
        {
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
            self.runtime = Some(Runtime::emit(
                self.context,
                module,
                self.debug_allocator,
                self.windows,
            ));
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

    /// `assert(cond)` stops the program where the condition says no, and
    /// `debug_assert` says the same until a release build decides the check
    /// is not worth its price. The condition still runs in a release build;
    /// only the stopping stands aside.
    fn assert_builtin(
        &mut self,
        module: &Module<'static>,
        name: &str,
        args: &[Expr],
        span: Span,
    ) -> Option<Typed> {
        if args.len() != 1 {
            self.unsupported(span, &format!("`{name}` with anything but one condition"));
            return None;
        }
        let condition = self.expr(module, &args[0], None)?;
        if condition.ty != DuxType::Bool {
            self.diagnostics.push(
                Diagnostic::error(
                    code::TYPE_MISMATCH,
                    format!(
                        "`{name}` reads a bool condition, and `{}` is not one",
                        condition.ty.name()
                    ),
                )
                .with_span(span),
            );
            return None;
        }
        let test = condition.int_value()?;
        let checked = name == "assert" || self.debug_allocator;
        if !checked || self.dead {
            return Some(Typed::void());
        }
        let carries_on = self.new_block("assert.ok");
        let broken = self.new_block("assert.trap");
        if self
            .builder
            .build_conditional_branch(test, carries_on, broken)
            .is_err()
        {
            return None;
        }
        self.builder.position_at_end(broken);
        let file = self.sources.name_of(span.file).to_string();
        let line = self.sources.line_col(span.file, span.start).0;
        self.trap(module, &format!("dux: assertion failed at {file}:{line}\n"));
        self.builder.position_at_end(carries_on);
        self.dead = false;
        Some(Typed::void())
    }

    /// The debug bounds check: when the answer says the index landed outside
    /// its view, the program stops where it broke the rule, and the message
    /// names the line the index was written on. A checked index stays whole
    /// for the paths that stay inside.
    fn bounds_guard(
        &mut self,
        module: &Module<'static>,
        within: IntValue<'static>,
        span: Span,
        what: &str,
    ) {
        if !self.bounds_checks || self.dead {
            return;
        }
        let carries_on = self.new_block("bounds.ok");
        let broken = self.new_block("bounds.trap");
        if self
            .builder
            .build_conditional_branch(within, carries_on, broken)
            .is_err()
        {
            return;
        }
        self.builder.position_at_end(broken);
        let file = self.sources.name_of(span.file).to_string();
        let line = self.sources.line_col(span.file, span.start).0;
        self.trap(
            module,
            &format!("dux: {what} out of bounds at {file}:{line}\n"),
        );
        self.builder.position_at_end(carries_on);
        self.dead = false;
    }

    /// `index < len`, unsigned: a negative index is a very large one and
    /// lands outside like any other.
    fn index_reaches_into(
        &mut self,
        index: IntValue<'static>,
        len: IntValue<'static>,
    ) -> Option<IntValue<'static>> {
        self.builder
            .build_int_compare(IntPredicate::ULT, index, len, "within")
            .ok()
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
            } => self.binding(
                module,
                name.name.clone(),
                ty.as_ref(),
                value.as_ref(),
                *span,
            ),
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
                match self.loops.last().copied() {
                    Some(loop_) => {
                        // A `break` or `continue` leaves the loop body while
                        // deferred work registered directly in it is still
                        // armed: replay that chain on the way out. The
                        // replay is only this path's, so the chain stands
                        // for the paths that carry on through the body.
                        self.fire_before(module, loop_.defer_base, false);
                        let target = if is_break {
                            loop_.exit
                        } else {
                            loop_.continuation
                        };
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
            Stmt::Block { block, .. } => self.block_body_owning(module, block, &[], true),
            Stmt::Match {
                subject,
                arms,
                span,
            } => {
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
                    self.unsupported(
                        ty.span(),
                        "this type annotation (primitives, pointers and records today)",
                    );
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
                        _ => match value.basic() {
                            Some(basic) => {
                                let _ = self.builder.build_store(slot, basic);
                            }
                            None => self.unsupported(span, "storing this value"),
                        },
                    }
                }
            }
            None => {
                let zero = ty.llvm(self.context).const_zero();
                let _ = self.builder.build_store(slot, zero);
            }
        }
        self.note_slot(&name, ty, slot);
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
            Expr::Ident {
                name,
                span: target_span,
            } => match self.slot_of(name) {
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
                }
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
            Expr::Index { base, index, .. } => match self.place_of(module, base, index, true) {
                Some(place) => place,
                None => return,
            },
            Expr::Field { .. } | Expr::Member { .. } => {
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
            }
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
        self.fire_before(module, 0, false);
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
                    Some(value) => match self.returnable(value, ty) {
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
        // The bindings a pattern condition made live for the `then` arm alone,
        // which is where the checker can see them too.
        self.block_body_owning(module, then_branch, &bound, true);
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
                self.block_body_owning(module, block, &[], true);
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
            self.unsupported(
                span,
                "an array produced by an `if` arm (arrays do not travel as values yet)",
            );
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
        let at_open_block = self
            .defer_blocks
            .last()
            .is_some_and(|(_, depth)| *depth == self.scopes.len());
        if self.scopes.len() != 2 && !at_open_block {
            self.unsupported(
                span,
                "`defer` here (a `defer` belongs at the top of a function or directly inside a block)",
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
        if let Expr::Field { base, name, .. } = callee.as_ref() {
            self.defer_method(module, base, name, args, span);
            return;
        }
        let Expr::Ident { name, .. } = callee.as_ref() else {
            self.unsupported(
                span,
                "deferring a call to anything but a named function or a method",
            );
            return;
        };
        if name == "print" || name == "println" {
            self.defer_write(module, name, args, span);
            return;
        }
        if name == "free" || name == "del" {
            let Some(value) =
                self.expr(module, args.first().unwrap_or(&Expr::Error { span }), None)
            else {
                return;
            };
            if value.ty == DuxType::Error {
                return;
            }
            let Some(slot) = self.capture(module, value, "deferred", span) else {
                return;
            };
            self.defers.push(Deferred::Release { slot, span });
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
        for (index, (parameter, arg)) in signature.params.iter().zip(args.iter()).enumerate() {
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

    /// `defer arena.destroy()`: the receiver and every argument are evaluated
    /// now and kept in slots; only the call waits. A pointer receiver keeps
    /// the address of its place, a value receiver a copy of its own.
    fn defer_method(
        &mut self,
        module: &Module<'static>,
        base: &Expr,
        name: &dux_ast::Ident,
        args: &[Expr],
        span: Span,
    ) {
        let place = self.place_of_expr(module, base);
        let (base_ty, base_at) = match place {
            Some((ty, at)) => (ty, Some(at)),
            None => {
                let Some(value) = self.expr(module, base, None) else {
                    return;
                };
                let at = self.materialize(value, "recv");
                (value.ty, at)
            }
        };
        let owner = match (self.record_of(base_ty), self.enum_of(base_ty)) {
            (Some(record), _) => record.name,
            (None, Some(shape)) => shape.name,
            _ => {
                self.unsupported(
                    base.span(),
                    "deferring a method of anything but a record or a sum type",
                );
                return;
            }
        };
        let key = format!("{owner}.{}", name.name);
        if !self.functions.contains_key(&key) {
            self.unsupported(
                span,
                &format!("deferring `{key}` (an instance made per receiver is not there to defer)"),
            );
            return;
        }
        let Some(signature) = self.signatures.get(key.as_str()).cloned() else {
            self.unsupported(span, &format!("deferring `{key}`"));
            return;
        };
        let Some(want) = signature.receiver else {
            self.unsupported(span, &format!("`{key}` is not a method"));
            return;
        };
        let Some(at) = base_at else {
            self.unsupported(
                base.span(),
                "deferring a call whose receiver has no place to keep",
            );
            return;
        };
        let receiver = match want {
            DuxType::Pointer(target) if base_ty == *target => Typed::pointer(at, want),
            DuxType::Pointer(_)
            | DuxType::Struct(_)
            | DuxType::Enum(_)
            | DuxType::Slice(_)
            | DuxType::Str
            | DuxType::Array(_) => match self.load(base_ty, at) {
                Some(value) if value.ty == want => value,
                _ => {
                    self.unsupported(
                        base.span(),
                        &format!("`{}` cannot take `{}` as its receiver", key, base_ty.name()),
                    );
                    return;
                }
            },
            _ => {
                self.unsupported(span, &format!("deferring `{key}`"));
                return;
            }
        };
        let Some(receiver_slot) = self.capture(module, receiver, "deferrecv", base.span()) else {
            return;
        };
        if args.len() != signature.params.len() {
            self.diagnostics.push(
                Diagnostic::error(
                    code::WRONG_ARITY,
                    format!(
                        "`{key}` takes {} arguments, given {}",
                        signature.params.len(),
                        args.len()
                    ),
                )
                .with_span(span),
            );
            return;
        }
        let mut slots = vec![receiver_slot];
        for (index, (parameter, arg)) in signature.params.iter().zip(args).enumerate() {
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
            name: key,
            slots,
            hidden_context: signature.hidden_context,
        });
    }

    fn defer_write(&mut self, module: &Module<'static>, name: &str, args: &[Expr], span: Span) {
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
        self.fire_before(module, 0, true);
    }

    /// Fire the deferred work registered since `base`, most recent first.
    /// A block on its way out and the end of a function consume what they
    /// fire; a `return` only replays the chain along its own path, leaving
    /// the rest of the stack standing for the paths that did not return.
    fn fire_before(&mut self, module: &Module<'static>, base: usize, consume: bool) {
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
                span: Span,
            },
        }
        for index in (base..self.defers.len()).rev() {
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
                Deferred::Release { slot, span } => Owned::Release {
                    slot: *slot,
                    span: *span,
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
                Owned::Release {
                    slot: (ty, slot),
                    span,
                } => {
                    let memory = match self.load(ty, slot) {
                        Some(value) => value
                            .pointer_value()
                            .or_else(|| self.slice_parts(value).map(|(data, _)| data)),
                        None => None,
                    };
                    let Some(memory) = memory else {
                        continue;
                    };
                    self.call_free(span, memory);
                }
            }
        }
        if consume {
            self.defers.truncate(base);
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
            defer_base: self.defers.len(),
        });
        self.block_body_owning(module, body, &bound, true);
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
            defer_base: self.defers.len(),
        });
        self.block_body_owning(module, body, &[], true);
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
            defer_base: self.defers.len(),
        });
        self.block_body_owning(module, body, &[], true);
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
            None => Some(Typed::int(
                self.context.i64_type().const_zero(),
                DuxType::I64,
            )),
        };
        let end_value = self.expr(module, end_expr, Some(DuxType::I64));
        let (Some(start_int), Some(end_int)) = (
            start_value.and_then(|v| v.int_value()),
            end_value.and_then(|v| v.int_value()),
        ) else {
            self.dead = true;
            return;
        };
        let counter = self.alloca(DuxType::I64, "iter");
        let limit = self.alloca(DuxType::I64, "limit");
        let _ = self.builder.build_store(counter, start_int);
        let _ = self.builder.build_store(limit, end_int);
        let value_slot = if value.name != "_" {
            let slot = self.alloca(DuxType::I64, &value.name);
            self.note_slot(&value.name, DuxType::I64, slot);
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
            defer_base: self.defers.len(),
        });
        self.block_body_owning(module, body, &[], true);
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
                self.note_slot(&bind.name, DuxType::I64, slot);
                if let Some(scope) = self.scopes.last_mut() {
                    scope.insert(bind.name.clone(), (DuxType::I64, slot));
                }
                Some(slot)
            }
            None => None,
        };
        let value_slot = if value.name != "_" {
            let slot = self.alloca(element, &value.name);
            self.note_slot(&value.name, element, slot);
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
                    None => unsafe {
                        self.builder
                            .build_gep(element.llvm(self.context), base, &[i], "elem")
                    },
                };
                if let Ok(at) = at {
                    if let Some(element) = self.load(element, at) {
                        match (element.ty.array_info(), element.core) {
                            (Some(info), Core::Ptr(source)) => self.copy_array(slot, source, info),
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
            defer_base: self.defers.len(),
        });
        self.block_body_owning(module, body, &[], true);
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
        let Some(count) = self.const_length(len) else {
            self.unsupported(
                len.span(),
                "a repeated array whose length is neither a literal nor a `const` parameter this call filled in",
            );
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
            let (_, data, text_len) = self.view_elements(base)?;
            let index_value = self.expr(module, index, None)?;
            if index_value.ty == DuxType::Error {
                return None;
            }
            let index64 = self.index_to_i64(index_value, index.span())?;
            let within = self.index_reaches_into(index64.int_value()?, text_len);
            if let Some(within) = within {
                self.bounds_guard(module, within, index.span(), "byte index");
            }
            let at = unsafe {
                self.builder.build_gep(
                    DuxType::U8.llvm(self.context),
                    data,
                    &[index64.int_value()?],
                    "byte",
                )
            }
            .ok()?;
            return Some((DuxType::U8, at));
        }
        if let Some(element) = base.ty.slice_element() {
            let (data, len) = self.slice_parts(base)?;
            let index_value = self.expr(module, index, None)?;
            if index_value.ty == DuxType::Error {
                return None;
            }
            let index64 = self.index_to_i64(index_value, index.span())?;
            let within = self.index_reaches_into(index64.int_value()?, len);
            if let Some(within) = within {
                self.bounds_guard(module, within, index.span(), "index");
            }
            let at = unsafe {
                self.builder.build_gep(
                    element.llvm(self.context),
                    data,
                    &[index64.int_value()?],
                    "idx",
                )
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
            let count = self.context.i64_type().const_int(info.len, false);
            let within = self.index_reaches_into(index64.int_value()?, count);
            if let Some(within) = within {
                self.bounds_guard(module, within, index.span(), "index");
            }
            let array_ty = info.elem.llvm(self.context).array_type(info.len as u32);
            let zero = self.context.i64_type().const_zero();
            unsafe {
                self.builder
                    .build_gep(array_ty, array, &[zero, index64.int_value()?], "idx")
            }
            .ok()?
        } else {
            unsafe {
                self.builder.build_gep(
                    element.llvm(self.context),
                    array,
                    &[index64.int_value()?],
                    "idx",
                )
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
            let Ok(from) =
                (unsafe { self.builder.build_gep(array_ty, source, &[zero, i], "elem") })
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
            Expr::CondBinding { pattern, value, .. } => {
                self.pattern_condition(module, pattern, value, bound)
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
                if let Expr::Ident { name: place, .. } = base.as_ref() {
                    if self.slot_of(place).is_none()
                        && !self.known(&name.name)
                        && !self.known(&format!("{place}{}", name.name))
                    {
                        if let Some(found) = self.qualified_slot(place, &name.name) {
                            return Some(found);
                        }
                    }
                }
                let (ty, at) = self.place_of_expr(module, base)?;
                let (info, at) = self.record_address(ty, at)?;
                let (index, field_ty) = self.field_of(info, &name.name, name.span)?;
                Some((field_ty, self.field_address(info, at, index)?))
            }
            Expr::Member { base, index, .. } => {
                let (ty, at) = self.place_of_expr(module, base)?;
                let (info, at) = self.tuple_address(ty, at)?;
                let elem = self.tuple_field(info, *index, expr.span())?;
                Some((elem, self.tuple_field_address(info, at, *index)?))
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

    /// `&` of something that is not a place: the value is built standing in a
    /// slot of its own, and that slot is what the view or the pointer aims at.
    fn borrowed_temporary(&mut self, module: &Module<'static>, operand: &Expr) -> Option<Typed> {
        let value = self.expr(module, operand, None)?;
        if value.ty == DuxType::Error {
            return Some(Typed::error());
        }
        if value.ty.array_info().is_some() {
            return self.view_of_array(value);
        }
        if value.ty.slice_element().is_some() {
            return Some(value);
        }
        let at = match value.core {
            Core::Ptr(at) => at,
            _ => {
                let slot = self.alloca(value.ty, "tmp");
                let _ = self.builder.build_store(slot, value.basic()?);
                slot
            }
        };
        Some(Typed::pointer(at, self.pointer_to(value.ty)))
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
    fn view_elements(
        &mut self,
        value: Typed,
    ) -> Option<(DuxType, PointerValue<'static>, IntValue<'static>)> {
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

    /// `.runes()`: the code points the string's UTF-8 bytes spell. The buffer
    /// holds as many runes as there are bytes — no sequence spells more than
    /// one per byte — and the view handed back narrows to what was decoded.
    /// A lead whose continuations are missing writes the replacement rune and
    /// steps one byte; a stray continuation byte belongs to no rune on its
    /// own and steps silently.
    fn decode_runes(&mut self, at: PointerValue<'static>, span: Span) -> Option<Typed> {
        let text = self.load(DuxType::Str, at)?;
        let (data, length) = self.slice_parts(text)?;
        let i64_type = self.context.i64_type();
        let i8_type = self.context.i8_type();
        let i32_type = self.context.i32_type();
        let bytes = self
            .builder
            .build_int_mul(length, i64_type.const_int(4, false), "runes")
            .ok()?;
        let buffer = self.allocate(span, bytes, 4)?;
        let cursor = self.alloca(DuxType::I64, "runes.at");
        let written = self.alloca(DuxType::I64, "runes.written");
        let _ = self.builder.build_store(cursor, i64_type.const_zero());
        let _ = self.builder.build_store(written, i64_type.const_zero());
        let head = self.new_block("runes.head");
        let body = self.new_block("runes.body");
        let exit = self.new_block("runes.done");
        self.br(head);
        self.builder.position_at_end(head);
        self.dead = false;
        let here = self
            .builder
            .build_load(i64_type, cursor, "at")
            .ok()?
            .into_int_value();
        let more = self
            .builder
            .build_int_compare(IntPredicate::ULT, here, length, "more")
            .ok()?;
        let _ = self
            .builder
            .build_conditional_branch(more, body, exit)
            .ok()?;
        self.builder.position_at_end(body);
        let here = self
            .builder
            .build_load(i64_type, cursor, "at")
            .ok()?
            .into_int_value();
        let byte_at = |this: &mut Self, position: IntValue<'static>, name: &str| {
            unsafe { this.builder.build_gep(i8_type, data, &[position], name) }
                .ok()
                .map(|cell| {
                    let byte = this.builder.build_load(i8_type, cell, name).ok()?;
                    this.builder
                        .build_int_cast(byte.into_int_value(), i64_type, "wide")
                        .ok()
                })
        };
        let lead = byte_at(self, here, "byte")??;
        let mask = |this: &mut Self, value: IntValue<'static>, bits: u64| {
            this.builder
                .build_and(value, i64_type.const_int(bits, false), "mask")
                .ok()
        };
        let ascii = self
            .builder
            .build_int_compare(
                IntPredicate::ULT,
                lead,
                i64_type.const_int(0x80, false),
                "ascii",
            )
            .ok()?;
        let one = self.new_block("runes.one");
        let checks = self.new_block("runes.checks");
        let _ = self
            .builder
            .build_conditional_branch(ascii, one, checks)
            .ok()?;
        self.builder.position_at_end(checks);
        let two_head = mask(self, lead, 0xE0)?;
        let is_two = self
            .builder
            .build_int_compare(
                IntPredicate::EQ,
                two_head,
                i64_type.const_int(0xC0, false),
                "two",
            )
            .ok()?;
        let two = self.new_block("runes.two");
        let three_checks = self.new_block("runes.three.checks");
        let _ = self
            .builder
            .build_conditional_branch(is_two, two, three_checks)
            .ok()?;
        self.builder.position_at_end(three_checks);
        let three_head = mask(self, lead, 0xF0)?;
        let is_three = self
            .builder
            .build_int_compare(
                IntPredicate::EQ,
                three_head,
                i64_type.const_int(0xE0, false),
                "three",
            )
            .ok()?;
        let three = self.new_block("runes.three");
        let four_checks = self.new_block("runes.four.checks");
        let _ = self
            .builder
            .build_conditional_branch(is_three, three, four_checks)
            .ok()?;
        self.builder.position_at_end(four_checks);
        let four_head = mask(self, lead, 0xF8)?;
        let is_four = self
            .builder
            .build_int_compare(
                IntPredicate::EQ,
                four_head,
                i64_type.const_int(0xF0, false),
                "four",
            )
            .ok()?;
        let four = self.new_block("runes.four");
        let stray = self.new_block("runes.stray");
        let _ = self
            .builder
            .build_conditional_branch(is_four, four, stray)
            .ok()?;
        // A lead of the wrong shape: a continuation byte with no rune in
        // front of it steps over itself, and anything else no byte can start
        // becomes the replacement.
        self.builder.position_at_end(stray);
        let stray_head = mask(self, lead, 0xC0)?;
        let is_stray = self
            .builder
            .build_int_compare(
                IntPredicate::EQ,
                stray_head,
                i64_type.const_int(0x80, false),
                "stray",
            )
            .ok()?;
        let skip = self.new_block("runes.skip");
        let bogus = self.new_block("runes.bogus");
        let _ = self
            .builder
            .build_conditional_branch(is_stray, skip, bogus)
            .ok()?;
        let step = |this: &mut Self, rune: Option<IntValue<'static>>, after: u64| {
            if let Some(rune) = rune {
                let count = this
                    .builder
                    .build_load(i64_type, written, "written")
                    .ok()?
                    .into_int_value();
                let cell =
                    unsafe { this.builder.build_gep(i32_type, buffer, &[count], "slot") }.ok()?;
                let small = this
                    .builder
                    .build_int_truncate(rune, i32_type, "rune")
                    .ok()?;
                let _ = this.builder.build_store(cell, small).ok()?;
                let grown = this
                    .builder
                    .build_int_add(count, i64_type.const_int(1, false), "wrote")
                    .ok()?;
                let _ = this.builder.build_store(written, grown).ok()?;
            }
            let onward = this
                .builder
                .build_int_add(here, i64_type.const_int(after, false), "next")
                .ok()?;
            let _ = this.builder.build_store(cursor, onward).ok()?;
            this.br(head);
            Some(())
        };
        let replacement = i64_type.const_int(0xFFFD, false);
        self.builder.position_at_end(skip);
        step(self, None, 1)?;
        self.builder.position_at_end(bogus);
        step(self, Some(replacement), 1)?;
        // One ASCII byte is the whole rune.
        self.builder.position_at_end(one);
        step(self, Some(lead), 1)?;
        // Two bytes: `110xxxxx 10xxxxxx`.
        self.builder.position_at_end(two);
        let need = self
            .builder
            .build_int_add(here, i64_type.const_int(1, false), "need")
            .ok()?;
        let value = self.new_block("runes.two.value");
        let bail = self.new_block("runes.two.bail");
        self.runes_continuation(need, length, data, value, bail)?;
        self.builder.position_at_end(value);
        let next = byte_at(self, need, "follow")??;
        let high = self
            .builder
            .build_and(lead, i64_type.const_int(0x1F, false), "high")
            .ok()?;
        let high = self
            .builder
            .build_left_shift(high, i64_type.const_int(6, false), "high")
            .ok()?;
        let low = self
            .builder
            .build_and(next, i64_type.const_int(0x3F, false), "low")
            .ok()?;
        let rune = self.builder.build_or(high, low, "rune").ok()?;
        step(self, Some(rune), 2)?;
        self.builder.position_at_end(bail);
        step(self, Some(replacement), 1)?;
        // Three bytes: `1110xxxx 10xxxxxx 10xxxxxx`.
        self.builder.position_at_end(three);
        let first = self
            .builder
            .build_int_add(here, i64_type.const_int(1, false), "need")
            .ok()?;
        let second = self
            .builder
            .build_int_add(here, i64_type.const_int(2, false), "need")
            .ok()?;
        let value = self.new_block("runes.three.value");
        let bail = self.new_block("runes.three.bail");
        let onward = self.new_block("runes.three.second");
        self.runes_continuation(first, length, data, onward, bail)?;
        self.builder.position_at_end(onward);
        self.runes_continuation(second, length, data, value, bail)?;
        self.builder.position_at_end(value);
        let one_more = byte_at(self, first, "follow")??;
        let two_more = byte_at(self, second, "follow")??;
        let high = self
            .builder
            .build_and(lead, i64_type.const_int(0x0F, false), "high")
            .ok()?;
        let high = self
            .builder
            .build_left_shift(high, i64_type.const_int(12, false), "high")
            .ok()?;
        let middle = self
            .builder
            .build_and(one_more, i64_type.const_int(0x3F, false), "middle")
            .ok()?;
        let middle = self
            .builder
            .build_left_shift(middle, i64_type.const_int(6, false), "middle")
            .ok()?;
        let low = self
            .builder
            .build_and(two_more, i64_type.const_int(0x3F, false), "low")
            .ok()?;
        let rune = self
            .builder
            .build_or(
                self.builder.build_or(high, middle, "rune").ok()?,
                low,
                "rune",
            )
            .ok()?;
        step(self, Some(rune), 3)?;
        self.builder.position_at_end(bail);
        step(self, Some(replacement), 1)?;
        // Four bytes: `11110xxx 10xxxxxx 10xxxxxx 10xxxxxx`.
        self.builder.position_at_end(four);
        let first = self
            .builder
            .build_int_add(here, i64_type.const_int(1, false), "need")
            .ok()?;
        let second = self
            .builder
            .build_int_add(here, i64_type.const_int(2, false), "need")
            .ok()?;
        let third = self
            .builder
            .build_int_add(here, i64_type.const_int(3, false), "need")
            .ok()?;
        let value = self.new_block("runes.four.value");
        let bail = self.new_block("runes.four.bail");
        let onward = self.new_block("runes.four.second");
        let farther = self.new_block("runes.four.third");
        self.runes_continuation(first, length, data, onward, bail)?;
        self.builder.position_at_end(onward);
        self.runes_continuation(second, length, data, farther, bail)?;
        self.builder.position_at_end(farther);
        self.runes_continuation(third, length, data, value, bail)?;
        self.builder.position_at_end(value);
        let one_more = byte_at(self, first, "follow")??;
        let two_more = byte_at(self, second, "follow")??;
        let three_more = byte_at(self, third, "follow")??;
        let high = self
            .builder
            .build_and(lead, i64_type.const_int(0x07, false), "high")
            .ok()?;
        let high = self
            .builder
            .build_left_shift(high, i64_type.const_int(18, false), "high")
            .ok()?;
        let middle = self
            .builder
            .build_and(one_more, i64_type.const_int(0x3F, false), "middle")
            .ok()?;
        let middle = self
            .builder
            .build_left_shift(middle, i64_type.const_int(12, false), "middle")
            .ok()?;
        let lower = self
            .builder
            .build_and(two_more, i64_type.const_int(0x3F, false), "lower")
            .ok()?;
        let lower = self
            .builder
            .build_left_shift(lower, i64_type.const_int(6, false), "lower")
            .ok()?;
        let low = self
            .builder
            .build_and(three_more, i64_type.const_int(0x3F, false), "low")
            .ok()?;
        let rune = self
            .builder
            .build_or(
                self.builder.build_or(high, middle, "rune").ok()?,
                self.builder.build_or(lower, low, "rune").ok()?,
                "rune",
            )
            .ok()?;
        step(self, Some(rune), 4)?;
        self.builder.position_at_end(bail);
        step(self, Some(replacement), 1)?;
        self.builder.position_at_end(exit);
        self.dead = false;
        let count = self
            .builder
            .build_load(i64_type, written, "count")
            .ok()?
            .into_int_value();
        Some(self.view(DuxType::I32, buffer, count))
    }

    /// One continuation byte of a multi-byte rune: where `need` is a
    /// continuation byte the sequence carries on into `holds`; a missing byte
    /// or a byte that stopped being one sends the sequence to `bails`.
    fn runes_continuation(
        &mut self,
        need: IntValue<'static>,
        length: IntValue<'static>,
        data: PointerValue<'static>,
        holds: BasicBlock<'static>,
        bails: BasicBlock<'static>,
    ) -> Option<()> {
        let peek = self.new_block("runes.peek");
        let room = self
            .builder
            .build_int_compare(IntPredicate::ULT, need, length, "room")
            .ok()?;
        let _ = self
            .builder
            .build_conditional_branch(room, peek, bails)
            .ok()?;
        self.builder.position_at_end(peek);
        let cell = unsafe {
            self.builder
                .build_gep(self.context.i8_type(), data, &[need], "peek")
        }
        .ok()?;
        let byte = self
            .builder
            .build_load(self.context.i8_type(), cell, "follow")
            .ok()?;
        let wide = self
            .builder
            .build_int_cast(byte.into_int_value(), self.context.i64_type(), "wide")
            .ok()?;
        let mask = self
            .builder
            .build_and(wide, self.context.i64_type().const_int(0xC0, false), "mask")
            .ok()?;
        let holds_it = self
            .builder
            .build_int_compare(
                IntPredicate::EQ,
                mask,
                self.context.i64_type().const_int(0x80, false),
                "follows",
            )
            .ok()?;
        let _ = self
            .builder
            .build_conditional_branch(holds_it, holds, bails)
            .ok()?;
        Some(())
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
                        if value.ty == DuxType::Error {
                            "this value".to_string()
                        } else {
                            value.ty.name()
                        }
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
            None => Typed {
                core: Core::Int(zero),
                ty: DuxType::I64,
            },
        };
        let to = match end.as_deref() {
            Some(hi) => self.view_bound(module, hi)?,
            None => Typed {
                core: Core::Int(len),
                ty: DuxType::I64,
            },
        };
        let from = from.int_value()?;
        let to = to.int_value()?;
        if self.bounds_checks {
            let in_order = self
                .builder
                .build_int_compare(IntPredicate::ULE, from, to, "order")
                .ok()?;
            let inside = self
                .builder
                .build_int_compare(IntPredicate::ULE, to, len, "inside")
                .ok()?;
            let within = self.builder.build_and(in_order, inside, "within").ok()?;
            self.bounds_guard(module, within, range.span(), "index range");
        }
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
        if let Some((index, (_, ty))) = listed
            .iter()
            .enumerate()
            .find(|(_, (field, _))| *field == name)
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
        unsafe {
            self.builder
                .build_gep(info.llvm, base, &[zero, at], "field")
        }
        .ok()
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
        // `math.LIMIT`: a name that walks off a module is that module's own,
        // and a module's constant is read the same way a bare one is.
        if let Expr::Ident { name: place, .. } = base {
            if self.slot_of(place).is_none() {
                let named = format!("{place}.{name}");
                if let Some(value) = self.consts.get(&named).cloned() {
                    return Some(self.const_reference(&value, None, span));
                }
                if let Some((ty, at)) = self.qualified_slot(place, name) {
                    return self.load(ty, at);
                }
            }
        }
        if let Some((ty, at)) = self.place_of_expr(module, base) {
            let Some((info, at)) = self.record_address(ty, at) else {
                self.unsupported(span, &format!("reading `.{name}` on a `{}`", ty.name()));
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
        self.record_value(module, ty, info, fields, span)
    }

    /// Fill a record's fields in, in the order written. The value lives in a
    /// slot of its own, which is what makes assignment a copy of the whole.
    fn record_value(
        &mut self,
        module: &Module<'static>,
        ty: DuxType,
        info: &'static StructInfo,
        fields: &[(dux_ast::Ident, Expr)],
        span: Span,
    ) -> Option<Typed> {
        let _ = span;
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

    /// The type of an anonymous tuple, interned by its elements: two
    /// `(i32, i64)`s written a thousand places apart are one type.
    fn tuple_type(&mut self, elems: &[DuxType]) -> DuxType {
        let key = format!(
            "({})",
            elems
                .iter()
                .map(|ty| ty.key())
                .collect::<Vec<_>>()
                .join(", ")
        );
        if let Some(info) = self.tuples.get(&key).copied() {
            return DuxType::Tuple(info);
        }
        let spelled = format!(
            "({})",
            elems
                .iter()
                .map(|ty| ty.name())
                .collect::<Vec<_>>()
                .join(", ")
        );
        let fields: Vec<BasicTypeEnum<'static>> =
            elems.iter().map(|ty| ty.llvm(self.context)).collect();
        let llvm = self.context.struct_type(&fields, false);
        let info: &'static TupleInfo = Box::leak(Box::new(TupleInfo {
            spelled,
            elems: elems.to_vec(),
            llvm,
        }));
        self.tuples.insert(key, info);
        DuxType::Tuple(info)
    }

    /// The tuple a place reaches and the address of that tuple. A place of
    /// tuple type is the tuple; a place of `*T` type holds the address, so
    /// `p.0` reads the pointer out before stepping into the target.
    fn tuple_address(
        &mut self,
        ty: DuxType,
        at: PointerValue<'static>,
    ) -> Option<(&'static TupleInfo, PointerValue<'static>)> {
        match ty {
            DuxType::Tuple(info) => Some((info, at)),
            DuxType::Pointer(target) => {
                let DuxType::Tuple(info) = *target else {
                    return None;
                };
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

    /// `t.{index}`, with the out-of-range diagnostic in one place.
    fn tuple_field(&mut self, info: &'static TupleInfo, index: u32, span: Span) -> Option<DuxType> {
        match info.elems.get(index as usize) {
            Some(ty) => Some(*ty),
            None => {
                self.diagnostics.push(
                    Diagnostic::error(
                        code::TYPE_MISMATCH,
                        format!("`{}` has no member `.{index}`", info.spelled),
                    )
                    .with_span(span)
                    .with_note(format!(
                        "the members are .0..{}",
                        info.elems.len().saturating_sub(1)
                    )),
                );
                None
            }
        }
    }

    fn tuple_field_address(
        &self,
        info: &'static TupleInfo,
        base: PointerValue<'static>,
        index: u32,
    ) -> Option<PointerValue<'static>> {
        let zero = self.context.i32_type().const_zero();
        let at = self.context.i32_type().const_int(index as u64, false);
        unsafe {
            self.builder
                .build_gep(info.llvm, base, &[zero, at], "member")
        }
        .ok()
    }

    /// `(a, b)`: the elements are built into a slot of the tuple's own,
    /// which is what makes assignment a copy of the whole.
    fn tuple_literal(
        &mut self,
        module: &Module<'static>,
        elements: &[Expr],
        expected: Option<DuxType>,
        span: Span,
    ) -> Option<Typed> {
        let wanted: Vec<Option<DuxType>> = match expected {
            Some(DuxType::Tuple(info)) if info.elems.len() == elements.len() => {
                info.elems.iter().map(|ty| Some(*ty)).collect()
            }
            _ => vec![None; elements.len()],
        };
        let mut values = Vec::new();
        for (element, want) in elements.iter().zip(wanted.iter()) {
            let value = self.expr(module, element, *want)?;
            if value.ty == DuxType::Error {
                return Some(Typed::error());
            }
            let value = match want {
                Some(want) => self.coerce_implicit(module, value, *want, element.span())?,
                None => value,
            };
            values.push(value);
        }
        let elems: Vec<DuxType> = values.iter().map(|value| value.ty).collect();
        let ty = self.tuple_type(&elems);
        let DuxType::Tuple(info) = ty else {
            return None;
        };
        let slot = self.alloca(ty, "tuple");
        let _ = self
            .builder
            .build_store(slot, info.llvm.const_zero())
            .ok()?;
        for (index, value) in values.iter().enumerate() {
            let at = self.tuple_field_address(info, slot, index as u32)?;
            match (info.elems[index].array_info(), value.core) {
                (Some(array), Core::Ptr(source)) => self.copy_array(at, source, array),
                (None, _) => {
                    let Some(basic) = value.basic() else {
                        self.unsupported(span, "storing this value in a tuple");
                        return None;
                    };
                    let _ = self.builder.build_store(at, basic).ok()?;
                }
                (Some(_), _) => {
                    self.unsupported(span, "storing this array value in a tuple");
                    return None;
                }
            }
        }
        self.load(ty, slot)
    }

    /// `t.0`: a positional read. A place reaches its member directly; a
    /// value the program just built is stood in a slot of its own first.
    fn member_expr(
        &mut self,
        module: &Module<'static>,
        base: &Expr,
        index: u32,
        span: Span,
    ) -> Option<Typed> {
        if let Some((ty, at)) = self.place_of_expr(module, base) {
            if let Some((info, at)) = self.tuple_address(ty, at) {
                let elem = self.tuple_field(info, index, span)?;
                let at = self.tuple_field_address(info, at, index)?;
                return self.load(elem, at);
            }
        }
        let value = self.expr(module, base, None)?;
        let DuxType::Tuple(info) = value.ty else {
            self.unsupported(
                span,
                &format!("reading `.{index}` on a `{}`", value.ty.name()),
            );
            return None;
        };
        let elem = self.tuple_field(info, index, span)?;
        let slot = self.alloca(value.ty, "tuple");
        let _ = self.builder.build_store(slot, value.basic()?).ok()?;
        let at = self.tuple_field_address(info, slot, index)?;
        self.load(elem, at)
    }

    /// `x?`: the value that succeeded, unwrapped where the question mark
    /// stands. The failure walks out of the enclosing function the way an
    /// explicit `return Err(…)` would, defers firing on its way out.
    fn try_expr(&mut self, module: &Module<'static>, operand: &Expr, span: Span) -> Option<Typed> {
        let value = self.expr(module, operand, None)?;
        if value.ty == DuxType::Error {
            return Some(Typed::error());
        }
        let DuxType::Enum(info) = value.ty else {
            self.diagnostics.push(
                Diagnostic::error(
                    code::TYPE_MISMATCH,
                    format!("`?` walks `Result` and `Option`, not `{}`", value.ty.name()),
                )
                .with_span(span),
            );
            return None;
        };
        let answer = self.result_ty?;
        let DuxType::Enum(outer) = answer else {
            self.diagnostics.push(
                Diagnostic::error(
                    code::TYPE_MISMATCH,
                    format!(
                        "`?` cannot hand a `{}` back from this function",
                        answer.name()
                    ),
                )
                .with_span(span),
            );
            return None;
        };
        let (failure, success) = match info.name {
            "Result" => ("Err", "Ok"),
            "Option" => ("None", "Some"),
            _ => {
                self.unsupported(span, &format!("propagating a `{}`", info.name));
                return None;
            }
        };
        if outer.name != info.name {
            self.diagnostics.push(
                Diagnostic::error(
                    code::TYPE_MISMATCH,
                    format!(
                        "`?` hands back a `{}`, but this function returns a `{}`",
                        info.name, outer.name
                    ),
                )
                .with_span(span),
            );
            return None;
        }
        let (bad_index, bad) = info.variant(failure)?;
        let (_, good) = info.variant(success)?;
        let (outer_bad_index, outer_bad) = outer.variant(failure)?;
        if bad.fields.len() != outer_bad.fields.len() {
            self.diagnostics.push(
                Diagnostic::error(
                    code::TYPE_MISMATCH,
                    format!(
                        "`{}`::{failure} carries {} value(s); `{}`::{failure} takes {}",
                        info.name,
                        bad.fields.len(),
                        outer.name,
                        outer_bad.fields.len()
                    ),
                )
                .with_span(span),
            );
            return None;
        }
        for ((_, mine), (_, wants)) in bad.fields.iter().zip(outer_bad.fields.iter()) {
            if mine != wants {
                self.diagnostics.push(
                    Diagnostic::error(
                        code::TYPE_MISMATCH,
                        format!(
                            "`?` hands back a `{}`; this function carries a `{}`",
                            mine.name(),
                            wants.name()
                        ),
                    )
                    .with_span(span),
                );
                return None;
            }
        }
        let slot = self.alloca(value.ty, "try");
        let _ = self.builder.build_store(slot, value.basic()?).ok()?;
        let holds_failure = self.variant_holds(info, bad_index, slot)?;
        let happy = self.new_block("try.happy");
        let bail = self.new_block("try.bail");
        let _ = self
            .builder
            .build_conditional_branch(holds_failure, bail, happy)
            .ok()?;
        self.builder.position_at_end(bail);
        self.dead = false;
        self.fire_before(module, 0, false);
        let out = self.alloca(answer, "propagate");
        if let Some(carrier) = outer.niche {
            if outer_bad_index != carrier {
                let null = self.context.ptr_type(AddressSpace::default()).const_zero();
                let _ = self.builder.build_store(self.byte_at(out, 0), null).ok()?;
            }
        } else {
            let zero = outer.llvm.into_struct_type().const_zero();
            let _ = self.builder.build_store(out, zero).ok()?;
            let tag = outer.tag.const_int(outer_bad_index as u64, false);
            let _ = self
                .builder
                .build_store(self.byte_at(out, outer.tag_at), tag)
                .ok()?;
        }
        for (position, (_, field_ty)) in bad.fields.iter().enumerate() {
            let from = self.byte_at(slot, bad.offsets[position]);
            let into = self.byte_at(out, outer_bad.offsets[position]);
            if let Some(array) = field_ty.array_info() {
                self.copy_array(into, from, array);
            } else {
                let held = self
                    .builder
                    .build_load(field_ty.llvm(self.context), from, "carry")
                    .ok()?;
                let _ = self.builder.build_store(into, held).ok()?;
            }
        }
        let handed = self.load(answer, out)?;
        match self.returnable(handed, answer) {
            Some(basic) => {
                let _ = self.builder.build_return(Some(&basic)).ok()?;
            }
            None => {
                self.unsupported(span, "returning this through `?`");
                let zero = answer.llvm(self.context).const_zero();
                let _ = self.builder.build_return(Some(&zero)).ok()?;
            }
        }
        self.dead = true;
        self.builder.position_at_end(happy);
        self.dead = false;
        let Some(((_, field_ty), offset)) = good.fields.first().zip(good.offsets.first()) else {
            self.diagnostics.push(
                Diagnostic::error(
                    code::TYPE_MISMATCH,
                    format!("`{}::{success}` carries no value to unwrap", info.name),
                )
                .with_span(span),
            );
            return None;
        };
        self.load(*field_ty, self.byte_at(slot, *offset))
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

    /// A `DISubprogram` per function, with its real signature: every
    /// parameter and the answer are described by the type DIEs the lowering
    /// resolves, so a debugger reads a function as the program wrote it.
    fn begin_function_debug(
        &mut self,
        function: &FnDecl,
        linkage_name: &str,
        fn_value: FunctionValue<'static>,
    ) {
        let Some(unit) = self.compile_unit else {
            return;
        };
        if self.dibuilder.is_none() {
            return;
        }
        let file = self.di_file(function.name.span.file);
        let (line, _) = self
            .sources
            .line_col(function.name.span.file, function.name.span.start);
        let mut params = Vec::new();
        let mut exact = true;
        for param in &function.params {
            match self.type_of(&param.ty).and_then(|ty| self.di_type(ty)) {
                Some(ty) => params.push(ty),
                None => exact = false,
            }
        }
        let return_type: Option<Option<inkwell::debug_info::DIType<'static>>> =
            match function.return_ty.as_ref() {
                None => Some(None),
                Some(ty) => match self.type_of(ty) {
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
        self.di_scope_file = Some(file);
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
        self.dibuilder.as_ref()?;
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
            DuxType::C(kind) => (
                kind.spelled(),
                kind.width() as u64,
                if kind.signed() {
                    DW_ATE_SIGNED
                } else {
                    DW_ATE_UNSIGNED
                },
            ),
            DuxType::F32 => ("f32", 32, DW_ATE_FLOAT),
            DuxType::F64 => ("f64", 64, DW_ATE_FLOAT),
            DuxType::CLongDouble => ("c_long_double", 128, DW_ATE_FLOAT),
            _ => {
                let key = ty.key();
                if let Some(found) = self.di_types.get(&key).copied() {
                    return Some(found);
                }
                if !self.di_under_construction.insert(key.clone()) {
                    return None;
                }
                let built = self.di_derived(ty);
                self.di_under_construction.remove(&key);
                if let Some(built) = built {
                    self.di_types.insert(key, built);
                }
                return built;
            }
        };
        let created = match &self.dibuilder {
            Some(dibuilder) => dibuilder
                .create_basic_type(name, bits, encoding, DIFlags::ZERO)
                .ok()
                .map(|ty| ty.as_type()),
            None => None,
        };
        created
    }

    /// The shapes above the primitives: records with their fields and byte
    /// offsets, tuples by position, arrays and slices, pointers whose pointee
    /// is a type too, sum types told as the tag-plus-payload they really
    /// are, and function pointers with their signature. A record imported
    /// from C answers by its own name — `point`, not `demo.point` — because
    /// a debugger is showing the user the program, not the linker's table.
    fn di_derived(&mut self, ty: DuxType) -> Option<inkwell::debug_info::DIType<'static>> {
        let scope = self.compile_unit?.as_debug_info_scope();
        let data = TargetData::create(&self.layout);
        match ty {
            DuxType::Ptr => {
                let void = self.di_void()?;
                Some(
                    self.dibuilder
                        .as_ref()?
                        .create_pointer_type("rawptr", void, 64, 64, AddressSpace::default())
                        .as_type(),
                )
            }
            DuxType::Pointer(target) => {
                let pointee = self.di_type(*target).or_else(|| self.di_void())?;
                Some(
                    self.dibuilder
                        .as_ref()?
                        .create_pointer_type("", pointee, 64, 64, AddressSpace::default())
                        .as_type(),
                )
            }
            DuxType::Str => {
                let (file, line) = self.di_decl_site("");
                self.di_pair("string", file, line, "data", "length")
            }
            DuxType::Slice(_) => {
                let (file, line) = self.di_decl_site("");
                let name = ty.name();
                self.di_pair(&name, file, line, "data", "length")
            }
            DuxType::Array(info) => {
                let element = self.di_type(info.elem)?;
                let element_llvm = self.llvm_of(info.elem);
                let size = data.get_abi_size(&element_llvm) * info.len;
                let align = data.get_abi_alignment(&element_llvm);
                let bounds = 0..info.len as i64;
                let subscripts = [bounds];
                Some(
                    self.dibuilder
                        .as_ref()?
                        .create_array_type(element, size, align, &subscripts)
                        .as_type(),
                )
            }
            DuxType::Tuple(info) => {
                let elems = info.elems.clone();
                let llvm: BasicTypeEnum = info.llvm.into();
                let (file, line) = self.di_decl_site("");
                let size = data.get_abi_size(&llvm) * 8;
                let align = data.get_abi_alignment(&llvm);
                let mut members: Vec<inkwell::debug_info::DIType<'static>> = Vec::new();
                for (index, elem) in elems.iter().enumerate() {
                    let index = index as u32;
                    let Some(elem_dity) = self.di_type(*elem) else {
                        continue;
                    };
                    let at = data.offset_of_element(&info.llvm, index).unwrap_or(0) * 8;
                    let elem_size = data.get_abi_size(&self.llvm_of(*elem)) * 8;
                    let elem_align = data.get_abi_alignment(&self.llvm_of(*elem));
                    members.push(self.di_member(
                        &format!("{index}"),
                        (file, line),
                        elem_size,
                        elem_align,
                        at,
                        elem_dity,
                    )?);
                }
                Some(
                    self.dibuilder
                        .as_ref()?
                        .create_struct_type(
                            scope,
                            &ty.name(),
                            file,
                            line,
                            size,
                            align,
                            DIFlags::ZERO,
                            None,
                            &members,
                            0,
                            None,
                            "",
                        )
                        .as_type(),
                )
            }
            DuxType::Struct(info) => {
                let fields = self.struct_field_list(info).to_vec();
                let site = info.name.to_string();
                let written = di_written_name(info.name);
                let (file, line) = self.di_decl_site(&site);
                let llvm: BasicTypeEnum = info.llvm.into();
                let size = data.get_abi_size(&llvm) * 8;
                let align = data.get_abi_alignment(&llvm);
                let mut members: Vec<inkwell::debug_info::DIType<'static>> = Vec::new();
                for (index, (field, field_ty)) in fields.iter().enumerate() {
                    let index = index as u32;
                    let Some(field_dity) = self.di_type(*field_ty) else {
                        continue;
                    };
                    let at = data.offset_of_element(&info.llvm, index).unwrap_or(0) * 8;
                    let field_llvm = self.llvm_of(*field_ty);
                    members.push(self.di_member(
                        field,
                        (file, line),
                        data.get_abi_size(&field_llvm) * 8,
                        data.get_abi_alignment(&field_llvm),
                        at,
                        field_dity,
                    )?);
                }
                let _ = spelled;
                Some(
                    self.dibuilder
                        .as_ref()?
                        .create_struct_type(
                            scope,
                            written,
                            file,
                            line,
                            size,
                            align,
                            DIFlags::ZERO,
                            None,
                            &members,
                            0,
                            None,
                            "",
                        )
                        .as_type(),
                )
            }
            DuxType::Enum(info) => {
                let written = di_written_name(info.name).to_string();
                let (file, line) = self.di_decl_site(info.name);
                let llvm = info.llvm;
                let size = data.get_abi_size(&llvm) * 8;
                let align = data.get_abi_alignment(&llvm);
                let niche = !matches!(llvm, BasicTypeEnum::StructType(_));
                let mut members: Vec<inkwell::debug_info::DIType<'static>> = Vec::new();
                if niche {
                    // The pointer that is the sum type doubles as its own
                    // tag; the debugger sees the address, and the variants
                    // keep their names through the struct below.
                } else {
                    let tag_dity = self.di_type(DuxType::U16)?;
                    members.push(self.di_member(
                        "tag",
                        (file, line),
                        16,
                        2,
                        info.tag_at * 8,
                        tag_dity,
                    )?);
                }
                for variant in &info.variants {
                    if variant.fields.is_empty() {
                        continue;
                    }
                    let name = variant.name.to_string();
                    let fields = variant.fields.clone();
                    let offsets = variant.offsets.clone();
                    let at_base = if niche {
                        0
                    } else {
                        *offsets.first().unwrap_or(&0)
                    };
                    let mut field_types: Vec<(
                        String,
                        inkwell::debug_info::DIType<'static>,
                        u64,
                        u64,
                        u32,
                    )> = Vec::new();
                    for (at, (field, field_ty)) in fields.iter().enumerate() {
                        let Some(field_dity) = self.di_type(*field_ty) else {
                            continue;
                        };
                        let field_llvm = self.llvm_of(*field_ty);
                        field_types.push((
                            field.to_string(),
                            field_dity,
                            *offsets.get(at).unwrap_or(&0) * 8 - at_base * 8,
                            data.get_abi_size(&field_llvm) * 8,
                            data.get_abi_alignment(&field_llvm),
                        ));
                    }
                    let mut member_types: Vec<inkwell::debug_info::DIType<'static>> = Vec::new();
                    for (field, field_dity, at, field_size, field_align) in field_types {
                        member_types.push(self.di_member(
                            &field,
                            (file, line),
                            field_size,
                            field_align,
                            at,
                            field_dity,
                        )?);
                    }
                    let payload = self.dibuilder.as_ref()?.create_struct_type(
                        scope,
                        &format!("{written}::{name}"),
                        file,
                        line,
                        size - at_base * 8,
                        align,
                        DIFlags::ZERO,
                        None,
                        &member_types,
                        0,
                        None,
                        "",
                    );
                    members.push(self.di_member(
                        &name,
                        (file, line),
                        size - at_base * 8,
                        align,
                        at_base * 8,
                        payload.as_type(),
                    )?);
                }
                Some(
                    self.dibuilder
                        .as_ref()?
                        .create_struct_type(
                            scope,
                            &written,
                            file,
                            line,
                            size,
                            align,
                            DIFlags::ZERO,
                            None,
                            &members,
                            0,
                            None,
                            "",
                        )
                        .as_type(),
                )
            }
            DuxType::Fn(_) => {
                let name = ty.name();
                Some(
                    self.dibuilder
                        .as_ref()?
                        .create_basic_type(&name, 64, DW_ATE_ADDRESS, DIFlags::ZERO)
                        .ok()?
                        .as_type(),
                )
            }
            _ => None,
        }
    }

    /// The `{ data, length }` shape a `string` and a `[]T` share.
    fn di_pair(
        &mut self,
        name: &str,
        file: DIFile<'static>,
        line: u32,
        first: &str,
        second: &str,
    ) -> Option<inkwell::debug_info::DIType<'static>> {
        let scope = self.compile_unit?.as_debug_info_scope();
        let void = self.di_void()?;
        let pointer = self
            .dibuilder
            .as_ref()?
            .create_pointer_type("", void, 64, 64, AddressSpace::default())
            .as_type();
        let length = self.di_type(DuxType::U64)?;
        let data_member = self.di_member(first, (file, line), 64, 64, 0, pointer)?;
        let length_member = self.di_member(second, (file, line), 64, 64, 64, length)?;
        Some(
            self.dibuilder
                .as_ref()?
                .create_struct_type(
                    scope,
                    name,
                    file,
                    line,
                    128,
                    64,
                    DIFlags::ZERO,
                    None,
                    &[data_member, length_member],
                    0,
                    None,
                    "",
                )
                .as_type(),
        )
    }

    fn di_member(
        &self,
        name: &str,
        place: (DIFile<'static>, u32),
        size_bits: u64,
        align_bits: u32,
        offset_bits: u64,
        ty: inkwell::debug_info::DIType<'static>,
    ) -> Option<inkwell::debug_info::DIType<'static>> {
        let scope = self.compile_unit?.as_debug_info_scope();
        let (file, line) = place;
        Some(
            self.dibuilder
                .as_ref()?
                .create_member_type(
                    scope,
                    name,
                    file,
                    line,
                    size_bits,
                    align_bits,
                    offset_bits,
                    DIFlags::ZERO,
                    ty,
                )
                .as_type(),
        )
    }

    /// The void a pointer to nothing points at, and the void a function
    /// without an answer returns.
    fn di_void(&self) -> Option<inkwell::debug_info::DIType<'static>> {
        Some(
            self.dibuilder
                .as_ref()?
                .create_basic_type("void", 0, DW_ATE_ADDRESS, DIFlags::ZERO)
                .ok()?
                .as_type(),
        )
    }

    /// Where a type was declared, for the DIE that speaks of it. A type the
    /// program never declared — a slice, a tuple, the view inside a
    /// `string` — belongs to the first file, where the program is.
    fn di_decl_site(&mut self, name: &str) -> (DIFile<'static>, u32) {
        if let Some(span) = self.di_decl_sites.get(name).copied() {
            let file = self.di_file(span.file);
            let (line, _) = self.sources.line_col(span.file, span.start);
            return (file, line as u32);
        }
        let file = self.di_file(self.sources.first_file().unwrap_or_default());
        (file, 0)
    }

    /// The LLVM shape a Dux type has, for the layout questions a DIE asks.
    fn llvm_of(&self, ty: DuxType) -> BasicTypeEnum<'static> {
        ty.llvm(self.context)
    }

    /// A slot a name lives in gets a debugger's blessing too: the name,
    /// its type and where the value rests. A name whose type has no DIE
    /// yet, or a slot bound where no debug location stands, passes
    /// unannounced — the program runs the same either way.
    fn note_slot(&mut self, name: &str, ty: DuxType, storage: PointerValue<'static>) {
        if name == "_" {
            return;
        }
        let Some(location) = self.builder.get_current_debug_location() else {
            return;
        };
        let Some(di_type) = self.di_type(ty) else {
            return;
        };
        let Some(file) = self.di_scope_file else {
            return;
        };
        let Some(scope) = self.scope else {
            return;
        };
        let Some(block) = self.allocas else {
            return;
        };
        let Some(dibuilder) = &self.dibuilder else {
            return;
        };
        let variable = dibuilder.create_auto_variable(
            scope.as_debug_info_scope(),
            name,
            file,
            location.get_line(),
            di_type,
            true,
            DIFlags::ZERO,
            0,
        );
        let expression = dibuilder.create_expression(Vec::new());
        unsafe {
            inkwell::llvm_sys::debuginfo::LLVMDIBuilderInsertDeclareRecordAtEnd(
                dibuilder.as_mut_ptr(),
                storage.as_value_ref(),
                variable.as_mut_ptr(),
                expression.as_mut_ptr(),
                location.as_mut_ptr(),
                block.as_mut_ptr(),
            );
        }
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
                self.context
                    .i32_type()
                    .const_int(*value as u32 as u64, false),
                DuxType::I32,
            )),
            Expr::Str { text, kind, span } => Some(self.string_literal(text, *kind, *span)),
            Expr::Ident { name, span } => {
                if let Some((ty, slot)) = self.slot_of(name) {
                    return self.load(ty, slot);
                }
                let named = self.named_in_home(name);
                if let Some(c) = self.consts.get(name).cloned().or_else(|| {
                    self.consts
                        .get(named.as_str())
                        .cloned()
                        .filter(|_| named != *name)
                }) {
                    return Some(self.const_reference(&c, expected, *span));
                }
                // A `const` parameter is a value the body may name, and this
                // is the instantiation that says which one.
                if let Some(Stood::Const(value)) = self.stands_for(name) {
                    let count = self.context.i64_type().const_int(value, false);
                    return Some(Typed::int(count, DuxType::U64));
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
            } => self.binary(module, *op, lhs, rhs, *span, expected),
            Expr::Cast { value, target, .. } => self.cast(module, value, target),
            Expr::Call { callee, args, span } => self.call(module, callee, args, *span, expected),
            Expr::Path {
                qualifier,
                name,
                span,
                ..
            } => {
                if self.shapes.contains_key(qualifier) {
                    self.variant_value(module, qualifier, name, &[], &[], *span, expected)
                } else {
                    self.unsupported(*span, expression_name(expr));
                    None
                }
            }
            Expr::Match {
                subject,
                arms,
                span,
            } => self.matching(module, subject, arms, expected, true, *span),
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
            Expr::ArrayLiteral { elements, span, .. } => {
                self.array_literal(module, elements, expected, *span)
            }
            Expr::Tuple { elements, span } => self.tuple_literal(module, elements, expected, *span),
            Expr::Member { base, index, span } => self.member_expr(module, base, *index, *span),
            Expr::Try { operand, span } => self.try_expr(module, operand, *span),
            Expr::RepeatedArray { value, len, span } => {
                self.repeated_array(module, value, len, expected, *span)
            }
            Expr::Index {
                base, index, span, ..
            } => self.index_expr(module, base, index, *span),
            Expr::Field { base, name, span } => {
                self.field_expr(module, base, &name.name, name.span, *span)
            }
            Expr::StructLiteral {
                name,
                args,
                fields,
                span,
            } => {
                let named = self.named_in_home(name);
                if let Some(record) = self.generic_structs.get(named.as_str()).copied() {
                    return self.generic_record_value(module, record, args, fields, *span);
                }
                if self.structs.contains_key(named.as_str()) || !name.contains("::") {
                    return self.struct_literal(module, &named, fields, *span);
                }
                let (owner, variant) = name.split_once("::").unwrap_or((name.as_str(), ""));
                let given: Vec<(&str, &Expr)> = fields
                    .iter()
                    .map(|(field, value)| (field.name.as_str(), value))
                    .collect();
                self.variant_value(module, owner, variant, &[], &given, *span, expected)
            }
            Expr::InterpString { parts, .. } => self.build_text(module, parts, &[], expr.span()),
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
            core: Core::Aggregate(
                self.context
                    .const_struct(&[bytes.into(), length.into()], false),
            ),
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
                    value,
                    span: literal_span,
                    ..
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
                if let Expr::Ident { name, .. } = operand {
                    let named = self.named_in_home(name);
                    if self.slot_of(name).is_none() && self.signatures.contains_key(&named) {
                        return self.function_address(operand, span);
                    }
                }
                match self
                    .borrowed(module, operand, span)
                    .or_else(|| self.borrowed_temporary(module, operand))
                {
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
                }
            }
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
        self.builder.build_memcpy(data, 1, head, 1, head_len).ok()?;
        let joined = unsafe {
            self.builder.build_gep(
                DuxType::U8.llvm(self.context),
                data,
                &[head_len],
                "text.tail",
            )
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
            .build_call(
                memcmp,
                &[left.into(), right.into(), prefix.into()],
                "text.cmp",
            )
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
        expected: Option<DuxType>,
    ) -> Option<Typed> {
        if matches!(op, BinaryOp::And | BinaryOp::Or) {
            return self.short_circuit(module, op, lhs, rhs);
        }
        // Untyped constants keep their untyped-ness through arithmetic (D17),
        // so `4 * 1024` handed to a `usize` is still the number, not an `i32`
        // the conversion has to forgive.
        if let (Some(left), Some(right)) = (self.literal_integer(lhs), self.literal_integer(rhs)) {
            let folded = match op {
                BinaryOp::Add => left.checked_add(right),
                BinaryOp::Sub => left.checked_sub(right),
                BinaryOp::Mul => left.checked_mul(right),
                BinaryOp::Div => left.checked_div(right),
                BinaryOp::Mod => left.checked_rem(right),
                _ => None,
            };
            if let Some(value) = folded {
                return Some(self.constant_integer(value, expected, span));
            }
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
            self.unsupported(span, &format!("this operator on `{}` values", bad.name()));
            return None;
        }
        self.binary_typed(module, op, left, right, span)
    }

    /// The number an integer literal expression denotes: a literal, a
    /// parenthesis, a negation, or arithmetic over those — what an untyped
    /// constant folds from (D17). Anything else is not literal.
    fn literal_integer(&self, expr: &Expr) -> Option<i128> {
        match expr {
            Expr::Int { value, .. } => i128::try_from(*value).ok(),
            Expr::Paren { inner, .. } => self.literal_integer(inner),
            Expr::Unary {
                op: UnaryOp::Neg,
                operand,
                ..
            } => self.literal_integer(operand)?.checked_neg(),
            Expr::Binary { op, lhs, rhs, .. } => {
                let left = self.literal_integer(lhs)?;
                let right = self.literal_integer(rhs)?;
                match op {
                    BinaryOp::Add => left.checked_add(right),
                    BinaryOp::Sub => left.checked_sub(right),
                    BinaryOp::Mul => left.checked_mul(right),
                    BinaryOp::Div => left.checked_div(right),
                    BinaryOp::Mod => left.checked_rem(right),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// A folded number written the way the literal would have been written:
    /// in the type asked for (D17), or in the smallest one that holds it.
    fn constant_integer(&mut self, value: i128, expected: Option<DuxType>, span: Span) -> Typed {
        let unsigned = value.unsigned_abs();
        if let Some(to) = expected.filter(|ty| ty.is_integral() || *ty == DuxType::Bool) {
            if to.fits(unsigned, value < 0) {
                let constant = to.int_type(self.context).const_int(value as u64, value < 0);
                return Typed::int(constant, to);
            }
            self.diagnostics.push(
                Diagnostic::error(
                    code::LITERAL_OUT_OF_RANGE,
                    format!("the constant {value} does not fit a `{}`", to.name()),
                )
                .with_span(span),
            );
            return Typed::error();
        }
        if matches!(expected, Some(DuxType::F32) | Some(DuxType::F64)) {
            let ty = if expected == Some(DuxType::F32) {
                DuxType::F32
            } else {
                DuxType::F64
            };
            let constant = ty.float_type(self.context).const_float(value as f64);
            return Typed {
                core: Core::Float(constant),
                ty,
            };
        }
        let ty = if i32::MIN as i128 <= value && value <= i32::MAX as i128 {
            DuxType::I32
        } else if i64::MIN as i128 <= value && value <= i64::MAX as i128 {
            DuxType::I64
        } else {
            DuxType::U64
        };
        let constant = ty.int_type(self.context).const_int(value as u64, value < 0);
        Typed::int(constant, ty)
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
                .build_int_compare(predicate, left.int_value()?, right.int_value()?, "cmptmp")
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
                    .build_int_compare(predicate, left.int_value()?, right.int_value()?, "cmptmp")
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
        let (pointer, count, negative) =
            match (left.ty.is_pointer_like(), right.ty.is_pointer_like()) {
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
    fn step(&mut self, pointer: Typed, count: Typed, negative: bool, span: Span) -> Option<Typed> {
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
                    format!(
                        "pointer arithmetic steps by an integer, not a `{}`",
                        count.ty.name()
                    ),
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
        let left = self.condition(module, lhs);
        // `left` lives wherever its evaluation came to rest: a bounds check
        // inside the left condition opens a fresh block, and the branch that
        // consumes `left` is written there, so that block is the predecessor
        // the phi sees — not the block the short-circuit began in.
        let left_block = self.builder.get_insert_block()?;
        let rhs_block = self.new_block("sc.rhs");
        let done = self.new_block("sc.done");
        if op == BinaryOp::And {
            let _ = self.builder.build_conditional_branch(left, rhs_block, done);
        } else {
            let _ = self.builder.build_conditional_branch(left, done, rhs_block);
        }
        self.builder.position_at_end(rhs_block);
        let right = self.condition(module, rhs);
        let right_block = self.builder.get_insert_block()?;
        self.br(done);
        self.builder.position_at_end(done);
        let phi = self
            .builder
            .build_phi(self.context.bool_type(), "sc.result")
            .ok()?;
        phi.add_incoming(&[(&left, left_block), (&right, right_block)]);
        Some(Typed::int(
            phi.as_basic_value().into_int_value(),
            DuxType::Bool,
        ))
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
        if value.ty != DuxType::Ptr && !matches!(value.ty, DuxType::Pointer(_))
            || to != DuxType::Str
        {
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
                    value.ty.key(),
                    to.key(),
                    to.key()
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
            // D18: the C ladder and Dux's ladder are separate.
            if matches!(from, DuxType::C(_)) != matches!(to, DuxType::C(_)) {
                return false;
            }
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
        // C's widest float holds every IEEE value the language has (D25).
        if from.is_float() && to == DuxType::CLongDouble {
            return true;
        }
        if from == DuxType::Ptr && to == DuxType::Str {
            return true;
        }
        // An address is its own `Option`'s `Some`: the niche carries it, and
        // nothing about the word changes. A string handed to C is its
        // bytes' address, which is as good a `Some` as any.
        if from.is_pointer_like() && to.is_address_word() {
            return true;
        }
        if from == DuxType::Str && to.is_address_word() {
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
        // Every pointer is the same word; a target is something the program
        // says *about* the address, not a different address.
        if from.is_pointer_like() && to.is_address_word() {
            return Some(Typed {
                core: value.core,
                ty: to,
            });
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
                    .build_signed_int_to_float(
                        value.int_value()?,
                        to.float_type(self.context),
                        "itofp",
                    )
                    .ok()?
            } else {
                self.builder
                    .build_unsigned_int_to_float(
                        value.int_value()?,
                        to.float_type(self.context),
                        "itofp",
                    )
                    .ok()?
            };
            return Some(Typed::float(cast, to));
        }
        if from.is_float() && to.is_integral() {
            let cast = if to.is_signed() {
                self.builder
                    .build_float_to_signed_int(
                        value.float_value()?,
                        to.int_type(self.context),
                        "fptoi",
                    )
                    .ok()?
            } else {
                self.builder
                    .build_float_to_unsigned_int(
                        value.float_value()?,
                        to.int_type(self.context),
                        "fptoi",
                    )
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
        // C's widest float takes an IEEE value by extension and gives one
        // back by truncation; the widening is the lossless direction, and
        // narrowing is a written `as`.
        if from.is_float() && to == DuxType::CLongDouble {
            let cast = self
                .builder
                .build_float_ext(value.float_value()?, wide_float_type(self.context), "widen")
                .ok()?;
            return Some(Typed::float(cast, to));
        }
        if from == DuxType::CLongDouble && to.is_float() {
            let cast = self
                .builder
                .build_float_trunc(value.float_value()?, to.float_type(self.context), "narrow")
                .ok()?;
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
        if from == DuxType::Str && (to == DuxType::Ptr || to.is_address_word()) {
            // The bytes of a string are a valid C string only when they hold
            // no NUL; `as` hands the address over and trusts the program.
            let (data, _) = self.slice_parts(value)?;
            return Some(Typed::pointer(data, to));
        }

        if from.is_pointer_like() && to.is_address_word() {
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
                .build_int_to_ptr(
                    value.int_value()?,
                    self.context.ptr_type(AddressSpace::default()),
                    "itop",
                )
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
            Expr::Ident { name, .. } => {
                if matches!(self.slot_of(name), Some((DuxType::Fn(_), _))) {
                    return self.indirect_call(module, callee, args, span);
                }
                self.call_named(module, name, args, callee, span, expected)
            }
            Expr::Paren { inner, .. } => self.call(module, inner, args, span, expected),
            Expr::Path {
                qualifier, name, ..
            } if self.shapes.contains_key(qualifier) => {
                self.variant_value(module, qualifier, name, args, &[], span, expected)
            }
            Expr::Path {
                qualifier,
                name,
                args: written,
                ..
            } if qualifier.is_empty() && self.generic_fns.contains_key(name) => {
                self.generic_call(module, name, written, args, span)
            }
            Expr::Field { base, name, .. } => {
                // `math.sqrt(x)`: what walks off the dot is a module, and a
                // module's functions are known by where they live.
                if let Expr::Ident { name: place, .. } = base.as_ref() {
                    let named = format!("{place}.{}", name.name);
                    if self.functions.contains_key(&named) || self.generic_fns.contains_key(&named)
                    {
                        return self.call_named(module, &named, args, callee, span, expected);
                    }
                }
                // `mem.Arena.new(4096)`: a module, a type it holds, and a
                // method that type owns. No receiver walks off the dot.
                if let Expr::Field {
                    base: owner,
                    name: type_name,
                    ..
                } = base.as_ref()
                {
                    if let Expr::Ident { name: place, .. } = owner.as_ref() {
                        let qualified =
                            format!("{place}.{}.{name}", type_name.name, name = name.name);
                        let plain = format!("{}.{name}", type_name.name, name = name.name);
                        let named = if self.functions.contains_key(&qualified) {
                            qualified
                        } else if self.functions.contains_key(&plain) {
                            plain
                        } else {
                            return self.call_method(module, base, name, args, span);
                        };
                        return self.call_named(module, &named, args, callee, span, expected);
                    }
                }
                self.call_method(module, base, name, args, span)
            }
            _ => {
                self.unsupported(span, "calling anything but a named function or a method");
                None
            }
        }
    }

    /// The address `&doubling` names: the function itself, in the shape a
    /// `fn` type carries. A function that takes the hidden context has no
    /// address in that shape, and says so.
    fn function_address(&mut self, operand: &Expr, span: Span) -> Option<Typed> {
        let miss = || {
            Diagnostic::error(
                code::TYPE_MISMATCH,
                "`&` takes the address of a binding, a field, an element or a function",
            )
            .with_span(operand.span())
            .with_note("a temporary has no address to keep: bind it first")
        };
        let Expr::Ident { name, .. } = operand else {
            self.diagnostics.push(miss());
            return None;
        };
        let named = self.named_in_home(name);
        let Some(signature) = self.signatures.get(named.as_str()).cloned() else {
            self.diagnostics.push(miss());
            return None;
        };
        if signature.hidden_context || signature.receiver.is_some() {
            self.unsupported(
                span,
                "taking the address of a function that carries a `context`: \
                 mark it `#[no_context]`",
            );
            return None;
        }
        let Some(function) = self.functions.get(named.as_str()).copied() else {
            self.diagnostics.push(miss());
            return None;
        };
        let address = function.as_global_value().as_pointer_value();
        Some(Typed {
            core: Core::Ptr(address),
            ty: DuxType::function(signature.params, signature.return_ty),
        })
    }

    /// A call through a `fn` value. The shape the value carries decides the
    /// arguments and the answer; the address carries nothing of its own.
    fn indirect_call(
        &mut self,
        module: &Module<'static>,
        callee: &Expr,
        args: &[Expr],
        span: Span,
    ) -> Option<Typed> {
        let target = self.expr(module, callee, None)?;
        let DuxType::Fn(info) = target.ty else {
            self.diagnostics.push(
                Diagnostic::error(
                    code::TYPE_MISMATCH,
                    "only a `fn` value can be called through its address".to_string(),
                )
                .with_span(callee.span()),
            );
            return None;
        };
        let function = target.pointer_value()?;
        if args.len() != info.params.len() {
            self.diagnostics.push(
                Diagnostic::error(
                    code::WRONG_ARITY,
                    format!(
                        "`{}` takes {} arguments, given {}",
                        target.ty.name(),
                        info.params.len(),
                        args.len()
                    ),
                )
                .with_span(span),
            );
            return None;
        }
        let mut arguments = Vec::new();
        for (parameter, arg) in info.params.iter().zip(args) {
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
        let llvm_params: Vec<BasicMetadataTypeEnum<'static>> = info
            .params
            .iter()
            .map(|ty| BasicMetadataTypeEnum::from(ty.llvm(self.context)))
            .collect();
        let return_ty = info.returns;
        let fn_type = match return_ty {
            Some(ty) => ty.llvm(self.context).fn_type(&llvm_params, false),
            None => self.context.void_type().fn_type(&llvm_params, false),
        };
        self.invoke_pointer(function, fn_type, arguments, return_ty)
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
        // Inside a module a bare name means the module's own function first.
        let named = self.named_in_home(name);
        let name = named.as_str();
        // The allocation builtins belong to the prelude, so a program that
        // declares its own `make` wins.
        if matches!(name, "new" | "make" | "free" | "del") && !self.functions.contains_key(name) {
            return self.memory_builtin(module, name, args, span);
        }
        if name == "hash" && !self.functions.contains_key(name) {
            return self.hash_builtin(module, args, span);
        }
        if (name == "assert" || name == "debug_assert") && !self.functions.contains_key(name) {
            return self.assert_builtin(module, name, args, span);
        }
        if self.generic_fns.contains_key(name) {
            return self.generic_call(module, name, &[], args, span);
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
        let (params, return_ty, hidden_context, external) = {
            let signature = self
                .signatures
                .get(name)
                .expect("a callable function has a recorded signature");
            (
                signature.params.clone(),
                signature.return_ty,
                signature.hidden_context,
                signature.external,
            )
        };
        if args.len() < params.len() || (args.len() > params.len() && !external) {
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
        // The System V rule for calls through `...`: `%al` says how many of
        // the extra arguments went into vector registers. Counting them is
        // the callee's business; setting the register is ours.
        let mut vector_registers = 0;
        for (index, arg) in args.iter().enumerate() {
            let parameter = params.get(index).copied();
            let value = self.expr(module, arg, parameter)?;
            if value.ty == DuxType::Error {
                return None;
            }
            // A C function exchanges addresses: a string crosses as the
            // address of its bytes (the count does not travel), and an
            // address of one shape is the same word as an address of any
            // other. An argument past the declared list gets C's default
            // promotions: an `f32` becomes a double, and every integer is
            // already at least an `i32` here.
            let pointer_slot = external && parameter.is_none_or(|ty| ty.is_address_word());
            let value = if pointer_slot && value.ty == DuxType::Str {
                let Some((data, _)) = self.slice_parts(value) else {
                    self.unsupported(arg.span(), "passing this string to C");
                    return None;
                };
                Typed::pointer(data, parameter.unwrap_or(DuxType::Ptr))
            } else if external
                && value.ty.is_pointer_like()
                && parameter.is_some_and(|ty| {
                    ty.is_address_word() || ty.llvm(self.context) == value.ty.llvm(self.context)
                })
            {
                // The same word in two spellings: every pointer is one
                // address, and a niche `Option<*T>` *is* its pointer.
                Typed {
                    core: value.core,
                    ty: parameter.unwrap_or(value.ty),
                }
            } else if external
                && matches!(value.ty, DuxType::Struct(_))
                && parameter.is_some_and(|ty| matches!(ty, DuxType::Struct(_)))
            {
                // The record travels by address, into the wrapper that
                // opens it up again on the C side.
                let at = self.materialize(value, "arg")?;
                Typed::pointer(at, DuxType::Ptr)
            } else if let Some(parameter) = parameter {
                self.coerce_implicit(module, value, parameter, arg.span())?
            } else if value.ty == DuxType::F32 {
                let promoted = self
                    .builder
                    .build_float_ext(value.float_value()?, self.context.f64_type(), "promote")
                    .ok()?;
                vector_registers += 1;
                Typed::float(promoted, DuxType::F64)
            } else {
                if value.ty.is_float() {
                    vector_registers += 1;
                }
                value
            };
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
        if external && args.len() > params.len() {
            self.set_vector_registers(vector_registers.min(8));
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
        if let Expr::Path {
            qualifier,
            name: type_name,
            args: written,
            ..
        } = base
        {
            // `Vec<i32>.new()`: the base is a type, not a value, and the
            // method is called on it rather than on a receiver.
            if qualifier.is_empty() && !written.is_empty() {
                let plain = format!("{type_name}.{}", name.name);
                if self.generic_methods.contains_key(&plain) {
                    return self
                        .static_generic_method(module, type_name, written, &name.name, args, span);
                }
            }
        }
        let place = self.place_of_expr(module, base);
        let (base_ty, base_at) = match place {
            Some((ty, at)) => (ty, Some(at)),
            None => {
                let value = self.expr(module, base, None)?;
                (value.ty, self.materialize(value, "recv"))
            }
        };
        if name.name == "len" && args.is_empty() {
            if let Some(len) = self.length_of(module, base_ty, base_at, base.span()) {
                return Some(len);
            }
        }
        // `s.bytes()` is the view the string already is: the same address and
        // the same count, over `u8`.
        if name.name == "bytes" && args.is_empty() && base_ty == DuxType::Str {
            let text = self.load(DuxType::Str, base_at?)?;
            let (data, length) = self.slice_parts(text)?;
            return Some(self.view(DuxType::U8, data, length));
        }
        // `s.runes()` decodes the UTF-8 into the code points it spells, a
        // `[]rune` the context owns.
        if name.name == "runes" && args.is_empty() && base_ty == DuxType::Str {
            return self.decode_runes(base_at?, span);
        }
        // A receiver is a record or a sum type, and either may be the one an
        // `impl` extends.
        let owner = match (self.record_of(base_ty), self.enum_of(base_ty)) {
            (Some(record), _) => Owner {
                name: record.name,
                spelled: record.spelled.clone(),
                args: record.args.clone(),
                as_ty: DuxType::Struct(record),
            },
            (None, Some(shape)) => Owner {
                name: shape.name,
                spelled: shape.spelled.clone(),
                args: shape.args.clone(),
                as_ty: DuxType::Enum(shape),
            },
            _ => {
                // The methods a primitive is used through belong to the
                // standard library (DESIGN 3.1), so a name that is not there
                // yet is a gap in this build rather than a verdict on the
                // program. A record with no such method is the program's own
                // mistake, and stays one.
                if base_ty != DuxType::Error && base_ty.methodless() {
                    self.unsupported(
                        name.span,
                        &format!("a `.{}` on a `{}`", name.name, base_ty.name()),
                    );
                    return None;
                }
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
            }
        };
        let key = format!("{}.{}", owner.name, name.name);
        // A method of a generic `impl` is code that means something once a
        // receiver says what its parameters are. The call has that, so this is
        // where the method becomes a function.
        let key = if owner.args.is_empty() {
            key
        } else {
            match self.instantiate_method(
                module,
                owner.name,
                owner.as_ty,
                &owner.spelled,
                &owner.args,
                &name.name,
            ) {
                Some(key) => key,
                None => return Some(Typed::error()),
            }
        };
        if self.skipped.contains(&key) {
            return Some(Typed::error());
        }
        let Some(function) = self.functions.get(&key).copied() else {
            self.diagnostics.push(
                Diagnostic::error(
                    code::UNDECLARED,
                    format!("`{}` has no method `{}`", owner.name, name.name),
                )
                .with_span(name.span)
                .with_note(format!(
                    "methods are declared in an `impl {}` block",
                    owner.name
                )),
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
        let want = signature.receiver.unwrap_or(owner.as_ty);
        let receiver = self.receiver(module, base_ty, base_at, want, name, base.span())?;
        if args.len() != params.len() {
            self.diagnostics.push(
                Diagnostic::error(
                    code::WRONG_ARITY,
                    format!(
                        "`{}.{}` takes {} arguments, given {}",
                        owner.name,
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
        // The `sret` shape: a record that comes back from C comes back
        // through a slot the caller reserves, and the call itself answers
        // nothing. A Dux function returning a record returns it as a
        // value, and its call lands in `finish_call` instead.
        let through_slot = return_ty.is_some_and(|ty| matches!(ty, DuxType::Struct(_)))
            && function.get_type().get_return_type().is_none();
        if through_slot {
            let ty = return_ty?;
            let slot = self.alloca(ty, "retslot");
            let mut arguments = arguments;
            arguments.insert(0, slot.into());
            let call = self.builder.build_call(function, &arguments, "").ok()?;
            call.add_attribute(
                AttributeLoc::Param(0),
                Emitter::sret_attribute(self.context),
            );
            let whole = self
                .builder
                .build_load(ty.llvm(self.context), slot, "ret")
                .ok()?;
            return Some(Typed {
                core: Core::Aggregate(whole.into_struct_value()),
                ty,
            });
        }
        let call = self.builder.build_call(function, &arguments, "call").ok()?;
        self.finish_call(call, return_ty)
    }

    /// The `sret` parameter attribute, by the name LLVM gives it in IR.
    fn sret_attribute(context: &Context) -> Attribute {
        context.create_enum_attribute(Attribute::get_named_enum_kind_id("sret"), 0)
    }

    /// The System V variadic rule: the call instruction carries no count of
    /// the vector-register arguments, so `%al` must say it. An odd number
    /// keeps the stack aligned the way the shadow space expects.
    fn set_vector_registers(&mut self, count: u32) {
        let fn_type = self.context.void_type().fn_type(&[], false);
        let asm = self.context.create_inline_asm(
            fn_type,
            format!("mov $${count}, %al"),
            "~{eax}".to_string(),
            true,
            false,
            Some(inkwell::InlineAsmDialect::ATT),
            false,
        );
        let _ = self.builder.build_indirect_call(fn_type, asm, &[], "");
    }

    /// A call through an address: what a `fn` value carries, and what the
    /// context's allocator slots hold. The shape is written at the call
    /// because the address itself carries none.
    fn invoke_pointer(
        &mut self,
        function: PointerValue<'static>,
        fn_type: FunctionType<'static>,
        arguments: Vec<BasicMetadataValueEnum<'static>>,
        return_ty: Option<DuxType>,
    ) -> Option<Typed> {
        let call = self
            .builder
            .build_indirect_call(fn_type, function, &arguments, "call")
            .ok()?;
        self.finish_call(call, return_ty)
    }

    fn finish_call(
        &mut self,
        call: CallSiteValue<'static>,
        return_ty: Option<DuxType>,
    ) -> Option<Typed> {
        let answer = call.try_as_basic_value();
        let ty = return_ty?;
        // A fixed array that comes back from a call comes back whole; a fixed
        // array everywhere else lives at an address. The slot is where the two
        // ways of holding one meet.
        if ty.array_info().is_some() {
            let ValueKind::Basic(whole) = answer else {
                return Some(Typed::error());
            };
            let slot = self.alloca(ty, "retarr");
            let _ = self.builder.build_store(slot, whole);
            return Some(Typed::pointer(slot, ty));
        }
        let core = match answer {
            ValueKind::Basic(BasicValueEnum::IntValue(value)) => Core::Int(value),
            ValueKind::Basic(BasicValueEnum::FloatValue(value)) => Core::Float(value),
            ValueKind::Basic(BasicValueEnum::PointerValue(value)) => Core::Ptr(value),
            ValueKind::Basic(BasicValueEnum::StructValue(value)) => Core::Aggregate(value),
            _ => return Some(Typed::error()),
        };
        Some(Typed { core, ty })
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
                return self.write_text(module, empty, true).map(|_| Typed::void());
            }
            self.unsupported(span, "`print` with no argument");
            return None;
        }
        match &args[0] {
            Expr::Str {
                text,
                span: arg_span,
                ..
            } => {
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
            Expr::InterpString { parts, .. } => if name == "println" {
                self.write_interpolation(module, parts, &args[1..], true, span)
            } else {
                self.write_interpolation(module, parts, &args[1..], false, span)
            }
            .map(|_| Typed::void()),
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
                    written: Some(expr),
                    ..
                } => {
                    let value = self.expr(module, expr, None)?;
                    values.push(value);
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
                    format!("{} arguments given for {} holes", values.len(), next_value),
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
        let _ = self.builder.build_call(printf, &arguments, "printf").ok()?;
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
        let _ = self.builder.build_call(snprintf, &writing, "format").ok()?;
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
                self.unsupported(span, "printing an array (array formatting lands at M3)");
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
            DuxType::Tuple(info) => {
                self.unsupported(
                    span,
                    &format!(
                        "printing a `{}` whole (print its members, one at a time)",
                        info.spelled
                    ),
                );
                None
            }
            _ => value.basic().map(|one| vec![one.into()]),
        }
    }
}

impl Emitter {
    /// A value put somewhere: a method reads a receiver's length and its bytes
    /// through an address, so a receiver a call built — a string or a view,
    /// which live in registers — gets a place of its own to stand in.
    fn materialize(&mut self, value: Typed, name: &str) -> Option<PointerValue<'static>> {
        if let Some(at) = value.pointer_value() {
            return Some(at);
        }
        let ty = value.ty;
        let slot = self.alloca(ty, name);
        let _ = self.builder.build_store(slot, value.basic()?);
        Some(slot)
    }

    /// What a `return` hands the caller. A fixed array lives at an address but
    /// crosses the boundary whole, so the array is read out of wherever it was
    /// standing.
    fn returnable(&mut self, value: Typed, ty: DuxType) -> Option<BasicValueEnum<'static>> {
        if ty.array_info().is_some() {
            let Core::Ptr(at) = value.core else {
                return None;
            };
            return self
                .builder
                .build_load(ty.llvm(self.context), at, "retarr")
                .ok();
        }
        value.basic()
    }

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
    fn alloca_in_entry(&mut self, ty: BasicTypeEnum<'static>, name: &str) -> PointerValue<'static> {
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
        let function = self
            .builder
            .get_insert_block()
            .unwrap()
            .get_parent()
            .unwrap();
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
        let printf = self.context.i32_type().fn_type(&[c_char.into()], true);
        let fn_value = declared(module, "printf", printf);
        self.printf = Some(fn_value);
        fn_value
    }

    fn snprintf_fn(&mut self, module: &Module<'static>) -> FunctionValue<'static> {
        let c_char = self.context.ptr_type(AddressSpace::default());
        let size = self.context.i64_type();
        let fn_type = self
            .context
            .i32_type()
            .fn_type(&[c_char.into(), size.into(), c_char.into()], true);
        self.declare_c(module, "snprintf", fn_type)
    }

    fn fwrite_fn(&mut self, module: &Module<'static>) -> FunctionValue<'static> {
        let c_char = self.context.ptr_type(AddressSpace::default());
        let size = self.context.i64_type();
        let fn_type = size.fn_type(
            &[c_char.into(), size.into(), size.into(), c_char.into()],
            false,
        );
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
        if self.windows {
            let number = self.context.i32_type().const_int(1, false);
            let fn_type = c_char.fn_type(&[self.context.i32_type().into()], false);
            let iob = declared(module, "__acrt_iob_func", fn_type);
            let stream = self
                .builder
                .build_call(iob, &[number.into()], "stdout")
                .ok()?
                .try_as_basic_value()
                .basic()
                .expect("stdout")
                .into_pointer_value();
            return Some(stream);
        }
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
        let imported = decl
            .attributes
            .iter()
            .find(|attribute| attribute.name == "c_name");
        if let Some(attribute) = imported {
            // A `#[c_name]` global is a window on storage the C world
            // owns: no initializer, no private copy, the symbol the
            // header promised. The definition lives in some translation
            // unit, and an unresolved use says so at the link.
            let Some(AttributeArgument::Literal(symbol)) = attribute.arguments.first() else {
                self.unsupported(decl.span, "a `#[c_name]` without the symbol it names");
                return;
            };
            let global = match module.get_global(symbol) {
                Some(found) => found,
                None => {
                    let declared = module.add_global(ty.llvm(self.context), None, symbol);
                    declared.set_linkage(Linkage::External);
                    declared
                }
            };
            self.globals.insert(name, (ty, global));
            return;
        }
        let initializer = match &decl.value {
            None => ty.llvm(self.context).const_zero(),
            Some(value) => {
                let Some(value) = self.const_expr(value) else {
                    return;
                };
                if matches!(value, ConstValue::Str(_)) {
                    self.unsupported(decl.span, "a `global` string (it lands with slices at M3)");
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
                DuxType::Bool => self
                    .context
                    .bool_type()
                    .const_int(u64::from(*v != 0), false)
                    .into(),
                other => other
                    .int_type(self.context)
                    .const_int(*v as u64, false)
                    .into(),
            },
            ConstValue::Float(v) => ty.float_type(self.context).const_float(*v).into(),
            ConstValue::Bool(b) => self
                .context
                .bool_type()
                .const_int(u64::from(*b), false)
                .into(),
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
                        self.context
                            .bool_type()
                            .const_int(u64::from(*v != 0), false),
                        DuxType::Bool,
                    ),
                    other => Typed::int(
                        other.int_type(self.context).const_int(*v as u64, false),
                        other,
                    ),
                }
            }
            ConstValue::Float(v) => {
                let ty = if c.ty == DuxType::F32 {
                    DuxType::F32
                } else {
                    DuxType::F64
                };
                Typed::float(ty.float_type(self.context).const_float(*v), ty)
            }
            ConstValue::Bool(b) => Typed::int(
                self.context.bool_type().const_int(u64::from(*b), false),
                DuxType::Bool,
            ),
            ConstValue::Str(text) => self.string_literal(text, dux_ast::StrKind::Utf8, span),
            ConstValue::Struct(_) => {
                self.diagnostics.push(
                    Diagnostic::error(code::CODEGEN_FAILED, "a record constant lost its storage")
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
        if let Some(found) = self.globals.get(name) {
            return Some((found.0, found.1.as_pointer_value()));
        }
        // A bare name inside a module means the module's own, and a
        // `global` declared there answers to the name it was written with.
        if !self.home.is_empty() {
            if let Some((ty, global)) = self.globals.get(&format!("{}{name}", self.home)) {
                return Some((*ty, global.as_pointer_value()));
            }
        }
        None
    }

    /// `demo.shared`: a `global` read through the module that declares it.
    /// The name is the flattened one the declaration carries out of its
    /// module, and a base that means a local, a global of this module's
    /// own, or a type is no module at all.
    fn qualified_slot(&self, place: &str, name: &str) -> Option<(DuxType, PointerValue<'static>)> {
        if self.known(place) || self.consts.contains_key(place) {
            return None;
        }
        [
            format!("{}{}.{name}", self.home, place),
            format!("{place}.{name}"),
        ]
        .into_iter()
        .find_map(|key| {
            self.globals
                .get(&key)
                .map(|(ty, global)| (*ty, global.as_pointer_value()))
        })
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

/// The machine's widest C float: LLVM's x86 80-bit extended, padded and
/// aligned to sixteen bytes by the target layout - exactly the shape clang
/// gives `long double`. inkwell does not wrap the type; LLVM's C API does,
/// which is where every other compiler gets it from.
fn wide_float_type(context: &inkwell::context::Context) -> FloatType<'_> {
    unsafe {
        FloatType::new(inkwell::llvm_sys::core::LLVMX86FP80TypeInContext(
            context.raw(),
        ))
    }
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
        DuxType::C(kind) => match (kind.width(), kind.signed()) {
            (64, true) => "ld",
            (64, false) => "lu",
            (_, true) => "d",
            _ => "u",
        },
        DuxType::F32 | DuxType::F64 => "g",
        DuxType::Str => ".*s",
        DuxType::Ptr | DuxType::Pointer(_) | DuxType::Fn(_) => "p",
        DuxType::Array(..)
        | DuxType::Struct(..)
        | DuxType::Tuple(..)
        | DuxType::Enum(_)
        | DuxType::Slice(_)
        | DuxType::CLongDouble
        | DuxType::Error => return None,
    })
}

fn expression_name(expr: &Expr) -> &'static str {
    match expr {
        Expr::Member { .. } => "a tuple member",
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
                    ForKind::ThreeClause { init, cond, step } => {
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
                    Expr::Ident { name, .. }
                        if matches!(name.as_str(), "new" | "make" | "free" | "del") =>
                    {
                        self.allocates = true;
                    }
                    Expr::Ident { name, .. } => self.calls.push(name.clone()),
                    Expr::Field { base, name, .. } => {
                        // `.runes()` decodes into a buffer the context owns.
                        if name.name == "runes" {
                            self.allocates = true;
                        }
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
            Expr::Try { operand, .. } => self.expr(operand),
            Expr::Tuple { elements, .. } => {
                for element in elements {
                    self.expr(element);
                }
            }
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
                        written: Some(value),
                        ..
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
    let empty = variants
        .iter()
        .position(|variant| variant.fields.is_empty())?;
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
        || matches!(
            ty,
            DuxType::Struct(_) | DuxType::Enum(_) | DuxType::Tuple(_)
        )
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
fn payload_field_of(
    info: Option<&'static EnumInfo>,
    variant: usize,
    name: &str,
) -> Option<DuxType> {
    let fields = &info?.variants.get(variant)?.fields;
    fields
        .iter()
        .find(|(field, _)| *field == name)
        .map(|(_, ty)| *ty)
}

/// What an enum and its arguments are spelled as, which is also the key the
/// instantiation is cached under: `Option<i32>`.
impl Emitter {
    /// What a generic parameter of the code being lowered stands for. Outside
    /// an instantiation it stands for nothing, which is the honest answer: the
    /// only code this build emits is code whose types are known.
    fn stands_for(&self, name: &str) -> Option<Stood> {
        self.substituting
            .iter()
            .find(|(param, _)| param == name)
            .map(|(_, stood)| *stood)
    }

    /// Whether a name written here is a `type` alias, in this module or
    /// anywhere the bare spelling can still reach.
    fn stands_for_alias(&self, name: &str) -> bool {
        self.aliases.contains_key(&self.aliases_key(name)) || self.aliases.contains_key(name)
    }

    fn aliases_key(&self, name: &str) -> String {
        if self.home.is_empty() {
            name.to_string()
        } else {
            format!("{}{name}", self.home)
        }
    }

    /// An array length the lowering can count: a literal, or the value a
    /// `const` parameter was handed.
    fn const_length(&self, len: &Expr) -> Option<u64> {
        match len {
            Expr::Int { value, .. } => u64::try_from(*value).ok(),
            Expr::Ident { name, .. } => match self.stands_for(name) {
                Some(Stood::Const(value)) => Some(value),
                _ => None,
            },
            _ => None,
        }
    }

    /// A generic `struct` at one instantiation. `Pair<i32, string>` and
    /// `Pair<string, i32>` are as different as two `struct`s that share no
    /// name, so each gets its own nominal LLVM type and its own field types.
    fn record_instance_of(&mut self, name: &str, args: &[DuxType]) -> Option<DuxType> {
        let spelled = instantiation(name, args);
        if let Some(info) = self.instances.get(&spelled).copied() {
            if self.resolving.contains(&spelled) {
                return self.in_progress_record(&spelled, info, name);
            }
            return Some(DuxType::Struct(info));
        }
        let decl = self.generic_structs.get(name).copied()?;
        if decl.type_params.len() != args.len() {
            self.unsupported(
                decl.span,
                &format!(
                    "`{name}` is written over {} parameters; this wants {}",
                    decl.type_params.len(),
                    args.len()
                ),
            );
            return None;
        }
        let llvm = self.context.opaque_struct_type(&spelled);
        let info: &'static StructInfo = Box::leak(Box::new(StructInfo {
            name: Box::leak(name.to_string().into_boxed_str()) as &str,
            spelled: spelled.clone(),
            args: args.to_vec(),
            llvm,
        }));
        // The handle exists before the fields are read, so a `*Self` of the
        // same instantiation points at a type that is still being laid out.
        self.instances.insert(spelled.clone(), info);
        let saved = std::mem::take(&mut self.substituting);
        self.substituting = decl
            .type_params
            .iter()
            .zip(args)
            .map(|(param, arg)| (param.name.name.clone(), Stood::Ty(*arg)))
            .collect();
        self.resolving.push(spelled.clone());
        let mut fields = Vec::new();
        let mut field_types = Vec::new();
        let mut unresolved = false;
        for field in &decl.fields {
            let Some(ty) = self.type_of(&field.ty) else {
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
        self.substituting = saved;
        if unresolved {
            self.instances.remove(&spelled);
            self.unsupported(
                decl.span,
                &format!("the type of one of `{spelled}`\'s fields"),
            );
            return None;
        }
        llvm.set_body(&field_types, false);
        self.struct_fields.insert(spelled, fields);
        Some(DuxType::Struct(info))
    }

    /// A generic record whose fields are still being read: only a pointer may
    /// aim at it, because a value of it would contain itself.
    fn in_progress_record(
        &mut self,
        spelled: &str,
        info: &'static StructInfo,
        name: &str,
    ) -> Option<DuxType> {
        if self.pointer_depth > 0 {
            return Some(DuxType::Struct(info));
        }
        let span = self
            .generic_structs
            .get(name)
            .map(|decl| decl.span)
            .unwrap_or_default();
        self.diagnostics.push(
            Diagnostic::error(
                code::CODEGEN_FAILED,
                format!(
                    "`{spelled}` has no size: a `struct` field must reach itself through a pointer"
                ),
            )
            .with_span(span),
        );
        None
    }

    /// `Pair<i32, string> { first: 1, second: "one" }`: the arguments say what
    /// the record was made of, and the fields written have to fit that.
    fn generic_record_value(
        &mut self,
        module: &Module<'static>,
        record: &dux_ast::StructDecl,
        args: &[Type],
        fields: &[(dux_ast::Ident, Expr)],
        span: Span,
    ) -> Option<Typed> {
        let mut given = Vec::new();
        for arg in args {
            let Some(ty) = self.type_of(arg) else {
                self.unsupported(arg.span(), "this type argument");
                return None;
            };
            given.push(ty);
        }
        let ty = self.record_instance_of(&record.name.name, &given)?;
        let info = self.struct_of(ty)?;
        self.record_value(module, ty, info, fields, span)
    }

    /// A call of a generic function. What it emits is the one instantiation
    /// this call asks for: the declaration's code, the call's types. Nothing
    /// is emitted for a generic no call asked for.
    fn generic_call(
        &mut self,
        module: &Module<'static>,
        name: &str,
        written: &[Type],
        args: &[Expr],
        span: Span,
    ) -> Option<Typed> {
        let decl = self.generic_fns.get(name).copied()?;
        // The arguments are the caller's, so they are lowered in the
        // caller's types. What the parameters stand for is not known until
        // they are, and is not the caller's business afterwards at all.
        let mut lowered = Vec::new();
        for arg in args {
            lowered.push(self.expr(module, arg, None)?);
        }
        if lowered.len() != decl.params.len() {
            self.diagnostics.push(
                Diagnostic::error(
                    code::WRONG_ARITY,
                    format!(
                        "`{name}` takes {} arguments, given {}",
                        decl.params.len(),
                        lowered.len()
                    ),
                )
                .with_span(span),
            );
            return None;
        }
        let saved = std::mem::take(&mut self.substituting);
        let instance = match self.instantiation_of(decl, written, &lowered, span) {
            None => None,
            Some(bound) => {
                self.substituting = bound.clone();
                self.declare_instance(
                    module,
                    Asking {
                        decl,
                        owner: None,
                        receiver: None,
                    },
                    name,
                    spelled(name, &bound),
                    bound,
                )
            }
        };
        self.substituting = saved;
        let Some(instance) = instance else {
            return Some(Typed::error());
        };
        let Some(function) = self.functions.get(&instance.key).copied() else {
            return Some(Typed::error());
        };
        let (params, return_ty, hidden_context) = {
            let signature = self
                .signatures
                .get(&instance.key)
                .expect("an instantiated function has a recorded signature");
            (
                signature.params.clone(),
                signature.return_ty,
                signature.hidden_context,
            )
        };
        let mut arguments = Vec::new();
        for (given, (arg, value)) in params.iter().zip(args.iter().zip(lowered)) {
            let value = self.coerce_implicit(module, value, *given, arg.span())?;
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

    /// A method of a generic `impl` called on the type itself, as in
    /// `Vec<i32>.new()`: the type arguments are written, and there is no
    /// receiver to pass.
    fn static_generic_method(
        &mut self,
        module: &Module<'static>,
        owner: &str,
        written: &[Type],
        method: &str,
        args: &[Expr],
        span: Span,
    ) -> Option<Typed> {
        let plain = format!("{owner}.{method}");
        let decl = self.generic_methods.get(&plain).copied()?;
        let Some(params) = self.impl_params.get(owner).cloned() else {
            self.unsupported(
                span,
                &format!("`{plain}` written on a type that is not generic"),
            );
            return None;
        };
        if params.len() != written.len() {
            self.unsupported(
                span,
                &format!("`{plain}`: its `impl` and the type it extends do not agree on the number of parameters"),
            );
            return None;
        }
        let mut bound = Vec::new();
        let mut parts = Vec::new();
        for (param, argument) in params.iter().zip(written) {
            let ty = self.type_of(argument)?;
            parts.push(ty.name());
            bound.push((param.clone(), Stood::Ty(ty)));
        }
        let mut lowered = Vec::new();
        for arg in args {
            lowered.push(self.expr(module, arg, None)?);
        }
        let spelled = format!("{owner}<{}>.{method}", parts.join(", "));
        let saved = std::mem::take(&mut self.substituting);
        self.substituting = bound.clone();
        let instance = self.declare_instance(
            module,
            Asking {
                decl,
                owner: Some(Box::leak(owner.to_string().into_boxed_str())),
                receiver: None,
            },
            &plain,
            spelled,
            bound,
        );
        self.substituting = saved;
        let Some(instance) = instance else {
            return Some(Typed::error());
        };
        let Some(function) = self.functions.get(&instance.key).copied() else {
            return Some(Typed::error());
        };
        let (params, return_ty, hidden_context) = {
            let signature = self
                .signatures
                .get(&instance.key)
                .expect("an instantiated function has a recorded signature");
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
                        "`{plain}` takes {} arguments, given {}",
                        params.len(),
                        args.len()
                    ),
                )
                .with_span(span),
            );
            return None;
        }
        let mut arguments = Vec::new();
        for (given, (arg, value)) in params.iter().zip(args.iter().zip(lowered)) {
            let value = self.coerce_implicit(module, value, *given, arg.span())?;
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

    /// What the parameters of a generic call stand for: what was written
    /// between `<` and `>`, or what the arguments turned out to be.
    fn instantiation_of(
        &mut self,
        decl: &FnDecl,
        written: &[Type],
        lowered: &[Typed],
        span: Span,
    ) -> Option<Vec<(String, Stood)>> {
        let mut bound: Vec<(String, Stood)> = Vec::new();
        if !written.is_empty() {
            let mut types = decl.type_params.iter().filter(|param| !param.is_const);
            for arg in written {
                if let Type::ConstArgument { value, .. } = arg {
                    let Some(ConstValue::Int(count)) = self.const_expr(value) else {
                        self.unsupported(
                            value.span(),
                            "a `const` argument that is not a whole number",
                        );
                        return None;
                    };
                    let Ok(count) = u64::try_from(count) else {
                        self.unsupported(
                            value.span(),
                            "a `const` argument that does not fit a `u64`",
                        );
                        return None;
                    };
                    let Some(param) = decl.type_params.iter().find(|param| param.is_const) else {
                        self.unsupported(
                            value.span(),
                            "a value where this function takes no `const` parameter",
                        );
                        return None;
                    };
                    bound.push((param.name.name.clone(), Stood::Const(count)));
                    continue;
                }
                let Some(param) = types.next() else { break };
                let Some(ty) = self.type_of(arg) else {
                    self.unsupported(arg.span(), "this type argument");
                    return None;
                };
                bound.push((param.name.name.clone(), Stood::Ty(ty)));
            }
        } else {
            for (param, value) in decl.params.iter().zip(lowered) {
                self.bind_parameters(&decl.type_params, &param.ty, value.ty, &mut bound);
            }
        }
        let mut missing: Vec<&str> = Vec::new();
        for param in &decl.type_params {
            if !bound.iter().any(|(known, _)| *known == param.name.name) {
                missing.push(param.name.name.as_str());
            }
        }
        if !missing.is_empty() {
            self.diagnostics.push(
                Diagnostic::error(
                    code::TYPE_MISMATCH,
                    format!(
                        "this call of `{}` cannot say what {} stands for",
                        decl.name.name,
                        missing
                            .iter()
                            .map(|name| format!("`{name}`"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                )
                .with_span(span)
                .with_note("write the arguments between `<` and `>`, or give the binding a type"),
            );
            return None;
        }
        // The same call shape always fills the parameters in the same order,
        // and two calls that ask for the same thing get the same function.
        let order = decl
            .type_params
            .iter()
            .map(|param| param.name.name.clone())
            .collect::<Vec<_>>();
        bound.sort_by_key(|(param, _)| {
            order
                .iter()
                .position(|name| name == param)
                .unwrap_or(usize::MAX)
        });
        Some(bound)
    }

    /// One declared parameter type against the type the call actually passed,
    /// binding the parameters that type mentions.
    fn bind_parameters(
        &mut self,
        params: &[dux_ast::TypeParam],
        declared: &Type,
        given: DuxType,
        bound: &mut Vec<(String, Stood)>,
    ) {
        let mentions = |name: &str, bound: &[(String, Stood)]| {
            params
                .iter()
                .any(|param| param.name.name == name && !param.is_const)
                && !bound.iter().any(|(known, _)| known == name)
        };
        match declared {
            Type::Named { name, args, .. } if args.is_empty() => {
                if mentions(name, bound) {
                    bound.push((name.clone(), Stood::Ty(given)));
                }
            }
            Type::Named { name, args, .. } => {
                let _ = name;
                let inner: Vec<DuxType> = match given {
                    DuxType::Enum(info) => info.args.clone(),
                    DuxType::Struct(info) => info.args.clone(),
                    _ => Vec::new(),
                };
                for (param, given) in args.iter().zip(inner) {
                    self.bind_parameters(params, param, given, bound);
                }
            }
            Type::Pointer { pointee, .. } => {
                if let Some(target) = self.pointee_of(given) {
                    self.bind_parameters(params, pointee, target, bound);
                }
            }
            Type::Slice { element, .. } => {
                if let DuxType::Slice(target) = given {
                    self.bind_parameters(params, element, *target, bound);
                }
            }
            Type::Array { element, .. } => {
                if let Some(info) = given.array_info() {
                    self.bind_parameters(params, element, info.elem, bound);
                }
            }
            _ => {}
        }
    }

    /// Declare the instantiation this call asked for, and queue its body if it
    /// is not emitted yet. The declaration is the same machinery every other
    /// function goes through, read under the types this call brought.
    fn declare_instance(
        &mut self,
        module: &Module<'static>,
        asking: Asking<'static>,
        name: &str,
        spelled: String,
        bound: Vec<(String, Stood)>,
    ) -> Option<Instance> {
        if self.allocating.contains(name) {
            self.allocating.insert(spelled.clone());
        }
        if self.skipped.contains(&spelled) {
            return None;
        }
        let key = spelled;
        let instance = Instance {
            symbol: symbol_of(&key),
            key,
            owner: asking.owner,
            receiver: asking.receiver,
            decl: asking.decl,
            bound,
        };
        if self.functions.contains_key(&instance.key) {
            return Some(instance);
        }
        let unit = instance.unit();
        self.declare(module, &unit);
        if !self.functions.contains_key(&instance.key) {
            return None;
        }
        if !self.emitted.contains(&instance.key) {
            self.pending.push(instance.clone());
        }
        Some(instance)
    }

    /// A method of a generic `impl`, at the instantiation the receiver says.
    /// `Pair<i32, string>.first` is a function; `Pair.first` is only code.
    fn instantiate_method(
        &mut self,
        module: &Module<'static>,
        owner: &'static str,
        as_ty: DuxType,
        spelled: &str,
        args: &[DuxType],
        method: &str,
    ) -> Option<String> {
        let plain = format!("{owner}.{method}");
        let decl = self.generic_methods.get(&plain).copied()?;
        let params = self.impl_params.get(owner)?.clone();
        if params.len() != args.len() {
            self.unsupported(
                decl.name.span,
                &format!("`{plain}`: its `impl` and the type it extends do not agree on the number of parameters"),
            );
            return None;
        }
        let bound: Vec<(String, Stood)> = params
            .iter()
            .zip(args)
            .map(|(param, arg)| (param.clone(), Stood::Ty(*arg)))
            .collect();
        let spelled = format!("{spelled}.{method}");
        self.substituting = bound.clone();
        let instance = self.declare_instance(
            module,
            Asking {
                decl,
                owner: Some(owner),
                receiver: Some(as_ty),
            },
            &plain,
            spelled,
            bound,
        );
        self.substituting = Vec::new();
        instance.map(|instance| instance.key)
    }

    /// The other half of an instantiation: the body, in the types the call
    /// asked for. A call inside it may ask for another one, which the queue
    /// reaches after this body is done.
    fn lower_instance(&mut self, module: &Module<'static>, instance: &Instance) {
        if self.emitted.contains(&instance.key) {
            return;
        }
        self.emitted.insert(instance.key.clone());
        let saved = std::mem::take(&mut self.substituting);
        self.substituting = instance.bound.clone();
        let unit = instance.unit();
        self.lower_body(module, &unit);
        self.substituting = saved;
    }
}

impl Instance {
    fn unit(&self) -> Unit<'static> {
        Unit {
            key: self.key.clone(),
            symbol: self.symbol.clone(),
            owner: self.owner,
            external: false,
            instantiated: true,
            receiver: self.receiver,
            decl: self.decl,
        }
    }
}

/// The name of the function one set of arguments asks for: `max<i32>`.
fn spelled(name: &str, bound: &[(String, Stood)]) -> String {
    let parts = bound
        .iter()
        .map(|(_, stood)| match stood {
            Stood::Ty(ty) => ty.name(),
            Stood::Const(value) => value.to_string(),
        })
        .collect::<Vec<_>>();
    if parts.is_empty() {
        return name.to_string();
    }
    format!("{name}<{}>", parts.join(", "))
}

/// A symbol LLVM will print without quoting it: `<`, `>` and the commas
/// between arguments become dots.
/// What a receiver is called: a record or a sum type, with what it was made
/// of. A method belongs to whichever one an `impl` extended.
struct Owner {
    name: &'static str,
    spelled: String,
    args: Vec<DuxType>,
    as_ty: DuxType,
}

/// The name an instantiation is an instantiation of: `Pair<i32, string>.first`
/// is an instantiation of `Pair.first`, and `max<i32>` is an instantiation of
/// `max`. The written name is where a module's home is recorded.
fn spelled_without_arguments(key: &str) -> String {
    let mut name = String::new();
    let mut nested = 0usize;
    for c in key.chars() {
        match c {
            '<' => nested += 1,
            '>' => nested = nested.saturating_sub(1),
            _ if nested == 0 => name.push(c),
            _ => {}
        }
    }
    name
}

/// A declaration renamed to the name it is known by from outside the module it
/// lives in. An `extern` block is left exactly as it was written: those names
/// are the C symbols they bind.
fn named_in(home: &str, decl: &Decl) -> Decl {
    let mut decl = decl.clone();
    match &mut decl {
        Decl::Fn(function) => function.name.name = format!("{home}{}", function.name.name),
        Decl::Struct(record) => record.name.name = format!("{home}{}", record.name.name),
        Decl::Enum(shape) => shape.name.name = format!("{home}{}", shape.name.name),
        Decl::TypeAlias(alias) => alias.name.name = format!("{home}{}", alias.name.name),
        Decl::Const(what) => what.name.name = format!("{home}{}", what.name.name),
        Decl::Global(shared) => shared.name.name = format!("{home}{}", shared.name.name),
        Decl::Impl(block) => block.type_name.name = format!("{home}{}", block.type_name.name),
        Decl::Module(namespace) => namespace.name.name = format!("{home}{}", namespace.name.name),
        _ => {}
    }
    decl
}

fn symbol_of(key: &str) -> String {
    let mut symbol = String::new();
    for letter in key.chars() {
        match letter {
            // `max<i32, f64>` and `Pair<i32, string>.first` both name a
            // function once the arguments are said; the dots say so.
            '<' | ',' | ' ' | '.' => {
                if !symbol.ends_with('.') {
                    symbol.push('.');
                }
            }
            '>' => {}
            other => symbol.push(other),
        }
    }
    format!("dux.{symbol}")
}

fn instantiation(name: &str, args: &[DuxType]) -> String {
    if args.is_empty() {
        return name.to_string();
    }
    format!(
        "{name}<{}>",
        args.iter()
            .map(|arg| arg.key())
            .collect::<Vec<_>>()
            .join(", ")
    )
}
