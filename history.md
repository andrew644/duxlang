`DESIGN.md` written (13 sections, ~39 KB) + `ADMIN.md` with the `dnf` requests. I have **not** committed — the doc
is deliberately full of undecided items, so it's unfinished work.

Here's the grill. Answer by number; each has a recommendation so "all defaults" is a valid answer.

**What I found on your box that shapes the plan:** LLVM 23.1.5 + clang 23 exist but
`llvm-devel`/`clang-devel`/`lld` do **not**, so I designed M0–M3 around emitting textual IR and shelling out to
`clang` (no native LLVM headers needed). Also `clang -Xclang -ast-dump=json` works, which is a real alternative to
libclang for Zig-style import.

**Blocking — I can't write M1/M2 without these:**

1. **What is Dux *for*?** Games/engine, CLIs, services, embedded? This picks the allocator model, the stdlib, and
which C headers the interop test corpus must survive. It's the answer I need most.
2. **Allocator model** (§4.8): Odin's implicit `context` hidden parameter, or Zig's explicit `alloc: Allocator`
args? Context breaks C-ABI compatibility of `proc` types by default — that's a direct collision with your interop
goal. My lean: explicit allocator, no hidden context.
3. **Nullable pointers**: can `*T` be nil (Odin), or is `?*T` the only nullable spelling? Decides whether
`Option(*T)` niche-optimises to pointer size.
4. **`enum` + `union` two keywords, or one Rust-style sum type?** And is `enum` integer-backed (usable as a C enum
/ bitflags base)?
5. **Error sugar** (§4.5): `Result(T,E)` + `or else`/`or_return`, or Zig `E!T`, or Rust `Result` + `?`, or Odin
multi-return `(T, Error)`? Related: **single return value or multi-return?** These two are coupled and they define
the feel of the language.
6. **Error type model**: parameterised `E` per function, or Zig's global error set? I recommend parameterised only
— a global error set kills `Result(T, E)`'s whole point.
7. **Syntax**: Odin-ish (`::`, `proc`, `^T`, `#!`) or Zig-ish (`fn`, `const`, `[*]T`)? **Write me a 10-line "hello
world + sum type + Result" snippet in the syntax you actually want to type.** I'll treat it as normative and
rewrite §3.2/§5 around it.
8. **C int types**: distinct `c_int`/`c_long`/`c_char`/`c_long_double`, or collapse to `i32`/`i64`? I strongly
recommend distinct — collapsing to `i64` for `long` is how silent corruption ships on LLP64 and 32-bit `time_t`.
9. **`static inline` functions** in headers: skip them, or compile the translation unit to bitcode and link it?
Skipping means header-only libs (stb, cglm, cxx) are unusable, which undercuts "painless interop".
10. **`panic` semantics**: print + trap, no unwinding, `defer` does *not* run. Confirm?
11. **Comptime**: constant folding + `when` only, no comptime interpreter. Confirm — this single call is worth
roughly 25% of the compiler's complexity, and it's the #1 way Odin/Zig envy eats the project.
12. **LLVM linkage** (`[Q09]`): textual IR + `clang` only, or link libLLVM via `llvm-sys 231` (needs the `dnf`
install in `ADMIN.md`)?
13. **Header reader** (`[Q10]`): `libclang` (needs `clang-devel`) or `clang -ast-dump=json` subprocess (works
right now)? I lean subprocess for v1 — snapshot-testable golden JSON, no new system dep, and I keep an internal
`CDecl` layer so libclang can slot in later.
14. **Is LSP/IDE support a stated goal?** My front-end design already pays for it (arena-allocated immutable AST,
error-tolerant parse, `dux check`). Say no and I'll stop paying.

**Non-blocking — I'll adopt these defaults unless you object:** no borrow checker/GC/runtime; zero implicit
numeric conversions (constants excepted); `int = i32`; wrap-in-release/trap-in-debug overflow; `or else` requires
`Result` return type; unused `Result` = warning; monomorphisation-only generics with no traits; layout of C types
always read from clang metadata, never re-derived; `[dynamic]T` and `map[K]V` built-in syntax; DWARF by M7; M0
ends with `hello.dux` → `Hello, world!` through the e2e harness.

