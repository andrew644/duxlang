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
fn the_arms_of_an_if_value_must_agree() {
    assert_eq!(
        codes(
            "fn main() {
    c := true
    x := if c { 1 } else { \"two\" }
    println($\"{}\", x)
}
"
        ),
        vec!["E0201"]
    );
    assert_eq!(
        codes(
            "fn main() {
    c := true
    x := if c { 1 } else { 2 }
    y := if c { 1 } else { 2.5 }
    z := if c { \"a\" } else { \"b\" }
    println($\"{} {} {}\", x, y, z)
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
fn a_field_of_a_record_that_does_not_exist_is_reported_once_per_use() {
    assert_eq!(
        codes(
            "struct Point {
    x: i32,
    y: i32,
}

fn main() {
    p := Point { x: 1, y: 2 }
    p.z = 3
    println($\"{}\", p.w)
}
"
        ),
        vec!["E0201", "E0201"]
    );
}

#[test]
fn a_record_field_is_assigned_a_value_of_its_own_type() {
    assert_eq!(
        codes(
            "struct Window {
    title: string,
    width: i32,
}

fn main() {
    w := Window { title: \"dux\", width: 8 }
    w.title = \"duxlang\"
    w.width += 2
    println($\"{} {}\", w.title, w.width)
}
"
        ),
        Vec::<&'static str>::new()
    );
    assert_eq!(
        codes(
            "struct Window {
    title: string,
    width: i32,
}

fn main() {
    w := Window { title: \"dux\", width: 8 }
    w.title = 8
    println($\"{}\", w.title)
}
"
        ),
        vec!["E0201"]
    );
}

#[test]
fn a_record_nested_in_a_record_keeps_its_own_fields() {
    assert_eq!(
        codes(
            "struct Vec3 {
    x: f32,
    y: f32,
}

struct AABB {
    min: Vec3,
    max: Vec3,
}

fn main() {
    box := AABB { min: Vec3 { x: 0.0, y: 0.0 }, max: Vec3 { x: 1.0, y: 1.0 } }
    edge: f32 = box.max.x
    wide: f64 = edge
    box.min.y = 3.0
    println($\"{} {}\", wide, box.min.y)
}
"
        ),
        Vec::<&'static str>::new()
    );
    assert_eq!(
        codes(
            "struct Vec3 {
    x: f32,
    y: f32,
}

struct AABB {
    min: Vec3,
    max: Vec3,
}

fn main() {
    box := AABB { min: Vec3 { x: 0.0, y: 0.0 }, max: Vec3 { x: 1.0, y: 1.0 } }
    box.max.x = \"east\"
    box.min = 2.0
    println($\"{}\", box.max.x)
}
"
        ),
        vec!["E0201", "E0201"]
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

#[test]
fn a_pointer_carries_its_target() {
    assert_eq!(
        codes(
            "fn main() {
    n: i32 = 1
    p := &n
    *p = 2
    wide: i64 = *p
    p += 1
    println($\"{}\", wide)
}
"
        ),
        Vec::<&'static str>::new()
    );
    assert_eq!(
        codes(
            "fn main() {
    n: i32 = 1
    p := &n
    *p = \"text\"
    println($\"{}\", *p)
}
"
        ),
        vec!["E0201"]
    );
    assert_eq!(
        codes(
            "fn main() {
    n: i32 = 1
    wide: i64 = 2
    p := &n
    q: *i64 = p
    println($\"{} {}\", *p, *q)
}
"
        ),
        vec!["E0200"]
    );
    assert_eq!(
        codes(
            "fn main() {
    n: i32 = 1
    p := &n
    bits: rawptr = p
    println($\"{}\", *bits)
}
"
        ),
        vec!["E0200"]
    );
}

#[test]
fn a_field_reached_through_a_pointer_is_the_targets_field() {
    assert_eq!(
        codes(
            "struct Vec3 {
    x: f32,
}

fn main() {
    v := Vec3 { x: 1.0 }
    p := &v
    p.x = 2.0
    wide: f64 = p.x
    p.z = 3.0
    println($\"{}\", wide)
}
"
        ),
        vec!["E0201"]
    );
    assert_eq!(
        codes(
            "struct Vec3 {
    x: f32,
}

fn main() {
    v := Vec3 { x: 1.0 }
    p := &v
    p.x = 2.0
    println($\"{}\", p.x)
}
"
        ),
        Vec::<&'static str>::new()
    );
}

const VEC3: &str = "struct Vec3 {
    x: f32,
}

impl Vec3 {
    fn doubled(self) -> f32 {
        return self.x + self.x
    }

    fn scale(self: *Vec3, k: f32) {
        self.x *= k
    }
}
";

#[test]
fn a_method_finds_its_receiver_through_a_pointer() {
    assert_eq!(
        codes(&format!(
            "{VEC3}
fn main() {{
    v := Vec3 {{ x: 1.0 }}
    p := &v
    p.scale(2.0)
    wide: f64 = p.doubled()
    println($\"{{}}\", wide)
}}
"
        )),
        Vec::<&'static str>::new()
    );
}

#[test]
fn a_method_call_is_checked_against_the_methods_signature() {
    assert_eq!(
        codes(&format!(
            "{VEC3}
fn main() {{
    v := Vec3 {{ x: 1.0 }}
    v.scale(\"two\")
    println($\"{{}}\", v.x)
}}
"
        )),
        vec!["E0201"]
    );
    assert_eq!(
        codes(&format!(
            "{VEC3}
fn main() {{
    v := Vec3 {{ x: 1.0 }}
    v.scale(2.0, 3.0)
    println($\"{{}}\", v.x)
}}
"
        )),
        vec!["E0112"]
    );
}

#[test]
fn a_method_the_record_does_not_have_is_named() {
    let found = diagnose(&format!(
        "{VEC3}
fn main() {{
    v := Vec3 {{ x: 1.0 }}
    println($\"{{}}\", v.nope())
}}
"
    ));
    assert_eq!(
        found.iter().map(|d| d.code).collect::<Vec<&'static str>>(),
        vec!["E0110"]
    );
    assert!(
        found[0].message.contains("`Vec3` has no method `nope`"),
        "{:?}",
        found[0]
    );
    assert!(
        found[0]
            .note
            .as_deref()
            .is_some_and(|note| note.contains("impl Vec3")),
        "{:?}",
        found[0]
    );
}

#[test]
fn self_is_the_record_or_a_pointer_to_it() {
    assert_eq!(
        codes(
            "struct Vec3 {
    x: f32,
}

impl Vec3 {
    fn via_pointer(self: *Vec3) -> f32 {
        self.x = 1.0
        return self.x
    }

    fn via_copy(self) -> f32 {
        self.x = 2.0
        return self.x
    }
}

fn main() {
    v := Vec3 { x: 0.0 }
    v.via_pointer()
    println($\"{}\", v.via_copy())
}
"
        ),
        Vec::<&'static str>::new()
    );
}

#[test]
fn an_array_is_a_slice_that_knows_its_own_length() {
    assert_eq!(
        codes(
            "fn take(xs: []i32) -> i64 {
    return xs.len() as i64
}

fn main() {
    dims: [i32; 4] = [10, 20, 30, 40]
    window: []i32 = &dims
    middle := &dims[1..3]
    println($\"{} {} {} {} {}\", take(dims), window.len(), middle.len(), window[0], middle[1])
}
"
        ),
        Vec::<&'static str>::new()
    );
    assert_eq!(
        codes(
            "fn main() {
    dims: [i32; 2] = [1, 2]
    window: []i64 = &dims
    println($\"{}\", window.len())
}
"
        ),
        // The element type is not a conversion away; it is simply another type.
        vec!["E0201"]
    );
    assert_eq!(
        codes(
            "fn main() {
    dims: [i32; 2] = [1, 2]
    window: []i32 = &dims
    wide: i64 = window[\"one\"]
    println($\"{}\", wide)
}
"
        ),
        vec!["E0201"]
    );
}

#[test]
fn a_slice_elements_type_flows_through_indexing_and_loops() {
    assert_eq!(
        codes(
            "struct Vec3 {
    x: f32,
}

fn main() {
    points: [Vec3; 2] = [Vec3 { x: 1.0 }, Vec3 { x: 2.0 }]
    view: []Vec3 = &points
    for p in view {
        p.x = 3.0
    }
    wide: f64 = view[0]
    println($\"{}\", wide)
}
"
        ),
        vec!["E0201"]
    );
    assert_eq!(
        codes(
            "struct Vec3 {
    x: f32,
}

fn main() {
    points: [Vec3; 2] = [Vec3 { x: 1.0 }, Vec3 { x: 2.0 }]
    view: []Vec3 = &points
    for i, p in view {
        println($\"{} {}\", i, p.x)
    }
}
"
        ),
        Vec::<&'static str>::new()
    );
}

#[test]
fn new_and_make_carry_their_element_type() {
    assert_eq!(
        codes(
            "package main
fn main() {
    p := new(i32)
    *p = 7
    xs := make(i32, 4)
    xs[0] = 1
    total: i32 = *p + xs[0]
    println($\"{}\", total)
    free(p)
    del(xs)
}"
        ),
        Vec::<&str>::new()
    );
}

#[test]
fn a_heap_pointer_is_a_pointer_with_a_target() {
    assert_eq!(
        codes(
            "package main
fn main() {
    p := new(i32)
    println($\"{}\", *p)
    q := p as rawptr
    println($\"{}\", 1)
    free(p)
}"
        ),
        Vec::<&str>::new()
    );
    let found = codes(
        "package main
fn main() {
    p := new(i32)
    *p = \"text\"
}",
    );
    assert_eq!(found, vec![dux_diag::code::TYPE_MISMATCH]);
}

#[test]
fn context_is_an_ordinary_name_a_function_can_read_and_rebind() {
    assert_eq!(
        codes(
            "package main
fn uses() {
    outer := context
    context = outer
}
fn main() {
    uses()
}"
        ),
        Vec::<&str>::new()
    );
}

#[test]
fn a_program_may_declare_its_own_prelude_names() {
    assert_eq!(
        codes(
            "package main
fn make() -> i32 {
    return 1
}
fn main() {
    println($\"{}\", make())
}"
        ),
        Vec::<&str>::new()
    );
}

#[test]
fn text_joins_and_compares_as_one_type() {
    assert_eq!(
        codes(
            "package main
fn main() {
    a := \"ab\"
    b := \"cd\"
    whole: string = a + b
    same: bool = a == b
    different: bool = a != whole
    println($\"{} {} {}\", whole.len(), same, different)
}"
        ),
        Vec::<&str>::new()
    );
    let found = codes(
        "package main
fn main() {
    a := \"ab\"
    n := a + 1
}",
    );
    assert_eq!(found, vec![dux_diag::code::TYPE_MISMATCH]);
}

#[test]
fn a_string_index_is_a_byte_and_a_subslice_is_text() {
    assert_eq!(
        codes(
            "package main
fn main() {
    text := \"hello\"
    first: u8 = text[1]
    head: string = text[0..2]
    println($\"{} {}\", first, head.len())
}"
        ),
        Vec::<&str>::new()
    );
    let found = codes(
        "package main
fn main() {
    text := \"hello\"
    n: bool = text[0]
}",
    );
    assert_eq!(found, vec![dux_diag::code::TYPE_MISMATCH]);
}

#[test]
fn bytes_reach_text_for_free_and_text_gives_up_its_address() {
    assert_eq!(
        codes(
            "package main
fn echo(text: string) {
    println(text)
}
fn main() {
    echo(c\"bytes\")
    where_ := \"text\" as rawptr
    println($\"{}\", 1)
}"
        ),
        Vec::<&str>::new()
    );
    let found = codes(
        "package main
fn main() {
    n: i32 = \"text\"
}",
    );
    assert_eq!(found, vec![dux_diag::code::TYPE_MISMATCH]);
}

#[test]
fn a_c_string_is_bytes_not_text() {
    assert_eq!(
        codes(
            "package main
fn main() {
    text := c\"bytes\"
    where_: rawptr = text
    println($\"{}\", 1)
}"
        ),
        Vec::<&str>::new()
    );
    let found = codes(
        "package main
fn main() {
    text := \"text\"
    where_: rawptr = text
}",
    );
    assert_eq!(
        found,
        vec![dux_diag::code::TYPE_MISMATCH],
        "a string keeps its address behind an `as`"
    );
}
