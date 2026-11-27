//! The mapping table (DESIGN 5.2), written once: `CDecl` in, Dux source
//! text out. The driver feeds this text through the same parse-check-emit
//! road as any other module body, so the importer's output is exactly the
//! declarations a program could have written by hand — because they are.

use crate::{wrapper_name, CDecl, CEnum, CFunction, CStruct, CType, CTypedef, MacroValue};

/// Spell a namespace's worth of C declarations as Dux. A declaration the
/// table cannot cross today is left out, silently for now and loudly for
/// `dux cimports` later; nothing half-mapped sneaks through.
pub fn spell(namespace: &str, decls: &[CDecl]) -> String {
    spell_report(namespace, decls).0
}

/// Spell the namespace and name everything that did not cross. The names
/// here are the whole point of `dux cimports`: a header is a negotiation,
/// and the program deserves to know what it lost.
pub fn spell_report(
    namespace: &str,
    decls: &[CDecl],
) -> (String, Vec<crate::Skipped>, Vec<String>) {
    let mut missed: Vec<crate::Skipped> = Vec::new();
    let mut crossed: Vec<String> = Vec::new();
    let text = spell_with(namespace, decls, &mut missed, &mut crossed);
    (text, missed, crossed)
}

fn spell_with(
    namespace: &str,
    decls: &[CDecl],
    missed: &mut Vec<crate::Skipped>,
    crossed: &mut Vec<String>,
) -> String {
    let mut out = String::new();
    let mut externals: Vec<&CFunction> = Vec::new();
    let mut types: Vec<(String, String)> = Vec::new();
    let mut constants: Vec<(String, String)> = Vec::new();
    let mut emitted: std::collections::HashSet<String> = std::collections::HashSet::new();
    let ctx = Ctx::new(decls);
    for decl in decls {
        match decl {
            CDecl::Function(function) => {
                externals.push(function);
                emitted.insert(function.name.clone());
            }
            CDecl::Struct(record) => {
                if ctx.opaque.contains(&record.name) {
                    missed.push(crate::Skipped {
                        name: record.name.clone(),
                        reason: "record body stays behind: a field does not cross".to_string(),
                    });
                } else if record.union {
                    missed.push(crate::Skipped {
                        name: record.name.clone(),
                        reason: "unions are still waiting".to_string(),
                    });
                }
                if let Some(text) = spell_struct(&ctx, record) {
                    keep(&mut emitted, &mut types, record.name.clone(), text);
                } else if !record.union && !ctx.opaque.contains(&record.name) {
                    missed.push(crate::Skipped {
                        name: record.name.clone(),
                        reason: if is_plain_name(&record.name) {
                            "a field cannot be spelled".to_string()
                        } else {
                            "anonymous records stay unnamed".to_string()
                        },
                    });
                }
            }
            CDecl::Typedef(definition) => {
                if let Some(text) = spell_typedef(&ctx, definition) {
                    keep(&mut emitted, &mut types, safe(&definition.name), text);
                } else if !is_exact_width(&definition.name) {
                    missed.push(crate::Skipped {
                        name: definition.name.clone(),
                        reason: "the typedef's target has no Dux shape".to_string(),
                    });
                }
            }
            CDecl::Enum(enumeration) => {
                for (name, value, _) in &enumeration.constants {
                    let int =
                        spelling(&ctx, &enumeration.int).unwrap_or_else(|| "c_int".to_string());
                    let named = safe(name);
                    keep(
                        &mut emitted,
                        &mut constants,
                        named.clone(),
                        format!("const {named}: {int} = {value}"),
                    );
                }
            }
            CDecl::Variable(global) => {
                missed.push(crate::Skipped {
                    name: global.name.clone(),
                    reason: "global variables are still waiting".to_string(),
                });
            }
            CDecl::Macro(macro_value) => {
                if let Some(text) = spell_macro(macro_value) {
                    keep(&mut emitted, &mut constants, macro_value.name.clone(), text);
                }
            }
        }
    }
    for (name, text) in types {
        crossed.push(name);
        out.push_str(&text);
        out.push('\n');
    }
    if !externals.is_empty() {
        out.push_str("extern \"C\" {\n");
        for function in externals {
            let Some(text) = spell_function(&ctx, function) else {
                missed.push(crate::Skipped {
                    name: function.name.clone(),
                    reason: "the signature has no Dux spelling".to_string(),
                });
                continue;
            };
            // A `static inline` definition is called through the wrapper
            // the compiled translation unit hands it: the symbol a Dux
            // declaration binds to is the doorway, not the private name.
            // Two reasons to call through a wrapper: a `static inline`
            // definition needs a doorway out of the translation unit, and
            // a record crossing the boundary by value needs the one
            // spelling everybody agrees on — result through a slot,
            // records in through pointers.
            let needs_wrapper = function.header_definition || crate::wraps_record(function);
            let text = if needs_wrapper {
                match wrapper_name(namespace, function) {
                    Some(wrapper) => format!("#[c_name(\"{wrapper}\")] {text}"),
                    None => {
                        missed.push(crate::Skipped {
                            name: function.name.clone(),
                            reason: if function.variadic {
                                "a variadic function cannot be wrapped".to_string()
                            } else {
                                "a wrapper for this signature cannot be written in C".to_string()
                            },
                        });
                        continue;
                    }
                }
            } else {
                text
            };
            crossed.push(function.name.clone());
            out.push_str("    ");
            out.push_str(&text);
            out.push('\n');
        }
        out.push_str("}\n\n");
    }
    for (name, text) in constants {
        crossed.push(name);
        out.push_str(&text);
        out.push('\n');
    }
    out
}

