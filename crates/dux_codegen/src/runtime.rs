//! The generated program's allocation support.
//!
//! Every allocation goes through a *context*: two procedure pointers and a
//! label. `main` starts with the default context; a future `mem.Arena` is a
//! different `dux.Context` value bound to the hidden `context` parameter.
//!
//! Two default allocators are available:
//!
//! * the **debug allocator** maps every block with `mmap` and parks a guard
//!   page immediately after the payload: a write past the end of a block hits
//!   the guard and stops the program at once. Each block carries a header —
//!   a live/dead magic word, the payload size, the mapping's base and extent —
//!   so a release knows exactly what to hand back to `munmap`, and a magic
//!   check or a lookup in the location table catches a double free and a
//!   release of memory this allocator never handed out. Every allocation
//!   lands in a location table — payload, size, and the `file:line` the call
//!   site was written at — so the exit report names where each unreleased
//!   block came from.
//! * the **plain allocator** is a `malloc`/`free` wrapper used when the debug
//!   allocator is off, so a release build pays nothing.

use inkwell::builder::Builder;
use inkwell::context::Context;
use inkwell::module::{Linkage, Module};
use inkwell::types::{BasicTypeEnum, FunctionType, StructType};
use inkwell::values::{BasicMetadataValueEnum, FunctionValue, GlobalValue, IntValue, PointerValue};
use inkwell::{AddressSpace, IntPredicate};

const MAGIC_ALIVE: u64 = 0xA11C_0DE5_AFE0_0000;
const MAGIC_DEAD: u64 = 0xDEAD_BEEF_0000_0000;
/// Magic, payload size, mapping base, mapping extent.
const HEADER: u64 = 32;
/// The page the guard is measured in: this build runs where 4096 is a page.
const PAGE: u64 = 4096;
/// Blocks the location table can hold. Past it allocations still run and
/// still free, but the exit report can only give the tally, not the places.
const TABLE_SLOTS: u64 = 16_384;
const SIZE_MAX: u64 = u64::MAX;

/// The allocation runtime of one generated module.
pub struct Runtime {
    /// The context `main` starts with: `@dux.default_context`. Its own slots
    /// carry the allocator and the deallocator; every allocation goes
    /// through whatever context is current, so no call site names these.
    pub default_context: PointerValue<'static>,
    /// `void @dux.runtime.report()` — unreleased blocks, in debug mode.
    pub report: Option<FunctionValue<'static>>,
    /// `noreturn @dux.runtime.trap(ptr message)` — a language rule the running
    /// program broke, such as a `match` whose every arm declined.
    pub trap: FunctionValue<'static>,
}

impl Runtime {
    /// Emit the allocator runtime into `module`.
    pub fn emit(
        context: &'static Context,
        module: &Module<'static>,
        debug: bool,
        windows: bool,
    ) -> Self {
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
            context
                .void_type()
                .fn_type(&[ptr.into(), ptr.into()], false),
            None,
        );
        free.set_linkage(Linkage::Internal);

        // The counters and the location table exist only for the debug
        // allocator: a release build should not carry globals it never touches.
        let live = debug.then(|| counter(context, module, "dux.runtime.live"));
        let bytes = debug.then(|| counter(context, module, "dux.runtime.bytes"));
        let table = debug.then(|| allocation_table(context, module));
        let slots = debug.then(|| counter(context, module, "dux.runtime.table.next"));
        let overflowed = debug.then(|| counter(context, module, "dux.runtime.table.full"));
        let malloc = declare(module, "malloc", ptr.fn_type(&[i64_type.into()], false));
        let libc_free = declare(
            module,
            "free",
            context.void_type().fn_type(&[ptr.into()], false),
        );
        let mmap = declare(
            module,
            "mmap",
            ptr.fn_type(
                &[
                    ptr.into(),
                    i64_type.into(),
                    context.i32_type().into(),
                    context.i32_type().into(),
                    context.i32_type().into(),
                    i64_type.into(),
                ],
                false,
            ),
        );
        let mprotect = declare(
            module,
            "mprotect",
            context.i32_type().fn_type(
                &[ptr.into(), i64_type.into(), context.i32_type().into()],
                false,
            ),
        );
        let munmap = declare(
            module,
            "munmap",
            context
                .i32_type()
                .fn_type(&[ptr.into(), i64_type.into()], false),
        );
        let oom = emit_oom(context, module, &builder, windows);
        let trap = emit_trap(context, module, &builder, windows);

