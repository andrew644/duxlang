//! The type checker. Where the resolver proves that names exist, this pass
//! proves that values fit: the D25 conversion lattice, untyped literals,
//! conditions, and the `fn` signature contract.
//!
//! The rule that keeps this pass honest is the same one the resolver lives
//! by: report only what is provably wrong *today*. Anything a later
//! milestone owns (struct fields in depth, patterns, generic instantiation,
//! C types, pointer targets) gets [`Ty::Opaque`], which never errors. A
//! checker that guesses is worse than a checker that waits.

use std::collections::{HashMap, HashSet};

use dux_ast::{
    AssignOp, BinaryOp, Block, Decl, Expr, FieldPattern, ForKind, ImplDecl, MatchArm, Pattern,
    Param, Program, Stmt, Type, TypeAliasDecl, UnaryOp,
};
use crate::sums::{self, Payload, Shape};

/// A variant's payload once an instantiation has filled it in.
use dux_diag::code;
use dux_diag::Diagnostic;
use dux_source::Span;

/// The C integer types, distinct from Dux's own (D18): `c_long` is not `i64`
/// even where the widths agree, so an LLP64 or a 32-bit `time_t` build is a
/// type error rather than quietly wrong bytes. The widths are the host C
/// ABI's — LP64 here — because a hand-written declaration says what the C
/// says, and C on this machine means LP64. A future `#[c_include]` reads
/// widths from clang instead of assuming them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CInt {
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
    Bool,
}

impl CInt {
    pub fn spelled(self) -> &'static str {
        match self {
            CInt::Char => "c_char",
            CInt::SChar => "c_schar",
            CInt::UChar => "c_uchar",
            CInt::Short => "c_short",
            CInt::UShort => "c_ushort",
            CInt::Int => "c_int",
            CInt::UInt => "c_uint",
            CInt::Long => "c_long",
            CInt::ULong => "c_ulong",
            CInt::LongLong => "c_long_long",
            CInt::ULongLong => "c_ulong_long",
            CInt::Bool => "c_bool",
        }
    }

    pub fn width(self) -> u32 {
        match self {
            CInt::Char | CInt::SChar | CInt::UChar | CInt::Bool => 8,
            CInt::Short | CInt::UShort => 16,
            CInt::Int | CInt::UInt => 32,
            CInt::Long | CInt::ULong | CInt::LongLong | CInt::ULongLong => 64,
        }
    }

    pub fn signed(self) -> bool {
        // `char` is signed on this target, which is what the spelling promises
        // until an import asks clang.
        matches!(
            self,
            CInt::Char | CInt::SChar | CInt::Short | CInt::Int | CInt::Long | CInt::LongLong
        )
    }
}

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
    /// A C integer: its own type, never interchangeable with `i8`..`i64`
    /// without a written `as` (D18).
    C(CInt),
    F32,
    F64,
    Str,
    Ptr,
    /// `*T`: a non-null pointer with a known target. `rawptr` is `Ptr`.
    Targeted(Box<Ty>),
    Unit,
    IntConst(i128),
    FloatConst(f64),
    Array(Box<Ty>),
    /// `[]T`: a `{ data, len }` view over elements of `T`.
    Slice(Box<Ty>),
    Struct(String),
    /// A declared `enum`, with the arguments it was made of: `Shape` carries
    /// none, `Option<i32>` carries the one. Nominal, like a record.
    Enum { name: String, args: Vec<Ty> },
    /// A generic `struct` made concrete: `Pair<i32, string>`. Nominal like a
    /// record, and the arguments say what its fields were filled with.
    Instance { name: String, args: Vec<Ty> },
    /// `return`, `break`, `panic`: a value that cannot arrive, so an arm that
    /// ends in one has whatever type the other arms agree on (DESIGN 4.5).
    Never,
    /// Unknown to the checker today: an imported or C type, a generic
    /// instantiation, a pointer target, a field of a shape M3 will know.
    /// Silences every rule it touches.
    Opaque,
}

impl Ty {
    fn name(&self) -> String {
        match self {
            Ty::Bool => "bool".to_string(),
            Ty::C(kind) => kind.spelled().to_string(),
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
            Ty::Targeted(target) => format!("*{}", target.name()),
            Ty::Unit => "()".to_string(),
            Ty::IntConst(value) => format!("integer literal {value}"),
            Ty::FloatConst(value) => format!("float literal {value}"),
            Ty::Array(element) => format!("[{}]", element.name()),
            Ty::Slice(element) => format!("[]{}", element.name()),
            Ty::Struct(name) => name.clone(),
            Ty::Instance { name, args } if args.is_empty() => name.clone(),
            Ty::Instance { name, args } => format!(
                "{name}<{}>",
                args.iter().map(Ty::name).collect::<Vec<_>>().join(", ")
            ),
            Ty::Enum { name, args } if args.is_empty() => name.clone(),
            Ty::Enum { name, args } => format!(
                "{name}<{}>",
                args.iter().map(Ty::name).collect::<Vec<_>>().join(", ")
            ),
            Ty::Never => "never".to_string(),
            Ty::Opaque => "<opaque>".to_string(),
        }
    }

    fn is_integral(&self) -> bool {
        matches!(
            self,
            Ty::I8 | Ty::I16 | Ty::I32 | Ty::I64 | Ty::U8 | Ty::U16 | Ty::U32 | Ty::U64 | Ty::C(_)
        )
    }

    fn is_signed(&self) -> bool {
        match self {
            Ty::C(kind) => kind.signed(),
            _ => matches!(self, Ty::I8 | Ty::I16 | Ty::I32 | Ty::I64),
        }
    }

    fn is_float(&self) -> bool {
        matches!(self, Ty::F32 | Ty::F64)
    }

    /// `*T` or `rawptr`: both hold an address.
    fn is_pointer(&self) -> bool {
        matches!(self, Ty::Ptr | Ty::Targeted(_))
    }

    /// The record a method call reaches through: a record itself, or the
    /// record a pointer aims at.
    /// The record a value is, with the arguments it was made of. A record
    /// written without arguments — which is all an `impl` of a generic record
    /// can say — has none.
    fn record(&self) -> Option<(&String, &[Ty])> {
        match self {
            Ty::Struct(name) => Some((name, &[])),
            Ty::Instance { name, args } | Ty::Enum { name, args } => Some((name, args)),
            _ => None,
        }
    }

    /// What a value is called, with whatever it was made of: a record or a sum
    /// type, either of which an `impl` may extend.
    fn record_owner(&self) -> Option<&String> {
        match self {
            Ty::Struct(name) | Ty::Instance { name, .. } => Some(name),
            Ty::Targeted(target) => target.record_owner(),
            _ => None,
        }
    }

    /// What a pointer points at, when the type says.
    fn target(&self) -> Option<&Ty> {
        match self {
            Ty::Targeted(target) => Some(target),
            _ => None,
        }
    }

    fn is_numeric(&self) -> bool {
        self.is_integral() || self.is_float()
    }

    fn width(&self) -> u32 {
        match self {
            Ty::Bool => 1,
            Ty::C(kind) => kind.width(),
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
            Ty::C(kind) => match kind.signed() {
                true => {
                    let limit = 1i128 << (kind.width() - 1);
                    -limit <= value && value < limit
                }
                false => value >= 0 && value < 1i128 << kind.width(),
            },
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
            // D18: the C ladder and Dux's ladder are separate. Same width and
            // signedness is not the same type; crossing families is written.
            if matches!(self, Ty::C(_)) != matches!(to, Ty::C(_)) {
                return false;
            }
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
        // A pointer fits into its own `Option` through the null niche: every
        // address stays an address, and NULL newly means "nothing". That is
        // the whole reason `Option<*T>` is the C pointer shape — and C says
        // `T *`, which is `Option<*T>` to the last letter.
        if self.is_pointer() && option_of_pointer(to) {
            if let Ty::Enum { args, .. } = to {
                let inner = args.first();
                return inner.is_some_and(|inner| self == inner);
            }
        }
        // An array is a slice that knows its own length, so borrowing its
        // elements as a view loses nothing.
        if let (Ty::Array(element), Ty::Slice(want)) = (self, to) {
            return element == want;
        }
        false
    }
}

/// How many values a variant is made from, in the source's own spelling.
fn payload_arity(payload: &Payload) -> usize {
    match payload {
        Payload::Empty => 0,
        Payload::Tuple(types) => types.len(),
        Payload::Named(fields) => fields.len(),
    }
}

/// How far one arm's pattern reaches into a value. A variant is reached by
/// naming it, and reaches no further than its own payload patterns go, which
/// is what makes `Some(None)` one arm's worth of evidence about `Option<Option<T>>`
/// rather than two.
#[derive(Clone)]
enum Cover {
    /// Nothing at all. The starting point, and the identity of a union.
    Nothing,
    /// Every value of the type: a binder, a wildcard, an `else`.
    Whole,
    /// These variants, each as far as the pattern written for it reaches.
    Parts(Vec<(String, Cover)>),
}

impl Cover {
    fn one(variant: &str, payload: Cover) -> Cover {
        Cover::Parts(vec![(variant_of_name(variant).to_string(), payload)])
    }

    /// What two patterns reach between them.
    fn joined(self, other: Cover) -> Cover {
        match (self, other) {
            (Cover::Whole, _) | (_, Cover::Whole) => Cover::Whole,
            (Cover::Nothing, other) => other,
            (other, Cover::Nothing) => other,
            (Cover::Parts(mut mine), Cover::Parts(theirs)) => {
                for (variant, payload) in theirs {
                    match mine.iter_mut().find(|(name, _)| *name == variant) {
                        Some((_, known)) => *known = known.clone().joined(payload),
                        None => mine.push((variant, payload)),
                    }
                }
                Cover::Parts(mine)
            }
        }
    }

    /// The variants this reaches, however far, which is all a type with named
    /// values and no tree — a `bool` — needs to hear.
    fn names(&self) -> Vec<String> {
        match self {
            Cover::Parts(list) => list.iter().map(|(name, _)| name.clone()).collect(),
            Cover::Whole => Vec::new(),
            Cover::Nothing => Vec::new(),
        }
    }
}

/// What the arms of a `match` reach. An arm with a guard proves nothing about
/// the rest of the type, so a guarded arm marks nothing here.
#[derive(Clone)]
struct Coverage {
    /// Is there anything to prove? A value the checker cannot name has no
    /// variants to enumerate.
    check: bool,
    /// A sum type has a tree to walk; a `bool` has two values and no enum to
    /// say so, and anything else has nothing to enumerate at all.
    enum_name: Option<String>,
    args: Vec<Ty>,
    reach: Cover,
    declared: Vec<(String, bool)>,
    catch_all: bool,
    guarded: bool,
}

impl Coverage {
    /// A subject with no variants of its own.
    fn plain(check: bool) -> Coverage {
        Coverage {
            check,
            enum_name: None,
            args: Vec::new(),
            reach: Cover::Nothing,
            declared: Vec::new(),
            catch_all: false,
            guarded: false,
        }
    }