Two flags on the roadmap: **M4 (sum types/`Option`/`Result`) and M6 (`#c_include`) are the two features you
actually asked for** — say the word and I'll pull the C-interop spike earlier than M6, since it's also the
riskiest part (§11.1).

❯ Dux is for game dev primarily. Allocator should be like odin's implicit contex hidden parameter. Only way to
  repesent null pointers is an optional pointer that is none. Use rust style sum type. We will delay the enum int
  based decision for now. Single return for simplicity. We can add the sugar later. Parameterised errors only. I
  added the example dir for basic syntax. Result / sum type syntax should match what rust does for now. Keep c_
  types for compatability with C. But main dux code should use rust style types with the ability to convert them
  where necessary. We support header-only C libs, so compile them. Confirmed on 10, no unwinding. There will be no
  metaprogramming. we will link libLLVM (using inkwell?) Use libclang. LSP will be supported. Allow automatic
  numeric conversions where the destination type is larger than the source type (i32 -> i64 is ok) Create example
  dux files in examples dir for me to review and edit syntax. We can leave C-interop where you have it in the
  roadmap.

---

## Session 2 (2026-11-23) — decisions folded in, syntax examples written, committed

All 14 grill answers are now **in the design**, and `ADMIN.md` request #1 is done (I verified:
`llvm-devel`/`clang-devel`/`lld`/`lldb`/`mingw64-gcc-c++` are installed, `libLLVM-23.so` +
`libclang.so` + `/usr/include/clang-c` present). Also verified `inkwell 0.11.0` publishes an
`llvm23-1` feature → exact match for system LLVM 23.1.5, and `llvm-sys 231.0.2` too. Note:
`crates.io`'s REST API 403s from this box but the sparse index + `static.crates.io` work, so
cargo is fine (recorded in `ADMIN.md` §4).

**What changed**

- `DESIGN.md` rewritten from "grill list" to **decided**: 13 → 13 sections, `[Q##]` markers
  replaced by D1–D25 decision log (§12) and a short `[OPEN-n]` list (§13). Headline decisions:
  games/engine domain; `inkwell(llvm23-1)` + libclang (no textual-IR build path); Rust-shaped
  `enum` + `match`; single return + tuple; per-function `E`; implicit Odin `context` with
  `#[no_context]` for C callbacks; `*T` non-null so `Option<*T>` is the only nullable pointer
  (⇒ niche layout is mandatory, not optional); implicit conversion only when lossless; distinct
  `c_*` types + `as`; `static inline`/header-only via cached bitcode archive; no metaprogramming;
  no `unsafe`; LSP yes. M0 now links libLLVM for real. C interop stays at M6 as instructed.
- `example/` grew from `fizzbuzz.dux` (untouched, it is the anchor) to 11 syntax probes +
  `README.md`, which is the review artifact: rules the examples assume (15), **open items 1–10 to
  rule on** (binding spellings, `impl` receivers, `&`/`*`, slices vs arrays, `Vec<T>.new()`
  spelling, `fn(...)` vs `proc(...)`, which error sugar, `loop` vs `for {}`, interpolation holes,
  when integer-backed `enum` returns), plus a "not invented here yet" list (visibility, `#[test]`,
  bit fields, C `union`, format specs).
- `ADMIN.md`: request #1 marked done with the verified package state + the inkwell/LLVM pairing
  record; new request #2 `sudo dnf install stb-devel glfw-devel` (M6 interop corpus: `stb_image.h`
  is the direct test for header-only support, GLFW for a games sample). Both are available in the
  Fedora repos. SDL2/GL/zlib/sqlite3/pthread headers are already present.

**Committed** as one commit (docs + examples only; no compiler code exists yet, so no tests to run).

**Next step when you pick it up:** wait for the owner's rulings on `example/README.md` open items
1–10 (syntax churn is cheap now, ruinous after M1), then start **M0**: cargo workspace,
`duxc --version`, e2e harness (`.dux` → compile → run → stdout compare, `UPDATE_EXPECTED=1`), and
`hello.dux` codegen'd through inkwell against system LLVM 23.1.5 → prints `Hello, world!`. M0 is
not started; nothing under `crates/` exists.