/// Record a declaration under the name it defines, once. C gives one name
/// two lives now and then — glibc spells `FP_NAN` both a macro and an enum
/// constant — and the second definition of a name is the one nobody asked
/// for twice.
fn keep(
    emitted: &mut std::collections::HashSet<String>,
    sink: &mut Vec<(String, String)>,
    named: String,
    text: String,
) {
    if emitted.insert(named.clone()) {
        sink.push((named, text));
    }
}

/// The exact-width names mean what they say, and they mean Dux types
/// (DESIGN 5.2: `int8_t`..`uint64_t` map straight through). The typedefs
/// that hold them are not imported — there is nothing left to say.
fn is_exact_width(name: &str) -> bool {
    matches!(
        name,
        "int8_t"
            | "int16_t"
            | "int32_t"
            | "int64_t"
            | "uint8_t"
            | "uint16_t"
            | "uint32_t"
            | "uint64_t"
            | "size_t"
            | "ssize_t"
            | "__size_t"
            | "__ssize_t"
            | "ptrdiff_t"
            | "intptr_t"
            | "uintptr_t"
    )
}

fn exact_width(name: &str) -> Option<&'static str> {
    Some(match name {
        "int8_t" => "i8",
        "int16_t" => "i16",
        "int32_t" => "i32",
        "int64_t" => "i64",
        "uint8_t" => "u8",
        "uint16_t" => "u16",
        "uint32_t" => "u32",
        "uint64_t" => "u64",
        "size_t" | "uintptr_t" | "__size_t" => "usize",
        "ssize_t" | "ptrdiff_t" | "intptr_t" | "__ssize_t" => "isize",
        _ => return None,
    })
}

fn spell_typedef(ctx: &Ctx, definition: &CTypedef) -> Option<String> {
    if is_exact_width(&definition.name) || !is_plain_name(&definition.name) {
        return None;
    }
    // A typedef gets a line when the name is writable and what it names can
    // cross: primitives, and records the table emitted (a forward declaration
    // included, which is all `FILE` ever needs to be). An alias whose name
    // clang guards with `__` stays transparent, so nothing dangles.
    let underlying = resolve(&definition.underlying);
    if is_primitive(underlying) || ctx.has_record(underlying) {
        let spelled = spelling(ctx, &definition.underlying)?;
        Some(format!("type {} = {spelled}", safe(&definition.name)))
    } else {
        None
    }
}

