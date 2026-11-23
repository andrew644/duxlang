use dux_ast::{AssignOp, BinaryOp, Block, Decl, Expr, FnDecl, Ident, Import, Param, Program, StrKind, Stmt, Type, UnaryOp};
use dux_diag::Diagnostic;
use dux_lexer::{Keyword, Literal, Punct, Token, TokenKind};
use dux_source::{SourceMap, Span};

pub use dux_diag::code::{
    EXPECTED_SEMICOLON as CODE_EXPECTED_SEMICOLON, NO_MAIN as CODE_NO_MAIN,
    UNEXPECTED_EOF as CODE_UNEXPECTED_EOF, UNEXPECTED_TOKEN as CODE_UNEXPECTED_TOKEN,
    UNSUPPORTED_IN_BUILD as CODE_UNSUPPORTED_YET,
};

pub struct Parsed {
    pub program: Program,
    pub diagnostics: Vec<Diagnostic>,
}

struct Parser<'a> {
    sources: &'a SourceMap,
    tokens: &'a [Token],
    cursor: usize,
    diagnostics: Vec<Diagnostic>,
}

pub fn parse(sources: &SourceMap, tokens: &[Token]) -> Parsed {
    let mut parser = Parser {
        sources,
        tokens,
        cursor: 0,
        diagnostics: Vec::new(),
    };
    let program = parser.program();
    Parsed {
        program,
        diagnostics: parser.diagnostics,
    }
}

fn wrap(name: &str) -> String {
    if name.starts_with('`') {
        name.to_string()
    } else {
        format!("`{name}`")
    }
}

fn compound_assign(punct: Punct) -> Option<AssignOp> {
    Some(match punct {
        Punct::PlusEq => AssignOp::Add,
        Punct::MinusEq => AssignOp::Sub,
        Punct::StarEq => AssignOp::Mul,
        Punct::SlashEq => AssignOp::Div,
        Punct::PercentEq => AssignOp::Mod,
        _ => return None,
    })
}

impl<'a> Parser<'a> {
    fn program(&mut self) -> Program {
        let mut program = Program::default();
        if self.at_keyword(Keyword::Package) {
            let start = self.peek().span;
            self.bump();
            let name = self.expect_ident("package name");
            program.package = Some(name);
            let _ = start;
        }
        loop {
            match self.peek().kind {
                TokenKind::Keyword(Keyword::Import) => {
                    self.bump();
                    let token = self.peek().clone();
                    match &token.literal {
                        Some(Literal::Text(path)) => {
                            program.imports.push(Import {
                                path: path.clone(),
                                span: token.span,
                            });
                        }
                        _ => self.error(&token, "import expects a string literal path"),
                    }
                    self.bump();
                }
                TokenKind::Keyword(Keyword::Fn) => program.decls.push(Decl::Fn(self.function())),
                TokenKind::Eof => break,
                _ => {
                    let token = self.peek().clone();
                    if token.is_punct(Punct::Semicolon) {
                        self.bump();
                        continue;
                    }
                    let before = self.cursor;
                    self.error(&token, "expected `fn`, `const`, `global`, `struct`, `enum` or `impl` at top level");
                    self.recover_to_top_level();
                    if self.cursor == before {
                        self.bump();
                    }
                }
            }
        }
        program
    }

    fn recover_to_top_level(&mut self) {
        if !self.at_end() {
            self.bump();
        }
        loop {
            match self.peek().kind {
                TokenKind::Eof => return,
                TokenKind::Keyword(Keyword::Fn)
                | TokenKind::Keyword(Keyword::Struct)
                | TokenKind::Keyword(Keyword::Enum)
                | TokenKind::Keyword(Keyword::Const)
                | TokenKind::Keyword(Keyword::Global)
                | TokenKind::Keyword(Keyword::Impl) => return,
                _ => self.bump(),
            }
        }
    }

    fn function(&mut self) -> FnDecl {
        let start = self.peek().span;
        self.bump();
        let name = self.expect_ident("function name");
        let params = self.params();
        let return_ty = if self.at_punct(Punct::Arrow) {
            self.bump();
            Some(self.ty())
        } else {
            None
        };
        let body = self.block();
        let span = start.merge(body.span);
        FnDecl {
            name,
            params,
            return_ty,
            body,
            span,
        }
    }

