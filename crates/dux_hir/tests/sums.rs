use dux_diag::Diagnostic;
use dux_hir::{check, resolve};
use dux_source::SourceMap;

const SUMS: &str = "enum Shape {
    Circle(f32),
    Rect { w: f32, h: f32 },
    Empty,
}

enum Code {
    One,
    Two,
}
";

/// A fixture is a fragment of a program; the resolver wants a whole one.
fn completed(source: &str) -> String {
    match source.contains("fn main") {
        true => source.to_string(),
        false => format!("{source}\nfn main() {{}}\n"),
    }
}

fn diagnose(source: &str) -> Vec<Diagnostic> {
    let mut sources = SourceMap::new();
    let file = sources.add("t.dux", completed(source));
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

fn codes_of(diagnostics: &[Diagnostic]) -> Vec<&'static str> {
    diagnostics.iter().map(|d| d.code).collect()
}

fn codes(source: &str) -> Vec<&'static str> {
    codes_of(&diagnose(source))
}

/// Names and types a program gets wrong before the checker could: the resolver
/// answers these, and its report is what a fixture should read.
fn names_and_types(source: &str) -> Vec<&'static str> {
    let mut sources = SourceMap::new();
    let file = sources.add("t.dux", completed(source));
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
    let mut found = codes_of(&resolve(&parsed.program).diagnostics);
    found.extend(codes_of(&check(&parsed.program)));
    found
}

fn with_sums(source: &str) -> String {
    format!("{SUMS}{source}")
}

fn sums(source: &str) -> Vec<&'static str> {
    codes(&with_sums(source))
}

const CLEAN: Vec<&str> = Vec::<&str>::new();

#[test]
fn a_match_that_reaches_every_variant_is_clean() {
    assert_eq!(
        sums(
            "fn area(shape: Shape) -> f32 {
    match shape {
        Shape::Circle(r) => { r * r },
        Shape::Rect { w, h } => { w * h },
        Shape::Empty => { 0.0 },
    }
}

fn main() {
    println($\"{}\", area(Shape::Empty))
}"
        ),
        CLEAN
    );
}

#[test]
fn a_wildcard_or_an_else_covers_whatever_the_arms_leave() {
    for tail in ["_ => { 0.0 },", "else => { 0.0 },"] {
        assert_eq!(
            sums(&format!(
                "fn area(shape: Shape) -> f32 {{
    match shape {{
        Shape::Circle(r) => {{ r * r }},
        {tail}
    }}
}}

fn main() {{
    println($\"{{}}\", area(Shape::Empty))
}}"
            )),
            CLEAN
        );
    }
}

#[test]
fn a_missing_variant_is_e0133_and_names_it() {
    let source = SUMS.to_string()
        + "fn area(shape: Shape) -> f32 {
    match shape {
        Shape::Circle(r) => { r * r },
        Shape::Empty => { 0.0 },
    }
}

fn main() {
    println($\"{}\", area(Shape::Empty))
}";
    let found = diagnose(&source);
    assert_eq!(codes_of(&found), vec![dux_diag::code::NON_EXHAUSTIVE]);
    let message = &found[0].message;
    assert!(
        message.contains("not exhaustive") && message.contains("`Shape::Rect`"),
        "{message}"
    );
    let at = found[0]
        .span
        .expect("the fault carries a span")
        .start as usize;
    assert_eq!(
        &source[at..at + 5],
        "match",
        "the `match` is where the fault lies"
    );
    assert!(
        found[0].note.is_some(),
        "the fault should say what to write instead"
    );
}

#[test]
fn a_guard_is_no_evidence_that_a_variant_is_handled() {
    assert_eq!(
        sums(
            "fn pick(code: Code) -> i32 {
    match code {
        Code::One if true => 1,
        Code::Two => 2,
    }
}

fn main() {
    println($\"{}\", pick(Code::One))
}"
        ),
        vec![dux_diag::code::NON_EXHAUSTIVE]
    );
    assert_eq!(
        sums(
            "fn pick(code: Code) -> i32 {
    match code {
        Code::One if true => 1,
        Code::Two if false => 2,
    }
}

fn main() {
    println($\"{}\", pick(Code::One))
}"
        ),
        vec![dux_diag::code::NON_EXHAUSTIVE]
    );
    assert_eq!(
        codes(
            "fn main() {
    n := 1
    match n {
        x if x > 0 => 1,
        _ => 0,
    }
}"
        ),
        CLEAN
    );
    assert_eq!(
        codes(
            "fn sign(n: i32) -> i32 {
    match n {
        x if x > \"0\" => 1,
        _ => 0,
    }
}"
        ),
        vec![dux_diag::code::TYPE_MISMATCH]
    );
}

