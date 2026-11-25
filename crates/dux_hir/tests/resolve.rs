use dux_diag::Diagnostic;
use dux_hir::resolve;
use dux_source::SourceMap;
use std::path::Path;

const EXAMPLE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../example");

fn diagnose(source: &str) -> Vec<Diagnostic> {
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
    resolve(&parsed.program).diagnostics
}

fn codes(source: &str) -> Vec<&'static str> {
    diagnose(source).iter().map(|d| d.code).collect()
}

#[test]
fn a_clean_program_resolves_without_a_word() {
    assert_eq!(
        codes(
            "package main

const KI: usize = 1024
global hits: i32 = 0

fn bump() -> i32 {
    hits += 1
    return hits
}

fn main() {
    page := KI + 1
    bump()
    println($\"{} {}\", hits, page)
}
"
        ),
        Vec::<&'static str>::new()
    );
}

#[test]
fn an_undeclared_name_is_e0110_and_every_use_is_reported() {
    let diagnostics = diagnose(
        "fn main() {
    a := nope
    b := nope + 1
    println(\"x\")
}
",
    );
    let codes: Vec<&str> = diagnostics.iter().map(|d| d.code).collect();
    assert_eq!(codes, vec!["E0110", "E0110"]);
    assert!(diagnostics
        .iter()
        .all(|d| d.message.contains("`nope` is not declared")));
    assert!(diagnostics.iter().all(|d| d.span.is_some()));
}

#[test]
fn a_call_that_supplies_the_wrong_number_of_arguments_is_e0112() {
    let declares_add = "fn add(a: i32, b: i32) -> i32 {
    return a + b
}
";
    assert_eq!(
        codes(&format!("{declares_add}fn main() {{\n    add(1)\n}}\n")),
        vec!["E0112"]
    );
    assert_eq!(
        codes(&format!(
            "{declares_add}fn main() {{\n    add(1, 2, 3)\n}}\n"
        )),
        vec!["E0112"]
    );
    assert_eq!(
        codes(&format!("{declares_add}fn main() {{\n    add(1, 2)\n}}\n")),
        Vec::<&'static str>::new()
    );
}

#[test]
fn a_milestone_gap_in_the_arguments_silences_the_arity_check() {
    assert_eq!(
        codes(
            "fn add(a: i32, b: i32) -> i32 {
    return a + b
}

fn main() {
    add((1, 2))
}
"
        ),
        // the tuple literal is an E0007 milestone gap owned by the parser;
        // the resolver must not cascade an E0112 on top of it.
        Vec::<&'static str>::new()
    );
}

#[test]
fn extern_and_builtin_calls_escape_the_arity_check() {
    assert_eq!(
        codes(
            "extern \"C\" {
    fn printf(fmt: *c_char) -> i32;
}

fn main() {
    printf(c\"a %d %s\\n\", 1, \"two\")
}
"
        ),
        Vec::<&'static str>::new()
    );
    assert_eq!(
        codes(
            "fn main() {
    new(i32)
    make(i32, 4)
    println($\"{}\", 1)
}
"
        ),
        Vec::<&'static str>::new()
    );
}

#[test]
fn duplicate_top_level_definitions_and_parameters_are_e0111() {
    assert_eq!(
        codes(
            "fn f() {}

fn f() {}

fn main() {
    f()
}
"
        ),
        vec!["E0111"]
    );
    assert_eq!(
        codes(
            "struct S {
    x: i32,
}

enum S {
    A
}

fn main() {}
"
        ),
        vec!["E0111"]
    );
    assert_eq!(
        codes(
            "fn f(a: i32, a: i32) {}

fn main() {}
"
        ),
        vec!["E0111"]
    );
}

