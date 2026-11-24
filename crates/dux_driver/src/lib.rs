use dux_ast::Program;
use dux_codegen::{CompiledModule, OptLevel, Options};
use dux_diag::{code, render, Diagnostic, Severity};
use dux_parser::parse;
use dux_source::FileId;

pub use dux_codegen::OptLevel as Optimization;
pub use dux_source::SourceMap;

pub const DEFAULT_OUTPUT_DIR: &str = ".dux-cache";

#[derive(Debug)]
pub struct Analyzed {
    pub sources: SourceMap,
    pub file: FileId,
    pub program: Program,
    pub diagnostics: Vec<Diagnostic>,
}

impl Analyzed {
    pub fn has_errors(&self) -> bool {
        dux_diag::has_errors(&self.diagnostics)
    }
}

#[derive(Debug)]
pub struct Failure {
    pub sources: SourceMap,
    pub diagnostics: Vec<Diagnostic>,
}

impl Failure {
    fn bare(diagnostics: Vec<Diagnostic>) -> Failure {
        Failure {
            sources: SourceMap::new(),
            diagnostics,
        }
    }

    pub fn has_errors(&self) -> bool {
        dux_diag::has_errors(&self.diagnostics)
    }

    pub fn report(&self, stderr: &mut impl std::io::Write) {
        report(&self.sources, &self.diagnostics, stderr);
    }
}

pub fn report(sources: &SourceMap, diagnostics: &[Diagnostic], stderr: &mut impl std::io::Write) {
    for diagnostic in diagnostics {
        let _ = writeln!(stderr, "{}", render(sources, diagnostic));
    }
}

pub fn analyze(path: &std::path::Path) -> Result<Analyzed, Failure> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        Failure::bare(vec![Diagnostic::error(
            code::SOURCE_IO,
            format!("cannot read {}: {e}", path.display()),
        )])
    })?;
    let mut sources = SourceMap::new();
    let file = sources.add(path.display().to_string(), text);
    let lexed = dux_lexer::lex(&sources, file);
    let mut diagnostics = lexed.diagnostics;
    let parsed = parse(&sources, &lexed.tokens);
    diagnostics.extend(parsed.diagnostics);
    let syntax_clean = diagnostics
        .iter()
        .all(|d| d.severity != Severity::Error || d.code == code::UNSUPPORTED_IN_BUILD);
    if syntax_clean {
        diagnostics.extend(dux_hir::resolve(&parsed.program).diagnostics);
    }
    Ok(Analyzed {
        sources,
        file,
        program: parsed.program,
        diagnostics,
    })
}

#[derive(Debug)]
pub struct FormattedFile {
    pub original: String,
    pub text: String,
}

pub fn format_text(name: &str, text: &str) -> Result<String, Failure> {
    let mut sources = SourceMap::new();
    let file = sources.add(name, text);
    let lexed = dux_lexer::lex(&sources, file);
    let mut diagnostics = lexed.diagnostics;
    let parsed = parse(&sources, &lexed.tokens);
    diagnostics.extend(parsed.diagnostics);
    if dux_diag::has_errors(&diagnostics) {
        return Err(Failure {
            sources,
            diagnostics,
        });
    }
    Ok(dux_ast::fmt::format(&parsed.program))
}

pub fn format_path(path: &std::path::Path) -> Result<FormattedFile, Failure> {
    let original = std::fs::read_to_string(path).map_err(|e| {
        Failure::bare(vec![Diagnostic::error(
            code::SOURCE_IO,
            format!("cannot read {}: {e}", path.display()),
        )])
    })?;
    let text = format_text(&path.display().to_string(), &original)?;
    Ok(FormattedFile { original, text })
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Emit {
    Execute,
    Object,
    LlvmIr,
    Ast,
}

pub struct BuildRequest<'a> {
    pub source: &'a std::path::Path,
    pub out_dir: std::path::PathBuf,
    pub emit: Emit,
    pub opt_level: OptLevel,
}

#[derive(Debug)]
pub struct BuiltArtifact {
    pub object: std::path::PathBuf,
    pub executable: std::path::PathBuf,
    pub ir: String,
    pub data_layout: String,
}

fn compile(request: &BuildRequest) -> Result<(Analyzed, CompiledModule, BuiltArtifact), Failure> {
    let analyzed = analyze(request.source)?;
    if analyzed.has_errors() {
        return Err(Failure {
            sources: analyzed.sources,
            diagnostics: analyzed.diagnostics,
        });
    }
    let options = Options {
        opt_level: request.opt_level,
    };
    let module = match CompiledModule::build(&analyzed.program, options) {
        Ok(module) => module,
        Err(diagnostics) => {
            return Err(Failure {
                sources: analyzed.sources,
                diagnostics,
            })
        }
    };
    std::fs::create_dir_all(&request.out_dir).map_err(|e| {
        Failure::bare(vec![Diagnostic::error(
            code::SOURCE_IO,
            format!("cannot create {}: {e}", request.out_dir.display()),
        )])
    })?;
    let object = request
        .out_dir
        .join(format!("{}.o", output_stem(request.source)));
    let executable = request.out_dir.join(output_stem(request.source));
    module
        .write_object(&object)
        .map_err(|e| Failure::bare(vec![e.to_diagnostic()]))?;
    let artifact = BuiltArtifact {
        object,
        executable,
        ir: module.ir_text(),
        data_layout: module.data_layout(),
    };
    Ok((analyzed, module, artifact))
}

