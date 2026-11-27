//! The libclang half: a synthetic translation unit goes in, `CDecl` comes
//! out. Everything layout-shaped is asked of clang — sizes, alignments,
//! field offsets, bitfields, enum integer types — so the importer inherits
//! the target's C ABI instead of guessing at it.

use clang_sys::{
    self as clang, CXChildVisitResult, CXClientData, CXCursor, CXFile, CXTranslationUnit, CXType,
};

const SKIP: CXChildVisitResult = 2;
use dux_diag::{code, Diagnostic};
use std::ffi::{c_char, c_int, c_ulong, CStr, CString};

use crate::{
    CDecl, CEnum, CField, CFunction, CMacro, CStruct, CType, CTypedef, CVariable, Import,
    MacroValue, Seen, Skipped,
};

pub fn import(what: &Import) -> Result<Seen, Vec<Diagnostic>> {
    let source = format!("#include \"{header}\"\n", header = what.header);
    let main = b"dux_cinterop.c";
    let resource = resource_include();
    let mut owned: Vec<CString> = Vec::new();
    let fail: Vec<Diagnostic> = vec![bare(&what.header)];
    owned.push(CString::new(source.as_str()).map_err(|_| fail.clone())?);
    owned.push(CString::new(main as &[u8]).map_err(|_| fail.clone())?);
    for flag in &what.flags {
        owned.push(CString::new(flag.as_str()).map_err(|_| fail.clone())?);
    }
    if let Some(dir) = &resource {
        owned.push(CString::new(format!("-isystem{dir}")).expect("no interior nul"));
    }
    for path in &what.include_paths {
        owned.push(CString::new(format!("-I{path}")).map_err(|_| fail.clone())?);
    }
    // The reader reads headers; it does not judge them. Real ones warn.
    owned.push(CString::new("-w").expect("no interior nul"));
    let argv: Vec<*const c_char> = owned
        .iter()
        .skip(2)
        .map(|argument| argument.as_ptr())
        .collect();
    let mut unsaved = clang::CXUnsavedFile {
        Filename: owned[1].as_ptr(),
        Contents: owned[0].as_ptr(),
        Length: source.len() as c_ulong,
    };
    unsafe {
        let index = clang::clang_createIndex(0, 0);
        let unit = clang::clang_parseTranslationUnit(
            index,
            owned[1].as_ptr(),
            argv.as_ptr(),
            argv.len() as c_int,
            &mut unsaved,
            1,
            clang::CXTranslationUnit_DetailedPreprocessingRecord,
        );
        if unit.is_null() {
            clang::clang_disposeIndex(index);
            return Err(vec![bare(&what.header)]);
        }
        let mut problems = Vec::new();
        let count = clang::clang_getNumDiagnostics(unit);
        for at in 0..count {
            let diagnostic = clang::clang_getDiagnostic(unit, at);
            let severity = clang::clang_getDiagnosticSeverity(diagnostic);
            if severity >= clang::CXDiagnostic_Error {
                problems.push(Diagnostic::error(
                    code::CODEGEN_FAILED,
                    format!(
                        "`{header}` did not parse: {text}",
                        header = what.header,
                        text = string(clang::clang_formatDiagnostic(
                            diagnostic,
                            clang::CXDiagnostic_DisplaySourceLocation
                                | clang::CXDiagnostic_DisplayColumn
                        ))
                    ),
                ));
            }
            clang::clang_disposeDiagnostic(diagnostic);
        }
        if !problems.is_empty() {
            clang::clang_disposeTranslationUnit(unit);
            clang::clang_disposeIndex(index);
            return Err(problems);
        }
        let main_file = clang::clang_getFile(unit, owned[1].as_ptr());
        let root = clang::clang_getTranslationUnitCursor(unit);
        let mut seen: Vec<CDecl> = Vec::new();
        let mut missed: Vec<Skipped> = Vec::new();
        let mut carry = Carry {
            unit,
            main_file,
            seen: &mut seen,
            missed: &mut missed,
        };
        clang::clang_visitChildren(root, visit, &mut carry as *mut Carry as CXClientData);
        clang::clang_disposeTranslationUnit(unit);
        clang::clang_disposeIndex(index);
        Ok(Seen {
            decls: dedup(carry.seen.split_off(0)),
            skipped: carry.missed.split_off(0),
        })
    }
}

