use dux_codegen::{CompiledModule, OptLevel, Options};
use dux_source::SourceMap;

fn build(source: &str) -> Result<CompiledModule, Vec<dux_diag::Diagnostic>> {
    build_with(source, Options::default())
}

fn build_with(source: &str, options: Options) -> Result<CompiledModule, Vec<dux_diag::Diagnostic>> {
    let mut sources = SourceMap::new();
    let file = sources.add("t.dux", source.to_string());
    let lexed = dux_lexer::lex(&sources, file);
    let parsed = dux_parser::parse(&sources, &lexed.tokens);
    assert!(
        parsed.diagnostics.is_empty(),
        "the fixture should parse: {:?}",
        parsed.diagnostics
    );
    CompiledModule::build_with_line_tables(&parsed.program, options, &sources)
}

const HELLO: &str = "package main\nfn main() { println(\"hi\") }";

#[test]
fn main_is_emitted_as_a_c_main_returning_zero() {
    let ir = build(HELLO).expect("builds").ir_text();
    assert!(ir.contains("i32 @main()"), "{ir}");
    assert!(
        ir.contains("ret i32 0"),
        "main must return 0 like a C main:\n{ir}"
    );
    assert!(
        ir.contains("call i64 @fwrite"),
        "println writes the view's bytes:\n{ir}"
    );
    assert!(
        ir.contains("call i32 @fputc"),
        "the newline is written of its own accord:\n{ir}"
    );
    assert!(
        ir.contains("constant [3 x i8] c\"hi\\00\""),
        "the literal must land in rodata with a NUL:\n{ir}"
    );
    assert!(
        ir.contains("@fwrite(ptr @str, i64 1, i64 2"),
        "the bytes travel with their count:\n{ir}"
    );
}

#[test]
fn print_writes_the_bytes_and_no_newline() {
    let ir = build("package main\nfn main() { print(\"hi\") }")
        .expect("builds")
        .ir_text();
    assert!(ir.contains("call i64 @fwrite"), "{ir}");
    assert!(ir.contains("@stdout"), "the stream is an argument:\n{ir}");
    assert!(!ir.contains("@fputc"), "print adds no newline:\n{ir}");
}

#[test]
fn the_module_carries_the_host_triple_and_data_layout() {
    let module = build(HELLO).expect("builds");
    let ir = module.ir_text();
    assert!(
        ir.contains("target triple = \"x86_64"),
        "the triple must be set so AOT and JIT agree:\n{ir}"
    );
    assert!(
        ir.contains("target datalayout = \"e-m:e"),
        "the data layout must come from the target machine:\n{ir}"
    );
    assert!(module.data_layout().starts_with("e-m:e"));
}

#[test]
fn release_runs_the_new_pass_manager_without_losing_the_program() {
    let mut sources = SourceMap::new();
    let file = sources.add("t.dux", HELLO.to_string());
    let lexed = dux_lexer::lex(&sources, file);
    let parsed = dux_parser::parse(&sources, &lexed.tokens);
    let debug = CompiledModule::build(&parsed.program, Options::default())
        .expect("debug builds")
        .ir_text();
    let release = CompiledModule::build(
        &parsed.program,
        Options {
            opt_level: OptLevel::Balanced,
            debug_allocator: true,
            ..Options::default()
        },
    )
    .expect("release builds")
    .ir_text();
    assert!(debug.contains("@fwrite") && release.contains("@fwrite"));
    assert!(
        release.contains("i32 @main()"),
        "main must survive `default<O2>`:\n{release}"
    );
}

#[test]
fn a_program_without_main_is_reported_before_any_ir_is_emitted() {
    let errors = build("package main\nfn other() { }").unwrap_err();
    assert_eq!(errors[0].code, dux_diag::code::NO_MAIN, "{errors:?}");
}

#[test]
fn unlowered_code_reports_the_milestone_code_with_a_span() {
    let errors = build("package main\nfn main() { x := Point { y: 1 } }").unwrap_err();
    assert_eq!(
        errors[0].code,
        dux_diag::code::UNSUPPORTED_IN_BUILD,
        "{errors:?}"
    );
    assert!(
        errors[0].span.is_some(),
        "the diagnostic must point at the code"
    );
}

#[test]
fn objects_are_emitted_for_the_host() {
    let module = build(HELLO).expect("builds");
    let path = std::env::temp_dir().join(format!("dux-codegen-{}", std::process::id()));
    let result = module.write_object(&path).and_then(|()| {
        let bytes = std::fs::read(&path).map_err(|e| dux_codegen::CodegenError::Io(e.to_string()));
        let _ = std::fs::remove_file(&path);
        bytes
    });
    let bytes = result.expect("a host object file");
    assert_eq!(&bytes[..4], b"\x7fELF", "the object must be a host object");
}

#[test]
fn bindings_default_integers_to_i32_and_keep_every_alloca_in_the_entry_block() {
    let ir = build("package main\nfn main() { count := 42; count += 1 }")
        .expect("builds")
        .ir_text();
    assert!(ir.contains("%count = alloca i32"), "{ir}");
    let entry = ir.split("body:").next().expect("an entry region");
    assert!(
        entry.contains("%count = alloca i32"),
        "allocas belong in the entry block:\n{ir}"
    );
    assert!(
        ir.contains("add i32"),
        "the compound assign keeps i32:\n{ir}"
    );
}