#[test]
fn shadowing_is_not_a_duplicate() {
    assert_eq!(
        codes(
            "global g: i32 = 0

fn f(g: i32) -> i32 {
    inner := g + 1
    return inner
}

fn main() {
    g += 1
    println($\"{}\", f(2))
}
"
        ),
        Vec::<&'static str>::new()
    );
}

#[test]
fn scopes_follow_the_block_tree() {
    assert_eq!(
        codes(
            "fn main() {
    {
        hidden := 1
    }
    println($\"{}\", hidden)
}
"
        ),
        vec!["E0110"]
    );
    assert_eq!(
        codes(
            "fn main() {
    for i := 0, i < 3, i += 1 {
        use_me := i
        println($\"{}\", use_me)
    }
}
"
        ),
        Vec::<&'static str>::new()
    );
    assert_eq!(
        codes(
            "fn main() {
    for i, v in xs {
        println($\"{}{}\", i, v)
    }
}
"
        ),
        vec!["E0110"]
    );
    assert_eq!(
        codes(
            "fn main() {
    x := 1
    x := x + 1
    println($\"{}\", x)
}
"
        ),
        Vec::<&'static str>::new()
    );
}

#[test]
fn pattern_bindings_scope_to_their_body_only() {
    assert_eq!(
        codes(
            "fn find() -> Option<i32> {
    return None
}

fn main() {
    if Some(v) := find() {
        println($\"{}\", v)
    }
    println($\"{}\", v)
}
"
        ),
        vec!["E0110"]
    );
    assert_eq!(
        codes(
            "fn find() -> Option<i32> {
    return None
}

fn main() {
    if Some(v) := find() {
        println($\"{}\", v)
    }
}
"
        ),
        Vec::<&'static str>::new()
    );
    assert_eq!(
        codes(
            "fn next() -> Option<i32> {
    return None
}

fn take(n: i32) {}

fn main() {
    while Some(n) := next() {
        take(n)
    }
}
"
        ),
        Vec::<&'static str>::new()
    );
}

#[test]
fn interpolation_holes_are_uses() {
    assert_eq!(
        codes(
            "fn main() {
    println($\"value={mystery}\")
}
"
        ),
        vec!["E0110"]
    );
    assert_eq!(
        codes(
            "fn main() {
    m := 1
    println($\"value={m} {}\", m)
}
"
        ),
        Vec::<&'static str>::new()
    );
}

#[test]
fn forward_references_work_in_both_directions() {
    assert_eq!(
        codes(
            "fn main() {
    helper()
}

fn helper() {}
"
        ),
        Vec::<&'static str>::new()
    );
    assert_eq!(
        codes(
            "fn main() {
    println($\"{}\", LATER)
}

const LATER: i32 = 7
"
        ),
        Vec::<&'static str>::new()
    );
}

#[test]
fn universe_names_are_always_available() {
    assert_eq!(
        codes(
            "fn main() {
    o := Some(1)
    n: Option<i32> = None
    r := Ok(2)
    e := Err(\"x\")
    free(e)
    println($\"{} {} {} {}\", o, n, r, e)
}
"
        ),
        Vec::<&'static str>::new()
    );
    assert_eq!(
        codes(
            "fn main() {
    p := &mystery_box
    free(p)
}
"
        ),
        vec!["E0110"]
    );
}