    /// Say what no arm reaches (E0133). A guarded arm is not evidence, which is
    /// why `Some(x) if x > 0 =>` leaves `Some` unhandled.
    fn finish(self, checker: &mut Checker, subject: &Ty, span: Span) {
        if !self.check || self.catch_all {
            return;
        }
        let missing = match &self.enum_name {
            Some(owner) => checker.holes(owner, &self.args, &self.reach, true),
            None => self
                .declared
                .iter()
                .filter(|(_, covered)| !*covered)
                .map(|(name, _)| name.clone())
                .collect(),
        };
        if missing.is_empty() {
            if !self.declared.is_empty() || matches!(self.reach, Cover::Parts(_)) {
                return;
            }
            checker.report(
                span,
                Diagnostic::error(
                    code::NON_EXHAUSTIVE,
                    format!("a `match` on a `{}` needs an arm for every value", subject.name()),
                )
                .with_note("add a `_ =>` arm for whatever the others leave"),
            );
            return;
        }
        let which = missing
            .iter()
            .map(|variant| format!("`{variant}`"))
            .collect::<Vec<_>>()
            .join(", ");
        let (word, verb) = if missing.len() == 1 {
            ("variant", "is")
        } else {
            ("variants", "are")
        };
        checker.report(
            span,
            Diagnostic::error(
                code::NON_EXHAUSTIVE,
                format!("`match` is not exhaustive: {word} {which} {verb} not handled"),
            )
            .with_note("handle what is left, or add a `_ =>` arm"),
        );
    }

    /// Record how far one arm reaches. A `bool` is counted by the literals that
    /// name its two values; a sum type is counted by walking its tree.
    fn mark(&mut self, reach: &Cover) {
        if self.guarded {
            return;
        }
        if matches!(reach, Cover::Whole) {
            self.catch_all = true;
            return;
        }
        if self.enum_name.is_none() {
            for name in reach.names() {
                if let Some((_, covered)) =
                    self.declared.iter_mut().find(|(known, _)| *known == name)
                {
                    *covered = true;
                }
            }
            return;
        }
        self.reach = self.reach.clone().joined(reach.clone());
    }
}

/// The variant half of a `Shape::Rect` spelling, or the name itself when the
/// enum was left out.
fn payload_is_empty(shape: &Shape, variant: &str) -> bool {
    shape
        .variant(variant)
        .map(|(_, payload)| payload.is_empty())
        .unwrap_or(true)
}

fn variant_of_name(name: &str) -> &str {
    name.rsplit("::").next().unwrap_or(name)
}

/// `Option<*T>`, for any pointer: the shape a C `T *` arrives as, and the
/// shape a C string literal adopts when the slot asks for one.
fn option_of_pointer(ty: &Ty) -> bool {
    matches!(ty, Ty::Enum { name, args } if name == "Option" && args.first().is_some_and(Ty::is_pointer))
}

fn primitive(name: &str) -> Option<Ty> {
    Some(match name {
        "bool" => Ty::Bool,
        "c_char" => Ty::C(CInt::Char),
        "c_schar" => Ty::C(CInt::SChar),
        "c_uchar" => Ty::C(CInt::UChar),
        "c_short" => Ty::C(CInt::Short),
        "c_ushort" => Ty::C(CInt::UShort),
        "c_int" => Ty::C(CInt::Int),
        "c_uint" => Ty::C(CInt::UInt),
        "c_long" => Ty::C(CInt::Long),
        "c_ulong" => Ty::C(CInt::ULong),
        "c_long_long" => Ty::C(CInt::LongLong),
        "c_ulong_long" => Ty::C(CInt::ULongLong),
        "c_bool" => Ty::C(CInt::Bool),
        // C's floats map straight through (DESIGN 5.2): no distinct type and
        // no newtype, because there is only one IEEE shape each.
        "c_float" => Ty::F32,
        "c_double" => Ty::F64,
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
        "never" => Ty::Never,
        _ => return None,
    })
}

/// The universe of value-level builtins that never need a `use`.
fn builtin_result(name: &str) -> Option<Ty> {
    match name {
        "print" | "println" => Some(Ty::Unit),
        // Checked where the argument is a type: `new(i32)` is not a call with
        // a value argument, so the shape is picked apart before this.
        "new" => Some(Ty::Ptr),
        "make" => Some(Ty::Opaque),
        "free" | "del" => Some(Ty::Unit),
        // `hash` reads a value's bits, which is what a generic key needs
        // before any type an `impl` can name exists. What counts as hashable
        // is the lowering's question, per concrete type.
        "hash" => Some(Ty::U64),
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
    /// The declaration came from C, where a `char *` is what a string
    /// crosses as: the argument types meet at the boundary, not in the
    /// lattice.
    external: bool,
}

/// A `struct` written with type parameters. Its fields are source types that
/// only mean something once an instantiation says what the parameters are.
#[derive(Clone)]
struct GenericRecord {
    params: Vec<String>,
    fields: Vec<(String, Type)>,
}

/// A method of a generic `impl`. The `impl` carries the parameters, so the
/// method's own signature only means something once a receiver says them.
#[derive(Clone)]
struct GenericMethod {
    params: Vec<String>,
    /// How the receiver was written, when there was one.
    receiver: Option<Param>,
    /// How the parameters after the receiver were written.
    declared: Vec<Param>,
    result: Option<Type>,
    body: Block,
}

/// A `fn` written with type parameters, kept as it was written so every call
/// can be read again with the arguments that call brought.
#[derive(Clone)]
struct GenericFunction {
    type_params: Vec<String>,
    const_params: Vec<String>,
    params: Vec<Type>,
    result: Option<Type>,
}

struct Context {
    name: String,
    ty: Ty,
    is_const: bool,
}

pub(crate) struct Checker<'a> {
    program: &'a Program,
    /// The module whose declaration is being read, written the way a use would
    /// write it. Inside `mod math` a bare name means the module's own first,
    /// which is how `sqrt` gets to be `math.sqrt` without the file saying so.
    home: String,
    diagnostics: Vec<Diagnostic>,
    functions: HashMap<String, Signature>,
    methods: HashMap<(String, String), Signature>,
    structs: HashMap<String, Vec<(String, Type)>>,
    /// Every generic `struct`, by name: its fields name parameters, so they
    /// are kept as source types and read again per instantiation.
    generic_structs: HashMap<String, GenericRecord>,
    /// Every generic `fn`, by name, as written.
    generic_functions: HashMap<String, GenericFunction>,
    /// Methods of a generic `impl`, by the record they extend and their name.
    generic_methods: HashMap<(String, String), GenericMethod>,
    /// The instantiation the body being checked is read under: a type
    /// parameter name means whatever this says it means.
    instantiation: Vec<(String, Ty)>,
    /// A generic body is read once per instantiation, not once per call.
    read_instantiations: HashSet<String>,
    aliases: HashMap<String, Type>,
    /// Every `enum` in the file, over the two the universe predeclares.
    enums: HashMap<String, Shape>,
    globals: HashMap<String, Context>,
    scopes: Vec<Vec<Context>>,
    result: Ty,
    in_main: bool,
}

impl<'a> Checker<'a> {
    fn new(program: &'a Program) -> Self {
        let mut checker = Checker {
            program,
            home: String::new(),
            diagnostics: Vec::new(),
            functions: HashMap::new(),
            methods: HashMap::new(),
            structs: HashMap::new(),
            generic_structs: HashMap::new(),
            generic_functions: HashMap::new(),
            generic_methods: HashMap::new(),
            instantiation: Vec::new(),
            read_instantiations: HashSet::new(),
            aliases: HashMap::new(),
            enums: sums::prelude()
                .into_iter()
                .map(|shape| (shape.name.clone(), shape))
                .collect(),
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
        for scoped in self.program.scoped() {
            self.home = scoped.home.clone();
            let decl = scoped.decl;
            let named = |name: &str| format!("{}{}", scoped.home, name);
            match decl {
                Decl::Struct(decl) => {
                    let fields = decl
                        .fields
                        .iter()
                        .map(|field| (field.name.name.clone(), field.ty.clone()))
                        .collect();
                    let name = named(&decl.name.name);
                    if decl.type_params.is_empty() {
                        self.structs.insert(name, fields);
                    } else {
                        self.generic_structs.insert(
                            name,
                            GenericRecord {
                                params: decl
                                    .type_params
                                    .iter()
                                    .map(|param| param.name.name.clone())
                                    .collect(),
                                fields,
                            },
                        );
                    }
                }
                Decl::TypeAlias(TypeAliasDecl { name, type_params, aliased, .. }) => {
                    if type_params.is_empty() {
                        self.aliases.insert(named(&name.name), aliased.clone());
                    }
                }
                Decl::Enum(decl) => {
                    let mut shape = Shape::from_decl(decl);
                    shape.name = named(&shape.name);
                    self.enums.insert(shape.name.clone(), shape);
                }
                _ => {}
            }
        }
        self.home.clear();
    }

    fn collect_functions(&mut self) {
        let scoped = self.program.scoped();
        for entry in scoped.iter() {
            let Decl::Fn(function) = entry.decl else {
                continue;
            };
            self.home = entry.home.clone();
            let named = format!("{}{}", entry.home, function.name.name);
            let signature = self.signature(function, false);
            if !function.type_params.is_empty() {
                let mut type_params = Vec::new();
                let mut const_params = Vec::new();
                for param in &function.type_params {
                    if param.is_const {
                        const_params.push(param.name.name.clone());
                    } else {
                        type_params.push(param.name.name.clone());
                    }
                }
                self.generic_functions.insert(
                    named.clone(),
                    GenericFunction {
                        type_params,
                        const_params,
                        params: function.params.iter().map(|p| p.ty.clone()).collect(),
                        result: function.return_ty.clone(),
                    },
                );
            }
            self.functions.insert(named, signature);
        }
        self.home.clear();
        for entry in scoped.iter() {
            let Decl::Extern(extern_block) = entry.decl else {
                continue;
            };
            self.home = entry.home.clone();
            for function in &extern_block.functions {
                let signature = self.signature(function, true);
                self.functions
                    .insert(format!("{}{}", entry.home, function.name.name), signature);
            }
        }
        self.home.clear();
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
            // A C declaration checks the arguments it declares; only what
            // comes past the written list — the `...` of it — goes unchecked.
            check_arguments: function.type_params.is_empty(),
            external,
        }
    }

    fn resolve_type(&self, ty: &Type) -> Ty {
        let instantiation = self.instantiation.as_slice();
        self.resolve_type_in(ty, 0, instantiation)
    }

