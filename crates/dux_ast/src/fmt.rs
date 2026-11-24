//! The canonical formatter: an AST printer whose output re-parses to the same
//! program. Layout is normalised (one statement per line, no semicolons, four
//! space indentation, one blank line between top level declarations) and the
//! parenthesisation is derived from the tree, never from the original text.
//!
//! Comments are not part of the AST, so formatting drops them; a file worth
//! its comments should not be formatted until the lexer attaches them.

use crate::*;

pub fn format(program: &Program) -> String {
    let printer = Printer;
    let mut out = String::new();
    if let Some(package) = &program.package {
        out.push_str(&format!("package {}\n", package.name));
    }
    for import in &program.imports {
        out.push_str(&format!("import \"{}\"\n", import.path));
    }
    for decl in &program.decls {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&printer.decl(decl, 0));
        out.push('\n');
    }
    out
}

struct Printer;

fn pad(level: usize) -> String {
    "    ".repeat(level)
}

fn quote(text: &str) -> String {
    let mut out = String::from("\"");
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\0' => out.push_str("\\0"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

fn char_literal(value: char) -> String {
    let body = match value {
        '\'' => "\\'",
        '\\' => "\\\\",
        '\n' => "\\n",
        '\t' => "\\t",
        '\r' => "\\r",
        '\0' => "\\0",
        other => return format!("'{other}'"),
    };
    format!("'{body}'")
}

fn float_literal(value: f64) -> String {
    let text = format!("{value}");
    if text.contains('.') || text.contains('e') || text.contains("inf") || text.contains("NaN") {
        text
    } else {
        format!("{text}.0")
    }
}

fn interpolation(parts: &[InterpolationPart]) -> String {
    let mut out = String::from("$\"");
    for part in parts {
        match part {
            InterpolationPart::Text(text) => {
                for ch in text.chars() {
                    match ch {
                        '{' | '}' => {
                            out.push(ch);
                            out.push(ch);
                        }
                        other => out.push(other),
                    }
                }
            }
            InterpolationPart::Hole { written, .. } => match written {
                Some(expr) => out.push_str(&format!("{{{}}}", Printer.expr(expr, 0))),
                None => out.push_str("{}"),
            },
        }
    }
    out.push('"');
    out
}

fn type_arguments(args: &[Type]) -> String {
    if args.is_empty() {
        return String::new();
    }
    format!(
        "<{}>",
        args.iter().map(|arg| Printer.ty(arg)).collect::<Vec<_>>().join(", ")
    )
}

impl Printer {
    fn decl(&self, decl: &Decl, level: usize) -> String {
        let p = pad(level);
        match decl {
            Decl::Fn(function) => self.function(function, level, false),
            Decl::Struct(decl) => {
                let head = format!(
                    "{}struct {}{}",
                    self.attributes_inline(&decl.attributes),
                    decl.name.name,
                    self.type_params(&decl.type_params)
                );
                if decl.fields.is_empty() {
                    return format!("{head} {{ }}");
                }
                let body = decl
                    .fields
                    .iter()
                    .map(|field| {
                        format!("{}{}: {},\n", pad(level + 1), field.name.name, self.ty(&field.ty))
                    })
                    .collect::<String>();
                format!("{head} {{\n{body}{p}}}")
            }
            Decl::Enum(decl) => {
                let head = format!(
                    "{}enum {}{}",
                    self.attributes_inline(&decl.attributes),
                    decl.name.name,
                    self.type_params(&decl.type_params)
                );
                if decl.variants.is_empty() {
                    return format!("{head} {{ }}");
                }
                let body = decl
                    .variants
                    .iter()
                    .map(|variant| {
                        let body = match &variant.body {
                            VariantBody::Empty => String::new(),
                            VariantBody::Tuple(types) => format!(
                                "({})",
                                types.iter().map(|ty| self.ty(ty)).collect::<Vec<_>>().join(", ")
                            ),
                            VariantBody::Named(fields) => format!(
                                " {{ {} }}",
                                fields
                                    .iter()
                                    .map(|f| format!("{}: {}", f.name.name, self.ty(&f.ty)))
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ),
                        };
                        format!("{}{}{body}\n", pad(level + 1), variant.name.name)
                    })
                    .collect::<String>();
                format!("{head} {{\n{body}{p}}}")
            }
            Decl::Const(decl) => format!(
                "{}const {}{} = {}",
                self.attributes_inline(&decl.attributes),
                decl.name.name,
                self.ty_annotation(decl.ty.as_ref()),
                self.expr(&decl.value, level)
            ),
            Decl::Global(decl) => format!(
                "{}global {}{}{}",
                self.attributes_inline(&decl.attributes),
                decl.name.name,
                self.ty_annotation(decl.ty.as_ref()),
                decl
                    .value
                    .as_ref()
                    .map(|value| format!(" = {}", self.expr(value, level)))
                    .unwrap_or_default()
            ),
            Decl::TypeAlias(decl) => format!(
                "type {}{} = {}",
                decl.name.name,
                self.type_params(&decl.type_params),
                self.ty(&decl.aliased)
            ),
            Decl::Impl(decl) => {
                if decl.methods.is_empty() {
                    return format!("impl {} {{ }}", decl.type_name.name);
                }
                let body = decl
                    .methods
                    .iter()
                    .map(|method| self.function(method, level + 1, false))
                    .collect::<Vec<_>>()
                    .join("\n\n");
                format!("impl {} {{\n{body}\n{p}}}", decl.type_name.name)
            }
            Decl::Module(decl) => format!(
                "{}mod {};",
                self.attributes_inline(&decl.attributes),
                decl.name.name
            ),
            Decl::Extern(block) => {
                let attributes = self.attributes_standalone(&block.attributes, level);
                if block.functions.is_empty() {
                    return format!("{p}{attributes}extern \"{}\" {{ }}", block.abi);
                }
                let body = block
                    .functions
                    .iter()
                    .map(|function| self.function(function, level + 1, true))
                    .collect::<Vec<_>>()
                    .join("\n");
                format!("{p}{attributes}extern \"{}\" {{\n{body}\n{p}}}", block.abi)
            }
            Decl::Error { .. } => format!("{p}<error>"),
        }
    }

    fn function(&self, function: &FnDecl, level: usize, signature_only: bool) -> String {
        let p = pad(level);
        let attributes = self.attributes_standalone(&function.attributes, level);
        let params = function
            .params
            .iter()
            .map(|param| self.param(param))
            .collect::<Vec<_>>()
            .join(", ");
        let head = format!(
            "{attributes}{p}fn {}{}({}){}",
            function.name.name,
            self.type_params(&function.type_params),
            params,
            function
                .return_ty
                .as_ref()
                .map(|ty| format!(" -> {}", self.ty(ty)))
                .unwrap_or_default()
        );
        if signature_only && function.body.stmts.is_empty() {
            return format!("{head};");
        }
        if function.body.stmts.is_empty() {
            return format!("{head} {{\n{p}}}");
        }
        format!("{head} {{\n{}{p}}}", self.block(&function.body, level + 1))
    }

    fn param(&self, param: &Param) -> String {
        if param.name.name == "self"
            && matches!(&param.ty, Type::Named { name, args, .. } if name == "Self" && args.is_empty())
        {
            return "self".to_string();
        }
        format!("{}: {}", param.name.name, self.ty(&param.ty))
    }

    fn ty_annotation(&self, ty: Option<&Type>) -> String {
        ty.map(|ty| format!(": {}", self.ty(ty)))
            .unwrap_or_default()
    }

    fn type_params(&self, params: &[TypeParam]) -> String {
        if params.is_empty() {
            return String::new();
        }
        let inner = params
            .iter()
            .map(|param| {
                let keyword = if param.is_const { "const " } else { "" };
                match &param.ty {
                    Some(ty) => format!("{keyword}{}: {}", param.name.name, self.ty(ty)),
                    None => format!("{keyword}{}", param.name.name),
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!("<{inner}>")
    }

    fn attribute(&self, attribute: &Attribute) -> String {
        if attribute.arguments.is_empty() {
            return format!("#[{}]", attribute.name);
        }
        let arguments = attribute
            .arguments
            .iter()
            .map(|argument| match argument {
                AttributeArgument::Literal(text) => {
                    if text.parse::<i128>().is_ok() {
                        text.clone()
                    } else {
                        quote(text)
                    }
                }
                AttributeArgument::Named(name, value) => {
                    if value.parse::<i128>().is_ok() {
                        format!("{name} = {value}")
                    } else {
                        format!("{name} = {}", quote(value))
                    }
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!("#[{}({arguments})]", attribute.name)
    }

    fn attributes_standalone(&self, attributes: &[Attribute], level: usize) -> String {
        attributes
            .iter()
            .map(|attribute| format!("{}{}\n", pad(level), self.attribute(attribute)))
            .collect()
    }

    fn attributes_inline(&self, attributes: &[Attribute]) -> String {
        if attributes.is_empty() {
            return String::new();
        }
        format!(
            "{} ",
            attributes
                .iter()
                .map(|attribute| self.attribute(attribute))
                .collect::<Vec<_>>()
                .join(" ")
        )
    }

    fn ty(&self, ty: &Type) -> String {
        match ty {
            Type::Named { name, args, .. } => format!("{name}{}", type_arguments(args)),
            Type::Pointer { pointee, .. } => format!("*{}", self.ty(pointee)),
            Type::Slice { element, .. } => format!("[]{}", self.ty(element)),
            Type::Array { element, len, .. } => {
                format!("[{}; {}]", self.ty(element), self.expr(len, 0))
            }
            Type::Tuple { elements, .. } => format!(
                "({})",
                elements.iter().map(|ty| self.ty(ty)).collect::<Vec<_>>().join(", ")
            ),
            Type::Function { params, return_ty, .. } => format!(
                "fn({}){}",
                params.iter().map(|ty| self.ty(ty)).collect::<Vec<_>>().join(", "),
                return_ty
                    .as_ref()
                    .map(|ty| format!(" -> {}", self.ty(ty)))
                    .unwrap_or_default()
            ),
            Type::ConstArgument { value, .. } => self.expr(value, 0),
            Type::Error { .. } => "<error>".to_string(),
        }
    }

    fn block(&self, block: &Block, level: usize) -> String {
        block
            .stmts
            .iter()
            .map(|stmt| format!("{}\n", self.stmt(stmt, level)))
            .collect()
    }

    fn stmt(&self, stmt: &Stmt, level: usize) -> String {
        let p = pad(level);
        match stmt {
            Stmt::Binding { name, ty, value, .. } => {
                let suffix = match (ty, value) {
                    (Some(ty), Some(value)) => {
                        format!(": {} = {}", self.ty(ty), self.expr(value, level))
                    }
                    (Some(ty), None) => format!(": {}", self.ty(ty)),
                    (None, Some(value)) => format!(" := {}", self.expr(value, level)),
                    (None, None) => String::new(),
                };
                format!("{p}{}{suffix}", name.name)
            }
            Stmt::Assign { target, op, value, .. } => {
                format!("{p}{} {} {}", self.expr(target, level), op.symbol(), self.expr(value, level))
            }
            Stmt::Expr { expr, .. } => format!("{p}{}", self.expr(expr, level)),
            Stmt::Return { value, .. } => format!(
                "{p}return{}",
                value
                    .as_ref()
                    .map(|value| format!(" {}", self.expr(value, level)))
                    .unwrap_or_default()
            ),
            Stmt::Break { .. } => format!("{p}break"),
            Stmt::Continue { .. } => format!("{p}continue"),
            Stmt::Defer { stmt, .. } => format!("{p}defer {}", self.stmt(stmt, level).trim_start()),
            Stmt::Block { block, .. } => format!("{p}{{\n{}{p}}}", self.block(block, level + 1)),
            Stmt::If { cond, then_branch, else_branch, .. } => {
                let mut out = format!(
                    "{p}if {} {{\n{}",
                    self.condition(cond, level),
                    self.block(then_branch, level + 1)
                );
                out.push_str(&format!("{p}}}"));
                let mut current = else_branch.as_deref();
                while let Some(Stmt::If { cond, then_branch, else_branch, .. }) = current {
                    out.push_str(&format!(
                        " else if {} {{\n{}{p}}}",
                        self.condition(cond, level),
                        self.block(then_branch, level + 1)
                    ));
                    current = else_branch.as_deref();
                }
                if let Some(other) = current {
                    match other {
                        Stmt::Block { block, .. } => {
                            out.push_str(&format!(" else {{\n{}{p}}}", self.block(block, level + 1)));
                        }
                        other => {
                            out.push_str(&format!(" else {}", self.stmt(other, level).trim_start()));
                        }
                    }
                }
                out
            }
            Stmt::While { cond, body, .. } => format!(
                "{p}while {} {{\n{}{p}}}",
                self.condition(cond, level),
                self.block(body, level + 1)
            ),
            Stmt::Loop { body, .. } => {
                format!("{p}loop {{\n{}{p}}}", self.block(body, level + 1))
            }
            Stmt::For { kind, body, .. } => {
                let head = match kind {
                    ForKind::ThreeClause { init, cond, step } => format!(
                        "{p}for {}, {}, {} {{",
                        self.stmt(init, level).trim_start(),
                        self.expr(cond, level),
                        self.stmt(step, level).trim_start()
                    ),
                    ForKind::In { index, value, iterable } => format!(
                        "{p}for {}{} in {} {{",
                        index
                            .as_ref()
                            .map(|index| format!("{}, ", index.name))
                            .unwrap_or_default(),
                        value.name,
                        self.expr(iterable, level)
                    ),
                };
                format!("{head}\n{}{p}}}", self.block(body, level + 1))
            }
            Stmt::Match { subject, arms, .. } => {
                format!("{p}{}", self.match_expression(subject, arms, level))
            }
            Stmt::Error { .. } => format!("{p}<error>"),
        }
    }

    fn match_expression(&self, subject: &Expr, arms: &[MatchArm], level: usize) -> String {
        let body = arms
            .iter()
            .map(|arm| format!("{}{},\n", pad(level + 1), self.arm(arm, level + 1)))
            .collect::<String>();
        format!(
            "match {} {{\n{body}{}}}",
            self.expr(subject, level),
            pad(level)
        )
    }

    fn arm(&self, arm: &MatchArm, level: usize) -> String {
        let guard = arm
            .guard
            .as_ref()
            .map(|guard| format!(" if {}", self.expr(guard, level)))
            .unwrap_or_default();
        let body = match &arm.body {
            MatchBody::Expr(expr) => self.expr(expr, level),
            MatchBody::Block(block) => {
                format!("{{\n{}{}}}", self.block(block, level + 1), pad(level))
            }
            MatchBody::Stmt(stmt) => self.stmt(stmt, level).trim_start().to_string(),
        };
        format!("{}{guard} => {body}", self.pattern(&arm.pattern))
    }

    fn pattern(&self, pattern: &Pattern) -> String {
        match pattern {
            Pattern::Wildcard { .. } => "_".to_string(),
            Pattern::Else { .. } => "else".to_string(),
            Pattern::Name { name, .. } => name.name.clone(),
            Pattern::Literal { value, .. } => self.expr(value, 0),
            Pattern::Path { qualifier, name, .. } => format!("{qualifier}::{name}"),
            Pattern::Tuple { qualifier, name, elements, .. } => format!(
                "{}{name}({})",
                qualifier.as_ref().map(|q| format!("{q}::")).unwrap_or_default(),
                elements
                    .iter()
                    .map(|element| self.pattern(element))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Pattern::Struct { qualifier, name, fields, rest, .. } => {
                let mut written = fields
                    .iter()
                    .map(|field| match field {
                        FieldPattern::Shorthand { name, .. } => name.name.clone(),
                        FieldPattern::Nested { name, pattern, .. } => {
                            format!("{}: {}", name.name, self.pattern(pattern))
                        }
                    })
                    .collect::<Vec<_>>();
                if *rest {
                    written.push("..".to_string());
                }
                format!(
                    "{}{name} {{ {} }}",
                    qualifier.as_ref().map(|q| format!("{q}::")).unwrap_or_default(),
                    written.join(", ")
                )
            }
            Pattern::Range { start, end, inclusive, .. } => format!(
                "{}{}{}",
                self.pattern(start),
                if *inclusive { "..=" } else { ".." },
                self.pattern(end)
            ),
            Pattern::Or { alternatives, .. } => alternatives
                .iter()
                .map(|alternative| self.pattern(alternative))
                .collect::<Vec<_>>()
                .join(" | "),
            Pattern::Error { .. } => "<error>".to_string(),
        }
    }

    fn condition(&self, cond: &Expr, level: usize) -> String {
        match cond {
            Expr::CondBinding { pattern, value, .. } => {
                format!("{} := {}", self.pattern(pattern), self.expr(value, level))
            }
            other => self.expr(other, level),
        }
    }

    fn block_expression(&self, expr: &Expr, level: usize) -> String {
        match expr {
            Expr::Block { block, .. } => {
                if block.stmts.is_empty() {
                    return "{}".to_string();
                }
                format!(
                    "{{\n{}{}}}",
                    self.block(block, level + 1),
                    pad(level)
                )
            }
            other => format!("{{ {} }}", self.expr(other, level)),
        }
    }

    fn if_expression(&self, cond: &Expr, then_arm: &Expr, else_arm: Option<&Expr>, level: usize) -> String {
        let mut out = format!(
            "if {} {}",
            self.condition(cond, level),
            self.block_expression(then_arm, level)
        );
        let mut current = else_arm;
        while let Some(arm) = current {
            match arm {
                Expr::If {
                    cond,
                    then_arm,
                    else_arm,
                    ..
                } => {
                    out.push_str(&format!(
                        " else if {} {}",
                        self.condition(cond, level),
                        self.block_expression(then_arm, level)
                    ));
                    current = else_arm.as_deref();
                }
                other => {
                    out.push_str(&format!(" else {}", self.block_expression(other, level)));
                    current = None;
                }
            }
        }
        out
    }

    fn expr(&self, expr: &Expr, level: usize) -> String {
        match expr {
            Expr::Int { value, .. } => value.to_string(),
            Expr::Float { value, .. } => float_literal(*value),
            Expr::Str { text, kind, .. } => match kind {
                StrKind::Raw => format!("r\"{text}\""),
                StrKind::CString => format!("c{}", quote(text)),
                _ => quote(text),
            },
            Expr::Char { value, .. } => char_literal(*value),
            Expr::Bool { value, .. } => value.to_string(),
            Expr::Ident { name, .. } => name.clone(),
            Expr::Call { callee, args, .. } => format!(
                "{}({})",
                self.expr(callee, level),
                args.iter()
                    .map(|arg| self.expr(arg, level))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Expr::Field { base, name, .. } => {
                format!("{}.{}", self.expr(base, level), name.name)
            }
            Expr::Index { base, index, .. } => {
                format!("{}[{}]", self.expr(base, level), self.expr(index, level))
            }
            Expr::Range {
                start,
                end,
                inclusive,
                ..
            } => format!(
                "{}{}{}",
                start
                    .as_ref()
                    .map(|start| self.expr(start, level))
                    .unwrap_or_default(),
                if *inclusive { "..=" } else { ".." },
                end
                    .as_ref()
                    .map(|end| self.expr(end, level))
                    .unwrap_or_default()
            ),
            Expr::Member { base, index, .. } => {
                format!("{}.{}", self.expr(base, level), index)
            }
            Expr::ArrayLiteral { elements, .. } => format!(
                "[{}]",
                elements
                    .iter()
                    .map(|element| self.expr(element, level))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Expr::RepeatedArray { value, len, .. } => {
                format!("[{}; {}]", self.expr(value, level), self.expr(len, level))
            }
            Expr::Path { qualifier, name, .. } => format!("{qualifier}::{name}"),
            Expr::StructLiteral { name, fields, .. } => {
                if fields.is_empty() {
                    return format!("{name} {{ }}");
                }
                format!(
                    "{name} {{ {} }}",
                    fields
                        .iter()
                        .map(|(field, value)| format!("{}: {}", field.name, self.expr(value, level)))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
            Expr::CondBinding { pattern, value, .. } => {
                format!("{} := {}", self.pattern(pattern), self.expr(value, level))
            }
            Expr::Match { subject, arms, .. } => self.match_expression(subject, arms, level),
            Expr::InterpString { parts, .. } => interpolation(parts),
            Expr::If {
                cond,
                then_arm,
                else_arm,
                ..
            } => self.if_expression(cond, then_arm, else_arm.as_deref(), level),
            Expr::Block { .. } => self.block_expression(expr, level),
            Expr::Unary { op, operand, .. } => {
                format!("{}{}", op.symbol(), self.expr(operand, level))
            }
            Expr::Binary { op, lhs, rhs, .. } => format!(
                "{} {} {}",
                self.expr(lhs, level),
                op.symbol(),
                self.expr(rhs, level)
            ),
            Expr::Cast { value, target, .. } => {
                format!("{} as {}", self.expr(value, level), self.ty(target))
            }
            Expr::Paren { inner, .. } => format!("({})", self.expr(inner, level)),
            Expr::Error { .. } => "<error>".to_string(),
        }
    }
}