#[test]
fn imports_and_c_namespaces_resolve_as_namespaces() {
    assert_eq!(
        codes(
            "import \"std/fmt\"
import \"std/mem\"

fn main() {
    fmt.println(\"x\")
    mem.Arena.new(8)
}
"
        ),
        Vec::<&'static str>::new()
    );
    assert_eq!(
        codes(
            "extern \"C\" {
    #[c_link(name = \"ctime\")]
    fn time(t: *i64) -> i64;
}

fn main() {
    secs := time(None)
    println($\"{}\", secs)
}
"
        ),
        Vec::<&'static str>::new()
    );
}

#[test]
fn methods_on_values_are_not_resolved_as_free_names() {
    assert_eq!(
        codes(
            "struct S {
    x: i32,
}

impl S {
    fn get(self) -> i32 {
        return self.x
    }

    fn twice(self) -> i32 {
        return self.get() * 2
    }
}

fn main() {
    s := S { x: 1 }
    println($\"{}\", s.twice())
}
"
        ),
        Vec::<&'static str>::new()
    );
}

#[test]
fn a_missing_or_malformed_main_is_reported_once() {
    assert_eq!(codes("fn other() {}\n"), vec!["E0100"]);
    assert_eq!(
        codes(
            "fn main(a: i32) {}
"
        ),
        vec!["E0101"]
    );
    assert_eq!(
        codes(
            "fn main() -> string {
    return \"x\"
}
"
        ),
        vec!["E0101"]
    );
    assert_eq!(
        codes(
            "fn main() -> i32 {
    return 0
}
"
        ),
        Vec::<&'static str>::new()
    );
}

#[test]
fn wildcards_and_underscore_bindings_are_not_uses() {
    assert_eq!(
        codes(
            "fn two() -> i32 {
    return 2
}

fn main() {
    _ := two()
    for _ in 0..3 {
        print(\"x\")
    }
}
"
        ),
        Vec::<&'static str>::new()
    );
}

#[test]
fn the_normative_corpus_resolves_without_a_single_diagnostic() {
    let mut offenders = Vec::new();
    for entry in std::fs::read_dir(EXAMPLE_DIR).expect("example/ exists") {
        let path = entry.expect("readable entry").path();
        if path.extension().is_some_and(|e| e != "dux") {
            continue;
        }
        let source = std::fs::read_to_string(&path).expect("example is readable");
        let mut sources = SourceMap::new();
        let file = sources.add(path.display().to_string(), source);
        let lexed = dux_lexer::lex(&sources, file);
        let parsed = dux_parser::parse(&sources, &lexed.tokens);
        let blocking: Vec<&Diagnostic> = parsed
            .diagnostics
            .iter()
            .filter(|d| {
                d.severity == dux_diag::Severity::Error
                    && d.code != dux_diag::code::UNSUPPORTED_IN_BUILD
            })
            .collect();
        if !blocking.is_empty() {
            continue;
        }
        let diagnostics = resolve(&parsed.program).diagnostics;
        if !diagnostics.is_empty() {
            offenders.push(format!(
                "{}: {:?}",
                path.file_stem().unwrap().to_string_lossy(),
                diagnostics
                    .iter()
                    .map(|d| (d.code, d.message.clone()))
                    .collect::<Vec<_>>()
            ));
        }
    }
    assert!(
        offenders.is_empty(),
        "the resolver must stay silent on the review samples:\n{}",
        offenders.join("\n")
    );
    let example_count = std::fs::read_dir(Path::new(EXAMPLE_DIR))
        .expect("example/ exists")
        .count();
    assert!(
        example_count >= 12,
        "the corpus should still be reviewed as a whole: {example_count} files"
    );
}

#[test]
fn an_impls_parameters_reach_its_method_bodies() {
    assert_eq!(
        codes(
            "struct Buf<T> {
    data: *T,
}

impl Buf<T> {
    fn one() -> Buf<T> {
        return Buf<T> { data: new(T) }
    }

    fn many(n: usize) -> []T {
        return make(T, n)
    }
}

fn main() {
    b := Buf<i32>.one()
}
"
        ),
        Vec::<&'static str>::new()
    );
}

#[test]
fn a_name_that_is_not_an_impls_parameter_is_still_unknown_in_its_methods() {
    assert_eq!(
        codes(
            "struct Buf<T> {
    data: *T,
}

impl Buf<T> {
    fn one() -> Buf<T> {
        return Buf<T> { data: new(Z) }
    }
}

fn main() {
    b := Buf<i32>.one()
}
"
        ),
        vec![dux_diag::code::UNDECLARED]
    );
}
