//! A deterministic text rendering of a parsed program, used by `--emit=ast`.
//!
//! Spans are deliberately absent so the dump is diffable across runs and
//! across files that differ only in whitespace.

use crate::*;

pub fn dump(program: &Program) -> String {
    let mut out = String::new();
    if let Some(package) = &program.package {
        out.push_str(&format!("package {}\n", package.name));
    }
    for import in &program.imports {
        out.push_str(&format!("import \"{}\"\n", import.path));
    }
    if !program.imports.is_empty() {
        out.push('\n');
    }
    for decl in &program.decls {
        out.push_str(&dump_decl(decl, 0));
    }
    out
}

fn indent(level: usize) -> String {
    "  ".repeat(level)
}

fn dump_attributes(attributes: &[Attribute]) -> String {
    attributes
        .iter()
        .map(|attribute| {
            let arguments = attribute
                .arguments
                .iter()
                .map(|argument| match argument {
                    AttributeArgument::Literal(text) => format!("\"{text}\""),
                    AttributeArgument::Named(name, value) => format!("{name} = \"{value}\""),
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("#[{}({arguments})]", attribute.name)
        })
        .collect::<Vec<_>>()
        .join(" ")
        + if attributes.is_empty() { "" } else { " " }
}

fn dump_decl(decl: &Decl, level: usize) -> String {
    let pad = indent(level);
    match decl {
        Decl::Fn(function) => dump_fn(function, level),
        Decl::Struct(decl) => {
            let params = dump_type_params(&decl.type_params);
            let mut out = format!("{pad}{}struct {}{params} {{\n", dump_attributes(&decl.attributes), decl.name.name);
            for field in &decl.fields {
                out.push_str(&format!("{}{}: {}\n", indent(level + 1), field.name.name, dump_type(&field.ty)));
            }
            out.push_str(&format!("{pad}}}\n"));
            out
        }
        Decl::Enum(decl) => {
            let params = dump_type_params(&decl.type_params);
            let mut out = format!("{pad}{}enum {}{params} {{\n", dump_attributes(&decl.attributes), decl.name.name);
            for variant in &decl.variants {
                let body = match &variant.body {
                    VariantBody::Empty => String::new(),
                    VariantBody::Tuple(types) => format!(
                        "({})",
                        types.iter().map(dump_type).collect::<Vec<_>>().join(", ")
                    ),
                    VariantBody::Named(fields) => format!(
                        " {{ {} }}",
                        fields
                            .iter()
                            .map(|f| format!("{}: {}", f.name.name, dump_type(&f.ty)))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                };
                out.push_str(&format!(
                    "{}{}{body}\n",
                    indent(level + 1),
                    variant.name.name
                ));
            }
            out.push_str(&format!("{pad}}}\n"));
            out
        }
        Decl::Const(decl) => format!(
            "{pad}{}const {}{} = {}\n",
            dump_attributes(&decl.attributes),
            decl.name.name,
            decl.ty.as_ref().map(|t| format!(": {}", dump_type(t))).unwrap_or_default(),
            dump_expr(&decl.value)
        ),
        Decl::Global(decl) => format!(
            "{pad}{}global {}{}{}\n",
            dump_attributes(&decl.attributes),
            decl.name.name,
            decl.ty.as_ref().map(|t| format!(": {}", dump_type(t))).unwrap_or_default(),
            decl.value.as_ref().map(|v| format!(" = {}", dump_expr(v))).unwrap_or_default()
        ),
        Decl::TypeAlias(decl) => format!(
            "{pad}type {}{} = {}\n",
            decl.name.name,
            dump_type_params(&decl.type_params),
            dump_type(&decl.aliased)
        ),
        Decl::Impl(decl) => {
            let mut out = format!(
                "{pad}impl {}{} {{\n",
                decl.type_name.name,
                dump_type_params(&decl.type_params)
            );
            for method in &decl.methods {
                out.push_str(&dump_fn(method, level + 1));
            }
            out.push_str(&format!("{pad}}}\n"));
            out
        }
        Decl::Module(decl) => {
            let head = dump_attributes(&decl.attributes);
            if decl.decls.is_empty() {
                return format!("{pad}{head}mod {};\n", decl.name.name);
            }
            let mut out = format!("{pad}{head}mod {} {{\n", decl.name.name);
            for inner in &decl.decls {
                out.push_str(&dump_decl(inner, level + 1));
            }
            out.push_str(&format!("{pad}}}\n"));
            out
        }
        Decl::Extern(block) => {
            let mut out = format!("{pad}extern \"{}\" {{\n", block.abi);
            for function in &block.functions {
                let params = function
                    .params
                    .iter()
                    .map(|p| format!("{}: {}", p.name.name, dump_type(&p.ty)))
                    .collect::<Vec<_>>()
                    .join(", ");
                out.push_str(&format!(
                    "{}{}fn {}({}){};\n",
                    indent(level + 1),
                    dump_attributes(&function.attributes),
                    function.name.name,
                    params,
                    function.return_ty.as_ref().map(|t| format!(" -> {}", dump_type(t))).unwrap_or_default()
                ));
            }
            out.push_str(&format!("{pad}}}\n"));
            out
        }
        Decl::Error { .. } => format!("{pad}<error>\n"),
    }
}

fn dump_fn(function: &FnDecl, level: usize) -> String {
    let pad = indent(level);
    let params = function
        .params
        .iter()
        .map(|p| {
            if p.name.name == "self" && matches!(&p.ty, Type::Named { name, .. } if name == "Self") {
                "self".to_string()
            } else {
                format!("{}: {}", p.name.name, dump_type(&p.ty))
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "{pad}{}fn {}{}({}){} {{\n{}{pad}}}\n",
        dump_attributes(&function.attributes),
        function.name.name,
        dump_type_params(&function.type_params),
        params,
        function.return_ty.as_ref().map(|t| format!(" -> {}", dump_type(t))).unwrap_or_default(),
        dump_block(&function.body, level + 1)
    )
}

fn dump_type_params(params: &[TypeParam]) -> String {
    if params.is_empty() {
        return String::new();
    }
    let inner = params
        .iter()
        .map(|p| {
            let keyword = if p.is_const { "const " } else { "" };
            match &p.ty {
                Some(ty) => format!("{keyword}{}: {}", p.name.name, dump_type(ty)),
                None => format!("{keyword}{}", p.name.name),
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("<{inner}>")
}

fn dump_type_arguments(args: &[Type]) -> String {
    if args.is_empty() {
        return String::new();
    }
    format!(
        "<{}>",
        args.iter().map(dump_type).collect::<Vec<_>>().join(", ")
    )
}

fn dump_type(ty: &Type) -> String {
    match ty {
        Type::Named { name, args, .. } => {
            if args.is_empty() {
                name.clone()
            } else {
                format!("{name}<{}>", args.iter().map(dump_type).collect::<Vec<_>>().join(", "))
            }
        }
        Type::Pointer { pointee, .. } => format!("*{}", dump_type(pointee)),
        Type::Slice { element, .. } => format!("[{}]", dump_type(element)),
        Type::Array { element, len, .. } => format!("[{}; {}]", dump_type(element), dump_expr(len)),
        Type::Tuple { elements, .. } => format!(
            "({})",
            elements.iter().map(dump_type).collect::<Vec<_>>().join(", ")
        ),
        Type::Function { params, return_ty, .. } => format!(
            "fn({}){}",
            params.iter().map(dump_type).collect::<Vec<_>>().join(", "),
            return_ty.as_ref().map(|t| format!(" -> {}", dump_type(t))).unwrap_or_default()
        ),
        Type::ConstArgument { value, .. } => dump_expr(value),
        Type::Error { .. } => "<error>".to_string(),
    }
}

fn dump_block(block: &Block, level: usize) -> String {
    let mut out = String::new();
    for stmt in &block.stmts {
        out.push_str(&dump_stmt(stmt, level));
    }
    out
}

fn dump_stmt(stmt: &Stmt, level: usize) -> String {
    let pad = indent(level);
    match stmt {
        Stmt::Binding { name, ty, value, .. } => {
            let suffix = match (ty, value) {
                (Some(ty), Some(value)) => format!(": {} = {}", dump_type(ty), dump_expr(value)),
                (Some(ty), None) => format!(": {}", dump_type(ty)),
                (None, Some(value)) => format!(" := {}", dump_expr(value)),
                (None, None) => String::new(),
            };
            format!("{pad}{}{suffix}\n", name.name)
        }
        Stmt::Assign { target, op, value, .. } => {
            format!("{pad}{} {} {}\n", dump_expr(target), op.symbol(), dump_expr(value))
        }
        Stmt::Expr { expr, .. } => format!("{pad}{}\n", dump_expr(expr)),
        Stmt::Return { value, .. } => format!(
            "{pad}return{}\n",
            value.as_ref().map(|v| format!(" {}", dump_expr(v))).unwrap_or_default()
        ),
        Stmt::Break { .. } => format!("{pad}break\n"),
        Stmt::Continue { .. } => format!("{pad}continue\n"),
        Stmt::Defer { stmt, .. } => {
            format!("{pad}defer {}
", dump_stmt(stmt, 0).trim_end())
        }
        Stmt::Block { block, .. } => format!("{pad}{{\n{}{pad}}}\n", dump_block(block, level + 1)),
        Stmt::If { cond, then_branch, else_branch, .. } => {
            let mut out = format!("{pad}if {} {{\n{}{pad}}}", dump_cond(cond), dump_block(then_branch, level + 1), pad = pad);
            let mut current = else_branch.as_deref();
            while let Some(Stmt::If { cond, then_branch, else_branch, .. }) = current {
                out.push_str(&format!(
                    " else if {} {{\n{}{pad}}}",
                    dump_cond(cond),
                    dump_block(then_branch, level + 1),
                    pad = pad
                ));
                current = else_branch.as_deref();
            }
            if let Some(Stmt::Block { block, .. }) = current {
                out.push_str(&format!(" else {{\n{}{pad}}}", dump_block(block, level + 1), pad = pad));
            } else if current.is_some() {
                out.push_str(" else <error>");
            }
            out.push('\n');
            out
        }
        Stmt::While { cond, body, .. } => format!(
            "{pad}while {} {{\n{}{pad}}}\n",
            dump_cond(cond),
            dump_block(body, level + 1)
        ),
        Stmt::Loop { body, .. } => format!("{pad}loop {{\n{}{pad}}}\n", dump_block(body, level + 1)),
        Stmt::For { kind, body, span: _ } => match kind {
            ForKind::ThreeClause { init, cond, step } => format!(
                "{pad}for {}, {}, {} {{\n{}{pad}}}\n",
                dump_stmt(init, 0).trim_end(),
                dump_expr(cond),
                dump_stmt(step, 0).trim_end(),
                dump_block(body, level + 1)
            ),
            ForKind::In { index, value, iterable } => format!(
                "{pad}for {}{} in {} {{\n{}{pad}}}\n",
                index.as_ref().map(|i| format!("{}, ", i.name)).unwrap_or_default(),
                value.name,
                dump_expr(iterable),
                dump_block(body, level + 1)
            ),
        },
        Stmt::Match { subject, arms, .. } => format!(
            "{pad}{}\n",
            dump_match(subject, arms)
        ),
        Stmt::Error { .. } => format!("{pad}<error>\n"),
    }
}

fn dump_match(subject: &Expr, arms: &[MatchArm]) -> String {
    let arms = arms
        .iter()
        .map(|arm| {
            let guard = arm
                .guard
                .as_ref()
                .map(|guard| format!(" if {}", dump_expr(guard)))
                .unwrap_or_default();
            let body = match &arm.body {
                MatchBody::Expr(expr) => dump_expr(expr),
                MatchBody::Block(block) => format!("{{ {} }}", dump_block(block, 0).trim()),
                MatchBody::Stmt(stmt) => dump_stmt(stmt, 0).trim().to_string(),
            };
            format!("{}{guard} => {body}", dump_pattern(&arm.pattern))
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("match {} {{ {arms} }}", dump_expr(subject))
}

fn dump_pattern(pattern: &Pattern) -> String {
    match pattern {
        Pattern::Wildcard { .. } => "_".to_string(),
        Pattern::Else { .. } => "else".to_string(),
        Pattern::Name { name, .. } => name.name.clone(),
        Pattern::Literal { value, .. } => dump_expr(value),
        Pattern::Path { qualifier, name, .. } => format!("{qualifier}::{name}"),
        Pattern::Tuple { qualifier, name, elements, .. } => format!(
            "{}{name}({})",
            qualifier.as_ref().map(|q| format!("{q}::")).unwrap_or_default(),
            elements.iter().map(dump_pattern).collect::<Vec<_>>().join(", ")
        ),
        Pattern::Struct { qualifier, name, fields, rest, .. } => {
            let mut written = fields
                .iter()
                .map(|field| match field {
                    FieldPattern::Shorthand { name, .. } => name.name.clone(),
                    FieldPattern::Nested { name, pattern, .. } => {
                        format!("{}: {}", name.name, dump_pattern(pattern))
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
            dump_pattern(start),
            if *inclusive { "..=" } else { ".." },
            dump_pattern(end)
        ),
        Pattern::Or { alternatives, .. } => alternatives
            .iter()
            .map(dump_pattern)
            .collect::<Vec<_>>()
            .join(" | "),
        Pattern::Error { .. } => "<error>".to_string(),
    }
}

fn dump_cond(cond: &Expr) -> String {
    match cond {
        Expr::CondBinding { pattern, value, .. } => {
            format!("{} := {}", dump_pattern(pattern), dump_expr(value))
        }
        other => dump_expr(other),
    }
}

fn dump_expr(expr: &Expr) -> String {
    match expr {
        Expr::Int { value, .. } => value.to_string(),
        Expr::Float { value, .. } => format!("{value}"),
        Expr::Str { text, kind, .. } => match kind {
            StrKind::Raw => format!("r\"{text}\""),
            StrKind::CString => format!("c\"{text}\""),
            _ => format!("\"{text}\""),
        },
        Expr::Char { value, .. } => format!("'{value}'"),
        Expr::Bool { value, .. } => value.to_string(),
        Expr::Ident { name, .. } => name.clone(),
        Expr::Call { callee, args, .. } => format!(
            "{}({})",
            dump_expr(callee),
            args.iter().map(dump_expr).collect::<Vec<_>>().join(", ")
        ),
        Expr::Field { base, name, .. } => format!("{}.{}", dump_expr(base), name.name),
        Expr::Index { base, index, .. } => format!("{}[{}]", dump_expr(base), dump_expr(index)),
        Expr::Range { start, end, inclusive, .. } => format!(
            "{}{}{}",
            start.as_ref().map(|e| dump_expr(e)).unwrap_or_default(),
            if *inclusive { "..=" } else { ".." },
            end.as_ref().map(|e| dump_expr(e)).unwrap_or_default()
        ),
        Expr::Member { base, index, .. } => format!("{}.{}", dump_expr(base), index),
        Expr::ArrayLiteral { elements, .. } => format!(
            "[{}]",
            elements.iter().map(dump_expr).collect::<Vec<_>>().join(", ")
        ),
        Expr::RepeatedArray { value, len, .. } => {
            format!("[{}; {}]", dump_expr(value), dump_expr(len))
        }
        Expr::Path {
            qualifier,
            name,
            args,
            ..
        } => {
            let spelled = if qualifier.is_empty() {
                name.clone()
            } else {
                format!("{qualifier}::{name}")
            };
            format!("{spelled}{}", dump_type_arguments(args))
        }
        Expr::StructLiteral {
            name, args, fields, ..
        } => {
            let head = format!("{name}{}", dump_type_arguments(args));
            let body = fields
                .iter()
                .map(|(f, v)| format!("{}: {}", f.name, dump_expr(v)))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{head} {{ {body} }}")
        }
        Expr::CondBinding { pattern, value, .. } => {
            format!("({} := {})", dump_pattern(pattern), dump_expr(value))
        }
        Expr::Match { subject, arms, .. } => dump_match(subject, arms),
        Expr::InterpString { parts, .. } => {
            let body = parts
                .iter()
                .map(|part| match part {
                    InterpolationPart::Text(text) => text.clone(),
                    InterpolationPart::Hole { written: Some(expr), .. } => {
                        format!("${{{}}}", dump_expr(expr))
                    }
                    InterpolationPart::Hole { written: None, .. } => "{}".to_string(),
                })
                .collect::<String>();
            format!("$\"{body}\"")
        }
        Expr::If { cond, then_arm, else_arm, .. } => format!(
            "if {} {{ {} }}{}",
            dump_expr(cond),
            dump_expr(then_arm),
            else_arm
                .as_ref()
                .map(|e| format!(" else {{ {} }}", dump_expr(e)))
                .unwrap_or_default()
        ),
        Expr::Block { value, .. } => format!("{{ {} }}", dump_expr(value)),
        Expr::Unary { op, operand, .. } => format!("{}{}", op.symbol(), dump_expr(operand)),
        Expr::Binary { op, lhs, rhs, .. } => {
            format!("({} {} {})", dump_expr(lhs), op.symbol(), dump_expr(rhs))
        }
        Expr::Cast { value, target, .. } => format!("({} as {})", dump_expr(value), dump_type(target)),
        Expr::Paren { inner, .. } => dump_expr(inner),
        Expr::Error { .. } => "<error>".to_string(),
    }
}

