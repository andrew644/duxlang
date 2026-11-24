use dux_source::{SourceMap, Span};

pub mod code {
    pub const UNEXPECTED_CHAR: &str = "E0001";
    pub const UNTERMINATED_STRING: &str = "E0002";
    pub const BAD_ESCAPE: &str = "E0003";
    pub const NUMBER_OUT_OF_RANGE: &str = "E0004";
    pub const UNEXPECTED_TOKEN: &str = "E0005";
    pub const UNEXPECTED_EOF: &str = "E0006";
    pub const UNSUPPORTED_IN_BUILD: &str = "E0007";
    pub const EXPECTED_SEMICOLON: &str = "E0008";
    pub const NO_MAIN: &str = "E0100";
    pub const BAD_MAIN_SIGNATURE: &str = "E0101";
    pub const UNDECLARED: &str = "E0110";
    pub const DUPLICATE_DEFINITION: &str = "E0111";
    pub const WRONG_ARITY: &str = "E0112";
    pub const LINK_FAILED: &str = "E0900";
    pub const SOURCE_IO: &str = "E0901";
    pub const CODEGEN_FAILED: &str = "E0902";
}


#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Severity {
    Error,
    Warning,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Diagnostic {
    pub severity: Severity,
    pub code: &'static str,
    pub message: String,
    pub span: Option<Span>,
    pub note: Option<String>,
}

impl Diagnostic {
    pub fn error(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Error,
            code,
            message: message.into(),
            span: None,
            note: None,
        }
    }

    pub fn warning(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warning,
            code,
            message: message.into(),
            span: None,
            note: None,
        }
    }

    pub fn with_span(mut self, span: Span) -> Self {
        self.span = Some(span);
        self
    }

    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }
}

pub fn render(sources: &SourceMap, diagnostic: &Diagnostic) -> String {
    let mut out = match diagnostic.span {
        Some(span) => {
            let (line, col) = sources.line_col(span.file, span.start);
            format!(
                "{}:{}:{}: {}[{}]: {}",
                sources.name_of(span.file),
                line,
                col,
                diagnostic.severity.as_str(),
                diagnostic.code,
                diagnostic.message
            )
        }
        None => format!(
            "{}[{}]: {}",
            diagnostic.severity.as_str(),
            diagnostic.code,
            diagnostic.message
        ),
    };

    if let Some(span) = diagnostic.span {
        let text = sources.line_at(span.file, span.start);
        let (_, col) = sources.line_col(span.file, span.start);
        let caret_start = col.saturating_sub(1);
        let caret_len = (span.len.max(1) as usize).max(1);
        out.push_str("\n  | ");
        out.push_str(text);
        out.push_str("\n  | ");
        for _ in 0..caret_start {
            out.push(' ');
        }
        for _ in 0..caret_len {
            out.push('^');
        }
    }

    if let Some(note) = &diagnostic.note {
        out.push_str("\n  = ");
        out.push_str(note);
    }

    out
}

pub fn has_errors(diagnostics: &[Diagnostic]) -> bool {
    diagnostics
        .iter()
        .any(|d| d.severity == Severity::Error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dux_source::FileId;

    #[test]
    fn renders_location_snippet_and_caret() {
        let mut sources = SourceMap::new();
        let file = sources.add("app.dux", "fn main() { nope }");
        let diagnostic = Diagnostic::error("E0001", "unexpected token")
            .with_span(Span::new(file, 12, 4));
        let rendered = render(&sources, &diagnostic);
        assert!(rendered.starts_with("app.dux:1:13: error[E0001]: unexpected token"), "{rendered}");
        assert!(rendered.contains("fn main() { nope }"), "{rendered}");
        assert!(rendered.contains("^^^^"), "{rendered}");
    }

    #[test]
    fn renders_spanless_diagnostic() {
        let sources = SourceMap::new();
        let diagnostic = Diagnostic::error("E0100", "no main function").with_note("expected `fn main()`");
        let rendered = render(&sources, &diagnostic);
        assert_eq!(rendered, "error[E0100]: no main function\n  = expected `fn main()`");
    }

    #[test]
    fn warnings_are_not_errors() {
        assert!(!has_errors(&[Diagnostic::warning("W0001", "unused")]));
        assert!(has_errors(&[Diagnostic::error("E0001", "bad")]));
        let _ = FileId(0);
    }
}
