use dux_cinterop::{spell, spell_report, CDecl, CType, Import as CImport};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

fn mini() -> Vec<CDecl> {
    seen().decls
}

fn seen() -> dux_cinterop::Seen {
    dux_cinterop::import(&CImport {
        header: "mini.h".to_string(),
        flags: Vec::new(),
        include_paths: vec![FIXTURES.to_string()],
    })
    .expect("the fixture parses")
}

fn function<'a>(decls: &'a [CDecl], named: &str) -> &'a dux_cinterop::CFunction {
    decls
        .iter()
        .find_map(|decl| match decl {
            CDecl::Function(f) if f.name == named => Some(f),
            _ => None,
        })
        .unwrap_or_else(|| panic!("`{named}` should be imported"))
}

#[test]
fn functions_arrive_with_their_shapes() {
    let decls = mini();
    let add = function(&decls, "dux_add");
    assert_eq!(
        add.params,
        vec![("a".to_string(), CType::Int), ("b".to_string(), CType::Int)]
    );
    assert_eq!(add.returns, Some(CType::Int));
    assert!(!add.variadic);

    let sum = function(&decls, "dux_sum");
    assert!(sum.variadic, "`...` is part of the shape");
    assert_eq!(sum.returns, Some(CType::Double));

    let twice = function(&decls, "dux_twice");
    assert!(
        twice.header_definition,
        "`static inline` says where the definition lives"
    );
}

#[test]
fn layout_is_read_never_rederived() {
    let decls = mini();
    let point = decls
        .iter()
        .find_map(|decl| match decl {
            CDecl::Struct(s) if s.name == "point" => Some(s),
            _ => None,
        })
        .expect("`point` is imported");
    assert_eq!(point.size, Some(8));
    assert_eq!(point.align, Some(4));
    let offsets: Vec<(&str, Option<u64>)> = point
        .fields
        .iter()
        .map(|field| (field.name.as_str(), field.offset))
        .collect();
    assert_eq!(offsets, vec![("x", Some(0)), ("y", Some(4))]);

    let rect = decls
        .iter()
        .find_map(|decl| match decl {
            CDecl::Struct(s) if s.name == "rect" => Some(s),
            _ => None,
        })
        .expect("`rect` is imported");
    assert_eq!(rect.size, Some(16));
    assert_eq!(rect.fields[0].ty, CType::Struct("point".to_string()));
}

#[test]
fn the_opaque_struct_stays_behind_a_pointer() {
    let decls = mini();
    let hidden = decls
        .iter()
        .find_map(|decl| match decl {
            CDecl::Struct(s) if s.name == "hidden" => Some(s),
            _ => None,
        })
        .expect("the forward declaration is seen");
    assert_eq!(hidden.size, None, "clang cannot size it, so nobody can");
    let hide = function(&decls, "dux_hide");
    assert!(matches!(
        &hide.params[0].1,
        CType::Pointer(target) if matches!(**target, CType::Typedef(_, _))
    ));
}

#[test]
fn enums_carry_their_integer_type_and_values() {
    let decls = mini();
    let color = decls
        .iter()
        .find_map(|decl| match decl {
            CDecl::Enum(e) if e.name == "color" => Some(e),
            _ => None,
        })
        .expect("`color` is imported");
    // clang picks the first of `int`/`unsigned` that holds every value;
    // this enum has none negative, so the C truth is `unsigned`.
    assert_eq!(color.int, CType::UInt);
    let values: Vec<(&str, i64)> = color
        .constants
        .iter()
        .map(|(name, value, _)| (name.as_str(), *value))
        .collect();
    assert_eq!(values, vec![("RED", 0), ("GREEN", 7), ("BLUE", 8)]);
}

#[test]
fn the_mapping_table_crosses_what_it_promises() {
    let text = spell("mini", &mini());
    assert!(
        text.contains("fn dux_add(a: c_int, b: c_int) -> c_int"),
        "{text}"
    );
    assert!(
        text.contains("fn dux_name() -> Option<*c_char>"),
        "a C `char *` arrives as maybe-nothing:\n{text}"
    );
    assert!(
        text.contains("fn dux_sum(count: c_int, ) -> c_double")
            || text.contains("fn dux_sum(count: c_int,) -> c_double")
            || text.contains("fn dux_sum(count: c_int) -> c_double")
            || text.contains("dux_sum(count: c_int, ...)")
            || text.contains("fn dux_sum(count: c_int, "),
        "the written list is what Dux declares:\n{text}"
    );
    assert!(text.contains("const DUX_MAX: i32 = 64"), "{text}");
    assert!(text.contains("const DUX_PAGES: i32 = 4096"), "{text}");
    assert!(text.contains("const DUX_TAG: string = \"dux\""), "{text}");
    assert!(text.contains("const DUX_RATIO: f64 = 1.5"), "{text}");
    assert!(
        !text.contains("DUX_DOUBLED"),
        "function-like macros wait:\n{text}"
    );
    assert!(text.contains("type time_like = c_long"), "{text}");
    assert!(text.contains("struct point {"), "{text}");
    assert!(
        text.contains("struct bits {}"),
        "a record with a bitfield crosses by name and keeps its body to itself:\n{text}"
    );
    assert!(!text.contains("union score"), "unions wait:\n{text}");
    assert!(
        text.contains("struct hidden {}"),
        "an opaque record gets the empty body it deserves:\n{text}"
    );
    assert!(
        text.contains("type hidden_t = hidden"),
        "the typedef names a record that now exists:\n{text}"
    );
    assert!(
        !text.contains("dux_counter"),
        "extern variables wait:\n{text}"
    );
}

