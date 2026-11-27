use dux_ast::{
    AttributeArgument, Block, Decl, Expr, FnDecl, Ident, Import, ModuleDecl, Program, Stmt,
};
use dux_codegen::{CompiledModule, OptLevel, Options};
use dux_diag::{code, render, Diagnostic, Severity};
use dux_parser::parse;
use dux_source::{FileId, Span};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub use dux_codegen::BoundsCheck;
pub use dux_codegen::OptLevel as Optimization;
pub use dux_source::SourceMap;

pub const DEFAULT_OUTPUT_DIR: &str = ".dux-cache";

#[derive(Debug)]
pub struct Analyzed {
    pub sources: SourceMap,
    pub file: FileId,
    pub program: Program,
    pub diagnostics: Vec<Diagnostic>,
    /// The C translation units the program asked for, ready to compile.
    pub c_units: Vec<CUnit>,
    /// What each `#[c_include]` namespace became: the names that crossed
    /// and the names that did not, with the reason each one did not.
    pub c_namespaces: Vec<CNamespace>,
}

/// One `#[c_include]` namespace, as the reader saw it. This is what
/// `dux cimports` reads out.
#[derive(Debug)]
pub struct CNamespace {
    pub namespace: String,
    pub header: String,
    pub crossed: Vec<String>,
    pub skipped: Vec<dux_cinterop::Skipped>,
}

/// One `#[c_include]` namespace's translation unit: the source (include
/// plus the wrappers that give `static inline` definitions a name), and
/// everything the compiler needs to make bitcode of it.
#[derive(Clone, Debug)]
pub struct CUnit {
    pub headers: Vec<String>,
    pub source: String,
    pub flags: Vec<String>,
    pub include_paths: Vec<String>,
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
    let mut program = parsed.program;
    diagnostics.extend(parsed.diagnostics);
    // `std/prelude` is read into every program (DESIGN 3.1). Its declarations
    // arrive bare, not under a namespace: the prelude is not a place names
    // live in, it is the set of names every program starts with. Where it is
    // not there, the universe scope is what the program has, as it was.
    read_prelude(path, &mut sources, &mut program, &mut diagnostics);
    // An import brings a file in beside the one that was named, and what it
    // brings is a namespace: the declarations arrive under the name the import
    // wrote, so `import "std/math"` makes `math.sqrt` mean something.
    let mut imported = load_imports(path, &mut sources, &mut program, &mut diagnostics);
    program.decls.append(&mut imported);
    // A `#[c_include("...")] mod ns;` keeps its promise the slow way: the
    // header is read here, once, and the namespace fills with the Dux
    // declarations the mapping table writes. From this point the importer's
    // output travels the same road as any other module body, because it is.
    let mut c_namespaces = Vec::new();
    let c_units = fill_c_modules(
        path,
        &mut sources,
        &mut program,
        &mut diagnostics,
        &mut c_namespaces,
    );
    let syntax_clean = diagnostics
        .iter()
        .all(|d| d.severity != Severity::Error || d.code == code::UNSUPPORTED_IN_BUILD);
    if syntax_clean {
        let resolved = dux_hir::resolve(&program);
        let names_clean = resolved
            .diagnostics
            .iter()
            .all(|d| d.severity != Severity::Error);
        diagnostics.extend(resolved.diagnostics);
        if names_clean {
            diagnostics.extend(dux_hir::check(&program));
        }
    }
    Ok(Analyzed {
        sources,
        file,
        program,
        diagnostics,
        c_units,
        c_namespaces,
    })
}

/// What the program's C imports saw, without compiling anything. The road
/// runs the same analysis the build runs; only the report differs.
pub fn cimports(path: &Path) -> Result<Vec<CNamespace>, Failure> {
    Ok(analyze(path)?.c_namespaces)
}