struct Carry<'a> {
    unit: CXTranslationUnit,
    main_file: CXFile,
    seen: &'a mut Vec<CDecl>,
    missed: &'a mut Vec<Skipped>,
}

extern "C" fn visit(cursor: CXCursor, _parent: CXCursor, data: CXClientData) -> CXChildVisitResult {
    let carry = unsafe { &mut *(data as *mut Carry) };
    let kind = unsafe { clang::clang_getCursorKind(cursor) };
    if skip(carry.main_file, cursor) {
        return SKIP;
    }
    let named = string(unsafe { clang::clang_getCursorSpelling(cursor) });
    if named.is_empty() || named.starts_with("__") {
        return SKIP;
    }
    let found = match kind {
        k if k == clang::CXCursor_FunctionDecl => function(carry.unit, cursor, &named),
        k if k == clang::CXCursor_StructDecl || k == clang::CXCursor_UnionDecl => Some(
            CDecl::Struct(record(cursor, &named, k == clang::CXCursor_UnionDecl)),
        ),
        k if k == clang::CXCursor_TypedefDecl => Some(CDecl::Typedef(CTypedef {
            name: named,
            underlying: from_type(unsafe { clang::clang_getTypedefDeclUnderlyingType(cursor) }),
        })),
        k if k == clang::CXCursor_EnumDecl => Some(CDecl::Enum(enumeration(cursor, &named))),
        // A variable declared inside a function body belongs to that body,
        // not to the header's interface; it never reaches the report.
        k if k == clang::CXCursor_VarDecl
            && unsafe { clang::clang_getCursorSemanticParent(cursor) }.kind
                == clang::CXCursor_FunctionDecl =>
        {
            None
        }
        k if k == clang::CXCursor_VarDecl => Some(CDecl::Variable(CVariable {
            name: named,
            file_local: (unsafe { clang::clang_Cursor_getStorageClass(cursor) })
                == clang::CX_SC_Static,
            ty: from_type(unsafe { clang::clang_getCursorType(cursor) }),
        })),
        k if k == clang::CXCursor_MacroDefinition => {
            match macro_value(carry.unit, cursor, &named) {
                Some(value) => Some(CDecl::Macro(CMacro { name: named, value })),
                None => {
                    let why = if unsafe { clang::clang_Cursor_isMacroFunctionLike(cursor) } != 0 {
                        "its parameter list is not a list of names"
                    } else {
                        "the macro does not fold to a constant"
                    };
                    carry.missed.push(Skipped {
                        name: named,
                        reason: why.to_string(),
                    });
                    None
                }
            }
        }
        _ => None,
    };
    if let Some(decl) = found {
        carry.seen.push(decl);
    }
    SKIP
}

