use dux_ast::{Expr, FnDecl, Program, Stmt, StrKind};
use dux_diag::{code, Diagnostic};
use dux_source::Span;
use inkwell::context::Context;
use inkwell::module::Module;
use inkwell::builder::Builder;
use inkwell::targets::{
    CodeModel, FileType, InitializationConfig, RelocMode, Target, TargetMachine,
};
use inkwell::types::IntType;
use inkwell::values::{BasicMetadataValueEnum, FunctionValue, IntValue, PointerValue};
use inkwell::{AddressSpace, OptimizationLevel};
use std::path::Path;

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
        let context = process_context();
        let module = context.create_module(module_name(program));
        let machine = target_machine(options.opt_level)?;
        module.set_triple(&machine.get_triple());
        module.set_data_layout(&machine.get_target_data().get_data_layout());

        let mut emitter = Emitter {
            context,
            puts: None,
            fputs: None,
            diagnostics: Vec::new(),
        };
        let main = select_main(program)?;
        emitter.lower_main(&module, main);

        if !emitter.diagnostics.is_empty() {
            return Err(emitter.diagnostics);
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

fn run_passes(module: &Module, machine: &TargetMachine, opt_level: OptLevel) -> Result<(), Vec<Diagnostic>> {
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

struct Emitter {
    context: &'static Context,
    puts: Option<FunctionValue<'static>>,
    fputs: Option<FunctionValue<'static>>,
    diagnostics: Vec<Diagnostic>,
}

impl Emitter {
    fn lower_main(&mut self, module: &Module<'static>, main: &FnDecl) {
        let i32: IntType = self.context.i32_type();
        let zero: IntValue = i32.const_int(0, false);
        let fn_value = module.add_function("main", i32.fn_type(&[], false), None);
        let block = self.context.append_basic_block(fn_value, "entry");
        let builder = self.context.create_builder();
        builder.position_at_end(block);

        for stmt in &main.body.stmts {
            self.lower_stmt(module, &builder, stmt);
        }
        if let Err(e) = builder.build_return(Some(&zero)) {
            self.diagnostics.push(Diagnostic::error(
                code::CODEGEN_FAILED,
                format!("failed to emit the return from main: {e}"),
            ));
        }
    }

    fn lower_stmt(
        &mut self,
        module: &Module<'static>,
        builder: &Builder<'static>,
        stmt: &Stmt,
    ) {
        match stmt {
            Stmt::Expr { expr, .. } => {
                if !matches!(expr, Expr::Call { .. }) {
                    self.unsupported(expr.span(), "an expression statement that is not a call");
                }
                self.lower_call_as_statement(module, builder, expr);
            }
            Stmt::Return { value, span } => match value {
                None => self.unsupported(*span, "`return` in `fn main()`"),
                Some(_) => self.unsupported(*span, "`return` with a value"),
            },
            other => self.unsupported(other.span(), statement_name(other)),
        }
    }

    fn lower_call_as_statement(
        &mut self,
        module: &Module<'static>,
        builder: &Builder<'static>,
        expr: &Expr,
    ) {
        let Expr::Call { callee, args, span } = expr else {
            self.unsupported(expr.span(), "this statement");
            return;
        };
        let Expr::Ident { ref name, .. } = **callee else {
            self.unsupported(*span, "calling anything but a plain named function");
            return;
        };
        if !matches!(name.as_str(), "print" | "println") {
            self.unsupported(*span, &format!("call to `{name}`"));
            return;
        }
        if args.len() != 1 {
            self.unsupported(*span, &format!("`{name}` with {} arguments", args.len()));
            return;
        }
        let Expr::Str { text, kind, span: arg_span } = &args[0] else {
            self.unsupported(args[0].span(), &format!("a non-literal argument to `{name}`"));
            return;
        };
        if *kind == StrKind::Interpolated {
            self.unsupported(*arg_span, "string interpolation");
            return;
        }
        let payload = text.clone();
        let global = match builder.build_global_string_ptr(&payload, "lit") {
            Ok(global) => global,
            Err(e) => {
                self.diagnostics.push(
                    Diagnostic::error(
                        code::CODEGEN_FAILED,
                        format!("failed to emit the string literal: {e}"),
                    )
                    .with_span(*arg_span),
                );
                return;
            }
        };
        let text_ptr = global.as_pointer_value();
        let arguments: Vec<BasicMetadataValueEnum> = if name == "println" {
            vec![text_ptr.into()]
        } else {
            let Some(stream) = self.stdout(module, builder) else {
                return;
            };
            vec![text_ptr.into(), stream.into()]
        };
        let callee = if name == "println" {
            self.libc_puts(module)
        } else {
            self.libc_fputs(module)
        };
        if let Err(e) = builder.build_call(callee, &arguments, "write") {
            self.diagnostics.push(
                Diagnostic::error(
                    code::CODEGEN_FAILED,
                    format!("failed to emit the write call: {e}"),
                )
                .with_span(*span),
            );
        }
    }

    fn libc_puts(&mut self, module: &Module<'static>) -> FunctionValue<'static> {
        if let Some(puts) = self.puts {
            return puts;
        }
        let c_char = self.context.ptr_type(AddressSpace::default());
        let puts = module.add_function(
            "puts",
            self.context.i32_type().fn_type(&[c_char.into()], false),
            None,
        );
        self.puts = Some(puts);
        puts
    }

    fn libc_fputs(&mut self, module: &Module<'static>) -> FunctionValue<'static> {
        if let Some(fputs) = self.fputs {
            return fputs;
        }
        let c_char = self.context.ptr_type(AddressSpace::default());
        let fputs = module.add_function(
            "fputs",
            self.context
                .i32_type()
                .fn_type(&[c_char.into(), c_char.into()], false),
            None,
        );
        self.fputs = Some(fputs);
        fputs
    }

    fn stdout(
        &mut self,
        module: &Module<'static>,
        builder: &Builder<'static>,
    ) -> Option<PointerValue<'static>> {
        let c_char = self.context.ptr_type(AddressSpace::default());
        let stream = module
            .get_global("stdout")
            .unwrap_or_else(|| module.add_global(c_char, None, "stdout"));
        match builder.build_load(c_char, stream.as_pointer_value(), "stdout") {
            Ok(loaded) => Some(loaded.into_pointer_value()),
            Err(e) => {
                self.diagnostics.push(Diagnostic::error(
                    code::CODEGEN_FAILED,
                    format!("failed to reach `stdout`: {e}"),
                ));
                None
            }
        }
    }

    fn unsupported(&mut self, span: Span, what: &str) {
        self.diagnostics.push(
            Diagnostic::error(
                code::UNSUPPORTED_IN_BUILD,
                format!("{what} is not supported by this compiler build yet"),
            )
            .with_span(span)
            .with_note("this is a milestone limitation, not a language decision"),
        );
    }
}

fn statement_name(stmt: &Stmt) -> &'static str {
    match stmt {
        Stmt::Binding { .. } => "a binding",
        Stmt::Assign { .. } => "an assignment",
        Stmt::Expr { .. } => "an expression statement",
        Stmt::Return { .. } => "a return",
        Stmt::Break { .. } => "`break`",
        Stmt::Continue { .. } => "`continue`",
        Stmt::Defer { .. } => "`defer`",
        Stmt::Block { .. } => "a nested block",
        Stmt::Error { .. } => "an erroneous statement",
    }
}
