use dux_diag::Diagnostic;
use dux_hir::{check, resolve};
use dux_source::SourceMap;

const MAX: &str = "fn max<T>(a: T, b: T) -> T {
    return if a > b { a } else { b }
}
";

const PAIR: &str = "struct Pair<A, B> {
    first: A,
    second: B,
}
";

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
        parsed.diagnostics.is_empty(),
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

fn reported(source: &str) -> Vec<Diagnostic> {
    let mut sources = SourceMap::new();
    let file = sources.add("t.dux", completed(source));
    let lexed = dux_lexer::lex(&sources, file);
    let parsed = dux_parser::parse(&sources, &lexed.tokens);
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    let mut diagnostics = resolve(&parsed.program).diagnostics;
    diagnostics.extend(check(&parsed.program));
    diagnostics
}

fn codes(source: &str) -> Vec<&'static str> {
    diagnose(source).iter().map(|d| d.code).collect::<Vec<_>>()
}

fn said(source: &str) -> Vec<String> {
    diagnose(source)
        .iter()
        .map(|d| format!("{} {}", d.message, d.note.clone().unwrap_or_default()))
        .collect()
}

#[test]
fn a_generic_function_reads_its_arguments_to_know_what_it_returns() {
    assert_eq!(
        codes(&format!(
            "{MAX}fn main() {{ n := max(3, 7); println($\"{{}}\", n) }}"
        )),
        Vec::<&str>::new(),
        "`max(3, 7)` is `max<i32>`, and the body's `>` works on `i32`"
    );
    assert_eq!(
        codes(&format!("{MAX}fn main() {{ narrow: i16 = max(3, 7) }}")),
        vec![dux_diag::code::LOSSY_CONVERSION],
        "the answer is an `i32`, which is what the call said and not what the binding asks"
    );
}

#[test]
fn a_type_argument_written_out_stands_over_the_arguments() {
    assert_eq!(
        codes(&format!(
            "{MAX}fn main() {{ println($\"{{}}\", max<f64>(2.5, 1.5)) }}"
        )),
        Vec::<&str>::new()
    );
    let said = said(&format!("{MAX}fn main() {{ max<string>(\"a\", \"b\") }}"));
    assert_eq!(
        said.len(),
        1,
        "ordering two `string`s is not a thing, and it is the call that asked for it: {said:?}"
    );
    assert!(
        said[0].contains("max<string>"),
        "the note points back at the generic body: {}",
        said[0]
    );
    assert!(
        said[0].contains("`string`"),
        "what went wrong survives: {}",
        said[0]
    );
}

#[test]
fn an_instantiation_is_read_once_however_often_it_is_called() {
    assert_eq!(
        codes(&format!(
            "{MAX}fn main() {{ max(1, 2); max(3, 4); max(5, 6) }}"
        )),
        Vec::<&str>::new()
    );
    assert_eq!(
        codes(&format!(
            "{MAX}fn main() {{ max<string>(\"a\", \"b\"); max<string>(\"c\", \"d\") }}"
        )),
        vec![dux_diag::code::TYPE_MISMATCH],
        "the second call asks for the same instantiation the first one already read"
    );
}

#[test]
fn a_generic_record_field_is_read_through_its_arguments() {
    let program = format!(
        "{PAIR}fn main() {{ p := Pair<i32, string> {{ first: 1, second: \"one\" }}; w: i32 = p.first; name: string = p.second; println($\"{{}} {{}}\", w, name) }}"
    );
    assert_eq!(codes(&program), Vec::<&str>::new());
    let wrong = format!(
        "{PAIR}fn main() {{ p := Pair<i32, string> {{ first: 1, second: \"one\" }}; n: i16 = p.first }}"
    );
    assert_eq!(
        codes(&wrong),
        vec![dux_diag::code::LOSSY_CONVERSION],
        "`p.first` is the `i32` the arguments said it was"
    );
    let missing = format!(
        "{PAIR}fn main() {{ p := Pair<i32, string> {{ first: 1, second: \"one\" }}; q := p.third; println($\"{{}}\", q) }}"
    );
    let said = said(&missing);
    assert_eq!(said.len(), 1, "{said:?}");
    assert!(
        said[0].contains("`Pair` has no field `third`"),
        "{}",
        said[0]
    );
    assert!(
        said[0].contains("first, second"),
        "the fields a generic record has are worth naming: {}",
        said[0]
    );
}

#[test]
fn a_field_that_does_not_fit_its_argument_is_said_at_the_call() {
    let said = said(&format!(
        "{PAIR}fn main() {{ p := Pair<i32, string> {{ first: \"one\", second: 1 }} }}"
    ));
    assert_eq!(said.len(), 2, "{said:?}");
    for line in &said {
        assert!(
            line.contains("`i32`") || line.contains("`string`"),
            "the complaint names both what was asked and what was given: {line}"
        );
    }
}

#[test]
fn a_const_parameter_answers_an_array_length() {
    let program = "fn zeros<const N: usize>() -> [f32; N] {
    return [0.0; N]
}

