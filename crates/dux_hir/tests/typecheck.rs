use dux_diag::Diagnostic;
use dux_hir::{check, resolve};
use dux_source::SourceMap;

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
    let resolved = resolve(&parsed.program);
    assert!(
        resolved.diagnostics.is_empty(),
        "checker fixtures must resolve cleanly first: {:?}",
        resolved.diagnostics
    );
    check(&parsed.program)
}

fn codes(source: &str) -> Vec<&'static str> {
    diagnose(source)
        .iter()
        .map(|d| d.code)
        .collect()
}

#[test]
fn a_clean_program_typechecks_without_a_word() {
    assert_eq!(
        codes(
            "const KI = 1024

fn area(w: f64, h: f64) -> f64 {
    return w * h
}

fn main() {
    page: usize = KI + 1
    scale := 2
    total := 0
    for i := 0, i < 8, i += 1 {
        total += i * scale
    }
    wide := 1
    wider: i64 = wide
    bytes := wide as u8
    println($\"{} {} {} {} {}\", page, total, wider, bytes, area(2.0, 3.0))
}
"
        ),
        Vec::<&'static str>::new()
    );
}

#[test]
fn an_implicit_lossy_conversion_is_e0200_and_names_as() {
    let diagnostics = diagnose(
        "fn main() {
    wide := 1
    narrow: i8 = 0
    narrow = wide
}
",
    );
    assert_eq!(codes_of(&diagnostics), vec!["E0200"]);
    assert!(diagnostics[0].message.contains("write `as i8`"));
    assert!(diagnostics[0].message.contains("for free"));
}

#[test]
fn a_lossless_widening_is_free() {
    assert_eq!(
        codes(
            "fn main() {
    small: u16 = 7
    medium := small
    wide: i64 = small
    real: f64 = small
    println($\"{} {} {}\", medium, wide, real)
}
"
        ),
        Vec::<&'static str>::new()
    );
}

#[test]
fn signed_never_widens_into_unsigned_for_free() {
    assert_eq!(
        codes(
            "fn main() {
    s: i32 = 1
    u: u32 = s
    println($\"{}\", u)
}
"
        ),
        vec!["E0200"]
    );
    assert_eq!(
        codes(
            "fn main() {
    s: i32 = 1
    u: u32 = s as u32
    println($\"{}\", u)
}
"
        ),
        Vec::<&'static str>::new()
    );
}

#[test]
fn an_argument_that_does_not_fit_its_parameter_is_reported() {
    assert_eq!(
        codes(
            "fn takes(s: string) {}

fn main() {
    takes(42)
}
"
        ),
        vec!["E0201"]
    );
    assert_eq!(
        codes(
            "fn half(x: f64) -> f64 {
    return x / 2.0
}

fn main() {
    println($\"{}\", half(3))
}
"
        ),
        Vec::<&'static str>::new()
    );
}

#[test]
fn a_return_that_does_not_fit_the_signature_is_reported() {
    assert_eq!(
        codes(
            "fn count() -> i32 {
    return \"many\"
}

fn main() {
    println($\"{}\", count())
}
"
        ),
        vec!["E0201"]
    );
    assert_eq!(
        codes(
            "fn shout() {
    return \"noisy\"
}

fn main() {
    shout()
}
"
        ),
        vec!["E0201"]
    );
}

#[test]
fn a_literal_out_of_range_for_its_target_is_e0204() {
    assert_eq!(
        codes(
            "fn main() {
    small: u8 = 300
    println($\"{}\", small)
}
"
        ),
        vec!["E0204"]
    );
    assert_eq!(
        codes(
            "fn main() {
    signed: i8 = -128
    println($\"{}\", signed)
}
"
        ),
        Vec::<&'static str>::new()
    );
    assert_eq!(
        codes(
            "fn main() {
    signed: i8 = -129
    println($\"{}\", signed)
}
"
        ),
        vec!["E0204"]
    );
}

#[test]
fn a_condition_that_is_not_bool_is_e0202() {
    assert_eq!(
        codes(
            "fn main() {
    n := 1
    if n {
        println(\"x\")
    }
}
"
        ),
        vec!["E0202"]
    );
    assert_eq!(
        codes(
            "fn main() {
    n := 1
    if n == 1 {
        println(\"x\")
    }
}
"
        ),
        Vec::<&'static str>::new()
    );
    assert_eq!(
        codes(
            "fn flag() {
}

fn main() {
    while flag() {
        println(\"x\")
    }
}
"
        ),
        vec!["E0202"]
    );
}