/// Built-ins, the synthetic main file, and anything the program never named
/// have no business in a namespace.
fn skip(main_file: CXFile, cursor: CXCursor) -> bool {
    let at = unsafe { clang::clang_getCursorLocation(cursor) };
    let mut file: CXFile = std::ptr::null_mut();
    unsafe {
        clang::clang_getSpellingLocation(
            at,
            &mut file,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
    }
    file.is_null() || file == main_file
}

fn function(_unit: CXTranslationUnit, cursor: CXCursor, named: &str) -> Option<CDecl> {
    let ty = unsafe { clang::clang_getCursorType(cursor) };
    let mut params = Vec::new();
    unsafe {
        clang::clang_visitChildren(
            cursor,
            collect_params,
            &mut params as *mut Vec<(String, CType)> as CXClientData,
        );
    }
    let result = from_type(unsafe { clang::clang_getResultType(ty) });
    let linkage = unsafe { clang::clang_getCursorLinkage(cursor) };
    let defined = unsafe { clang::clang_isCursorDefinition(cursor) } != 0;
    Some(CDecl::Function(CFunction {
        name: named.to_string(),
        params,
        returns: if result == CType::Void {
            None
        } else {
            Some(result)
        },
        variadic: unsafe { clang::clang_isFunctionTypeVariadic(ty) } != 0,
        header_definition: defined && linkage == clang::CXLinkage_Internal,
    }))
}

extern "C" fn collect_params(
    cursor: CXCursor,
    _parent: CXCursor,
    data: CXClientData,
) -> CXChildVisitResult {
    if unsafe { clang::clang_getCursorKind(cursor) } != clang::CXCursor_ParmDecl {
        return clang::CXChildVisit_Continue;
    }
    let carry = unsafe { &mut *(data as *mut Vec<(String, CType)>) };
    let named = string(unsafe { clang::clang_getCursorSpelling(cursor) });
    let ty = from_type(unsafe { clang::clang_getCursorType(cursor) });
    carry.push((named, ty));
    clang::CXChildVisit_Continue
}

fn record(cursor: CXCursor, named: &str, union: bool) -> CStruct {
    let ty = unsafe { clang::clang_getCursorType(cursor) };
    let size = unsafe { clang::clang_Type_getSizeOf(ty) };
    let align = unsafe { clang::clang_Type_getAlignOf(ty) };
    let mut fields: Vec<CField> = Vec::new();
    unsafe {
        clang::clang_visitChildren(
            cursor,
            collect_fields,
            &mut fields as *mut Vec<CField> as CXClientData,
        );
    }
    CStruct {
        name: named.to_string(),
        union,
        size: if size < 0 { None } else { Some(size as u64) },
        align: if align < 0 { None } else { Some(align as u64) },
        fields,
    }
}

extern "C" fn collect_fields(
    cursor: CXCursor,
    _parent: CXCursor,
    data: CXClientData,
) -> CXChildVisitResult {
    if unsafe { clang::clang_getCursorKind(cursor) } != clang::CXCursor_FieldDecl {
        return clang::CXChildVisit_Continue;
    }
    let carry = unsafe { &mut *(data as *mut Vec<CField>) };
    let named = string(unsafe { clang::clang_getCursorSpelling(cursor) });
    if named.is_empty() {
        return clang::CXChildVisit_Continue;
    }
    let base = unsafe { clang::clang_getCursorType(clang::clang_getCursorSemanticParent(cursor)) };
    let offset_bits = unsafe { clang::clang_Type_getOffsetOf(base, cstring(&named).as_ptr()) };
    let width = unsafe { clang::clang_getFieldDeclBitWidth(cursor) };
    let ty = from_type(unsafe { clang::clang_getCursorType(cursor) });
    let field = if width >= 0 && offset_bits >= 0 {
        CField {
            name: named,
            ty,
            offset: None,
            bits: Some((offset_bits as u32, width as u32)),
        }
    } else if offset_bits >= 0 {
        CField {
            name: named,
            ty,
            offset: Some((offset_bits / 8) as u64),
            bits: None,
        }
    } else {
        CField {
            name: named,
            ty,
            offset: None,
            bits: None,
        }
    };
    carry.push(field);
    clang::CXChildVisit_Continue
}

fn enumeration(cursor: CXCursor, named: &str) -> CEnum {
    let int = from_type(unsafe { clang::clang_getEnumDeclIntegerType(cursor) });
    let unsigned = int_is_unsigned(&int);
    let ty = unsafe { clang::clang_getCursorType(cursor) };
    let size = unsafe { clang::clang_Type_getSizeOf(ty) };
    let mut constants: Vec<(String, i64, bool)> = Vec::new();
    let mut carry = (unsigned, &mut constants);
    unsafe {
        clang::clang_visitChildren(
            cursor,
            collect_constants,
            &mut carry as *mut (bool, &mut Vec<(String, i64, bool)>) as CXClientData,
        );
    }
    CEnum {
        name: named.to_string(),
        int,
        size: if size < 0 { None } else { Some(size as u64) },
        constants,
    }
}

fn int_is_unsigned(int: &CType) -> bool {
    matches!(
        int,
        CType::Bool | CType::UChar | CType::UShort | CType::UInt | CType::ULong | CType::ULongLong
    )
}

extern "C" fn collect_constants(
    cursor: CXCursor,
    _parent: CXCursor,
    data: CXClientData,
) -> CXChildVisitResult {
    if unsafe { clang::clang_getCursorKind(cursor) } != clang::CXCursor_EnumConstantDecl {
        return clang::CXChildVisit_Continue;
    }
    let carry = unsafe { &mut *(data as *mut (bool, &mut Vec<(String, i64, bool)>)) };
    let named = string(unsafe { clang::clang_getCursorSpelling(cursor) });
    let number = if carry.0 {
        (unsafe { clang::clang_getEnumConstantDeclUnsignedValue(cursor) }) as i64
    } else {
        unsafe { clang::clang_getEnumConstantDeclValue(cursor) }
    };
    carry.1.push((named, number, carry.0));
    clang::CXChildVisit_Continue
}

fn macro_value(unit: CXTranslationUnit, cursor: CXCursor, _named: &str) -> Option<MacroValue> {
    let extent = unsafe { clang::clang_getCursorExtent(cursor) };
    let tokens = tokenize(unit, extent);
    if (unsafe { clang::clang_Cursor_isMacroFunctionLike(cursor) }) != 0 {
        return function_macro_shape(&tokens);
    }
    fold(tokens.iter().map(String::as_str).skip(1))
}

/// A function-like macro definition reads `NAME ( params ) body`; the
/// parameter names and the body tokens travel on, and the spelling layer
/// decides whether the body is a Dux expression. A variadic macro (`...`
/// among the parameters) has no list to hand on.
fn function_macro_shape(tokens: &[String]) -> Option<MacroValue> {
    let piece = |at: usize| {
        tokens
            .get(at)
            .map(String::as_str)
            .filter(|one| *one != "\u{0}")
    };
    let open = match piece(1)? {
        "(" => 2,
        _ => return None,
    };
    let mut params = Vec::new();
    let mut at = open;
    loop {
        match piece(at)? {
            ")" => {
                at += 1;
                break;
            }
            "," => {
                at += 1;
            }
            one if is_identifier(one) => {
                params.push(one.to_string());
                at += 1;
                if matches!(piece(at), Some(",") | Some(")")) {
                    continue;
                }
                return None;
            }
            _ => return None,
        }
    }
    let body: Vec<String> = tokens[at..]
        .iter()
        .filter(|one| *one != "\u{0}")
        .map(String::to_string)
        .collect();
    Some(MacroValue::Function { params, body })
}

fn is_identifier(piece: &str) -> bool {
    !piece.is_empty()
        && !piece.starts_with(|c: char| c.is_ascii_digit())
        && piece.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn tokenize(unit: CXTranslationUnit, extent: clang::CXSourceRange) -> Vec<String> {
    let mut count: core::ffi::c_uint = 0;
    let mut tokens: *mut clang::CXToken = std::ptr::null_mut();
    unsafe {
        clang::clang_tokenize(unit, extent, &mut tokens, &mut count);
    }
    let room = count as usize;
    let mut out = Vec::with_capacity(room);
    for at in 0..room {
        let token = unsafe { *tokens.add(at) };
        if unsafe { clang::clang_getTokenKind(token) } == clang::CXToken_Comment {
            continue;
        }
        out.push(string(unsafe {
            clang::clang_getTokenSpelling(unit, token)
        }));
    }
    unsafe { clang::clang_disposeTokens(unit, tokens, count) };
    out
}

/// Fold a token sequence over integer and float literals, `+ - * /`,
/// parentheses and string literals. Anything else is not a constant.
fn fold<'a>(tokens: impl Iterator<Item = &'a str>) -> Option<MacroValue> {
    let pieces: Vec<&str> = tokens.filter(|piece| *piece != "\u{0}").collect();
    let mut peek = pieces.iter().peekable();
    let value = sum(&mut peek)?;
    if peek.peek().is_some() {
        return None;
    }
    Some(value)
}

fn sum<'a>(peek: &mut std::iter::Peekable<std::slice::Iter<'a, &'a str>>) -> Option<MacroValue> {
    let mut left = product(peek)?;
    loop {
        match peek.peek().map(|piece| **piece) {
            Some("+") => {
                peek.next()?;
                left = combine(left, product(peek)?, |a, b| a + b, |a, b| a + b)?;
            }
            Some("-") => {
                peek.next()?;
                left = combine(left, product(peek)?, |a, b| a - b, |a, b| a - b)?;
            }
            _ => return Some(left),
        }
    }
}

