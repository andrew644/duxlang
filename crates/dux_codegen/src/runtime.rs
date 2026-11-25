//! The generated program's allocation support.
//!
//! Every allocation goes through a *context*: two procedure pointers and a
//! label. `main` starts with the default context; a future `mem.Arena` is a
//! different `dux.Context` value bound to the hidden `context` parameter.
//!
//! Two default allocators are available:
//!
//! * the **debug allocator** heads every block with a live/dead magic word and
//!   the payload size. Releasing checks the magic, so a double free or a
//!   release of memory this allocator never handed out is caught at the point
//!   of the bug instead of in a later crash. It counts live allocations and
//!   bytes, and reports what is still live when the program exits.
//! * the **plain allocator** is a `malloc`/`free` wrapper used when the debug
//!   allocator is off, so a release build pays nothing.
//!
//! Guard pages and a per-allocation location table are recorded as follow-ups
//! in `roadmap.md`.

use inkwell::builder::Builder;
use inkwell::context::Context;
use inkwell::module::{Linkage, Module};
use inkwell::types::{BasicTypeEnum, FunctionType, StructType};
use inkwell::values::{BasicMetadataValueEnum, FunctionValue, GlobalValue, IntValue, PointerValue};
use inkwell::{AddressSpace, IntPredicate};

const MAGIC_ALIVE: u64 = 0xA11C_0DE5_AFE0_0000;
const MAGIC_DEAD: u64 = 0xDEAD_BEEF_0000_0000;
const HEADER: u64 = 16;
const SIZE_MAX: u64 = u64::MAX;

/// The allocation runtime of one generated module.
pub struct Runtime {
    /// The context `main` starts with: `@dux.default_context`.
    pub default_context: PointerValue<'static>,
    /// `ptr @dux.runtime.alloc(ptr context, i64 size, i64 align, ptr location)`
    pub alloc: FunctionValue<'static>,
    /// `void @dux.runtime.free(ptr context, ptr memory)`
    pub free: FunctionValue<'static>,
    /// `void @dux.runtime.report()` — unreleased blocks, in debug mode.
    pub report: Option<FunctionValue<'static>>,
    /// `noreturn @dux.runtime.trap(ptr message)` — a language rule the running
    /// program broke, such as a `match` whose every arm declined.
    pub trap: FunctionValue<'static>,
}

impl Runtime {
    /// Emit the allocator runtime into `module`.
    pub fn emit(context: &'static Context, module: &Module<'static>, debug: bool) -> Self {
        let builder = context.create_builder();
        let ptr = context.ptr_type(AddressSpace::default());
        let i64_type = context.i64_type();

        let context_type: StructType<'static> = context.opaque_struct_type("dux.Context");
        context_type.set_body(&[ptr.into(), ptr.into(), ptr.into()], false);

        let alloc = module.add_function(
            "dux.runtime.alloc",
            ptr.fn_type(
                &[ptr.into(), i64_type.into(), i64_type.into(), ptr.into()],
                false,
            ),
            None,
        );
        alloc.set_linkage(Linkage::Internal);
        let free = module.add_function(
            "dux.runtime.free",
            context.void_type().fn_type(&[ptr.into(), ptr.into()], false),
            None,
        );
        free.set_linkage(Linkage::Internal);

        // The counters exist only for the debug allocator: a release build
        // should not carry globals it never touches.
        let live = debug.then(|| counter(context, module, "dux.runtime.live"));
        let bytes = debug.then(|| counter(context, module, "dux.runtime.bytes"));
        let malloc = declare(
            module,
            "malloc",
            ptr.fn_type(&[i64_type.into()], false),
        );
        let libc_free = declare(
            module,
            "free",
            context.void_type().fn_type(&[ptr.into()], false),
        );
        let oom = emit_oom(context, module, &builder);
        let trap = emit_trap(context, module, &builder);

        emit_alloc(context, &builder, alloc, malloc, oom, live, bytes, debug);
        let report = if debug {
            let (live, bytes) = (live.expect("counters"), bytes.expect("counters"));
            emit_free_debug(
                context,
                module,
                &builder,
                free,
                libc_free,
                Some(live),
                Some(bytes),
            );
            Some(emit_report(context, module, &builder, live, bytes))
        } else {
            emit_free_plain(context, &builder, free, libc_free);
            None
        };

        let label = builder
            .build_global_string_ptr(if debug { "debug" } else { "default" }, "dux.runtime.label")
            .expect("context label")
            .as_pointer_value();
        let default = module.add_global(context_type, None, "dux.default_context");
        default.set_initializer(&context_type.const_named_struct(&[
            alloc.as_global_value().as_pointer_value().into(),
            free.as_global_value().as_pointer_value().into(),
            label.into(),
        ]));

        Runtime {
            default_context: default.as_pointer_value(),
            alloc,
            free,
            report,
            trap,
        }
    }
}