/// `std/prelude`, spliced in as though it were written above every program's
/// own declarations. A program outside a tree that carries a standard library
/// simply has no prelude, which is what it had before there was one.
fn read_prelude(
    entry: &Path,
    sources: &mut SourceMap,
    program: &mut Program,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let Some(file) = find_module(&module_roots(entry), PRELUDE) else {
        return;
    };
    let Ok(text) = std::fs::read_to_string(&file) else {
        diagnostics.push(Diagnostic::error(
            code::SOURCE_IO,
            format!("cannot read {}: {}", file.display(), PRELUDE),
        ));
        return;
    };
    let id = sources.add(file.display().to_string(), text);
    let lexed = dux_lexer::lex(sources, id);
    diagnostics.extend(lexed.diagnostics);
    let parsed = parse(sources, &lexed.tokens);
    diagnostics.extend(parsed.diagnostics);
    program.imports.extend(parsed.program.imports);
    program.decls.extend(parsed.program.decls);
}

/// Where `import "a/b"` looks: a module path is resolved against the module
/// root — every directory above the file that was named, nearest first — and
/// then against the standard library root, which `$DUX_STDLIB` names.
pub fn module_roots(entry: &Path) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = entry
        .parent()
        .map(|dir| dir.ancestors().map(|root| root.to_path_buf()).collect())
        .unwrap_or_default();
    if let Some(library) = std::env::var_os("DUX_STDLIB") {
        roots.push(PathBuf::from(library));
    }
    roots
}

/// The module every program is read on top of.
const PRELUDE: &str = "std/prelude";

fn find_module(roots: &[PathBuf], path: &str) -> Option<PathBuf> {
    roots
        .iter()
        .map(|root| root.join(path).with_extension("dux"))
        .find(|candidate| candidate.is_file())
}

