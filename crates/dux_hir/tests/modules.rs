use dux_diag::Diagnostic;
use dux_hir::{check, resolve};
use dux_source::SourceMap;

const MATH: &str = "mod math {
    fn square(x: i32) -> i32 {
        return twice(x)
    }

    fn twice(x: i32) -> i32 {
        return x * 2
    }
}
";

fn completed(source: &str) -> String {
    match source.contains("fn main") {
        true => source.to_string(),
        false => format!("{source}\nfn main() {{}}\n"),
    }
}

fn reported(source: &str) -> Vec<Diagnostic> {
    let mut sources = SourceMap::new();
    let file = sources.add("t.dux", completed(source));
    let lexed = dux_lexer::lex(&sources, file);
    let parsed = dux_parser::parse(&sources, &lexed.tokens);
    assert!(
        parsed.diagnostics.is_empty(),
        "fixture must parse: {:?}",
        parsed.diagnostics
    );
    let mut diagnostics = resolve(&parsed.program).diagnostics;
    diagnostics.extend(check(&parsed.program));
    diagnostics
}

fn codes(source: &str) -> Vec<&'static str> {
    reported(source).iter().map(|d| d.code).collect()
}

#[test]
fn a_declaration_is_known_by_where_its_module_says() {
    let program = format!(
        "{MATH}fn main() {{\n    println($\"{{}}\", math.square(3))\n}}"
    );
    assert_eq!(codes(&program), Vec::<&str>::new(), "one call through the module it lives in: {:?}", reported(&program));
}

#[test]
fn a_module_reads_its_own_names_bare() {
    let program = format!("{MATH}fn main() {{\n    println($\"{{}}\", twice(3))\n}}");
    let reported = reported(&program);
    assert_eq!(codes(&program), vec![dux_diag::code::UNDECLARED]);
    assert!(
        reported[0].message.contains("twice"),
        "the name that was not found is the name that was written: {:?}",
        reported[0]
    );
}

#[test]
fn two_modules_may_say_the_same_name() {
    let program = "mod a {
    fn helper() -> i32 {
        return 1
    }
}

mod b {
    fn helper() -> i32 {
        return 2
    }
}

fn main() {
    println($\"{}\", a.helper() + b.helper())
}";
    assert_eq!(
        codes(program),
        Vec::<&str>::new(),
        "two `helper`s, two places: {:?}",
        reported(program)
    );
}

#[test]
fn a_record_in_a_module_is_written_bare_inside_and_qualified_outside() {
    let program = "mod shape {
    struct Size {
        w: i32,
    }

    fn area(s: Size) -> i32 {
        return s.w * s.w
    }
}

fn main() {
    s := shape.Size { w: 3 }
    println($\"{}\", shape.area(s))
}";
    assert_eq!(codes(program), Vec::<&str>::new(), "{:?}", reported(program));

    let wrong = program.replace("println($\"{}\", shape.area(s))", "println($\"{}\", shape.area(shape.Size { w: \"three\" }))");
    assert_eq!(
        codes(&wrong),
        vec![dux_diag::code::TYPE_MISMATCH],
        "the record outside is the same record the module's own code reads: {:?}",
        reported(&wrong)
    );
}

#[test]
fn a_generic_function_lives_where_it_was_written() {
    let program = "mod picks {
    fn bigger<T>(a: T, b: T) -> T {
        return if a > b { a } else { b }
    }
}

fn main() {
    println($\"{}\", picks.bigger(3, 9))
}";
    assert_eq!(codes(program), Vec::<&str>::new(), "{:?}", reported(program));

    let wrong = program.replace("picks.bigger(3, 9)", "picks.bigger(3, \"nine\")");
    assert!(!codes(&wrong).is_empty(), "the arguments of one instantiation still have to agree");
}