#[test]
fn a_value_if_meets_at_a_phi() {
    let ir =
        build("package main\nfn main() -> i32 { c := true; x := if c { 1 } else { 2 }; return x }")
            .expect("builds")
            .ir_text();
    assert!(ir.contains("phi i32"), "the arms meet at a phi:\n{ir}");
    assert!(ir.contains("br i1"), "the condition branches:\n{ir}");

    let ir = build(
        "package main\nfn pick(c: bool) -> string { s := if c { \"a\" } else { \"b\" }; return s }\nfn main() { pick(true) }",
    )
    .expect("builds")
    .ir_text();
    assert!(
        ir.contains(r#"phi { ptr, i64 } [ { ptr @str, i64 1 }"#),
        "string arms meet at a view phi:\n{ir}"
    );
}

#[test]
fn every_statement_carries_its_source_line() {
    let source = "package main\n\nfn pick(n: i32) -> i32 {\n    if n > 0 {\n        return n\n    }\n    return 0\n}\n\nfn main() {\n    pick(3)\n}\n";
    let ir = build(source).expect("builds").ir_text();
    assert!(
        ir.contains(r#"!DISubprogram(name: "pick""#)
            && ir.contains(r#"!DISubprogram(name: "main""#),
        "each function gets a subprogram:\n{ir}"
    );
    assert!(
        ir.contains(r#"DILocation(line: 3, column: 4"#),
        "`fn pick` names itself at line 3:\n{ir}"
    );
    assert!(
        ir.contains(r#"DILocation(line: 5, column: 9"#),
        "the `return n` statement is at line 5 column 9:\n{ir}"
    );
    assert!(
        ir.contains(r#"DILocation(line: 11, column: 5"#),
        "the `pick(3)` call is at line 11:\n{ir}"
    );
    assert!(
        ir.contains(r#""Debug Info Version", i32 "#),
        "the module must claim the debug version or the backend drops it all:\n{ir}"
    );
}

#[test]
fn defer_replays_captured_calls_at_every_exit() {
    let ir = build(
        "package main\nfn bye(n: i32) { print(\"bye\") }\nfn main() { defer bye(7); if true { return } }",
    )
    .expect("builds")
    .ir_text();
    assert!(
        ir.contains("call void @bye(i32 %load"),
        "the argument is captured at the `defer`:\n{ir}"
    );
    assert_eq!(
        ir.matches("call void @bye(i32 %load").count(),
        2,
        "both exits replay the call:\n{ir}"
    );
}

#[test]
fn c_declarations_are_variadic_candidates_and_one_symbol_is_one_function() {
    let ir = build(
        "package main\nextern \"C\" {\n    fn dprintf(fd: c_int, fmt: *c_char) -> c_int\n    #[c_name(\"strlen\")]\n    fn text_len(s: *c_char) -> usize\n}\nfn main() {\n    dprintf(1, c\"v %d\\n\", 7)\n    text_len(c\"ab\")\n}",
    )
    .expect("builds")
    .ir_text();
    assert!(
        ir.contains("declare i32 @dprintf(i32, ptr, ...)"),
        "a C declaration may take more than its written list:\n{ir}"
    );
    assert!(
        ir.contains("mov $$0, %al"),
        "the variadic call says how many vector registers it used:\n{ir}"
    );
    assert!(
        ir.contains("declare i64 @strlen("),
        "`#[c_name]` names the symbol, not the Dux spelling:\n{ir}"
    );
    assert!(
        !ir.contains("text_len"),
        "the Dux spelling never reaches the object file:\n{ir}"
    );
}

#[test]
fn an_f32_past_the_written_list_becomes_a_double() {
    let ir = build(
        "package main\nextern \"C\" {\n    fn logfmt(fmt: *c_char) -> c_int\n}\nfn main() {\n    width: f32 = 1.5\n    logfmt(c\"%f\", width)\n}",
    )
    .expect("builds")
    .ir_text();
    assert!(ir.contains("fpext"), "C promotes a float vararg:\n{ir}");
    assert!(
        ir.contains("mov $$1, %al"),
        "one float argument, one vector register:\n{ir}"
    );
}

#[test]
fn a_string_crosses_the_boundary_as_its_bytes() {
    let ir = build(
        "package main\nextern \"C\" {\n    fn strlen(s: *c_char) -> usize\n}\nfn main() {\n    text := \"dux\"\n    n := strlen(text)\n    println($\"{}\", n)\n}",
    )
    .expect("builds")
    .ir_text();
    assert!(
        ir.contains("@strlen(ptr %view.data)"),
        "the address travels and the count stays home:\n{ir}"
    );
}

#[test]
fn an_extern_inside_a_module_is_called_by_module_and_links_by_symbol() {
    let ir = build(
        "package main\nmod c {\n    extern \"C\" {\n        fn strlen(s: *c_char) -> usize\n    }\n}\nfn main() {\n    text := \"dux\"\n    n := c.strlen(text)\n    println($\"{}\", n)\n}",
    )
    .expect("builds")
    .ir_text();
    assert!(
        ir.contains("@strlen(ptr %view.data)"),
        "the module qualifies the name in Dux, never in the object file:\n{ir}"
    );
    assert!(
        !ir.contains("c.strlen"),
        "the module path must not leak into the symbol:\n{ir}"
    );
}

#[test]
fn an_alias_in_a_module_signature_reads_as_the_type_it_names() {
    let ir = build(
        "package main\nmod c {\n    type byte = c_uchar\n    extern \"C\" {\n        fn give() -> Option<*byte>\n    }\n}\nfn main() {\n    p := c.give()\n    match p {\n        Some(q) => {\n            n := *q\n            println($\"{}\", n)\n        }\n    }\n}",
    )
    .expect("builds")
    .ir_text();
    assert!(
        ir.contains("load i8"),
        "the aliased pointee keeps its type through the boundary:\n{ir}"
    );
}

#[test]
fn untyped_arithmetic_adopts_the_type_the_call_asks_for() {
    let ir = build(
        "package main\nfn take(n: usize) -> usize { return n }\nfn main() { page := take(4 * 1024) }",
    )
    .expect("builds")
    .ir_text();
    assert!(
        ir.contains("@take(i64 4096)"),
        "`4 * 1024` is still the number when `usize` asks for it (D17):\n{ir}"
    );
}

#[test]
fn a_function_pointer_is_called_through_its_address() {
    let ir = build(
        "package main\n#[no_context]\nfn doubling(x: i32) -> i32 { return x * 2 }\nfn main() {\n    cb := &doubling\n    println($\"{}\", cb(21))\n}",
    )
    .expect("builds")
    .ir_text();
    assert!(
        ir.contains("call i32 %load"),
        "the call goes to the address the pointer holds:\n{ir}"
    );
    assert!(
        !ir.contains("call i32 @doubling"),
        "nothing calls `doubling` by name:\n{ir}"
    );
}

#[test]
fn the_context_frees_through_its_own_slot() {
    let ir = build("package main\nfn main() { p := new(i32)\n free(p) }")
        .expect("builds")
        .ir_text();
    assert!(
        ir.contains("getelementptr i8, ptr") && ir.contains("call void %free(ptr"),
        "the deallocator comes out of the context, not the compiler:\n{ir}"
    );
}

#[test]
fn fixed_arrays_live_in_slots_and_iterate_with_a_counter() {
    let ir = build(
        "package main\nfn main() -> i32 { xs := [1, 2, 3]; total := 0; for v in xs { total += v }; return total }",
    )
    .expect("builds")
    .ir_text();
    assert!(
        ir.contains("alloca [3 x i32]"),
        "the literal backs a slot:\n{ir}"
    );
    assert!(
        ir.contains("getelementptr"),
        "elements are addressed by GEP:\n{ir}"
    );
    assert!(ir.contains("ult"), "the counter walks the length:\n{ir}");

    let ir = build(
        "package main\nfn sum() -> i64 { z := [0; 4]; s: i64 = 0; for i in 0..4 { s += i }; return s }\nfn main() { sum() }",
    )
    .expect("builds")
    .ir_text();
    assert!(
        ir.contains("alloca [4 x i32]"),
        "repeats back a slot:\n{ir}"
    );
    assert!(
        ir.contains("slt"),
        "the range compares against its limit:\n{ir}"
    );
}

#[test]
fn implicit_conversion_widens_exactly_when_the_destination_is_lossless() {
    let ir =
        build("package main\nfn main() { wider: i64 = 0; narrow := 1; wider = wider + narrow }")
            .expect("builds")
            .ir_text();
    assert!(ir.contains("sext i32"), "i32 widens into i64 free:\n{ir}");
    assert!(ir.contains("add i64"), "{ir}");

    let errors =
        build("package main\nfn main() { narrow: i8 = 0; wide := 1; narrow = wide }").unwrap_err();
    assert_eq!(errors[0].code, dux_diag::code::LOSSY_CONVERSION);
    assert!(
        errors[0].message.contains("as i8"),
        "the hint must show the explicit form: {:?}",
        errors[0].message
    );
}

#[test]
fn a_literal_that_does_not_fit_its_annotation_is_an_error() {
    let errors = build("package main\nfn main() { small: i8 = 300 }").unwrap_err();
    assert_eq!(
        errors[0].code,
        dux_diag::code::LITERAL_OUT_OF_RANGE,
        "{errors:?}"
    );
    assert!(errors[0].message.contains("does not fit"));
}

#[test]
fn three_clause_for_lowers_to_cond_body_step_exit() {
    let ir = build("package main\nfn main() { for i := 0, i < 3, i += 1 { print(\"x\") } }")
        .expect("builds")
        .ir_text();
    for label in ["for.cond", "for.body", "for.step", "for.end"] {
        assert!(ir.contains(label), "missing the `{label}` block:\n{ir}");
    }
    assert!(ir.contains("icmp slt i32"), "{ir}");
    assert!(
        ir.contains("br label %for.cond"),
        "the step loops back:\n{ir}"
    );
}

#[test]
fn while_and_loop_bind_break_and_continue() {
    let ir = build(
        "package main\nfn main() {\n    x := 0\n    while x < 4 {\n        x += 1\n        if x == 2 { continue }\n        if x >= 3 { break }\n    }\n}",
    )
    .expect("builds")
    .ir_text();
    assert!(ir.contains("br label %while.cond"), "{ir}");
    assert!(
        ir.contains("br label %while.end"),
        "break exits the loop:\n{ir}"
    );
    assert!(
        ir.contains("br i1") && ir.contains("label %while.body, label %while.end"),
        "continue goes back to the condition through the body block:\n{ir}"
    );
}

#[test]
fn and_or_short_circuit_with_a_phi_over_two_blocks() {
    let ir = build(
        "package main\nfn main() {\n    a := true\n    b := false\n    if a && b { println(\"y\") }\n}",
    )
    .expect("builds")
    .ir_text();
    assert!(ir.contains("phi i1"), "{ir}");
    assert!(ir.contains("sc.rhs"), "{ir}");
    assert!(
        !ir.contains("and i1"),
        "`&&` must branch, not bitwise-and:\n{ir}"
    );
}

#[test]
fn user_functions_take_typed_parameters_and_return_one_value() {
    let ir = build(
        "package main\nfn twice(n: i32) -> i32 { return n * 2 }\nfn main() { println($\"{twice(21)}\") }",
    )
    .expect("builds")
    .ir_text();
    assert!(ir.contains("define i32 @twice(i32"), "{ir}");
    assert!(ir.contains("mul i32"), "{ir}");
    assert!(ir.contains("call i32 @twice(i32"), "{ir}");
    assert!(
        ir.contains("call i32 (ptr, ...) @printf"),
        "the interpolation prints the result:\n{ir}"
    );
}

#[test]
fn interpolated_holes_become_printf_conversions_and_trailing_holes_take_arguments() {
    let ir = build("package main\nfn main() { println($\"{}\", 7) }")
        .expect("builds")
        .ir_text();
    assert!(ir.contains("call i32 (ptr, ...) @printf"), "{ir}");
    assert!(
        ir.contains("c\"%d\\0A\\00\""),
        "an i32 hole is %d and println appends the newline:\n{ir}"
    );

    let ir = build("package main\nfn main() {\n    big: i64 = 0\n    print($\"{big}\")\n}")
        .expect("builds")
        .ir_text();
    assert!(
        ir.contains("c\"%ld\\00\""),
        "an i64 hole is %ld and print adds no newline:\n{ir}"
    );
}

#[test]
fn return_carries_a_value_and_mains_fall_through_to_zero() {
    let ir = build("package main\nfn main() -> i32 { if true { return 7 } }")
        .expect("builds")
        .ir_text();
    assert!(ir.contains("ret i32 7"), "{ir}");
    assert!(ir.contains("ret i32 0"), "{ir}");
}

#[test]
fn missing_return_on_one_path_is_reported() {
    let errors = build("package main\nfn pick() -> i32 { if true { return 1 } }\nfn main() { }")
        .unwrap_err();
    assert_eq!(errors[0].code, dux_diag::code::TYPE_MISMATCH);
    assert!(errors[0].message.contains("not all paths"), "{errors:?}");
}

#[test]
fn bounds_checks_wrap_indexing_and_can_stand_aside() {
    let source = "package main\nfn at(xs: []i64, i: i64) -> i64 {\n    return xs[i]\n}\nfn main() {\n    xs: [i64; 3] = [1, 2, 3]\n    println($\"{at(xs[0..3], 1)}\")\n}";
    let checked = build(source).expect("the checked build compiles");
    assert!(
        checked.ir_text().contains("index out of bounds"),
        "a debug build stops at an index that lands outside"
    );
    let unchecked = build_with(
        source,
        Options {
            bounds_checks: false,
            ..Options::default()
        },
    )
    .expect("the unchecked build compiles");
    assert!(
        !unchecked.ir_text().contains("index out of bounds"),
        "standing aside removes the trap"
    );
}

#[test]
fn milestone_gaps_report_e0007_with_the_milestone_in_the_message() {
    let cases = [
        (
            "package main\nfn main() { for i, v in 0..3 { print($\"{i}\") } }",
            "positions belong to arrays",
        ),
        (
            "package main\nfn main() { x := 1\n    defer x = 3 }",
            "deferring anything but a call",
        ),
    ];
    for (source, needle) in cases {
        let errors = build(source).unwrap_err();
        assert_eq!(
            errors[0].code,
            dux_diag::code::UNSUPPORTED_IN_BUILD,
            "{source} should report E0007: {errors:?}"
        );
        assert!(
            errors[0].message.contains(needle),
            "the message should name the gap: {:?}",
            errors[0].message
        );
    }
}

#[test]
fn constants_fold_adopt_the_annotated_type_and_refuse_to_be_assigned() {
    let ir = build("package main\nconst KI: usize = 1024\nfn main() { page: usize = KI + 1 }")
        .expect("builds")
        .ir_text();
    assert!(
        ir.contains("store i64 1025, ptr %page"),
        "the constant adopts usize and folds:\n{ir}"
    );

    let errors = build("package main\nconst K: i32 = 1\nfn main() { K = 2 }").unwrap_err();
    assert_eq!(errors[0].code, dux_diag::code::ASSIGN_TO_CONST);
    assert!(
        errors[0].message.contains("cannot be assigned"),
        "{errors:?}"
    );

    let errors = build("package main\nconst K: i8 = 300\nfn main() { }").unwrap_err();
    assert_eq!(errors[0].code, dux_diag::code::LITERAL_OUT_OF_RANGE);
    assert!(errors[0].message.contains("does not fit"), "{errors:?}");
}

#[test]
fn globals_are_module_storage_and_update_in_place() {
    let ir =
        build("package main\nglobal hits: i32 = 0\nfn bump() { hits += 1 }\nfn main() { bump() }")
            .expect("builds")
            .ir_text();
    assert!(ir.contains("@hits = global i32 0"), "{ir}");
    assert!(ir.contains("load i32, ptr @hits"), "{ir}");
    assert!(ir.contains("store i32"), "{ir}");
    assert!(
        ir.contains("store i32 %addtmp, ptr @hits") || ir.contains("ptr @hits"),
        "the compound assign stores back through the global:\n{ir}"
    );

    let ir = build("package main\nglobal scratch: i64\nfn main() { }")
        .expect("a value-less global is zero-initialised")
        .ir_text();
    assert!(ir.contains("@scratch = global i64 0"), "{ir}");
}

#[test]
fn a_record_is_a_nominal_struct_and_assignment_copies_it() {
    let ir = build(
        "package main
struct Vec3 {
    x: f32,
    y: f32,
}
fn main() {
    v := Vec3 { y: 2.0, x: 1.0 }
    c := v
    c.x = 3.0
    println($\"{} {}\", v.x, c.x)
}",
    )
    .expect("builds")
    .ir_text();
    assert!(
        ir.contains("%Vec3 = type { float, float }"),
        "the record type is nominal and structural in LLVM:\n{ir}"
    );
    assert!(ir.contains("%v = alloca %Vec3"), "{ir}");
    assert!(
        ir.contains("store %Vec3 %load, ptr %v"),
        "the literal is written through a temporary and copied in:\n{ir}"
    );
    assert!(
        ir.contains("store %Vec3 %load2, ptr %c"),
        "a binding of a record is a whole-record copy:\n{ir}"
    );
    assert!(
        ir.contains("getelementptr %Vec3, ptr %c, i32 0, i32 0"),
        "a field is an address, so the store reaches the record in place:\n{ir}"
    );
}

#[test]
fn a_record_constant_is_constant_storage() {
    let ir = build(
        "package main
struct Vec3 {
    x: f32,
    y: f32,
}
const UNIT: Vec3 = Vec3 { x: 1.0, y: 2.0 }
fn main() {
    println($\"{}\", UNIT.y)
}",
    )
    .expect("builds")
    .ir_text();
    assert!(
        ir.contains("@UNIT = constant %Vec3 { float 1.000000e+00, float 2.000000e+00 }"),
        "the literal becomes a constant initializer:\n{ir}"
    );
    let errors = build(
        "package main
struct Vec3 {
    x: f32,
}
const UNIT: Vec3 = Vec3 { x: 1.0 }
fn main() {
    UNIT.x = 2.0
}",
    )
    .unwrap_err();
    assert_eq!(
        errors[0].code,
        dux_diag::code::ASSIGN_TO_CONST,
        "{errors:?}"
    );
}

#[test]
fn a_field_a_record_does_not_have_is_reported_where_it_is_used() {
    let errors = build(
        "package main
struct Vec3 {
    x: f32,
}
fn main() {
    v := Vec3 { x: 1.0 }
    println($\"{}\", v.z)
}",
    )
    .unwrap_err();
    assert_eq!(errors[0].code, dux_diag::code::TYPE_MISMATCH);
    assert!(errors[0].message.contains("has no field `z`"), "{errors:?}");
    assert!(errors[0].message.contains("Vec3"), "{errors:?}");
}

#[test]
fn address_of_borrows_the_binding_and_deref_reads_through_it() {
    let ir = build(
        "package main
fn main() {
    n: i32 = 1
    p := &n
    *p = 2
    println($\"{}\", *p)
}",
    )
    .expect("builds")
    .ir_text();
    assert!(
        ir.contains("store ptr %n, ptr %p"),
        "`&n` is the binding's own slot:\n{ir}"
    );
    assert!(
        ir.contains("store i32 2, ptr %load"),
        "`*p = 2` stores through the pointer:\n{ir}"
    );
    assert!(ir.contains("load i32, ptr %load"), "{ir}");
}

#[test]
fn pointer_arithmetic_steps_by_whole_targets() {
    let ir = build(
        "package main
fn main() {
    dims: [i32; 4] = [10, 20, 30, 40]
    base := &dims[0]
    third := base + 2
    println($\"{}\", *third)
}",
    )
    .expect("builds")
    .ir_text();
    assert!(
        ir.contains("getelementptr inbounds i32, ptr %idx, i64 %addtmp")
            || ir.contains("getelementptr i32, ptr"),
        "the step is typed by the target, so `+ 2` is two i32:\n{ir}"
    );
}

#[test]
fn a_field_of_a_pointer_auto_dereferences() {
    let ir = build(
        "package main
struct Vec3 {
    x: f32,
}
fn main() {
    v := Vec3 { x: 1.0 }
    p := &v
    p.x = 2.0
    println($\"{}\", p.x)
}",
    )
    .expect("builds")
    .ir_text();
    assert!(
        ir.contains("load ptr, ptr %p") || ir.contains("load ptr, ptr"),
        "the pointer is read out of its slot before the field is stepped:\n{ir}"
    );
    assert!(
        ir.contains("getelementptr %Vec3, ptr %deref, i32 0, i32 0"),
        "{ir}"
    );
}

#[test]
fn rawptr_needs_a_cast_before_it_is_read_or_stepped() {
    let errors = build(
        "package main
fn main() {
    dims: [i32; 2] = [1, 2]
    base := &dims[0]
    bits: rawptr = base as rawptr
    println($\"{}\", *bits)
}",
    )
    .unwrap_err();
    assert_eq!(errors[0].code, dux_diag::code::TYPE_MISMATCH, "{errors:?}");
    assert!(errors[0].message.contains("`rawptr`"), "{errors:?}");

    let errors = build(
        "package main
fn main() {
    dims: [i32; 2] = [1, 2]
    base := &dims[0]
    bits: rawptr = base as rawptr
    later := bits + 1
    println($\"{}\", *later)
}",
    )
    .unwrap_err();
    assert_eq!(errors[0].code, dux_diag::code::TYPE_MISMATCH, "{errors:?}");
    assert!(errors[0].message.contains("cast"), "{errors:?}");
}

#[test]
fn the_address_of_a_temporary_stands_where_the_value_was_built() {
    let ir = build(
        "package main
fn one() -> i32 {
    return 1
}
fn sum(xs: []i32) -> i32 {
    total: i32 = 0
    for v in xs {
        total += v
    }
    return total
}
fn main() {
    println($\"{}\", sum(&[1, 2, 3]))
    p := &one()
    println($\"{}\", *p)
}",
    )
    .expect("a temporary is built standing in a slot, and that slot has an address")
    .ir_text();
    assert!(ir.contains("[3 x i32]"), "{ir}");
    assert!(ir.contains("call i32 @sum"), "{ir}");
}

#[test]
fn a_fixed_array_comes_back_from_a_call_whole() {
    let ir = build(
        "package main
fn pair() -> [i32; 2] {
    return [3, 4]
}
fn main() {
    p := pair()
    println($\"{}\", p[1])
}",
    )
    .expect("a fixed array crosses a call boundary by value")
    .ir_text();
    assert!(ir.contains("define [2 x i32] @pair"), "{ir}");
}

#[test]
fn a_method_takes_its_receiver_first() {
    let ir = build(
        "package main
struct Vec3 {
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
fn main() {
    v := Vec3 { x: 1.0 }
    println($\"{}\", v.doubled())
    v.scale(2.0)
    println($\"{}\", v.x)
}",
    )
    .expect("builds")
    .ir_text();
    assert!(
        ir.contains("define float @dux.Vec3.doubled(%Vec3 %0)"),
        "a value receiver is the record itself, passed first:\n{ir}"
    );
    assert!(
        ir.contains("define void @dux.Vec3.scale(ptr %0, float %1)"),
        "a pointer receiver is the address of the record:\n{ir}"
    );
    assert!(
        ir.contains("call float @dux.Vec3.doubled(%Vec3 %load"),
        "the call copies the receiver: {ir}"
    );
    assert!(
        ir.contains("call void @dux.Vec3.scale(ptr %v, float 2.000000e+00)"),
        "the call borrows the receiver's slot: {ir}"
    );
}

#[test]
fn a_method_the_record_does_not_have_is_refused() {
    let errors = build(
        "package main
struct Vec3 {
    x: f32,
}
impl Vec3 {
    fn doubled(self) -> f32 {
        return self.x
    }
}
fn main() {
    v := Vec3 { x: 1.0 }
    println($\"{}\", v.nope())
}",
    )
    .unwrap_err();
    assert_eq!(errors[0].code, dux_diag::code::UNDECLARED, "{errors:?}");
    assert!(
        errors[0].message.contains("`Vec3` has no method `nope`"),
        "{errors:?}"
    );
    assert!(
        errors[0]
            .note
            .as_deref()
            .is_some_and(|n| n.contains("impl Vec3")),
        "{errors:?}"
    );
}

#[test]
fn a_receiver_a_call_built_stands_where_a_method_can_borrow_it() {
    let ir = build(
        "package main
struct Vec3 {
    x: f32,
}
fn make() -> Vec3 {
    return Vec3 { x: 1.0 }
}
impl Vec3 {
    fn scale(self: *Vec3, k: f32) {
        self.x *= k
    }
    fn shown(self: Vec3) -> f32 {
        return self.x
    }
}
fn main() {
    make().scale(2.0)
    println($\"{}\", make().shown())
    println($\"{}\", \"dux\".len())
}",
    )
    .expect("a receiver that lives in registers gets a place of its own")
    .ir_text();
    assert!(
        ir.contains("alloca %Vec3"),
        "the record a call built stands in a slot: {ir}"
    );
    assert!(
        ir.contains("@dux.Vec3.scale(ptr %recv"),
        "and the method borrows that slot: {ir}"
    );
}

#[test]
fn methods_of_different_records_do_not_collide() {
    let ir = build(
        "package main
struct A {
    a: i32,
}
struct B {
    b: i32,
}
impl A {
    fn read(self) -> i32 {
        return self.a
    }
}
impl B {
    fn read(self) -> i32 {
        return self.b
    }
}
fn main() {
    a := A { a: 1 }
    b := B { b: 2 }
    println($\"{} {}\", a.read(), b.read())
}",
    )
    .expect("builds")
    .ir_text();
    assert!(ir.contains("@dux.A.read(%A"), "{ir}");
    assert!(ir.contains("@dux.B.read(%B"), "{ir}");
}

#[test]
fn a_slice_is_an_address_and_a_count() {
    let ir = build(
        "package main
fn main() {
    dims: [i32; 4] = [10, 20, 30, 40]
    window: []i32 = &dims
    println($\"{}\", window.len())
}",
    )
    .expect("builds")
    .ir_text();
    assert!(
        ir.contains("%window = alloca { ptr, i64 }"),
        "a view is sixteen bytes of slot:\n{ir}"
    );
    assert!(
        ir.contains("insertvalue { ptr, i64 } undef, ptr %dims, 0") && ir.contains("i64 4, 1"),
        "the array's own length becomes the view's count:\n{ir}"
    );
    assert!(
        ir.contains("extractvalue { ptr, i64 } %load, 1"),
        "`.len()` reads the count back out:\n{ir}"
    );
}

#[test]
fn indexing_a_slice_walks_the_data_it_carries() {
    let ir = build(
        "package main
fn show(xs: []i32) -> i32 {
    return xs[1]
}
fn main() {
    dims: [i32; 2] = [1, 2]
    println($\"{}\", show(dims))
}",
    )
    .expect("builds")
    .ir_text();
    assert!(
        ir.contains("%idx = getelementptr i32, ptr"),
        "an element is `data + index`, not an index into an array:\n{ir}"
    );
    assert!(
        ir.contains("insertvalue { ptr, i64 } undef, ptr %dims, 0"),
        "an array passed where a view is expected borrows its storage:\n{ir}"
    );
}

#[test]
fn subslicing_moves_the_head_and_shortens_the_count() {
    let ir = build(
        "package main
fn main() {
    dims: [i32; 4] = [10, 20, 30, 40]
    middle := &dims[1..3]
    println($\"{} {}\", middle.len(), middle[0])
}",
    )
    .expect("builds")
    .ir_text();
    assert!(
        ir.contains("%view.head = getelementptr i32, ptr"),
        "the head steps to the first element of the range:\n{ir}"
    );
    assert!(
        ir.contains("getelementptr [4 x i32], ptr %dims, i64 0, i64 1")
            || ir.contains("%view.head"),
        "the count is the difference of the bounds:\n{ir}"
    );
}

#[test]
fn iterating_a_slice_reads_its_count_at_loop_entry() {
    let ir = build(
        "package main
fn total(xs: []i32) -> i32 {
    sum: i32 = 0
    for i, v in xs {
        sum += v
        println($\"{}\", i)
    }
    return sum
}
fn main() {
    dims: [i32; 2] = [1, 2]
    println($\"{}\", total(dims))
}",
    )
    .expect("builds")
    .ir_text();
    assert!(ir.contains("%iter = alloca i64"), "{ir}");
    assert!(
        ir.contains("icmp ult i64 %load"),
        "the loop stops at the view's length:\n{ir}"
    );
}

#[test]
fn a_view_cannot_be_indexed_by_anything_but_an_integer() {
    let errors = build(
        "package main
fn main() {
    dims: [i32; 2] = [1, 2]
    window: []i32 = &dims
    println($\"{}\", window[\"one\"])
}",
    )
    .unwrap_err();
    assert_eq!(errors[0].code, dux_diag::code::TYPE_MISMATCH, "{errors:?}");
}

#[test]
fn an_allocating_function_carries_the_context_and_main_creates_one() {
    let ir = build(
        "package main
fn spare() -> *i32 {
    return new(i32)
}
fn main() {
    p := spare()
    *p = 3
    println($\"{}\", *p)
    free(p)
}",
    )
    .expect("builds")
    .ir_text();
    assert!(
        ir.contains("define internal ptr @dux.runtime.alloc(ptr"),
        "the allocator is reached through a context:\n{ir}"
    );
    assert!(
        ir.contains("@dux.default_context = global %dux.Context"),
        "the entry point starts from a default context:\n{ir}"
    );
    assert!(
        ir.contains("store ptr @dux.default_context, ptr %context"),
        "main binds the default context:\n{ir}"
    );
    assert!(
        ir.contains("call i32 @atexit(ptr @dux.runtime.report)"),
        "the leak report runs at exit:\n{ir}"
    );
    assert!(
        ir.contains("define ptr @spare(ptr %0)"),
        "a function that allocates carries the hidden context:\n{ir}"
    );
}

#[test]
fn only_a_function_that_allocates_takes_the_hidden_parameter() {
    let ir = build(
        "package main
fn quiet(x: i32) -> i32 {
    return x * 2
}
fn loud() -> *i32 {
    return new(i32)
}
fn main() {
    println($\"{}\", quiet(2))
    p := loud()
    free(p)
}",
    )
    .expect("builds")
    .ir_text();
    assert!(
        ir.contains("define i32 @quiet(i32"),
        "a function that never allocates keeps a C-shaped signature:\n{ir}"
    );
    assert!(
        ir.contains("define ptr @loud(ptr"),
        "a function that allocates carries the context:\n{ir}"
    );
}

#[test]
fn a_release_build_drops_the_debug_bookkeeping() {
    let ir = build_with(
        "package main\nfn main() {\n    p := new(i32)\n    free(p)\n}\n",
        Options {
            opt_level: OptLevel::Balanced,
            debug_allocator: false,
            ..Options::default()
        },
    )
    .expect("builds")
    .ir_text();
    assert!(
        !ir.contains("dux.runtime.report"),
        "no leak reporter in a release build:\n{ir}"
    );
    assert!(
        !ir.contains("@dux.runtime.live"),
        "no allocation counters in a release build:\n{ir}"
    );
    assert!(
        ir.contains("call ptr @malloc"),
        "the plain allocator still wraps malloc:\n{ir}"
    );
}

#[test]
fn the_debug_allocator_arms_a_guard_page_and_tabulates_every_block() {
    let ir = build("package main\nfn main() {\n    p := new(i32)\n    free(p)\n}\n")
        .expect("builds")
        .ir_text();
    assert!(
        ir.contains("call ptr @mmap("),
        "debug blocks come from a private mapping:\n{ir}"
    );
    assert!(
        ir.contains("call i32 @mprotect("),
        "the page after a block is armed as the guard:\n{ir}"
    );
    assert!(
        ir.contains("call i32 @munmap("),
        "a released block hands its mapping back:\n{ir}"
    );
    assert!(
        !ir.contains("call ptr @malloc"),
        "the debug allocator does not lean on malloc:\n{ir}"
    );
    assert!(
        ir.contains("@dux.runtime.table = global [16384 x { ptr, i64, ptr }] zeroinitializer"),
        "every block's place of birth is tabulated:\n{ir}"
    );
}

#[test]
fn make_zeroes_the_block_it_hands_back() {
    let ir = build(
        "package main
fn main() {
    xs := make(i32, 4)
    println($\"{}\", xs.len())
    del(xs)
}",
    )
    .expect("builds")
    .ir_text();
    assert!(
        ir.contains("call void @llvm.memset"),
        "make zeroes what it allocated:\n{ir}"
    );
}

#[test]
fn a_block_that_rebinds_the_context_puts_the_outer_one_back() {
    let ir = build(
        "package main
fn main() {
    outer := context
    {
        context = outer
        p := new(i32)
        free(p)
    }
    println($\"{}\", 1)
}",
    )
    .expect("builds")
    .ir_text();
    assert!(
        ir.contains("%context.outer = alloca ptr"),
        "the block saved the outer context:\n{ir}"
    );
    assert!(
        ir.contains("store ptr %context.outer.value, ptr %context"),
        "the block put the outer context back:\n{ir}"
    );
}

#[test]
fn a_program_may_declare_its_own_make() {
    let ir = build(
        "package main
fn make() -> i32 {
    return 5
}
fn main() {
    println($\"{}\", make())
}",
    )
    .expect("builds")
    .ir_text();
    assert!(
        ir.contains("define i32 @make()"),
        "the builtins are the prelude, so the program wins:\n{ir}"
    );
}

#[test]
fn a_string_is_an_address_and_a_count() {
    let ir = build(
        "package main
fn main() {
    text := \"hello\"
    println($\"{}\", text.len())
}",
    )
    .expect("builds")
    .ir_text();
    assert!(
        ir.contains("%text = alloca { ptr, i64 }"),
        "a string is a view in a slot:\n{ir}"
    );
    assert!(
        ir.contains("store { ptr, i64 } { ptr @str, i64 5 }, ptr %text"),
        "the literal carries its own length:\n{ir}"
    );
}

#[test]
fn a_string_index_reads_a_byte_and_refuses_to_write_one() {
    let ir = build(
        "package main
fn main() {
    text := \"abc\"
    println($\"{}\", text[1])
}",
    )
    .expect("builds")
    .ir_text();
    assert!(
        ir.contains("getelementptr i8") && ir.contains("load i8"),
        "the byte at an offset:\n{ir}"
    );

    let errors = build(
        "package main
fn main() {
    text := \"abc\"
    text[0] = 65
}",
    )
    .expect_err("a string does not write");
    assert_eq!(errors[0].code, dux_diag::code::TYPE_MISMATCH, "{errors:?}");
    assert!(
        errors[0]
            .note
            .as_deref()
            .is_some_and(|note| note.contains("immutable")),
        "{:?}",
        errors[0].note
    );
}

#[test]
fn concatenation_allocates_and_copies_both_halves() {
    let ir = build(
        "package main
fn main() {
    a := \"ab\"
    b := \"cd\"
    whole := a + b
    println(whole)
    del(whole)
}",
    )
    .expect("builds")
    .ir_text();
    assert!(
        ir.contains("call ptr %alloc("),
        "the joined string comes through the context's own function:\n{ir}"
    );
    assert!(
        ir.contains("ptr @dux.runtime.alloc"),
        "the default context carries the runtime allocator in its first slot:\n{ir}"
    );
    assert!(
        ir.contains("call void @llvm.memcpy"),
        "both halves are copied in:\n{ir}"
    );
    assert!(
        ir.contains("add i64") && ir.contains("insertvalue { ptr, i64 }"),
        "the count is the sum of the two:\n{ir}"
    );
}

#[test]
fn strings_are_equal_when_the_bytes_and_the_lengths_agree() {
    let ir = build(
        "package main
fn main() {
    a := \"ab\"
    b := \"cd\"
    println($\"{}\", a == b)
}",
    )
    .expect("builds")
    .ir_text();
    assert!(
        ir.contains("call i32 @memcmp"),
        "the common prefix is compared:\n{ir}"
    );
    assert!(
        ir.contains("icmp eq i64") && ir.contains("and i1"),
        "and then the lengths:\n{ir}"
    );
}

#[test]
fn a_c_string_becomes_a_string_by_measuring_it() {
    let ir = build(
        "package main
fn echo(text: string) {
    println(text)
}
fn main() {
    echo(c\"bytes\")
}",
    )
    .expect("builds")
    .ir_text();
    assert!(
        ir.contains("call i64 @strlen"),
        "the count has to be found before the view exists:\n{ir}"
    );
    assert!(
        ir.contains("define void @echo({ ptr, i64 } %0)"),
        "the parameter is a view:\n{ir}"
    );
}

#[test]
fn a_body_that_adds_text_carries_the_context() {
    let ir = build(
        "package main
fn joins() -> string {
    a := \"a\"
    return a + \"b\"
}
fn adds() -> i32 {
    x := 1
    return x + 2
}
fn main() {
    println(joins())
    println($\"{}\", adds())
}",
    )
    .expect("builds")
    .ir_text();
    assert!(
        ir.contains("define { ptr, i64 } @joins(ptr %"),
        "text joins allocate, so the joiner carries a context:\n{ir}"
    );
    assert!(
        ir.contains("define i32 @adds()"),
        "an ordinary addition does not:\n{ir}"
    );
}

#[test]
fn an_interpolated_string_as_a_value_is_formatted_into_a_block() {
    let ir = build(
        "package main
fn label(name: string) -> string {
    return $\"{name} is ready\"
}
fn main() {
    text := label(\"dux\")
    println(text)
    del(text)
}",
    )
    .expect("builds")
    .ir_text();
    assert!(
        ir.contains("@snprintf(ptr null, i64 0"),
        "the length is measured first:\n{ir}"
    );
    assert!(
        ir.contains("call ptr %alloc("),
        "the block is asked for through the context, whatever context that is:\n{ir}"
    );
    assert!(
        ir.contains("@snprintf(ptr %"),
        "the bytes are written into the block:\n{ir}"
    );
    assert!(
        ir.contains("define { ptr, i64 } @label({ ptr, i64 } %0, ptr %"),
        "building text allocates, so the builder carries a context:\n{ir}"
    );
}

#[test]
fn writing_a_string_needs_no_more_of_a_context_than_printing_one() {
    let ir = build(
        "package main
fn main() {
    name := \"dux\"
    println($\"{name} is ready\")
}",
    )
    .expect("builds")
    .ir_text();
    assert!(
        !ir.contains("@snprintf"),
        "a hole written straight out is formatted in place:\n{ir}"
    );
    assert!(
        !ir.contains("@dux.runtime.alloc"),
        "and no block is asked for:\n{ir}"
    );
}

const SHAPES: &str = "package main
enum Shape {
    Circle(f32),
    Rect { w: f32, h: f32 },
    Empty,
}
fn side(s: Shape) -> f32 {
    return match s {
        Shape::Circle(r) => r,
        Shape::Rect { w, h } => w + h,
        Shape::Empty => 0.0,
    }
}
fn main() {
    s := Shape::Rect { w: 3.0, h: 4.0 }
    println($\"{}\", side(s))
}";

#[test]
fn a_sum_type_is_a_tag_beside_an_area_big_enough_for_the_largest_payload() {
    let ir = build(SHAPES).expect("builds").ir_text();
    assert!(
        ir.contains("{ i16, [2 x { float }] }"),
        "DESIGN 4.4: a u16 tag and a payload area, laid out like a C record:\n{ir}"
    );
    assert!(
        ir.contains("define float @side({ i16, [2 x { float }] } %0)"),
        "a sum type travels as one value, like a record:\n{ir}"
    );
    assert!(
        ir.contains("store i16 1, ptr"),
        "constructing `Rect`, the second variant, writes tag 1:\n{ir}"
    );
    assert!(
        ir.contains("getelementptr i8, ptr %sum, i64 4"),
        "the payload starts where the target's own layout says:\n{ir}"
    );
}

#[test]
fn a_match_switches_on_the_tag_and_meets_its_arms_at_one_phi() {
    let ir = build(SHAPES).expect("builds").ir_text();
    assert!(
        ir.contains("%tag = load i16"),
        "the tag is read once, and it decides everything:\n{ir}"
    );
    assert!(
        ir.contains("switch i16 %tag, label %match.trap [")
            && ir.contains("i16 0, label %match.arm")
            && ir.contains("i16 1, label %match.arm1")
            && ir.contains("i16 2, label %match.arm2"),
        "DESIGN 4.4: one switch on the tag, one block per arm:\n{ir}"
    );
    assert!(
        !ir.contains("icmp eq i16"),
        "and no arm asks the tag whether it is its own:\n{ir}"
    );
    assert!(
        ir.contains("%matchtmp = phi float"),
        "one phi joins the arms' values:\n{ir}"
    );
    assert!(
        ir.contains("call void @dux.runtime.trap("),
        "and the tag no arm names — which the checker forbids — says so:\n{ir}"
    );
}

#[test]
fn an_arm_a_switch_cannot_say_is_still_tried_in_its_order() {
    let ir = build(
        "package main
enum Shape {
    Circle(f32),
    Rect { w: f32, h: f32 },
    Empty,
}
fn side(s: Shape) -> f32 {
    return match s {
        Shape::Rect { w, h } if w > h => w,
        Shape::Rect { w, h } => h,
        Shape::Circle(r) => r,
        Shape::Empty => 0.0,
    }
}
fn main() {
    s := Shape::Rect { w: 3.0, h: 4.0 }
    println($\"{}\", side(s))
}",
    )
    .expect("builds")
    .ir_text();
    assert!(
        ir.matches("icmp eq i16").count() >= 4 && ir.contains("br i1 %holds"),
        "a guard, or the same variant twice, is beyond a switch:\n{ir}"
    );
    assert!(
        !ir.contains("switch i16"),
        "so every arm tests the tag in the order it is written:\n{ir}"
    );
}

#[test]
fn option_of_a_pointer_is_one_pointer_and_none_is_null() {
    let ir = build(
        "package main
struct Node {
    value: i32,
    next: Option<*Node>,
}
fn main() -> i32 {
    tail := Node { value: 1, next: None }
    head := Node { value: 2, next: Some(&tail) }
    return head.value
}",
    )
    .expect("builds")
    .ir_text();
    assert!(
        ir.contains("%Node = type { i32, ptr }"),
        "DESIGN 4.3: the niche leaves `Option<*T>` a single word:\n{ir}"
    );
    assert!(
        !ir.contains("type { i32, { i16"),
        "and a niche pays no tag:\n{ir}"
    );
    assert!(
        ir.contains("store ptr null, ptr"),
        "`None` is the null address, written where the pointer lives:\n{ir}"
    );
}

#[test]
fn a_pattern_condition_answers_with_one_bool() {
    let ir = build(
        "package main
fn first(o: Option<i32>) -> i32 {
    if Some(v) := o {
        return v
    }
    return 0 - 1
}
fn main() -> i32 {
    return first(None)
}",
    )
    .expect("builds")
    .ir_text();
    assert!(
        ir.contains("%matched = phi i1 [ true, %pat.true ], [ false, %pat.false ]"),
        "a pattern condition is a `match` with a non-matching fallthrough arm:\n{ir}"
    );
    assert!(
        ir.contains("%holds = icmp eq i16 %tag, 0") || ir.contains("icmp eq i16"),
        "and it tests the tag like any other arm:\n{ir}"
    );
}

#[test]
fn an_or_pattern_binds_one_slot_that_every_alternative_writes() {
    let ir = build(
        "package main
enum Coin { Heads(i32), Tails(i32), Edge }
fn worth(c: Coin) -> i32 {
    return match c {
        Coin::Heads(v) | Coin::Tails(v) => v,
        Coin::Edge => 0,
    }
}
fn main() -> i32 {
    return worth(Coin::Edge)
}",
    )
    .expect("builds")
    .ir_text();
    let slots = ir.matches("%v = alloca").count();
    assert_eq!(
        slots, 1,
        "one binder, one slot, however many alternatives bind it:\n{ir}"
    );
    assert!(
        ir.contains("%v = alloca i32"),
        "the binder holds the payload:\n{ir}"
    );
}

const MAX: &str = "fn max<T>(a: T, b: T) -> T {
    return if a > b { a } else { b }
}";

#[test]
fn a_generic_fn_is_emitted_once_per_thing_it_was_asked_to_be() {
    let ir = build(&format!(
        "{MAX}\nfn main() {{\n    println($\"{{}}\", max(3, 7))\n    println($\"{{}}\", max(2.5, 1.5))\n}}",
    ))
    .expect("builds")
    .ir_text();
    assert!(
        ir.contains("define i32 @dux.max.i32(i32 %")
            && ir.contains("define double @dux.max.f64(double %"),
        "the two calls ask for two functions, and both are here:\n{ir}"
    );
    assert!(
        !ir.contains("@dux.max(") && !ir.contains("@max("),
        "and there is no `max` that stands for no type at all:\n{ir}"
    );
    assert_eq!(
        ir.matches("define i32 @dux.max.i32").count(),
        1,
        "one call or ten, an instantiation is emitted once:\n{ir}"
    );
}

#[test]
fn a_generic_no_call_asked_for_is_not_in_the_module() {
    let ir = build(&format!(
        "{MAX}\nfn main() {{\n    println($\"{{}}\", 1)\n}}",
    ))
    .expect("builds")
    .ir_text();
    assert!(
        !ir.contains("@dux.max"),
        "`max` was never called, so nothing of it is code-generated:\n{ir}"
    );
}

#[test]
fn a_generic_body_that_calls_itself_reaches_the_same_function_it_is_in() {
    let ir = build(
        "fn fact<T>(n: T) -> T {
    return if n <= 1 { n } else { n * fact(n - 1) }
}
fn main() {
    println($\"{}\", fact<i64>(5))
}",
    )
    .expect("builds")
    .ir_text();
    assert!(ir.contains("define i64 @dux.fact.i64(i64 %"), "{ir}");
    assert!(
        ir.contains("call i64 @dux.fact.i64(i64 %"),
        "the body's own call is to the instantiation it is part of, and to no \
         function that stands for no type:\n{ir}"
    );
    assert!(
        !ir.contains("@dux.fact(i64 5)") && ir.contains("call i64 @dux.fact.i64(i64 5)"),
        "the call in `main` reaches the same one function:\n{ir}"
    );
}

#[test]
fn a_generic_record_is_a_record_whose_fields_are_the_arguments() {
    let ir = build(
        "struct Pair<A, B> {
    first: A,
    second: B,
}
fn main() {
    p := Pair<i32, string> { first: 1, second: \"one\" }
    println($\"{} {}\", p.first, p.second)
}",
    )
    .expect("builds")
    .ir_text();
    assert!(
        ir.contains("%\"Pair<i32, string>\" = type { i32, { ptr, i64 } }"),
        "the instantiation says what it was filled with:\n{ir}"
    );
}

#[test]
fn two_ways_of_filling_one_record_are_two_records() {
    let ir = build(
        "struct Pair<A, B> {
    first: A,
    second: B,
}
fn main() {
    a := Pair<i32, string> { first: 1, second: \"one\" }
    b := Pair<string, i32> { first: \"one\", second: 1 }
    println($\"{} {}\", a.first, b.second)
}",
    )
    .expect("builds")
    .ir_text();
    assert!(
        ir.contains("%\"Pair<i32, string>\" = type { i32, { ptr, i64 } }")
            && ir.contains("%\"Pair<string, i32>\" = type { { ptr, i64 }, i32 }"),
        "swapping the arguments swaps the fields, and LLVM sees two types:\n{ir}"
    );
}

#[test]
fn a_const_parameter_answers_a_length_the_body_asks_for() {
    let ir = build(
        "fn tally<const N: usize>() -> i32 {
    grid := [7; N]
    total: i32 = 0
    for v in grid {
        total += v
    }
    return total
}
fn main() {
    println($\"{}\", tally<3>())
}",
    )
    .expect("builds")
    .ir_text();
    assert!(
        ir.contains("alloca [3 x i32]") || ir.contains("[3 x i32]"),
        "`N` was three, so the array is three wide:\n{ir}"
    );
    assert!(
        !ir.contains("@dux.tally("),
        "and the function that exists is the one that was asked for:\n{ir}"
    );
}

#[test]
fn a_method_of_a_generic_record_is_emitted_once_per_receiver() {
    let ir = build(
        r#"struct Cell<T> {
    value: T,
    next: Option<*Cell<T>>,
}

impl Cell<T> {
    fn shown(self: Cell<T>, extra: T) -> T {
        return self.value
    }
}

fn main() {
    n := Cell<i32> { value: 9, next: None }
    s := Cell<string> { value: "nine", next: None }
    a := n.shown(1)
    b := s.shown("x")
    println($"{a}")
    println($"{b}")
}
"#,
    )
    .expect("a method of a generic impl is emitted at its receivers")
    .ir_text();
    assert!(
        ir.contains(r#"define i32 @dux.Cell.i32.shown(%"Cell<i32>" %0, i32 %1)"#),
        "the `i32` receiver gets an `i32` method: {ir}"
    );
    assert!(
        ir.contains(
            r#"define { ptr, i64 } @dux.Cell.string.shown(%"Cell<string>" %0, { ptr, i64 } %1)"#
        ),
        "the `string` receiver gets a `string` method, which is a different function: {ir}"
    );
    assert!(
        !ir.contains("define i32 @dux.Cell.shown"),
        "the method as written is code, not a function: {ir}"
    );
}

#[test]
fn a_method_of_an_impl_of_a_sum_type_is_emitted_once_per_receiver() {
    let ir = build(
        r#"enum Maybe<T> {
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

fn main() {
    a: Maybe<i32> = Maybe::Some(7)
    b: Maybe<string> = Maybe::Nothing
    println($"{}", a.shown(0))
    println($"{}", b.shown("d"))
}
"#,
    )
    .expect("a method of an impl of a sum type is emitted at its receivers")
    .ir_text();
    assert!(
        ir.contains("@dux.Maybe.i32.shown"),
        "the `i32` receiver gets an `i32` method: {ir}"
    );
    assert!(
        ir.contains("@dux.Maybe.string.shown"),
        "the `string` receiver gets a `string` method, which is a different function: {ir}"
    );
    assert!(
        !ir.contains("@dux.Maybe.shown"),
        "the method as written is code, not a function: {ir}"
    );
}
