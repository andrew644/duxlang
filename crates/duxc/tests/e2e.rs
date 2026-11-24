use std::path::{Path, PathBuf};
use std::process::Command;

const FIXTURE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/e2e");
const EXAMPLE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../example");
const SCRATCH_DIR: &str = env!("CARGO_TARGET_TMPDIR");

#[derive(Debug)]
struct Spec {
    name: String,
    fixture: PathBuf,
    expected: PathBuf,
    expect_error: Option<String>,
    expected_exit: i32,
    expected_stderr: Option<String>,
    release: bool,
    program_args: Vec<String>,
}

impl Spec {
    fn load(path: PathBuf) -> Spec {
        let stem = path
            .file_stem()
            .expect("fixture has a file stem")
            .to_string_lossy()
            .to_string();
        let source = std::fs::read_to_string(&path).expect("fixture is readable");
        let expected = path.with_extension("expected");
        let mut spec = Spec {
            name: stem.clone(),
            fixture: path,
            expected,
            expect_error: None,
            expected_exit: 0,
            expected_stderr: None,
            release: false,
            program_args: Vec::new(),
        };
        for line in source.lines() {
            let Some(directive) = line.trim().strip_prefix("//@") else {
                continue;
            };
            let mut words = directive.split_whitespace();
            match words.next() {
                Some("exit") => {
                    spec.expected_exit = words
                        .next()
                        .and_then(|v| v.parse().ok())
                        .unwrap_or_else(|| panic!("{}: `//@exit` needs a number", spec.name));
                }
                Some("expect-error") => {
                    spec.expect_error = Some(
                        words
                            .next()
                            .unwrap_or_else(|| panic!("{}: `//@expect-error` needs a code", spec.name))
                            .to_string(),
                    );
                }
                Some("stderr") => {
                    spec.expected_stderr = Some(directive["stderr".len()..].trim().to_string());
                }
                Some("release") => spec.release = true,
                Some("args") => spec.program_args = words.map(String::from).collect(),
                Some(other) => panic!("{}: unknown directive `//@{other}`", spec.name),
                None => {}
            }
        }
        spec
    }
}