    /// A source type, with the enclosing enum's parameters replaced by the
    /// arguments of an instantiation. A parameter name is only a type while
    /// that substitution is in hand, which is why it travels as a binding.
    /// The name a written name answers to. A module's own declarations are
    /// known by where they live, and are written bare from inside it.
    fn at_home(&self, name: &str, known: impl Fn(&str) -> bool) -> String {
        let named = format!("{}{name}", self.home);
        if !self.home.is_empty() && known(&named) {
            return named;
        }
        name.to_string()
    }

    fn resolve_type_in(&self, ty: &Type, depth: u32, params: &[(String, Ty)]) -> Ty {
        if depth > 8 {
            return Ty::Opaque;
        }
        match ty {
            Type::Named { name, args, .. } => {
                if args.is_empty() {
                    if let Some((_, to)) = params.iter().find(|(known, _)| known == name) {
                        return to.clone();
                    }
                }
                if let Some(primitive) = primitive(name) {
                    return primitive;
                }
                let named = self.at_home(name, |spelling| {
                    self.enums.contains_key(spelling)
                        || self.structs.contains_key(spelling)
                        || self.generic_structs.contains_key(spelling)
                        || self.aliases.contains_key(spelling)
                });
                let name: &str = named.as_str();
                if let Some(shape) = self.enums.get(name) {
                    let resolved: Vec<Ty> = args
                        .iter()
                        .map(|arg| self.resolve_type_in(arg, depth + 1, params))
                        .collect();
                    // `Option` without its argument is a value the checker has
                    // to learn from context, so an omission stays silent.
                    if !shape.takes(args.len()) {
                        return Ty::Opaque;
                    }
                    return Ty::Enum {
                        name: name.to_string(),
                        args: resolved,
                    };
                }
                if !args.is_empty() {
                    if let Some(record) = self.generic_structs.get(name) {
                        if record.params.len() != args.len() {
                            return Ty::Opaque;
                        }
                        return Ty::Instance {
                            name: name.to_string(),
                            args: args
                                .iter()
                                .map(|arg| self.resolve_type_in(arg, depth + 1, params))
                                .collect(),
                        };
                    }
                    return Ty::Opaque;
                }
                if self.structs.contains_key(name) {
                    return Ty::Struct(name.to_string());
                }
                match self.aliases.get(name) {
                    Some(target) => self.resolve_type_in(target, depth + 1, params),
                    None => Ty::Opaque,
                }
            }
            Type::Pointer { pointee, .. } => {
                let target = self.resolve_type_in(pointee, depth + 1, params);
                if target == Ty::Opaque {
                    Ty::Ptr
                } else {
                    Ty::Targeted(Box::new(target))
                }
            }
            Type::Array { element, .. } => {
                Ty::Array(Box::new(self.resolve_type_in(element, depth + 1, params)))
            }
            Type::Slice { element, .. } => {
                Ty::Slice(Box::new(self.resolve_type_in(element, depth + 1, params)))
            }
            Type::Tuple { .. } | Type::Function { .. } | Type::ConstArgument { .. } => Ty::Opaque,
            Type::Error { .. } => Ty::Opaque,
        }
    }

    /// Type arguments written at a use are the one part of generics this build
    /// does not read yet (M5). The arguments are still walked, so a fault inside
    /// one is not lost, and the answer is the type the name has without them —
    /// which is why the build has to say so rather than carry on.
    /// `Pair<i32, string> { first: 1, second: "one" }`: the arguments fill the
    /// record's fields in, and what was written has to fit what they became.
    fn generic_record_value(
        &mut self,
        name: &str,
        record: &GenericRecord,
        args: &[Type],
        fields: &[(dux_ast::Ident, Expr)],
        span: Span,
        expected: Option<&Ty>,
    ) -> Ty {
        // The arguments are written in the scope of the call, so they are read
        // there; only the record's own fields are read under them.
        let mut made_of: Vec<Ty> = Vec::new();
        if args.len() == record.params.len() {
            for arg in args {
                made_of.push(self.resolve_type(arg));
            }
        }
        let saved = std::mem::take(&mut self.instantiation);
        if made_of.len() == record.params.len() {
            for (param, ty) in record.params.iter().zip(&made_of) {
                self.instantiation.push((param.clone(), ty.clone()));
            }
        } else if let Some(Ty::Instance { name: owner, args: known }) = expected {
            if owner == name && known.len() == record.params.len() {
                for (param, ty) in record.params.iter().zip(known) {
                    made_of.push(ty.clone());
                    self.instantiation.push((param.clone(), ty.clone()));
                }
            }
        }
        if made_of.len() != record.params.len() {
            made_of.clear();
        }
        for (field, value) in fields {
            let declared = record
                .fields
                .iter()
                .find(|(known, _)| known == &field.name)
                .map(|(_, ty)| ty.clone());
            match declared {
                Some(declared) => {
                    let want = self.resolve_type(&declared);
                    if want == Ty::Opaque {
                        let _ = self.infer(value);
                    } else {
                        self.expect(value, &want);
                    }
                }
                None => {
                    let _ = self.infer(value);
                    self.missing_field(name, &field.name, field.span);
                }
            }
        }
        self.instantiation = saved;
        let _ = span;
        Ty::Instance {
            name: name.to_string(),
            args: made_of,
        }
    }

    /// A call of a generic function. The type parameters are answered either
    /// as written (`max<i64>`) or read off the arguments (`max(3, 7)`), and
    /// the body is then read once with those types in it: an instantiation
    /// that cannot work is said here, at the call that asked for it.
    fn generic_call(
        &mut self,
        name: &str,
        generic: &GenericFunction,
        written: &[Type],
        args: &[Expr],
        span: Span,
    ) -> Ty {
        if generic.params.len() != args.len() {
            // The resolver counts the arguments of a call by name and says so;
            // the checker reads what it can and says nothing twice.
            for argument in args {
                let _ = self.infer(argument);
            }
            return Ty::Opaque;
        }
        let saved = std::mem::take(&mut self.instantiation);
        let bound = self.substitution_for(generic, name, written, args, span);
        self.instantiation = bound.clone();
        for (declared, argument) in generic.params.iter().zip(args) {
            let param = self.resolve_type(declared);
            if param == Ty::Opaque {
                let _ = self.infer(argument);
            } else {
                self.expect(argument, &param);
            }
        }
        let result = generic
            .result
            .as_ref()
            .map(|ty| self.resolve_type(ty))
            .unwrap_or(Ty::Unit);
        self.read_instantiation(name, &bound, span);
        self.instantiation = saved;
        result
    }

    /// What the type parameters of a generic call were asked to be. Written
    /// arguments answer directly; otherwise they are read off what the call
    /// actually passed, one declared parameter type at a time.
    fn substitution_for(
        &mut self,
        generic: &GenericFunction,
        name: &str,
        written: &[Type],
        args: &[Expr],
        span: Span,
    ) -> Vec<(String, Ty)> {
        let mut bound: Vec<(String, Ty)> = Vec::new();
        if !written.is_empty() {
            let asked = generic.type_params.len() + generic.const_params.len();
            if asked != written.len() {
                self.report(
                    span,
                    Diagnostic::error(
                        code::TYPE_MISMATCH,
                        format!(
                            "`{name}` is written over {asked} parameter{}",
                            if asked == 1 { "" } else { "s" }
                        ),
                    )
                    .with_note(format!(
                        "`{name}` takes {} and {} type parameters, and this call gives {}",
                        generic.type_params.len(),
                        generic.const_params.len(),
                        written.len()
                    )),
                );
            }
            let mut types = generic.type_params.iter();
            let mut consts = generic.const_params.iter();
            for arg in written {
                match arg {
                    // A `const` parameter takes a value, and the checker's
                    // lattice has no lengths to answer with: `[f32; N]` is
                    // `Ty::Array(f32)` either way. The value is codegen's.
                    Type::ConstArgument { .. } => {
                        let _ = consts.next();
                    }
                    other => {
                        if let Some(param) = types.next() {
                            let ty = self.resolve_type(other);
                            bound.push((param.clone(), ty));
                        }
                    }
                }
            }
            return bound;
        }
        for (declared, argument) in generic.params.iter().zip(args) {
            let seen = self.infer(argument);
            self.fill(&generic.type_params, declared, &seen, &mut bound);
        }
        bound
    }

    /// One declared parameter type against what the call passed, binding the
    /// parameters that type mentions. A literal has not chosen a type yet, so
    /// it adopts its default (D17).
    fn fill(
        &self,
        params: &[String],
        declared: &Type,
        seen: &Ty,
        bound: &mut Vec<(String, Ty)>,
    ) {
        let seen = seen.defaulted();
        let asks = |name: &str| {
            params.iter().any(|param| param == name)
                && !bound.iter().any(|(known, _)| known == name)
        };
        match declared {
            Type::Named { name, args, .. } if args.is_empty() => {
                if asks(name) {
                    bound.push((name.clone(), seen));
                }
            }
            Type::Named { args, .. } => {
                let inner: &[Ty] = match &seen {
                    Ty::Enum { args, .. } | Ty::Instance { args, .. } => args,
                    _ => &[],
                };
                for (declared, seen) in args.iter().zip(inner) {
                    self.fill(params, declared, seen, bound);
                }
            }
            Type::Pointer { pointee, .. } => {
                if let Ty::Targeted(target) = &seen {
                    self.fill(params, pointee, target, bound);
                }
            }
            Type::Array { element, .. } => {
                if let Ty::Array(target) = &seen {
                    self.fill(params, element, target, bound);
                }
            }
            Type::Slice { element, .. } => {
                if let Ty::Slice(target) = &seen {
                    self.fill(params, element, target, bound);
                }
            }
            _ => {}
        }
    }

    /// Read a generic body with one instantiation's types in it. Which `T` is
    /// asked for decides whether a body that never worked for any `T` works
    /// here, and only this reading can say (DESIGN 10).
    fn read_instantiation(&mut self, name: &str, bound: &[(String, Ty)], span: Span) {
        let arguments = bound
            .iter()
            .map(|(_, ty)| ty.name())
            .collect::<Vec<_>>()
            .join(", ");
        let asked = format!("{name}<{arguments}>");
        let program = self.program;
        let Some(declaration) = program.find_function(name) else {
            return;
        };
        let result = declaration
            .return_ty
            .as_ref()
            .map(|ty| self.resolve_type(ty))
            .unwrap_or(Ty::Unit);
        self.read_body(&declaration.params, &result, &declaration.body, &asked, name, span);
    }