/// The generated program's hashing support. `hash` is a builtin because a
/// generic key is the one place a value's bits are wanted before any type an
/// `impl` can name exists; the tables that use it are ordinary code written
/// against these two.
#[derive(Clone, Copy)]
pub struct HashRuntime {
    /// `i64 @dux.runtime.hash_mix(i64 word)` — one word through a
    /// splitmix64 finalizer, so neighbouring words and small integers land
    /// far apart.
    pub mix: FunctionValue<'static>,
    /// `i64 @dux.runtime.hash_bytes(ptr data, i64 len)` — FNV-1a over the
    /// bytes of a `string` or a byte view.
    pub bytes: FunctionValue<'static>,
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x100_0000_01b3;

impl HashRuntime {
    /// Emit the hashing runtime. `lower` calls this the first time a program
    /// asks for a hash, the same way the allocator arrives only when
    /// something allocates.
    pub fn emit(context: &'static Context, module: &Module<'static>) -> Self {
        let builder = context.create_builder();
        let i64_type = context.i64_type();
        let ptr = context.ptr_type(AddressSpace::default());

        let mix = module.add_function(
            "dux.runtime.hash_mix",
            i64_type.fn_type(&[i64_type.into()], false),
            None,
        );
        mix.set_linkage(Linkage::Internal);
        let entry = context.append_basic_block(mix, "entry");
        builder.position_at_end(entry);
        let word = mix.get_nth_param(0).unwrap().into_int_value();
        let golden = i64_type.const_int(0x9E37_79B9_7F4A_7C15, false);
        let mut state = builder
            .build_int_add(word, golden, "state")
            .expect("golden");
        for (shift, factor) in [
            (30u64, 0xBF58_476D_1CE4_E5B9u64),
            (27, 0x94D0_49BB_1331_11EB),
        ] {
            let moved = builder
                .build_right_shift(state, i64_type.const_int(shift, false), false, "apart")
                .expect("shift");
            state = builder.build_xor(state, moved, "fold").expect("xor");
            state = builder
                .build_int_mul(state, i64_type.const_int(factor, false), "spread")
                .expect("spread");
        }
        let tail = builder
            .build_right_shift(state, i64_type.const_int(31, false), false, "tail")
            .expect("tail");
        let mixed = builder.build_xor(state, tail, "mixed").expect("xor");
        builder.build_return(Some(&mixed)).expect("return");

        let bytes = module.add_function(
            "dux.runtime.hash_bytes",
            i64_type.fn_type(&[ptr.into(), i64_type.into()], false),
            None,
        );
        bytes.set_linkage(Linkage::Internal);
        let entry = context.append_basic_block(bytes, "entry");
        let head = context.append_basic_block(bytes, "head");
        let body = context.append_basic_block(bytes, "body");
        let done = context.append_basic_block(bytes, "done");
        builder.position_at_end(entry);
        let data = bytes.get_nth_param(0).unwrap().into_pointer_value();
        let len = bytes.get_nth_param(1).unwrap().into_int_value();
        let _ = builder.build_unconditional_branch(head);

        builder.position_at_end(head);
        let hash = builder.build_phi(i64_type, "hash").expect("hash");
        let walked = builder.build_phi(i64_type, "walked").expect("walked");
        let at = builder.build_phi(ptr, "at").expect("at");
        hash.add_incoming(&[(&i64_type.const_int(FNV_OFFSET, false), entry)]);
        walked.add_incoming(&[(&i64_type.const_zero(), entry)]);
        at.add_incoming(&[(&data, entry)]);
        let more = builder
            .build_int_compare(IntPredicate::ULT, walked.as_basic_value().into_int_value(), len, "more")
            .expect("more");
        let _ = builder.build_conditional_branch(more, body, done);

        builder.position_at_end(body);
        let byte = builder
            .build_load(
                context.i8_type(),
                at.as_basic_value().into_pointer_value(),
                "byte",
            )
            .expect("byte")
            .into_int_value();
        let wide = builder
            .build_int_z_extend(byte, i64_type, "wide")
            .expect("wide");
        let folded = builder
            .build_xor(hash.as_basic_value().into_int_value(), wide, "fold")
            .expect("fold");
        let spread = builder
            .build_int_mul(folded, i64_type.const_int(FNV_PRIME, false), "spread")
            .expect("spread");
        let next = builder
            .build_int_add(
                walked.as_basic_value().into_int_value(),
                i64_type.const_int(1, false),
                "next",
            )
            .expect("next");
        let ahead = unsafe {
            builder
                .build_gep(
                    context.i8_type(),
                    at.as_basic_value().into_pointer_value(),
                    &[i64_type.const_int(1, false)],
                    "ahead",
                )
                .expect("ahead")
        };
        hash.add_incoming(&[(&spread, body)]);
        walked.add_incoming(&[(&next, body)]);
        at.add_incoming(&[(&ahead, body)]);
        let _ = builder.build_unconditional_branch(head);

        builder.position_at_end(done);
        builder
            .build_return(Some(&hash.as_basic_value().into_int_value()))
            .expect("return");

        HashRuntime { mix, bytes }
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_alloc(
    context: &'static Context,
    builder: &Builder<'static>,
    alloc: FunctionValue<'static>,
    malloc: FunctionValue<'static>,
    oom: FunctionValue<'static>,
    live: Option<GlobalValue<'static>>,
    bytes: Option<GlobalValue<'static>>,
    debug: bool,
) {
    let i64_type = context.i64_type();
    let entry = context.append_basic_block(alloc, "entry");
    let failed = context.append_basic_block(alloc, "failed");
    let body = context.append_basic_block(alloc, "body");
    builder.position_at_end(entry);

    let size = alloc.get_nth_param(1).unwrap().into_int_value();
    let total = if debug {
        builder
            .build_int_add(size, i64_type.const_int(HEADER, false), "total")
            .expect("header")
    } else {
        size
    };
    let base = call_pointer(builder, malloc, &[total.into()], "block");
    let null = builder.build_is_null(base, "null").expect("null");
    builder
        .build_conditional_branch(null, failed, body)
        .expect("branch");

    builder.position_at_end(failed);
    let location = alloc.get_nth_param(3).unwrap();
    let _ = builder.build_call(oom, &[location.into()], "oom");
    let _ = builder.build_unreachable();

    builder.position_at_end(body);
    if !debug {
        builder.build_return(Some(&base)).expect("return");
        return;
    }
    builder
        .build_store(base, i64_type.const_int(MAGIC_ALIVE, false))
        .expect("magic");
    builder
        .build_store(offset(builder, context, base, 8, "size"), size)
        .expect("size");
    bump(builder, context, live, i64_type.const_int(1, false), "live");
    bump(builder, context, bytes, size, "bytes");
    builder
        .build_return(Some(&offset(builder, context, base, HEADER, "memory")))
        .expect("return");
}

#[allow(clippy::too_many_arguments)]
fn emit_free_debug(
    context: &'static Context,
    module: &Module<'static>,
    builder: &Builder<'static>,
    free: FunctionValue<'static>,
    libc_free: FunctionValue<'static>,
    live: Option<GlobalValue<'static>>,
    bytes: Option<GlobalValue<'static>>,
) {
    let i64_type = context.i64_type();
    let entry = context.append_basic_block(free, "entry");
    let check = context.append_basic_block(free, "check");
    let bad = context.append_basic_block(free, "bad");
    let release = context.append_basic_block(free, "release");
    builder.position_at_end(entry);
    let memory = free.get_nth_param(1).unwrap().into_pointer_value();
    let is_null = builder.build_is_null(memory, "null").expect("null");
    builder
        .build_conditional_branch(is_null, bad, check)
        .expect("branch");

    builder.position_at_end(check);
    let header = offset(builder, context, memory, SIZE_MAX - HEADER + 1, "header");
    let magic = builder
        .build_load(i64_type, header, "magic")
        .expect("magic")
        .into_int_value();
    let alive = builder
        .build_int_compare(
            IntPredicate::EQ,
            magic,
            i64_type.const_int(MAGIC_ALIVE, false),
            "alive",
        )
        .expect("alive");
    builder
        .build_conditional_branch(alive, release, bad)
        .expect("branch");

    builder.position_at_end(bad);
    write_stderr(
        context,
        module,
        builder,
        "dux: free of memory this allocator did not hand out, or a double free\n",
    );
    abort(context, module, builder);

    builder.position_at_end(release);
    builder
        .build_store(header, i64_type.const_int(MAGIC_DEAD, false))
        .expect("poison");
    let payload = builder
        .build_load(
            i64_type,
            offset(builder, context, header, 8, "payload"),
            "payload",
        )
        .expect("payload")
        .into_int_value();
    drop_count(builder, context, live, i64_type.const_int(1, false), "live");
    drop_count(builder, context, bytes, payload, "bytes");
    let _ = builder.build_call(libc_free, &[header.into()], "libc_free");
    builder.build_return(None).expect("return");
}

fn emit_free_plain(
    context: &'static Context,
    builder: &Builder<'static>,
    free: FunctionValue<'static>,
    libc_free: FunctionValue<'static>,
) {
    let entry = context.append_basic_block(free, "entry");
    builder.position_at_end(entry);
    let memory = free.get_nth_param(1).unwrap().into_pointer_value();
    let _ = builder.build_call(libc_free, &[memory.into()], "libc_free");
    builder.build_return(None).expect("return");
}

fn emit_report(
    context: &'static Context,
    module: &Module<'static>,
    builder: &Builder<'static>,
    live: GlobalValue<'static>,
    bytes: GlobalValue<'static>,
) -> FunctionValue<'static> {
    // The report is only emitted when both counters exist.
    let report = module.add_function(
        "dux.runtime.report",
        context.void_type().fn_type(&[], false),
        None,
    );
    report.set_linkage(Linkage::Internal);
    let entry = context.append_basic_block(report, "entry");
    let say = context.append_basic_block(report, "say");
    let done = context.append_basic_block(report, "done");
    builder.position_at_end(entry);
    let live_now = read(builder, context, live, "live");
    let clean = builder
        .build_int_compare(
            IntPredicate::EQ,
            live_now,
            context.i64_type().const_zero(),
            "clean",
        )
        .expect("clean");
    builder
        .build_conditional_branch(clean, done, say)
        .expect("branch");

    builder.position_at_end(say);
    let leaks = read(builder, context, live, "leaks");
    let trailing = read(builder, context, bytes, "trailing");
    let format = builder
        .build_global_string_ptr(
            "dux: leaked %lu allocation(s), %lu byte(s)\n",
            "dux.leak.format",
        )
        .expect("leak format")
        .as_pointer_value();
    let stderr = stream(context, module, builder, "stderr");
    let _ = builder.build_call(
        fprintf(context, module),
        &[stderr.into(), format.into(), leaks.into(), trailing.into()],
        "leak",
    );
    let _ = builder.build_unconditional_branch(done);
    builder.position_at_end(done);
    builder.build_return(None).expect("return");
    report
}

/// A broken language rule stops the program where it happened, with a word
/// about it on stderr.
fn emit_trap(
    context: &'static Context,
    module: &Module<'static>,
    builder: &Builder<'static>,
) -> FunctionValue<'static> {
    let ptr = context.ptr_type(AddressSpace::default());
    let trap = module.add_function(
        "dux.runtime.trap",
        context.void_type().fn_type(&[ptr.into()], false),
        None,
    );
    trap.set_linkage(Linkage::Internal);
    let entry = context.append_basic_block(trap, "entry");
    builder.position_at_end(entry);
    let message = builder
        .get_insert_block()
        .unwrap()
        .get_parent()
        .unwrap()
        .get_first_param()
        .expect("message");
    let stderr = stream(context, module, builder, "stderr");
    let _ = builder.build_call(
        fputs(context, module),
        &[message.into(), stderr.into()],
        "report",
    );
    abort(context, module, builder);
    trap
}

fn emit_oom(
    context: &'static Context,
    module: &Module<'static>,
    builder: &Builder<'static>,
) -> FunctionValue<'static> {
    let ptr = context.ptr_type(AddressSpace::default());
    let oom = module.add_function(
        "dux.runtime.oom",
        context.void_type().fn_type(&[ptr.into()], false),
        None,
    );
    oom.set_linkage(Linkage::Internal);
    let entry = context.append_basic_block(oom, "entry");
    builder.position_at_end(entry);
    write_stderr(context, module, builder, "dux: out of memory\n");
    abort(context, module, builder);
    oom
}