        emit_alloc(
            context, &builder, alloc, malloc, mmap, mprotect, oom, table, slots, overflowed, live,
            bytes, debug,
        );
        let report = if debug {
            let (live, bytes) = (live.expect("counters"), bytes.expect("counters"));
            emit_free_debug(
                context,
                module,
                &builder,
                windows,
                free,
                munmap,
                table.expect("table"),
                slots.expect("slots"),
                overflowed.expect("overflowed"),
                Some(live),
                Some(bytes),
            );
            Some(emit_report(
                context,
                module,
                &builder,
                windows,
                table.expect("table"),
                slots.expect("slots"),
                live,
                bytes,
            ))
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
            .build_int_compare(
                IntPredicate::ULT,
                walked.as_basic_value().into_int_value(),
                len,
                "more",
            )
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
    mmap: FunctionValue<'static>,
    mprotect: FunctionValue<'static>,
    oom: FunctionValue<'static>,
    table: Option<GlobalValue<'static>>,
    slots: Option<GlobalValue<'static>>,
    overflowed: Option<GlobalValue<'static>>,
    live: Option<GlobalValue<'static>>,
    bytes: Option<GlobalValue<'static>>,
    debug: bool,
) {
    let i64_type = context.i64_type();
    let i32_type = context.i32_type();
    let entry = context.append_basic_block(alloc, "entry");
    let failed = context.append_basic_block(alloc, "failed");
    let body = context.append_basic_block(alloc, "body");
    builder.position_at_end(entry);

    let size = alloc.get_nth_param(1).unwrap().into_int_value();
    let align = alloc.get_nth_param(2).unwrap().into_int_value();
    let location = alloc.get_nth_param(3).unwrap().into_pointer_value();
    if !debug {
        let base = call_pointer(builder, malloc, &[size.into()], "block");
        let null = builder.build_is_null(base, "null").expect("null");
        builder
            .build_conditional_branch(null, failed, body)
            .expect("branch");
        builder.position_at_end(failed);
        let _ = builder.build_call(oom, &[location.into()], "oom");
        let _ = builder.build_unreachable();
        builder.position_at_end(body);
        builder.build_return(Some(&base)).expect("return");
        return;
    }

    // The payload sits at the end of the writable region, flush against the
    // guard page, so its size rounded up to at least a doubleword is what the
    // mapping has to fit: rounding keeps the payload aligned to whatever the
    // call site asked for.
    let floor = i64_type.const_int(16, false);
    let wide = builder
        .build_int_compare(IntPredicate::UGE, align, floor, "wide")
        .expect("wide");
    let quantum = builder
        .build_select(wide, align, floor, "quantum")
        .expect("quantum")
        .into_int_value();
    let padded = builder
        .build_int_add(size, quantum, "padded")
        .expect("padded");
    let one = i64_type.const_int(1, false);
    let slop = builder.build_int_sub(padded, one, "slop").expect("slop");
    let mask = builder.build_int_neg(quantum, "mask").expect("mask");
    let room = builder.build_and(slop, mask, "room").expect("room");
    let header = i64_type.const_int(HEADER, false);
    let need = builder.build_int_add(room, header, "need").expect("need");
    let page = i64_type.const_int(PAGE, false);
    let page_slop = builder
        .build_int_add(need, i64_type.const_int(PAGE - 1, false), "page slop")
        .expect("page slop");
    let page_mask = builder.build_int_neg(page, "page mask").expect("page mask");
    let writable = builder
        .build_and(page_slop, page_mask, "writable")
        .expect("writable");
    let extent = builder
        .build_int_add(writable, page, "extent")
        .expect("extent");

    // mmap(null, extent, PROT_READ|PROT_WRITE, MAP_PRIVATE|MAP_ANONYMOUS, -1, 0)
    let map = call_pointer(
        builder,
        mmap,
        &[
            context
                .ptr_type(AddressSpace::default())
                .const_null()
                .into(),
            extent.into(),
            i32_type.const_int(3, false).into(),
            i32_type.const_int(0x22, false).into(),
            i32_type.const_all_ones().into(),
            i64_type.const_zero().into(),
        ],
        "mapping",
    );
    let bits = builder
        .build_ptr_to_int(map, i64_type, "bits")
        .expect("bits");
    let refused = builder
        .build_int_compare(IntPredicate::EQ, bits, i64_type.const_all_ones(), "refused")
        .expect("refused");
    builder
        .build_conditional_branch(refused, failed, body)
        .expect("branch");

    builder.position_at_end(failed);
    let _ = builder.build_call(oom, &[location.into()], "oom");
    let _ = builder.build_unreachable();

    builder.position_at_end(body);
    // mprotect(guard, PAGE, PROT_NONE) — the page after the payload is now a
    // fault: an overrun stops the program where it happens.
    let guard = unsafe {
        builder
            .build_gep(context.i8_type(), map, &[writable], "guard")
            .expect("guard")
    };
    let _ = builder.build_call(
        mprotect,
        &[guard.into(), page.into(), i32_type.const_zero().into()],
        "arm",
    );
    let back = builder.build_int_neg(room, "back").expect("back");
    let memory = unsafe {
        builder
            .build_gep(context.i8_type(), guard, &[back], "memory")
            .expect("memory")
    };
    let at_header = offset(builder, context, memory, SIZE_MAX - HEADER + 1, "header");
    builder
        .build_store(at_header, i64_type.const_int(MAGIC_ALIVE, false))
        .expect("magic");
    builder
        .build_store(offset(builder, context, at_header, 8, "size"), size)
        .expect("size");
    builder
        .build_store(offset(builder, context, at_header, 16, "base"), map)
        .expect("base");
    builder
        .build_store(offset(builder, context, at_header, 24, "extent"), extent)
        .expect("extent");
    record(
        context, builder, table, slots, overflowed, memory, size, location,
    );
    bump(builder, context, live, one, "live");
    bump(builder, context, bytes, size, "bytes");
    builder.build_return(Some(&memory)).expect("return");
}

/// The location table's row shape: the payload, its size, and the
/// `file:line` string of the allocation that made it.
fn table_entry_type(context: &'static Context) -> StructType<'static> {
    let ptr = context.ptr_type(AddressSpace::default());
    context.struct_type(&[ptr.into(), context.i64_type().into(), ptr.into()], false)
}

fn allocation_table(context: &'static Context, module: &Module<'static>) -> GlobalValue<'static> {
    let array = table_entry_type(context).array_type(TABLE_SLOTS as u32);
    let global = module.add_global(array, None, "dux.runtime.table");
    global.set_initializer(&array.const_zero());
    global
}

fn slot_pointer(
    context: &'static Context,
    builder: &Builder<'static>,
    table: GlobalValue<'static>,
    slot: IntValue<'static>,
    name: &str,
) -> PointerValue<'static> {
    let array = table_entry_type(context).array_type(TABLE_SLOTS as u32);
    unsafe {
        builder
            .build_gep(
                array,
                table.as_pointer_value(),
                &[context.i64_type().const_zero(), slot],
                name,
            )
            .expect("row")
    }
}