fn product<'a>(
    peek: &mut std::iter::Peekable<std::slice::Iter<'a, &'a str>>,
) -> Option<MacroValue> {
    let mut left = atom(peek)?;
    loop {
        match peek.peek().map(|piece| **piece) {
            Some("*") => {
                peek.next()?;
                left = combine(left, atom(peek)?, |a, b| a.wrapping_mul(b), |a, b| a * b)?;
            }
            Some("/") => {
                peek.next()?;
                let right = atom(peek)?;
                match (left, right) {
                    (MacroValue::Int(a), MacroValue::Int(b)) if b != 0 => {
                        left = MacroValue::Int(a.wrapping_div(b));
                    }
                    (MacroValue::Float(a), MacroValue::Float(b)) if b != 0.0 => {
                        left = MacroValue::Float(a / b);
                    }
                    _ => return None,
                }
            }
            _ => return Some(left),
        }
    }
}

fn combine(
    left: MacroValue,
    right: MacroValue,
    ints: impl Fn(i128, i128) -> i128,
    floats: impl Fn(f64, f64) -> f64,
) -> Option<MacroValue> {
    match (left, right) {
        (MacroValue::Int(a), MacroValue::Int(b)) => Some(MacroValue::Int(ints(a, b))),
        (MacroValue::Float(a), MacroValue::Float(b)) => Some(MacroValue::Float(floats(a, b))),
        (MacroValue::Int(a), MacroValue::Float(b)) => Some(MacroValue::Float(floats(a as f64, b))),
        (MacroValue::Float(a), MacroValue::Int(b)) => Some(MacroValue::Float(floats(a, b as f64))),
        _ => None,
    }
}