fn bump(
    builder: &Builder<'static>,
    context: &'static Context,
    global: Option<GlobalValue<'static>>,
    amount: IntValue<'static>,
    name: &str,
) {
    let Some(global) = global else {
        return;
    };
    let now = read(builder, context, global, name);
    let next = builder.build_int_add(now, amount, "next").expect("counter");
    builder
        .build_store(global.as_pointer_value(), next)
        .expect("store");
}

fn drop_count(
    builder: &Builder<'static>,
    context: &'static Context,
    global: Option<GlobalValue<'static>>,
    amount: IntValue<'static>,
    name: &str,
) {
    let Some(global) = global else {
        return;
    };
    let now = read(builder, context, global, name);
    let next = builder.build_int_sub(now, amount, "next").expect("counter");
    builder
        .build_store(global.as_pointer_value(), next)
        .expect("store");
}

fn read(
    builder: &Builder<'static>,
    context: &'static Context,
    global: GlobalValue<'static>,
    name: &str,
) -> IntValue<'static> {
    builder
        .build_load(context.i64_type(), global.as_pointer_value(), name)
        .expect("load")
        .into_int_value()
}

fn offset(
    builder: &Builder<'static>,
    context: &'static Context,
    base: PointerValue<'static>,
    bytes: u64,
    name: &str,
) -> PointerValue<'static> {
    let index = context.i64_type().const_int(bytes, false);
    unsafe {
        builder
            .build_gep(context.i8_type(), base, &[index], name)
            .expect("gep")
    }
}