#[test]
fn a_bool_must_be_matched_both_ways() {
    assert_eq!(
        codes(
            "fn flip(b: bool) -> bool {
    match b {
        true => false,
    }
}"
        ),
        vec![dux_diag::code::NON_EXHAUSTIVE]
    );
    assert_eq!(
        codes(
            "fn flip(b: bool) -> bool {
    match b {
        true => false,
        false => true,
    }
}"
        ),
        CLEAN
    );
}

#[test]
fn a_number_needs_a_catch_all() {
    assert_eq!(
        codes(
            "fn sign(n: i32) -> i32 {
    match n {
        0 => 0,
    }
}"
        ),
        vec![dux_diag::code::NON_EXHAUSTIVE]
    );
    assert_eq!(
        codes(
            "fn sign(n: i32) -> i32 {
    match n {
        0 => 0,
        1 ..= 9 => 1,
        -8 ..= -1 => -1,
        _ => -2,
    }
}"
        ),
        CLEAN
    );
    assert_eq!(
        codes(
            "fn sign(n: i32) -> i32 {
    match n {
        \"x\" => 0,
        _ => 1,
    }
}"
        ),
        vec![dux_diag::code::TYPE_MISMATCH]
    );
}

#[test]
fn an_or_pattern_counts_every_alternative_it_lists() {
    assert_eq!(
        sums(
            "fn number(code: Code) -> i32 {
    match code {
        Code::One | Code::Two => 1,
    }
}

fn main() {
    println($\"{}\", number(Code::One))
}"
        ),
        CLEAN
    );
    assert_eq!(
        sums(
            "fn number(code: Code) -> i32 {
    match code {
        Code::One | Code::One => 1,
    }
}

fn main() {
    println($\"{}\", number(Code::One))
}"
        ),
        vec![dux_diag::code::NON_EXHAUSTIVE]
    );
}

#[test]
fn a_binder_takes_the_type_of_the_payload_it_sits_on() {
    assert_eq!(
        sums(
            "fn area(shape: Shape) -> f32 {
    match shape {
        Shape::Circle(r) => { r * r },
        Shape::Rect { w, h } => { w * h },
        Shape::Empty => { 0.0 },
    }
}

fn main() {
    println($\"{}\", area(Shape::Empty))
}"
        ),
        CLEAN
    );
    assert_eq!(
        names_and_types(
            &with_sums(
                "fn area(shape: Shape) -> f32 {
    match shape {
        Shape::Circle(r) => { r * r },
        Shape::Rect { w, h } => { w * h },
        Shape::Empty => { r },
    }
}

fn main() {
    println($\"{}\", area(Shape::Empty))
}"
            )
        ),
        vec![dux_diag::code::UNDECLARED]
    );
    assert_eq!(
        sums(
            "fn area(shape: Shape) -> f32 {
    match shape {
        Shape::Circle(r) => { r * r },
        Shape::Rect { w, h } => { w * h },
        Shape::Empty => { \"none\" },
    }
}

fn main() {
    println($\"{}\", area(Shape::Empty))
}"
        ),
        vec![dux_diag::code::TYPE_MISMATCH]
    );
}

#[test]
fn a_pattern_of_the_wrong_sum_type_is_refused() {
    assert_eq!(
        sums(
            "fn number(shape: Shape) -> i32 {
    match shape {
        Code::One => 1,
        _ => 0,
    }
}

fn main() {
    println($\"{}\", number(Shape::Empty))
}"
        ),
        vec![dux_diag::code::TYPE_MISMATCH]
    );
    assert_eq!(
        sums(
            "fn number(shape: Shape) -> i32 {
    match shape {
        Shape::Ring => 1,
        _ => 0,
    }
}

fn main() {
    println($\"{}\", number(Shape::Empty))
}"
        ),
        vec![dux_diag::code::UNDECLARED]
    );
    assert_eq!(
        codes(
            "fn number(n: i32) -> i32 {
    match n {
        Code::One => 1,
        _ => 0,
    }
}"
        ),
        vec![dux_diag::code::TYPE_MISMATCH]
    );
}