fn atom<'a>(peek: &mut std::iter::Peekable<std::slice::Iter<'a, &'a str>>) -> Option<MacroValue> {
    let piece = peek.next()?;
    match *piece {
        "(" => {
            let inside = sum(peek)?;
            if peek.next().copied() == Some(")") {
                Some(inside)
            } else {
                None
            }
        }
        "-" => {
            let inner = atom(peek)?;
            match inner {
                MacroValue::Int(value) => Some(MacroValue::Int(value.checked_neg()?)),
                MacroValue::Float(value) => Some(MacroValue::Float(-value)),
                _ => None,
            }
        }
        "+" => atom(peek),
        other => literal(other),
    }
}

pub(crate) fn literal(piece: &str) -> Option<MacroValue> {
    let bytes = piece.as_bytes();
    if bytes.len() >= 2
        && (bytes[0] == b'"' || bytes[0] == b'\'')
        && bytes[bytes.len() - 1] == bytes[0]
    {
        let inner = &piece[1..piece.len() - 1];
        return Some(MacroValue::Text(inner.to_string()));
    }
    let cleaned = strip_suffix(piece);
    if let Ok(value) = cleaned.parse::<i128>() {
        return Some(MacroValue::Int(value));
    }
    if cleaned.starts_with("0x") || cleaned.starts_with("0X") {
        let value = i128::from_str_radix(&cleaned[2..], 16).ok()?;
        return Some(MacroValue::Int(value));
    }
    if cleaned.contains('.') || cleaned.contains('e') || cleaned.contains('E') {
        return cleaned.parse::<f64>().ok().map(MacroValue::Float);
    }
    None
}