    /// Read a body once in the types one thing asked it to be, so a body that
    /// does not work out for them is said where they were asked for. The
    /// parameters are already read through the substitution in hand.
    fn read_body(
        &mut self,
        declared: &[Param],
        result: &Ty,
        body: &Block,
        asked: &str,
        name: &str,
        span: Span,
    ) {
        if !self.read_instantiations.insert(asked.to_string()) {
            return;
        }
        let mut locals = Vec::new();
        for param in declared {
            let ty = self.resolve_type(&param.ty);
            locals.push((param.name.name.clone(), ty));
        }
        let result = result.clone();
        let said: Vec<(&'static str, String, u32)> = self
            .diagnostics
            .iter()
            .map(|done| (done.code, done.message.clone(), done.span.map_or(0, |s| s.start)))
            .collect();
        let saved_result = std::mem::replace(&mut self.result, result);
        let saved_main = std::mem::replace(&mut self.in_main, false);
        let mark = self.diagnostics.len();
        self.scopes.push(Vec::new());
        for (name, ty) in locals {
            if let Some(scope) = self.scopes.last_mut() {
                scope.push(Context {
                    name,
                    ty,
                    is_const: false,
                });
            }
        }
        self.block(body);
        self.scopes.pop();
        self.result = saved_result;
        self.in_main = saved_main;
        let mut fresh = self.diagnostics.split_off(mark);
        fresh.retain(|late| {
            !said.iter().any(|(code, message, start)| {
                late.code == *code && late.message == *message && late.span.map_or(0, |s| s.start) == *start
            })
        });
        self.diagnostics.extend(fresh);
        for late in &mut self.diagnostics[mark..] {
            let note = format!("in the instantiation of `{asked}`, in the body of `{name}`");
            late.span = Some(span);
            late.note = Some(match late.note.take() {
                Some(already) => format!("{note}; {already}"),
                None => note,
            });
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
            .or_else(|| {
                if !self.home.is_empty() {
                    if let Some(found) = self.globals.get(&format!("{}{name}", self.home)) {
                        return Some(found);
                    }
                }
                self.globals.get(name)
            })
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
        if value == Ty::Never {
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
            (Ty::Enum { name, args }, to @ Ty::Enum { .. }) => {
                if !self.enum_fits(name, args, to) {
                    self.mismatch(span, to, &value);
                }
                to.clone()
            }
            (_, to) => {
                let from = value.defaulted();
                if from == *to {
                    return to.clone();
                }
                let on_the_numeric_lattice = (from.is_numeric() && to.is_numeric())
                    || (from == Ty::Ptr && *to == Ty::Str)
                    || (from.is_pointer() && to.is_pointer())
                    || (from.is_pointer() && option_of_pointer(to))
                    || matches!((&from, to), (Ty::Array(element), Ty::Slice(want)) if element == want);
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

    /// Can a value of one enum stand where another is wanted? The name is the
    /// type (two look-alike enums are different types); an argument the checker
    /// never learned (`None` alone, a generic left to context) asks for nothing.
    fn enum_fits(&self, name: &str, args: &[Ty], to: &Ty) -> bool {
        let Ty::Enum {
            name: want,
            args: want_args,
        } = to
        else {
            return false;
        };
        if name != want {
            return false;
        }
        if want_args.is_empty() || args.is_empty() {
            return true;
        }
        if args.len() != want_args.len() {
            return false;
        }
        args.iter().zip(want_args).all(|(got, want)| {
            *got == *want || *got == Ty::Opaque || *want == Ty::Opaque || *got == Ty::Never
        })
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
        self.meet(a, b, span, "an `if` value")
    }

    /// Join two branches that must answer with one type. `what` names the
    /// construct, so the complaint reads `the arms of a \`match\` must agree`.
    fn meet(&mut self, a: Ty, b: Ty, span: Span, what: &str) -> Ty {
        if a == Ty::Opaque || b == Ty::Opaque {
            return Ty::Opaque;
        }
        match (&a, &b) {
            (Ty::Never, other) | (other, Ty::Never) => other.clone(),
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
            (left, right) if left.record().is_some() && right.record().is_some() => {
                let (Some((left_name, left_args)), Some((right_name, right_args))) =
                    (a.record(), b.record())
                else {
                    return self.disagree(&a, &b, span, what);
                };
                if left_name != right_name || left_args.len() != right_args.len() {
                    return self.disagree(&a, &b, span, what);
                }
                // `impl Pair<A, B>` can only ever say `Pair`, so a record
                // whose arguments were never said takes whatever the other
                // side says it was made of.
                if left_args.is_empty() {
                    return b;
                }
                if right_args.is_empty() {
                    return a;
                }
                let mut args = Vec::new();
                for (left, right) in left_args.iter().zip(right_args) {
                    let (left, right) = (left.clone(), right.clone());
                    args.push(self.meet(left, right, span, what));
                }
                // A record and a sum type are different things even when the
                // name and the arguments print the same, so the answer keeps
                // the shape the question was asked in.
                if matches!(a, Ty::Enum { .. }) || matches!(b, Ty::Enum { .. }) {
                    return Ty::Enum {
                        name: left_name.clone(),
                        args,
                    };
                }
                Ty::Instance {
                    name: left_name.clone(),
                    args,
                }
            }
            _ => {
                let a = a.defaulted();
                let b = b.defaulted();
                if a == b {
                    a
                } else {
                    self.disagree(&a, &b, span, what)
                }
            }
        }
    }

    /// Two things that were asked to agree and did not.
    fn disagree(&mut self, a: &Ty, b: &Ty, span: Span, what: &str) -> Ty {
        self.report(
            span,
            Diagnostic::error(
                code::TYPE_MISMATCH,
                format!(
                    "the arms of {what} must agree: `{}` vs `{}`",
                    a.name(),
                    b.name()
                ),
            ),
        );
        Ty::Opaque
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
        if left.is_pointer() || right.is_pointer() {
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
        if matches!(
            left,
            Ty::Struct(_) | Ty::Instance { .. } | Ty::Array(_)
        ) || matches!(
            right,
            Ty::Struct(_) | Ty::Instance { .. } | Ty::Array(_)
        ) {
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
        let same_target = left == right || left == Ty::Ptr || right == Ty::Ptr;
        match op {
            BinaryOp::Add | BinaryOp::Sub
                if (left.is_pointer() && numeric(&right)) || (numeric(&left) && right.is_pointer()) =>
            {
                if !left.is_pointer() {
                    return right.clone();
                }
                left
            }
            BinaryOp::Eq | BinaryOp::NotEq if left.is_pointer() && right.is_pointer() => {
                if same_target {
                    Ty::Bool
                } else {
                    self.error(
                        code::TYPE_MISMATCH,
                        format!(
                            "`{}` compares pointers to different things: `{}` and `{}`",
                            symbol(op),
                            left.name(),
                            right.name()
                        ),
                        span,
                    );
                    Ty::Opaque
                }
            }
            BinaryOp::Lt | BinaryOp::Gt | BinaryOp::Le | BinaryOp::Ge
                if left.is_pointer() && right.is_pointer() =>
            {
                if same_target {
                    Ty::Bool
                } else {
                    self.error(
                        code::TYPE_MISMATCH,
                        format!(
                            "`{}` compares pointers to different things: `{}` and `{}`",
                            symbol(op),
                            left.name(),
                            right.name()
                        ),
                        span,
                    );
                    Ty::Opaque
                }
            }
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
                let target = self.infer(operand).defaulted();
                match target {
                    // `&arr` and `&arr[a..b]` both make a view; taking the
                    // address of a view is the same view, unchanged.
                    Ty::Array(element) | Ty::Slice(element) => Ty::Slice(element),
                    target => Ty::Targeted(Box::new(target)),
                }
            }
            UnaryOp::Load => {
                let value = self.infer(operand);
                match value {
                    Ty::Targeted(target) => *target,
                    Ty::Ptr => Ty::Opaque,
                    Ty::Opaque => Ty::Opaque,
                    concrete => {
                        self.error(
                            code::TYPE_MISMATCH,
                            format!("`*` needs a pointer, found `{}`", concrete.name()),
                            span,
                        );
                        Ty::Opaque
                    }
                }
            }
        }
    }

    // ------------------------------------------------------------ sum types

    /// The enum a value belongs to, when it belongs to one.
    fn sum_of<'t>(&self, ty: &'t Ty) -> Option<(&'t str, &'t [Ty])> {
        match ty {
            Ty::Enum { name, args } => Some((name, args)),
            _ => None,
        }
    }

    /// Which enum a name written without its enum belongs to. The prelude's two
    /// answer first; a program's own enum may own the name, and where more than
    /// one does the spelling is ambiguous, so nothing is claimed.
    fn owner_of_variant(&self, variant: &str) -> Option<String> {
        for name in ["Option", "Result"] {
            if let Some(shape) = self.enums.get(name) {
                if shape.has_variant(variant) {
                    return Some(name.to_string());
                }
            }
        }
        let owners: Vec<&String> = self
            .enums
            .iter()
            .filter(|(_, shape)| shape.has_variant(variant))
            .map(|(name, _)| name)
            .collect();
        match owners.as_slice() {
            [one] => Some((*one).clone()),
            _ => None,
        }
    }

    /// A variant named with no parentheses and no fields: `Shape::Empty`,
    /// `ParseError::OutOfRange`, `None`. A payload-carrying variant spelled this
    /// way is a call missing its argument, which is worth one diagnostic.
    fn nullary_variant_of(
        &mut self,
        owner: &str,
        variant: &str,
        span: Span,
        expected: Option<&Ty>,
    ) -> Ty {
        let Some(shape) = self.enums.get(owner).cloned() else {
            return Ty::Opaque;
        };
        let Some((_, payload)) = shape.variant(variant) else {
            return Ty::Opaque;
        };
        if !payload.is_empty() {
            self.error(
                code::TYPE_MISMATCH,
                format!("`{owner}::{variant}` needs its payload: write `{owner}::{variant}(..)`"),
                span,
            );
            return Ty::Opaque;
        }
        self.enum_value(&shape, expected)
    }

    /// The same, where the enum is not written: `None`, or a bare `Active`.
    fn nullary_variant(&mut self, name: &str, span: Span, expected: Option<&Ty>) -> Ty {
        let Some(owner) = self.owner_of_variant(name) else {
            return Ty::Opaque;
        };
        self.nullary_variant_of(&owner, name, span, expected)
    }

    /// The instantiation a sum-type value takes: the one the surrounding
    /// program asks for, or parameters the checker has no way to know yet. With
    /// none of them known the value is left unknown too, which is how a `None`
    /// beside a C pointer stays quiet (DESIGN 2). A raw pointer is a position a
    /// `None` can fill with its null, and that is what it fills.
    fn enum_value(&self, shape: &Shape, expected: Option<&Ty>) -> Ty {
        match expected {
            Some(Ty::Enum { name, args }) if name == &shape.name && shape.takes(args.len()) => {
                Ty::Enum {
                    name: shape.name.clone(),
                    args: args.clone(),
                }
            }
            Some(Ty::Ptr) => Ty::Ptr,
            _ => {
                if shape.params.is_empty() {
                    return Ty::Enum {
                        name: shape.name.clone(),
                        args: Vec::new(),
                    };
                }
                Ty::Opaque
            }
        }
    }

    /// `Shape::Circle(2.0)`, `Some(3)`, `Err(ParseError::OutOfRange)`.
    fn constructor(
        &mut self,
        owner: &str,
        variant: &str,
        args: &[Expr],
        span: Span,
        expected: Option<&Ty>,
    ) -> Ty {
        let Some(shape) = self.enums.get(owner).cloned() else {
            return Ty::Opaque;
        };
        let Some((_, payload)) = shape.variant(variant).map(|(i, p)| (i, p.clone())) else {
            self.report(
                span,
                Diagnostic::error(code::UNDECLARED, format!("`{owner}` has no variant `{variant}`"))
                    .with_note(format!(
                        "the variants of `{owner}` are {}",
                        shape
                            .variants
                            .iter()
                            .map(|(name, _)| format!("{owner}::{name}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )),
            );
            for argument in args {
                let _ = self.infer(argument);
            }
            return Ty::Opaque;
        };
        let written = payload_arity(&payload);
        if written != args.len() {
            self.report(
                span,
                Diagnostic::error(
                    code::WRONG_ARITY,
                    format!(
                        "`{owner}::{variant}` takes {written} payload(s), the call gives {}",
                        args.len()
                    ),
                ),
            );
            for argument in args {
                let _ = self.infer(argument);
            }
            return self.enum_value(&shape, expected);
        }
        let inferred: Vec<Ty> = args
            .iter()
            .map(|argument| self.infer(argument).defaulted())
            .collect();
        let instantiation = self.instantiation(&shape, &payload, &inferred, expected);
        let resolved = self.payload_of(&shape, &instantiation, &payload);
        for (argument, want) in args.iter().zip(resolved) {
            self.expect(argument, &want);
        }
        Ty::Enum {
            name: shape.name.clone(),
            args: instantiation,
        }
    }

    /// Which parameters an instantiation uses. The context decides when it has
    /// a say; otherwise a parameter is whatever the payload written for it is.
    fn instantiation(
        &self,
        shape: &Shape,
        payload: &Payload,
        inferred: &[Ty],
        expected: Option<&Ty>,
    ) -> Vec<Ty> {
        if let Some(Ty::Enum { name, args }) = expected {
            if name == &shape.name && shape.takes(args.len()) {
                return args.clone();
            }
        }
        let written: Vec<&Type> = match payload {
            Payload::Empty => Vec::new(),
            Payload::Tuple(types) => types.iter().collect(),
            Payload::Named(fields) => fields.iter().map(|(_, ty)| ty).collect(),
        };
        let mut known: Vec<(String, Ty)> = Vec::new();
        for (spelled, got) in written.iter().zip(inferred) {
            if let Type::Named { name, args, .. } = spelled {
                if args.is_empty()
                    && shape.params.iter().any(|param| param == name)
                    && !known.iter().any(|(known, _)| known == name)
                {
                    known.push((name.clone(), got.clone()));
                }
            }
        }
        shape
            .params
            .iter()
            .map(|param| {
                known
                    .iter()
                    .find(|(known, _)| known == param)
                    .map(|(_, ty)| ty.clone())
                    .unwrap_or(Ty::Opaque)
            })
            .collect()
    }

    /// `Shape::Rect { w: 3.0, h: 4.0 }`: a variant whose payload is named, made
    /// field by field. Every field the payload declares has to be written.
    fn named_constructor(
        &mut self,
        owner: &str,
        written: &str,
        fields: &[(dux_ast::Ident, Expr)],
        span: Span,
        expected: Option<&Ty>,
    ) -> Ty {
        let Some(shape) = self.enums.get(owner).cloned() else {
            return Ty::Opaque;
        };
        let variant = variant_of_name(written);
        let Some(payload) = shape.variant(variant).map(|(_, payload)| payload.clone()) else {
            return Ty::Opaque;
        };
        if !matches!(payload, Payload::Named(_)) && !payload.is_empty() {
            self.error(
                code::TYPE_MISMATCH,
                format!("`{written}` takes its payload by position: write `{written}(..)`"),
                span,
            );
        }
        let declared = self.payload_fields(&shape, &self.made_of(&shape, expected), &payload);
        for (field, value) in fields {
            match declared.iter().find(|(known, _)| known == &field.name) {
                Some((_, ty)) => {
                    self.expect(value, ty);
                }
                None => {
                    let _ = self.infer(value);
                    self.missing_field(written, &field.name, field.span);
                }
            }
        }
        let left: Vec<&String> = declared
            .iter()
            .filter(|(known, _)| !fields.iter().any(|(field, _)| field.name == *known))
            .map(|(known, _)| known)
            .collect();
        if !left.is_empty() {
            self.report(
                span,
                Diagnostic::error(
                    code::TYPE_MISMATCH,
                    format!(
                        "`{written}` needs {}",
                        left.iter()
                            .map(|field| format!("`{field}`"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                )
                .with_note("a sum-type value writes every field of its payload"),
            );
        }
        self.enum_value(&shape, expected)
    }

    /// The arguments an enum is made of where nothing says otherwise: its own
    /// parameters, standing in for whatever the checker is not told.
    fn made_of(&self, shape: &Shape, expected: Option<&Ty>) -> Vec<Ty> {
        match expected {
            Some(Ty::Enum { name, args }) if name == &shape.name && shape.takes(args.len()) => {
                args.clone()
            }
            _ => shape.params.iter().map(|_| Ty::Opaque).collect(),
        }
    }

    /// A variant's payload with the enum's parameters replaced by an
    /// instantiation's arguments.
    fn payload_of(&self, shape: &Shape, args: &[Ty], payload: &Payload) -> Vec<Ty> {
        let bindings: Vec<(String, Ty)> = shape
            .params
            .iter()
            .cloned()
            .zip(args.iter().cloned())
            .collect();
        match payload {
            Payload::Empty => Vec::new(),
            Payload::Tuple(types) => types
                .iter()
                .map(|ty| self.resolve_type_in(ty, 1, &bindings))
                .collect(),
            Payload::Named(fields) => fields
                .iter()
                .map(|(_, ty)| self.resolve_type_in(ty, 1, &bindings))
                .collect(),
        }
    }

    /// The fields a struct-payload variant declares, resolved the same way.
    fn payload_fields(
        &self,
        shape: &Shape,
        args: &[Ty],
        payload: &Payload,
    ) -> Vec<(String, Ty)> {
        let bindings: Vec<(String, Ty)> = shape
            .params
            .iter()
            .cloned()
            .zip(args.iter().cloned())
            .collect();
        match payload {
            Payload::Named(fields) => fields
                .iter()
                .map(|(name, ty)| (name.clone(), self.resolve_type_in(ty, 1, &bindings)))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// What the arms of a `match` on this type have to cover.
    fn coverage(&self, subject: &Ty) -> Coverage {
        match subject {
            Ty::Opaque => Coverage::plain(false),
            Ty::Bool => Coverage {
                declared: vec![("true".to_string(), false), ("false".to_string(), false)],
                ..Coverage::plain(true)
            },
            Ty::Enum { name, args } if self.enums.contains_key(name) => Coverage {
                enum_name: Some(name.clone()),
                args: args.clone(),
                ..Coverage::plain(true)
            },
            _ => Coverage::plain(true),
        }
    }

    /// The values of `owner<args>` that `reach` does not touch, spelled the way
    /// a pattern spells them: `Shape::Rect` at the top, `Some(None)` one level
    /// in. A level the checker cannot name has no holes to report, and neither
    /// has one that every arm reaches.
    fn holes(&self, owner: &str, args: &[Ty], reach: &Cover, outer: bool) -> Vec<String> {
        let Cover::Parts(list) = reach else {
            return Vec::new();
        };
        let Some(shape) = self.enums.get(owner) else {
            return Vec::new();
        };
        let subject = Ty::Enum {
            name: owner.to_string(),
            args: args.to_vec(),
        };
        let mut holes = Vec::new();
        for (variant, _) in &shape.variants {
            let Some((_, inside)) = list.iter().find(|(name, _)| name == variant) else {
                holes.push(if outer {
                    format!("{owner}::{variant}")
                } else if payload_is_empty(shape, variant) {
                    variant.clone()
                } else {
                    format!("{variant}(..)")
                });
                continue;
            };
            let Some(Ty::Enum { name, args }) = self.payload_tree(&subject, variant) else {
                continue;
            };
            for hole in self.holes(&name, &args, inside, false) {
                holes.push(format!("{variant}({hole})"));
            }
        }
        holes
    }

    /// The one thing a variant carries, when it carries exactly one. That is
    /// where coverage has somewhere to go: a variant that carries nothing, or
    /// two things, or one thing that is not a sum type, is reached whole by the
    /// pattern that names it.
    fn payload_tree(&self, subject: &Ty, variant: &str) -> Option<Ty> {
        let (owner, args) = self.sum_of(subject)?;
        let shape = self.enums.get(owner)?;
        let (_, payload) = shape.variant(variant_of_name(variant))?;
        let mut fields = self.payload_of(shape, args, payload);
        if fields.len() == 1 {
            fields.pop()
        } else {
            None
        }
    }

    /// How far one arm's pattern reaches, which is the one question exhaustive
    /// checking asks of a pattern. Nothing here reports: a pattern that is not
    /// a variant — a binder, a literal, a range — is evidence about nothing the
    /// checker can enumerate, and coverage never claims more than it is given.
    fn cover_of(&self, pattern: &Pattern, subject: &Ty) -> Cover {
        match pattern {
            Pattern::Literal { value, .. } => match &**value {
                Expr::Bool { value, .. } => {
                    Cover::one(if *value { "true" } else { "false" }, Cover::Whole)
                }
                _ => Cover::Nothing,
            },
            Pattern::Range { .. } => Cover::Nothing,
            Pattern::Or { alternatives, .. } => alternatives
                .iter()
                .map(|one| self.cover_of(one, subject))
                .fold(Cover::Nothing, Cover::joined),
            Pattern::Wildcard { .. } | Pattern::Else { .. } | Pattern::Error { .. } => Cover::Whole,
            Pattern::Name { name, .. } => match self.enum_variant_of(subject, &name.name) {
                Some(_) => Cover::one(&name.name, Cover::Whole),
                None => Cover::Whole,
            },
            Pattern::Path { name, .. } => Cover::one(name, Cover::Whole),
            Pattern::Tuple {
                name, elements, ..
            } => {
                let inside = match (self.payload_tree(subject, name), elements.as_slice()) {
                    (Some(payload), [one]) => self.cover_of(one, &payload),
                    _ => Cover::Whole,
                };
                Cover::one(name, inside)
            }
            Pattern::Struct {
                name, fields, rest, ..
            } => {
                let inside = match (self.payload_tree(subject, name), fields.as_slice()) {
                    (Some(payload), [FieldPattern::Nested { pattern, .. }]) if !*rest => {
                        self.cover_of(pattern, &payload)
                    }
                    _ => Cover::Whole,
                };
                Cover::one(name, inside)
            }
        }
    }

    /// Check a pattern against the value it will be matched against, binding
    /// the names it introduces. The answer says whether the pattern refutes
    /// nothing: a wildcard, an `else`, a name that binds the whole value.
    fn pattern(&mut self, pattern: &Pattern, subject: &Ty) -> bool {
        match pattern {
            Pattern::Wildcard { .. } | Pattern::Else { .. } | Pattern::Error { .. } => true,
            Pattern::Literal { value, span } => {
                let spelled = self.infer(value);
                self.coerce(spelled, subject, *span);
                false
            }
            Pattern::Range { start, end, .. } => {
                for bound in [start, end] {
                    match &**bound {
                        Pattern::Literal { value, span } => {
                            let spelled = self.infer(value);
                            self.coerce(spelled, subject, *span);
                        }
                        other => {
                            let _ = self.pattern(other, subject);
                        }
                    }
                }
                false
            }
            Pattern::Or { alternatives, .. } => {
                let mut covers = false;
                for alternative in alternatives {
                    covers |= self.pattern(alternative, subject);
                }
                covers
            }
            Pattern::Name { name, span } => {
                match self.enum_variant_of(subject, &name.name) {
                    Some(true) => false,
                    Some(false) => {
                        self.error(
                            code::TYPE_MISMATCH,
                            format!(
                                "`{}` carries a payload: write `{}(..)` to take it apart",
                                name.name, name.name
                            ),
                            *span,
                        );
                        false
                    }
                    None => {
                        self.define(&name.name, subject.clone(), false);
                        true
                    }
                }
            }
            Pattern::Path { qualifier, name, span } => {
                let Some((_, _, payload)) =
                    self.variant_of_pattern(Some(qualifier), name, subject, *span)
                else {
                    return false;
                };
                if !payload.is_empty() {
                    self.error(
                        code::TYPE_MISMATCH,
                        format!(
                            "`{qualifier}::{name}` carries a payload: write `{qualifier}::{name}(..)`",
                        ),
                        *span,
                    );
                }
                false
            }
            Pattern::Tuple {
                qualifier,
                name,
                elements,
                span,
            } => {
                let resolved = self.variant_of_pattern(qualifier.as_deref(), name, subject, *span);
                let want = match &resolved {
                    Some((shape, args, payload)) => self.payload_of(shape, args, payload),
                    None => elements.iter().map(|_| Ty::Opaque).collect(),
                };
                if resolved.is_some() && want.len() != elements.len() {
                    self.error(
                        code::WRONG_ARITY,
                        format!(
                            "`{name}` carries {} payload(s), the pattern gives {}",
                            want.len(),
                            elements.len()
                        ),
                        *span,
                    );
                }
                for (element, want) in elements.iter().zip(want) {
                    let _ = self.pattern(element, &want);
                }
                false
            }
            Pattern::Struct {
                qualifier,
                name,
                fields,
                rest,
                span,
            } => {
                let resolved = self.variant_of_pattern(qualifier.as_deref(), name, subject, *span);
                let declared = match &resolved {
                    Some((shape, args, payload)) => {
                        if matches!(payload, Payload::Tuple(_)) && !payload.is_empty() {
                            self.error(
                                code::TYPE_MISMATCH,
                                format!(
                                    "`{name}` carries its payload by position: write `{name}(..)`",
                                ),
                                *span,
                            );
                        }
                        self.payload_fields(shape, args, payload)
                    }
                    None => Vec::new(),
                };
                for field in fields {
                    match field {
                        FieldPattern::Shorthand { name: field, span } => {
                            match declared.iter().find(|(known, _)| known == &field.name) {
                                Some((_, ty)) => self.define(&field.name, ty.clone(), false),
                                None => {
                                    if resolved.is_some() {
                                        self.error(
                                            code::TYPE_MISMATCH,
                                            format!("`{name}` has no field `{}`", field.name),
                                            *span,
                                        );
                                    }
                                    self.define(&field.name, Ty::Opaque, false);
                                }
                            }
                        }
                        FieldPattern::Nested {
                            name: field,
                            pattern,
                            ..
                        } => {
                            let want = declared
                                .iter()
                                .find(|(known, _)| known == &field.name)
                                .map(|(_, ty)| ty.clone())
                                .unwrap_or(Ty::Opaque);
                            if want == Ty::Opaque && resolved.is_some() {
                                self.error(
                                    code::TYPE_MISMATCH,
                                    format!("`{name}` has no field `{}`", field.name),
                                    field.span,
                                );
                            }
                            let _ = self.pattern(pattern, &want);
                        }
                    }
                }
                if !rest {
                    let missing: Vec<&String> = declared
                        .iter()
                        .filter(|(known, _)| {
                            !fields.iter().any(|field| match field {
                                FieldPattern::Shorthand { name, .. }
                                | FieldPattern::Nested { name, .. } => name.name == *known,
                            })
                        })
                        .map(|(known, _)| known)
                        .collect();
                    if !missing.is_empty() {
                        self.report(
                            *span,
                            Diagnostic::error(
                                code::TYPE_MISMATCH,
                                format!(
                                    "the pattern for `{name}` leaves out {}",
                                    missing
                                        .iter()
                                        .map(|field| format!("`{field}`"))
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                ),
                            )
                            .with_note("write every field, or `..` to leave the rest alone"),
                        );
                    }
                }
                false
            }
        }
    }

    /// Is a bare name a variant of the enum being matched? `Some(true)` says it
    /// is and carries nothing, so the pattern matches without binding.
    fn enum_variant_of(&self, subject: &Ty, name: &str) -> Option<bool> {
        let (owner, _) = self.sum_of(subject)?;
        let shape = self.enums.get(owner)?;
        shape.variant(name).map(|(_, payload)| payload.is_empty())
    }

    /// Resolve a variant written in a pattern against the type being matched.
    /// `None` means the pattern is already reported, or the subject is a value
    /// the checker cannot name — and a pattern over an unknown still binds.
    fn variant_of_pattern(
        &mut self,
        owner: Option<&str>,
        variant: &str,
        subject: &Ty,
        span: Span,
    ) -> Option<(Shape, Vec<Ty>, Payload)> {
        let owner = match owner {
            Some(owner) => owner.to_string(),
            None => match self.owner_of_variant(variant) {
                Some(owner) => owner,
                None => {
                    self.error(
                        code::UNDECLARED,
                        format!("`{variant}` is not a variant of anything in scope"),
                        span,
                    );
                    return None;
                }
            },
        };
        let Some(shape) = self.enums.get(&owner).cloned() else {
            self.error(
                code::TYPE_MISMATCH,
                format!("`{owner}` is not a sum type, so `{owner}::{variant}` matches nothing"),
                span,
            );
            return None;
        };
        let Some(payload) = shape.variant(variant).map(|(_, payload)| payload.clone()) else {
            self.report(
                span,
                Diagnostic::error(code::UNDECLARED, format!("`{owner}` has no variant `{variant}`"))
                    .with_note(format!(
                        "the variants of `{owner}` are {}",
                        shape
                            .variants
                            .iter()
                            .map(|(name, _)| format!("{owner}::{name}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )),
            );
            return None;
        };
        if !matches!(subject, Ty::Opaque) {
            let (named, args) = match self.sum_of(subject) {
                Some((named, args)) => (named.to_string(), args.to_vec()),
                None => {
                    self.error(
                        code::TYPE_MISMATCH,
                        format!(
                            "`{owner}::{variant}` cannot match a value of type `{}`",
                            subject.name()
                        ),
                        span,
                    );
                    return None;
                }
            };
            if named != owner {
                self.error(
                    code::TYPE_MISMATCH,
                    format!("this arm is a `{owner}`, the `match` is on a `{named}`"),
                    span,
                );
                return None;
            }
            return Some((shape, args, payload));
        }
        Some((
            shape.clone(),
            shape.params.iter().map(|_| Ty::Opaque).collect(),
            payload,
        ))
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
                // A C string literal is an address the program wrote down:
                // in a slot that names a pointer, it takes that pointer's
                // shape, exactly as an integer literal takes a `c_int`.
                dux_ast::StrKind::CString => match expected {
                    Some(want) if want.is_pointer() || option_of_pointer(want) => want.clone(),
                    _ => Ty::Ptr,
                },
                _ => Ty::Str,
            },
            Expr::Ident { name, span } => match self.lookup(name).map(|context| context.ty.clone()) {
                Some(ty) => ty,
                None => self.nullary_variant(name, *span, expected),
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
            Expr::Call { callee, args, span } => self.call(callee, args, *span, expected),
            Expr::Field { base, name, .. } => {
                let base = self.infer(base);
                self.field_type(&base, &name.name, name.span)
            }
            Expr::Index { base, index, span } => {
                let base = self.infer(base);
                // `[a..b]` is subslicing: it widens nothing and shortens nothing,
                // it makes a view over the same elements.
                if matches!(&**index, Expr::Range { .. }) {
                    for bound in range_bounds(index) {
                        let _ = self.infer(bound);
                    }
                    return match &base {
                        Ty::Array(element) => Ty::Slice(element.clone()),
                        Ty::Slice(element) => Ty::Slice(element.clone()),
                        Ty::Str => Ty::Str,
                        Ty::Opaque | Ty::Ptr => Ty::Opaque,
                        other => {
                            self.error(
                                code::TYPE_MISMATCH,
                                format!("there is no subslicing of `{}`", other.name()),
                                *span,
                            );
                            Ty::Opaque
                        }
                    };
                }
                let index = self.infer(index);
                match &base {
                    Ty::Array(element) => {
                        self.check_index(&index, *span);
                        (**element).clone()
                    }
                    Ty::Slice(element) => {
                        self.check_index(&index, *span);
                        (**element).clone()
                    }
                    Ty::Str => {
                        self.check_index(&index, *span);
                        Ty::U8
                    }
                    Ty::Opaque | Ty::Ptr => Ty::Opaque,
                    Ty::Targeted(target) => {
                        self.check_index(&index, *span);
                        (**target).clone()
                    }
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
            Expr::Path {
                qualifier,
                name,
                args,
                span,
            } => {
                for arg in args {
                    let _ = self.resolve_type(arg);
                }
                self.nullary_variant_of(qualifier, name, *span, expected)
            }
            Expr::StructLiteral {
                name,
                args,
                fields,
                span,
            } => {
                if !self.structs.contains_key(name.as_str()) {
                    if let Some(record) = self.generic_structs.get(name.as_str()).cloned() {
                        return self
                            .generic_record_value(name, &record, args, fields, *span, expected);
                    }
                    if let Some(owner) = self.owner_of_variant(variant_of_name(name)) {
                        return self.named_constructor(&owner, name, fields, *span, expected);
                    }
                }
                for arg in args {
                    let _ = self.resolve_type(arg);
                }
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
            Expr::CondBinding { pattern, value, .. } => {
                let subject = self.infer(value);
                self.scopes.push(Vec::new());
                let _ = self.pattern(pattern, &subject);
                self.scopes.pop();
                Ty::Bool
            }
            Expr::Match { subject, arms, span } => {
                let subject = self.infer(subject);
                self.arms(arms, &subject, expected, *span)
            }
            Expr::If { cond, then_arm, else_arm, span } => {
                let bound = self.condition(cond);
                let then_ty = self.expression_in(then_arm, expected);
                if bound {
                    self.scopes.pop();
                }
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
        if let Ty::Targeted(target) = base {
            return self.field_type(target, field, span);
        }
        if let Ty::Instance { name, args } = base {
            let record = self.generic_structs.get(name).cloned();
            if let Some(record) = record {
                let declared = record
                    .fields
                    .iter()
                    .find(|(known, _)| known == field)
                    .map(|(_, ty)| ty.clone());
                if let Some(declared) = declared {
                    let saved = std::mem::take(&mut self.instantiation);
                    self.instantiation = record
                        .params
                        .iter()
                        .cloned()
                        .zip(args.iter().cloned())
                        .collect();
                    let ty = self.resolve_type(&declared);
                    self.instantiation = saved;
                    return ty;
                }
                self.missing_field(name, field, span);
            }
            return Ty::Opaque;
        }
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
            .unwrap_or_else(|| {
                self.generic_structs
                    .get(owner)
                    .map(|record| record.fields.iter().map(|(known, _)| known.as_str()).collect())
                    .unwrap_or_default()
            });
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

    /// The type named as a builtin's first argument, as in `new(Vec3)`.
    fn type_argument(&mut self, argument: Option<&Expr>) -> Option<Ty> {
        let Some(Expr::Ident { name, span }) = argument else {
            return None;
        };
        let written = Type::Named {
            name: name.clone(),
            args: Vec::new(),
            span: *span,
        };
        let resolved = self.resolve_type(&written);
        (resolved != Ty::Opaque).then_some(resolved)
    }

    fn call(&mut self, callee: &Expr, args: &[Expr], span: Span, expected: Option<&Ty>) -> Ty {
        let name = match callee {
            Expr::Ident { name, .. } => Some(name.clone()),
            Expr::Paren { inner, .. } => match inner.as_ref() {
                Expr::Ident { name, .. } => Some(name.clone()),
                _ => None,
            },
            _ => None,
        };
        if let Some(name) = &name {
            // `new(T)` and `make(T, n)` name a type, not a value, so their
            // result is known without checking the first argument.
            if matches!(name.as_str(), "new" | "make") && !self.functions.contains_key(name) {
                if let Some(element) = self.type_argument(args.first()) {
                    for argument in args.iter().skip(1) {
                        let _ = self.infer(argument);
                    }
                    return match name.as_str() {
                        "new" => Ty::Targeted(Box::new(element)),
                        _ => Ty::Slice(Box::new(element)),
                    };
                }
            }
            let named = self.at_home(name, |spelling| {
                self.functions.contains_key(spelling) || self.generic_functions.contains_key(spelling)
            });
            if named != *name {
                if let Some(generic) = self.generic_functions.get(named.as_str()).cloned() {
                    return self.generic_call(&named, &generic, &[], args, span);
                }
                if let Some(signature) = self.functions.get(named.as_str()).cloned() {
                    return self.call_signature(&signature, args, span);
                }
            }
            if !self.functions.contains_key(name) {
                if let Some(result) = builtin_result(name) {
                    for arg in args {
                        let _ = self.infer(arg);
                    }
                    return result;
                }
            }
            // `Some(3)` and `Err(e)` are spelled without their enum, which is
            // how a predeclared sum type is meant to read.
            if !self.functions.contains_key(name) {
                if let Some(owner) = self.owner_of_variant(name) {
                    return self.constructor(&owner, name, args, span, expected);
                }
            }
            if let Some(generic) = self.generic_functions.get(name.as_str()).cloned() {
                return self.generic_call(name, &generic, &[], args, span);
            }
            if let Some(signature) = self.functions.get(name).cloned() {
                return self.call_signature(&signature, args, span);
            }
        }
        // The type arguments a path carries are read where the path itself is
        // checked, so a call says nothing about them and nothing is said twice.
        if let Expr::Path {
            qualifier,
            name,
            args: written,
            ..
        } = callee
        {
            if qualifier.is_empty() {
                if let Some(generic) = self.generic_functions.get(name.as_str()).cloned() {
                    return self.generic_call(name, &generic, written, args, span);
                }
            }
            if self.enums.contains_key(qualifier) {
                return self.constructor(qualifier, name, args, span, expected);
            }
            // `math.sqrt(x)`: a name written through the module it lives in.
            if !qualifier.is_empty() {
                let named = format!("{qualifier}.{name}");
                if let Some(generic) = self.generic_functions.get(named.as_str()).cloned() {
                    return self.generic_call(&named, &generic, written, args, span);
                }
                if let Some(signature) = self.functions.get(named.as_str()).cloned() {
                    return self.call_signature(&signature, args, span);
                }
            }
        }
        if let Expr::Field { base, name, .. } = callee {
            // `Vec<i32>.new()`: a method of a generic `impl` called on the
            // type, which says what the `impl`'s parameters stand for.
            if let Expr::Path {
                qualifier,
                name: type_name,
                args: written,
                ..
            } = base.as_ref()
            {
                if qualifier.is_empty() && !written.is_empty() {
                    let spelled = Type::Named {
                        name: type_name.clone(),
                        args: written.clone(),
                        span,
                    };
                    let base = self.resolve_type(&spelled);
                    let key = (type_name.clone(), name.name.clone());
                    if let Some(generic) = self.generic_methods.get(&key).cloned() {
                        return self.generic_method(&base, &key, &generic, args, span);
                    }
                }
            }
            // `math.sqrt(x)`: what walks off the dot is a module, not a value,
            // and a module's members are known by where they live.
            if let Expr::Ident {
                name: qualifier, ..
            } = base.as_ref()
            {
                if self.lookup(qualifier).is_none() {
                    let named = format!("{qualifier}.{}", name.name);
                    if let Some(generic) = self.generic_functions.get(named.as_str()).cloned() {
                        return self.generic_call(&named, &generic, &[], args, span);
                    }
                    if let Some(signature) = self.functions.get(named.as_str()).cloned() {
                        return self.call_signature(&signature, args, span);
                    }
                }
            }
            let base = self.infer(base);
            if name.name == "len" && args.is_empty() {
                match &base {
                    Ty::Array(_) | Ty::Slice(_) | Ty::Str => return Ty::U64,
                    _ => {}
                }
            }
            // A `string` is a view over bytes already: `.bytes()` says what it
            // is a view of.
            if name.name == "bytes" && args.is_empty() && matches!(&base, Ty::Str) {
                return Ty::Slice(Box::new(Ty::U8));
            }
            // A sum type is a type an `impl` may extend too, and its arguments
            // say what the `impl`'s parameters stand for. A sum type with no
            // method of that name stays silent: the standard library's methods
            // on `Option` and `Result` arrive with the milestone that writes
            // them, and a call asking for one is not the program's mistake.
            if let Ty::Enum { name: owner, .. } = &base {
                let key = (owner.clone(), name.name.clone());
                if let Some(generic) = self.generic_methods.get(&key).cloned() {
                    return self.generic_method(&base, &key, &generic, args, span);
                }
            }
            if let Some(owner) = base.record_owner() {
                let key = (owner.clone(), name.name.clone());
                if let Some(generic) = self.generic_methods.get(&key).cloned() {
                    return self.generic_method(&base, &key, &generic, args, span);
                }
                if let Some(signature) = self
                    .methods
                    .get(&(owner.clone(), name.name.clone()))
                    .cloned()
                {
                    if signature.check_arguments && signature.params.len() != args.len() {
                        self.report(
                            name.span,
                            Diagnostic::error(
                                code::WRONG_ARITY,
                                format!(
                                    "`{owner}.{}` takes {} arguments, given {}",
                                    name.name,
                                    signature.params.len(),
                                    args.len()
                                ),
                            )
                            .with_note("a method's receiver is written before the dot, not inside it"),
                        );
                        for argument in args {
                            let _ = self.infer(argument);
                        }
                        return signature.result;
                    }
                    return self.call_signature(&signature, args, span);
                }
                self.report(
                    name.span,
                    Diagnostic::error(
                        code::UNDECLARED,
                        format!("`{owner}` has no method `{}`", name.name),
                    )
                    .with_note(format!("methods are declared in an `impl {owner}` block")),
                );
                return Ty::Opaque;
            }
            // A method on a type the checker does not model yet (a string
            // view, a slice, `rawptr`) belongs to the standard library and is
            // silent until the milestone that owns it arrives.
            let _ = span;
            return Ty::Opaque;
        }
        let _ = self.infer(callee);
        for arg in args {
            let _ = self.infer(arg);
        }
        Ty::Opaque
    }

    /// A method of a generic `impl`, at the receiver that called for it. The
    /// receiver says what the parameters are, and the signature the call sees
    /// is the one that was written with those answers put in their place.
    fn generic_method(
        &mut self,
        base: &Ty,
        key: &(String, String),
        generic: &GenericMethod,
        args: &[Expr],
        span: Span,
    ) -> Ty {
        // The receiver may arrive as a value or as a pointer to one; either
        // way it is a record and it says what the parameters are.
        let seen = base.target().unwrap_or(base);
        let Some((_, given)) = seen.record() else {
            for argument in args {
                let _ = self.infer(argument);
            }
            return Ty::Opaque;
        };
        if given.len() != generic.params.len() {
            return Ty::Opaque;
        }
        let saved = std::mem::take(&mut self.instantiation);
        self.instantiation = generic
            .params
            .iter()
            .zip(given)
            .map(|(param, given)| (param.clone(), given.clone()))
            .collect();
        let arguments = given.iter().map(|ty| ty.name()).collect::<Vec<_>>().join(", ");
        let asked = format!("{}<{arguments}>.{}", key.0, key.1);
        let mut params = Vec::new();
        for param in &generic.declared {
            params.push(self.resolve_type(&param.ty));
        }
        let result = generic
            .result
            .as_ref()
            .map(|ty| self.resolve_type(ty))
            .unwrap_or(Ty::Unit);
        let outcome = self.call_signature(
            &Signature {
                params,
                result: result.clone(),
                check_arguments: true,
                external: false,
            },
            args,
            span,
        );
        // The body reads the receiver by name, so the reading of it starts
        // with the receiver bound, as `function` does.
        let mut read = Vec::new();
        if let Some(receiver) = &generic.receiver {
            read.push(receiver.clone());
        }
        read.extend(generic.declared.iter().cloned());
        self.read_body(&read, &result, &generic.body, &asked, &format!("{}.{}", key.0, key.1), span);
        self.instantiation = saved;
        outcome
    }

    fn call_signature(&mut self, signature: &Signature, args: &[Expr], span: Span) -> Ty {
        if signature.check_arguments {
            // The zip stops at the shorter side: an arity miss is reported
            // where the arity lives, and arguments past a C declaration's
            // written list are the callee's to interpret.
            for (argument, param) in args.iter().zip(&signature.params) {
                if signature.external {
                    // A C slot adopts what literals bring to it — an integer
                    // takes the `c_int`, a `c".."` takes the pointer shape —
                    // and a `string` crosses to any pointer slot: that is
                    // what the boundary converts, not the lattice.
                    let want = if param.is_pointer()
                        || option_of_pointer(param)
                        || param.is_integral()
                    {
                        Some(param)
                    } else {
                        None
                    };
                    let got = self.typed(argument, want);
                    if got == Ty::Str && (param.is_pointer() || option_of_pointer(param)) {
                        continue;
                    }
                    self.coerce(got, param, argument.span());
                    continue;
                }
                self.expect(argument, param);
            }
            for argument in args.iter().skip(signature.params.len()) {
                let _ = self.infer(argument);
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

    fn function(
        &mut self,
        function: &dux_ast::FnDecl,
        receiver: Option<&str>,
        owner: &Ty,
    ) {
        self.result = match &function.return_ty {
            Some(ty) => self.resolve_type(ty),
            None => Ty::Unit,
        };
        self.in_main = receiver.is_none() && function.name.name == "main";
        self.scopes.push(Vec::new());
        for parameter in &function.params {
            let ty = if parameter.name.name == "self" {
                match receiver {
                    Some(_) => match &parameter.ty {
                        Type::Named { name, args, .. }
                            if args.is_empty()
                                && !name.is_empty()
                                && name.as_str() != "self"
                                && self.resolve_type(&parameter.ty) != Ty::Opaque =>
                        {
                            self.resolve_type(&parameter.ty)
                        }
                        Type::Pointer { .. } => Ty::Targeted(Box::new(owner.clone())),
                        _ => owner.clone(),
                    },
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
        let saved = std::mem::take(&mut self.instantiation);
        self.instantiation = block
            .type_params
            .iter()
            .map(|param| (param.name.name.clone(), Ty::Opaque))
            .collect();
        let owner = named_owner(&self.home, &block.type_name.name);
        // A bare `self` names the type the `impl` extends, filled with that
        // `impl`'s own parameters: a method of `impl Cell<T>` reads its
        // receiver as `Cell<T>`, symbolic and all.
        let arguments: Vec<Ty> = self.instantiation.iter().map(|_| Ty::Opaque).collect();
        let receiver = if arguments.is_empty() {
            Ty::Struct(owner.clone())
        } else if self.enums.contains_key(owner.as_str()) {
            Ty::Enum {
                name: owner.clone(),
                args: arguments,
            }
        } else {
            Ty::Instance {
                name: owner.clone(),
                args: arguments,
            }
        };
        for method in &block.methods {
            self.function(method, Some(&owner), &receiver);
        }
        self.instantiation = saved;
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
                let bound = self.condition(cond);
                self.block(then_branch);
                if bound {
                    self.scopes.pop();
                }
                if let Some(other) = else_branch {
                    match &**other {
                        Stmt::Block { block, .. } => self.block(block),
                        other => self.statement(other),
                    }
                }
            }
            Stmt::While { cond, body, .. } => {
                let bound = self.condition(cond);
                self.block(body);
                if bound {
                    self.scopes.pop();
                }
            }
            Stmt::Loop { body, .. } => self.block(body),
            Stmt::For { kind, body, span } => {
                let _ = span;
                self.scopes.push(Vec::new());
                match kind {
                    ForKind::ThreeClause { init, cond, step } => {
                        self.statement(init);
                        let bound = self.condition(cond);
                        self.scopes.push(Vec::new());
                        for statement in &body.stmts {
                            self.statement(statement);
                        }
                        self.scopes.pop();
                        if bound {
                            self.scopes.pop();
                        }
                        self.statement(step);
                    }
                    ForKind::In { index, value, iterable } => {
                        let is_range = matches!(iterable, Expr::Range { .. });
                        let iterable = self.infer(iterable);
                        let element = match &iterable {
                            Ty::Array(inner) | Ty::Slice(inner) => Some((**inner).clone()),
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
            Stmt::Match { subject, arms, span } => {
                let subject = self.infer(subject);
                let _ = self.arms(arms, &subject, None, *span);
            }
            Stmt::Error { .. } => {}
        }
    }

    /// Walk the arms of a `match`. Each arm binds its own names, its body must
    /// agree with the others on one type when the `match` is a value, and
    /// together the arms must reach every value of the subject (DESIGN 4.4).
    fn arms(
        &mut self,
        arms: &[MatchArm],
        subject: &Ty,
        expected: Option<&Ty>,
        span: Span,
    ) -> Ty {
        let mut joined: Option<Ty> = None;
        let mut coverage = self.coverage(subject);
        for arm in arms {
            self.scopes.push(Vec::new());
            coverage.guarded = arm.guard.is_some();
            let reach = self.cover_of(&arm.pattern, subject);
            let covers = self.pattern(&arm.pattern, subject);
            if covers {
                coverage.mark(&Cover::Whole);
            } else {
                coverage.mark(&reach);
            }
            if let Some(guard) = &arm.guard {
                let ty = self.infer(guard);
                self.require_bool(ty, guard.span(), "the `if` of a `match` arm needs a `bool`");
            }
            let arm_ty = match &arm.body {
                dux_ast::MatchBody::Expr(expr) => self.expression_in(expr, expected),
                dux_ast::MatchBody::Block(block) => self.block_value(block, expected),
                dux_ast::MatchBody::Stmt(stmt) => {
                    self.statement(stmt);
                    match stmt {
                        Stmt::Return { .. } | Stmt::Break { .. } | Stmt::Continue { .. } => {
                            Ty::Never
                        }
                        _ => Ty::Unit,
                    }
                }
            };
            joined = Some(match joined {
                None => arm_ty,
                Some(known) => self.meet(known, arm_ty, arm.span, "a `match`"),
            });
            self.scopes.pop();
        }
        coverage.finish(self, subject, span);
        match joined {
            Some(ty) => ty,
            None => {
                if expected.is_none() {
                    self.error(
                        code::TYPE_MISMATCH,
                        "a `match` with no arms has no value",
                        span,
                    );
                }
                expected.cloned().unwrap_or(Ty::Opaque)
            }
        }
    }

    /// A block in arm position: its statements run, its last expression is its value.
    fn block_value(&mut self, block: &Block, expected: Option<&Ty>) -> Ty {
        self.scopes.push(Vec::new());
        let mut value = None;
        let last = block.stmts.len().saturating_sub(1);
        for (index, statement) in block.stmts.iter().enumerate() {
            if index == last {
                if let Stmt::Expr { expr, .. } = statement {
                    value = Some(expr);
                    continue;
                }
            }
            self.statement(statement);
        }
        let ty = match value {
            Some(expr) => self.expression_in(expr, expected),
            None => Ty::Unit,
        };
        self.scopes.pop();
        ty
    }

    /// Check a control-flow head. A pattern condition binds, so it pushes the
    /// scope its bindings live in and answers `true` to say the caller owns the
    /// matching pop, which happens when the body has been checked.
    fn condition(&mut self, cond: &Expr) -> bool {
        if let Expr::CondBinding { pattern, value, .. } = cond {
            let subject = self.infer(value);
            self.scopes.push(Vec::new());
            let _ = self.pattern(pattern, &subject);
            return true;
        }
        let ty = self.infer(cond);
        if ty != Ty::Opaque && ty != Ty::Bool {
            self.require_bool(ty, cond.span(), "this condition needs a `bool`");
        }
        false
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
                    if to.is_pointer() && matches!(op, AssignOp::Add | AssignOp::Sub) {
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
            Expr::Unary {
                op: UnaryOp::Load,
                operand,
                ..
            } => {
                let pointer = self.infer(operand);
                if let Some(target) = pointer.target().cloned() {
                    self.expect(value, &target);
                    if op != AssignOp::Set {
                        self.compound(op, &target, span);
                    }
                    return;
                }
            }
            Expr::Unary { operand, .. } => {
                let _ = self.infer(operand);
            }
            _ => {}
        }
        let _ = self.infer(value);
    }

    fn compound(&mut self, op: AssignOp, target: &Ty, span: Span) {
        if *target == Ty::Opaque
            || target.is_numeric()
            || target.is_pointer()
            || *target == Ty::Str
            || matches!(target, Ty::Slice(_))
        {
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
        for entry in self.program.scoped().iter() {
            let home = entry.home.clone();
            self.home = home.clone();
            let decl = entry.decl;
            let named = |name: &str| format!("{home}{name}");
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
                    let name = named(&decl.name.name);
                    self.globals.insert(
                        name.clone(),
                        Context { name, ty: bound, is_const: true },
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
                    let name = named(&decl.name.name);
                    self.globals.insert(
                        name.clone(),
                        Context { name, ty: bound, is_const: false },
                    );
                }
                _ => {}
            }
        }
        for entry in self.program.scoped().iter() {
            let home = entry.home.clone();
            self.home = home.clone();
            if let Decl::Impl(block) = entry.decl {
                let owner = named_owner(&home, &block.type_name.name);
                let saved = std::mem::take(&mut self.instantiation);
                self.instantiation = block
                    .type_params
                    .iter()
                    .map(|param| (param.name.name.clone(), Ty::Opaque))
                    .collect();
                for method in &block.methods {
                    let mut signature = self.signature(method, false);
                    let mut written = method.params.clone();
                    let receiver = if !written.is_empty() && written[0].name.name == "self" {
                        signature.params.remove(0);
                        Some(written.remove(0))
                    } else {
                        None
                    };
                    if !block.type_params.is_empty() {
                        self.generic_methods.insert(
                            (owner.clone(), method.name.name.clone()),
                            GenericMethod {
                                params: block
                                    .type_params
                                    .iter()
                                    .map(|param| param.name.name.clone())
                                    .collect(),
                                receiver,
                                declared: written,
                                result: method.return_ty.clone(),
                                body: method.body.clone(),
                            },
                        );
                    }
                    self.methods.insert((owner.clone(), method.name.name.clone()), signature);
                }
                self.instantiation = saved;
            }
        }
        for entry in self.program.scoped().iter() {
            self.home = entry.home.clone();
            match entry.decl {
                Decl::Fn(function) => self.function(function, None, &Ty::Unit),
                Decl::Impl(block) => self.impl_block(block),
                _ => {}
            }
        }
        self.home.clear();
        self.diagnostics
    }
}

/// The name a `struct` is known by from outside the module it lives in.
fn named_owner(home: &str, name: &str) -> String {
    format!("{home}{name}")
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
/// The two bounds of `[a..b]`, either of which may be missing.
fn range_bounds(index: &Expr) -> Vec<&Expr> {
    match index {
        Expr::Range { start, end, .. } => {
            start.iter().chain(end.iter()).map(|bound| &**bound).collect()
        }
        _ => Vec::new(),
    }
}

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
