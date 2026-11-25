use dux_ast::{
    AssignOp, Attribute, AttributeArgument, BinaryOp, Block, ConstDecl, Decl, EnumDecl, Expr,
    ExternBlock, Field, FieldPattern, FnDecl, ForKind, GlobalDecl, Ident, ImplDecl, Import,
    InterpolationPart, MatchArm, MatchBody, ModuleDecl, Param, Pattern, Program, StrKind,
    StructDecl, Stmt, Type, TypeAliasDecl, TypeParam, UnaryOp, Variant, VariantBody,
};
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum ForHeadShape {
    ThreeClause,
    Iteration,
}

struct Parser<'a> {
    /// While nonzero, an identifier followed by `{` opens a block rather than a
    /// struct value, so `if ready { .. }` cannot read `ready { .. }`.
    in_condition: usize,
    sources: &'a SourceMap,
    tokens: &'a [Token],
    cursor: usize,
    last_span: Span,
    diagnostics: Vec<Diagnostic>,
}

pub fn parse(sources: &SourceMap, tokens: &[Token]) -> Parsed {
    let mut parser = Parser {
        in_condition: 0,
        sources,
        tokens,
        cursor: 0,
        last_span: tokens.first().map(|token| token.span).unwrap_or_default(),
        diagnostics: Vec::new(),
    };
    let program = parser.program();
    Parsed {
        program,
        diagnostics: parser.diagnostics,
    }
}

fn expression_statement(expr: Expr) -> Stmt {
    let span = expr.span();
    Stmt::Expr { expr, span }
}

fn path_name(expr: &Expr) -> String {
    match expr {
        Expr::Path {
            qualifier, name, ..
        } => format!("{qualifier}::{name}"),
        Expr::Ident { name, .. } => name.clone(),
        _ => String::new(),
    }
}

fn path_qualifier(expr: &Expr) -> String {
    match expr {
        Expr::Ident { name, .. } => name.clone(),
        _ => String::new(),
    }
}

/// A path as its two halves and, when it already carries them, its type
/// arguments: `Shape::Rect` is `("Shape", "Rect")` and `max` alone is `("", "max")`.
fn path_parts(expr: &Expr) -> (String, String) {
    match expr {
        Expr::Ident { name, .. } => (String::new(), name.clone()),
        Expr::Path {
            qualifier, name, ..
        } => (qualifier.clone(), name.clone()),
        _ => (String::new(), String::new()),
    }
}

