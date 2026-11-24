use dux_ast::{FnDecl, Program};
use dux_diag::{code, Diagnostic};
use inkwell::context::Context;
use inkwell::module::Module;
use inkwell::targets::{
    CodeModel, FileType, InitializationConfig, RelocMode, Target, TargetMachine,
};
use inkwell::{OptimizationLevel};
use std::path::Path;

mod lower;

use lower::Emitter;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum OptLevel {
    None,
    Balanced,
    Aggressive,
}

impl OptLevel {
    pub fn pipeline(self) -> &'static str {
        match self {
            OptLevel::None => "default<O0>",
            OptLevel::Balanced => "default<O2>",
            OptLevel::Aggressive => "default<O3>",
        }
    }

    fn inkwell(self) -> OptimizationLevel {
        match self {
            OptLevel::None => OptimizationLevel::None,
            OptLevel::Balanced => OptimizationLevel::Default,
            OptLevel::Aggressive => OptimizationLevel::Aggressive,
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub struct Options {
    pub opt_level: OptLevel,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            opt_level: OptLevel::None,
        }
    }
}

#[derive(Debug)]
pub enum CodegenError {
    Llvm(String),
    Io(String),
}

impl CodegenError {
    pub fn to_diagnostic(&self) -> Diagnostic {
        match self {
            CodegenError::Llvm(message) => {
                Diagnostic::error(code::CODEGEN_FAILED, format!("llvm: {message}"))
            }
            CodegenError::Io(message) => {
                Diagnostic::error(code::CODEGEN_FAILED, format!("io: {message}"))
            }
        }
    }
}

#[derive(Debug)]
pub struct CompiledModule {
    module: Module<'static>,
    machine: TargetMachine,
}

fn process_context() -> &'static Context {
    thread_local! {
        static CONTEXT: &'static Context = Box::leak(Box::new(Context::create()));
    }
    CONTEXT.with(|context| *context)
}

impl CompiledModule {
    pub fn build(program: &Program, options: Options) -> Result<CompiledModule, Vec<Diagnostic>> {
        Self::build_with_line_tables(program, options, &dux_source::SourceMap::new())
    }

    /// Build with a line table: when the `SourceMap` is non-empty the module
    /// carries `!llvm.dbg.cu` metadata and every statement its line and
    /// column, so generated programs are steppable and backtrace-able.
    pub fn build_with_line_tables(
        program: &Program,
        options: Options,
        sources: &dux_source::SourceMap,
    ) -> Result<CompiledModule, Vec<Diagnostic>> {
        let context = process_context();
        let module = context.create_module(module_name(program));
        let machine = target_machine(options.opt_level)?;
        module.set_triple(&machine.get_triple());
        module.set_data_layout(&machine.get_target_data().get_data_layout());

        select_main(program)?;
        let diagnostics = Emitter::new(context, sources.clone()).lower(&module, program);
        if !diagnostics.is_empty() {
            return Err(diagnostics);
        }
        module.verify().map_err(|e| {
            vec![Diagnostic::error(
                code::CODEGEN_FAILED,
                format!("generated module is invalid: {e}"),
            )]
        })?;
        run_passes(&module, &machine, options.opt_level)?;
        Ok(CompiledModule { module, machine })
    }

    pub fn write_object(&self, path: &Path) -> Result<(), CodegenError> {
        self.machine
            .write_to_file(&self.module, FileType::Object, path)
            .map_err(|e| CodegenError::Io(e.to_string()))
    }

    pub fn ir_text(&self) -> String {
        self.module.print_to_string().to_string()
    }

    pub fn data_layout(&self) -> String {
        self.machine
            .get_target_data()
            .get_data_layout()
            .as_str()
            .to_string_lossy()
            .into_owned()
    }
}

fn target_machine(opt_level: OptLevel) -> Result<TargetMachine, Vec<Diagnostic>> {
    Target::initialize_all(&InitializationConfig::default());
    let triple = TargetMachine::get_default_triple();
    let target = Target::from_triple(&triple).map_err(|e| {
        vec![Diagnostic::error(
            code::CODEGEN_FAILED,
            format!("no backend for `{}`: {e}", triple.as_str().to_string_lossy()),
        )]
    })?;
    target
        .create_target_machine(
            &triple,
            "",
            "",
            opt_level.inkwell(),
            RelocMode::PIC,
            CodeModel::Default,
        )
        .ok_or_else(|| {
            vec![Diagnostic::error(
                code::CODEGEN_FAILED,
                format!(
                    "could not create a target machine for `{}`",
                    triple.as_str().to_string_lossy()
                ),
            )]
        })
}

fn run_passes(
    module: &Module,
    machine: &TargetMachine,
    opt_level: OptLevel,
) -> Result<(), Vec<Diagnostic>> {
    if opt_level == OptLevel::None {
        return Ok(());
    }
    module
        .run_passes(
            opt_level.pipeline(),
            machine,
            inkwell::passes::PassBuilderOptions::create(),
        )
        .map_err(|e| {
            vec![Diagnostic::error(
                code::CODEGEN_FAILED,
                format!("pass pipeline `{}` failed: {}", opt_level.pipeline(), e),
            )]
        })
}

fn module_name(program: &Program) -> &str {
    program
        .package
        .as_ref()
        .map(|p| p.name.as_str())
        .unwrap_or("dux_module")
}

fn select_main(program: &Program) -> Result<&FnDecl, Vec<Diagnostic>> {
    let Some(main) = program.find_function("main") else {
        return Err(vec![Diagnostic::error(
            code::NO_MAIN,
            "no `fn main()` in this program",
        )
        .with_note("the entry point is `fn main()` in the package named `main`")]);
    };
    if !main.params.is_empty() {
        return Err(vec![Diagnostic::error(
            code::BAD_MAIN_SIGNATURE,
            "`fn main()` takes no parameters",
        )
        .with_span(main.name.span)]);
    }
    if let Some(return_ty) = &main.return_ty {
        let ok = match return_ty {
            dux_ast::Type::Named { name, .. } => name == "i32" || name == "i64",
            _ => false,
        };
        if !ok {
            return Err(vec![Diagnostic::error(
                code::BAD_MAIN_SIGNATURE,
                "`fn main()` returns nothing or `i32`",
            )
            .with_span(return_ty.span())]);
        }
    }
    Ok(main)
}