#[test]
fn logical_operators_demand_bool_operands() {
    assert_eq!(
        codes(
            "fn main() {
    ok := true
    if ok && 1 {
        println(\"x\")
    }
}
"
        ),
        vec!["E0202"]
    );
    assert_eq!(
        codes(
            "fn main() {
    ok := true
    if ok && !ok {
        println(\"x\")
    }
}
"
        ),
        Vec::<&'static str>::new()
    );
}

#[test]
fn assigning_to_a_const_is_e0203() {
    let diagnostics = diagnose(
        "const MAX: i32 = 10

fn main() {
    MAX = 3
}
",
    );
    assert_eq!(codes_of(&diagnostics), vec!["E0203"]);
    assert!(diagnostics[0].message.contains("`MAX` is a `const`"));
}

#[test]
fn arithmetic_on_values_that_are_not_numbers_is_e0201() {
    assert_eq!(
        codes(
            "fn main() {
    a := \"x\"
    b := 1
    c := a + b
    println($\"{}\", c)
}
"
        ),
        vec!["E0201"]
    );
    assert_eq!(
        codes(
            "fn main() {
        t := true
        u := true
        v := t + u
        println($\"{}\", v)
}
"
        ),
        vec!["E0201"]
    );
}

#[test]
fn string_concatenation_is_typed_and_string_ordering_is_not() {
    assert_eq!(
        codes(
            "fn main() {
    path := \"/usr/\" + \"dux\"
    println($\"{}\", path)
}
"
        ),
        Vec::<&'static str>::new()
    );
    assert_eq!(
        codes(
            "fn main() {
    a := \"a\"
    b := \"b\"
    if a < b {
        println(\"x\")
    }
}
"
        ),
        vec!["E0201"]
    );
}

#[test]
fn mixed_numeric_operands_meet_at_the_common_type() {
    assert_eq!(
        codes(
            "fn main() {
    a: u8 = 1
    b: i32 = 2
    c := a + b
    d: i32 = c
    println($\"{}\", d)
}
"
        ),
        Vec::<&'static str>::new()
    );
    assert_eq!(
        codes(
            "fn main() {
    big: u64 = 1
    small: u8 = 2
    c := big + small
    d: u64 = c
    println($\"{}\", d)
}
"
        ),
        Vec::<&'static str>::new()
    );
}

#[test]
fn untyped_constants_adapt_to_the_target_through_arithmetic() {
    assert_eq!(
        codes(
            "const KI = 1024

fn main() {
    page: usize = KI + 1
    half := KI / 2
    println($\"{} {}\", page, half)
}
"
        ),
        Vec::<&'static str>::new()
    );
}

#[test]
fn struct_fields_are_checked_at_the_literal() {
    assert_eq!(
        codes(
            "struct Point {
    x: i32,
    y: i32,
}

fn main() {
    p := Point { x: 1, y: \"high\" }
    println($\"{}\", p.x)
}
"
        ),
        vec!["E0201"]
    );
    assert_eq!(
        codes(
            "struct Point {
    x: i32,
    y: i32,
}

fn origin() -> Point {
    return Point { x: 0, y: 0 }
}

fn main() {
    p := origin()
    println($\"{}\", p.y)
}
"
        ),
        Vec::<&'static str>::new()
    );
}

#[test]
fn indexing_an_array_yields_its_element_type() {
    assert_eq!(
        codes(
            "fn main() {
    dims: [i32; 3] = [1, 2, 3]
    n := dims[0]
    wide: i64 = n
    s := \"text\"
    b := s[0] == \"x\"
    println($\"{} {}\", wide, b)
}
"
        ),
        vec!["E0201"]
    );
}

#[test]
fn opaque_types_silence_every_rule_they_touch() {
    assert_eq!(
        codes(
            "extern \"C\" {
    fn c_string() -> *c_char;
}

fn main() {
    p := c_string()
    p += 1
    q: *c_char = p
    if q == None {
        println(\"null\")
    }
}
"
        ),
        Vec::<&'static str>::new()
    );
}

fn codes_of(diagnostics: &[Diagnostic]) -> Vec<&'static str> {
    diagnostics.iter().map(|d| d.code).collect()
}

#[test]
fn the_normative_corpus_typechecks_without_a_single_diagnostic() {
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
        if resolve(&parsed.program)
            .diagnostics
            .iter()
            .any(|d| d.severity == dux_diag::Severity::Error)
        {
            continue;
        }
        let diagnostics = check(&parsed.program);
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
        "the checker must stay silent on the review samples:\n{}",
        offenders.join("\n")
    );
}