fn write_stderr(
    context: &'static Context,
    module: &Module<'static>,
    builder: &Builder<'static>,
    message: &str,
) {
    let text = builder
        .build_global_string_ptr(message, "dux.message")
        .expect("message")
        .as_pointer_value();
    let stderr = stream(context, module, builder, "stderr");
    let _ = builder.build_call(fputs(context, module), &[text.into(), stderr.into()], "report");
}

fn abort(
    context: &'static Context,
    module: &Module<'static>,
    builder: &Builder<'static>,
) {
    let code = context.i32_type().const_int(2, false);
    let _ = builder.build_call(exit_fn(context, module), &[code.into()], "abort");
    let _ = builder.build_unreachable();
}

fn call_pointer(
    builder: &Builder<'static>,
    function: FunctionValue<'static>,
    arguments: &[BasicMetadataValueEnum<'static>],
    name: &str,
) -> PointerValue<'static> {
    builder
        .build_call(function, arguments, name)
        .expect("call")
        .try_as_basic_value()
        .basic()
        .map(|value| value.into_pointer_value())
        .expect("pointer result")
}

fn counter(
    context: &'static Context,
    module: &Module<'static>,
    name: &str,
) -> GlobalValue<'static> {
    let global = module.add_global(context.i64_type(), None, name);
    global.set_initializer(&context.i64_type().const_zero());
    global
}

