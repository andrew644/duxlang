mod cli;

use cli::{Command, Emit, USAGE};
use dux_driver::{BuildRequest, Emit as DriverEmit, Optimization};
use std::io::Write;
use std::path::Path;
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
    }
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
    };
    let request = request(file, out_dir, release, driver_emit);
    match dux_driver::build(&request, out) {
        Ok(artifact) => {
            let written = match (out, emit) {
                (Some(path), _) => path.display().to_string(),
                (None, Emit::Exe) => artifact.executable.display().to_string(),
                (None, Emit::Object) => artifact.object.display().to_string(),
                (None, Emit::Ir) => artifact.object.with_extension("ll").display().to_string(),
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
