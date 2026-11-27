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
    diagnose(source).iter().map(|d| d.code).collect()
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
        if path.extension().is_none_or(|e| e != "dux") {
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
        // A milestone gap is not a verdict (DESIGN: `E0007` is valid Dux this
        // build cannot compile yet), so the same rule the parse is given below
        // holds here: a file that only says "not yet" is silent.
        let blocking: Vec<&Diagnostic> = diagnostics
            .iter()
            .filter(|d| d.code != dux_diag::code::UNSUPPORTED_IN_BUILD)
            .collect();
        if !blocking.is_empty() {
            offenders.push(format!(
                "{}: {:?}",
                path.file_stem().unwrap().to_string_lossy(),
                blocking
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

#[test]
fn a_method_called_on_the_type_reads_the_generic_body_at_that_instantiation() {
    assert_eq!(
        codes(
            "struct Bag<T> {
    items: []T,
    count: usize,
}

impl Bag<T> {
    fn empty() -> Bag<T> {
        return Bag<T> { items: make(T, 4), count: 0 }
    }

    fn put(self: *Bag<T>, value: T) {
        self.items[self.count] = value
        self.count += 1
    }

    fn take(self: *Bag<T>) -> Option<T> {
        if self.count == 0 {
            return None
        }
        self.count -= 1
        return Some(self.items[self.count])
    }
}

fn main() {
    bag := Bag<i32>.empty()
    bag.put(1)
    bag.put(2)
    if Some(v) := bag.take() {
        println($\"{}\", v)
    }
    text := Bag<string>.empty()
    text.put(\"dux\")
    println($\"{}\", text.take().unwrap_or(\"?\"))
}
"
        ),
        Vec::<&'static str>::new()
    );
}

#[test]
fn a_method_called_on_the_type_checks_its_arguments_through_the_instantiation() {
    assert_eq!(
        codes(
            "struct Bag<T> {
    items: []T,
    count: usize,
}

impl Bag<T> {
    fn empty() -> Bag<T> {
        return Bag<T> { items: make(T, 4), count: 0 }
    }

    fn put(self: *Bag<T>, value: T) {
        self.items[self.count] = value
        self.count += 1
    }
}

fn main() {
    bag := Bag<i32>.empty()
    bag.put(\"not an i32\")
}
"
        ),
        vec![dux_diag::code::TYPE_MISMATCH]
    );
}

#[test]
fn hash_answers_an_integer_and_nothing_else_adopts_it_for_free() {
    assert_eq!(
        codes(
            "fn main() {
    slot := hash(\"hp\")
    where: usize = slot % 8
    println($\"{}\", where)
}
"
        ),
        Vec::<&'static str>::new()
    );
    assert_eq!(
        codes(
            "fn main() {
    small: i8 = hash(\"hp\")
    println($\"{}\", small)
}
"
        ),
        vec![dux_diag::code::LOSSY_CONVERSION]
    );
}

#[test]
fn c_integers_are_their_own_types() {
    let source = "package main
extern \"C\" {
    fn take(n: c_int) -> c_int
}
fn main() {
    w: c_long = 40000
    plain: i32 = 1
    take(plain)
    big: i64 = w as i64
    back: c_ulong = big as c_ulong
}";
    let found = codes(source);
    assert!(
        found.contains(&dux_diag::code::LOSSY_CONVERSION),
        "an `i32` is not a `c_int` for free, even where widths agree (D18): {found:?}"
    );
}

#[test]
fn untyped_literals_adopt_the_c_type_the_declaration_names() {
    let adopting = "package main
extern \"C\" {
    fn take(n: c_int) -> c_int
    fn byte(b: c_uchar) -> c_uchar
}
fn main() {
    take(7)
    byte(255)
}";
    assert!(
        codes(adopting).is_empty(),
        "in-range literals fit: {:?}",
        codes(adopting)
    );

    let beyond = "package main
extern \"C\" {
    fn byte(b: c_uchar) -> c_uchar
}
fn main() { byte(256) }";
    assert!(
        codes(beyond).contains(&dux_diag::code::LITERAL_OUT_OF_RANGE),
        "256 does not reach a `c_uchar`"
    );
}

#[test]
fn c_text_literals_take_the_pointer_shape_the_slot_asks_for() {
    let source = "package main
extern \"C\" {
    fn open(path: *c_char) -> c_int
    fn time(what: Option<*c_long>) -> c_long
}
fn main() {
    open(c\"slot.bin\")
    time(None)
    time(c\"not a pointer slot\")
}";
    let found = codes(source);
    assert!(
        !found.contains(&dux_diag::code::LOSSY_CONVERSION)
            && !found.contains(&dux_diag::code::TYPE_MISMATCH),
        "`c\"..\"` is an address the program wrote down: {found:?}"
    );
}

#[test]
fn strings_cross_to_c_pointers_but_not_to_dux_ones() {
    let to_c = "package main
extern \"C\" {
    fn strlen(s: *c_char) -> usize
}
fn main() { text := \"dux\"; strlen(text) }";
    assert!(
        codes(to_c).is_empty(),
        "a `string` crosses as its bytes: {:?}",
        codes(to_c)
    );

    let to_dux = "package main
fn takes(p: *c_char) { }
fn main() { text := \"dux\"; takes(text) }";
    assert!(
        codes(to_dux).contains(&dux_diag::code::TYPE_MISMATCH),
        "away from the boundary a `string` is not a pointer"
    );
}

#[test]
fn arguments_past_a_c_declaration_are_the_callees_to_interpret() {
    let source = "package main
extern \"C\" {
    fn printf(fmt: *c_char) -> c_int
}
fn main() {
    printf(c\"%d %s\", 42, \"word\")
    printf(42)
}";
    let found = codes(source);
    assert!(
        !found.contains(&dux_diag::code::WRONG_ARITY),
        "the written list is a floor, not a ceiling: {found:?}"
    );
    assert!(
        found.contains(&dux_diag::code::LOSSY_CONVERSION)
            || found.contains(&dux_diag::code::TYPE_MISMATCH),
        "a fixed slot still checks: {found:?}"
    );
}

#[test]
fn tuples_check_element_wise_and_positionally() {
    let clean = "fn shift(pair: (i32, i64), by: i32) -> (i32, i64) {
    return (pair.0 + by, pair.1)
}

fn main() {
    wide: (i64, i64) = (1, 2)
    wide.1 = 9
    first := wide.0
    small := (1, true)
    second := small.1
    if second {
        wide.0 = first
    }
}";
    assert_eq!(codes(clean), Vec::<&'static str>::new());
}

#[test]
fn a_member_outside_the_tuple_is_said() {
    let source = "fn main() {
    pair := (1, 2)
    println($\"{}\", pair.5)
}";
    assert_eq!(codes(source), vec![dux_diag::code::TYPE_MISMATCH]);
}

#[test]
fn a_tuple_that_disagrees_with_its_annotation_is_said() {
    let source = "fn main() {
    wide: (i64, i64, i64) = (1, 2)
}";
    assert_eq!(codes(source), vec![dux_diag::code::TYPE_MISMATCH]);
}

#[test]
fn positional_access_walks_nothing_but_a_tuple() {
    let source = "fn main() {
    n := 1
    println($\"{}\", n.0)
}";
    assert_eq!(codes(source), vec![dux_diag::code::TYPE_MISMATCH]);
}

#[test]
fn a_question_mark_unwraps_what_it_walks() {
    let clean = "fn third(lo: i32, hi: i32) -> Result<i32, string> {
    if lo < hi {
        return Ok(lo + hi)
    }
    return Err(\"no\")
}

fn outer(a: i32, b: i32) -> Result<i32, string> {
    first := third(a, b)?
    return Ok(first)
}

fn half(v: i32) -> Option<i32> {
    if v == 0 {
        return None
    }
    return Some(v)
}

fn chain(v: i32) -> Option<i32> {
    inner := half(v)?
    return Some(inner)
}

fn main() {
    one := outer(1, 2)
    two := chain(3)
}";
    assert_eq!(codes(clean), Vec::<&'static str>::new());
}

#[test]
fn a_question_mark_outside_its_function_is_said() {
    let source = "fn fails() -> Result<i32, string> {
    return Err(\"no\")
}

fn main() {
    x := fails()?
}";
    assert_eq!(codes(source), vec![dux_diag::code::TYPE_MISMATCH]);
}

#[test]
fn a_question_mark_walks_nothing_else() {
    let source = "fn main() -> i32 {
    n := 1
    return n?
}";
    assert_eq!(codes(source), vec![dux_diag::code::TYPE_MISMATCH]);
}

#[test]
fn one_question_hands_back_one_error_type() {
    let source = "fn text_failure() -> Result<i32, string> {
    return Err(\"no\")
}

fn number_failure() -> Result<i32, i32> {
    return text_failure()?
}

fn main() {
    one := number_failure()
}";
    assert_eq!(codes(source), vec![dux_diag::code::TYPE_MISMATCH]);
}