fn declare(
    module: &Module<'static>,
    name: &str,
    fn_type: FunctionType<'static>,
) -> FunctionValue<'static> {
    module
        .get_function(name)
        .unwrap_or_else(|| module.add_function(name, fn_type, None))
}

fn stream(
    context: &'static Context,
    module: &Module<'static>,
    builder: &Builder<'static>,
    name: &str,
) -> PointerValue<'static> {
    let ptr = context.ptr_type(AddressSpace::default());
    let global = module
        .get_global(name)
        .unwrap_or_else(|| module.add_global(BasicTypeEnum::from(ptr), None, name));
    builder
        .build_load(ptr, global.as_pointer_value(), name)
        .expect("stream")
        .into_pointer_value()
}

fn fputs(context: &'static Context, module: &Module<'static>) -> FunctionValue<'static> {
    let ptr = context.ptr_type(AddressSpace::default());
    declare(
        module,
        "fputs",
        context.i32_type().fn_type(&[ptr.into(), ptr.into()], false),
    )
}

fn fprintf(
    context: &'static Context,
    module: &Module<'static>,
) -> FunctionValue<'static> {
    let ptr = context.ptr_type(AddressSpace::default());
    declare(
        module,
        "fprintf",
        context.i32_type().fn_type(&[ptr.into(), ptr.into()], true),
    )
}

fn exit_fn(
    context: &'static Context,
    module: &Module<'static>,
) -> FunctionValue<'static> {
    declare(
        module,
        "exit",
        context.void_type().fn_type(&[context.i32_type().into()], false),
    )
}
