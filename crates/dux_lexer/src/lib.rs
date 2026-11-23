use dux_diag::Diagnostic;
use dux_source::{FileId, SourceMap, Span};

pub use dux_diag::code::{
    BAD_ESCAPE as CODE_BAD_ESCAPE, NUMBER_OUT_OF_RANGE as CODE_NUMBER_OUT_OF_RANGE,
    UNEXPECTED_CHAR as CODE_UNEXPECTED_CHAR, UNTERMINATED_STRING as CODE_UNTERMINATED_STRING,
};

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Keyword {
    Package,
    Import,
    Fn,
    Struct,
    Enum,
    Type,
    Const,
    Global,
    Impl,
    Union,
    Mod,
    Extern,
    If,
    Else,
    While,
    For,
    In,
    Loop,
    Break,
    Continue,
    Defer,
    Match,
    Return,
    As,
    When,
    True,
    False,
}

impl Keyword {
    pub fn from_ident(text: &str) -> Option<Keyword> {
        Some(match text {
            "package" => Keyword::Package,
            "import" => Keyword::Import,
            "fn" => Keyword::Fn,
            "struct" => Keyword::Struct,
            "enum" => Keyword::Enum,
            "type" => Keyword::Type,
            "const" => Keyword::Const,
            "global" => Keyword::Global,
            "impl" => Keyword::Impl,
            "union" => Keyword::Union,
            "mod" => Keyword::Mod,
            "extern" => Keyword::Extern,
            "if" => Keyword::If,
            "else" => Keyword::Else,
            "while" => Keyword::While,
            "for" => Keyword::For,
            "in" => Keyword::In,
            "loop" => Keyword::Loop,
            "break" => Keyword::Break,
            "continue" => Keyword::Continue,
            "defer" => Keyword::Defer,
            "match" => Keyword::Match,
            "return" => Keyword::Return,
            "as" => Keyword::As,
            "when" => Keyword::When,
            "true" => Keyword::True,
            "false" => Keyword::False,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Keyword::Package => "package",
            Keyword::Import => "import",
            Keyword::Fn => "fn",
            Keyword::Struct => "struct",
            Keyword::Enum => "enum",
            Keyword::Type => "type",
            Keyword::Const => "const",
            Keyword::Global => "global",
            Keyword::Impl => "impl",
            Keyword::Union => "union",
            Keyword::Mod => "mod",
            Keyword::Extern => "extern",
            Keyword::If => "if",
            Keyword::Else => "else",
            Keyword::While => "while",
            Keyword::For => "for",
            Keyword::In => "in",
            Keyword::Loop => "loop",
            Keyword::Break => "break",
            Keyword::Continue => "continue",
            Keyword::Defer => "defer",
            Keyword::Match => "match",
            Keyword::Return => "return",
            Keyword::As => "as",
            Keyword::When => "when",
            Keyword::True => "true",
            Keyword::False => "false",
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Punct {
    ColonEq,
    FatArrow,
    Arrow,
    EqEq,
    NotEq,
    Le,
    Ge,
    DotDotEq,
    PlusEq,
    MinusEq,
    StarEq,
    SlashEq,
    PercentEq,
    AndAnd,
    OrOr,
    Colon,
    Semicolon,
    Comma,
    DotDot,
    Dot,
    Pipe,
    Amp,
    Star,
    Plus,
    Minus,
    Slash,
    Percent,
    Bang,
    Lt,
    Gt,
    Assign,
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Hash,
}

impl Punct {
    pub fn as_str(self) -> &'static str {
        match self {
            Punct::ColonEq => ":=",
            Punct::FatArrow => "=>",
            Punct::Arrow => "->",
            Punct::EqEq => "==",
            Punct::NotEq => "!=",
            Punct::Le => "<=",
            Punct::Ge => ">=",
            Punct::DotDotEq => "..=",
            Punct::PlusEq => "+=",
            Punct::MinusEq => "-=",
            Punct::StarEq => "*=",
            Punct::SlashEq => "/=",
            Punct::PercentEq => "%=",
            Punct::AndAnd => "&&",
            Punct::OrOr => "||",
            Punct::Colon => ":",
            Punct::Semicolon => ";",
            Punct::Comma => ",",
            Punct::DotDot => "..",
            Punct::Dot => ".",
            Punct::Pipe => "|",
            Punct::Amp => "&",
            Punct::Star => "*",
            Punct::Plus => "+",
            Punct::Minus => "-",
            Punct::Slash => "/",
            Punct::Percent => "%",
            Punct::Bang => "!",
            Punct::Lt => "<",
            Punct::Gt => ">",
            Punct::Assign => "=",
            Punct::LParen => "(",
            Punct::RParen => ")",
            Punct::LBrace => "{",
            Punct::RBrace => "}",
            Punct::LBracket => "[",
            Punct::RBracket => "]",
            Punct::Hash => "#",
        }
    }

    fn assign_compound(self) -> Option<Punct> {
        Some(match self {
            Punct::PlusEq => Punct::Plus,
            Punct::MinusEq => Punct::Minus,
            Punct::StarEq => Punct::Star,
            Punct::SlashEq => Punct::Slash,
            Punct::PercentEq => Punct::Percent,
            _ => return None,
        })
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum TokenKind {
    Ident,
    Keyword(Keyword),
    Int,
    Float,
    Str,
    RawStr,
    CString,
    InterpStr,
    Char,
    Punct(Punct),
    Eof,
}

impl TokenKind {
    pub fn describe(self) -> String {
        match self {
            TokenKind::Ident => "identifier".to_string(),
            TokenKind::Keyword(k) => format!("`{}`", k.as_str()),
            TokenKind::Int => "integer literal".to_string(),
            TokenKind::Float => "float literal".to_string(),
            TokenKind::Str => "string literal".to_string(),
            TokenKind::RawStr => "raw string literal".to_string(),
            TokenKind::CString => "c-string literal".to_string(),
            TokenKind::InterpStr => "interpolated string literal".to_string(),
            TokenKind::Char => "character literal".to_string(),
            TokenKind::Punct(p) => format!("`{}`", p.as_str()),
            TokenKind::Eof => "end of input".to_string(),
        }
    }

    pub fn is_string(self) -> bool {
        matches!(
            self,
            TokenKind::Str | TokenKind::RawStr | TokenKind::CString | TokenKind::InterpStr
        )
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Literal {
    Int(u128),
    Float(f64),
    Text(String),
    Char(char),
}

#[derive(Clone, Debug)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
    pub literal: Option<Literal>,
    pub newline_before: bool,
}

impl Token {
    pub fn text<'a>(&self, sources: &'a SourceMap) -> &'a str {
        &sources.text_of(self.span.file)[self.span.start as usize..self.span.end() as usize]
    }

    pub fn text_of(&self) -> &str {
        match &self.literal {
            Some(Literal::Text(text)) => text,
            _ => "",
        }
    }

    pub fn is_keyword(&self, keyword: Keyword) -> bool {
        self.kind == TokenKind::Keyword(keyword)
    }

    pub fn is_punct(&self, punct: Punct) -> bool {
        self.kind == TokenKind::Punct(punct)
    }

    pub fn is_any_punct(&self) -> bool {
        matches!(self.kind, TokenKind::Punct(_))
    }
}

#[derive(Debug)]
pub struct Lexed {
    pub tokens: Vec<Token>,
    pub diagnostics: Vec<Diagnostic>,
}

impl Lexed {
    pub fn kinds(&self) -> Vec<TokenKind> {
        self.tokens.iter().map(|t| t.kind).collect()
    }

    pub fn eof_span(&self) -> Span {
        self.tokens.last().map(|t| t.span).unwrap_or_default()
    }
}

struct Lexer<'a> {
    file: FileId,
    text: &'a str,
    bytes: &'a [u8],
    cursor: usize,
    newline_before: bool,
    tokens: Vec<Token>,
    diagnostics: Vec<Diagnostic>,
}

pub fn lex(sources: &SourceMap, file: FileId) -> Lexed {
    let text = sources.text_of(file);
    let mut lexer = Lexer {
        file,
        text,
        bytes: text.as_bytes(),
        cursor: 0,
        newline_before: false,
        tokens: Vec::new(),
        diagnostics: Vec::new(),
    };
    lexer.run();
    Lexed {
        tokens: lexer.tokens,
        diagnostics: lexer.diagnostics,
    }
}

impl<'a> Lexer<'a> {
    fn run(&mut self) {
        loop {
            self.skip_trivia();
            let start = self.cursor;
            if self.at_end() {
                self.push(TokenKind::Eof, start, None);
                return;
            }
            self.token(start);
        }
    }

    fn skip_trivia(&mut self) {
        loop {
            match self.peek() {
                Some(b'\r') | Some(b' ') | Some(b'\t') => self.cursor += 1,
                Some(b'\n') => {
                    self.cursor += 1;
                    self.newline_before = true;
                }
                Some(b'/') if self.peek_at(1) == Some(b'/') => {
                    while self.cursor < self.bytes.len() && self.peek() != Some(b'\n') {
                        self.cursor += 1;
                    }
                }
                _ => return,
            }
        }
    }

    fn token(&mut self, start: usize) {
        match self.peek().unwrap() {
            b'a'..=b'z' | b'A'..=b'Z' | b'_' => self.ident_or_prefixed(start),
            b'0'..=b'9' => self.number(start),
            b'"' => {
                self.cursor += 1;
                self.string_body(start, TokenKind::Str);
            }
            b'\'' => self.char_literal(start),
            b'$' if self.peek_at(1) == Some(b'"') => {
                self.cursor += 2;
                self.interp_body(start);
            }
            _ => self.punct(start),
        }
    }

    fn ident_or_prefixed(&mut self, start: usize) {
        while matches!(
            self.peek(),
            Some(b'a'..=b'z') | Some(b'A'..=b'Z') | Some(b'0'..=b'9') | Some(b'_')
        ) {
            self.cursor += 1;
        }
        let word = &self.text[start..self.cursor];
        if word.len() == 1 && self.peek() == Some(b'"') {
            let kind = match word {
                "r" => TokenKind::RawStr,
                "c" => TokenKind::CString,
                _ => {
                    self.push_ident(start, word);
                    return;
                }
            };
            self.cursor += 1;
            self.string_body(start, kind);
            return;
        }
        self.push_ident(start, word);
    }

    fn push_ident(&mut self, start: usize, word: &str) {
        let kind = Keyword::from_ident(word)
            .map(TokenKind::Keyword)
            .unwrap_or(TokenKind::Ident);
        self.push(kind, start, None);
    }

    fn string_body(&mut self, start: usize, kind: TokenKind) {
        let raw = kind == TokenKind::RawStr;
        let mut value = String::new();
        loop {
            match self.peek() {
                None => {
                    self.diagnostic(CODE_UNTERMINATED_STRING, "unterminated string literal", start);
                    break;
                }
                Some(b'"') => {
                    self.cursor += 1;
                    break;
                }
                Some(b'\n') if !raw => {
                    self.diagnostic(
                        CODE_UNTERMINATED_STRING,
                        "string literal is not closed before end of line",
                        start,
                    );
                    break;
                }
                Some(b'\\') if !raw => {
                    self.cursor += 1;
                    let escaped = self.peek();
                    self.cursor += 1;
                    match escaped {
                        Some(b'n') => value.push('\n'),
                        Some(b't') => value.push('\t'),
                        Some(b'r') => value.push('\r'),
                        Some(b'0') => value.push('\0'),
                        Some(b'\\') => value.push('\\'),
                        Some(b'"') => value.push('"'),
                        Some(b'\'') => value.push('\''),
                        Some(b'$') => value.push('$'),
                        Some(other) => {
                            value.push(other as char);
                            self.diagnostic(
                                CODE_BAD_ESCAPE,
                                format!("unknown escape `\\{}`", other as char),
                                start,
                            );
                        }
                        None => {
                            self.diagnostic(CODE_UNTERMINATED_STRING, "unterminated string literal", start);
                            break;
                        }
                    }
                }
                Some(_) => value.push_str(self.next_utf8()),
            }
        }
        self.push(kind, start, Some(Literal::Text(value)));
    }

    fn interp_body(&mut self, start: usize) {
        let mut value = String::new();
        loop {
            match self.peek() {
                None => {
                    self.diagnostic(CODE_UNTERMINATED_STRING, "unterminated interpolated string", start);
                    break;
                }
                Some(b'"') => {
                    self.cursor += 1;
                    break;
                }
                Some(b'\n') => {
                    self.diagnostic(
                        CODE_UNTERMINATED_STRING,
                        "interpolated string is not closed before end of line",
                        start,
                    );
                    break;
                }
                Some(b'\\') => {
                    self.cursor += 1;
                    match self.peek() {
                        Some(b'n') => value.push('\n'),
                        Some(b't') => value.push('\t'),
                        Some(other) => value.push(other as char),
                        None => break,
                    }
                    self.cursor += 1;
                }
                Some(b'{') => {
                    self.cursor += 1;
                    value.push('{');
                    if self.peek() == Some(b'{') {
                        self.cursor += 1;
                    }
                }
                Some(b'}') => {
                    self.cursor += 1;
                    value.push('}');
                    if self.peek() == Some(b'}') {
                        self.cursor += 1;
                    }
                }
                Some(_) => value.push_str(self.next_utf8()),
            }
        }
        self.push(TokenKind::InterpStr, start, Some(Literal::Text(value)));
    }

    fn char_literal(&mut self, start: usize) {
        self.cursor += 1;
        let mut value = '\0';
        match self.peek() {
            Some(b'\\') => {
                self.cursor += 1;
                value = match self.peek() {
                    Some(b'n') => '\n',
                    Some(b't') => '\t',
                    Some(b'r') => '\r',
                    Some(b'0') => '\0',
                    Some(b'\\') => '\\',
                    Some(b'\'') => '\'',
                    Some(other) => {
                        self.diagnostic(
                            CODE_BAD_ESCAPE,
                            format!("unknown escape `\\{}`", other as char),
                            start,
                        );
                        other as char
                    }
                    None => '\0',
                };
                self.cursor += 1;
            }
            Some(_) => {
                value = self.next_utf8().chars().next().unwrap_or('\0');
            }
            None => {}
        }
        if self.peek() == Some(b'\'') {
            self.cursor += 1;
        } else {
            self.diagnostic(CODE_UNTERMINATED_STRING, "unterminated character literal", start);
        }
        self.push(TokenKind::Char, start, Some(Literal::Char(value)));
    }

    fn number(&mut self, start: usize) {
        let hex = self.peek() == Some(b'0') && matches!(self.peek_at(1), Some(b'x') | Some(b'X'));
        if hex {
            self.cursor += 2;
            let digits_start = self.cursor;
            self.eat_digits(|c| c.is_ascii_hexdigit());
            let digits = Self::undigits(&self.text[digits_start..self.cursor]);
            return match u128::from_str_radix(&digits, 16) {
                Ok(value) => self.push(TokenKind::Int, start, Some(Literal::Int(value))),
                Err(_) => self.overflow(start, 16),
            };
        }

        self.eat_digits(|c| c.is_ascii_digit());
        let mut is_float = false;
        if self.peek() == Some(b'.') && matches!(self.peek_at(1), Some(b) if b.is_ascii_digit()) {
            is_float = true;
            self.cursor += 1;
            self.eat_digits(|c| c.is_ascii_digit());
        }
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            is_float = true;
            self.cursor += 1;
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.cursor += 1;
            }
            self.eat_digits(|c| c.is_ascii_digit());
        }

        let digits = Self::undigits(&self.text[start..self.cursor]);
        if is_float {
            match digits.parse::<f64>() {
                Ok(value) => self.push(TokenKind::Float, start, Some(Literal::Float(value))),
                Err(_) => self.diagnostic(
                    CODE_NUMBER_OUT_OF_RANGE,
                    format!("invalid float literal `{digits}`"),
                    start,
                ),
            }
        } else {
            match digits.parse::<u128>() {
                Ok(value) => self.push(TokenKind::Int, start, Some(Literal::Int(value))),
                Err(_) => self.overflow(start, 10),
            }
        }
    }

    fn overflow(&mut self, start: usize, radix: u32) {
        let text = &self.text[start..self.cursor];
        self.diagnostic(
            CODE_NUMBER_OUT_OF_RANGE,
            format!("integer literal `{text}` does not fit in 128 bits (base {radix})"),
            start,
        );
        self.push(TokenKind::Int, start, Some(Literal::Int(u128::MAX)));
    }

    fn eat_digits(&mut self, accept: impl Fn(u8) -> bool) {
        while matches!(self.peek(), Some(byte) if accept(byte) || byte == b'_') {
            self.cursor += 1;
        }
    }

    fn undigits(text: &str) -> String {
        text.chars().filter(|c| *c != '_').collect()
    }

    fn punct(&mut self, start: usize) {
        let (punct, width) = match self.longest_punct() {
            Some(found) => found,
            None => {
                let byte = self.peek().unwrap();
                self.diagnostic(
                    CODE_UNEXPECTED_CHAR,
                    format!("unexpected character `{}`", byte as char),
                    start,
                );
                self.cursor += 1;
                return;
            }
        };
        self.cursor += width;
        self.push(TokenKind::Punct(punct), start, None);
    }

    fn longest_punct(&self) -> Option<(Punct, usize)> {
        const LONG: [(&str, Punct); 14] = [
            ("..=", Punct::DotDotEq),
            (":=", Punct::ColonEq),
            ("=>", Punct::FatArrow),
            ("->", Punct::Arrow),
            ("==", Punct::EqEq),
            ("!=", Punct::NotEq),
            ("<=", Punct::Le),
            (">=", Punct::Ge),
            ("+=", Punct::PlusEq),
            ("-=", Punct::MinusEq),
            ("*=", Punct::StarEq),
            ("/=", Punct::SlashEq),
            ("&&", Punct::AndAnd),
            ("||", Punct::OrOr),
        ];
        const SHORT: [(char, Punct); 17] = [
            (';', Punct::Semicolon),
            (',', Punct::Comma),
            (':', Punct::Colon),
            ('|', Punct::Pipe),
            ('&', Punct::Amp),
            ('*', Punct::Star),
            ('+', Punct::Plus),
            ('-', Punct::Minus),
            ('/', Punct::Slash),
            ('%', Punct::Percent),
            ('!', Punct::Bang),
            ('<', Punct::Lt),
            ('>', Punct::Gt),
            ('=', Punct::Assign),
            ('(', Punct::LParen),
            (')', Punct::RParen),
            ('{', Punct::LBrace),
        ];
        let three = self.slice(3);
        let two = self.slice(2);
        if three == "..=" {
            return Some((Punct::DotDotEq, 3));
        }
        if two == ".." {
            return Some((Punct::DotDot, 2));
        }
        if two == "%=" {
            return Some((Punct::PercentEq, 2));
        }
        for (text, punct) in LONG {
            if two == text {
                return Some((punct, 2));
            }
        }
        let head = self.peek()? as char;
        for (ch, punct) in SHORT {
            if head == ch {
                return Some((punct, 1));
            }
        }
        match head {
            '}' => Some((Punct::RBrace, 1)),
            '[' => Some((Punct::LBracket, 1)),
            ']' => Some((Punct::RBracket, 1)),
            '.' => Some((Punct::Dot, 1)),
            '#' => Some((Punct::Hash, 1)),
            _ => None,
        }
    }

    fn diagnostic(&mut self, code: &'static str, message: impl Into<String>, start: usize) {
        self.diagnostics.push(
            Diagnostic::error(code, message)
                .with_span(Span::new(self.file, start as u32, (self.cursor - start) as u32)),
        );
    }

    fn next_utf8(&mut self) -> &'a str {
        let head = self.bytes[self.cursor];
        let width = if head < 0x80 {
            1
        } else if head >= 0xf0 {
            4
        } else if head >= 0xe0 {
            3
        } else {
            2
        };
        let end = (self.cursor + width).min(self.bytes.len());
        let slice = &self.text[self.cursor..end];
        self.cursor = end;
        slice
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.cursor).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<u8> {
        self.bytes.get(self.cursor + offset).copied()
    }

