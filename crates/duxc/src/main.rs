mod cli;

use cli::{Command, Emit, USAGE};
use dux_driver::{BuildRequest, Emit as DriverEmit, Optimization};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const LLVM_VERSION: &str = "23.1";

fn main() -> ExitCode {
    let stderr = std::io::stderr();
    let mut err = stderr.lock();
    let command = match cli::parse(std::env::args_os().skip(1)) {
        Ok(command) => command,
        Err(problem) => {
            let _ = writeln!(err, "error: {}", problem.message());
            let _ = write!(err, "\n{USAGE}");
            return ExitCode::from(2);
        }
    };

    match command {
        Command::Help => {
            let _ = write!(std::io::stdout(), "{USAGE}");
            ExitCode::SUCCESS
        }
        Command::Version => {
            let _ = writeln!(
                std::io::stdout(),
                "dux {} (LLVM {}, inkwell feature llvm23-1)",
                env!("CARGO_PKG_VERSION"),
                LLVM_VERSION
            );
            ExitCode::SUCCESS
        }
        Command::Check { file } => check(&file, &mut err),
        Command::Build {
            file,
            out,
            out_dir,
            release,
            emit,
        } => build(&file, out.as_deref(), &out_dir, release, emit, &mut err),
        Command::Run {
            file,
            out_dir,
            release,
            args,
        } => {
            let request = request(&file, &out_dir, release, DriverEmit::Execute);
            let mut stdout = std::io::stdout();
            match dux_driver::run(&request, &args, &mut stdout) {
                Ok(code) => ExitCode::from(exit_of(code)),
                Err(failure) => {
                    failure.report(&mut err);
                    ExitCode::FAILURE
                }
            }
        }
        Command::Fmt { paths, check } => fmt(&paths, check, &mut err),
    }
}

fn fmt(paths: &[PathBuf], check: bool, err: &mut impl Write) -> ExitCode {
    let mut files = Vec::new();
    let mut failed = false;
    let roots: Vec<&Path> = if paths.is_empty() {
        vec![Path::new(".")]
    } else {
        paths.iter().map(|p| p.as_path()).collect()
    };
    for root in roots {
        if !collect_dux_files(root, &mut files, err) {
            failed = true;
        }
    }
    files.sort();
    files.dedup();
    let mut unformatted = Vec::new();
    for file in &files {
        match dux_driver::format_path(file) {
            Ok(formatted) => {
                if formatted.text == formatted.original {
                    continue;
                }
                if check {
                    unformatted.push(file.clone());
                } else {
                    match std::fs::write(file, &formatted.text) {
                        Ok(()) => {
                            let _ = writeln!(std::io::stdout(), "formatted {}", file.display());
                        }
                        Err(e) => {
                            report_io(err, file, e);
                            failed = true;
                        }
                    }
                }
            }
            Err(failure) => {
                failure.report(err);
                failed = true;
            }
        }
    }
    if failed {
        return ExitCode::FAILURE;
    }
    if check {
        if !unformatted.is_empty() {
            for file in &unformatted {
                let _ = writeln!(err, "not formatted: {}", file.display());
            }
            return ExitCode::FAILURE;
        }
        let _ = writeln!(
            std::io::stdout(),
            "all {} file(s) formatted",
            files.len()
        );
    }
    ExitCode::SUCCESS
}

fn collect_dux_files(root: &Path, files: &mut Vec<PathBuf>, err: &mut impl Write) -> bool {
    if root.is_dir() {
        let entries = match std::fs::read_dir(root) {
            Ok(entries) => entries,
            Err(e) => {
                report_io(err, root, e);
                return false;
            }
        };
        let mut children: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .collect();
        children.sort();
        let mut ok = true;
        for child in children {
            if child.is_dir() {
                ok &= collect_dux_files(&child, files, err);
            } else if child.extension().is_some_and(|e| e == "dux") {
                files.push(child);
            }
        }
        ok
    } else if root.extension().is_some_and(|e| e == "dux") {
        files.push(root.to_path_buf());
        true
    } else {
        let diagnostic = dux_diag::Diagnostic::error(
            dux_diag::code::SOURCE_IO,
            format!("{} is not a .dux file or directory", root.display()),
        );
        dux_driver::report(&dux_driver::SourceMap::new(), &[diagnostic], err);
        false
    }
}

fn report_io(err: &mut impl Write, path: &Path, error: std::io::Error) {
    let diagnostic = dux_diag::Diagnostic::error(
        dux_diag::code::SOURCE_IO,
        format!("cannot access {}: {error}", path.display()),
    );
    dux_driver::report(&dux_driver::SourceMap::new(), &[diagnostic], err);
}

fn check(file: &Path, err: &mut impl Write) -> ExitCode {
    match dux_driver::analyze(file) {
        Ok(analyzed) => {
            dux_driver::report(&analyzed.sources, &analyzed.diagnostics, err);
            if analyzed.has_errors() {
                return ExitCode::FAILURE;
            }
            let _ = writeln!(
                std::io::stdout(),
                "no errors: package {}, {} function(s), {} warning(s)",
                analyzed
                    .program
                    .package
                    .as_ref()
                    .map(|p| p.name.as_str())
                    .unwrap_or("<none>"),
                analyzed.program.functions().count(),
                dux_driver::warning_count(&analyzed.diagnostics)
            );
            ExitCode::SUCCESS
        }
        Err(failure) => {
            failure.report(err);
            ExitCode::FAILURE
        }
    }
}

fn build(
    file: &Path,
    out: Option<&Path>,
    out_dir: &Path,
    release: bool,
    emit: Emit,
    err: &mut impl Write,
) -> ExitCode {
    let driver_emit = match emit {
        Emit::Exe => DriverEmit::Execute,
        Emit::Object => DriverEmit::Object,
        Emit::Ir => DriverEmit::LlvmIr,
        Emit::Ast => DriverEmit::Ast,
    };
    let request = request(file, out_dir, release, driver_emit);
    match dux_driver::build(&request, out) {
        Ok(artifact) => {
            let written = match (out, emit) {
                (Some(path), _) => path.display().to_string(),
                (None, Emit::Exe) => artifact.executable.display().to_string(),
                (None, Emit::Object) => artifact.object.display().to_string(),
                (None, Emit::Ir) => artifact.object.with_extension("ll").display().to_string(),
                (None, Emit::Ast) => artifact.object.display().to_string(),
            };
            let _ = writeln!(std::io::stdout(), "wrote {written}");
            ExitCode::SUCCESS
        }
        Err(failure) => {
            failure.report(err);
            ExitCode::FAILURE
        }
    }
}

fn request<'a>(
    file: &'a Path,
    out_dir: &Path,
    release: bool,
    emit: DriverEmit,
) -> BuildRequest<'a> {
    BuildRequest {
        source: file,
        out_dir: out_dir.to_path_buf(),
        emit,
        opt_level: if release {
            Optimization::Balanced
        } else {
            Optimization::None
        },
    }
}

fn exit_of(code: i32) -> u8 {
    if code == 0 {
        0
    } else if !(0..=255).contains(&code) {
        1
    } else {
        code as u8
    }
}

#[cfg(test)]
mod tests {
    use super::exit_of;

    #[test]
    fn child_exit_codes_are_clamped_to_a_byte() {
        assert_eq!(exit_of(0), 0);
        assert_eq!(exit_of(3), 3);
        assert_eq!(exit_of(-1), 1);
        assert_eq!(exit_of(300), 1);
    }

    #[test]
    fn version_names_the_llvm_it_was_built_against() {
        assert_eq!(super::LLVM_VERSION, "23.1");
    }
}