fn main() {
    grid := zeros<4>()
    first: f32 = grid[0]
    println($\"{}\", first)
}";
    assert_eq!(codes(program), Vec::<&str>::new());
}

#[test]
fn a_generic_call_still_counts_its_arguments() {
    assert_eq!(
        reported(&format!("{MAX}fn main() {{ max(1) }}"))
            .iter()
            .map(|d| d.code)
            .collect::<Vec<_>>(),
        vec![dux_diag::code::WRONG_ARITY],
        "a generic call is still a call of two arguments"
    );
    let said = said(&format!("{MAX}fn main() {{ max<i64, i64>(1, 2) }}"));
    assert_eq!(said.len(), 1, "{said:?}");
    assert!(
        said[0].contains("written over 1 parameter"),
        "a type argument list has an arity too: {}",
        said[0]
    );
}

#[test]
fn two_instantiations_of_the_same_record_are_two_types() {
    let program = format!(
        "{PAIR}fn takes(p: Pair<i32, string>) {{ }}\nfn main() {{ q := Pair<string, i32> {{ first: \"a\", second: 1 }}; takes(q) }}"
    );
    let said = said(&program);
    assert_eq!(said.len(), 1, "{said:?}");
    assert!(
        said[0].contains("Pair<string, i32>") && said[0].contains("Pair<i32, string>"),
        "the two instantiations are named as what they are: {}",
        said[0]
    );
}

#[test]
fn a_generic_enum_carries_its_arguments_into_its_variants() {
    let program = "enum Either<L, R> {
    Left(L),
    Right(R),
}

fn main() {
    e: Either<string, i32> = Either::Right(42)
    match e {
        Either::Left(s) => println($\"left {}\", s),
        Either::Right(n) => println($\"right {}\", n),
    }
}";
    assert_eq!(codes(program), Vec::<&str>::new());
    let wrong = program.replace("println($\"right {}\", n)", "s2: string = n");
    assert_eq!(
        codes(&wrong),
        vec![dux_diag::code::TYPE_MISMATCH],
        "`Right` carries the `i32` the arguments said, not anything else"
    );
}

const CELL: &str = "struct Cell<T> {
    value: T,
    next: Option<*Cell<T>>,
}

impl Cell<T> {
    fn value(self: Cell<T>) -> T {
        return self.value
    }

    fn shown(self: Cell<T>, extra: T) -> T {
        return self.value
    }
}
";

#[test]
fn a_method_of_a_generic_record_sees_what_its_receiver_said() {
    let clean = said(&format!(
        r#"{CELL}fn main() {{
    n := Cell<i32> {{ value: 9, next: None }}
    s := Cell<string> {{ value: "s", next: None }}
    a := n.shown(1)
    b := s.shown("x")
    println(1)
    println(a)
    println(b)
}}"#
    ));
    assert_eq!(
        clean,
        Vec::<&str>::new(),
        "one method, two receivers, each in its own types: {clean:?}"
    );

    let wrong = codes(&format!(
        r#"{CELL}fn main() {{
    s := Cell<string> {{ value: "s", next: None }}
    s.shown(1)
}}"#
    ));
    assert_eq!(
        wrong,
        vec![dux_diag::code::TYPE_MISMATCH],
        "the receiver says `T` is `string`, and the argument has to fit that: {wrong:?}"
    );
}

#[test]
fn a_method_body_that_does_not_fit_a_receiver_is_said_at_the_call() {
    let body = CELL.replace(
        "return self.value\n    }\n\n    fn shown",
        "return self.next\n    }\n\n    fn shown",
    );
    let program = format!(
        r#"{body}fn main() {{
    s := Cell<string> {{ value: "s", next: None }}
    s.value()
}}"#
    );
    let reported = reported(&program);
    assert_eq!(
        reported.len(),
        1,
        "the body is read again for the receiver that called it: {reported:?}"
    );
    let note = reported[0].note.as_deref().unwrap_or_default();
    assert!(
        note.contains("Cell<string>.value"),
        "the complaint says which filling it was read in: {note}"
    );
}

const MAYBE: &str = "enum Maybe<T> {
    Some(T),
    Nothing,
}

impl Maybe<T> {
    fn shown(self, fallback: T) -> T {
        match self {
            Maybe::Some(v) => return v,
            Maybe::Nothing => return fallback,
        }
    }
}
";

#[test]
fn a_method_of_a_generic_sum_type_sees_what_its_receiver_said() {
    let clean = said(&format!(
        r#"{MAYBE}fn main() {{
    a: Maybe<i32> = Maybe::Some(7)
    b: Maybe<string> = Maybe::Nothing
    println(a.shown(0))
    println(b.shown("d"))
}}"#
    ));
    assert_eq!(
        clean,
        Vec::<&str>::new(),
        "one method, two receivers, each in its own types: {clean:?}"
    );

    let wrong = codes(&format!(
        r#"{MAYBE}fn main() {{
    b: Maybe<string> = Maybe::Nothing
    b.shown(0)
}}"#
    ));
    assert_eq!(
        wrong,
        vec![dux_diag::code::TYPE_MISMATCH],
        "the receiver says `T` is `string`, and the argument has to fit that: {wrong:?}"
    );
}
