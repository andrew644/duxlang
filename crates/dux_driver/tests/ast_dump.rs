use dux_source::SourceMap;

fn dump_source(source: &str) -> String {
    let mut sources = SourceMap::new();
    let file = sources.add("t.dux", source.to_string());
    let lexed = dux_lexer::lex(&sources, file);
    let parsed = dux_parser::parse(&sources, &lexed.tokens);
    assert!(
        parsed
            .diagnostics
            .iter()
            .all(|d| d.code == dux_diag::code::UNSUPPORTED_IN_BUILD),
        "fixture must parse: {:?}",
        parsed.diagnostics
    );
    dux_ast::dump::dump(&parsed.program)
}

#[test]
fn declarations_render_in_source_order_without_spans() {
    let text = dump_source(
        "package main\n\
         const KI: usize = 1024\n\
         global hits: i32 = 0\n\
         type Score = i64\n\
         struct Vec3 {\n    x: f64,\n    y: f64,\n}\n\
         enum Shape {\n    Empty,\n    Rect(f64, f64),\n    Named { w: i32 },\n}\n\
         impl Vec3 {\n    fn len(self) -> f64 { return self.x }\n}\n",
    );
    assert!(text.starts_with("package main\n"), "{text}");
    assert!(text.contains("const KI: usize = 1024\n"), "{text}");
    assert!(text.contains("global hits: i32 = 0\n"), "{text}");
    assert!(text.contains("type Score = i64\n"), "{text}");
    assert!(
        text.contains("struct Vec3 {\n  x: f64\n  y: f64\n}\n"),
        "{text}"
    );
    assert!(
        text.contains("enum Shape {\n  Empty\n  Rect(f64, f64)\n  Named { w: i32 }\n}\n"),
        "{text}"
    );
    assert!(
        text.contains("impl Vec3 {\n  fn len(self) -> f64 {\n    return self.x\n  }\n}\n"),
        "{text}"
    );
}

#[test]
fn expressions_render_with_full_parenthesisation() {
    let text = dump_source("package main\nfn main() { x := 1 + 2 * 3 }\n");
    assert!(text.contains("x := (1 + (2 * 3))"), "{text}");
}

#[test]
fn interpolation_keeps_written_and_positional_holes_apart() {
    let text = dump_source("package main\nfn main() { println($\"i={i} and {}\", n) }\n");
    assert!(text.contains("println($\"i=${i} and {}\", n)"), "{text}");
}

#[test]
fn control_flow_renders_nested_shape() {
    let text = dump_source(
        "package main\nfn main() {\n    for i := 0, i < 3, i += 1 {\n        if i == 1 { continue }\n        else if i == 2 { break }\n    }\n    while x < 4 { x -= 1 }\n    loop { defer println(\"b\") }\n}\n",
    );
    assert!(text.contains("for i := 0, (i < 3), i += 1 {"), "{text}");
    assert!(
        text.contains(
            "if (i == 1) {\n      continue\n    } else if (i == 2) {\n      break\n    }\n"
        ),
        "{text}"
    );
    assert!(text.contains("while (x < 4) {\n    x -= 1\n  }"), "{text}");
    assert!(text.contains("defer println(\"b\")"), "{text}");
}