#[test]
fn a_variant_makes_what_its_payload_declares() {
    assert_eq!(
        sums(
            "fn main() {
    shape := Shape::Circle(2.0)
    other: Shape = Shape::Rect { w: 1.0, h: 2.0 }
    println($\"{} {}\", shape, other)
}"
        ),
        CLEAN
    );
    assert_eq!(
        sums(
            "fn main() {
    shape := Shape::Circle(2.0, 3.0)
    println($\"{}\", shape)
}"
        ),
        vec![dux_diag::code::WRONG_ARITY]
    );
    assert_eq!(
        sums(
            "fn main() {
    shape := Shape::Circle(\"wide\")
    println($\"{}\", shape)
}"
        ),
        vec![dux_diag::code::TYPE_MISMATCH]
    );
    assert_eq!(
        sums(
            "fn main() {
    shape := Shape::Empty(1)
    println($\"{}\", shape)
}"
        ),
        vec![dux_diag::code::WRONG_ARITY]
    );
    assert_eq!(
        sums(
            "fn main() {
    shape := Shape::Rect { w: 1.0, h: 2.0, depth: 3.0 }
    println($\"{}\", shape)
}"
        ),
        vec![dux_diag::code::TYPE_MISMATCH]
    );
}

#[test]
fn a_struct_pattern_takes_every_field_or_says_it_leaves_them() {
    assert_eq!(
        sums(
            "fn area(shape: Shape) -> f32 {
    match shape {
        Shape::Rect { w, h } => { w * h },
        _ => { 0.0 },
    }
}

fn main() {
    println($\"{}\", area(Shape::Empty))
}"
        ),
        CLEAN
    );
    assert_eq!(
        sums(
            "fn area(shape: Shape) -> f32 {
    match shape {
        Shape::Rect { w } => { w },
        _ => { 0.0 },
    }
}

fn main() {
    println($\"{}\", area(Shape::Empty))
}"
        ),
        vec![dux_diag::code::TYPE_MISMATCH]
    );
    assert_eq!(
        sums(
            "fn area(shape: Shape) -> f32 {
    match shape {
        Shape::Rect { w, .. } => { w },
        _ => { 0.0 },
    }
}

fn main() {
    println($\"{}\", area(Shape::Empty))
}"
        ),
        CLEAN
    );
    assert_eq!(
        sums(
            "fn area(shape: Shape) -> f32 {
    match shape {
        Shape::Rect { w, depth } => { w },
        _ => { 0.0 },
    }
}

fn main() {
    println($\"{}\", area(Shape::Empty))
}"
        ),
        vec![dux_diag::code::TYPE_MISMATCH, dux_diag::code::TYPE_MISMATCH]
    );
}

#[test]
fn a_nested_pattern_reaches_through_a_payload() {
    assert_eq!(
        codes(
            "enum Level {
    Low(i32),
    High(string),
}

enum Item {
    Mark(Level),
    Bare,
}

fn value(item: Item) -> i32 {
    match item {
        Item::Mark(Level::Low(n)) => n,
        Item::Mark(Level::High(text)) => { text.len() as i32 },
        Item::Bare => 0,
    }
}

fn main() {
    println($\"{}\", value(Item::Mark(Level::Low(3))))
}"
        ),
        Vec::<&str>::new()
    );
    assert_eq!(
        codes(
            "enum Level {
    Low(i32),
    High(string),
}

enum Item {
    Mark(Level),
}

fn value(item: Item) -> i32 {
    match item {
        Item::Mark(Level::Low(n)) => n,
        Item::Mark(Level::High(text)) => text,
    }
}"
        ),
        vec![dux_diag::code::TYPE_MISMATCH]
    );
}

