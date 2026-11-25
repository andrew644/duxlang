//! The mapping table (DESIGN 5.2), written once: `CDecl` in, Dux source
//! text out. The driver feeds this text through the same parse-check-emit
//! road as any other module body, so the importer's output is exactly the
//! declarations a program could have written by hand — because they are.

use crate::{CDecl, CEnum, CFunction, CStruct, CType, CTypedef, MacroValue};

/// Spell a namespace's worth of C declarations as Dux. A declaration the
/// table cannot cross today is left out, silently for now and loudly for
/// `dux cimports` later; nothing half-mapped sneaks through.
pub fn spell(decls: &[CDecl]) -> String {
    let mut out = String::new();
    let mut externals: Vec<&CFunction> = Vec::new();
    let mut types = Vec::new();
    let mut constants = Vec::new();
    let enums: Vec<&CEnum> = decls
        .iter()
        .filter_map(|decl| match decl {
            CDecl::Enum(e) => Some(e),
            _ => None,
        })
        .collect();
    for decl in decls {
        match decl {
            CDecl::Function(function) => externals.push(function),
            CDecl::Struct(record) => {
                if let Some(text) = spell_struct(record) {
                    types.push(text);
                }
            }
            CDecl::Typedef(definition) => {
                if let Some(text) = spell_typedef(definition) {
                    types.push(text);
                }
            }
            CDecl::Enum(enumeration) => {
                for (name, value, _) in &enumeration.constants {
                    let int =
                        spelling(&enumeration.int, &enums).unwrap_or_else(|| "c_int".to_string());
                    constants.push(format!("const {name}: {int} = {value}"));
                }
            }
            CDecl::Variable(_) => {}
            CDecl::Macro(macro_value) => {
                if let Some(text) = spell_macro(macro_value) {
                    constants.push(text);
                }
            }
        }
    }
    for text in types {
        out.push_str(&text);
        out.push('\n');
    }
    if !externals.is_empty() {
        out.push_str("extern \"C\" {\n");
        for function in externals {
            if let Some(text) = spell_function(function, &enums) {
                out.push_str("    ");
                out.push_str(&text);
                out.push('\n');
            }
        }
        out.push_str("}\n\n");
    }
    for text in constants {
        out.push_str(&text);
        out.push('\n');
    }
    out
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
        "size_t" | "uintptr_t" => "usize",
        "ssize_t" | "ptrdiff_t" | "intptr_t" => "isize",
        _ => return None,
    })
}

fn spell_typedef(definition: &CTypedef) -> Option<String> {
    if is_exact_width(&definition.name) {
        return None;
    }
    // Only typedefs of primitives get a line of their own: the alias adds a
    // name for something Dux spells directly. A typedef of a record is
    // spelled transparently as the record itself, so no line is needed and
    // a record the table cannot emit would leave a dangling name.
    if is_primitive(resolve(&definition.underlying)) {
        let spelled = spelling(&definition.underlying, &[])?;
        Some(format!("type {} = {spelled}", definition.name))
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

fn spell_struct(record: &CStruct) -> Option<String> {
    // An opaque forward declaration has no body to write: it exists only
    // behind a pointer, and the pointer spelling does not need a body.
    record.size?;
    if record.union {
        return None;
    }
    let mut body = String::new();
    let mut width = 0u64;
    for field in &record.fields {
        if field.bits.is_some() {
            return None;
        }
        let spelled = spelling(&field.ty, &[])?;
        body.push_str(&format!("    {}: {spelled},\n", field.name));
        width += 1;
    }
    if width == 0 {
        return None;
    }
    Some(format!("struct {} {{\n{body}}}", record.name))
}

fn spell_function(function: &CFunction, enums: &[&CEnum]) -> Option<String> {
    let mut params = Vec::new();
    for (name, ty) in &function.params {
        let spelled = spelling(ty, enums)?;
        let named = if name.is_empty() {
            format!(": {spelled}")
        } else {
            format!("{name}: {spelled}")
        };
        params.push(named);
    }
    let head = format!("fn {}({})", function.name, params.join(", "));
    match &function.returns {
        None => Some(head),
        Some(ty) => Some(format!("{head} -> {}", spelling(ty, enums)?)),
    }
}

fn spell_macro(macro_value: &crate::CMacro) -> Option<String> {
    match &macro_value.value {
        MacroValue::Int(value) => {
            let int = if *value >= i32::MIN as i128 && *value <= i32::MAX as i128 {
                "i32"
            } else {
                "i64"
            };
            Some(format!("const {}: {int} = {value}", macro_value.name))
        }
        MacroValue::Float(value) => Some(format!("const {}: f64 = {value}", macro_value.name)),
        MacroValue::Text(text) => {
            Some(format!("const {}: string = \"{}\"", macro_value.name, text))
        }
    }
}

/// The Dux spelling of a C type, or `None` when the table has no crossing
/// for it today. Pointers arrive as `Option<*T>`: NULL means "no value" far
/// more often than it means "crash now" (DESIGN 5.2).
fn spelling(ty: &CType, enums: &[&CEnum]) -> Option<String> {
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
        CType::LongDouble => return None,
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
            let inner = match spelling(target, enums) {
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
            if name.is_empty() {
                return None;
            }
            return Some(name.clone());
        }
        CType::Enum(name) => {
            let found = enums.iter().find(|e| e.name == *name)?;
            return spelling(&found.int, enums);
        }
        CType::Array(element, count) => {
            let inner = spelling(element, enums)?;
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
                parts.push(spelling(param, enums)?);
            }
            return Some(match returns.as_deref() {
                None | Some(CType::Void) => format!("fn({})", parts.join(", ")),
                Some(ty) => format!("fn({}) -> {}", parts.join(", "), spelling(ty, enums)?),
            });
        }
        CType::Typedef(name, underlying) => {
            if let Some(direct) = exact_width(name) {
                return Some(direct.to_string());
            }
            return spelling(underlying, enums);
        }
        CType::Opaque(_) => return None,
    };
    Some(simple.to_string())
}
