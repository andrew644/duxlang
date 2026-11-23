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
