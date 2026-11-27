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
    require_clean_stderr: bool,
    release: bool,
    debug_only: bool,
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
            require_clean_stderr: false,
            release: false,
            debug_only: false,
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
                            .unwrap_or_else(|| {
                                panic!("{}: `//@expect-error` needs a code", spec.name)
                            })
                            .to_string(),
                    );
                }
                Some("stderr") => {
                    spec.expected_stderr = Some(directive["stderr".len()..].trim().to_string());
                }
                Some("stderr_empty") => spec.require_clean_stderr = true,
                Some("release") => spec.release = true,
                Some("debug_only") => spec.debug_only = true,
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

/// The `-O2` promise: past the first optimization the program is the same
/// program. Every fixture that does not live in debug machinery runs in the
/// release build too, and its stdout must arrive exactly as the debug build
/// pinned it. Fixtures marked `//@debug_only` are the debug runtime's own —
/// guard pages, poison, bounds traps — and the release build never
/// promised them. A leak report is debug-only speech too, so the release
/// road reads stdout and exit codes and stays quiet about stderr.
#[test]
fn the_release_build_prints_what_the_debug_build_pinned_across_the_corpus() {
    let failures: Vec<String> = specs()
        .into_iter()
        .filter(|spec| !spec.debug_only && !spec.release)
        .filter_map(|spec| evaluate_release(spec).err())
        .collect();
    assert!(
        failures.is_empty(),
        "{} release fixture(s) disagreed with their debug pins:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

fn evaluate_release(spec: Spec) -> Result<(), String> {
    let spec = Spec {
        release: true,
        ..spec
    };
    let run = duxc(&spec, "run");
    if let Some(code) = &spec.expect_error {
        if run.code == 0 {
            return Err(format!(
                "{}: the release build compiled a program the debug build refused",
                spec.name
            ));
        }
        if !run.stderr.contains(code.as_str()) {
            return Err(format!(
                "{}: the release build refused with something else:\n{}",
                spec.name, run.stderr
            ));
        }
        return Ok(());
    }
    let expected = std::fs::read_to_string(&spec.expected)
        .unwrap_or_else(|e| panic!("{}: no .expected ({e})", spec.name));
    if run.stdout != expected {
        return Err(format!(
            "{}: release stdout differs\n--- expected ---\n{expected}--- actual ---\n{}",
            spec.name, run.stdout
        ));
    }
    if run.code != spec.expected_exit {
        return Err(format!(
            "{}: release exit code {} but expected {}",
            spec.name, run.code, spec.expected_exit
        ));
    }
    if spec.require_clean_stderr && !run.stderr.is_empty() {
        return Err(format!(
            "{}: release stderr is not empty:\n{}",
            spec.name, run.stderr
        ));
    }
    Ok(())
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
            if spec.require_clean_stderr && !run.stderr.is_empty() {
                return Err(format!(
                    "{}: stderr is not empty:\n{}",
                    spec.name, run.stderr
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
    assert_eq!(
        stderr, "",
        "the normative hello example must compile cleanly"
    );
    assert_eq!(output.status.code(), Some(0), "stdout: {stdout}");
    assert_eq!(stdout, "Hello, world!\n");
}

#[test]
fn the_normative_sum_types_example_compiles_runs_and_prints_the_shapes() {
    let sums = Path::new(EXAMPLE_DIR).join("sum_types.dux");
    let scratch = Path::new(SCRATCH_DIR).join("example_sum_types");
    let _ = std::fs::remove_dir_all(&scratch);
    let output = Command::new(env!("CARGO_BIN_EXE_duxc"))
        .arg("run")
        .arg(&sums)
        .arg("--out-dir")
        .arg(&scratch)
        .output()
        .expect("duxc is runnable");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        !stderr.contains("error["),
        "the sum-type example must compile cleanly:\n{stderr}"
    );
    assert_eq!(output.status.code(), Some(0), "stdout: {stdout}");
    assert_eq!(
        stdout,
        "area=12.5664\narea=12\narea=0\n0 -> zero\n2 -> small\n7 -> single digit\n\
         -5 -> negative -5\n42 -> big\nload save1.dux\n"
    );
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
        if path.extension().is_none_or(|e| e != "dux") {
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
        let stem = path
            .file_stem()
            .expect("stem")
            .to_string_lossy()
            .to_string();
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
        if path.extension().is_none_or(|e| e != "dux") {
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
fn the_generics_example_is_the_m5_proof_that_runs() {
    let path = Path::new(EXAMPLE_DIR).join("generics.dux");
    let scratch = Path::new(SCRATCH_DIR).join("generics-example");
    let _ = std::fs::remove_dir_all(&scratch);
    let output = Command::new(env!("CARGO_BIN_EXE_duxc"))
        .arg("run")
        .arg(&path)
        .arg("--out-dir")
        .arg(&scratch)
        .output()
        .expect("duxc is runnable");
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "7\n2.5\n9\npair=1 one grid0=0\nright 42\nsum=6 hp=30 mp=10 keys=2\n"
    );
}

#[test]
fn no_example_hangs_or_panics_the_compiler() {
    for entry in std::fs::read_dir(EXAMPLE_DIR).expect("example/ exists") {
        let path = entry.expect("readable entry").path();
        if path.extension().is_none_or(|e| e != "dux") {
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
    let failures: Vec<String> = specs()
        .iter()
        .filter_map(|spec| evaluate(spec).err())
        .collect();
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

fn run_example(stem: &str) -> Run {
    let path = Path::new(EXAMPLE_DIR).join(format!("{stem}.dux"));
    let scratch = Path::new(SCRATCH_DIR).join(format!("example_{stem}"));
    let _ = std::fs::remove_dir_all(&scratch);
    let output = Command::new(env!("CARGO_BIN_EXE_duxc"))
        .arg("run")
        .arg(&path)
        .arg("--out-dir")
        .arg(&scratch)
        .output()
        .expect("duxc is runnable");
    Run {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        code: output.status.code().unwrap_or(-1),
    }
}

#[test]
fn the_normative_pointers_example_walks_a_list_with_no_null_in_it() {
    let run = run_example("pointers");
    assert_eq!(run.stderr, "", "the pointer example must compile cleanly");
    assert_eq!(run.code, 0, "stdout: {}", run.stdout);
    assert_eq!(
        run.stdout,
        "n=3 through the pointer=3\nhops=2\nlen=4 first=10 last=40\n[0]=20\n[1]=30\n\n\
         base=10 third=30\nthrough rawptr=30\n"
    );
}

#[test]
fn the_normative_control_flow_example_runs_every_loop_and_a_match() {
    let run = run_example("control_flow");
    assert_eq!(
        run.stderr, "",
        "the control-flow example must compile cleanly"
    );
    assert_eq!(run.code, 0, "stdout: {}", run.stdout);
    assert_eq!(
        run.stdout,
        "i=1\ni=2\ni=3\n10 20 30 \n[0]=10 [1]=20 [2]=30 \ntick tick tick \ntock tock tock \n\
         x=5\nodd medium\nbody\nruns first\nruns second\n"
    );
}

#[test]
fn the_debugger_stops_on_dux_lines_and_prints_dux_values() {
    let fixture = Path::new(FIXTURE_DIR).join("debug_types.dux");
    let source = std::fs::read_to_string(&fixture).expect("the fixture is readable");
    let printed = source
        .lines()
        .position(|line| line.trim_start().starts_with("println"))
        .expect("the fixture prints something")
        + 1;
    let scratch = Path::new(SCRATCH_DIR).join("lldb-smoke");
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).expect("scratch is writable");
    let object = scratch.join("program");
    let build = Command::new(env!("CARGO_BIN_EXE_duxc"))
        .arg("build")
        .arg(&fixture)
        .arg("-o")
        .arg(&object)
        .arg("--out-dir")
        .arg(&scratch)
        .output()
        .expect("the compiler runs");
    assert_eq!(
        build.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let debugger = ["lldb-23", "lldb"]
        .iter()
        .map(|tool| {
            Command::new(tool)
                .arg("-b")
                .args([
                    "-o",
                    &format!("breakpoint set --file debug_types.dux --line {printed}"),
                ])
                .args(["-o", "run"])
                .args(["-o", "frame variable"])
                .args(["-o", "continue"])
                .arg(&object)
                .output()
        })
        .find_map(|attempt| attempt.ok())
        .expect("lldb is runnable");
    let saw = String::from_utf8_lossy(&debugger.stdout).into_owned();
    assert!(
        debugger.status.success(),
        "lldb failed:{}",
        String::from_utf8_lossy(&debugger.stderr)
    );
    for wanted in [
        "debug_types.dux:",
        "(Vec3) big = (x = 2, y = 4, z = 6)",
        "(int[3]) cells = ([0] = 4, [1] = 5, [2] = 6)",
        "length = 2",
        "Tick = (0 = 9)",
        "exited with status = 0",
    ] {
        assert!(
            saw.contains(wanted),
            "the debugger never showed {wanted}:\n{saw}"
        );
    }
}

#[test]
fn the_debug_info_describes_the_program_the_way_a_debugger_reads_it() {
    let fixture = Path::new(FIXTURE_DIR).join("debug_types.dux");
    let scratch = Path::new(SCRATCH_DIR).join("debug-types");
    let _ = std::fs::remove_dir_all(&scratch);
    let object = scratch.join("program");
    std::fs::create_dir_all(&scratch).expect("scratch is writable");
    let build = Command::new(env!("CARGO_BIN_EXE_duxc"))
        .arg("build")
        .arg(&fixture)
        .arg("-o")
        .arg(&object)
        .arg("--out-dir")
        .arg(&scratch)
        .output()
        .expect("the compiler runs");
    assert_eq!(
        build.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let dump = ["llvm-dwarfdump-23", "llvm-dwarfdump"]
        .iter()
        .map(|tool| Command::new(tool).arg("--debug-info").arg(&object).output())
        .find_map(|attempt| attempt.ok())
        .expect("llvm-dwarfdump is runnable");
    assert!(
        dump.status.success(),
        "llvm-dwarfdump failed: {}",
        String::from_utf8_lossy(&dump.stderr)
    );
    let dwarf = String::from_utf8_lossy(&dump.stdout).into_owned();
    for wanted in [
        "DW_TAG_structure_type",
        "DW_AT_name\t(\"Vec3\")",
        "DW_AT_name\t(\"x\")",
        "DW_AT_name\t(\"Event::Tick\")",
        "DW_TAG_array_type",
        "DW_TAG_variable",
        "DW_AT_name\t(\"big\")",
        "DW_AT_name\t(\"k\")",
    ] {
        assert!(
            dwarf.contains(wanted),
            "the debug info does not say {wanted}:\n{dwarf}"
        );
    }
}

#[test]
fn cimports_names_what_crossed_and_what_the_header_kept_back() {
    let report = Path::new(FIXTURE_DIR).join("c_header_lib.dux");
    let text = Command::new(env!("CARGO_BIN_EXE_duxc"))
        .args(["cimports", report.to_str().expect("utf-8 path")])
        .output()
        .expect("the compiler runs");
    let said = String::from_utf8_lossy(&text.stdout);
    assert!(
        said.contains("edge (cedge.h)") && said.contains("5 declaration(s) crossed"),
        "the namespace reports itself:\n{said}"
    );
    assert!(
        said.contains("cedge_visits - the variable belongs to one translation unit"),
        "a name that did not cross says why:\n{said}"
    );
    let json = Command::new(env!("CARGO_BIN_EXE_duxc"))
        .args(["cimports", "--json", report.to_str().expect("utf-8 path")])
        .output()
        .expect("the compiler runs");
    let said = String::from_utf8_lossy(&json.stdout);
    assert!(
        said.starts_with("[{\"namespace\":\"edge\"") && said.contains("\"skipped\":["),
        "the same report, machine-readable:\n{said}"
    );
}