fn strip_suffix(piece: &str) -> &str {
    let mut end = piece.len();
    let bytes = piece.as_bytes();
    while end > 0 && matches!(bytes[end - 1], b'u' | b'U' | b'l' | b'L' | b'f' | b'F') {
        end -= 1;
    }
    &piece[..end]
}

fn from_type(ty: CXType) -> CType {
    let kind = ty.kind;
    match kind {
        k if k == clang::CXType_Void => CType::Void,
        k if k == clang::CXType_Bool => CType::Bool,
        k if k == clang::CXType_Char_S => CType::Char,
        k if k == clang::CXType_Char_U || k == clang::CXType_UChar => CType::UChar,
        k if k == clang::CXType_SChar => CType::SChar,
        k if k == clang::CXType_Short => CType::Short,
        k if k == clang::CXType_UShort => CType::UShort,
        k if k == clang::CXType_Int => CType::Int,
        k if k == clang::CXType_UInt => CType::UInt,
        k if k == clang::CXType_Long => CType::Long,
        k if k == clang::CXType_ULong => CType::ULong,
        k if k == clang::CXType_LongLong => CType::LongLong,
        k if k == clang::CXType_ULongLong => CType::ULongLong,
        k if k == clang::CXType_Float => CType::Float,
        k if k == clang::CXType_Double => CType::Double,
        k if k == clang::CXType_LongDouble => CType::LongDouble,
        k if k == clang::CXType_Pointer => CType::Pointer(Box::new(from_type(unsafe {
            clang::clang_getPointeeType(ty)
        }))),
        k if k == clang::CXType_ConstantArray => {
            let count = unsafe { clang::clang_getNumElements(ty) };
            CType::Array(
                Box::new(from_type(unsafe { clang::clang_getArrayElementType(ty) })),
                count.max(0) as u64,
            )
        }
        k if k == clang::CXType_FunctionProto || k == clang::CXType_FunctionNoProto => {
            let count = unsafe { clang::clang_getNumArgTypes(ty) };
            let mut params = Vec::new();
            for at in 0..count {
                params.push(from_type(unsafe { clang::clang_getArgType(ty, at as u32) }));
            }
            CType::FuncPtr {
                params,
                returns: Some(Box::new(from_type(unsafe {
                    clang::clang_getResultType(ty)
                }))),
                variadic: unsafe { clang::clang_isFunctionTypeVariadic(ty) } != 0,
            }
        }
        k if k == clang::CXType_Typedef => {
            let decl = unsafe { clang::clang_getTypeDeclaration(ty) };
            let name = string(unsafe { clang::clang_getCursorSpelling(decl) });
            let underlying = from_type(unsafe { clang::clang_getTypedefDeclUnderlyingType(decl) });
            CType::Typedef(name, Box::new(underlying))
        }
        k if k == clang::CXType_Record => {
            let decl = unsafe { clang::clang_getTypeDeclaration(ty) };
            let name = string(unsafe { clang::clang_getCursorSpelling(decl) });
            if name.is_empty() {
                return CType::Opaque(string(unsafe { clang::clang_getTypeSpelling(ty) }));
            }
            if unsafe { clang::clang_getCursorKind(decl) } == clang::CXCursor_UnionDecl {
                CType::Union(name)
            } else {
                CType::Struct(name)
            }
        }
        k if k == clang::CXType_Enum => {
            let decl = unsafe { clang::clang_getTypeDeclaration(ty) };
            CType::Enum(string(unsafe { clang::clang_getCursorSpelling(decl) }))
        }
        k if k == clang::CXType_Elaborated => {
            from_type(unsafe { clang::clang_Type_getNamedType(ty) })
        }
        _ => {
            // Newer clang reports some built-in and macro-made types under
            // kind codes this binding has never heard of (glibc's `size_t`
            // arrives as clang 19's "predefined sugar"). A canonical type
            // asks the same question in a vocabulary everyone shares; if
            // that answers with a different kind, read the answer.
            let canonical = unsafe { clang::clang_getCanonicalType(ty) };
            if canonical.kind != ty.kind {
                let inner = from_type(canonical);
                // The sugar had a name (`size_t` arrives this way from
                // glibc and the compiler's own predefinitions). Keep it:
                // the mapping table reads `size_t` and answers `usize`,
                // and that answer is only reachable through the name.
                let spelled = string(unsafe { clang::clang_getTypeSpelling(ty) });
                let plain = !spelled.is_empty()
                    && spelled
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_');
                return if plain
                    && spelled != string(unsafe { clang::clang_getTypeSpelling(canonical) })
                {
                    CType::Typedef(spelled, Box::new(inner))
                } else {
                    inner
                };
            }
            // A typedef clang would not name through the type still has a
            // declaration if it has one at all.
            let decl = unsafe { clang::clang_getTypeDeclaration(ty) };
            if unsafe { clang::clang_getCursorKind(decl) } == clang::CXCursor_TypedefDecl {
                let name = string(unsafe { clang::clang_getCursorSpelling(decl) });
                let underlying =
                    from_type(unsafe { clang::clang_getTypedefDeclUnderlyingType(decl) });
                return CType::Typedef(name, Box::new(underlying));
            }
            let spelling = string(unsafe { clang::clang_getTypeSpelling(ty) });
            if spelling == "void" {
                return CType::Void;
            }
            CType::Opaque(spelling)
        }
    }
}