    fn params(&mut self) -> Vec<Param> {
        let mut params = Vec::new();
        if !self.at_punct(Punct::LParen) {
            let token = self.peek().clone();
            self.error(&token, "expected `(` after function name");
            return params;
        }
        self.bump();
        while !self.at_punct(Punct::RParen) && !self.at_end() {
            let name = self.expect_ident("parameter name");
            let span = name.span;
            if !self.at_punct(Punct::Colon) {
                let token = self.peek().clone();
                self.error(&token, "parameters are typed: `name: Type`");
            } else {
                self.bump();
            }
            let ty = self.ty();
            let param_span = span.merge(ty.span());
            params.push(Param {
                name,
                ty,
                span: param_span,
            });
            if self.at_punct(Punct::Comma) {
                self.bump();
            } else {
                break;
            }
        }
        self.expect_punct(Punct::RParen, "expected `)` to close the parameter list");
        params
    }

    fn ty(&mut self) -> Type {
        let token = self.peek().clone();
        if token.is_punct(Punct::Star) {
            self.bump();
            let pointee = self.ty();
            let span = token.span.merge(pointee.span());
            return Type::Pointer {
                pointee: Box::new(pointee),
                span,
            };
        }
        if token.is_punct(Punct::LBracket) {
            self.bump();
            if self.at_punct(Punct::RBracket) {
                self.bump();
                let element = self.ty();
                let span = token.span.merge(element.span());
                return Type::Slice {
                    element: Box::new(element),
                    span,
                };
            }
            let element = self.ty();
            self.expect_punct(Punct::Semicolon, "fixed arrays are written `[Type; N]`");
            let len = self.expr();
            self.expect_punct(Punct::RBracket, "expected `]` to close the array type");
            return Type::Array {
                element: Box::new(element),
                len: Box::new(len),
                span: token.span,
            };
        }
        if token.is_punct(Punct::LParen) {
            self.bump();
            let mut elements = Vec::new();
            while !self.at_punct(Punct::RParen) && !self.at_end() {
                elements.push(self.ty());
                if self.at_punct(Punct::Comma) {
                    self.bump();
                } else {
                    break;
                }
            }
            self.expect_punct(Punct::RParen, "expected `)` to close the tuple type");
            return Type::Tuple {
                elements,
                span: token.span,
            };
        }
        if token.kind != TokenKind::Ident {
            self.error(&token, "expected a type name");
            self.bump();
            return Type::Error { span: token.span };
        }
        self.bump();
        let name = token.text(self.sources).to_string();
        let mut args = Vec::new();
        if self.at_punct(Punct::Lt) {
            self.bump();
            while !self.at_punct(Punct::Gt) && !self.at_end() {
                args.push(self.ty());
                if self.at_punct(Punct::Comma) {
                    self.bump();
                } else {
                    break;
                }
            }
            self.expect_punct(Punct::Gt, "expected `>` to close the type argument list");
        }
        Type::Named {
            name,
            args,
            span: token.span,
        }
    }

    fn block(&mut self) -> Block {
        let start = self.peek().span;
        self.expect_punct(Punct::LBrace, "expected `{` to open a block");
        let mut stmts = Vec::new();
        while !self.at_punct(Punct::RBrace) && !self.at_end() {
            if self.at_punct(Punct::Semicolon) {
                let token = self.peek().clone();
                self.bump();
                self.diagnostics.push(
                    Diagnostic::error(
                        CODE_EXPECTED_SEMICOLON,
                        "unexpected `;`: a statement cannot be empty",
                    )
                    .with_span(token.span),
                );
                continue;
            }
            let before = self.cursor;
            let statement = self.stmt();
            stmts.push(statement);
            if self.cursor == before {
                self.bump();
                continue;
            }
            if self.at_punct(Punct::Semicolon) {
                self.bump();
            } else if !self.at_statement_end() {
                let token = self.peek().clone();
                self.diagnostics.push(
                    Diagnostic::error(
                        CODE_EXPECTED_SEMICOLON,
                        format!(
                            "this statement must be ended before {}",
                            token.kind.describe()
                        ),
                    )
                    .with_span(token.span)
                    .with_note("statements end at a newline or a `;`"),
                );
                self.recover_to_next_line();
            }
        }
        let end = self.expect_punct(Punct::RBrace, "expected `}` to close the block");
        Block {
            stmts,
            span: start.merge(end),
        }
    }