    fn at_end(&self) -> bool {
        self.cursor >= self.bytes.len()
    }

    fn slice(&self, width: usize) -> &'a str {
        let end = (self.cursor + width).min(self.bytes.len());
        &self.text[self.cursor..end]
    }

    fn push(&mut self, kind: TokenKind, start: usize, literal: Option<Literal>) {
        self.tokens.push(Token {
            kind,
            span: Span::new(self.file, start as u32, (self.cursor - start) as u32),
            literal,
            newline_before: std::mem::take(&mut self.newline_before),
        });
    }
}

impl Punct {
    pub fn compound_operator(self) -> Option<Punct> {
        self.assign_compound()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags(text: &str) -> Vec<(bool, String)> {
        let mut sources = SourceMap::new();
        let file = sources.add("t.dux", text);
        lex(&sources, file)
            .tokens
            .iter()
            .map(|t| (t.newline_before, t.kind.describe()))
            .collect()
    }

    fn flagged(pairs: &[(bool, &str)]) -> Vec<(bool, String)> {
        pairs
            .iter()
            .map(|(newline, kind)| (*newline, kind.to_string()))
            .collect()
    }

    fn kinds(text: &str) -> Vec<TokenKind> {
        let mut sources = SourceMap::new();
        let file = sources.add("t.dux", text);
        lex(&sources, file).kinds()
    }

    fn diagnostics(text: &str) -> Vec<(String, String)> {
        let mut sources = SourceMap::new();
        let file = sources.add("t.dux", text);
        lex(&sources, file)
            .diagnostics
            .into_iter()
            .map(|d| (d.code.to_string(), d.message))
            .collect()
    }

    #[test]
    fn lexes_hello_world() {
        assert_eq!(
            kinds("package main\nfn main() { println(\"hi\") }"),
            vec![
                TokenKind::Keyword(Keyword::Package),
                TokenKind::Ident,
                TokenKind::Keyword(Keyword::Fn),
                TokenKind::Ident,
                TokenKind::Punct(Punct::LParen),
                TokenKind::Punct(Punct::RParen),
                TokenKind::Punct(Punct::LBrace),
                TokenKind::Ident,
                TokenKind::Punct(Punct::LParen),
                TokenKind::Str,
                TokenKind::Punct(Punct::RParen),
                TokenKind::Punct(Punct::RBrace),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn keywords_beat_identifiers() {
        assert_eq!(
            kinds("fn if else while for in loop break continue defer match return as when"),
            vec![
                TokenKind::Keyword(Keyword::Fn),
                TokenKind::Keyword(Keyword::If),
                TokenKind::Keyword(Keyword::Else),
                TokenKind::Keyword(Keyword::While),
                TokenKind::Keyword(Keyword::For),
                TokenKind::Keyword(Keyword::In),
                TokenKind::Keyword(Keyword::Loop),
                TokenKind::Keyword(Keyword::Break),
                TokenKind::Keyword(Keyword::Continue),
                TokenKind::Keyword(Keyword::Defer),
                TokenKind::Keyword(Keyword::Match),
                TokenKind::Keyword(Keyword::Return),
                TokenKind::Keyword(Keyword::As),
                TokenKind::Keyword(Keyword::When),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn declaration_punctuators_are_longest_match() {
        assert_eq!(
            kinds("a := b: T = c -> d => e ..= f .. g"),
            vec![
                TokenKind::Ident,
                TokenKind::Punct(Punct::ColonEq),
                TokenKind::Ident,
                TokenKind::Punct(Punct::Colon),
                TokenKind::Ident,
                TokenKind::Punct(Punct::Assign),
                TokenKind::Ident,
                TokenKind::Punct(Punct::Arrow),
                TokenKind::Ident,
                TokenKind::Punct(Punct::FatArrow),
                TokenKind::Ident,
                TokenKind::Punct(Punct::DotDotEq),
                TokenKind::Ident,
                TokenKind::Punct(Punct::DotDot),
                TokenKind::Ident,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn string_prefixes_are_distinct_token_kinds() {
        assert_eq!(
            kinds("r\"raw\" c\"cstr\" $\"interp {x} {}\" \"plain\""),
            vec![
                TokenKind::RawStr,
                TokenKind::CString,
                TokenKind::InterpStr,
                TokenKind::Str,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn escapes_are_decoded_and_interpolation_is_kept_verbatim() {
        let mut sources = SourceMap::new();
        let file = sources.add("t.dux", "\"a\\tb\\n\\\"c\\\"\"\n$\"x{}y{z}\"");
        let lexed = lex(&sources, file);
        assert_eq!(
            lexed.tokens[0].literal,
            Some(Literal::Text("a\tb\n\"c\"".to_string()))
        );
        assert_eq!(
            lexed.tokens[1].literal,
            Some(Literal::Text("x{}y{z}".to_string()))
        );
    }

    #[test]
    fn raw_strings_keep_backslashes() {
        let mut sources = SourceMap::new();
        let file = sources.add("t.dux", "r\"C:\\dux\\std\"");
        assert_eq!(
            lex(&sources, file).tokens[0].literal,
            Some(Literal::Text("C:\\dux\\std".to_string()))
        );
    }

    #[test]
    fn numbers_cover_hex_underscores_and_floats() {
        let mut sources = SourceMap::new();
        let file = sources.add("t.dux", "0xff_00 1_024 0.75 1e3");
        let lexed = lex(&sources, file);
        assert_eq!(lexed.tokens[0].literal, Some(Literal::Int(0xff00)));
        assert_eq!(lexed.tokens[1].literal, Some(Literal::Int(1024)));
        assert_eq!(lexed.tokens[2].literal, Some(Literal::Float(0.75)));
        assert_eq!(lexed.tokens[3].literal, Some(Literal::Float(1000.0)));
    }

    #[test]
    fn unterminated_string_recovers_at_next_line() {
        let mut sources = SourceMap::new();
        let file = sources.add("t.dux", "x := \"oops\ny := 1");
        let lexed = lex(&sources, file);
        assert!(matches!(
            lexed.diagnostics.first().map(|d| d.code),
            Some(CODE_UNTERMINATED_STRING)
        ));
        assert_eq!(
            lexed.kinds().last(),
            Some(&TokenKind::Eof),
            "lexer must always terminate with Eof"
        );
        assert!(lexed
            .tokens
            .iter()
            .any(|t| t.kind == TokenKind::Ident && t.text(&sources) == "y"));
    }

    #[test]
    fn reports_bad_escape_and_unexpected_character() {
        let codes: Vec<String> = diagnostics("\"\\q\"").into_iter().map(|(c, _)| c).collect();
        assert_eq!(codes, vec![CODE_BAD_ESCAPE.to_string()]);
        let (code, message) = diagnostics("@").remove(0);
        assert_eq!(code, CODE_UNEXPECTED_CHAR);
        assert!(message.contains('@'), "{message}");
    }

    #[test]
    fn character_literals_are_runes() {
        let mut sources = SourceMap::new();
        let file = sources.add("t.dux", "'0' 'é'");
        let lexed = lex(&sources, file);
        assert_eq!(lexed.tokens[0].literal, Some(Literal::Char('0')));
        assert_eq!(lexed.tokens[1].literal, Some(Literal::Char('é')));
    }

    #[test]
    fn a_newline_is_recorded_on_the_token_that_follows_it() {
        assert_eq!(
            flags("a\nb b\n\nc"),
            flagged(&[
                (false, "identifier"),
                (true, "identifier"),
                (false, "identifier"),
                (true, "identifier"),
                (false, "end of input"),
            ])
        );
    }

    #[test]
    fn a_newline_inside_a_raw_string_does_not_start_a_new_line() {
        assert_eq!(
            flags("r\"a\nb\"\nc"),
            flagged(&[
                (false, "raw string literal"),
                (true, "identifier"),
                (false, "end of input"),
            ])
        );
    }

    #[test]
    fn comments_are_trivia() {
        assert_eq!(
            kinds("// leading\nx // trailing\n"),
            vec![TokenKind::Ident, TokenKind::Eof]
        );
    }
}