/// Every module the program imports, and every module those import, each one
/// wrapped in the namespace it was asked for. A file two modules import is
/// read once, which is also what stops a cycle.
fn load_imports(
    entry: &Path,
    sources: &mut SourceMap,
    program: &mut Program,
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<Decl> {
    let roots = module_roots(entry);
    let mut queue: Vec<(Import, usize)> = program
        .imports
        .iter()
        .cloned()
        .enumerate()
        .map(|(depth, import)| (import, depth))
        .collect();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut modules = Vec::new();
    while let Some((import, depth)) = queue.pop() {
        let Some(file) = find_module(&roots, &import.path) else {
            let searched = roots
                .iter()
                .map(|root| root.join(&import.path).with_extension("dux"))
                .map(|candidate| candidate.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            // A `std/..` path names this compiler's own standard library,
            // which arrives a piece at a time: what is not here yet is a
            // milestone gap, not a verdict on the program. Any other path is
            // the program's own module root, and a miss there is its error.
            let missing = format!("there is no module `{}`", import.path);
            let diagnostic = if import.path.starts_with("std/") {
                Diagnostic::error(
                    code::UNSUPPORTED_IN_BUILD,
                    format!(
                        "the standard library does not ship `{}` in this build yet",
                        import.path
                    ),
                )
            } else {
                Diagnostic::error(code::UNDECLARED, missing)
            };
            diagnostics.push(
                diagnostic
                    .with_span(import.span)
                    .with_note(format!("looked for it at: {searched}")),
            );
            continue;
        };
        if !seen.insert(file.clone()) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&file) else {
            diagnostics.push(Diagnostic::error(
                code::SOURCE_IO,
                format!("cannot read {}: {}", file.display(), import.path),
            ));
            continue;
        };
        let namespace = import.path.rsplit('/').next().unwrap_or(&import.path);
        let id = sources.add(file.display().to_string(), text);
        let lexed = dux_lexer::lex(sources, id);
        diagnostics.extend(lexed.diagnostics);
        let parsed = parse(sources, &lexed.tokens);
        let mut imported = parsed.program;
        diagnostics.extend(parsed.diagnostics);
        if depth > 32 {
            diagnostics.push(Diagnostic::error(
                code::UNDECLARED,
                format!("`{}` imports too deeply to resolve", import.path),
            ));
            continue;
        }
        for nested in &imported.imports {
            queue.push((nested.clone(), depth + 1));
        }
        modules.push(Decl::Module(ModuleDecl {
            attributes: Vec::new(),
            name: Ident {
                name: namespace.to_string(),
                span: import.span,
            },
            decls: std::mem::take(&mut imported.decls),
            span: import.span,
        }));
    }
    modules
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
    pub bounds_check: BoundsCheck,
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
    let module = match generate(&analyzed, &analyzed.program, request) {
        Ok(module) => module,
        Err(diagnostics) => {
            return Err(Failure {
                sources: analyzed.sources,
                diagnostics,
            })
        }
    };
    let artifact = package(&module, request, &output_stem(request.source))?;
    Ok((analyzed, module, artifact))
}

/// Lower a program to a module. The program may not be the one the source
/// says — a test harness hands in a program it built itself.
fn generate(
    analyzed: &Analyzed,
    program: &Program,
    request: &BuildRequest,
) -> Result<CompiledModule, Vec<Diagnostic>> {
    let bitcode =
        build_c_bitcode(&analyzed.c_units, &request.out_dir).map_err(|f| f.diagnostics)?;
    let options = Options {
        debug_allocator: request.opt_level == dux_codegen::OptLevel::None,
        bounds_checks: request
            .bounds_check
            .enabled(request.opt_level == dux_codegen::OptLevel::None),
        opt_level: request.opt_level,
        bitcode,
    };
    CompiledModule::build_with_line_tables(program, options, &analyzed.sources)
}

/// Write a module's object file and name the executable that will carry it.
fn package(
    module: &CompiledModule,
    request: &BuildRequest,
    stem: &str,
) -> Result<BuiltArtifact, Failure> {
    std::fs::create_dir_all(&request.out_dir).map_err(|e| {
        Failure::bare(vec![Diagnostic::error(
            code::SOURCE_IO,
            format!("cannot create {}: {e}", request.out_dir.display()),
        )])
    })?;
    let object = request.out_dir.join(format!("{stem}.o"));
    let executable = request.out_dir.join(stem);
    module
        .write_object(&object)
        .map_err(|e| Failure::bare(vec![e.to_diagnostic()]))?;
    Ok(BuiltArtifact {
        object,
        executable,
        ir: module.ir_text(),
        data_layout: module.data_layout(),
    })
}

pub fn build(
    request: &BuildRequest,
    out_override: Option<&std::path::Path>,
) -> Result<BuiltArtifact, Failure> {
    if request.emit == Emit::Ast {
        return emit_ast(request, out_override);
    }
    let (analyzed, _module, artifact) = compile(request)?;
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
            link(&artifact, &libraries(&analyzed.program))?;
            if let Some(path) = out_override {
                std::fs::copy(&artifact.executable, path).map_err(|e| io_failure(path, e))?;
            }
        }
        Emit::Ast => unreachable!("`--emit=ast` returns before code generation"),
    }
    Ok(artifact)
}

/// The `#[test]` functions a program declares, in the order they were
/// written. A test that takes parameters or answers something is not a test;
/// the program hears about it before anything runs.
fn test_functions(analyzed: &Analyzed) -> Result<Vec<(String, Span)>, Failure> {
    let mut tests = Vec::new();
    let mut complaints = Vec::new();
    for function in analyzed.program.functions() {
        if !function
            .attributes
            .iter()
            .any(|attribute| attribute.name == "test")
        {
            continue;
        }
        if !function.params.is_empty() || function.return_ty.is_some() {
            complaints.push(
                Diagnostic::error(
                    code::BAD_MAIN_SIGNATURE,
                    format!(
                        "`{}` is marked `#[test]`, which takes no parameters and returns nothing",
                        function.name.name
                    ),
                )
                .with_span(function.span),
            );
        }
        tests.push((function.name.name.clone(), function.span));
    }
    if !complaints.is_empty() {
        return Err(Failure::bare(complaints));
    }
    Ok(tests)
}

