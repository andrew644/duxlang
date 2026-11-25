//! The C reader. `#[c_include("h")] mod ns;` reaches machine code through
//! here: a synthetic translation unit goes to libclang, the cursors come
//! back normalised into `CDecl`, and `spell` turns those into the Dux
//! declarations a namespace is made of.
//!
//! Layout is read, never re-derived (DESIGN 5.1): sizes, alignments, field
//! offsets and bitfields are whatever clang says for this target, so an
//! imported struct is the struct C would lay down, not Dux's guess at it.
//!
//! The cursor walk and the `CDecl` layer are separable on purpose: `walk`
//! talks to libclang and fills `CDecl`, `spell` maps `CDecl` to Dux without
//! knowing libclang exists.

mod spell;
mod unit;
mod walk;

pub use spell::{spell, spell_report};
pub use unit::{c_source, wrapper_name};
pub use walk::import;

/// A name the boundary could not carry, and why. This is what
/// `dux cimports` reads out: nothing is quietly waiting.
#[derive(Clone, Debug, PartialEq)]
pub struct Skipped {
    pub name: String,
    pub reason: String,
}

/// What a header left behind: the declarations that crossed and the names
/// that did not, each skip with the reason it did not.
pub struct Seen {
    pub decls: Vec<CDecl>,
    pub skipped: Vec<Skipped>,
}

/// A C type, as clang saw it. `Typedef` keeps the name the program will use
/// and the type underneath, because the mapping table treats `int32_t` and
/// `time_t` differently even though both are `int` by the time clang is done
/// with them.
#[derive(Clone, Debug, PartialEq)]
pub enum CType {
    Void,
    Bool,
    Char,
    SChar,
    UChar,
    Short,
    UShort,
    Int,
    UInt,
    Long,
    ULong,
    LongLong,
    ULongLong,
    Float,
    Double,
    LongDouble,
    Pointer(Box<CType>),
    Struct(String),
    Union(String),
    Enum(String),
    Array(Box<CType>, u64),
    FuncPtr {
        params: Vec<CType>,
        returns: Option<Box<CType>>,
        variadic: bool,
    },
    /// A typedef: the name, and what clang says it really is.
    Typedef(String, Box<CType>),
    /// Something the reader could not name. It may only ever travel behind a
    /// pointer; the spelling layer refuses to size it.
    Opaque(String),
}

impl CType {
    pub fn is_pointer(&self) -> bool {
        matches!(self, CType::Pointer(_))
    }
}

/// One member of a `struct` or `union`. `offset` is in bytes from clang, in
/// bits for a bitfield, where `bits` carries `(offset, width)`.
#[derive(Clone, Debug, PartialEq)]
pub struct CField {
    pub name: String,
    pub ty: CType,
    pub offset: Option<u64>,
    pub bits: Option<(u32, u32)>,
}

/// A `struct` or `union` with its layout copied from clang. `size` of `None`
/// is the opaque forward declaration: usable behind a pointer, and `sizeof`
/// is a compile error by design (DESIGN 5.2).
#[derive(Clone, Debug, PartialEq)]
pub struct CStruct {
    pub name: String,
    pub union: bool,
    pub size: Option<u64>,
    pub align: Option<u64>,
    pub fields: Vec<CField>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CEnum {
    pub name: String,
    pub int: CType,
    pub size: Option<u64>,
    pub constants: Vec<(String, i64, bool)>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CTypedef {
    pub name: String,
    pub underlying: CType,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CFunction {
    pub name: String,
    pub params: Vec<(String, CType)>,
    pub returns: Option<CType>,
    pub variadic: bool,
    /// `static inline` (or a macro's cousin): the definition lives in the
    /// header, so the only way to call it is to compile the header and link
    /// the definition in (D19).
    pub header_definition: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CVariable {
    pub name: String,
    pub ty: CType,
}

#[derive(Clone, Debug, PartialEq)]
pub enum MacroValue {
    Int(i128),
    Float(f64),
    Text(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct CMacro {
    pub name: String,
    pub value: MacroValue,
}

#[derive(Clone, Debug, PartialEq)]
pub enum CDecl {
    Function(CFunction),
    Struct(CStruct),
    Typedef(CTypedef),
    Enum(CEnum),
    Variable(CVariable),
    Macro(CMacro),
}

impl CDecl {
    pub fn name(&self) -> &str {
        match self {
            CDecl::Function(f) => &f.name,
            CDecl::Struct(s) => &s.name,
            CDecl::Typedef(t) => &t.name,
            CDecl::Enum(e) => &e.name,
            CDecl::Variable(v) => &v.name,
            CDecl::Macro(m) => &m.name,
        }
    }
}

/// What to import: one header, plus the flags the namespace accumulated.
pub struct Import {
    pub header: String,
    pub flags: Vec<String>,
    pub include_paths: Vec<String>,
}
