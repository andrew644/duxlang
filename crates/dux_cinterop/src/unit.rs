//! The translation unit the driver compiles. Reading a header tells Dux
//! what is *there*; this file is what makes it *exist*: the include, plus
//! an external-linkage wrapper for every `static inline` definition the
//! header carries. C keeps those functions private to the translation
//! unit that uses them, and a Dux program calling one is the use the
//! header-only library was waiting for — but the symbol has to leave the
//! unit with a name LLVM can find, so each one gets a doorway.

use crate::{CDecl, CFunction, CType};

/// The wrapper symbol for a `static inline` definition, or `None` when the
/// signature cannot be rewritten in C (a vararg `static inline`, an enum
/// at the boundary). An unwrappable function is not imported at all: a
/// declaration whose definition can never arrive is a link error waiting
/// to happen, and the table does not write those.
pub fn wrapper_name(namespace: &str, function: &CFunction) -> Option<String> {
    if function.variadic {
        return None;
    }
    if !wraps_record(function) && !function.header_definition {
        return None;
    }
    // The namespace is part of the name: two headers can reach the same
    // declaration through their own includes (`stb_image.h` reads
    // `stdio.h` the way any library does), and two translation units
    // that each hand out a doorway need two doors.
    c_spelling(&function.returns.clone().unwrap_or(CType::Void))?;
    for (_, ty) in &function.params {
        c_spelling(ty)?;
    }
    Some(format!("__duxw_{namespace}_{}", function.name))
}

/// Whether a C function hands back a record by value. LLVM and clang do
/// not agree on how to spell one in IR — two floats are a `<2 x float>`
/// to one and a `{ float, float }` to the other — so Dux never asks a C
/// function for that shape directly. A wrapper takes the result's address
/// instead, and the one spelling works for every record on every ABI.
pub fn returns_record(returns: Option<&CType>) -> bool {
    match returns {
        Some(CType::Struct(_)) => true,
        Some(CType::Typedef(_, target)) => matches!(**target, CType::Struct(_)),
        _ => false,
    }
}

/// Whether a record crosses this function's boundary by value, in either
/// direction. Such a function is called through a wrapper: LLVM and clang
/// disagree about the shape of a by-value record often enough to matter
/// (two floats are a vector to one and a pair to the other), and one
/// spelling — results through an address the caller reserves, records in
/// through pointers — is right on every ABI.
pub fn wraps_record(function: &CFunction) -> bool {
    if function.variadic {
        return false;
    }
    returns_record(function.returns.as_ref())
        || function
            .params
            .iter()
            .any(|(_, ty)| returns_record(Some(ty)))
}

/// The C source: one include per header the reader was given, and one
/// wrapper per wrappable definition. The wrappers are what `#[c_name]` on
/// the spelled declaration points at. A namespace built from several
/// headers compiles as one translation unit, so a definition two headers
/// both reach is wrapped exactly once.
pub fn c_source(namespace: &str, headers: &[String], decls: &[CDecl]) -> String {
    let mut out = String::new();
    for header in headers {
        out.push_str(&format!("#include \"{header}\"\n"));
    }
    for decl in decls {
        let CDecl::Function(function) = decl else {
            continue;
        };
        let record = returns_record(function.returns.as_ref());
        if !wraps_record(function) && !function.header_definition {
            continue;
        }
        let Some(wrapper) = wrapper_name(namespace, function) else {
            continue;
        };
        let mut params = String::new();
        let mut arguments = String::new();
        for (at, (_, ty)) in function.params.iter().enumerate() {
            let spelled = c_spelling(ty).expect("wrapper_name checked this");
            if at > 0 {
                params.push_str(", ");
                arguments.push_str(", ");
            }
            // A record comes in through a pointer, and the call behind the
            // wrapper opens it up again. C sees exactly what it expects.
            if returns_record(Some(ty)) {
                params.push_str(&format!("{spelled} *__p{at}"));
                arguments.push_str(&format!("*__p{at}"));
            } else {
                params.push_str(&format!("{spelled} __p{at}"));
                arguments.push_str(&format!("__p{at}"));
            }
        }
        // The parentheses are load-bearing: they stop a function-like macro
        // from swallowing the call, which a real header (stb's `stbi__err`)
        // really does.
        let call = format!("({})({arguments})", function.name);
        match &function.returns {
            None | Some(CType::Void) => {
                out.push_str(&format!("void {wrapper}({params}) {{ {call}; }}\n"));
            }
            Some(returns) if record => {
                let spelled = c_spelling(returns).expect("wrapper_name checked this");
                let head = if params.is_empty() {
                    format!("{spelled} *__dux_out")
                } else {
                    format!("{spelled} *__dux_out, {params}")
                };
                out.push_str(&format!(
                    "void {wrapper}({head}) {{ *__dux_out = {call}; }}\n"
                ));
            }
            Some(returns) => {
                let spelled = c_spelling(returns).expect("wrapper_name checked this");
                out.push_str(&format!(
                    "{spelled} {wrapper}({params}) {{ return {call}; }}\n"
                ));
            }
        }
    }
    out
}

/// The C spelling of an imported type — the mirror of `spell::spelling`,
/// written so a wrapper's prototype is byte-for-byte compatible with the
/// declaration the header itself wrote.
pub fn c_spelling(ty: &CType) -> Option<String> {
    let simple: &'static str = match ty {
        CType::Void => "void",
        CType::Bool => "_Bool",
        CType::Char => "char",
        CType::SChar => "signed char",
        CType::UChar => "unsigned char",
        CType::Short => "short",
        CType::UShort => "unsigned short",
        CType::Int => "int",
        CType::UInt => "unsigned int",
        CType::Long => "long",
        CType::ULong => "unsigned long",
        CType::LongLong => "long long",
        CType::ULongLong => "unsigned long long",
        CType::Float => "float",
        CType::Double => "double",
        CType::LongDouble => "long double",
        CType::Typedef(name, _) => return Some(name.clone()),
        CType::Struct(name) => return Some(format!("struct {name}")),
        CType::Union(name) => return Some(format!("union {name}")),
        // An enum at the boundary would need the tag, and the tag may not
        // exist (an anonymous enum behind a typedef). Those wait.
        CType::Enum(_) => return None,
        CType::Pointer(target) => return Some(format!("{} *", c_spelling(target)?)),
        CType::Array(..) => return None,
        CType::FuncPtr {
            params,
            returns,
            variadic,
        } => {
            if *variadic {
                return None;
            }
            let mut parts = String::new();
            for (at, param) in params.iter().enumerate() {
                if at > 0 {
                    parts.push_str(", ");
                }
                parts.push_str(&c_spelling(param)?);
            }
            return Some(match returns.as_deref() {
                None | Some(CType::Void) => format!("void (*)({})", parts),
                Some(inner) => format!("{} (*)({})", c_spelling(inner)?, parts),
            });
        }
        CType::Opaque(_) => return None,
    };
    Some(simple.to_string())
}