/// Walk typedef aliases to the thing they finally mean.
fn resolve(ty: &CType) -> &CType {
    match ty {
        CType::Typedef(_, underlying) => resolve(underlying),
        other => other,
    }
}

fn is_primitive(ty: &CType) -> bool {
    matches!(
        ty,
        CType::Bool
            | CType::Char
            | CType::SChar
            | CType::UChar
            | CType::Short
            | CType::UShort
            | CType::Int
            | CType::UInt
            | CType::Long
            | CType::ULong
            | CType::LongLong
            | CType::ULongLong
            | CType::Float
            | CType::Double
    )
}

/// The name Dux gives a C name that collides with one of its keywords. A
/// C struct is free to field a member called `type`; Dux has to write it
/// down as something, and a trailing underscore says which.
fn safe(name: &str) -> String {
    const RESERVED: &[&str] = &[
        "package", "import", "fn", "struct", "enum", "type", "const", "global", "impl", "union",
        "mod", "extern", "if", "else", "while", "for", "in", "loop", "break", "continue", "defer",
        "match", "return", "as", "when", "true", "false",
    ];
    if RESERVED.contains(&name) {
        format!("{name}_")
    } else {
        name.to_string()
    }
}

/// Whether a C name can be written down in Dux. clang spells an anonymous
/// nested record `struct (unnamed at ...)`, and that is not anybody's name.
fn is_plain_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        && !name.starts_with(|c: char| c.is_ascii_digit())
}

fn record_crosses(
    name: &str,
    bodies: &std::collections::HashMap<&str, &CStruct>,
    underlying: &std::collections::HashMap<&str, &CType>,
    decided: &mut std::collections::HashMap<String, bool>,
    trail: &mut Vec<String>,
) -> bool {
    if let Some(known) = decided.get(name) {
        return *known;
    }
    if trail.iter().any(|seen| seen == name) {
        return false;
    }
    trail.push(name.to_string());
    let yes = match bodies.get(name) {
        None => false,
        Some(record) => {
            record.size.is_some()
                && record.fields.iter().all(|field| {
                    field.bits.is_none()
                        && field_crosses(&field.ty, bodies, underlying, decided, trail)
                })
        }
    };
    trail.pop();
    decided.insert(name.to_string(), yes);
    yes
}

fn field_crosses(
    ty: &CType,
    bodies: &std::collections::HashMap<&str, &CStruct>,
    underlying: &std::collections::HashMap<&str, &CType>,
    decided: &mut std::collections::HashMap<String, bool>,
    trail: &mut Vec<String>,
) -> bool {
    match ty {
        // A pointer needs no shape from what it points at, unless what it
        // points at is a function: no Dux type names one yet, so no field
        // may hold one.
        CType::FuncPtr { .. } => false,
        CType::Pointer(target) => !matches!(resolve(target), CType::FuncPtr { .. }),
        CType::Array(element, _) => field_crosses(element, bodies, underlying, decided, trail),
        CType::Struct(name) | CType::Union(name) => {
            bodies.contains_key(name.as_str())
                && record_crosses(name, bodies, underlying, decided, trail)
        }
        CType::Typedef(_, target) => field_crosses(target, bodies, underlying, decided, trail),
        CType::Opaque(_) | CType::Void => false,
        _ => true,
    }
}

fn spell_struct(ctx: &Ctx, record: &CStruct) -> Option<String> {
    if record.union || !is_plain_name(&record.name) {
        return None;
    }
    // An opaque forward declaration gets an empty body. It exists only
    // behind a pointer; Dux needs the name to exist so `*FILE` can mean
    // something, and nobody may ask the body for its size. A record whose
    // fields cannot cross is given the same body for the same reason.
    if record.size.is_none() || ctx.opaque.contains(&record.name) {
        return Some(format!("struct {} {{}}", record.name));
    }
    let mut body = String::new();
    let mut width = 0u64;
    for field in &record.fields {
        if field.bits.is_some() || !is_plain_name(&field.name) {
            return None;
        }
        let spelled = spelling(ctx, &field.ty)?;
        body.push_str(&format!("    {}: {spelled},\n", safe(&field.name)));
        width += 1;
    }
    if width == 0 {
        return None;
    }
    Some(format!("struct {} {{\n{body}}}", record.name))
}

