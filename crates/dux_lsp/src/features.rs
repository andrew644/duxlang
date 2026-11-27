//! What the server knows about a program, asked in the editor's terms.
//!
//! The compiler hands over diagnostics as it reads them; hover and
//! go-to-definition ask the parsed program for the name under the caret.
//! A name is found where a name is bound — a parameter, a `let`, a
//! declaration — and the answer carries the place that binding was
//! written, which may live in another file: an import's declarations
//! keep the spans of the file they were read from.

use std::path::PathBuf;

use serde_json::{json, Value};

use dux_ast::{Block, Decl, FnDecl, Program, Stmt, Type, VariantBody};
use dux_diag::{Diagnostic, Severity};
use dux_driver::Analyzed;
use dux_source::{FileId, SourceMap, Span};

/// The file a document's URI names, if the URI names a file at all.
pub fn path_of_uri(uri: &str) -> Option<PathBuf> {
    let path = uri.strip_prefix("file://")?;
    Some(PathBuf::from(percent_decode(path)))
}

/// The URI that reaches this file, when its name is already a full path.
pub fn uri_of_path(path: &str) -> Option<String> {
    if path.starts_with('/') {
        Some(format!("file://{path}"))
    } else {
        None
    }
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok();
            if let Some(byte) = hex.and_then(|hex| u8::from_str_radix(hex, 16).ok()) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The identifier the caret stands on: its name, the namespace written in
/// front of the dot if there was one, and where in the buffer it begins.
#[derive(Clone, Debug, PartialEq)]
pub struct Focus {
    pub name: String,
    pub home: Option<String>,
    pub start: usize,
}

impl Focus {
    pub fn end(&self) -> usize {
        self.start + self.name.len()
    }
}

fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// The focus of attention at an editor position (line and column, both
/// counted from zero). A caret on no name at all asks for nothing.
pub fn focus_at(text: &str, line: usize, character: usize) -> Option<Focus> {
    let offset = byte_offset(text, line, character)?;
    let bytes = text.as_bytes();
    let mut at = offset;
    if at >= bytes.len() || !is_name_byte(bytes[at]) {
        at = at.checked_sub(1)?;
    }
    if !is_name_byte(bytes[at]) {
        return None;
    }
    let mut start = at;
    while start > 0 && is_name_byte(bytes[start - 1]) {
        start -= 1;
    }
    let mut end = at + 1;
    while end < bytes.len() && is_name_byte(bytes[end]) {
        end += 1;
    }
    let name = text[start..end].to_string();
    let home = if start > 0 && bytes[start - 1] == b'.' {
        let mut home_start = start - 1;
        while home_start > 0 && is_name_byte(bytes[home_start - 1]) {
            home_start -= 1;
        }
        Some(text[home_start..start - 1].to_string())
    } else {
        None
    };
    Some(Focus { name, home, start })
}

fn byte_offset(text: &str, line: usize, character: usize) -> Option<usize> {
    let mut offset = 0;
    for _ in 0..line {
        let newline = text[offset..].find('\n')?;
        offset += newline + 1;
    }
    for (columns, (within, _)) in text[offset..].char_indices().enumerate() {
        if columns == character {
            return Some(offset + within);
        }
        if text.as_bytes()[offset + within] == b'\n' {
            break;
        }
    }
    Some((offset + text[offset..].find('\n').unwrap_or(text.len() - offset)).min(text.len()))
}

/// An answer to hover or go-to-definition: the signature worth showing
/// and the place the name was written.
#[derive(Clone, Debug, PartialEq)]
pub struct Answer {
    pub signature: String,
    pub file: FileId,
    pub span: Span,
}

/// Where the caret's name was bound. Locals answer first — a `let` hides
/// a declaration the same name would otherwise reach — and then the
/// program's declarations, the file's own and every import's.
pub fn lookup(analyzed: &Analyzed, focus: &Focus) -> Option<Answer> {
    let entry = analyzed.file;
    if focus.home.is_none() {
        if let Some(local) = local_binding(&analyzed.program, entry, focus) {
            return Some(local);
        }
    }
    declaration(&analyzed.program, focus)
}

fn local_binding(program: &Program, entry: FileId, focus: &Focus) -> Option<Answer> {
    let mut best: Option<(usize, Answer)> = None;
    let mut consider = |name: &dux_ast::Ident, ty: Option<&Type>, signature: String| {
        if name.name != focus.name || name.span.file != entry {
            return;
        }
        let written = name.span.start as usize;
        if written > focus.start {
            return;
        }
        let _ = ty;
        let answer = Answer {
            signature,
            file: entry,
            span: name.span,
        };
        if best.as_ref().is_none_or(|(at, _)| written >= *at) {
            best = Some((written, answer));
        }
    };
    for function in program.functions().chain(program.methods()) {
        if function_inside(function, entry, focus.start) {
            for param in &function.params {
                consider(
                    &param.name,
                    Some(&param.ty),
                    format!("{}: {}", param.name.name, type_text(&param.ty)),
                );
            }
            bindings_in(&function.body.stmts, &mut consider);
        }
    }
    best.map(|(_, answer)| answer)
}

fn bindings_in(stmts: &[Stmt], consider: &mut impl FnMut(&dux_ast::Ident, Option<&Type>, String)) {
    for stmt in stmts {
        match stmt {
            Stmt::Binding { name, ty, .. } => consider(
                name,
                ty.as_ref(),
                match ty {
                    Some(ty) => format!("{}: {} = …", name.name, type_text(ty)),
                    None => format!("{} := …", name.name),
                },
            ),
            Stmt::For {
                kind: dux_ast::ForKind::In { index, value, .. },
                ..
            } => {
                if let Some(index) = index {
                    consider(index, None, format!("for {}: … in …", index.name));
                }
                consider(value, None, format!("for {} in …", value.name));
            }
            Stmt::Match { arms, .. } => {
                for arm in arms {
                    for binder in arm.pattern.binders() {
                        consider(&binder, None, format!("match {}", binder.name));
                    }
                    match &arm.body {
                        dux_ast::MatchBody::Block(block) => bindings_in(&block.stmts, consider),
                        dux_ast::MatchBody::Stmt(stmt) => {
                            bindings_in(std::slice::from_ref(stmt), consider)
                        }
                        dux_ast::MatchBody::Expr(_) => {}
                    }
                }
            }
            other => {
                if let Some(block) = block_of(other) {
                    bindings_in(&block.stmts, consider);
                }
                if let Stmt::If {
                    else_branch: Some(else_stmt),
                    ..
                } = other
                {
                    bindings_in(std::slice::from_ref(else_stmt), consider);
                }
                if let Stmt::Defer { stmt, .. } = other {
                    bindings_in(std::slice::from_ref(stmt), consider);
                }
            }
        }
    }
}

fn block_of(stmt: &Stmt) -> Option<&Block> {
    match stmt {
        Stmt::Block { block, .. } => Some(block),
        Stmt::If { then_branch, .. } => Some(then_branch),
        Stmt::While { body, .. } => Some(body),
        Stmt::Loop { body, .. } => Some(body),
        Stmt::For { body, .. } => Some(body),
        Stmt::Defer { stmt, .. } => block_of(stmt),
        _ => None,
    }
}

fn function_inside(function: &FnDecl, entry: FileId, offset: usize) -> bool {
    let body = function.body.span;
    body.file == entry
        && body.start as usize <= offset
        && offset <= body.start as usize + body.len as usize
}

/// A named declaration the focus can reach: the program's own, an
/// import's, and the functions an `extern` block lends.
fn declaration(program: &Program, focus: &Focus) -> Option<Answer> {
    let wanted_home = match &focus.home {
        Some(home) => format!("{home}."),
        None => String::new(),
    };
    let mut found: Option<Answer> = None;
    visit_decls(&program.decls, "", &wanted_home, focus, &mut found);
    found
}

fn visit_decls(
    decls: &[Decl],
    home: &str,
    wanted_home: &str,
    focus: &Focus,
    found: &mut Option<Answer>,
) {
    for decl in decls {
        if found.is_some() {
            return;
        }
        match decl {
            Decl::Module(module) => {
                if home == wanted_home && module.name.name == focus.name {
                    *found = Some(Answer {
                        signature: format!("mod {}", module.name.name),
                        file: module.name.span.file,
                        span: module.name.span,
                    });
                    return;
                }
                let nested = format!("{home}{}.", module.name.name);
                visit_decls(&module.decls, &nested, wanted_home, focus, found);
            }
            Decl::Extern(block) => {
                for function in &block.functions {
                    check(
                        &function.name,
                        home,
                        wanted_home,
                        focus,
                        fn_signature(function),
                        found,
                    );
                }
            }
            Decl::Impl(implementation) => {
                for method in &implementation.methods {
                    check(
                        &method.name,
                        home,
                        wanted_home,
                        focus,
                        fn_signature(method),
                        found,
                    );
                }
            }
            other => {
                let (name, signature) = match other {
                    Decl::Fn(function) => (Some(&function.name), Some(fn_signature(function))),
                    Decl::Struct(record) => (
                        Some(&record.name),
                        Some(format!(
                            "record {}{} {{\n{}\n}}",
                            record.name.name,
                            type_params(&record.type_params),
                            record
                                .fields
                                .iter()
                                .map(|field| {
                                    format!("    {}: {}", field.name.name, type_text(&field.ty))
                                })
                                .collect::<Vec<_>>()
                                .join(",\n")
                        )),
                    ),
                    Decl::Enum(enumeration) => (
                        Some(&enumeration.name),
                        Some(format!(
                            "enum {}{} {{ {} }}",
                            enumeration.name.name,
                            type_params(&enumeration.type_params),
                            enumeration
                                .variants
                                .iter()
                                .map(|variant| match &variant.body {
                                    VariantBody::Empty => variant.name.name.clone(),
                                    VariantBody::Tuple(types) => format!(
                                        "({})",
                                        types.iter().map(type_text).collect::<Vec<_>>().join(", ")
                                    ),
                                    VariantBody::Named(fields) => format!(
                                        " {{ {} }}",
                                        fields
                                            .iter()
                                            .map(|field| {
                                                format!(
                                                    "{}: {}",
                                                    field.name.name,
                                                    type_text(&field.ty)
                                                )
                                            })
                                            .collect::<Vec<_>>()
                                            .join(", ")
                                    ),
                                })
                                .collect::<Vec<_>>()
                                .join(", ")
                        )),
                    ),
                    Decl::Const(constant) => (
                        Some(&constant.name),
                        Some(format!(
                            "const {}{}",
                            constant.name.name,
                            constant
                                .ty
                                .as_ref()
                                .map(|ty| format!(": {}", type_text(ty)))
                                .unwrap_or_default()
                        )),
                    ),
                    Decl::Global(global) => (
                        Some(&global.name),
                        Some(format!(
                            "global {}{}",
                            global.name.name,
                            global
                                .ty
                                .as_ref()
                                .map(|ty| format!(": {}", type_text(ty)))
                                .unwrap_or_default()
                        )),
                    ),
                    Decl::TypeAlias(alias) => (
                        Some(&alias.name),
                        Some(format!(
                            "type {}{} = {}",
                            alias.name.name,
                            type_params(&alias.type_params),
                            type_text(&alias.aliased)
                        )),
                    ),
                    _ => (None, None),
                };
                if let (Some(name), Some(signature)) = (name, signature) {
                    check(name, home, wanted_home, focus, signature, found);
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn check(
    name: &dux_ast::Ident,
    home: &str,
    wanted_home: &str,
    focus: &Focus,
    signature: String,
    found: &mut Option<Answer>,
) {
    if name.name == focus.name && home == wanted_home {
        *found = Some(Answer {
            signature,
            file: name.span.file,
            span: name.span,
        });
    }
}

fn fn_signature(function: &FnDecl) -> String {
    format!(
        "fn {}{}({}){}",
        function.name.name,
        type_params(&function.type_params),
        function
            .params
            .iter()
            .map(|param| format!("{}: {}", param.name.name, type_text(&param.ty)))
            .collect::<Vec<_>>()
            .join(", "),
        function
            .return_ty
            .as_ref()
            .map(|ty| format!(" -> {}", type_text(ty)))
            .unwrap_or_default()
    )
}

fn type_params(params: &[dux_ast::TypeParam]) -> String {
    if params.is_empty() {
        return String::new();
    }
    format!(
        "<{}>",
        params
            .iter()
            .map(|param| if param.is_const {
                format!("const {}", param.name.name)
            } else {
                param.name.name.clone()
            })
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// The type as it was written, near enough to read.
pub fn type_text(ty: &Type) -> String {
    match ty {
        Type::Named { name, args, .. } => {
            if args.is_empty() {
                name.clone()
            } else {
                format!(
                    "{}<{}>",
                    name,
                    args.iter().map(type_text).collect::<Vec<_>>().join(", ")
                )
            }
        }
        Type::Pointer { pointee, .. } => format!("*{}", type_text(pointee)),
        Type::Slice { element, .. } => format!("[]{}", type_text(element)),
        Type::Array { element, .. } => format!("[{}; _]", type_text(element)),
        Type::Tuple { elements, .. } => format!(
            "({})",
            elements
                .iter()
                .map(type_text)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Type::Function {
            params, return_ty, ..
        } => format!(
            "fn({}){}",
            params.iter().map(type_text).collect::<Vec<_>>().join(", "),
            return_ty
                .as_ref()
                .map(|ty| format!(" -> {}", type_text(ty)))
                .unwrap_or_default()
        ),
        Type::ConstArgument { .. } => "_".to_string(),
        Type::Error { .. } => "?".to_string(),
    }
}

/// The diagnostics a build would report for this file, in the editor's
/// shape. Diagnostics that belong to another file — the prelude, an
/// import — stay out of this document's list.
pub fn diagnostics(analyzed: &Analyzed, uri: &str) -> Vec<Value> {
    let Some(wanted) = analyzed
        .sources
        .find(
            &path_of_uri(uri)
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
        )
        .or_else(|| analyzed.sources.find(uri))
    else {
        return Vec::new();
    };
    diagnostics_for(&analyzed.sources, wanted, &analyzed.diagnostics)
}

pub fn diagnostics_for(
    sources: &SourceMap,
    file: FileId,
    diagnostics: &[Diagnostic],
) -> Vec<Value> {
    diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.span.is_none_or(|span| span.file == file))
        .map(|diagnostic| {
            let range = diagnostic
                .span
                .map(|span| range_of(sources, span))
                .unwrap_or_else(whole_file);
            json!({
                "range": range,
                "severity": match diagnostic.severity {
                    Severity::Error => 1,
                    Severity::Warning => 2,
                },
                "code": diagnostic.code,
                "source": "dux",
                "message": diagnostic.message,
            })
        })
        .collect()
}

/// A span in the editor's counting: lines and columns from zero.
pub fn range_of(sources: &SourceMap, span: Span) -> Value {
    let (start_line, start_col) = sources.line_col(span.file, span.start);
    let (end_line, end_col) = sources.line_col(span.file, span.start + span.len);
    json!({
        "start": {"line": start_line - 1, "character": start_col - 1},
        "end": {"line": end_line - 1, "character": end_col - 1},
    })
}

/// The range a caret's name covers, for hover to underline.
pub fn focus_range(text: &str, focus: &Focus) -> Value {
    let (start_line, start_character) = position_in(text, focus.start);
    let (end_line, end_character) = position_in(text, focus.end());
    json!({
        "start": {"line": start_line, "character": start_character},
        "end": {"line": end_line, "character": end_character},
    })
}

fn position_in(text: &str, offset: usize) -> (usize, usize) {
    let offset = offset.min(text.len());
    let mut line = 0;
    let mut start = 0;
    while let Some(newline) = text[start..offset].find('\n') {
        start += newline + 1;
        line += 1;
    }
    let character = text[start..offset].chars().count();
    (line, character)
}

fn whole_file() -> Value {
    json!({
        "start": {"line": 0, "character": 0},
        "end": {"line": 0, "character": 0},
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_uri_and_a_path_find_each_other() {
        assert_eq!(
            path_of_uri("file:///tmp/a b.dux").unwrap(),
            PathBuf::from("/tmp/a b.dux")
        );
        assert_eq!(uri_of_path("/tmp/a.dux").unwrap(), "file:///tmp/a.dux");
        assert_eq!(uri_of_path("relative.dux"), None);
        assert_eq!(path_of_uri("vscode://something"), None);
    }

    #[test]
    fn the_caret_finds_its_name_and_its_namespace() {
        let text = "fn main() {\n    let cells = math.sqrt(2)\n}\n";
        let on_sqrt = focus_at(text, 1, 22).expect("a name under the caret");
        assert_eq!(on_sqrt.name, "sqrt");
        assert_eq!(on_sqrt.home.as_deref(), Some("math"));
        let on_cells = focus_at(text, 1, 10).expect("a name under the caret");
        assert_eq!(on_cells.name, "cells");
        assert_eq!(on_cells.home, None);
        assert!(
            focus_at(text, 1, 1).is_none(),
            "a caret in the indentation is on no name"
        );
    }

    #[test]
    fn positions_count_from_zero() {
        let text = "one\ntwo\nthree";
        assert_eq!(position_in(text, 0), (0, 0));
        assert_eq!(position_in(text, 4), (1, 0));
        assert_eq!(position_in(text, 8), (2, 0));
        assert_eq!(position_in(text, 10), (2, 2));
    }

    #[test]
    fn types_say_themselves() {
        let named = Type::Named {
            name: "Vec".to_string(),
            args: vec![Type::Named {
                name: "i32".to_string(),
                args: Vec::new(),
                span: Span::new(FileId(0), 0, 1),
            }],
            span: Span::new(FileId(0), 0, 1),
        };
        assert_eq!(type_text(&named), "Vec<i32>");
        let slice = Type::Slice {
            element: Box::new(named.clone()),
            span: Span::new(FileId(0), 0, 1),
        };
        assert_eq!(type_text(&slice), "[]Vec<i32>");
    }
}