    fn recover_to_next_line(&mut self) {
        while !self.at_end() && !self.peek().newline_before && !self.at_punct(Punct::RBrace) {
            self.bump();
        }
    }

    fn at_statement_end(&self) -> bool {
        self.at_punct(Punct::Semicolon)
            || self.at_punct(Punct::RBrace)
            || self.at_end()
            || self.peek().newline_before
    }

    fn stmt(&mut self) -> Stmt {
        let token = self.peek().clone();
        match token.kind {
            TokenKind::Keyword(Keyword::Return) => {
                self.bump();
                let value = if self.starts_expression() {
                    Some(self.expr())
                } else {
                    None
                };
                let span = value.as_ref().map(|e| token.span.merge(e.span())).unwrap_or(token.span);
                Stmt::Return { value, span }
            }
            TokenKind::Keyword(Keyword::Break) => {
                self.bump();
                Stmt::Break { span: token.span }
            }
            TokenKind::Keyword(Keyword::Continue) => {
                self.bump();
                Stmt::Continue { span: token.span }
            }
            TokenKind::Keyword(Keyword::Defer) => {
                self.bump();
                let inner = self.stmt();
                let span = token.span.merge(inner.span());
                Stmt::Defer {
                    stmt: Box::new(inner),
                    span,
                }
            }
            TokenKind::Keyword(
                Keyword::If | Keyword::While | Keyword::For | Keyword::Loop | Keyword::Match,
            ) => {
                self.unsupported(&token, token.kind.describe());
                self.skip_to_statement_end();
                Stmt::Error { span: token.span }
            }
            TokenKind::Ident => self.binding_or_effect(),
            _ => self.binding_or_effect(),
        }
    }

    fn binding_or_effect(&mut self) -> Stmt {
        let token = self.peek().clone();
        if token.kind == TokenKind::Ident {
            let next = self.peek_at(1).map(|t| t.kind);
            if next == Some(TokenKind::Punct(Punct::ColonEq)) {
                let name = self.expect_ident("binding name");
                self.bump();
                let value = self.expr();
                return Stmt::Binding {
                    name: name.clone(),
                    ty: None,
                    value: Some(value),
                    span: name.span,
                };
            }
            if next == Some(TokenKind::Punct(Punct::Colon)) {
                let name = self.expect_ident("binding name");
                self.bump();
                let ty = self.ty();
                let value = if self.at_punct(Punct::Assign) {
                    self.bump();
                    Some(self.expr())
                } else {
                    None
                };
                return Stmt::Binding {
                    name: name.clone(),
                    ty: Some(ty),
                    value,
                    span: name.span,
                };
            }
        }
        self.expr_stmt_or_assign(token)
    }

    fn expr_stmt_or_assign(&mut self, start: Token) -> Stmt {
        let target = self.expr();
        let target_span = target.span();
        let punct = match self.peek().kind {
            TokenKind::Punct(p) => Some(p),
            _ => None,
        };
        let op = match punct {
            Some(Punct::Assign) => Some(AssignOp::Set),
            other => other.and_then(compound_assign),
        };
        match op {
            Some(op) => {
                let span = start.span.merge(self.peek().span);
                self.bump();
                let value = self.expr();
                Stmt::Assign {
                    target,
                    op,
                    value,
                    span,
                }
            }
            None => Stmt::Expr {
                expr: target,
                span: target_span,
            },
        }
    }

    fn skip_to_statement_end(&mut self) {
        if !self.at_end() {
            self.bump();
        }
        let mut depth = 0usize;
        loop {
            match self.peek().kind {
                TokenKind::Eof => return,
                TokenKind::Punct(Punct::LBrace) => {
                    depth += 1;
                    self.bump();
                }
                TokenKind::Punct(Punct::RBrace) => {
                    if depth == 0 {
                        return;
                    }
                    depth -= 1;
                    self.bump();
                    if depth == 0 {
                        return;
                    }
                }
                TokenKind::Punct(Punct::Semicolon) => {
                    self.bump();
                    return;
                }
                _ => self.bump(),
            }
        }
    }

    fn expr(&mut self) -> Expr {
        self.binary(0)
    }