/// Note an allocation in the location table. A full table is recorded once
/// and never blocks an allocation; it only costs the exit report its detail.
#[allow(clippy::too_many_arguments)]
fn record(
    context: &'static Context,
    builder: &Builder<'static>,
    table: Option<GlobalValue<'static>>,
    slots: Option<GlobalValue<'static>>,
    overflowed: Option<GlobalValue<'static>>,
    memory: PointerValue<'static>,
    size: IntValue<'static>,
    location: PointerValue<'static>,
) {
    let (Some(table), Some(slots), Some(overflowed)) = (table, slots, overflowed) else {
        return;
    };
    let i64_type = context.i64_type();
    let function = builder.get_insert_block().unwrap().get_parent().unwrap();
    let insert = context.append_basic_block(function, "table.insert");
    let overflow = context.append_basic_block(function, "table.overflow");
    let beyond = context.append_basic_block(function, "table.beyond");
    let current = read(builder, context, slots, "slot");
    let spare = builder
        .build_int_compare(
            IntPredicate::ULT,
            current,
            i64_type.const_int(TABLE_SLOTS, false),
            "spare",
        )
        .expect("spare");
    builder
        .build_conditional_branch(spare, insert, overflow)
        .expect("branch");

    builder.position_at_end(insert);
    let row = slot_pointer(context, builder, table, current, "row");
    builder.build_store(row, memory).expect("payload slot");
    builder
        .build_store(offset(builder, context, row, 8, "row size"), size)
        .expect("size slot");
    builder
        .build_store(offset(builder, context, row, 16, "row place"), location)
        .expect("location slot");
    let successor = builder
        .build_int_add(current, i64_type.const_int(1, false), "next slot")
        .expect("next slot");
    builder
        .build_store(slots.as_pointer_value(), successor)
        .expect("slot count");
    builder.build_unconditional_branch(beyond).expect("beyond");

    builder.position_at_end(overflow);
    builder
        .build_store(overflowed.as_pointer_value(), i64_type.const_int(1, false))
        .expect("overflow flag");
    builder.build_unconditional_branch(beyond).expect("beyond");

    builder.position_at_end(beyond);
}