pub fn build(
    request: &BuildRequest,
    out_override: Option<&std::path::Path>,
) -> Result<BuiltArtifact, Failure> {
    if request.emit == Emit::Ast {
        return emit_ast(request, out_override);
    }
    let (_analyzed, _module, artifact) = compile(request)?;
    match request.emit {
        Emit::LlvmIr => {
            let path = out_override
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| artifact.object.with_extension("ll"));
            write_file(&path, artifact.ir.as_bytes())?;
        }
        Emit::Object => {
            if let Some(path) = out_override {
                std::fs::copy(&artifact.object, path).map_err(|e| io_failure(path, e))?;
            }
        }
        Emit::Execute => {
            link(&artifact)?;
            if let Some(path) = out_override {
                std::fs::copy(&artifact.executable, path).map_err(|e| io_failure(path, e))?;
            }
        }
        Emit::Ast => unreachable!("`--emit=ast` returns before code generation"),
    }
    Ok(artifact)
}

fn emit_ast(
    request: &BuildRequest,
    out_override: Option<&std::path::Path>,
) -> Result<BuiltArtifact, Failure> {
    let analyzed = analyze(request.source)?;
    let blocking: Vec<Diagnostic> = analyzed
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error && d.code != code::UNSUPPORTED_IN_BUILD)
        .cloned()
        .collect();
    if !blocking.is_empty() {
        return Err(Failure {
            sources: analyzed.sources,
            diagnostics: blocking,
        });
    }
    std::fs::create_dir_all(&request.out_dir).map_err(|e| {
        Failure::bare(vec![Diagnostic::error(
            code::SOURCE_IO,
            format!("cannot create {}: {e}", request.out_dir.display()),
        )])
    })?;
    let path = out_override
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| request.out_dir.join(format!("{}.ast", output_stem(request.source))));
    write_file(&path, dux_ast::dump::dump(&analyzed.program).as_bytes())?;
    Ok(BuiltArtifact {
        object: path.clone(),
        executable: path,
        ir: String::new(),
        data_layout: String::new(),
    })
}

pub fn run(
    request: &BuildRequest,
    args: &[String],
    stdout: &mut impl std::io::Write,
) -> Result<i32, Failure> {
    let artifact = build(request, None)?;
    let output = std::process::Command::new(&artifact.executable)
        .args(args)
        .stderr(std::process::Stdio::inherit())
        .output()
        .map_err(|e| {
            Failure::bare(vec![Diagnostic::error(
                code::LINK_FAILED,
                format!("cannot execute {}: {e}", artifact.executable.display()),
            )])
        })?;
    stdout
        .write_all(&output.stdout)
        .map_err(|e| Failure::bare(vec![io_diagnostic(e)]))?;
    let _ = stdout.flush();
    Ok(output.status.code().unwrap_or(-1))
}

fn write_file(path: &std::path::Path, bytes: &[u8]) -> Result<(), Failure> {
    std::fs::write(path, bytes).map_err(|e| io_failure(path, e))
}

fn io_failure(path: &std::path::Path, error: std::io::Error) -> Failure {
    Failure::bare(vec![Diagnostic::error(
        code::SOURCE_IO,
        format!("cannot write {}: {error}", path.display()),
    )])
}

fn io_diagnostic(error: std::io::Error) -> Diagnostic {
    Diagnostic::error(code::SOURCE_IO, format!("cannot write output: {error}"))
}

fn link(artifact: &BuiltArtifact) -> Result<(), Failure> {
    let linker = linker_command();
    let status = std::process::Command::new(&linker)
        .arg(&artifact.object)
        .arg("-o")
        .arg(&artifact.executable)
        .stderr(std::process::Stdio::inherit())
        .status()
        .map_err(|e| {
            Failure::bare(vec![
                Diagnostic::error(code::LINK_FAILED, format!("cannot run the linker `{linker}`: {e}"))
                    .with_note("set DUX_LINKER to pick a linker; DESIGN.md 6.3 expects clang"),
            ])
        })?;
    if !status.success() {
        return Err(Failure::bare(vec![Diagnostic::error(
            code::LINK_FAILED,
            format!("`{linker}` exited with a failure status"),
        )]));
    }
    Ok(())
}

fn linker_command() -> String {
    std::env::var("DUX_LINKER").unwrap_or_else(|_| "clang".to_string())
}

fn output_stem(source: &std::path::Path) -> String {
    source
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "dux_program".to_string())
}

pub fn warning_count(diagnostics: &[Diagnostic]) -> usize {
    diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Warning)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_stem_drops_the_extension() {
        assert_eq!(output_stem(std::path::Path::new("a/b/hello.dux")), "hello");
        assert_eq!(output_stem(std::path::Path::new("weird")), "weird");
    }

    #[test]
    fn missing_source_is_a_diagnostic_not_a_panic() {
        let failure = analyze(std::path::Path::new("/nonexistent/nope.dux")).unwrap_err();
        assert_eq!(failure.diagnostics[0].code, code::SOURCE_IO);
        assert!(failure.has_errors());
    }

    #[test]
    fn warnings_are_counted_separately_from_errors() {
        let diagnostics = vec![
            Diagnostic::warning("W0001", "unused"),
            Diagnostic::error(code::NO_MAIN, "nope"),
        ];
        assert_eq!(warning_count(&diagnostics), 1);
        assert!(dux_diag::has_errors(&diagnostics));
    }
}