#[test]
fn long_double_crosses_as_c_long_double() {
    let decls = mini();
    let wide = function(&decls, "dux_wide");
    assert!(
        matches!(wide.returns, Some(CType::LongDouble)),
        "clang says `long double` and the walk keeps it"
    );
    let text = spell("mini", &decls);
    assert!(
        text.contains(
            "fn dux_wide(x: c_long_double, spare: Option<*c_long_double>) -> c_long_double"
        ),
        "C's widest float arrives whole, never disguised as a double:\n{text}"
    );
}

#[test]
fn static_inline_reaches_the_module_through_a_wrapper() {
    let decls = mini();
    let text = spell("mini", &decls);
    assert!(
        text.contains("#[c_name(\"__duxw_mini_dux_twice\")] fn dux_twice(x: c_int) -> c_int"),
        "the declaration binds to the doorway, not the private name:\n{text}"
    );
    let source = dux_cinterop::c_source("mini", &["mini.h".to_string()], &decls);
    assert!(
        source.contains("int __duxw_mini_dux_twice(int __p0) { return (dux_twice)(__p0); }"),
        "the wrapper is what clang compiles:\n{source}"
    );
    assert!(
        !source.contains("__duxw_mini_dux_add"),
        "a function with a library behind it needs no doorway:\n{source}"
    );
}

#[test]
fn a_real_header_walks() {
    let decls = dux_cinterop::import(&CImport {
        header: "time.h".to_string(),
        flags: Vec::new(),
        include_paths: Vec::new(),
    })
    .expect("`time.h` parses")
    .decls;
    let time = function(&decls, "time");
    assert!(matches!(&time.params[0].1, CType::Pointer(_)));
    let spelled = spell("mini", &decls);
    assert!(
        spelled.contains("fn time("),
        "the declaration reads as Dux:\n{spelled}"
    );
    assert!(
        spelled.contains("type time_t = c_long"),
        "a typedef of `long` crosses as `c_long`, not `i64`:\n{spelled}"
    );
}

#[test]
fn stdio_is_variadic_and_size_t_means_usize() {
    let decls = dux_cinterop::import(&CImport {
        header: "stdio.h".to_string(),
        flags: Vec::new(),
        include_paths: Vec::new(),
    })
    .expect("`stdio.h` parses")
    .decls;
    let printf = function(&decls, "printf");
    assert!(printf.variadic);
    let spelled = spell("mini", &decls);
    assert!(spelled.contains("fn printf("), "{spelled}");
    assert!(
        !spelled.contains("type size_t"),
        "`size_t` means `usize`; there is nothing left to say:\n{spelled}"
    );
    assert!(
        !spelled.contains("struct _IO_FILE"),
        "FILE stays opaque:\n{spelled}"
    );
}

#[test]
fn nothing_that_did_not_cross_is_quietly_waiting() {
    let mut missed = seen().skipped;
    missed.extend(spell_report("mini", &mini()).1);
    let named = |needle: &str| -> bool { missed.iter().any(|skip| skip.name == needle) };
    assert!(named("score"), "the union is named as skipped: {missed:?}");
    assert!(
        missed
            .iter()
            .any(|skip| skip.name == "score" && skip.reason.contains("union")),
        "and it says why: {missed:?}"
    );
    assert!(
        named("DUX_DOUBLED"),
        "a function-like macro is named, not hidden: {missed:?}"
    );
    assert!(
        named("dux_counter"),
        "a global variable is named, not hidden: {missed:?}"
    );
}

fn hostile(header: &str, flags: Vec<String>) -> (Vec<CDecl>, Vec<dux_cinterop::Skipped>) {
    let seen = dux_cinterop::import(&CImport {
        header: header.to_string(),
        flags,
        include_paths: Vec::new(),
    })
    .unwrap_or_else(|failure| panic!("`{header}` must parse: {failure:?}"));
    let (spelled, mut missed, crossed) = spell_report("mini", &seen.decls);
    assert!(
        !crossed.is_empty(),
        "`{header}` must bring something across"
    );
    assert!(
        !spelled.contains("long double"),
        "`long double` never sneaks through pretending to be `c_double`:\n{spelled}"
    );
    missed.extend(seen.skipped);
    (seen.decls, missed)
}

#[test]
fn the_hostile_headers_cross_what_they_can_and_say_what_they_cannot() {
    for header in ["pthread.h", "time.h", "sqlite3.h", "SDL2/SDL.h", "GL/gl.h"] {
        let (decls, missed) = hostile(header, Vec::new());
        assert!(
            !decls.is_empty(),
            "`{header}` yielded no declarations at all"
        );
        for skip in &missed {
            assert!(!skip.name.is_empty(), "`{header}` skipped an unnamed name");
            assert!(
                !skip.reason.is_empty(),
                "`{}` was skipped without a reason",
                skip.name
            );
        }
    }
}