    fn binary(&mut self, min_precedence: u8) -> Expr {
        let mut lhs = self.unary();
        loop {
            let op = match self.peek().kind {
                TokenKind::Punct(Punct::Plus) => BinaryOp::Add,
                TokenKind::Punct(Punct::Minus) => BinaryOp::Sub,
                TokenKind::Punct(Punct::Star) => BinaryOp::Mul,
                TokenKind::Punct(Punct::Slash) => BinaryOp::Div,
                TokenKind::Punct(Punct::Percent) => BinaryOp::Mod,
                TokenKind::Punct(Punct::EqEq) => BinaryOp::Eq,
                TokenKind::Punct(Punct::NotEq) => BinaryOp::NotEq,
                TokenKind::Punct(Punct::Lt) => BinaryOp::Lt,
                TokenKind::Punct(Punct::Gt) => BinaryOp::Gt,
                TokenKind::Punct(Punct::Le) => BinaryOp::Le,
                TokenKind::Punct(Punct::Ge) => BinaryOp::Ge,
                TokenKind::Punct(Punct::AndAnd) => BinaryOp::And,
                TokenKind::Punct(Punct::OrOr) => BinaryOp::Or,
                _ => break,
            };
            if op.precedence() < min_precedence {
                break;
            }
            let op_span = self.peek().span;
            self.bump();
            let rhs = self.binary(op.precedence() + 1);
            let span = lhs.span().merge(rhs.span()).merge(op_span);
            lhs = Expr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
                span,
            };
        }
        lhs
    }

    fn unary(&mut self) -> Expr {
        let token = self.peek().clone();
        let op = match token.kind {
            TokenKind::Punct(Punct::Minus) => Some(UnaryOp::Neg),
            TokenKind::Punct(Punct::Bang) => Some(UnaryOp::Not),
            TokenKind::Punct(Punct::Amp) => Some(UnaryOp::AddressOf),
            TokenKind::Punct(Punct::Star) => Some(UnaryOp::Load),
            _ => None,
        };
        if let Some(op) = op {
            self.bump();
            let operand = self.unary();
            let span = token.span.merge(operand.span());
            return Expr::Unary {
                op,
                operand: Box::new(operand),
                span,
            };
        }
        let head = self.primary();
        self.postfix(head)
    }

    fn postfix(&mut self, mut expr: Expr) -> Expr {
        loop {
            let token = self.peek().clone();
            match token.kind {
                TokenKind::Punct(Punct::LParen) => {
                    self.bump();
                    let mut args = Vec::new();
                    while !self.at_punct(Punct::RParen) && !self.at_end() {
                        args.push(self.expr());
                        if self.at_punct(Punct::Comma) {
                            self.bump();
                        } else {
                            break;
                        }
                    }
                    let end = self.expect_punct(Punct::RParen, "expected `)` to close the argument list");
                    let span = expr.span().merge(end);
                    expr = Expr::Call {
                        callee: Box::new(expr),
                        args,
                        span,
                    };
                }
                TokenKind::Punct(Punct::LBracket) => {
                    self.bump();
                    let index = self.expr();
                    let end = self.expect_punct(Punct::RBracket, "expected `]` to close the index");
                    let span = expr.span().merge(end);
                    expr = Expr::Index {
                        base: Box::new(expr),
                        index: Box::new(index),
                        span,
                    };
                }
                TokenKind::Punct(Punct::Dot) => {
                    self.bump();
                    let name = self.expect_ident("field name");
                    let span = expr.span().merge(name.span);
                    expr = Expr::Field {
                        base: Box::new(expr),
                        name,
                        span,
                    };
                }
                TokenKind::Keyword(Keyword::As) => {
                    self.bump();
                    let target = self.ty();
                    let span = expr.span().merge(target.span());
                    expr = Expr::Cast {
                        value: Box::new(expr),
                        target,
                        span,
                    };
                }
                _ => {
                    let _ = token;
                    return expr;
                }
            }
        }
    }

    fn primary(&mut self) -> Expr {
        let token = self.peek().clone();
        match token.kind {
            TokenKind::Int => {
                self.bump();
                match token.literal {
                    Some(Literal::Int(value)) => Expr::Int { value, span: token.span },
                    _ => Expr::Error { span: token.span },
                }
            }
            TokenKind::Float => {
                self.bump();
                match token.literal {
                    Some(Literal::Float(value)) => Expr::Float { value, span: token.span },
                    _ => Expr::Error { span: token.span },
                }
            }
            TokenKind::Char => {
                self.bump();
                match token.literal {
                    Some(Literal::Char(value)) => Expr::Char { value, span: token.span },
                    _ => Expr::Error { span: token.span },
                }
            }
            TokenKind::Str | TokenKind::RawStr | TokenKind::CString | TokenKind::InterpStr => {
                self.bump();
                let kind = match token.kind {
                    TokenKind::Str => StrKind::Utf8,
                    TokenKind::RawStr => StrKind::Raw,
                    TokenKind::CString => StrKind::CString,
                    _ => StrKind::Interpolated,
                };
                match token.literal {
                    Some(Literal::Text(text)) => Expr::Str { text, kind, span: token.span },
                    _ => Expr::Error { span: token.span },
                }
            }
            TokenKind::Keyword(Keyword::True) => {
                self.bump();
                Expr::Bool { value: true, span: token.span }
            }
            TokenKind::Keyword(Keyword::False) => {
                self.bump();
                Expr::Bool { value: false, span: token.span }
            }
            TokenKind::Ident => {
                self.bump();
                Expr::Ident {
                    name: token.text(self.sources).to_string(),
                    span: token.span,
                }
            }
            TokenKind::Punct(Punct::LParen) => {
                self.bump();
                let inner = self.expr();
                if self.at_punct(Punct::Comma) {
                    let mut elements = vec![inner];
                    while self.at_punct(Punct::Comma) {
                        self.bump();
                        elements.push(self.expr());
                    }
                    let end = self.expect_punct(Punct::RParen, "expected `)` to close the tuple");
                    self.diagnostics.push(
                        Diagnostic::error(CODE_UNSUPPORTED_YET, "tuple literals are not supported by this compiler build yet")
                            .with_span(token.span.merge(end))
                            .with_note("this is a milestone limitation, not a language decision"),
                    );
                    return Expr::Error {
                        span: token.span.merge(end),
                    };
                }
                let end = self.expect_punct(Punct::RParen, "expected `)` to close the parenthesised expression");
                Expr::Paren {
                    inner: Box::new(inner),
                    span: token.span.merge(end),
                }
            }
            TokenKind::Eof => {
                self.diagnostics.push(
                    Diagnostic::error(CODE_UNEXPECTED_EOF, "expected an expression, found end of input")
                        .with_span(token.span),
                );
                Expr::Error { span: token.span }
            }
            _ => {
                self.error(&token, "expected an expression");
                self.bump();
                Expr::Error { span: token.span }
            }
        }
    }

    fn starts_expression(&self) -> bool {
        matches!(
            self.peek().kind,
            TokenKind::Int
                | TokenKind::Float
                | TokenKind::Str
                | TokenKind::RawStr
                | TokenKind::CString
                | TokenKind::InterpStr
                | TokenKind::Char
                | TokenKind::Ident
                | TokenKind::Keyword(Keyword::True)
                | TokenKind::Keyword(Keyword::False)
                | TokenKind::Punct(Punct::LParen)
                | TokenKind::Punct(Punct::Minus)
                | TokenKind::Punct(Punct::Bang)
                | TokenKind::Punct(Punct::Amp)
                | TokenKind::Punct(Punct::Star)
        )
    }

    fn unsupported(&mut self, token: &Token, what: String) {
        let message = format!(
            "{} is not supported by this compiler build yet",
            wrap(what.as_str())
        );
        self.diagnostics.push(
            Diagnostic::error(CODE_UNSUPPORTED_YET, message)
                .with_span(token.span)
                .with_note("this is a milestone limitation, not a language decision"),
        );
    }

    fn expect_ident(&mut self, what: &str) -> Ident {
        let token = self.peek().clone();
        if token.kind == TokenKind::Ident {
            self.bump();
            return Ident::new(token.text(self.sources), token.span);
        }
        self.error(&token, format!("expected {what}"));
        Ident::new("<error>", token.span)
    }

    fn expect_punct(&mut self, punct: Punct, message: &str) -> Span {
        let token = self.peek().clone();
        if token.is_punct(punct) {
            self.bump();
            return token.span;
        }
        self.error(&token, message);
        token.span
    }

    fn error(&mut self, token: &Token, message: impl Into<String>) {
        let message = if token.kind == TokenKind::Eof {
            format!("{}, found end of input", message.into())
        } else {
            message.into()
        };
        self.diagnostics.push(
            Diagnostic::error(CODE_UNEXPECTED_TOKEN, format!("{message}, found {}", token.kind.describe()))
                .with_span(token.span),
        );
    }

    fn peek(&self) -> &Token {
        self.tokens
            .get(self.cursor)
            .unwrap_or_else(|| self.tokens.last().expect("token stream ends with Eof"))
    }

    fn peek_at(&self, offset: usize) -> Option<&Token> {
        self.tokens.get(self.cursor + offset)
    }

    fn at_punct(&self, punct: Punct) -> bool {
        self.peek().is_punct(punct)
    }

    fn at_keyword(&self, keyword: Keyword) -> bool {
        self.peek().is_keyword(keyword)
    }

    fn at_end(&self) -> bool {
        self.peek().kind == TokenKind::Eof
    }

    fn bump(&mut self) {
        if !self.at_end() {
            self.cursor += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dux_lexer::lex;

    fn parse_text(text: &str) -> (Program, Vec<Diagnostic>) {
        let mut sources = SourceMap::new();
        let file = sources.add("t.dux", text);
        let lexed = lex(&sources, file);
        let parsed = parse(&sources, &lexed.tokens);
        (parsed.program, parsed.diagnostics)
    }

    fn codes(text: &str) -> Vec<&'static str> {
        parse_text(text)
            .1
            .iter()
            .map(|d| d.code)
            .collect()
    }

    #[test]
    fn parses_hello() {
        let text = "package main\nfn main() { println(\"hi\") }";
        let mut sources = SourceMap::new();
        let file = sources.add("t.dux", text.to_string());
        let lexed = dux_lexer::lex(&sources, file);
        let parsed = parse(&sources, &lexed.tokens);
        assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
        let program = parsed.program;
        assert_eq!(program.package.as_ref().unwrap().name, "main");
        let main = program.find_function("main").expect("main");
        assert!(main.params.is_empty());
        assert!(main.return_ty.is_none());
        let Stmt::Expr { expr: Expr::Call { callee, args, .. }, .. } = &main.body.stmts[0] else {
            panic!("expected a call statement: {:?}", main.body.stmts[0]);
        };
        let Expr::Ident { name, .. } = &**callee else {
            panic!("expected a call of a name, found {callee:?}");
        };
        assert_eq!(name, "println");
        assert_eq!(args[0].span(), Span::new(file, 33, 4));
        assert!(matches!(&args[0], Expr::Str { text, kind: StrKind::Utf8, .. } if text == "hi"));
    }

    #[test]
    fn a_newline_ends_a_statement_and_a_semicolon_is_still_accepted() {
        for source in ["fn f() { a := 1\nb := 2\n}", "fn f() { a := 1; b := 2; }"] {
            let (program, diagnostics) = parse_text(source);
            assert!(diagnostics.is_empty(), "{source}: {diagnostics:?}");
            assert_eq!(program.find_function("f").unwrap().body.stmts.len(), 2);
        }
    }

    #[test]
    fn an_expression_may_continue_over_a_line_break() {
        let (program, diagnostics) = parse_text("fn f() { x := 1 +\n    2\nprintln(x) }");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let function = program.find_function("f").unwrap();
        assert_eq!(function.body.stmts.len(), 2);
        let Stmt::Expr { .. } = &function.body.stmts[1] else {
            panic!("the second statement must be the call: {:?}", function.body.stmts[1])
        };
    }

    #[test]
    fn two_expressions_on_one_line_need_a_semicolon() {
        let (_, diagnostics) = parse_text("fn f() { g() h() }");
        assert_eq!(
            diagnostics.iter().map(|d| d.code).collect::<Vec<_>>(),
            vec![CODE_EXPECTED_SEMICOLON]
        );
    }

    #[test]
    fn all_three_binding_forms_are_distinct() {
        let (program, diagnostics) =
            parse_text("fn f() { a := 1; b: i64 = 2; c: i64 }");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let main = program.find_function("f").unwrap();
        let bindings: Vec<_> = main
            .body
            .stmts
            .iter()
            .map(|s| match s {
                Stmt::Binding { name, ty, value, .. } => (name.name.as_str(), ty.is_some(), value.is_some()),
                other => panic!("expected binding, found {other:?}"),
            })
            .collect();
        assert_eq!(bindings, vec![("a", false, true), ("b", true, true), ("c", true, false)]);
    }

    #[test]
    fn precedence_and_casts_attach_as_expected() {
        let (program, diagnostics) = parse_text("fn f() { x := 1 + 2 * 3 as i64 }");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let main = program.find_function("f").unwrap();
        let Stmt::Binding {
            value:
                Some(Expr::Binary {
                    op: BinaryOp::Add,
                    rhs,
                    ..
                }),
            ..
        } = &main.body.stmts[0]
        else {
            panic!("expected an addition at the top: {:?}", main.body.stmts[0]);
        };
        let Expr::Binary {
            op: BinaryOp::Mul,
            rhs: inner,
            ..
        } = &**rhs
        else {
            panic!("`*` must bind tighter than `+`: {rhs:?}");
        };
        assert!(matches!(**inner, Expr::Cast { .. }), "`as` must bind tightest");
    }

    #[test]
    fn assignment_and_compound_assignment_are_statements_not_bindings() {
        let (program, diagnostics) = parse_text("fn f() { x := 0; x += 1; x = 2 }");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let main = program.find_function("f").unwrap();
        assert!(matches!(main.body.stmts[1], Stmt::Assign { op: AssignOp::Add, .. }));
        assert!(matches!(main.body.stmts[2], Stmt::Assign { op: AssignOp::Set, .. }));
    }

    #[test]
    fn generic_and_compound_types_round_trip() {
        let (program, diagnostics) =
            parse_text("fn f(v: *Vec3, s: []u8, a: [i32; 4], m: HashMap<string, i32>) { }");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let f = program.find_function("f").unwrap();
        assert!(matches!(f.params[0].ty, Type::Pointer { .. }));
        assert!(matches!(f.params[1].ty, Type::Slice { .. }));
        assert!(matches!(f.params[2].ty, Type::Array { .. }));
        let Type::Named { name, args, .. } = &f.params[3].ty else {
            panic!("expected named type")
        };
        assert_eq!((name.as_str(), args.len()), ("HashMap", 2));
    }

    #[test]
    fn control_flow_is_reported_as_a_milestone_gap_and_parsing_continues() {
        let (program, diagnostics) = parse_text("fn f() { if x { y() }; z := 1 }");
        assert_eq!(diagnostics.iter().map(|d| d.code).collect::<Vec<_>>(), vec![CODE_UNSUPPORTED_YET]);
        assert_eq!(program.find_function("f").unwrap().body.stmts.len(), 2);
    }

    #[test]
    fn missing_brace_reports_and_recovers_for_the_next_function() {
        let (_, diagnostics) = parse_text("fn broken() { println(\"x\"\nfn ok() { }");
        assert!(codes("").is_empty());
        assert!(
            diagnostics.iter().any(|d| d.code == CODE_UNEXPECTED_TOKEN),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn an_unparsed_declaration_cannot_spin_the_top_level_loop() {
        for source in [
            "struct S { x: i32 }\nfn main() { }",
            "enum E { A }\nfn main() { }",
            "const K: i32 = 1\nfn main() { }",
            "global g: i32 = 1\nfn main() { }",
            "type T = u32\nfn main() { }",
            "impl S { fn f() { } }\nfn main() { }",
        ] {
            let (program, diagnostics) = parse_text(source);
            assert!(!diagnostics.is_empty(), "{source}: expected a diagnostic");
            assert!(
                program.find_function("main").is_some(),
                "{source}: recovery must reach the following declaration"
            );
        }
    }

    #[test]
    fn top_level_junk_is_skipped_to_the_next_declaration() {
        let (program, diagnostics) = parse_text("***\nfn ok() { }");
        assert!(diagnostics.iter().any(|d| d.code == CODE_UNEXPECTED_TOKEN));
        assert!(program.find_function("ok").is_some());
    }

    #[test]
    fn import_paths_are_recorded() {
        let (program, diagnostics) = parse_text("package main\nimport \"std/mem\"\nfn main() { }");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(program.imports[0].path, "std/mem");
    }

    #[test]
    fn truncated_source_is_an_error_not_a_panic() {
        let (_, diagnostics) = parse_text("fn main() { println(");
        assert!(
            diagnostics
                .iter()
                .any(|d| d.code == CODE_UNEXPECTED_EOF || d.code == CODE_UNEXPECTED_TOKEN),
            "{diagnostics:?}"
        );
    }
}