#[test]
fn option_and_result_are_ordinary_predeclared_enums() {
    assert_eq!(
        codes(
            "fn half(n: i32) -> Option<i32> {
    if n % 2 == 0 {
        return Some(n / 2)
    }
    return None
}

fn main() {
    match half(4) {
        Some(half) => { println($\"{}\", half) },
        None => { println($\"nothing\") },
    }
    value: i32 = match half(3) {
        Some(half) => half,
        None => 0,
    }
    println($\"{}\", value)
}"
        ),
        Vec::<&str>::new()
    );
    assert_eq!(
        codes(
            "fn main() {
    value: i32 = match Some(1) {
        Some(n) => n,
        None => 0,
    }
    println($\"{}\", value)
}"
        ),
        Vec::<&str>::new()
    );
    assert_eq!(
        codes(
            "fn main() {
    value: i32 = match Some(\"text\") {
        Some(n) => n,
        None => 0,
    }
    println($\"{}\", value)
}"
        ),
        vec![dux_diag::code::TYPE_MISMATCH]
    );
    assert_eq!(
        codes(
            "fn main() {
    pick: Option<i32> = None
    println($\"{}\", pick)
}"
        ),
        Vec::<&str>::new()
    );
    assert_eq!(
        codes(
            "fn main() {
    pick: Option<i32> = Some(1)
    miss: Option<i32> = None
    println($\"{} {}\", pick, miss)
}"
        ),
        Vec::<&str>::new()
    );
}

#[test]
fn a_result_carries_two_arguments_and_arms_name_both() {
    assert_eq!(
        codes(
            "enum Why {
    Missing,
}

fn parse(text: string) -> Result<i32, Why> {
    if text.len() == 0 {
        return Err(Why::Missing)
    }
    return Ok(1)
}

fn main() {
    match parse(\"12\") {
        Ok(n) => { println($\"{}\", n) },
        Err(Why::Missing) => { println($\"empty\") },
    }
}"
        ),
        Vec::<&str>::new()
    );
    assert_eq!(
        codes(
            "enum Why {
    Missing,
}

fn main() {
    answer: Why = match Ok(1) {
        Ok(n) => n,
        Err(why) => why,
    }
    println($\"{}\", answer)
}"
        ),
        vec![dux_diag::code::TYPE_MISMATCH]
    );
}

#[test]
fn a_match_may_be_a_statement_and_its_arms_may_end_in_return() {
    assert_eq!(
        sums(
            "fn area(shape: Shape) -> f32 {
    match shape {
        Shape::Circle(r) => { return r * r },
        Shape::Rect { w, h } => { return w * h },
        Shape::Empty => { return 0.0 },
    }
    return 0.0
}

fn main() {
    println($\"{}\", area(Shape::Empty))
}"
        ),
        CLEAN
    );
}

#[test]
fn the_arms_of_a_match_expression_meet_at_one_type() {
    assert_eq!(
        sums(
            "fn main() {
    n := match Shape::Empty {
        Shape::Empty => 1,
        _ => \"one\",
    }
    println($\"{}\", n)
}"
        ),
        vec![dux_diag::code::TYPE_MISMATCH]
    );
}

#[test]
fn a_pattern_head_binds_what_it_matches_and_scopes_it_to_the_body() {
    assert_eq!(
        codes(
            "fn main() {
    if Some(n) := lookup() {
        println($\"{}\", n)
    }
}

fn lookup() -> Option<i32> {
    return Some(1)
}"
        ),
        Vec::<&str>::new()
    );
    assert_eq!(
        names_and_types(
            "fn main() {
    if Some(n) := lookup() {
        println($\"{}\", n)
    }
    println($\"{}\", n)
}

fn lookup() -> Option<i32> {
    return Some(1)
}"
        ),
        vec![dux_diag::code::UNDECLARED]
    );
    assert_eq!(
        codes(
            "fn main() {
    if Some(n) := lookup() {
        println($\"{}\", n)
    }
}

fn lookup() -> i32 {
    return 1
}"
        ),
        vec![dux_diag::code::TYPE_MISMATCH]
    );
    assert_eq!(
        codes(
            "fn main() {
    while Some(n) := lookup() {
        println($\"{}\", n)
    }
}

fn lookup() -> Option<i32> {
    return Some(1)
}"
        ),
        Vec::<&str>::new()
    );
    assert_eq!(
        codes(
            "fn main() {
    if Ok(n) := lookup() {
        println($\"{}\", n)
    }
}

fn lookup() -> Option<i32> {
    return Some(1)
}"
        ),
        vec![dux_diag::code::TYPE_MISMATCH]
    );
}