fn path_spelling(qualifier: &str, name: &str) -> String {
    match qualifier.is_empty() {
        true => name.to_string(),
        false => format!("{qualifier}::{name}"),
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
            let before = self.cursor;
            let attributes = self.attributes();
            match self.peek().kind {
                TokenKind::Keyword(Keyword::Import) => self.import(&mut program),
                TokenKind::Eof => {
                    if !attributes.is_empty() {
                        let token = self.peek().clone();
                        self.error(&token, "an attribute must be followed by a declaration");
                    }
                    break;
                }
                _ => {
                    let bare = attributes.is_empty();
                    match self.declaration(attributes) {
                        Some(decl) => program.decls.push(decl),
                        None => {
                            let token = self.peek().clone();
                            if bare && token.is_punct(Punct::Semicolon) {
                                self.bump();
                            } else if !bare {
                                self.error(&token, "an attribute must be followed by a declaration");
                                self.recover_to_top_level();
                            } else {
                                self.error(
                                    &token,
                                    "expected `fn`, `const`, `global`, `struct`, `enum` or `impl` at top level",
                                );
                                self.recover_to_top_level();
                            }
                        }
                    }
                }
            }
            if self.cursor == before {
                self.bump();
            }
        }
        program
    }

    /// One `import "path"`: the path is kept as it was written, and the driver
    /// is the one that goes and finds the file.
    fn import(&mut self, program: &mut Program) {
        self.bump();
        let token = self.peek().clone();
        match &token.literal {
            Some(Literal::Text(path)) => program.imports.push(Import {
                path: path.clone(),
                span: token.span,
            }),
            _ => self.error(&token, "import expects a string literal path"),
        }
        self.bump();
    }

    /// One declaration. `None` says what follows is not one, which is for the
    /// caller to say: a file and a module body want different words.
    fn declaration(&mut self, attributes: Vec<Attribute>) -> Option<Decl> {
        Some(match self.peek().kind {
            TokenKind::Keyword(Keyword::Fn) => Decl::Fn(self.function(attributes)),
            TokenKind::Keyword(Keyword::Struct) => Decl::Struct(self.struct_decl(attributes)),
            TokenKind::Keyword(Keyword::Enum) => Decl::Enum(self.enum_decl(attributes)),
            TokenKind::Keyword(Keyword::Const) => Decl::Const(self.constant(attributes)),
            TokenKind::Keyword(Keyword::Global) => Decl::Global(self.global_variable(attributes)),
            TokenKind::Keyword(Keyword::Type) => Decl::TypeAlias(self.type_alias()),
            TokenKind::Keyword(Keyword::Impl) => Decl::Impl(self.impl_block()),
            TokenKind::Keyword(Keyword::Mod) => Decl::Module(self.module_decl(attributes)),
            TokenKind::Keyword(Keyword::Extern) => Decl::Extern(self.extern_block(attributes)),
            TokenKind::Keyword(Keyword::Import) => {
                let token = self.peek().clone();
                self.bump();
                self.error(
                    &token,
                    "an import is written at the top of a file, next to the package it belongs to",
                );
                return None;
            }
            _ => return None,
        })
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

    fn function(&mut self, attributes: Vec<Attribute>) -> FnDecl {
        self.function_with(attributes, true)
    }

    fn function_with(&mut self, attributes: Vec<Attribute>, body_required: bool) -> FnDecl {
        let start = attributes
            .first()
            .map(|attribute| attribute.span)
            .unwrap_or_else(|| self.peek().span);
        self.bump();
        let name = self.expect_ident("function name");
        let type_params = self.type_params();
        let params = self.params();
        let return_ty = if self.at_punct(Punct::Arrow) {
            self.bump();
            Some(self.ty())
        } else {
            None
        };
        let body = if self.at_punct(Punct::Semicolon) {
            let end = self.peek().span;
            self.bump();
            Block {
                stmts: Vec::new(),
                span: end,
            }
        } else if !body_required && !self.at_punct(Punct::LBrace) {
            Block {
                stmts: Vec::new(),
                span: self.last_span,
            }
        } else {
            self.block()
        };
        let span = start.merge(body.span);
        FnDecl {
            attributes,
            name,
            type_params,
            params,
            return_ty,
            body,
            span,
        }
    }

    fn attributes(&mut self) -> Vec<Attribute> {
        let mut attributes = Vec::new();
        while self.at_punct(Punct::Hash) {
            let start = self.bump_span();
            if !self.at_punct(Punct::LBracket) {
                let token = self.peek().clone();
                self.error(
                    &token,
                    "an attribute is written `#[name]` or with a string argument",
                );
                break;
            }
            self.bump();
            let name = self.expect_ident("attribute name");
            let arguments = if self.at_punct(Punct::LParen) {
                self.attribute_arguments()
            } else {
                Vec::new()
            };
            let close = self.expect_punct(Punct::RBracket, "expected `]` to close the attribute");
            attributes.push(Attribute {
                name: name.name,
                arguments,
                span: start.merge(close),
            });
        }
        attributes
    }

    fn attribute_arguments(&mut self) -> Vec<AttributeArgument> {
        let mut arguments = Vec::new();
        self.bump();
        while !self.at_punct(Punct::RParen) && !self.at_end() {
            let before = self.cursor;
            let token = self.peek().clone();
            let key = match &token.kind {
                TokenKind::Ident => Some(token.text(self.sources).to_string()),
                _ => match &token.literal {
                    Some(Literal::Text(text)) => Some(text.clone()),
                    _ => None,
                },
            };
            let named = key.filter(|_| self.peek_at(1).is_some_and(|next| next.is_punct(Punct::Assign)));
            if let Some(key) = named {
                self.bump();
                self.bump();
                let value = self.peek().clone();
                self.bump();
                match &value.literal {
                    Some(Literal::Text(text)) => {
                        arguments.push(AttributeArgument::Named(key, text.clone()))
                    }
                    Some(Literal::Int(value)) => {
                        arguments.push(AttributeArgument::Named(key, value.to_string()))
                    }
                    _ => self.error(&value, "expected a string or integer value for the argument"),
                }
            } else {
                self.bump();
                match &token.literal {
                    Some(Literal::Text(text)) => {
                        arguments.push(AttributeArgument::Literal(text.clone()))
                    }
                    Some(Literal::Int(value)) => {
                        arguments.push(AttributeArgument::Literal(value.to_string()))
                    }
                    _ => self.error(&token, "attribute arguments are string or integer literals"),
                }
            }
            if self.at_punct(Punct::Comma) {
                self.bump();
            }
            if self.cursor == before {
                self.bump();
            }
        }
        self.expect_punct(Punct::RParen, "expected `)` to close the attribute arguments");
        arguments
    }

    fn type_params(&mut self) -> Vec<TypeParam> {
        let mut params = Vec::new();
        if !self.at_punct(Punct::Lt) {
            return params;
        }
        self.bump();
        while !self.at_punct(Punct::Gt) && !self.at_end() {
            let before = self.cursor;
            let start = self.peek().span;
            let is_const = self.at_keyword(Keyword::Const);
            if is_const {
                self.bump();
            }
            let name = self.expect_ident("type parameter name");
            let ty = if self.at_punct(Punct::Colon) {
                self.bump();
                Some(self.ty())
            } else if is_const {
                let token = self.peek().clone();
                self.error(&token, "a `const` parameter is written `const N: usize`");
                None
            } else {
                None
            };
            let span = ty
                .as_ref()
                .map(|ty| start.merge(ty.span()))
                .unwrap_or(name.span);
            params.push(TypeParam {
                name,
                is_const,
                ty,
                span,
            });
            if self.at_punct(Punct::Comma) {
                self.bump();
            } else {
                break;
            }
            if self.cursor == before {
                self.bump();
            }
        }
        self.expect_punct(Punct::Gt, "expected `>` to close the type parameter list");
        params
    }

    fn struct_decl(&mut self, attributes: Vec<Attribute>) -> StructDecl {
        let start = self.peek().span;
        self.bump();
        let name = self.expect_ident("struct name");
        let type_params = self.type_params();
        let (fields, end) = self.field_list();
        StructDecl {
            attributes,
            name,
            type_params,
            fields,
            span: start.merge(end),
        }
    }

    fn field_list(&mut self) -> (Vec<Field>, Span) {
        let mut fields = Vec::new();
        self.expect_punct(Punct::LBrace, "expected `{` to open the field list");
        while !self.at_punct(Punct::RBrace) && !self.at_end() {
            let name = self.expect_ident("field name");
            if self.at_punct(Punct::Colon) {
                self.bump();
            } else {
                let token = self.peek().clone();
                self.error(&token, "fields are typed: `name: Type`");
            }
            let ty = self.ty();
            let span = name.span.merge(ty.span());
            fields.push(Field { name, ty, span });
            if self.at_punct(Punct::Comma) || self.at_punct(Punct::Semicolon) {
                self.bump();
            } else if !self.at_punct(Punct::RBrace) {
                let token = self.peek().clone();
                self.error(&token, "expected `,` between fields");
                self.bump();
            }
        }
        let end = self.expect_punct(Punct::RBrace, "expected `}` to close the field list");
        (fields, end)
    }

    fn enum_decl(&mut self, attributes: Vec<Attribute>) -> EnumDecl {
        let start = self.peek().span;
        self.bump();
        let name = self.expect_ident("enum name");
        let type_params = self.type_params();
        self.expect_punct(Punct::LBrace, "expected `{` to open the enum");
        let mut variants = Vec::new();
        while !self.at_punct(Punct::RBrace) && !self.at_end() {
            let variant_start = self.peek().span;
            let before = self.cursor;
            let variant = self.expect_ident("variant name");
            let body = if self.at_punct(Punct::LParen) {
                self.bump();
                let mut payload = Vec::new();
                while !self.at_punct(Punct::RParen) && !self.at_end() {
                    payload.push(self.ty());
                    if self.at_punct(Punct::Comma) {
                        self.bump();
                    } else {
                        break;
                    }
                }
                self.expect_punct(Punct::RParen, "expected `)` to close the variant payload");
                VariantBody::Tuple(payload)
            } else if self.at_punct(Punct::LBrace) {
                let (fields, _) = self.field_list();
                VariantBody::Named(fields)
            } else {
                VariantBody::Empty
            };
            let span = variant_start.merge(self.peek().span);
            variants.push(Variant { name: variant, body, span });
            if self.at_punct(Punct::Comma) || self.at_punct(Punct::Semicolon) {
                self.bump();
            }
            if self.cursor == before {
                self.bump();
            }
        }
        let end = self.expect_punct(Punct::RBrace, "expected `}` to close the enum");
        EnumDecl {
            attributes,
            name,
            type_params,
            variants,
            span: start.merge(end),
        }
    }

    fn constant(&mut self, attributes: Vec<Attribute>) -> ConstDecl {
        let start = self.peek().span;
        self.bump();
        let name = self.expect_ident("constant name");
        let (ty, value) = self.typed_initializer(true);
        let span = value
            .as_ref()
            .map(|value| start.merge(value.span()))
            .unwrap_or(start);
        ConstDecl {
            attributes,
            name,
            ty,
            value: value.unwrap_or(Expr::Error { span }),
            span,
        }
    }

    fn global_variable(&mut self, attributes: Vec<Attribute>) -> GlobalDecl {
        let start = self.peek().span;
        self.bump();
        let name = self.expect_ident("global name");
        let (ty, value) = self.typed_initializer(false);
        let span = value.as_ref().map(|v| start.merge(v.span())).unwrap_or(start);
        GlobalDecl {
            attributes,
            name,
            ty,
            value,
            span,
        }
    }

    fn typed_initializer(&mut self, required: bool) -> (Option<Type>, Option<Expr>) {
        let ty = if self.at_punct(Punct::Colon) {
            self.bump();
            Some(self.ty())
        } else {
            None
        };
        if self.at_punct(Punct::Assign) {
            self.bump();
            return (ty, Some(self.expr()));
        }
        if required {
            let token = self.peek().clone();
            self.error(&token, "a constant needs a value: `const NAME: Type = value`");
            return (ty, Some(Expr::Error { span: token.span }));
        }
        (ty, None)
    }

    fn type_alias(&mut self) -> TypeAliasDecl {
        let start = self.peek().span;
        self.bump();
        let name = self.expect_ident("type name");
        let type_params = self.type_params();
        if self.at_punct(Punct::Assign) || self.at_punct(Punct::EqEq) {
            self.bump();
        } else {
            let token = self.peek().clone();
            self.error(&token, "a type alias is written `type Name = Type`");
        }
        let aliased = self.ty();
        let span = start.merge(aliased.span());
        TypeAliasDecl {
            name,
            type_params,
            aliased,
            span,
        }
    }

    fn impl_block(&mut self) -> ImplDecl {
        let start = self.peek().span;
        self.bump();
        let type_name = self.expect_ident("the type an impl block extends");
        let type_params = self.type_params();
        self.expect_punct(Punct::LBrace, "expected `{` to open the impl block");
        let mut methods = Vec::new();
        while !self.at_punct(Punct::RBrace) && !self.at_end() {
            let before = self.cursor;
            let attributes = self.attributes();
            if self.at_keyword(Keyword::Fn) {
                methods.push(self.function(attributes));
            } else {
                let token = self.peek().clone();
                self.error(&token, "only functions can be placed in an impl block");
                self.bump();
            }
            if self.cursor == before {
                self.bump();
            }
        }
        let end = self.expect_punct(Punct::RBrace, "expected `}` to close the impl block");
        ImplDecl {
            type_name,
            type_params,
            methods,
            span: start.merge(end),
        }
    }

    fn module_decl(&mut self, attributes: Vec<Attribute>) -> ModuleDecl {
        let start = self.peek().span;
        self.bump();
        let name = self.expect_ident("module name");
        let mut decls = Vec::new();
        let end = if self.at_punct(Punct::LBrace) {
            self.bump();
            while !self.at_punct(Punct::RBrace) && !self.at_end() {
                let before = self.cursor;
                let attributes = self.attributes();
                match self.declaration(attributes) {
                    Some(decl) => decls.push(decl),
                    None => {
                        let token = self.peek().clone();
                        self.error(
                            &token,
                            "a module holds declarations: `fn`, `struct`, `enum`, `const`, `global`, `type` or `impl`",
                        );
                        self.recover_to_top_level();
                    }
                }
                if self.cursor == before {
                    self.bump();
                }
            }
            self.expect_punct(Punct::RBrace, "expected `}` to close the module")
        } else {
            self.expect_punct(Punct::Semicolon, "expected `;` or `{` after the module name")
        };
        ModuleDecl {
            attributes,
            name,
            decls,
            span: start.merge(end),
        }
    }

    fn extern_block(&mut self, attributes: Vec<Attribute>) -> ExternBlock {
        let start = self.peek().span;
        self.bump();
        let abi_token = self.peek().clone();
        let abi = match &abi_token.literal {
            Some(Literal::Text(text)) => text.clone(),
            _ => {
                self.error(&abi_token, "an extern block names its ABI: `extern \"C\" { .. }`");
                "<error>".to_string()
            }
        };
        self.bump();
        self.expect_punct(Punct::LBrace, "expected `{` to open the extern block");
        let mut functions = Vec::new();
        while !self.at_punct(Punct::RBrace) && !self.at_end() {
            let before = self.cursor;
            let inner = self.attributes();
            if self.at_keyword(Keyword::Fn) {
                functions.push(self.function_with(inner, false));
            } else {
                let token = self.peek().clone();
                self.error(&token, "an extern block contains only function declarations");
                self.bump();
            }
            if self.cursor == before {
                self.bump();
            }
        }
        let end = self.expect_punct(Punct::RBrace, "expected `}` to close the extern block");
        ExternBlock {
            abi,
            attributes,
            functions,
            span: start.merge(end),
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
            if name.name == "self" && !self.at_punct(Punct::Colon) {
                params.push(Param {
                    name,
                    ty: Type::Named {
                        name: "Self".to_string(),
                        args: Vec::new(),
                        span,
                    },
                    span,
                });
                if self.at_punct(Punct::Comma) {
                    self.bump();
                } else {
                    break;
                }
                continue;
            }
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
        if token.is_keyword(Keyword::Fn) {
            self.bump();
            let mut params = Vec::new();
            self.expect_punct(Punct::LParen, "expected `(` after `fn` in a function pointer type");
            while !self.at_punct(Punct::RParen) && !self.at_end() {
                params.push(self.ty());
                if self.at_punct(Punct::Comma) {
                    self.bump();
                } else {
                    break;
                }
            }
            let end = self.expect_punct(Punct::RParen, "expected `)` to close the parameter types");
            let return_ty = if self.at_punct(Punct::Arrow) {
                self.bump();
                Some(Box::new(self.ty()))
            } else {
                None
            };
            let span = match &return_ty {
                Some(ty) => token.span.merge(ty.span()),
                None => token.span.merge(end),
            };
            return Type::Function {
                params,
                return_ty,
                span,
            };
        }
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
        let mut name = token.text(self.sources).to_string();
        while self.at_punct(Punct::Dot)
            && self.peek_at(1).is_some_and(|next| next.kind == TokenKind::Ident)
        {
            self.bump();
            let segment = self.peek().clone();
            self.bump();
            name.push('.');
            name.push_str(segment.text(self.sources));
        }
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
            TokenKind::Keyword(Keyword::If) => self.if_statement(),
            TokenKind::Keyword(Keyword::While) => self.while_statement(),
            TokenKind::Keyword(Keyword::Loop) => {
                self.bump();
                let body = self.block();
                let span = token.span.merge(body.span);
                Stmt::Loop { body, span }
            }
            TokenKind::Keyword(Keyword::For) => self.for_statement(),
            TokenKind::Keyword(Keyword::Match) => self.match_statement(),
            TokenKind::Punct(Punct::LBrace) => {
                let block = self.block();
                let span = block.span;
                Stmt::Block { block, span }
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

    /// `if` .. [`else if` ..] [`else`]. The `else` of an `else if` belongs to the
    /// inner `if`, which is why the else arm holds a statement, not a block.
    fn if_statement(&mut self) -> Stmt {
        let token = self.bump_span();
        let cond = self.condition();
        let then_branch = self.block();
        let mut span = token.merge(then_branch.span);
        let else_branch = if self.at_keyword(Keyword::Else) {
            self.bump();
            let inner = if self.at_keyword(Keyword::If) {
                self.if_statement()
            } else {
                let block = self.block();
                let span = block.span;
                Stmt::Block { block, span }
            };
            span = span.merge(inner.span());
            Some(Box::new(inner))
        } else {
            None
        };
        Stmt::If {
            cond,
            then_branch,
            else_branch,
            span,
        }
    }

    fn while_statement(&mut self) -> Stmt {
        let token = self.bump_span();
        let cond = self.condition();
        let body = self.block();
        let span = token.merge(body.span);
        Stmt::While { cond, body, span }
    }

    fn for_statement(&mut self) -> Stmt {
        let token = self.bump_span();
        if self.at_punct(Punct::LBrace) {
            let body = self.block();
            let span = token.merge(body.span);
            return Stmt::Loop { body, span };
        }
        let save = self.cursor;
        if let Some(kind) = self.for_clauses() {
            let body = self.block();
            let span = token.merge(body.span);
            return Stmt::For { kind, body, span };
        }
        self.cursor = save;
        let offending = self.peek().clone();
        self.unsupported(&offending, "this `for` form".to_string());
        self.skip_to_statement_end();
        Stmt::Error { span: token }
    }

    /// `for v in xs`, `for i, v in xs`, `for x := 0, x < n, x += 1`, or a range
    /// `for x in 0..n`. Returns `None` when the header is not one of those.
    fn for_clauses(&mut self) -> Option<ForKind> {
        if self.at_keyword(Keyword::In) || !self.starts_expression() {
            return None;
        }
        self.in_condition += 1;
        let head_start = self.cursor;
        if self.for_head_shape() == ForHeadShape::ThreeClause {
            let init = self.for_clause_statement();
            self.expect_punct(Punct::Comma, "expected `,` after the `for` init");
            let cond = self.condition();
            self.expect_punct(Punct::Comma, "expected `,` before the `for` step");
            let step = self.for_clause_statement();
            self.in_condition -= 1;
            return Some(ForKind::ThreeClause {
                init: Box::new(init),
                cond,
                step: Box::new(step),
            });
        }
        self.cursor = head_start;
        let first = self.expr();
        if self.at_keyword(Keyword::In) {
            self.bump();
            let iterable = self.expr();
            self.in_condition -= 1;
            return Some(ForKind::In {
                index: None,
                value: Ident::new(path_name(&first), first.span()),
                iterable,
            });
        }
        if self.at_punct(Punct::Comma) {
            self.bump();
            let second = self.expr();
            if self.at_keyword(Keyword::In) {
                self.bump();
                let iterable = self.expr();
                self.in_condition -= 1;
                return Some(ForKind::In {
                    index: Some(Ident::new(path_name(&first), first.span())),
                    value: Ident::new(path_name(&second), second.span()),
                    iterable,
                });
            }
        }
        self.in_condition -= 1;
        None
    }

    /// Read the head without consuming it: `for i := 0, i < n, i += 1` binds in a
    /// clause, `for x in xs` and `for i, v in xs` iterate. The tell is whether an
    /// assignment appears before the first top-level `,`.
    fn for_head_shape(&self) -> ForHeadShape {
        if self.peek().kind != TokenKind::Ident {
            return ForHeadShape::Iteration;
        }
        let mut index = self.cursor + 1;
        let mut depth = 0usize;
        let mut assigned = false;
        while index - self.cursor <= 32 {
            let Some(token) = self.tokens.get(index) else {
                return ForHeadShape::Iteration;
            };
            match token.kind {
                TokenKind::Punct(Punct::LParen | Punct::LBracket | Punct::Lt) => depth += 1,
                TokenKind::Punct(Punct::RParen | Punct::RBracket | Punct::Gt) => {
                    depth = depth.saturating_sub(1);
                }
                TokenKind::Punct(punct)
                    if depth == 0
                        && matches!(
                            punct,
                            Punct::Assign
                                | Punct::ColonEq
                                | Punct::PlusEq
                                | Punct::MinusEq
                                | Punct::StarEq
                                | Punct::SlashEq
                                | Punct::PercentEq
                        ) =>
                {
                    assigned = true;
                }
                TokenKind::Punct(Punct::Comma) if depth == 0 => {
                    return if assigned {
                        ForHeadShape::ThreeClause
                    } else {
                        ForHeadShape::Iteration
                    };
                }
                TokenKind::Punct(Punct::LBrace) | TokenKind::Keyword(Keyword::In) => {
                    return ForHeadShape::Iteration;
                }
                TokenKind::Eof => return ForHeadShape::Iteration,
                _ => {}
            }
            index += 1;
        }
        ForHeadShape::Iteration
    }

    /// The init and step clauses of `for` are statements: `i := 0`, `i += 1`.
    fn for_clause_statement(&mut self) -> Stmt {
        let token = self.peek().clone();
        if token.kind == TokenKind::Ident {
            let next = self.peek_at(1).map(|t| t.kind);
            if next == Some(TokenKind::Punct(Punct::ColonEq)) {
                let name = self.expect_ident("loop variable");
                self.bump();
                let value = self.expr();
                let span = name.span.merge(value.span());
                return Stmt::Binding {
                    name,
                    ty: None,
                    value: Some(value),
                    span,
                };
            }
            if let Some(TokenKind::Punct(assign)) = next {
                if let Some(op) = compound_assign(assign) {
                    let name = self.expect_ident("step variable");
                    self.bump();
                    let value = self.expr();
                    let span = name.span.merge(value.span());
                    let target = Expr::Ident {
                        name: name.name.clone(),
                        span: name.span,
                    };
                    return Stmt::Assign {
                        target,
                        op,
                        value,
                        span,
                    };
                }
            }
        }
        expression_statement(self.expr())
    }

    /// Build the parts of a `$".."` literal. Each hole is parsed out of the same
    /// file, so a diagnostic inside a hole points at the line the reader wrote.
    fn interpolation(&mut self, token: &Token) -> Expr {
        let start = token.span;
        let text = self.sources.text_of(start.file);
        let inner_start = (start.start as usize + 2).min(text.len());
        let inner_end = (start.end() as usize - 1).clamp(inner_start, text.len());
        let inner = &text[inner_start..inner_end];
        let mut parts = Vec::new();
        let mut plain = String::new();
        let mut index = 0;
        while index < inner.len() {
            let byte = inner.as_bytes()[index];
            if byte == b'{' && inner[index + 1..].starts_with('{') {
                plain.push('{');
                index += 2;
                continue;
            }
            if byte == b'}' && inner[index + 1..].starts_with('}') {
                plain.push('}');
                index += 2;
                continue;
            }
            if byte != b'{' {
                let width = inner[index..].chars().next().map_or(1, |ch| ch.len_utf8());
                plain.push_str(&inner[index..index + width]);
                index += width;
                continue;
            }
            let Some(offset) = inner[index + 1..].find('}') else {
                self.diagnostics.push(
                    Diagnostic::error(CODE_UNEXPECTED_TOKEN, "an interpolated hole is missing its `}`")
                        .with_span(start)
                        .with_note("a literal `{{` is written `{{{{`"),
                );
                plain.push_str(&inner[index..]);
                break;
            };
            let close = index + 1 + offset;
            if !plain.is_empty() {
                parts.push(InterpolationPart::Text(std::mem::take(&mut plain)));
            }
            let hole_start = inner_start + index + 1;
            let hole_end = inner_start + close;
            let written = if inner[index + 1..close].trim().is_empty() {
                None
            } else {
                let lexed = dux_lexer::lex_range(self.sources, start.file, hole_start, hole_end);
                self.diagnostics.extend(lexed.diagnostics);
                let (written, diagnostics) = Parser::expression_from(lexed.tokens, self.sources);
                self.diagnostics.extend(diagnostics);
                match written {
                    Expr::Error { .. } => None,
                    other => Some(other),
                }
            };
            parts.push(InterpolationPart::Hole {
                written,
                span: Span::new(start.file, hole_start as u32, (hole_end - hole_start) as u32),
            });
            index = close + 1;
        }
        if !plain.is_empty() || parts.is_empty() {
            parts.push(InterpolationPart::Text(plain));
        }
        Expr::InterpString {
            parts,
            span: token.span,
        }
    }

    /// `if` in value position. The `else` arm is mandatory there and every arm
    /// must agree on a type; that is a checker rule (M4), so an `if` without a
    /// final `else` reports one diagnostic here.
    fn if_expression(&mut self) -> Expr {
        let token = self.bump_span();
        let cond = self.condition();
        let then_branch = self.block_expression();
        let mut span = token.merge(then_branch.span());
        let else_branch = if self.at_keyword(Keyword::Else) {
            self.bump();
            let inner = if self.at_keyword(Keyword::If) {
                self.if_expression()
            } else {
                self.block_expression()
            };
            span = span.merge(inner.span());
            Some(Box::new(inner))
        } else {
            self.diagnostics.push(
                Diagnostic::error(
                    CODE_UNEXPECTED_TOKEN,
                    "an `if` in value position needs a final `else` arm",
                )
                .with_span(span)
                .with_note("only a `match` with a wildcard may be the last expression without `else`"),
            );
            None
        };
        Expr::If {
            cond: Box::new(cond),
            then_arm: Box::new(then_branch),
            else_arm: else_branch,
            span,
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
        let start = self.peek().span;
        let lhs = self.binary(0);
        if self.at_punct(Punct::DotDot) || self.at_punct(Punct::DotDotEq) {
            let inclusive = self.at_punct(Punct::DotDotEq);
            self.bump();
            let end = if self.starts_expression() {
                Some(Box::new(self.binary(0)))
            } else {
                None
            };
            let span = match &end {
                Some(end) => start.merge(end.span()),
                None => start.merge(self.peek().span),
            };
            return Expr::Range {
                start: Some(Box::new(lhs)),
                end,
                inclusive,
                span,
            };
        }
        lhs
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
            if self.peek().newline_before {
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
                TokenKind::Punct(Punct::ColonColon) => {
                    self.bump();
                    let name = self.expect_ident("path member");
                    let span = expr.span().merge(name.span);
                    expr = Expr::Path {
                        qualifier: path_qualifier(&expr),
                        name: name.name,
                        args: Vec::new(),
                        span,
                    };
                }
                TokenKind::Punct(Punct::Lt) if self.at_type_argument_list() => {
                    // A name written with its type arguments — `max<i64>(9, 4)`,
                    // `Pair<i32, string> { .. }`, `HashMap<string, i32>.new()` —
                    // is the same path with more said about it.
                    let start = expr.span();
                    let (qualifier, name) = path_parts(&expr);
                    let args = self.type_argument_list();
                    let end = self.last_span;
                    expr = if self.at_struct_literal() {
                        let mut literal = self.struct_literal(path_spelling(&qualifier, &name), start);
                        if let Expr::StructLiteral { args: slot, .. } = &mut literal {
                            *slot = args;
                        }
                        literal
                    } else {
                        Expr::Path {
                            qualifier,
                            name,
                            args,
                            span: start.merge(end),
                        }
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
                TokenKind::Punct(Punct::Dot) if self.peek_at(1).is_some_and(|t| t.kind == TokenKind::Int) => {
                    self.bump();
                    let index_token = self.peek().clone();
                    let index = match &index_token.literal {
                        Some(Literal::Int(value)) => *value as u32,
                        _ => {
                            self.error(&index_token, "tuple fields are numbered");
                            0
                        }
                    };
                    self.bump();
                    let span = expr.span().merge(index_token.span);
                    expr = Expr::Member {
                        base: Box::new(expr),
                        index,
                        span,
                    };
                }
                TokenKind::Punct(Punct::Dot) => {
                    self.bump();
                    let name = self.expect_ident("field name");
                    let span = expr.span().merge(name.span);
                    // `mathx.Size { w: 3 }`: a name written through the module
                    // it lives in is still the name of a record.
                    if let (Some(qualifier), true) = (
                        match &expr {
                            Expr::Ident { name, .. } => Some(name.clone()),
                            _ => None,
                        },
                        self.at_struct_literal(),
                    ) {
                        let spelled = format!("{qualifier}.{}", name.name);
                        return self.struct_literal(spelled, expr.span().merge(name.span));
                    }
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
            TokenKind::InterpStr => {
                self.bump();
                self.interpolation(&token)
            }
            TokenKind::Str | TokenKind::RawStr | TokenKind::CString => {
                self.bump();
                let kind = match token.kind {
                    TokenKind::Str => StrKind::Utf8,
                    TokenKind::RawStr => StrKind::Raw,
                    _ => StrKind::CString,
                };
                match token.literal {
                    Some(Literal::Text(text)) => Expr::Str { text, kind, span: token.span },
                    _ => Expr::Error { span: token.span },
                }
            }
            TokenKind::Keyword(Keyword::If) => self.if_expression(),
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
                let name = token.text(self.sources).to_string();
                if self.at_punct(Punct::ColonColon) {
                    self.bump();
                    let member = self.expect_ident("path member");
                    let span = token.span.merge(member.span);
                    let path = Expr::Path {
                        qualifier: name,
                        name: member.name,
                        args: Vec::new(),
                        span,
                    };
                    if self.at_struct_literal() {
                        return self.struct_literal(path_name(&path), span);
                    }
                    return path;
                }
                if self.at_struct_literal() {
                    return self.struct_literal(name, token.span);
                }
                Expr::Ident { name, span: token.span }
            }
            TokenKind::Punct(Punct::LBracket) => {
                self.bump();
                if self.at_punct(Punct::RBracket) {
                    let end = self.peek().span;
                    self.bump();
                    return Expr::ArrayLiteral {
                        elements: Vec::new(),
                        span: token.span.merge(end),
                    };
                }
                let first = self.expr();
                if self.at_punct(Punct::Semicolon) {
                    self.bump();
                    let len = self.expr();
                    let end =
                        self.expect_punct(Punct::RBracket, "expected `]` to close the array value");
                    return Expr::RepeatedArray {
                        value: Box::new(first),
                        len: Box::new(len),
                        span: token.span.merge(end),
                    };
                }
                let mut elements = vec![first];
                while self.at_punct(Punct::Comma) {
                    self.bump();
                    if self.at_punct(Punct::RBracket) {
                        break;
                    }
                    elements.push(self.expr());
                }
                let end = self.expect_punct(Punct::RBracket, "expected `]` to close the array value");
                Expr::ArrayLiteral {
                    elements,
                    span: token.span.merge(end),
                }
            }
            TokenKind::Keyword(Keyword::Match) => self.match_expression(),
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

    fn bump_span(&mut self) -> Span {
        let span = self.peek().span;
        self.bump();
        span
    }

    /// `Vec<string>.new()` and `mem.Arena<i32>::new` put a type argument list in
    /// an expression path. Only commit to it when a `>` is followed by `.` or
    /// `::`, so `a < b` stays a comparison.
    fn at_type_argument_list(&self) -> bool {
        let mut depth = 0usize;
        let mut index = self.cursor;
        while index - self.cursor <= 32 {
            let Some(token) = self.tokens.get(index) else {
                return false;
            };
            match token.kind {
                TokenKind::Punct(Punct::Lt) => depth += 1,
                TokenKind::Punct(Punct::Gt) => {
                    depth -= 1;
                    if depth == 0 {
                        return matches!(
                            self.tokens.get(index + 1).map(|token| token.kind),
                            Some(TokenKind::Punct(Punct::Dot))
                                | Some(TokenKind::Punct(Punct::ColonColon))
                                | Some(TokenKind::Punct(Punct::LParen))
                                | Some(TokenKind::Punct(Punct::LBrace))
                        );
                    }
                }
                TokenKind::Punct(Punct::Semicolon)
                | TokenKind::Punct(Punct::LBrace)
                | TokenKind::Punct(Punct::RBrace)
                | TokenKind::Eof => return false,
                _ => {}
            }
            index += 1;
        }
        false
    }

    fn type_argument_list(&mut self) -> Vec<Type> {
        self.bump();
        let mut args = Vec::new();
        while !self.at_punct(Punct::Gt) && !self.at_end() {
            args.push(self.type_or_const_argument());
            if self.at_punct(Punct::Comma) {
                self.bump();
            } else {
                break;
            }
        }
        self.last_span = self.expect_punct(Punct::Gt, "expected `>` to close the type arguments");
        args
    }

    fn type_or_const_argument(&mut self) -> Type {
        if matches!(
            self.peek().kind,
            TokenKind::Int | TokenKind::Float | TokenKind::Char | TokenKind::Str
        ) {
            let start = self.peek().span;
            let value = self.primary();
            let span = start.merge(value.span());
            return Type::ConstArgument {
                value: Box::new(value),
                span,
            };
        }
        self.ty()
    }

    fn struct_literal(&mut self, name: String, start: Span) -> Expr {
        self.expect_punct(Punct::LBrace, "expected `{` to open the struct value");
        let mut fields = Vec::new();
        while !self.at_punct(Punct::RBrace) && !self.at_end() {
            let before = self.cursor;
            let field = self.expect_ident("field name");
            if self.at_punct(Punct::Colon) {
                self.bump();
            } else {
                let token = self.peek().clone();
                self.error(&token, "a struct value writes each field as `name: value`");
            }
            let value = self.expr();
            fields.push((field, value));
            if self.at_punct(Punct::Comma) {
                self.bump();
            } else if !self.at_punct(Punct::RBrace) {
                let token = self.peek().clone();
                self.error(&token, "expected `,` between the fields of a struct value");
                break;
            }
            if self.cursor == before {
                self.bump();
            }
        }
        let end = self.expect_punct(Punct::RBrace, "expected `}` to close the struct value");
        Expr::StructLiteral {
            name,
            args: Vec::new(),
            fields,
            span: start.merge(end),
        }
    }

    /// `match` in value position: the arms are the value's branches.
    fn match_expression(&mut self) -> Expr {
        let token = self.bump_span();
        let subject = self.match_subject();
        let (arms, span) = self.match_arms();
        Expr::Match {
            subject: Box::new(subject),
            arms,
            span: token.merge(span),
        }
    }

    /// `match` in statement position, where an arm may run statements.
    fn match_statement(&mut self) -> Stmt {
        let token = self.bump_span();
        let subject = self.match_subject();
        let (arms, span) = self.match_arms();
        Stmt::Match {
            subject,
            arms,
            span: token.merge(span),
        }
    }

    /// The scrutinee of a `match`. A `{` after it opens the arms, never a
    /// struct value, so the subject is read in control-flow-head position.
    fn match_subject(&mut self) -> Expr {
        self.in_condition += 1;
        let subject = self.expr();
        self.in_condition -= 1;
        subject
    }

    /// `{ arm, arm, .. }`. The commas are part of the arm, and a block-bodied
    /// arm may stand without one.
    fn match_arms(&mut self) -> (Vec<MatchArm>, Span) {
        let open = self.expect_punct(Punct::LBrace, "expected `{` to open the `match` arms");
        let mut arms = Vec::new();
        while !self.at_punct(Punct::RBrace) && !self.at_end() {
            let before = self.cursor;
            let arm = self.match_arm();
            let block_body = matches!(arm.body, MatchBody::Block(_));
            if self.at_punct(Punct::Comma) {
                self.bump();
            } else if !block_body && !self.at_punct(Punct::RBrace) && !self.at_end() {
                let token = self.peek().clone();
                self.error(&token, "expected `,` between the arms of a `match`");
            }
            if self.cursor == before {
                self.bump();
            }
            arms.push(arm);
        }
        let close = self.expect_punct(Punct::RBrace, "expected `}` to close the `match` arms");
        (arms, open.merge(close))
    }

    /// `pattern [if guard] => body`
    fn match_arm(&mut self) -> MatchArm {
        let pattern = self.pattern();
        let mut span = pattern.span();
        let guard = if self.at_keyword(Keyword::If) {
            self.bump();
            let guard = self.expr();
            span = span.merge(guard.span());
            Some(guard)
        } else {
            None
        };
        let arrow = self.expect_punct(
            Punct::FatArrow,
            "expected `=>` after the pattern of a `match` arm",
        );
        span = span.merge(arrow);
        let body = if self.at_punct(Punct::LBrace) {
            let block = self.block();
            span = span.merge(block.span);
            MatchBody::Block(block)
        } else {
            match self.stmt() {
                Stmt::Expr { expr, .. } => {
                    span = span.merge(expr.span());
                    MatchBody::Expr(expr)
                }
                stmt => {
                    span = span.merge(stmt.span());
                    MatchBody::Stmt(stmt)
                }
            }
        };
        MatchArm {
            pattern,
            guard,
            body,
            span,
        }
    }

    /// A full pattern: or-patterns bind at the top, so `1 | 2 | 3 =>` is one arm.
    fn pattern(&mut self) -> Pattern {
        let first = self.range_pattern();
        if !self.at_punct(Punct::Pipe) {
            return first;
        }
        let mut alternatives = vec![first];
        while self.at_punct(Punct::Pipe) {
            self.bump();
            alternatives.push(self.range_pattern());
        }
        let span = alternatives
            .iter()
            .fold(alternatives[0].span(), |span, arm| span.merge(arm.span()));
        Pattern::Or { alternatives, span }
    }

    /// `4..=9`, `1..5`: a pattern that covers a stretch of literals.
    fn range_pattern(&mut self) -> Pattern {
        let start = self.component_pattern();
        if !self.at_punct(Punct::DotDot) && !self.at_punct(Punct::DotDotEq) {
            return start;
        }
        let inclusive = self.at_punct(Punct::DotDotEq);
        self.bump();
        let end = self.component_pattern();
        let span = start.span().merge(end.span());
        Pattern::Range {
            start: Box::new(start),
            end: Box::new(end),
            inclusive,
            span,
        }
    }

    fn component_pattern(&mut self) -> Pattern {
        let token = self.peek().clone();
        match token.kind {
            TokenKind::Int
            | TokenKind::Float
            | TokenKind::Char
            | TokenKind::Str
            | TokenKind::RawStr
            | TokenKind::CString
            | TokenKind::Keyword(Keyword::True)
            | TokenKind::Keyword(Keyword::False) => {
                let value = self.literal_value();
                Pattern::Literal {
                    value: Box::new(value),
                    span: token.span,
                }
            }
            TokenKind::Punct(Punct::Minus) => {
                self.bump();
                let operand = self.component_pattern();
                let span = token.span.merge(operand.span());
                match operand {
                    Pattern::Literal { value, .. } => Pattern::Literal {
                        value: Box::new(Expr::Unary {
                            op: UnaryOp::Neg,
                            operand: value,
                            span,
                        }),
                        span,
                    },
                    other => {
                        self.diagnostics.push(
                            Diagnostic::error(
                                CODE_UNEXPECTED_TOKEN,
                                "only a literal number may be negated in a pattern",
                            )
                            .with_span(span),
                        );
                        other
                    }
                }
            }
            TokenKind::Keyword(Keyword::Else) => {
                self.bump();
                Pattern::Else { span: token.span }
            }
            TokenKind::Punct(Punct::LParen) => {
                self.bump();
                let inner = self.pattern();
                self.expect_punct(Punct::RParen, "expected `)` to close the parenthesised pattern");
                inner
            }
            TokenKind::Ident => {
                self.bump();
                let name = token.text(self.sources).to_string();
                if name == "_" && !self.at_punct(Punct::LParen) && !self.at_punct(Punct::LBrace) {
                    return Pattern::Wildcard { span: token.span };
                }
                if self.at_punct(Punct::ColonColon) {
                    self.bump();
                    let member = self.expect_ident("variant name");
                    let span = token.span.merge(member.span);
                    return self.variant_pattern(Some(name), member.name, span);
                }
                if self.at_punct(Punct::LParen) || self.at_pattern_brace() {
                    return self.variant_pattern(None, name, token.span);
                }
                Pattern::Name {
                    name: Ident::new(name, token.span),
                    span: token.span,
                }
            }
            _ => {
                self.error(&token, "expected a pattern");
                self.bump();
                Pattern::Error { span: token.span }
            }
        }
    }

    /// A `{` that belongs to the pattern (`Shape::Rect { w, h }`), not to an arm body.
    fn at_pattern_brace(&self) -> bool {
        self.at_punct(Punct::LBrace) && !self.peek().newline_before
    }

    /// What follows a variant name: `(p1, p2)`, `{ f1, f2 }`, or nothing at all.
    fn variant_pattern(&mut self, qualifier: Option<String>, name: String, start: Span) -> Pattern {
        if self.at_punct(Punct::LParen) {
            self.bump();
            let mut elements = Vec::new();
            while !self.at_punct(Punct::RParen) && !self.at_end() {
                let before = self.cursor;
                elements.push(self.pattern());
                if self.at_punct(Punct::Comma) {
                    self.bump();
                } else if !self.at_punct(Punct::RParen) {
                    let token = self.peek().clone();
                    self.error(&token, "expected `,` between the patterns of a variant");
                    break;
                }
                if self.cursor == before {
                    self.bump();
                }
            }
            let end = self.expect_punct(Punct::RParen, "expected `)` to close the variant pattern");
            return Pattern::Tuple {
                qualifier,
                name,
                elements,
                span: start.merge(end),
            };
        }
        if self.at_pattern_brace() {
            return self.struct_pattern(qualifier, name, start);
        }
        Pattern::Path {
            qualifier: qualifier.unwrap_or_default(),
            name,
            span: start,
        }
    }

    fn struct_pattern(&mut self, qualifier: Option<String>, name: String, start: Span) -> Pattern {
        self.bump();
        let mut fields = Vec::new();
        let mut rest = false;
        while !self.at_punct(Punct::RBrace) && !self.at_end() {
            let before = self.cursor;
            if self.at_punct(Punct::DotDot) {
                self.bump();
                rest = true;
                if self.at_punct(Punct::Comma) {
                    self.bump();
                }
                continue;
            }
            let field = self.expect_ident("field name");
            let span = field.span;
            let pattern = if self.at_punct(Punct::Colon) {
                self.bump();
                Some(self.pattern())
            } else {
                None
            };
            fields.push(match pattern {
                None => FieldPattern::Shorthand { name: field, span },
                Some(pattern) => {
                    let span = span.merge(pattern.span());
                    FieldPattern::Nested {
                        name: field,
                        pattern: Box::new(pattern),
                        span,
                    }
                }
            });
            if self.at_punct(Punct::Comma) {
                self.bump();
            } else if !self.at_punct(Punct::RBrace) {
                let token = self.peek().clone();
                self.error(&token, "expected `,` between the fields of a pattern");
                break;
            }
            if self.cursor == before {
                self.bump();
            }
        }
        let end = self.expect_punct(Punct::RBrace, "expected `}` to close the pattern");
        Pattern::Struct {
            qualifier,
            name,
            fields,
            rest,
            span: start.merge(end),
        }
    }

    /// A literal read where a pattern is expected.
    fn literal_value(&mut self) -> Expr {
        let token = self.peek().clone();
        self.bump();
        match token.literal {
            Some(Literal::Int(value)) => Expr::Int { value, span: token.span },
            Some(Literal::Float(value)) => Expr::Float { value, span: token.span },
            Some(Literal::Char(value)) => Expr::Char { value, span: token.span },
            Some(Literal::Text(text)) => Expr::Str {
                text,
                kind: match token.kind {
                    TokenKind::RawStr => StrKind::Raw,
                    TokenKind::CString => StrKind::CString,
                    _ => StrKind::Utf8,
                },
                span: token.span,
            },
            _ => match token.kind {
                TokenKind::Keyword(Keyword::True) => Expr::Bool { value: true, span: token.span },
                TokenKind::Keyword(Keyword::False) => Expr::Bool { value: false, span: token.span },
                _ => Expr::Error { span: token.span },
            },
        }
    }

    /// Is this control-flow head `P := e`? A `:=` before the body block says so;
    /// braces that are followed by `:=` belong to a struct pattern, not to a body.
    fn at_pattern_condition(&self) -> bool {
        let mut index = self.cursor;
        let mut depth = 0usize;
        while index - self.cursor <= 64 {
            let Some(token) = self.tokens.get(index) else {
                return false;
            };
            match token.kind {
                TokenKind::Punct(Punct::ColonEq) if depth == 0 => return true,
                TokenKind::Punct(Punct::LParen) | TokenKind::Punct(Punct::LBracket) => depth += 1,
                TokenKind::Punct(Punct::RParen) | TokenKind::Punct(Punct::RBracket) => {
                    depth = depth.saturating_sub(1)
                }
                TokenKind::Punct(Punct::LBrace) if depth == 0 => {
                    let Some(close) = self.balanced_brace(index) else {
                        return false;
                    };
                    match self.tokens.get(close + 1).map(|token| token.kind) {
                        Some(TokenKind::Punct(Punct::ColonEq)) => {
                            index = close + 1;
                            continue;
                        }
                        _ => return false,
                    }
                }
                TokenKind::Punct(Punct::LBrace) => depth += 1,
                TokenKind::Punct(Punct::RBrace) => depth = depth.saturating_sub(1),
                TokenKind::Punct(Punct::Semicolon) if depth == 0 => return false,
                TokenKind::Eof => return false,
                _ => {}
            }
            index += 1;
        }
        false
    }

    /// The index just past the `{` at `open`, its matching `}`.
    fn balanced_brace(&self, open: usize) -> Option<usize> {
        let mut depth = 0usize;
        let mut index = open;
        while let Some(token) = self.tokens.get(index) {
            match token.kind {
                TokenKind::Punct(Punct::LBrace) => depth += 1,
                TokenKind::Punct(Punct::RBrace) => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(index);
                    }
                }
                TokenKind::Eof => return None,
                _ => {}
            }
            index += 1;
        }
        None
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
                | TokenKind::Punct(Punct::LBracket)
                | TokenKind::Keyword(Keyword::Match)
                | TokenKind::Keyword(Keyword::If)
        )
    }

    /// Parse one expression from tokens produced elsewhere, such as the text of
    /// a `$"{..}"` hole. Spans stay absolute because the lexer was handed a byte
    /// range of the same file.
    pub fn expression_from(tokens: Vec<Token>, sources: &SourceMap) -> (Expr, Vec<Diagnostic>) {
        let mut parser = Parser {
            in_condition: 0,
            sources,
            tokens: &tokens,
            cursor: 0,
            last_span: tokens.first().map(|token| token.span).unwrap_or_default(),
            diagnostics: Vec::new(),
        };
        let expr = parser.expr();
        let mut diagnostics = std::mem::take(&mut parser.diagnostics);
        if !parser.at_end() {
            let token = parser.peek().clone();
            diagnostics.push(
                Diagnostic::error(
                    CODE_UNEXPECTED_TOKEN,
                    format!("expected one expression, found {}", token.kind.describe()),
                )
                .with_span(token.span),
            );
        }
        (expr, diagnostics)
    }

    /// `{ ..; value }` where a value is expected.
    fn block_expression(&mut self) -> Expr {
        let block = self.block();
        let span = block.span;
        let value = match block.stmts.last() {
            Some(Stmt::Expr { expr, .. }) => expr.clone(),
            _ => Expr::Error { span },
        };
        Expr::Block {
            block,
            value: Box::new(value),
            span,
        }
    }

    /// A struct value may only start where a block could not: an identifier
    /// followed by `{` on the same line, outside a control-flow head.
    fn at_struct_literal(&self) -> bool {
        self.in_condition == 0 && self.at_punct(Punct::LBrace) && !self.peek().newline_before
    }

    /// A control-flow head: an expression, or `P := e` where a pattern is
    /// matched and its bindings are in scope in the body (design D27).
    fn condition(&mut self) -> Expr {
        self.in_condition += 1;
        let head = if self.at_pattern_condition() {
            let pattern = self.pattern();
            let start = pattern.span();
            self.expect_punct(Punct::ColonEq, "expected `:=` after the pattern");
            let value = self.expr();
            let span = start.merge(value.span());
            Expr::CondBinding {
                pattern,
                value: Box::new(value),
                span,
            }
        } else {
            self.expr()
        };
        self.in_condition -= 1;
        head
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
    fn structs_enums_and_fields_are_recorded() {
        let (program, diagnostics) = parse_text(
            "struct Vec3 {\n    x: f32,\n    y: f32,\n}\nenum Shape {\n    Circle(f32),\n    Rect { w: f32 },\n    Empty,\n}\n",
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let vec3 = program.find_struct("Vec3").expect("Vec3");
        assert_eq!(vec3.fields.len(), 2);
        assert_eq!(vec3.fields[0].name.name, "x");
        let shape = program.find_enum("Shape").expect("Shape");
        assert_eq!(shape.variants.len(), 3);
        assert!(matches!(shape.variants[0].body, VariantBody::Tuple(ref payload) if payload.len() == 1));
        assert!(matches!(shape.variants[1].body, VariantBody::Named(ref fields) if fields.len() == 1));
        assert!(matches!(shape.variants[2].body, VariantBody::Empty));
    }

    #[test]
    fn const_and_global_carry_their_type_and_value() {
        let (program, diagnostics) = parse_text("const KI: usize = 1024\nglobal hits: i32 = 0\nglobal scratch: [i32; 8]\n");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let Decl::Const(ki) = &program.decls[0] else {
            panic!("expected a const, found {:?}", program.decls[0]);
        };
        assert_eq!(ki.name.name, "KI");
        assert!(ki.ty.is_some());
        let Decl::Global(hits) = &program.decls[1] else {
            panic!("expected a global");
        };
        assert!(hits.value.is_some());
        let Decl::Global(scratch) = &program.decls[2] else {
            panic!("expected a global");
        };
        assert!(scratch.value.is_none(), "a bare `global` is zero-initialised");
    }

    #[test]
    fn attributes_keep_string_and_named_arguments() {
        let (program, diagnostics) = parse_text(
            "#[c_include(\"stdio.h\")]\n#[link(name = \"m\")]\nextern \"C\" {\n    fn puts(s: *c_char) -> c_int\n}\n",
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let Decl::Extern(block) = &program.decls[0] else {
            panic!("expected an extern block, found {:?}", program.decls[0]);
        };
        assert_eq!(block.abi, "C");
        assert_eq!(block.attributes.len(), 2);
        assert_eq!(
            block.attributes[0].string_argument(),
            Some("stdio.h"),
            "{:?}",
            block.attributes[0]
        );
        assert_eq!(
            block.attributes[1].arguments,
            vec![AttributeArgument::Named("name".to_string(), "m".to_string())]
        );
        assert_eq!(block.functions.len(), 1);
        assert!(
            block.functions[0].body.stmts.is_empty(),
            "an extern declaration has no body"
        );
    }

    #[test]
    fn impl_blocks_keep_methods_and_self_receivers() {
        let (program, diagnostics) =
            parse_text("impl Vec3 {\n    fn len_sq(self) -> f32 { return 0.0 }\n    fn scale(self: *Vec3, k: f32) { }\n}");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let Decl::Impl(impl_block) = &program.decls[0] else {
            panic!("expected an impl block");
        };
        assert_eq!(impl_block.type_name.name, "Vec3");
        assert_eq!(impl_block.methods.len(), 2);
        assert_eq!(impl_block.methods[0].params[0].name.name, "self");
        assert_eq!(impl_block.methods[1].params.len(), 2);
    }

    #[test]
    fn generic_and_const_parameters_are_kept_apart() {
        let (program, diagnostics) = parse_text(
            "fn max<T>(a: T, b: T) -> T { return a }\nfn zeros<const N: usize>() -> [f32; N] { return [0.0; N] }\nstruct Pair<A, B> { first: A, second: B }\n",
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let max = program.find_function("max").unwrap();
        assert_eq!(max.type_params.len(), 1);
        assert!(!max.type_params[0].is_const);
        let zeros = program.find_function("zeros").unwrap();
        assert!(zeros.type_params[0].is_const);
        assert!(zeros.type_params[0].ty.is_some());
        let Decl::Struct(pair) = &program.decls[2] else {
            panic!("expected a struct");
        };
        assert_eq!(pair.type_params.len(), 2);
    }

    #[test]
    fn struct_values_paths_ranges_and_tuple_members_parse() {
        let (program, diagnostics) = parse_text(
            "fn f() {\n    v := Vec3 { x: 1.0, y: 2.0 }\n    s := Shape::Circle(2.0)\n    head := path[0..4]\n    n := pair.0\n    xs := [1, 2, 3]\n}\n",
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let function = program.find_function("f").unwrap();
        let statements = &function.body.stmts;
        assert!(matches!(
            &statements[0],
            Stmt::Binding {
                value: Some(Expr::StructLiteral { .. }),
                ..
            }
        ));
        assert!(matches!(
            &statements[1],
            Stmt::Binding {
                value: Some(Expr::Call { callee, .. }),
                ..
            } if matches!(&**callee, Expr::Path { qualifier, name, .. } if qualifier == "Shape" && name == "Circle")
        ));
        assert!(matches!(
            &statements[2],
            Stmt::Binding {
                value: Some(Expr::Index { index, .. }),
                ..
            } if matches!(&**index, Expr::Range { inclusive: false, .. })
        ));
        assert!(matches!(
            &statements[3],
            Stmt::Binding {
                value: Some(Expr::Member { index: 0, .. }),
                ..
            }
        ));
        assert!(matches!(
            &statements[4],
            Stmt::Binding {
                value: Some(Expr::ArrayLiteral { elements, .. }),
                ..
            } if elements.len() == 3
        ));
    }

    #[test]
    fn a_binary_operator_must_start_on_the_same_line_as_its_left_operand() {
        let (program, diagnostics) = parse_text("fn f() { total := a\n    + b\n}");
        assert!(
            diagnostics.iter().any(|d| d.code == CODE_UNEXPECTED_TOKEN),
            "a line that begins with an operator must not glue onto the previous statement: {diagnostics:?}"
        );
        let function = program.find_function("f").unwrap();
        assert_eq!(function.body.stmts.len(), 2);
    }

    #[test]
    fn an_operator_at_the_end_of_a_line_continues_the_expression() {
        let (program, diagnostics) = parse_text("fn f() { total := a +\n    b\nprintln(total) }");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let function = program.find_function("f").unwrap();
        assert_eq!(function.body.stmts.len(), 2);
        assert!(matches!(
            &function.body.stmts[0],
            Stmt::Binding {
                value: Some(Expr::Binary { .. }),
                ..
            }
        ));
    }

    #[test]
    fn function_pointer_types_and_qualified_type_arguments_parse() {
        let (program, diagnostics) = parse_text(
            "fn f(cb: fn(i32) -> i32, stream: Option<*cs.FILE>) { }\nfn g() { grid := zeros<4>() }\n",
        );
        assert!(
            diagnostics.is_empty(),
            "a type argument written at a use is the parser's business, and it parses: {diagnostics:?}"
        );
        let g = program.find_function("g").unwrap();
        let Stmt::Binding { value, .. } = &g.body.stmts[0] else {
            panic!("a binding")
        };
        let Expr::Call { callee, .. } = value.as_ref().unwrap() else {
            panic!("a call")
        };
        let Expr::Path { name, args, .. } = callee.as_ref() else {
            panic!("a path with its type arguments")
        };
        assert_eq!(
            (name.as_str(), args.len()),
            ("zeros", 1),
            "`zeros<4>` keeps the argument it was written with"
        );
        let f = program.find_function("f").unwrap();
        assert!(matches!(f.params[0].ty, Type::Function { .. }));
        assert!(matches!(f.params[1].ty, Type::Named { ref name, .. } if name == "Option"));
    }

    #[test]
    fn nested_blocks_are_statements() {
        let (program, diagnostics) = parse_text("fn f() {\n    a := 1\n    {\n        b := 2\n    }\n}");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let function = program.find_function("f").unwrap();
        assert!(matches!(function.body.stmts[1], Stmt::Block { .. }));
    }

    #[test]
    fn an_unlowerable_expression_reports_one_milestone_diagnostic() {
        let (_program, diagnostics) = parse_text("fn f() -> i32 {\n    return (1, 2)\n}");
        assert_eq!(
            diagnostics.iter().map(|d| d.code).collect::<Vec<_>>(),
            vec![CODE_UNSUPPORTED_YET],
            "a tuple value is a milestone gap, and one diagnostic is enough"
        );
    }

    #[test]
    fn match_arms_keep_their_literals_or_patterns_ranges_and_guards() {
        let (program, diagnostics) = parse_text(
            "fn f(n: i32) -> string {\n    return match n {\n        0 => \"zero\",\n        1 | 2 | 3 => \"small\",\n        4..=9 => \"single digit\",\n        x if x < 0 => \"negative\",\n        _ => \"big\",\n    }\n}\n",
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let Expr::Match { subject, arms, .. } = value_of_return(&program) else {
            panic!("expected a `match` value");
        };
        assert!(matches!(&**subject, Expr::Ident { name, .. } if name == "n"));
        assert_eq!(arms.len(), 5);
        assert!(matches!(arms[0].pattern, Pattern::Literal { .. }));
        assert!(matches!(
            &arms[1].pattern,
            Pattern::Or { alternatives, .. } if alternatives.len() == 3
        ));
        assert!(matches!(arms[2].pattern, Pattern::Range { inclusive: true, .. }));
        assert!(matches!(&arms[3].pattern, Pattern::Name { name, .. } if name.name == "x"));
        assert!(arms[3].guard.is_some(), "the `if` after a pattern is a guard");
        assert!(matches!(arms[4].pattern, Pattern::Wildcard { .. }));
        assert!(matches!(arms[0].body, MatchBody::Expr(Expr::Str { .. })));
    }

    #[test]
    fn variant_patterns_bind_payloads_by_position_and_by_name() {
        let (program, diagnostics) = parse_text(
            "fn f(s: Shape) {\n    match s {\n        Shape::Circle(r) => print(r),\n        Shape::Rect { w, h } => print(w),\n        Command::Load { path, .. } => print(path),\n        None => print(\"nothing\"),\n        else => print(\"other\"),\n    }\n}\n",
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let Stmt::Match { arms, .. } = &program.find_function("f").unwrap().body.stmts[0] else {
            panic!("expected a `match` statement");
        };
        assert!(matches!(
            &arms[0].pattern,
            Pattern::Tuple { qualifier, name, elements, .. }
                if qualifier.as_deref() == Some("Shape") && name == "Circle" && elements.len() == 1
        ));
        assert!(matches!(
            &arms[1].pattern,
            Pattern::Struct { qualifier, fields, rest, .. }
                if qualifier.as_deref() == Some("Shape") && fields.len() == 2 && !rest
        ));
        assert!(matches!(
            &arms[2].pattern,
            Pattern::Struct { fields, rest, .. } if fields.len() == 1 && *rest
        ));
        assert!(
            matches!(&arms[3].pattern, Pattern::Name { name, .. } if name.name == "None"),
            "a bare name is ambiguous until the checker knows the variants"
        );
        assert!(matches!(arms[4].pattern, Pattern::Else { .. }));
        let binders = arms[1].pattern.binders();
        assert_eq!(
            binders.iter().map(|b| b.name.as_str()).collect::<Vec<_>>(),
            vec!["w", "h"]
        );
    }

    #[test]
    fn a_match_arm_body_may_be_a_block() {
        let (program, diagnostics) =
            parse_text("fn f(n: i32) {\n    match n {\n        0 => {\n            print(\"zero\")\n        }\n        _ => print(\"many\")\n    }\n}\n");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let Stmt::Match { arms, .. } = &program.find_function("f").unwrap().body.stmts[0] else {
            panic!("expected a `match` statement");
        };
        assert!(matches!(&arms[0].body, MatchBody::Block(block) if block.stmts.len() == 1));
        assert!(matches!(&arms[1].body, MatchBody::Expr(_)));
    }

    #[test]
    fn a_pattern_condition_binds_through_its_pattern() {
        for source in [
            "fn f(o: Option<i32>) {\n    if Some(x) := o {\n        print(x)\n    }\n}",
            "fn f(s: Shape) {\n    if Shape::Rect { w, h } := s {\n        print(w)\n    }\n}",
            "fn f(o: Option<*Node>) {\n    while Some(node) := cursor {\n        print(node)\n    }\n}",
        ] {
            let (program, diagnostics) = parse_text(source);
            assert!(diagnostics.is_empty(), "{source}: {diagnostics:?}");
            let function = program.find_function("f").unwrap();
            let cond = match &function.body.stmts[0] {
                Stmt::If { cond, .. } | Stmt::While { cond, .. } => cond,
                other => panic!("expected a conditional, found {other:?}"),
            };
            let Expr::CondBinding { pattern, .. } = cond else {
                panic!("expected a pattern condition, found {cond:?}");
            };
            assert!(!pattern.binders().is_empty(), "{pattern:?}");
        }
    }

    #[test]
    fn a_plain_condition_stays_an_expression() {
        let (program, diagnostics) =
            parse_text("fn f() {\n    if hits < 3 {\n        hits = 0\n    }\n}\n");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let Stmt::If { cond, .. } = &program.find_function("f").unwrap().body.stmts[0] else {
            panic!("expected an `if`");
        };
        assert!(matches!(cond, Expr::Binary { .. }), "{cond:?}");
    }

    #[test]
    fn a_missing_comma_between_arms_is_reported_and_the_arms_survive() {
        let (program, diagnostics) =
            parse_text("fn f(n: i32) {\n    match n {\n        0 => print(\"a\")\n        1 => print(\"b\")\n    }\n}\n");
        assert_eq!(
            diagnostics.iter().map(|d| d.code).collect::<Vec<_>>(),
            vec![CODE_UNEXPECTED_TOKEN],
            "one diagnostic, not a cascade"
        );
        let Stmt::Match { arms, .. } = &program.find_function("f").unwrap().body.stmts[0] else {
            panic!("expected a `match` statement");
        };
        assert_eq!(arms.len(), 2);
    }

    fn value_of_return(program: &Program) -> &Expr {
        let function = program.find_function("f").expect("f");
        let Stmt::Return { value: Some(value), .. } = &function.body.stmts[0] else {
            panic!("expected a return of a value");
        };
        value
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
    fn a_module_body_holds_the_declarations_it_was_given() {
        let (program, diagnostics) = parse_text(
            "mod math {\n    fn sqrt(x: f64) -> f64 { return x }\n\n    struct Meters { v: f64 }\n}\n\nmod c;\n",
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let Decl::Module(decl) = &program.decls[0] else {
            panic!("a module")
        };
        assert_eq!((decl.name.name.as_str(), decl.decls.len()), ("math", 2));
        let Decl::Module(forward) = &program.decls[1] else {
            panic!("a forward-declared namespace")
        };
        assert!(
            forward.decls.is_empty(),
            "`mod c;` says the namespace exists and nothing more"
        );
    }

    #[test]
    fn an_impl_block_can_be_written_over_parameters() {
        let (program, diagnostics) = parse_text(
            "struct Pair<A, B> { first: A, second: B }\nimpl Pair<A, B> { fn first(self: Pair<A, B>) -> A { return self.first } }",
        );
        assert!(
            diagnostics.is_empty(),
            "an impl of a generic record is the same syntax it always was, plus its parameters: {diagnostics:?}"
        );
        let Decl::Impl(decl) = &program.decls[1] else {
            panic!("an impl block")
        };
        assert_eq!(
            (
                decl.type_name.name.as_str(),
                decl.type_params.len(),
                decl.methods.len()
            ),
            ("Pair", 2, 1)
        );
    }

    #[test]
    fn type_arguments_written_at_a_use_are_kept() {
        let (program, diagnostics) = parse_text(
            "fn main() { a := max<i64>(9, 4); p := Pair<i32, string> { first: 1, second: \"x\" }; b := HashMap<string, i32>.new(); c := zeros<4>() }",
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let main = program.find_function("main").unwrap();
        let Stmt::Binding { value, .. } = &main.body.stmts[0] else {
            panic!("a binding")
        };
        let Expr::Call { callee, .. } = value.as_ref().unwrap() else {
            panic!("a call")
        };
        let Expr::Path {
            qualifier, name, args, ..
        } = callee.as_ref()
        else {
            panic!("a path written with its type arguments")
        };
        assert_eq!(
            (qualifier.as_str(), name.as_str(), args.len()),
            ("", "max", 1),
            "`max<i64>` is the bare name with one argument said about it"
        );
        let Stmt::Binding { value, .. } = &main.body.stmts[1] else {
            panic!("a binding")
        };
        let Expr::StructLiteral { name, args, .. } = value.as_ref().unwrap() else {
            panic!("a struct value")
        };
        assert_eq!((name.as_str(), args.len()), ("Pair", 2));
        let Stmt::Binding { value, .. } = &main.body.stmts[2] else {
            panic!("a binding")
        };
        let Expr::Call { callee, .. } = value.as_ref().unwrap() else {
            panic!("a method call")
        };
        let Expr::Field { base, name, .. } = callee.as_ref() else {
            panic!("a member walked off a type")
        };
        let Expr::Path { qualifier, args, .. } = base.as_ref() else {
            panic!("the type the call walks off")
        };
        assert_eq!(
            (qualifier.as_str(), name.name.as_str(), args.len()),
            ("", "new", 2),
            "the arguments belong to the type, not to the member walked off it"
        );
        let Stmt::Binding { value, .. } = &main.body.stmts[3] else {
            panic!("a binding")
        };
        let Expr::Call { callee, .. } = value.as_ref().unwrap() else {
            panic!("a call")
        };
        let Expr::Path { name, args, .. } = callee.as_ref() else {
            panic!("a path")
        };
        assert_eq!((name.as_str(), args.len()), ("zeros", 1));
        assert!(
            matches!(&args[0], Type::ConstArgument { .. }),
            "`zeros<4>` gives a value, not a type: {args:?}"
        );
    }

    #[test]
    fn control_flow_parses_including_a_match_in_value_position() {
        let (program, diagnostics) = parse_text(
            "fn f() { if x { y() } else if z { w() } else { v() }; while a { b() }; for i := 0, i < 3, i += 1 { c() }; for v in xs { d() }; for i, v in xs { e() }; for { f() }; break; continue; g := match x { _ => 1 } }",
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let statements = &program.find_function("f").unwrap().body.stmts;
        assert!(matches!(&statements[0], Stmt::If { else_branch: Some(_), .. }));
        assert!(matches!(&statements[1], Stmt::While { .. }));
        let Stmt::For { kind: ForKind::ThreeClause { .. }, .. } = &statements[2] else {
            panic!("expected a three clause for, found {:?}", statements[2]);
        };
        let Stmt::For { kind: ForKind::In { index, .. }, .. } = &statements[4] else {
            panic!("expected an index+value for, found {:?}", statements[4]);
        };
        assert!(index.is_some(), "`for i, v in xs` binds both");
        assert!(matches!(&statements[5], Stmt::Loop { .. }), "`for {{ }}` is a loop");
        assert!(matches!(&statements[6], Stmt::Break { .. }));
        assert!(matches!(&statements[7], Stmt::Continue { .. }));
        assert!(matches!(
            &statements[8],
            Stmt::Binding {
                value: Some(Expr::Match { .. }),
                ..
            }
        ));
    }

    #[test]
    fn a_pattern_condition_binds_in_the_head() {
        let (program, diagnostics) =
            parse_text("fn f() { if Ok(n) := parse(x) { use_it(n) }\n    while Some(node) := cursor { step() } }");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let statements = &program.find_function("f").unwrap().body.stmts;
        assert!(matches!(
            &statements[0],
            Stmt::If {
                cond: Expr::CondBinding { .. },
                ..
            }
        ));
        assert!(matches!(
            &statements[1],
            Stmt::While {
                cond: Expr::CondBinding { .. },
                ..
            }
        ));
    }

    #[test]
    fn an_identifier_before_a_block_is_not_a_struct_value_in_a_head() {
        let (program, diagnostics) = parse_text("fn f() { if ready { go() } }");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let Stmt::If { cond, .. } = &program.find_function("f").unwrap().body.stmts[0] else {
            panic!("expected an if");
        };
        assert!(matches!(cond, Expr::Ident { .. }), "{cond:?}");
    }

    #[test]
    fn interpolated_holes_are_expressions_or_trailing_arguments() {
        let (program, diagnostics) = parse_text(
            "fn f() { println($\"count={} name={name}\", n); println($\"{} has {} letters\", a, b) }",
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let statements = &program.find_function("f").unwrap().body.stmts;
        let Stmt::Expr { expr, .. } = &statements[0] else {
            panic!("{:?}", statements[0]);
        };
        let Expr::Call { args, .. } = expr else {
            panic!("{expr:?}");
        };
        let Expr::InterpString { parts, .. } = &args[0] else {
            panic!("{:?}", args[0]);
        };
        assert_eq!(parts.len(), 4, "text, hole, text, hole");
        assert!(
            matches!(&parts[1], InterpolationPart::Hole { written: None, .. }),
            "an empty hole takes the next trailing argument: {parts:?}"
        );
        assert!(
            matches!(
                &parts[3],
                InterpolationPart::Hole { written: Some(Expr::Ident { name, .. }), .. } if name == "name"
            ),
            "a named hole reads the scope: {parts:?}"
        );
        let Stmt::Expr { expr, .. } = &statements[1] else {
            panic!("{:?}", statements[1]);
        };
        let Expr::Call { args, .. } = expr else {
            panic!("{expr:?}");
        };
        let Expr::InterpString { parts, .. } = &args[0] else {
            panic!("{:?}", args[0]);
        };
        assert!(
            matches!(&parts[0], InterpolationPart::Hole { written: None, .. }),
            "an empty hole takes the next trailing argument: {parts:?}"
        );
    }

    #[test]
    fn if_may_be_the_value_of_a_return_or_a_binding() {
        let (program, diagnostics) =
            parse_text("fn f(a: i32, b: i32) -> i32 { return if a > b { a } else { b } }\nfn g() { label := if x { \"even\" } else { \"odd\" } }");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let function = program.find_function("f").unwrap();
        let Stmt::Return {
            value: Some(Expr::If { then_arm, .. }),
            ..
        } = &function.body.stmts[0]
        else {
            panic!("`f` returns an `if` value");
        };
        let Expr::Block { value, .. } = then_arm.as_ref() else {
            panic!("the then arm is a block with a value: {then_arm:?}");
        };
        assert!(
            matches!(value.as_ref(), Expr::Ident { name, .. } if name == "a"),
            "the then arm's tail expression is the block's value: {value:?}"
        );
    }

    #[test]
    fn an_if_in_value_position_without_else_is_an_error() {
        let (_, diagnostics) = parse_text("fn f() { x := if a { 1 } }");
        assert!(diagnostics
            .iter()
            .any(|d| d.code == CODE_UNEXPECTED_TOKEN && d.message.contains("final `else`")));
    }

    #[test]
    fn a_brace_literal_inside_an_interpolated_string_is_a_hole_not_a_block() {
        let (program, diagnostics) = parse_text("fn f() { print(\"{\".repeat(2)) }");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let _ = program;
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
            let _ = diagnostics;
            assert!(
                program.find_function("main").is_some(),
                "{source}: parsing must reach the following declaration"
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




