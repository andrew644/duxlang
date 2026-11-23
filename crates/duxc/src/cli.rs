use std::ffi::OsString;
use std::path::PathBuf;

pub const USAGE: &str = "\
dux — a small systems language for games

USAGE:
    duxc <COMMAND> [FILE] [OPTIONS] [-- PROGRAM ARGS]

COMMANDS:
    version           print the compiler version
    check <file>      lex, parse and report diagnostics without code generation
    build <file>      compile and link an executable
    run <file>        compile, link and execute, forwarding program arguments

OPTIONS:
    -o <path>         write the result to <path>
    --out-dir <dir>   scratch directory for objects (default: .dux-cache)
    --release         optimise (default<O2>) instead of -O0
    --emit=ir         write textual LLVM IR instead of linking
    --emit=obj        keep the object file
    --emit=exe        link an executable (default)
    --help            show this message
";

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Emit {
    Exe,
    Object,
    Ir,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Version,
    Help,
    Check { file: PathBuf },
    Build {
        file: PathBuf,
        out: Option<PathBuf>,
        out_dir: PathBuf,
        release: bool,
        emit: Emit,
    },
    Run {
        file: PathBuf,
        out_dir: PathBuf,
        release: bool,
        args: Vec<String>,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub enum UsageError {
    NoCommand,
    UnknownCommand(String),
    MissingFile(&'static str),
    BadEmit(String),
    MissingValue(String),
    ExpectedOption(String),
}

impl UsageError {
    pub fn message(&self) -> String {
        match self {
            UsageError::NoCommand => "expected a command".to_string(),
            UsageError::UnknownCommand(name) => format!("unknown command `{name}`"),
            UsageError::MissingFile(command) => {
                format!("`{command}` expects a .dux file")
            }
            UsageError::BadEmit(value) => {
                format!("`--emit={value}` is not one of ir, obj, exe")
            }
            UsageError::MissingValue(option) => format!("`{option}` expects a value"),
            UsageError::ExpectedOption(text) => format!("unexpected argument `{text}`"),
        }
    }
}

pub fn parse<I: IntoIterator<Item = OsString>>(argv: I) -> Result<Command, UsageError> {
    let argv: Vec<String> = argv
        .into_iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let Some(command) = argv.first() else {
        return Err(UsageError::NoCommand);
    };
    let rest = &argv[1..];
    match command.as_str() {
        "version" | "--version" | "-V" => Ok(Command::Version),
        "help" | "--help" | "-h" => Ok(Command::Help),
        "check" => {
            let file = file_arg(rest)?;
            Ok(Command::Check { file })
        }
        "build" | "b" => {
            let mut file = None;
            let mut out = None;
            let mut out_dir = PathBuf::from(dux_defaults::OUT_DIR);
            let mut release = false;
            let mut emit = Emit::Exe;
            let mut index = 0;
            while index < rest.len() {
                let arg = rest[index].as_str();
                index += 1;
                match arg {
                    "-o" => out = Some(PathBuf::from(value(rest, &mut index, "-o")?)),
                    "--out-dir" => out_dir = PathBuf::from(value(rest, &mut index, "--out-dir")?),
                    "--release" => release = true,
                    "--help" | "-h" => return Ok(Command::Help),
                    other if other.starts_with("--emit=") => {
                        emit = match other.trim_start_matches("--emit=") {
                            "ir" | "llvm-ir" => Emit::Ir,
                            "obj" | "object" => Emit::Object,
                            "exe" | "executable" => Emit::Exe,
                            other => return Err(UsageError::BadEmit(other.to_string())),
                        }
                    }
                    other if other.starts_with('-') && other.len() > 1 => {
                        return Err(UsageError::ExpectedOption(other.to_string()))
                    }
                    other => {
                        if file.is_some() {
                            return Err(UsageError::ExpectedOption(other.to_string()));
                        }
                        file = Some(PathBuf::from(other));
                    }
                }
            }
            Ok(Command::Build {
                file: file.ok_or(UsageError::MissingFile("build"))?,
                out,
                out_dir,
                release,
                emit,
            })
        }
        "run" | "r" => {
            let mut file = None;
            let mut out_dir = PathBuf::from(dux_defaults::OUT_DIR);
            let mut release = false;
            let mut args = Vec::new();
            let mut index = 0;
            while index < rest.len() {
                let arg = rest[index].as_str();
                index += 1;
                if arg == "--" {
                    args.extend(rest[index..].iter().cloned());
                    break;
                }
                match arg {
                    "--out-dir" => out_dir = PathBuf::from(value(rest, &mut index, "--out-dir")?),
                    "--release" => release = true,
                    other if file.is_none() && !other.starts_with('-') => {
                        file = Some(PathBuf::from(other))
                    }
                    other => return Err(UsageError::ExpectedOption(other.to_string())),
                }
            }
            Ok(Command::Run {
                file: file.ok_or(UsageError::MissingFile("run"))?,
                out_dir,
                release,
                args,
            })
        }
        other => Err(UsageError::UnknownCommand(other.to_string())),
    }
}

fn file_arg(rest: &[String]) -> Result<PathBuf, UsageError> {
    rest.first()
        .map(PathBuf::from)
        .ok_or(UsageError::MissingFile("check"))
}

fn value(rest: &[String], index: &mut usize, option: &str) -> Result<String, UsageError> {
    let taken = rest.get(*index).cloned().ok_or(UsageError::MissingValue(option.to_string()))?;
    *index += 1;
    Ok(taken)
}

pub mod dux_defaults {
    pub const OUT_DIR: &str = ".dux-cache";
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<OsString> {
        items.iter().map(OsString::from).collect()
    }

    #[test]
    fn version_and_help_need_no_file() {
        assert_eq!(parse(argv(&["version"])), Ok(Command::Version));
        assert_eq!(parse(argv(&["--help"])), Ok(Command::Help));
    }

    #[test]
    fn check_takes_a_single_file() {
        assert_eq!(
            parse(argv(&["check", "app.dux"])),
            Ok(Command::Check {
                file: PathBuf::from("app.dux")
            })
        );
        assert_eq!(
            parse(argv(&["check"])),
            Err(UsageError::MissingFile("check"))
        );
    }

    #[test]
    fn build_options_are_parsed_in_any_order() {
        assert_eq!(
            parse(argv(&["build", "app.dux", "--release", "-o", "out/app", "--emit=obj"])),
            Ok(Command::Build {
                file: PathBuf::from("app.dux"),
                out: Some(PathBuf::from("out/app")),
                out_dir: PathBuf::from(".dux-cache"),
                release: true,
                emit: Emit::Object,
            })
        );
    }

    #[test]
    fn out_dir_defaults_and_overrides() {
        assert_eq!(
            parse(argv(&["build", "app.dux"])).unwrap(),
            Command::Build {
                file: PathBuf::from("app.dux"),
                out: None,
                out_dir: PathBuf::from(".dux-cache"),
                release: false,
                emit: Emit::Exe,
            }
        );
        let Command::Build { out_dir, .. } = parse(argv(&["build", "a.dux", "--out-dir", "tmp"])).unwrap() else {
            panic!("expected build");
        };
        assert_eq!(out_dir, PathBuf::from("tmp"));
    }

    #[test]
    fn run_forwards_everything_after_a_double_dash() {
        let parsed = parse(argv(&["run", "app.dux", "--", "-x", "--release", "tail"])).unwrap();
        let Command::Run { file, args, .. } = parsed else {
            panic!("expected run")
        };
        assert_eq!(file, PathBuf::from("app.dux"));
        assert_eq!(args, vec!["-x", "--release", "tail"]);
    }

    #[test]
    fn bad_input_is_rejected_with_a_message() {
        assert_eq!(parse(argv(&[])), Err(UsageError::NoCommand));
        assert_eq!(
            parse(argv(&["teleport", "a.dux"])),
            Err(UsageError::UnknownCommand("teleport".to_string()))
        );
        assert_eq!(
            parse(argv(&["build", "a.dux", "--emit=ptx"])),
            Err(UsageError::BadEmit("ptx".to_string()))
        );
        assert_eq!(
            parse(argv(&["build", "a.dux", "-o"])),
            Err(UsageError::MissingValue("-o".to_string()))
        );
        assert_eq!(
            parse(argv(&["build", "a.dux", "-Werror"])),
            Err(UsageError::ExpectedOption("-Werror".to_string()))
        );
    }
}