fn spell_function(ctx: &Ctx, function: &CFunction) -> Option<String> {
    let mut params = Vec::new();
    for (at, (name, ty)) in function.params.iter().enumerate() {
        let spelled = spelling(ctx, ty)?;
        // C lets a declaration leave a parameter unnamed; Dux always names
        // them, and the name of an extern parameter is nobody's business.
        let named = if name.is_empty() {
            format!("_{at}: {spelled}")
        } else {
            format!("{}: {spelled}", safe(name))
        };
        params.push(named);
    }
    let head = format!("fn {}({})", function.name, params.join(", "));
    match &function.returns {
        None => Some(head),
        Some(ty) => Some(format!("{head} -> {}", spelling(ctx, ty)?)),
    }
}

fn spell_macro(macro_value: &crate::CMacro) -> Option<String> {
    match &macro_value.value {
        MacroValue::Int(value) => {
            // The value says how wide it is. C's biggest unsigned constants
            // (`UINTPTR_MAX` and friends) have no signed home at all, and
            // `u64` is where they fit.
            let int = if *value >= i32::MIN as i128 && *value <= i32::MAX as i128 {
                "i32"
            } else if *value >= i64::MIN as i128 && *value <= i64::MAX as i128 {
                "i64"
            } else if *value >= 0 && *value <= u64::MAX as i128 {
                "u64"
            } else {
                return None;
            };
            Some(format!(
                "const {}: {int} = {value}",
                safe(&macro_value.name)
            ))
        }
        MacroValue::Float(value) => {
            Some(format!("const {}: f64 = {value}", safe(&macro_value.name)))
        }
        MacroValue::Text(text) => Some(format!(
            "const {}: string = \"{}\"",
            safe(&macro_value.name),
            text
        )),
    }
}

/// The Dux spelling of a C type, or `None` when the table has no crossing
/// for it today. Pointers arrive as `Option<*T>`: NULL means "no value" far
/// more often than it means "crash now" (DESIGN 5.2).
/// What a spelled name may lean on: the enums whose integer type it needs
/// and the records and typedefs the table actually emitted.
struct Ctx {
    enums: Vec<CEnum>,
    records: std::collections::HashSet<String>,
    typedefs: std::collections::HashSet<String>,
    /// Records a body cannot be written for: one of their fields holds a
    /// union, an anonymous record, or another record with the same trouble.
    /// They cross by name with an empty body, the promise `FILE` makes:
    /// pointers travel, shapes do not.
    opaque: std::collections::HashSet<String>,
}

impl Ctx {
    fn new(decls: &[CDecl]) -> Self {
        let mut ctx = Ctx {
            enums: Vec::new(),
            records: std::collections::HashSet::new(),
            typedefs: std::collections::HashSet::new(),
            opaque: std::collections::HashSet::new(),
        };
        for decl in decls {
            match decl {
                CDecl::Enum(enumeration) => ctx.enums.push(enumeration.clone()),
                CDecl::Struct(record) if !record.union && is_plain_name(&record.name) => {
                    ctx.records.insert(record.name.clone());
                }
                _ => {}
            }
        }
        for decl in decls {
            if let CDecl::Typedef(definition) = decl {
                if is_plain_name(&definition.name) && !is_exact_width(&definition.name) && {
                    let base = resolve(&definition.underlying);
                    is_primitive(base) || ctx.has_record(base)
                } {
                    ctx.typedefs.insert(definition.name.clone());
                }
            }
        }
        let mut bodies = std::collections::HashMap::new();
        let mut underlying = std::collections::HashMap::new();
        for decl in decls {
            match decl {
                CDecl::Struct(record) if !record.union => {
                    bodies.insert(record.name.as_str(), record);
                }
                CDecl::Typedef(definition) => {
                    underlying.insert(definition.name.as_str(), &definition.underlying);
                }
                _ => {}
            }
        }
        for name in ctx.records.iter() {
            let mut decided = std::collections::HashMap::new();
            let mut trail = Vec::new();
            if !record_crosses(name, &bodies, &underlying, &mut decided, &mut trail) {
                ctx.opaque.insert(name.clone());
            }
        }
        ctx
    }