### M0 pre-flight spike (scratch, `/tmp/spike`, not committed): D3 verified

`inkwell 0.11.0` + feature `llvm23-1` **builds and works against system LLVM 23.1.5 on this
box**: module → new-PM `default<O2>` → `.o` → `clang` link → correct stdout. So "link libLLVM"
(D3) is no longer a risk, and M0 can start with the real back end. Four findings are now in
DESIGN.md §6.2 so they are not re-learned at M2:

- LLVM 23 removed the legacy pass manager: optimisation is `Module::run_passes("default<O2>",
  &tm, PassBuilderOptions)` (pipeline strings), inkwell's per-pass `add_*_pass` helpers are gone.
- `tm.set_data_layout(&module)` is not automatic — a fresh module's data layout is empty, which
  would silently mis-layout `Option<T>`/sum types.
- **`main` must be emitted as `i32 main()` returning 0**: a void `main` left `puts`' return value
  in `rax` and the hello binary exited 34 (its own byte count). That is a codegen rule, not trivia.
- `Context::ptr_type()` (inkwell's `IntType::ptr_type` is deprecated); DIBuilder entry point is
  `module.create_debug_info_builder(...)` with `DWARFSourceLanguage::C99` +
  `DWARFEmissionKind::LineTablesOnly`.

## Session 2b — owner's syntax edit pass folded in

The owner edited `types.dux`, `strings.dux`, `structs.dux`, `pointers.dux` (those edits landed
inside commit `6e2fe48`, which swept up the whole tree — worth remembering: `git add -A` here
also commits their in-flight edits). Their preferences, now applied to **all** examples and to
DESIGN.md (D26, D27):

- No `let`, no `mut`. `x := e` infers, `x: T = e` states the type, `x: T` declares
  zero-initialised storage, plain `=` assigns. Bindings are mutable.
- Module storage: they deleted `static hits: i32 = 0`. I re-added it as Odin's `global` and
  flagged it as `[OPEN-1]` together with the one `mask: u64 := 0xff00` (normalised to `: T =`).
- **Pattern conditions**: `while Some(node) := cursor { }`, `if Ok(n) := parse_slot("42") { }`
  replace `if let` / `while let` everywhere (README rule 2, DESIGN §4.4, M4).

`example/README.md` was restructured: rules 1-15 (what is settled) and open items 1-10 (what the
owner still has to rule on — `[OPEN-1]`..`[OPEN-10]` map 1:1 to DESIGN.md §13.1).


## Session 3 — M0 lands: the pipeline is real

`cargo test` is green: 55 tests (14 parser, 13 lexer, 7 codegen IR, 8 CLI, 3 driver, 3 diag,
3 source-map, 5 e2e). `example/hello.dux` compiles through system LLVM 23.1.5, links with clang
and prints `Hello, world!`.

Workspace: `dux_source`, `dux_diag`, `dux_lexer`, `dux_ast`, `dux_parser`, `dux_codegen`,
`dux_driver`, `duxc`. Only `dux_codegen` may know LLVM exists; `duxc` never links LLVM directly.
CLI is `duxc version|check|build|run` with `--emit=ir|obj|exe`, `--release`, `-o`, `--out-dir`,
and `run file -- program args`. Scratch goes to `--out-dir` (default `.dux-cache`), the harness to
`target/tmp`.

Decisions and bugs worth remembering, all now under tests:

- **Statement termination (D28).** The examples never wrote `;`, so requiring them was never the
  language. The lexer now records "a newline preceded this token" and the parser ends a statement
  at a newline, `;` or `}` (`E0008` when a line would otherwise hold two statements). Expressions
  may still wrap after an operator.
- **`println` double newline.** `println` appended `\n` *and* called `puts`, which appends its
  own. Now: `println` → `puts(text)`, `print` → `fputs(text, stdout)` (D29) — the compiler never
  adds a newline to a literal it was already printing.
- **Top-level recovery loop.** `recover_to_top_level` returned at the offending token, so an
  unparsed `struct`/`enum`/`const`/`global`/`impl` spun the `program()` loop forever — every
  example except `hello.dux` hung. Recovery now always consumes at least one token and the top
  level asserts forward progress. `no_example_hangs_or_panics_the_compiler` is now a permanent
  gate (it runs the whole `example/` corpus under `timeout 20`).
- **Semicolon policy in blocks.** `;` is a terminator handled by the block loop, so statements are
  indexed cleanly; a stray `;` is `E0008 "a statement cannot be empty"`.
- Raw strings must survive newlines: `string_body` used to report "not closed before end of line"
  even in raw mode.
- `as` binds tightest (`1 + 2 as i32` is `1 + (2 as i32)`); `*` tighter than `+`; verified.
- Codegen facts confirmed: `module.set_triple` + `set_data_layout` from the `TargetMachine`,
  `i32 main()` returning 0, `default<O2>` via `run_passes`, inkwell types are `!Sync` so the LLVM
  `Context` lives in a `thread_local!` (leaked) rather than a `static`.

Tests found the two codegen bugs and the parser's `=`-assignment regression above; the e2e harness
supports `//@exit`, `//@expect-error`, `//@stderr`, `//@release`, `//@args` and
`UPDATE_EXPECTED=1`.

Not done (next): top-level `const`/`global`/`struct`/`enum`/`type`/`impl`/attributes in the AST +
parser (M1) — today they are `E0005`/`E0007`; `match`, patterns, ranges and control flow are M2/M4.


## Session 4 — the front end reaches the normative examples

M0 committed (`36d0ad9`). This step made the parser able to read **every** file in `example/`
with zero syntax errors, which is what turns those files into a specification instead of a wish
list. `cargo test`: 68 passing, clippy clean.

Added to the AST and parser:

- Declarations: `const`, `global` (value optional — `global scratch: [i32; 8]` is
  zero-initialised), `struct`, `enum` with tuple/named/empty variant payloads, `type` aliases,
  `impl` blocks, `mod cs;` and `extern "C" { .. }` with body-less declarations.
- Attributes as data: `#[c_include("stdio.h")]`, `#[c_flag("-DSTB_IMAGE_IMPLEMENTATION")]`,
  `#[link(name = "m")]` (named argument), attached to the declaration that follows.
- Expressions: struct values (`Vec3 { x: 1.0 }`, also qualified `Shape::Rect { .. }`),
  `A::B` paths, array values `[1, 2, 3]` and repeated `[0.0; N]`, ranges `0..4` / `4..=9`,
  tuple members `pair.0`, nested block statements, `&x`, `*p = 2`.
- Types: function-pointer types `fn(i32) -> i32`, qualified type arguments `Option<*cs.FILE>`,
  const generic arguments `zeros<4>()`, and `TypeParam { is_const }` for `<const N: usize>`.
- `self` is a parameter with no written type; `self: *Vec3` is an explicit pointer receiver.

Two rules the examples forced into the open:

- **A binary operator must start on the same line as its left operand.** `x := a` newline
  `+ b` is two statements (as in Go); breaking *after* an operator is how an expression wraps.
  Without this, `p := &n` followed by `*p = 2` glued into `&n * p = 2`.
- **`match` / `if` in expression position report one `E0007` and are skipped as a unit**
  (`skip_expression_blocks`, `else` chains included), so an M4 gap produces one milestone
  diagnostic instead of a cascade.

Permanent new gates: `every_example_parses_so_the_review_samples_stay_the_target_surface`,
plus 11 new parser unit tests. `E0007` is now explicitly documented as "valid Dux, not yet
implemented — a milestone gap, never a syntax verdict".


## Session 5 — control flow and interpolation reach the AST

Parser work, no codegen yet. 75 tests pass, clippy clean, every example still parses with zero
syntax errors.

New AST: `Stmt::If` / `While` / `Loop` / `For{ForKind::ThreeClause|In}`, `Expr::If` (value
position, `else` mandatory), `Expr::Block` (block as a value), `Expr::CondBinding` for
`if P := e` / `while P := e` (design D27 — the pattern stays an expression until the checker owns
patterns at M4), and `Expr::InterpString { parts }` with `InterpolationPart::{Text, Hole}`.

- **Holes are parsed out of the same file.** `dux_lexer::lex_range` lexes a byte range and keeps
  spans absolute, so `println($"i={i}")` produces an expression whose span points at `i` in the
  original line and a diagnostic there lands on the right column. A hole with nothing written
  (`{}`) is *not* parsed — it means "take the next trailing argument", which is what
  `println($"{} has {} letters", name, n)` relies on.
- **A struct value cannot start inside a control-flow head** (`in_condition` depth):
  `if ready { .. }` must not read `ready { .. }`, and `while Some(node) := cursor { .. }` would
  otherwise read `cursor { .. }` as the value of the pattern condition.
- **`for` has three heads and they are told apart without consuming anything**: `for_head_shape`
  scans for an assignment before the first top-level `,` — that separates
  `for i := 1, i <= 100, i += 1` from `for i, v in xs`; `for { }` is the same statement as
  `loop { }`. The init and step of the three clause form are *statements* (`Stmt::Binding` /
  `Stmt::Assign`), not expressions.
- `match` is now the only control-flow form that reports `E0007`.

Fixed while writing the tests: an early `for_clauses` treated `for i, v in xs` as three-clause,
and a pattern condition's value was parsed outside the no-struct-literal guard.


## Session 6 — the emitter lowers a real program

Finished the half-written `crates/dux_codegen/src/lower.rs` (it had been left untracked and
disconnected: `binary_op` was a stub, the `&&`/`||` phi was fake, `declare` gave every function a
void return) and wired it into `CompiledModule::build`, replacing the hello-only `Emitter`.
`cargo test`: 86 passing, clippy clean. **`example/fizzbuzz.dux` now compiles, runs, and matches
`fizzbuzz.expected` in debug and `--release`** — that is the first non-trivial program the
compiler can actually build.

The emitter today:

- Functions: two passes (declare signatures, then lower bodies). Primitive parameters/results,
  `return`, arity checked at the call. `main` is `int main(void)` (D12); fall-through returns 0.
- Bindings and assignment: one alloca per binding in the entry block (inserted *before* the
  terminator, which is what the verifier demands), compound assignment through the same binary
  lowering. Integer literals default to `i32` (types.dux comment) and adopt an annotated target
  type when they fit; `-2147483648` reaches `i32` through the negation-of-a-literal path.
- D25 implemented where it bites: implicit conversion exactly when lossless (s→s/u→w, u→s
  strictly wider, ≤i32→f64, f32→f64); anything else is one diagnostic that prints the `as`
  spelling. `as` maps to the full cast set (sext/zext/trunc, sitofp/fptosi, fptrunc/fpext,
  ptr↔int). Mixed signedness at one rank picks the wider signed type.
- Control flow: `if`/`else if`/`else`, `while`, `loop`, three-clause `for` (cond/body/step/exit,
  `continue` binds to the step block), `break`/`continue` through a loop stack. Failed conditions
  become `false` so every block keeps a terminator and the verifier stays quiet.
- `&&`/`||` short-circuit with a real `phi i1`.
- `print`/`println`: literal strings stay `puts`/`fputs` (no libc stdio setup for hello world);
  `$".."` lowers to one `printf` with the holes' conversions chosen from their types
  (`%d/%u/%ld/%lu/%g/%s/%p`, bool selects a `"true"`/`"false"` literal, small ints and f32
  promote for varargs, `%%` escaped). Written holes read scope; `{}` holes consume the call's
  trailing arguments in order — `println($"{}", i)` (rule 10) works.

Still `E0007` by milestone, each named in its diagnostic: `defer`, `for .. in` (M3); pattern
conditions and `match` (M4); generics (M5); `const`/`global` (the resolver's side of M2).

Fixed while writing the tests: allocas after the entry terminator; void return type on every
declared function; the old `x := 1` milestone test now uses `defer` since bindings work.

Promoted into `tests/e2e/`: `fizzbuzz.dux` (with `//@release`) and `flow.dux` (functions, all
three loop forms, break/continue, short-circuit, interpolation), plus `lossy_narrow.dux`
(`//@expect-error E0005`) as the first negative codegen fixture. 11 new `ir.rs` tests pin the
shapes above.