/// The program that runs one test: the source's declarations without its
/// `main`, and a generated `main` whose whole body is calling the test. The
/// test runs exactly as `duxc run` would run it — same context, same debug
/// runtime, same exit through `atexit` — and the executable is alone, so
/// whatever it prints and leaks belongs to that one test.
fn test_harness(program: &Program, name: &str, span: Span) -> Program {
    let mut decls: Vec<Decl> = program
        .decls
        .iter()
        .filter(|decl| !matches!(decl, Decl::Fn(function) if function.name.name == "main"))
        .cloned()
        .collect();
    decls.push(Decl::Fn(FnDecl {
        attributes: Vec::new(),
        name: Ident::new("main", span),
        type_params: Vec::new(),
        params: Vec::new(),
        return_ty: None,
        body: Block {
            stmts: vec![Stmt::Expr {
                expr: Expr::Call {
                    callee: Box::new(Expr::Ident {
                        name: name.to_string(),
                        span,
                    }),
                    args: Vec::new(),
                    span,
                },
                span,
            }],
            span,
        },
        span,
    }));
    Program {
        package: program.package.clone(),
        imports: program.imports.clone(),
        decls,
    }
}

/// Compile one executable per `#[test]` function and run each alone. A test
/// that stops by signal still reports itself; the tests after it run. The
/// answer is (passed, failed).
pub fn run_tests(
    source: &Path,
    out_dir: &Path,
    verbose: bool,
    stdout: &mut impl std::io::Write,
) -> Result<(usize, usize), Failure> {
    let mut analyzed = analyze(source)?;
    // A test file is entered through its tests, not through a `main` it is
    // not expected to have; the harness supplies the entry either way.
    analyzed
        .diagnostics
        .retain(|diagnostic| diagnostic.code != code::NO_MAIN);
    if analyzed.has_errors() {
        return Err(Failure {
            sources: analyzed.sources,
            diagnostics: analyzed.diagnostics,
        });
    }
    let tests = test_functions(&analyzed)?;
    if tests.is_empty() {
        return Err(Failure::bare(vec![Diagnostic::error(
            code::NO_MAIN,
            format!("{} has no #[test] functions to run", source.display()),
        )]));
    }
    let request = BuildRequest {
        source,
        out_dir: out_dir.to_path_buf(),
        emit: Emit::Execute,
        opt_level: OptLevel::None,
        bounds_check: BoundsCheck::Auto,
    };
    let stem = output_stem(source);
    let mut passed = 0;
    let mut failed = 0;
    for (name, span) in &tests {
        let program = test_harness(&analyzed.program, name, *span);
        let module = match generate(&analyzed, &program, &request) {
            Ok(module) => module,
            Err(diagnostics) => {
                return Err(Failure {
                    sources: analyzed.sources,
                    diagnostics,
                })
            }
        };
        let artifact = package(&module, &request, &format!("{stem}-{name}"))?;
        link(&artifact, &libraries(&program))?;
        let output = std::process::Command::new(&artifact.executable)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .map_err(|e| {
                Failure::bare(vec![Diagnostic::error(
                    code::LINK_FAILED,
                    format!("cannot execute {}: {e}", artifact.executable.display()),
                )])
            })?;
        let status = exit_code(&output.status);
        let said = String::from_utf8_lossy(&output.stdout).into_owned();
        let complained = String::from_utf8_lossy(&output.stderr).into_owned();
        // The debug runtime speaks its leak report at the exit of every test
        // process, and a test that finishes with memory still held has
        // answered wrongly even though its exit code says otherwise.
        let leaked = complained.contains("dux: leaked");
        if status == 0 && !leaked {
            passed += 1;
            writeln!(stdout, "test {name} ... ok").map_err(|e| io_failure(source, e))?;
        } else {
            failed += 1;
            if status == 0 {
                writeln!(stdout, "test {name} ... FAILED (leaked memory)")
                    .map_err(|e| io_failure(source, e))?;
            } else {
                writeln!(stdout, "test {name} ... FAILED (exit {status})")
                    .map_err(|e| io_failure(source, e))?;
            }
        }
        if status != 0 || leaked || verbose {
            for line in said.lines().chain(complained.lines()) {
                writeln!(stdout, "  | {line}").map_err(|e| io_failure(source, e))?;
            }
        }
    }
    Ok((passed, failed))
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
    let path = out_override.map(|p| p.to_path_buf()).unwrap_or_else(|| {
        request
            .out_dir
            .join(format!("{}.ast", output_stem(request.source)))
    });
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
    // A program killed by a signal reports itself the way a shell would:
    // 128 plus the signal, so `duxc run` on a segfault says 139.
    Ok(exit_code(&output.status))
}