fn string(value: clang::CXString) -> String {
    unsafe {
        let raw = clang::clang_getCString(value);
        let text = if raw.is_null() {
            String::new()
        } else {
            CStr::from_ptr(raw).to_string_lossy().into_owned()
        };
        clang::clang_disposeString(value);
        text
    }
}

fn cstring(text: &str) -> CString {
    CString::new(text).unwrap_or_else(|_| CString::new("").expect("empty"))
}

/// libclang locates its own bundled headers (`stddef.h` and friends) next
/// to the shared library. Fedora keeps them two directories away, so the
/// importer points at the newest installed copy when the auto-detection
/// misses.
fn resource_include() -> Option<String> {
    if std::env::var_os("DUX_CLANG_RESOURCE_DIR").is_some() {
        return std::env::var("DUX_CLANG_RESOURCE_DIR").ok();
    }
    let mut best: Option<(u32, String)> = None;
    for root in ["/usr/lib/clang", "/usr/lib64/clang", "/usr/local/lib/clang"] {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let version = entry.file_name().to_string_lossy().to_string();
            let head: String = version.chars().take_while(|c| c.is_ascii_digit()).collect();
            let Ok(major) = head.parse::<u32>() else {
                continue;
            };
            let include = entry.path().join("include");
            if include.is_dir() && best.as_ref().is_none_or(|(found, _)| major >= *found) {
                best = Some((major, include.to_string_lossy().into_owned()));
            }
        }
    }
    best.map(|(_, dir)| dir)
}

fn bare(header: &str) -> Diagnostic {
    Diagnostic::error(
        code::SOURCE_IO,
        format!("`{header}` could not be read by the C importer"),
    )
    .with_note("check the header name and the include paths")
}

fn dedup(decls: Vec<CDecl>) -> Vec<CDecl> {
    let mut out: Vec<CDecl> = Vec::with_capacity(decls.len());
    for decl in decls {
        let clash = out.iter().any(|other| {
            std::mem::discriminant(other) == std::mem::discriminant(&decl)
                && other.name() == decl.name()
        });
        if !clash {
            out.push(decl);
        }
    }
    out
}
