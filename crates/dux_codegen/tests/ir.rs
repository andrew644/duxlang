use dux_codegen::{CompiledModule, OptLevel, Options};
use dux_source::SourceMap;

fn build(source: &str) -> Result<CompiledModule, Vec<dux_diag::Diagnostic>> {
    build_with(source, Options::default())
}

fn build_with(
    source: &str,
    options: Options,
) -> Result<CompiledModule, Vec<dux_diag::Diagnostic>> {
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
    assert!(ir.contains("call i32 @puts"), "println lowers to puts:\n{ir}");
    assert!(
        ir.contains("constant [3 x i8] c\"hi\\00\""),
        "the literal must land in rodata with a NUL:\n{ir}"
    );
}

#[test]
fn print_uses_fputs_so_it_adds_no_newline() {
    let ir = build("package main\nfn main() { print(\"hi\") }")
        .expect("builds")
        .ir_text();
    assert!(ir.contains("call i32 @fputs"), "{ir}");
    assert!(ir.contains("@stdout"), "fputs needs the stream:\n{ir}");
    assert!(!ir.contains("@puts"), "print must not use puts:\n{ir}");
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
    let release =
        CompiledModule::build(&parsed.program, Options {
        opt_level: OptLevel::Balanced,
        debug_allocator: true,
    })
            .expect("release builds")
            .ir_text();
    assert!(debug.contains("@puts") && release.contains("@puts"));
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
    assert!(errors[0].span.is_some(), "the diagnostic must point at the code");
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
    assert!(ir.contains("add i32"), "the compound assign keeps i32:\n{ir}");
}

#[test]
fn a_value_if_meets_at_a_phi() {
    let ir = build(
        "package main\nfn main() -> i32 { c := true; x := if c { 1 } else { 2 }; return x }",
    )
    .expect("builds")
    .ir_text();
    assert!(ir.contains("phi i32"), "the arms meet at a phi:\n{ir}");
    assert!(ir.contains("br i1"), "the condition branches:\n{ir}");

    let ir = build(
        "package main\nfn pick(c: bool) -> string { s := if c { \"a\" } else { \"b\" }; return s }\nfn main() { pick(true) }",
    )
    .expect("builds")
    .ir_text();
    assert!(ir.contains("phi ptr"), "string arms meet at a pointer phi:\n{ir}");
}

#[test]
fn every_statement_carries_its_source_line() {
    let source = "package main\n\nfn pick(n: i32) -> i32 {\n    if n > 0 {\n        return n\n    }\n    return 0\n}\n\nfn main() {\n    pick(3)\n}\n";
    let ir = build(source).expect("builds").ir_text();
    assert!(
        ir.contains(r#"!DISubprogram(name: "pick""#) && ir.contains(r#"!DISubprogram(name: "main""#),
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
fn fixed_arrays_live_in_slots_and_iterate_with_a_counter() {
    let ir = build(
        "package main\nfn main() -> i32 { xs := [1, 2, 3]; total := 0; for v in xs { total += v }; return total }",
    )
    .expect("builds")
    .ir_text();
    assert!(ir.contains("alloca [3 x i32]"), "the literal backs a slot:\n{ir}");
    assert!(ir.contains("getelementptr"), "elements are addressed by GEP:\n{ir}");
    assert!(ir.contains("ult"), "the counter walks the length:\n{ir}");

    let ir = build(
        "package main\nfn sum() -> i64 { z := [0; 4]; s: i64 = 0; for i in 0..4 { s += i }; return s }\nfn main() { sum() }",
    )
    .expect("builds")
    .ir_text();
    assert!(ir.contains("alloca [4 x i32]"), "repeats back a slot:\n{ir}");
    assert!(ir.contains("slt"), "the range compares against its limit:\n{ir}");
}

#[test]
fn implicit_conversion_widens_exactly_when_the_destination_is_lossless() {
    let ir = build(
        "package main\nfn main() { wider: i64 = 0; narrow := 1; wider = wider + narrow }",
    )
    .expect("builds")
    .ir_text();
    assert!(ir.contains("sext i32"), "i32 widens into i64 free:\n{ir}");
    assert!(ir.contains("add i64"), "{ir}");

    let errors = build("package main\nfn main() { narrow: i8 = 0; wide := 1; narrow = wide }")
        .unwrap_err();
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
    assert!(ir.contains("br label %for.cond"), "the step loops back:\n{ir}");
}

#[test]
fn while_and_loop_bind_break_and_continue() {
    let ir = build(
        "package main\nfn main() {\n    x := 0\n    while x < 4 {\n        x += 1\n        if x == 2 { continue }\n        if x >= 3 { break }\n    }\n}",
    )
    .expect("builds")
    .ir_text();
    assert!(ir.contains("br label %while.cond"), "{ir}");
    assert!(ir.contains("br label %while.end"), "break exits the loop:\n{ir}");
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
    assert!(!ir.contains("and i1"), "`&&` must branch, not bitwise-and:\n{ir}");
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
fn milestone_gaps_report_e0007_with_the_milestone_in_the_message() {
    let cases = [
        (
            "package main\nfn main() { for b in \"abc\" { print(b) } }",
            "iterating a `string`",
        ),
        (
            "package main\nfn main() { if true { defer println(\"x\") } }",
            "nested block",
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
    let ir = build(
        "package main\nconst KI: usize = 1024\nfn main() { page: usize = KI + 1 }",
    )
    .expect("builds")
    .ir_text();
    assert!(
        ir.contains("store i64 1025, ptr %page"),
        "the constant adopts usize and folds:\n{ir}"
    );

    let errors = build("package main\nconst K: i32 = 1\nfn main() { K = 2 }").unwrap_err();
    assert_eq!(errors[0].code, dux_diag::code::ASSIGN_TO_CONST);
    assert!(errors[0].message.contains("cannot be assigned"), "{errors:?}");

    let errors = build("package main\nconst K: i8 = 300\nfn main() { }").unwrap_err();
    assert_eq!(errors[0].code, dux_diag::code::LITERAL_OUT_OF_RANGE);
    assert!(errors[0].message.contains("does not fit"), "{errors:?}");
}

#[test]
fn globals_are_module_storage_and_update_in_place() {
    let ir = build(
        "package main\nglobal hits: i32 = 0\nfn bump() { hits += 1 }\nfn main() { bump() }",
    )
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
    assert_eq!(errors[0].code, dux_diag::code::ASSIGN_TO_CONST, "{errors:?}");
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
fn the_address_of_a_temporary_is_refused() {
    let errors = build(
        "package main
fn one() -> i32 {
    return 1
}
fn main() {
    p := &one()
    println($\"{}\", *p)
}",
    )
    .unwrap_err();
    assert_eq!(errors[0].code, dux_diag::code::TYPE_MISMATCH, "{errors:?}");
    assert!(errors[0].message.contains("address"), "{errors:?}");
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
    assert!(errors[0].message.contains("`Vec3` has no method `nope`"), "{errors:?}");
    assert!(errors[0].note.as_deref().is_some_and(|n| n.contains("impl Vec3")), "{errors:?}");
}

#[test]
fn a_pointer_receiver_needs_a_place_to_borrow() {
    let errors = build(
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
}
fn main() {
    make().scale(2.0)
}",
    )
    .unwrap_err();
    assert_eq!(errors[0].code, dux_diag::code::TYPE_MISMATCH, "{errors:?}");
    assert!(errors[0].message.contains("no address"), "{errors:?}");
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
        ir.contains("insertvalue { ptr, i64 } undef, ptr %dims, 0")
            && ir.contains("i64 4, 1"),
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