fn exit_code(status: &std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    match status.code() {
        Some(code) => code,
        None => 128 + status.signal().unwrap_or(0),
    }
}

/// Compile every C translation unit to bitcode, or point at the cached
/// one. The cache key is the content of everything that feeds clang —
/// the generated source, the flags, the include paths, the header file
/// itself and the compiler's version line — so an edit to a header, a new
/// `-D` or a toolchain upgrade each miss the cache exactly once.
fn build_c_bitcode(units: &[CUnit], out_dir: &Path) -> Result<Vec<PathBuf>, Failure> {
    if units.is_empty() {
        return Ok(Vec::new());
    }
    let compiler = std::env::var("DUX_CC").unwrap_or_else(|_| "clang".to_string());
    let version = cc_version(&compiler)?;
    let cache = out_dir.join("cimport");
    std::fs::create_dir_all(&cache).map_err(|e| {
        Failure::bare(vec![Diagnostic::error(
            code::SOURCE_IO,
            format!("cannot create {}: {e}", cache.display()),
        )])
    })?;
    let mut paths = Vec::new();
    for unit in units {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        let mut mix = |bytes: &[u8]| {
            for byte in bytes {
                hash ^= *byte as u64;
                hash = hash.wrapping_mul(0x100_0000_01b3);
            }
        };
        mix(unit.source.as_bytes());
        for header in &unit.headers {
            mix(header.as_bytes());
        }
        mix(version.as_bytes());
        for part in unit.flags.iter().chain(unit.include_paths.iter()) {
            mix(part.as_bytes());
        }
        // Every header the source names, if it is a file on disk: an edit
        // to a project header misses the cache; a system header is the
        // toolchain's own business (and its version is in the hash).
        for dir in &unit.include_paths {
            for header in &unit.headers {
                let file = PathBuf::from(dir).join(header);
                if let Ok(text) = std::fs::read(&file) {
                    mix(&text);
                }
            }
        }
        let bitcode = cache.join(format!("{hash:016x}.bc"));
        if bitcode.is_file() {
            paths.push(bitcode);
            continue;
        }
        let source_path = cache.join(format!("{hash:016x}.c"));
        std::fs::write(&source_path, &unit.source).map_err(|e| io_failure(&source_path, e))?;
        let output = std::process::Command::new(&compiler)
            .arg("-c")
            .arg("-emit-llvm")
            .arg("-w")
            .args(&unit.flags)
            .args(unit.include_paths.iter().map(|dir| format!("-I{dir}")))
            .arg("-o")
            .arg(&bitcode)
            .arg(&source_path)
            .stderr(std::process::Stdio::piped())
            .output()
            .map_err(|e| {
                Failure::bare(vec![Diagnostic::error(
                    code::LINK_FAILED,
                    format!(
                        "cannot run `{compiler}` for `{}`: {e}",
                        unit.headers.join(", ")
                    ),
                )])
            })?;
        if !output.status.success() {
            return Err(Failure::bare(vec![Diagnostic::error(
                code::LINK_FAILED,
                format!(
                    "`{compiler}` could not compile `{}`: {}",
                    unit.headers.join(", "),
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            )]));
        }
        paths.push(bitcode);
    }
    Ok(paths)
}

fn cc_version(compiler: &str) -> Result<String, Failure> {
    let output = std::process::Command::new(compiler)
        .arg("--version")
        .output()
        .map_err(|e| {
            Failure::bare(vec![Diagnostic::error(
                code::LINK_FAILED,
                format!("cannot run `{compiler}`: {e}"),
            )])
        })?;
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .to_string())
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

fn link(artifact: &BuiltArtifact, libraries: &[String]) -> Result<(), Failure> {
    let linker = linker_command();
    let mut command = std::process::Command::new(&linker);
    command
        .arg(&artifact.object)
        .arg("-o")
        .arg(&artifact.executable)
        .stderr(std::process::Stdio::inherit());
    // A library is named by the declaration that needs it, never by the link
    // line as a whole: `#[link(name = "m")]` says where a symbol lives.
    for library in libraries {
        command.arg(format!("-l{library}"));
    }
    let status = command.status().map_err(|e| {
        Failure::bare(vec![Diagnostic::error(
            code::LINK_FAILED,
            format!("cannot run the linker `{linker}`: {e}"),
        )
        .with_note(
            "set DUX_LINKER to pick a linker; DESIGN.md 6.3 expects clang",
        )])
    })?;
    if !status.success() {
        return Err(Failure::bare(vec![Diagnostic::error(
            code::LINK_FAILED,
            format!("`{linker}` exited with a failure status"),
        )]));
    }
    Ok(())
}

/// The libraries a `#[c_pkgconfig("name")]` asks pkg-config for. The flags
/// are the tool's answer, not ours to invent; a missing pkg-config or a
/// missing package is the diagnostic, not a silent shrug.
fn pkg_config(package: &str, mode: &str) -> Result<Vec<String>, Diagnostic> {
    let tool = std::env::var("DUX_PKG_CONFIG").unwrap_or_else(|_| "pkg-config".to_string());
    let output = std::process::Command::new(&tool)
        .arg(format!("--{mode}"))
        .arg(package)
        .output()
        .map_err(|e| {
            Diagnostic::error(
                code::LINK_FAILED,
                format!("cannot run `{tool}` for `{package}`: {e}"),
            )
        })?;
    if !output.status.success() {
        return Err(Diagnostic::error(
            code::LINK_FAILED,
            format!(
                "`{tool} --{mode} {package}` failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .map(|token| token.to_string())
        .collect())
}

/// Every `#[c_include]` namespace in the program, filled with the Dux the
/// C mapping table writes for its header.
#[allow(clippy::too_many_arguments)]
fn fill_c_modules(
    entry: &Path,
    sources: &mut SourceMap,
    program: &mut Program,
    diagnostics: &mut Vec<Diagnostic>,
    namespaces: &mut Vec<CNamespace>,
) -> Vec<CUnit> {
    let home = entry
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let mut units: Vec<CUnit> = Vec::new();
    for decl in &mut program.decls {
        let Decl::Module(module) = decl else {
            continue;
        };
        let headers: Vec<String> = module
            .attributes
            .iter()
            .filter(|attribute| attribute.name == "c_include")
            .filter_map(|attribute| attribute.string_argument())
            .map(|header| header.to_string())
            .collect();
        if headers.is_empty() {
            continue;
        }
        let mut flags: Vec<String> = module
            .attributes
            .iter()
            .filter(|attribute| attribute.name == "c_flag")
            .filter_map(|attribute| attribute.string_argument())
            .map(|flag| flag.to_string())
            .collect();
        let mut include_paths = vec![home.to_string_lossy().into_owned()];
        for package in module
            .attributes
            .iter()
            .filter(|attribute| attribute.name == "c_pkgconfig")
            .filter_map(|attribute| attribute.string_argument())
        {
            match pkg_config(package, "cflags") {
                Ok(tokens) => {
                    for token in tokens {
                        if let Some(dir) = token.strip_prefix("-I") {
                            include_paths.push(dir.to_string());
                        } else if token.starts_with("-D") {
                            flags.push(token);
                        }
                    }
                }
                Err(diagnostic) => diagnostics.push(diagnostic),
            }
        }
        // Each header is read on its own, but the namespace speaks with
        // one voice: declarations that two headers both declare are kept
        // once, the whole namespace is spelled once, and the translation
        // unit that carries the definitions includes all of them.
        let mut declarations: Vec<dux_cinterop::CDecl> = Vec::new();
        let mut missed: Vec<dux_cinterop::Skipped> = Vec::new();
        for header in &headers {
            let request = dux_cinterop::Import {
                header: header.clone(),
                flags: flags.clone(),
                include_paths: include_paths.clone(),
            };
            match dux_cinterop::import(&request) {
                Ok(seen) => {
                    for declaration in seen.decls {
                        let clash = declarations.iter().any(|other| {
                            std::mem::discriminant(other) == std::mem::discriminant(&declaration)
                                && other.name() == declaration.name()
                        });
                        if !clash {
                            declarations.push(declaration);
                        }
                    }
                    missed.extend(seen.skipped);
                }
                Err(failure) => diagnostics.extend(failure),
            }
        }
        let namespace = module.name.name.as_str();
        let (text, unspellable, mut crossed) = dux_cinterop::spell_report(namespace, &declarations);
        missed.extend(unspellable);
        missed.sort_by(|one, other| one.name.cmp(&other.name));
        missed.dedup();
        crossed.sort();
        namespaces.push(CNamespace {
            namespace: module.name.name.clone(),
            header: headers.join(", "),
            crossed,
            skipped: missed,
        });
        let id = sources.add(format!("<c_import {}>", headers.join(", ")), text);
        let lexed = dux_lexer::lex(sources, id);
        diagnostics.extend(lexed.diagnostics);
        let parsed = parse(sources, &lexed.tokens);
        diagnostics.extend(parsed.diagnostics);
        module.decls = parsed.program.decls;
        units.push(CUnit {
            source: dux_cinterop::c_source(namespace, &headers, &declarations),
            headers,
            flags,
            include_paths,
        });
    }
    units
}

/// The libraries the program's `extern` blocks asked for, in the order they
/// were declared and named once each.
fn libraries(program: &Program) -> Vec<String> {
    let mut named: Vec<String> = Vec::new();
    for entry in program.scoped() {
        let Decl::Extern(block) = entry.decl else {
            continue;
        };
        for attribute in &block.attributes {
            if attribute.name != "link" {
                continue;
            }
            for argument in &attribute.arguments {
                let AttributeArgument::Named(key, value) = argument else {
                    continue;
                };
                if key == "name" && !named.iter().any(|known| known == value) {
                    named.push(value.clone());
                }
            }
        }
    }
    for entry in program.scoped() {
        let Decl::Module(module) = entry.decl else {
            continue;
        };
        for attribute in &module.attributes {
            if attribute.name == "c_link" {
                if let Some(library) = attribute.string_argument() {
                    if !named.iter().any(|known| known == library) {
                        named.push(library.to_string());
                    }
                }
            }
            if attribute.name == "c_pkgconfig" {
                if let Some(package) = attribute.string_argument() {
                    if let Ok(tokens) = pkg_config(package, "libs") {
                        for token in tokens {
                            if let Some(library) = token.strip_prefix("-l") {
                                if !named.iter().any(|known| known == library) {
                                    named.push(library.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    named
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