    fn has_record(&self, ty: &CType) -> bool {
        match ty {
            CType::Struct(name) | CType::Union(name) => self.records.contains(name),
            _ => false,
        }
    }

    fn enum_int(&self, name: &str) -> Option<&CType> {
        self.enums
            .iter()
            .find(|enumeration| enumeration.name == name)
            .map(|enumeration| &enumeration.int)
    }
}

fn spelling(ctx: &Ctx, ty: &CType) -> Option<String> {
    let simple: &'static str = match ty {
        CType::Void => return None,
        CType::Bool => "c_bool",
        CType::Char => "c_char",
        CType::SChar => "c_schar",
        CType::UChar => "c_uchar",
        CType::Short => "c_short",
        CType::UShort => "c_ushort",
        CType::Int => "c_int",
        CType::UInt => "c_uint",
        CType::Long => "c_long",
        CType::ULong => "c_ulong",
        CType::LongLong => "c_long_long",
        CType::ULongLong => "c_ulong_long",
        CType::Float => "c_float",
        CType::Double => "c_double",
        // C's widest float keeps a type of its own (DESIGN 5.2): there is no
        // IEEE shape for it to borrow, and pretending it is a `c_double`
        // would mislay both its size and its ABI.
        CType::LongDouble => "c_long_double",
        CType::Pointer(target) => {
            if matches!(resolve(target), CType::Void) {
                return Some("rawptr".to_string());
            }
            if matches!(resolve(target), CType::FuncPtr { .. }) {
                return None;
            }
            // A pointer to something with no Dux body (an opaque struct, a
            // clang-internal record) is still a pointer: it crosses as a
            // bare address rather than not crossing at all.
            let inner = match spelling(ctx, target) {
                Some(inner) => inner,
                None if matches!(
                    resolve(target),
                    CType::Opaque(_) | CType::Struct(_) | CType::Union(_)
                ) =>
                {
                    return Some("rawptr".to_string());
                }
                None => return None,
            };
            return Some(format!("Option<*{inner}>"));
        }
        CType::Struct(name) | CType::Union(name) => {
            if !is_plain_name(name) {
                return None;
            }
            return Some(name.clone());
        }
        CType::Enum(name) => {
            return spelling(ctx, ctx.enum_int(name)?);
        }
        CType::Array(element, count) => {
            let inner = spelling(ctx, element)?;
            return Some(format!("[{inner}; {count}]"));
        }
        CType::FuncPtr {
            params,
            returns,
            variadic,
        } => {
            if *variadic {
                return None;
            }
            let mut parts = Vec::new();
            for param in params {
                parts.push(spelling(ctx, param)?);
            }
            return Some(match returns.as_deref() {
                None | Some(CType::Void) => format!("fn({})", parts.join(", ")),
                Some(ty) => format!("fn({}) -> {}", parts.join(", "), spelling(ctx, ty)?),
            });
        }
        CType::Typedef(name, underlying) => {
            if let Some(direct) = exact_width(name) {
                return Some(direct.to_string());
            }
            // A name the table emitted is a name Dux may use; one it had to
            // drop (clang's `__`-guarded internals) is see-through to what
            // it meant.
            if ctx.typedefs.contains(name) || ctx.records.contains(name) {
                return Some(name.clone());
            }
            return spelling(ctx, underlying);
        }
        CType::Opaque(_) => return None,
    };
    Some(simple.to_string())
}
