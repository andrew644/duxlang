use std::path::PathBuf;
use std::process::Command;

const SCRATCH_DIR: &str = env!("CARGO_TARGET_TMPDIR");

fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(SCRATCH_DIR).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch directory");
    dir
}

struct Run {
    stdout: String,
    stderr: String,
    code: i32,
}

fn duxc(args: &[&str]) -> Run {
    let output = Command::new(env!("CARGO_BIN_EXE_duxc"))
        .args(args)
        .output()
        .expect("duxc is runnable");
    Run {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        code: output.status.code().unwrap_or(-1),
    }
}

const UGLY: &str = "package main\nfn   main( ){\nprintln(\"hi\") ; x:=2\n}\n";
const CANONICAL: &str = "package main\n\nfn main() {\n    println(\"hi\")\n    x := 2\n}\n";

#[test]
fn fmt_rewrites_files_in_place_and_is_quiet_once_clean() {
    let dir = scratch("fmt_in_place");
    let file = dir.join("app.dux");
    std::fs::write(&file, UGLY).unwrap();

    let run = duxc(&["fmt", file.to_str().unwrap()]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert!(run.stdout.contains("formatted"), "{}", run.stdout);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), CANONICAL);

    let run = duxc(&["fmt", file.to_str().unwrap()]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), CANONICAL);

    let run = duxc(&["fmt", "--check", file.to_str().unwrap()]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert!(run.stdout.contains("all 1 file(s) formatted"), "{}", run.stdout);
}

#[test]
fn check_reports_unformatted_files_without_changing_them() {
    let dir = scratch("fmt_check");
    let file = dir.join("app.dux");
    std::fs::write(&file, UGLY).unwrap();

    let run = duxc(&["fmt", "--check", dir.to_str().unwrap()]);
    assert_eq!(run.code, 1);
    assert!(run.stderr.contains("not formatted"), "{}", run.stderr);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), UGLY);
}

#[test]
fn fmt_recurses_into_subdirectories_and_skips_foreign_files() {
    let dir = scratch("fmt_recursive");
    let nested = dir.join("deep");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(nested.join("app.dux"), UGLY).unwrap();
    std::fs::write(nested.join("notes.txt"), "  not dux  ").unwrap();

    let run = duxc(&["fmt", dir.to_str().unwrap()]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(
        std::fs::read_to_string(nested.join("app.dux")).unwrap(),
        CANONICAL
    );
    assert_eq!(std::fs::read_to_string(nested.join("notes.txt")).unwrap(), "  not dux  ");
}

#[test]
fn a_broken_file_fails_fmt_and_stays_untouched() {
    let dir = scratch("fmt_broken");
    let file = dir.join("bad.dux");
    std::fs::write(&file, "fn main() { x := }").unwrap();

    let run = duxc(&["fmt", file.to_str().unwrap()]);
    assert_eq!(run.code, 1);
    assert!(run.stderr.contains("E0005"), "{}", run.stderr);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "fn main() { x := }");
}

#[test]
fn a_milestone_gap_that_would_degrade_the_ast_refuses_formatting() {
    let dir = scratch("fmt_gap");
    let file = dir.join("pairs.dux");
    std::fs::write(&file, "fn main() {\n    pair := (1, 2)\n}\n").unwrap();

    let run = duxc(&["fmt", file.to_str().unwrap()]);
    assert_eq!(run.code, 1);
    assert!(run.stderr.contains("E0007"), "{}", run.stderr);
    assert!(
        std::fs::read_to_string(&file).unwrap().contains("(1, 2)"),
        "the formatter must never emit a file with the tuple erased"
    );
}

#[test]
fn a_match_formats_one_arm_to_a_line_and_survives_a_second_pass() {
    let dir = scratch("fmt_match");
    let file = dir.join("shapes.dux");
    std::fs::write(
        &file,
        "fn main() {\n    shape := 1\n    match shape { 0 => println(\"zero\"), 1 | 2 if shape < 3 => println(\"small\"), x if x < 0 => println(\"negative\"), _ => println(\"many\") }\n}\n",
    )
    .unwrap();

    let run = duxc(&["fmt", file.to_str().unwrap()]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    let formatted = std::fs::read_to_string(&file).unwrap();
    assert_eq!(
        formatted,
        concat!(
            "fn main() {\n",
            "    shape := 1\n",
            "    match shape {\n",
            "        0 => println(\"zero\"),\n",
            "        1 | 2 if shape < 3 => println(\"small\"),\n",
            "        x if x < 0 => println(\"negative\"),\n",
            "        _ => println(\"many\"),\n",
            "    }\n",
            "}\n"
        ),
        "{formatted}"
    );
    let again = duxc(&["fmt", file.to_str().unwrap()]);
    assert_eq!(again.code, 0, "{}", again.stderr);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), formatted);
}

#[test]
fn formatter_output_is_a_fixed_point() {
    let dir = scratch("fmt_corpus");
    let source = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/e2e/flow.dux");
    let copy = dir.join("flow.dux");
    std::fs::copy(source, &copy).expect("the fixture copies to scratch");

    let run = duxc(&["fmt", copy.to_str().unwrap()]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    let run = duxc(&["fmt", copy.to_str().unwrap()]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert!(
        !run.stdout.contains("formatted"),
        "a second pass must change nothing: {}",
        run.stdout
    );
    let run = duxc(&["fmt", "--check", dir.to_str().unwrap()]);
    assert_eq!(run.code, 0, "{}", run.stderr);
}

#[test]
fn extern_signatures_are_printed_the_way_the_parser_reads_them() {
    let clean = "package main\n\nextern \"C\" {\n    fn floor(x: f64) -> f64\n}\n\nfn main() {\n    floor(1.0)\n}\n";
    let dir = scratch("fmt_extern");
    let file = dir.join("abi.dux");
    std::fs::write(&file, clean).unwrap();

    let run = duxc(&["fmt", file.to_str().unwrap()]);
    assert_eq!(run.code, 0, "{} {}", run.stdout, run.stderr);
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        clean,
        "an extern block round-trips: the grammar separates declarations with newlines"
    );
}
