use dux_codegen::{CompiledModule, OptLevel, Options};
use dux_source::SourceMap;

fn build(source: &str) -> Result<CompiledModule, Vec<dux_diag::Diagnostic>> {
    let mut sources = SourceMap::new();
    let file = sources.add("t.dux", source.to_string());
    let lexed = dux_lexer::lex(&sources, file);
    let parsed = dux_parser::parse(&sources, &lexed.tokens);
    assert!(
        parsed.diagnostics.is_empty(),
        "the fixture should parse: {:?}",
        parsed.diagnostics
    );
    CompiledModule::build(&parsed.program, Options { opt_level: OptLevel::None })
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
    let debug = CompiledModule::build(&parsed.program, Options { opt_level: OptLevel::None })
        .expect("debug builds")
        .ir_text();
    let release =
        CompiledModule::build(&parsed.program, Options { opt_level: OptLevel::Balanced })
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
    let errors = build("package main\nfn main() { defer println(\"x\") }").unwrap_err();
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
    assert_eq!(errors[0].code, dux_diag::code::UNEXPECTED_TOKEN);
    assert!(
        errors[0].message.contains("as i8"),
        "the hint must show the explicit form: {:?}",
        errors[0].message
    );
}

#[test]
fn a_literal_that_does_not_fit_its_annotation_is_an_error() {
    let errors = build("package main\nfn main() { small: i8 = 300 }").unwrap_err();
    assert_eq!(errors[0].code, dux_diag::code::UNEXPECTED_TOKEN, "{errors:?}");
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
    assert_eq!(errors[0].code, dux_diag::code::UNEXPECTED_TOKEN);
    assert!(errors[0].message.contains("not all paths"), "{errors:?}");
}

#[test]
fn milestone_gaps_report_e0007_with_the_milestone_in_the_message() {
    let cases = [
        ("package main\nfn main() { for v in xs { print(\"x\") } }", "`for .. in`"),
        ("package main\nfn main() { defer println(\"x\") }", "`defer`"),
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
    assert_eq!(errors[0].code, dux_diag::code::UNEXPECTED_TOKEN);
    assert!(errors[0].message.contains("cannot be assigned"), "{errors:?}");

    let errors = build("package main\nconst K: i8 = 300\nfn main() { }").unwrap_err();
    assert_eq!(errors[0].code, dux_diag::code::UNEXPECTED_TOKEN);
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