#[allow(clippy::too_many_arguments)]
fn emit_free_debug(
    context: &'static Context,
    module: &Module<'static>,
    builder: &Builder<'static>,
    windows: bool,
    free: FunctionValue<'static>,
    munmap: FunctionValue<'static>,
    table: GlobalValue<'static>,
    slots: GlobalValue<'static>,
    overflowed: GlobalValue<'static>,
    live: Option<GlobalValue<'static>>,
    bytes: Option<GlobalValue<'static>>,
) {
    let i64_type = context.i64_type();
    let entry = context.append_basic_block(free, "entry");
    let done = context.append_basic_block(free, "done");
    let head = context.append_basic_block(free, "scan");
    let advance = context.append_basic_block(free, "advance");
    let unlisted = context.append_basic_block(free, "unlisted");
    let in_band = context.append_basic_block(free, "in band");
    let bad = context.append_basic_block(free, "bad");
    let erase = context.append_basic_block(free, "erase");
    let release = context.append_basic_block(free, "release");
    builder.position_at_end(entry);
    let memory = free.get_nth_param(1).unwrap().into_pointer_value();
    // As in C, freeing nothing is a silent no-op: an arena that handed out
    // nothing has nothing to take back.
    let is_null = builder.build_is_null(memory, "null").expect("null");
    builder
        .build_conditional_branch(is_null, done, head)
        .expect("branch");
    builder.position_at_end(done);
    builder.build_return(None).expect("return");

    // The location table is the authority on what is live: an unmapped
    // block's header can be read by nothing, so the table — not memory
    // behind the pointer — decides whether this pointer was handed out.
    builder.position_at_end(head);
    let slot = builder.build_phi(i64_type, "slot").expect("slot");
    slot.add_incoming(&[(&i64_type.const_zero(), entry)]);
    let row = slot_pointer(
        context,
        builder,
        table,
        slot.as_basic_value().into_int_value(),
        "row",
    );
    let listed = builder
        .build_load(context.ptr_type(AddressSpace::default()), row, "listed")
        .expect("listed")
        .into_pointer_value();
    let hit = builder
        .build_int_compare(IntPredicate::EQ, listed, memory, "hit")
        .expect("hit");
    builder
        .build_conditional_branch(hit, erase, advance)
        .expect("branch");

    builder.position_at_end(advance);
    let steps = builder
        .build_int_add(
            slot.as_basic_value().into_int_value(),
            i64_type.const_int(1, false),
            "steps",
        )
        .expect("steps");
    let filled = read(builder, context, slots, "filled");
    let more = builder
        .build_int_compare(IntPredicate::ULT, steps, filled, "more")
        .expect("more");
    slot.add_incoming(&[(&steps, advance)]);
    builder
        .build_conditional_branch(more, head, unlisted)
        .expect("branch");

    // Not in the table: either the table filled and missed this block — then
    // the header, which must be mapped for a live block, gets the last word —
    // or this pointer never came from the allocator, or was already released.
    // Either way the stale pointer is never read.
    builder.position_at_end(unlisted);
    let overflowing = read(builder, context, overflowed, "overflowing");
    let hopeless = builder
        .build_int_compare(
            IntPredicate::EQ,
            overflowing,
            i64_type.const_zero(),
            "hopeless",
        )
        .expect("hopeless");
    builder
        .build_conditional_branch(hopeless, bad, in_band)
        .expect("branch");

    builder.position_at_end(in_band);
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
        windows,
        "dux: free of memory this allocator did not hand out, or a double free\n",
    );
    abort(context, module, builder);

    builder.position_at_end(erase);
    builder
        .build_store(row, context.ptr_type(AddressSpace::default()).const_null())
        .expect("erase");
    builder
        .build_unconditional_branch(release)
        .expect("release");

    builder.position_at_end(release);
    let header = offset(builder, context, memory, SIZE_MAX - HEADER + 1, "header");
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
    let base = builder
        .build_load(
            context.ptr_type(AddressSpace::default()),
            offset(builder, context, header, 16, "map base"),
            "map base",
        )
        .expect("map base")
        .into_pointer_value();
    let extent = builder
        .build_load(
            i64_type,
            offset(builder, context, header, 24, "map extent"),
            "map extent",
        )
        .expect("map extent")
        .into_int_value();
    drop_count(builder, context, live, i64_type.const_int(1, false), "live");
    drop_count(builder, context, bytes, payload, "bytes");
    let _ = builder.build_call(munmap, &[base.into(), extent.into()], "munmap");
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

