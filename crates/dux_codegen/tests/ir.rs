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
    let errors = build("package main\nfn main() { x := 1 }").unwrap_err();
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