fn specs() -> Vec<Spec> {
    let mut found: Vec<Spec> = std::fs::read_dir(FIXTURE_DIR)
        .expect("tests/e2e exists")
        .filter_map(|entry| entry.expect("readable e2e entry").path().into())
        .filter(|path: &PathBuf| path.extension().is_some_and(|e| e == "dux"))
        .map(Spec::load)
        .collect();
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

struct Run {
    stdout: String,
    stderr: String,
    code: i32,
}

fn duxc(spec: &Spec, verb: &str) -> Run {
    let scratch = Path::new(SCRATCH_DIR).join(&spec.name);
    let _ = std::fs::remove_dir_all(&scratch);
    let mut command = Command::new(env!("CARGO_BIN_EXE_duxc"));
    command.arg(verb).arg(&spec.fixture);
    if spec.release {
        command.arg("--release");
    }
    if verb == "run" {
        command.arg("--out-dir").arg(&scratch);
        if !spec.program_args.is_empty() {
            command.arg("--");
            command.args(&spec.program_args);
        }
    }
    let output = command.output().expect("duxc is runnable");
    Run {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        code: output.status.code().unwrap_or(-1),
    }
}

fn update_expected() -> bool {
    std::env::var_os("UPDATE_EXPECTED").is_some()
}

fn evaluate(spec: &Spec) -> Result<(), String> {
    match &spec.expect_error {
        Some(code) => {
            let run = duxc(spec, "build");
            if run.code == 0 {
                return Err(format!(
                    "{}: expected a compile error {code}, but it compiled\nstdout: {}",
                    spec.name, run.stdout
                ));
            }
            if !run.stderr.contains(code) {
                return Err(format!(
                    "{}: expected diagnostic {code}, got:\n{}",
                    spec.name, run.stderr
                ));
            }
            Ok(())
        }
        None => {
            let run = duxc(spec, "run");
            if update_expected() {
                std::fs::write(&spec.expected, &run.stdout)
                    .expect("the .expected file is writable");
            }
            let expected = std::fs::read_to_string(&spec.expected).unwrap_or_else(|e| {
                panic!(
                    "{}: no {} ({e}); run `UPDATE_EXPECTED=1 cargo test` to create it",
                    spec.name,
                    spec.expected.display()
                )
            });
            if run.stdout != expected {
                return Err(format!(
                    "{}: stdout differs\n--- expected ---\n{expected}\n--- actual ---\n{}\n--- stderr ---\n{}",
                    spec.name, run.stdout, run.stderr
                ));
            }
            if run.code != spec.expected_exit {
                return Err(format!(
                    "{}: exit code {} but expected {}",
                    spec.name, run.code, spec.expected_exit
                ));
            }
            if let Some(needle) = &spec.expected_stderr {
                if !run.stderr.contains(needle) {
                    return Err(format!(
                        "{}: stderr does not contain {needle:?}, got:\n{}",
                        spec.name, run.stderr
                    ));
                }
            }
            Ok(())
        }
    }
}

#[test]
fn the_normative_hello_example_compiles_runs_and_prints_exactly_hello_world() {
    let hello = Path::new(EXAMPLE_DIR).join("hello.dux");
    let scratch = Path::new(SCRATCH_DIR).join("example_hello");
    let _ = std::fs::remove_dir_all(&scratch);
    let output = Command::new(env!("CARGO_BIN_EXE_duxc"))
        .arg("run")
        .arg(&hello)
        .arg("--out-dir")
        .arg(&scratch)
        .output()
        .expect("duxc is runnable");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert_eq!(stderr, "", "the normative hello example must compile cleanly");
    assert_eq!(output.status.code(), Some(0), "stdout: {stdout}");
    assert_eq!(stdout, "Hello, world!\n");
}

#[test]
fn a_built_binary_carries_the_line_table() {
    let hello = Path::new(EXAMPLE_DIR).join("hello.dux");
    let scratch = Path::new(SCRATCH_DIR).join("example_hello_debug");
    let _ = std::fs::remove_dir_all(&scratch);
    let output = Command::new(env!("CARGO_BIN_EXE_duxc"))
        .arg("build")
        .arg(&hello)
        .arg("--out-dir")
        .arg(&scratch)
        .output()
        .expect("duxc is runnable");
    assert_eq!(output.status.code(), Some(0), "build must succeed");
    let bytes = std::fs::read(scratch.join("hello")).expect("the built binary");
    let has_section = |needle: &str| {
        bytes
            .windows(needle.len())
            .any(|window| window == needle.as_bytes())
    };
    assert!(
        has_section(".debug_line"),
        "the binary must carry a DWARF line table"
    );
    assert!(
        has_section(".debug_info"),
        "the binary must carry DWARF subprogram info"
    );
}

#[test]
fn every_example_dumps_its_ast_so_milestone_gaps_never_hide_the_tree() {
    let scratch = Path::new(SCRATCH_DIR).join("ast-dumps");
    let _ = std::fs::remove_dir_all(&scratch);
    let mut offenders = Vec::new();
    for entry in std::fs::read_dir(EXAMPLE_DIR).expect("example/ exists") {
        let path = entry.expect("readable entry").path();
        if path.extension().is_some_and(|e| e != "dux") {
            continue;
        }
        let output = Command::new(env!("CARGO_BIN_EXE_duxc"))
            .arg("build")
            .arg(&path)
            .arg("--emit=ast")
            .arg("--out-dir")
            .arg(&scratch)
            .output()
            .expect("duxc is runnable");
        let stem = path.file_stem().expect("stem").to_string_lossy().to_string();
        let dump = scratch.join(format!("{stem}.ast"));
        if output.status.code() != Some(0) || !dump.exists() {
            offenders.push(format!(
                "{}: exit {:?}, dump exists {}, stderr: {}",
                path.display(),
                output.status.code(),
                dump.exists(),
                String::from_utf8_lossy(&output.stderr)
            ));
        }
    }
    assert!(
        offenders.is_empty(),
        "`--emit=ast` failed for:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn every_example_parses_so_the_review_samples_stay_the_target_surface() {
    let mut offenders = Vec::new();
    for entry in std::fs::read_dir(EXAMPLE_DIR).expect("example/ exists") {
        let path = entry.expect("readable entry").path();
        if path.extension().is_some_and(|e| e != "dux") {
            continue;
        }
        let run = duxc_check(&path);
        let syntax: Vec<&str> = run
            .stderr
            .lines()
            .filter(|line| {
                line.contains("error[E0001]")
                    || line.contains("error[E0002]")
                    || line.contains("error[E0005]")
                    || line.contains("error[E0006]")
                    || line.contains("error[E0008]")
            })
            .collect();
        if !syntax.is_empty() {
            offenders.push(format!("{}:\n{}", path.display(), syntax.join("\n")));
        }
    }
    assert!(
        offenders.is_empty(),
        "the parser cannot read these normative examples:\n\n{}",
        offenders.join("\n\n")
    );
}

fn duxc_check(path: &Path) -> Run {
    let output = Command::new(env!("CARGO_BIN_EXE_duxc"))
        .arg("check")
        .arg(path)
        .output()
        .expect("duxc is runnable");
    Run {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        code: output.status.code().unwrap_or(-1),
    }
}

#[test]
fn no_example_hangs_or_panics_the_compiler() {
    for entry in std::fs::read_dir(EXAMPLE_DIR).expect("example/ exists") {
        let path = entry.expect("readable entry").path();
        if path.extension().is_some_and(|e| e != "dux") {
            continue;
        }
        let output = Command::new("timeout")
            .arg("20")
            .arg(env!("CARGO_BIN_EXE_duxc"))
            .arg("check")
            .arg(&path)
            .output()
            .expect("`timeout` and duxc are both runnable");
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert_ne!(
            output.status.code(),
            Some(124),
            "{}: duxc did not finish in 20 seconds",
            path.display()
        );
        assert!(
            !stderr.contains("panicked"),
            "{}: duxc panicked\n{stderr}",
            path.display()
        );
    }
}

#[test]
fn the_corpus_is_not_empty() {
    assert!(
        !specs().is_empty(),
        "no .dux fixtures found in {FIXTURE_DIR}: the e2e gate would pass vacuously"
    );
}

#[test]
fn every_fixture_compiles_runs_and_matches_its_expected_stdout() {
    let failures: Vec<String> = specs().iter().filter_map(|spec| evaluate(spec).err()).collect();
    assert!(
        failures.is_empty(),
        "{} e2e fixture(s) failed:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

#[test]
fn every_run_fixture_ships_an_expected_file() {
    let missing: Vec<String> = specs()
        .into_iter()
        .filter(|s| s.expect_error.is_none() && !s.expected.exists())
        .map(|s| s.name)
        .collect();
    assert!(
        missing.is_empty(),
        "fixtures without a .expected file: {missing:?} (run `UPDATE_EXPECTED=1 cargo test`)"
    );
}