#[allow(clippy::too_many_arguments)]
fn emit_report(
    context: &'static Context,
    module: &Module<'static>,
    builder: &Builder<'static>,
    windows: bool,
    table: GlobalValue<'static>,
    slots: GlobalValue<'static>,
    live: GlobalValue<'static>,
    bytes: GlobalValue<'static>,
) -> FunctionValue<'static> {
    // The report is only emitted when the location table and both counters
    // exist.
    let report = module.add_function(
        "dux.runtime.report",
        context.void_type().fn_type(&[], false),
        None,
    );
    report.set_linkage(Linkage::Internal);
    let entry = context.append_basic_block(report, "entry");
    let head = context.append_basic_block(report, "walk");
    let say_row = context.append_basic_block(report, "say block");
    let step = context.append_basic_block(report, "step");
    let tally = context.append_basic_block(report, "tally");
    let say = context.append_basic_block(report, "say");
    let done = context.append_basic_block(report, "done");

    // Every row the allocator still owns names itself: the tally alone says
    // how much leaked, the table says from where.
    builder.position_at_end(entry);
    let _ = builder.build_unconditional_branch(head);
    builder.position_at_end(head);
    let slot = builder.build_phi(context.i64_type(), "slot").expect("slot");
    slot.add_incoming(&[(&context.i64_type().const_zero(), entry)]);
    let row = slot_pointer(
        context,
        builder,
        table,
        slot.as_basic_value().into_int_value(),
        "row",
    );
    let listed = builder
        .build_load(context.ptr_type(AddressSpace::default()), row, "listed")
        .expect("listed")
        .into_pointer_value();
    let gone = builder.build_is_null(listed, "gone").expect("gone");
    builder
        .build_conditional_branch(gone, step, say_row)
        .expect("branch");

    builder.position_at_end(say_row);
    let payload_size = builder
        .build_load(
            context.i64_type(),
            offset(builder, context, row, 8, "row payload"),
            "row payload",
        )
        .expect("row payload");
    let place = builder
        .build_load(
            context.ptr_type(AddressSpace::default()),
            offset(builder, context, row, 16, "row place"),
            "row place",
        )
        .expect("row place");
    let where_format = builder
        .build_global_string_ptr(
            "dux:   leaked %lu bytes, allocated at %s\n",
            "dux.leak.place",
        )
        .expect("place format")
        .as_pointer_value();
    let stderr = stream(context, module, builder, "stderr", windows);
    let _ = builder.build_call(
        fprintf(context, module),
        &[
            stderr.into(),
            where_format.into(),
            payload_size.into(),
            place.into(),
        ],
        "where",
    );
    let _ = builder.build_unconditional_branch(step);

    builder.position_at_end(step);
    let next = builder
        .build_int_add(
            slot.as_basic_value().into_int_value(),
            context.i64_type().const_int(1, false),
            "next",
        )
        .expect("next");
    let filled = read(builder, context, slots, "filled");
    let more = builder
        .build_int_compare(IntPredicate::ULT, next, filled, "more")
        .expect("more");
    slot.add_incoming(&[(&next, step)]);
    builder
        .build_conditional_branch(more, head, tally)
        .expect("branch");

    builder.position_at_end(tally);
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
    let stderr = stream(context, module, builder, "stderr", windows);
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
    windows: bool,
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
    let stderr = stream(context, module, builder, "stderr", windows);
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
    windows: bool,
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
    write_stderr(context, module, builder, windows, "dux: out of memory\n");
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
    windows: bool,
    message: &str,
) {
    let text = builder
        .build_global_string_ptr(message, "dux.message")
        .expect("message")
        .as_pointer_value();
    let stderr = stream(context, module, builder, "stderr", windows);
    let _ = builder.build_call(
        fputs(context, module),
        &[text.into(), stderr.into()],
        "report",
    );
}

fn abort(context: &'static Context, module: &Module<'static>, builder: &Builder<'static>) {
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
    windows: bool,
) -> PointerValue<'static> {
    let ptr = context.ptr_type(AddressSpace::default());
    // Windows has no `stdout` in storage to read: its C runtime hands out
    // the standard streams by call, `__acrt_iob_func` with the slot number.
    if windows {
        let index = u64::from(name == "stderr");
        let number = context.i32_type().const_int(index, false);
        let iob = declare(
            module,
            "__acrt_iob_func",
            ptr.fn_type(&[context.i32_type().into()], false),
        );
        return builder
            .build_call(iob, &[number.into()], name)
            .expect("iob")
            .try_as_basic_value()
            .basic()
            .expect("stream")
            .into_pointer_value();
    }
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

fn fprintf(context: &'static Context, module: &Module<'static>) -> FunctionValue<'static> {
    let ptr = context.ptr_type(AddressSpace::default());
    declare(
        module,
        "fprintf",
        context.i32_type().fn_type(&[ptr.into(), ptr.into()], true),
    )
}

fn exit_fn(context: &'static Context, module: &Module<'static>) -> FunctionValue<'static> {
    declare(
        module,
        "exit",
        context
            .void_type()
            .fn_type(&[context.i32_type().into()], false),
    )
}
