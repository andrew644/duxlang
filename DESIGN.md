# Dux — Design Document

Status: **decisions resolved 2026-11-23.** The blocking questions from the review round are
answered and recorded in [§12 Decision log](#12-decision-log); what is left is listed in
[§13 Open items](#13-open-items). The `.dux` files in [`example/`](example/README.md) are
**normative for surface syntax**: where this document and `example/*.dux` disagree, the
examples win.

---

## 1. Vision

Dux is a small, statically typed, AOT-compiled systems language **for games and game
engines**: manual memory through an implicit allocator context, no GC, no runtime, no borrow
checker, C ABI compatible, compiled through LLVM 23.

Four headline features:

1. **Rust-shaped sum types.** One `enum` keyword with per-variant payloads and `match`, plus
   `Option<T>` / `Result<T, E>` predeclared as ordinary enums. Optional and error handling are
   language-shaped, not stdlib-shaped.
2. **Zig-style C interop.** `#[c_include("SDL2/SDL.h")] mod sdl;` and the declarations are
   *there* — no binding files, no committed codegen, no separate build step. Header-only
   libraries work.
3. **Odin's implicit `context`.** `new`/`make`/`free` and every allocating stdlib call take a
   hidden context parameter; rebind `context` to point a subsystem at an arena.
4. **Honest numerics.** Rust-family widths (`i8…i128`, `u8…u128`, `isize usize`, `f16…f128`),
   implicit conversion only when it is provably lossless, and distinct `c_int` / `c_long` /
   `c_char` / `c_long_double` types so a platform ABI surprise is a type error.

### 1.1 Non-goals (v1)

Each of these is a language in disguise; they are out, not "out for now":

- No borrow checker, no ownership analysis, no lifetimes. Dangling pointers are the user's
  problem, as in Odin and C.
- No GC, no runtime, no `async`/`await`, no green threads, no scheduler.
- **No metaprogramming.** No Zig-style comptime interpreter, no code that runs types. Const
  evaluation is constant folding + `when` + `const` generic parameters, full stop.
- No C++ headers, no name mangling reimplementation, no template parsing. C only.
- No traits / typeclasses / interfaces. Generic bodies are checked symbolically and
  substituted monomorphically.
- No exceptions, no unwinding, no `catch`. `panic` traps (see D14).
- No incremental compilation, no hot reload, no self-hosting bootstrap. The reference
  implementation stays in Rust.
- No `unsafe` keyword. With no borrow checker there is no checker to switch off; the pointer
  type system already carries the guarantees we are willing to make.

---

## 2. Toolchain (measured on the dev box, 2026-11-23)

| Component | Status | Notes |
|---|---|---|
| LLVM | **23.1.5** + **`llvm-devel` installed** | `libLLVM-23.so`, static libs, headers, `cmake` files in `/usr/lib64/llvm23` |
| clang | **23.1.5** + **`clang-devel` installed** | `libclang.so.23.1` + `/usr/include/clang-c/*.h` → libclang C API usable |
| lld | **23.1.5 installed** | available for cross / wasm linking later |
| Rust | 1.100.0 / cargo 1.100.0 | implementation language |
| `inkwell` | **0.11.0 publishes an `llvm23-1` feature set** | the LLVM-23 binding we want |
| `llvm-sys` | **231.0.2** (LLVM 23) | inkwell's backend; exact major match with the installed LLVM |
| crates access | sparse index + `static.crates.io` reachable; `crates.io` REST API returns 403 from this box | `cargo fetch`/`build` work, `cargo search` does not |
| odin, zig | not installed | nothing to A/B against locally |

Consequences, all decided:

- **`[Q09]` → link libLLVM.** `dux_codegen` uses `inkwell` with feature `llvm23-1` (which
  pulls `llvm-sys 231`). Pass manager, `TargetMachine`, `DIBuilder` and static-asm are then
  available, and `ADMIN.md` requests are satisfied. Textual IR survives only as an
  `--emit=ir` *debug output*, not as a build strategy.
- **`[Q10]` → libclang.** `dux_cinterop` drives the libclang C API (`clang-sys`,
  `CXIndex`/`CXTranslationUnit`/`CXCursor`) and reads size/align/offset/bitfield metadata out
  of clang rather than re-deriving layout. `clang -Xclang -ast-dump=json` stays useful as a
  golden-file input for unit tests of the `CDecl` layer.
- `llvm-config` resolves on `PATH` (`/usr/bin/llvm-config` → 23.1.5), so `llvm-sys`'s build
  script finds the right LLVM; `LLVM_SYS_231_PREFIX` is the escape hatch.

---

## 3. Front end

### 3.1 Source model

- File extension `.dux`, one package per directory, `package name` header (no terminator).
- `import "std/mem"` — quoted module path, resolved against the module root, then the
  stdlib root. Import binds the last path segment as a namespace (`mem.Arena`).
- Entry point: `fn main()`. `fn main() -> i32` is also accepted. A freestanding `_start`
  mode is a post-v1 concern. `[OPEN-11]`
- UTF-8 source, LF or CRLF. Identifiers are ASCII alnum + `_` in v1. `[OPEN-12]`
- **Unsupported ≠ invalid.** `E0007` means "valid Dux that this build cannot compile yet";
  it is always a milestone gap with a note, never a verdict on the syntax. Syntax errors are
  `E0005`/`E0006`/`E0008`. `duxc check example/hello.dux` prints `no errors: …` while a file that
  only needs M4 features prints `E0007` for each unlowered construct.
- **Prelude.** `std/prelude` is imported implicitly into every file: `Option`, `Result`, `Vec`,
  `String`, `HashMap` and the inherent methods on primitives (`len`, `push`, `unwrap`, …). It is
  ordinary Dux source, not compiler magic, apart from the two predeclared enums in §4.5.
- **No significant whitespace, and no required semicolons.** A statement ends at a newline, at
  `;`, or at the enclosing `}`; `;` is accepted and is *required* to separate clauses that share a
  line (a `for` header, two statements on one line). The lexer records only "a newline preceded this
  token"; the parser decides at statement boundaries, so an expression may always break after an
  operator (`x := 1 +` / newline / `2` is one statement). `,` separates `for` clauses and `match`
  arms; `,` or newline separates fields inside `{}` bodies.

### 3.2 Surface syntax — normative

Every construct below appears in a file in [`example/`](example/README.md), which is the
review artifact. `example/fizzbuzz.dux` is the owner's own anchor sample.

```dux
package main

struct Vec3 { x: f32, y: f32, z: f32 }

enum Shape {                                   // one keyword, payloads per variant
    Circle(f32),
    Rect { w: f32, h: f32 },
    Empty,
}

fn area(s: Shape) -> f32 {                     // single return value
    return match s {
        Shape::Circle(r)     => 3.14159265 * r * r,
        Shape::Rect { w, h } => w * h,
        Shape::Empty         => 0.0,
    }
}

fn load(name: string) -> Result<Shape, string> { // parameterised error, no global error set
    match parse(name) {
        Ok(s)  => return Ok(s),
        Err(e) => return Err($"cannot load {name}: {e}"),   // no `?` sugar yet: match + return
    }
}

fn main() {
    arena := mem.Arena.new(64 * 1024)
    defer arena.destroy()
    context = arena.ctx()                      // hidden allocator parameter, Odin-style
    println($"area={}", area(Shape::Circle(1.0)))
}
```

Summary of the surface (details and rationale in `example/README.md`):

| area | spelling |
|---|---|
| declaration | `fn name(a: T) -> R { }`, `struct S { }`, `enum E { A, B(T) }`, `type A = B`, `const` (compile time), `global` (storage) |
| binding | `x := e` inferred · `x: T = e` explicit · `x: T` zero-initialised · `x = e` assigns (no `let`, no `mut`: bindings are mutable) |
| generics | `fn f<T>`, `struct P<A, B>`, `enum Either<L, R>`, `fn zeros<const N: usize>()`, call `zeros<4>()` |
| methods | `impl S { fn m(self) -> R, fn set(self: *S, …) }`, call site auto-`&` for pointer receivers |
| pointers | `&x` address-of, `*p` load/store, `p.field` auto-deref, `p + 1` arithmetic, `rawptr` untyped |
| optionality | `*T` never null; `Option<*T>` is the only nullable pointer; `None` is the only null |
| patterns | condition position takes a pattern: `if Some(x) := opt { }`, `while Some(node) := cursor { }` — runs on match, binds the payload, else falls through |
| aggregates | `[T; N]` array (value), `[]T` slice `{ ptr, len }`, `&arr`, `&arr[a..b]`, `(A, B)` tuple |
| control flow | `if`/`else if`, `while`, `for init, cond, step`, `for v in xs`, `for i, v in xs`, `for _ in 0..n`, `for _ in 0..=n`, `loop`, `break`, `continue`, `defer` |
| expressions | `match` and `if … else` are expressions in value position, statements elsewhere |
| sum types | `match v { E::A => …, E::B(x) => …, 1..=3 => …, x if x < 0 => …, _ => … }` |
| strings | `"…"`, `r"…"`, `c"…"` (NUL-terminated), `$"…{}…{name}…"` interpolation, `+` allocates |
| conversion | implicit iff lossless; otherwise `expr as T`, always written |
| attributes | `#[repr(C)]`, `#[no_context]`, `#[cc("stdcall")]`, `#[link(name = "m")]`, `#[test]` |
| C import | `#[c_include("h")] #[c_link("…")] #[c_flag("…")] mod ns;` plus `extern "C" { … }` |
| allocation | `new(T)`, `make(T, n)`, `free(p)`, `del(s)`, `Vec<T>`/`HashMap<K, V>` in stdlib |
| failure | `panic("…")` → print + trap; `Option::unwrap`, `Result::unwrap` panic |

Deliberate deviations from Rust, so nobody is surprised later: no `&`/`&mut` reference types
(one non-null `*T`), no unsized `[T]` (`[]T` is a value), no `impl Trait`/generics-on-traits,
no `Drop` (use `defer`), no `unsafe`, no `match` ergonomics that require deref patterns.

Deferred, additive, and *not* to be invented by the compiler: error sugar (`?` / `or else` /
`or_return`), integer-backed `enum` with discriminants for C enums and bitflags, indexed
format holes `{0}`, format specs (`{:>8.2}`), `pub` visibility, bit fields, C `union`.

### 3.3 Grammar sketch (EBNF-lite, v1 surface)

```
file        := "package" Ident (import | decl)*
import      := "import" STRING
decl        := fn | struct | enum | type_alias | const | static | impl | extern_block | c_mod
c_mod       := attr+ "mod" Ident ";"
attr        := "#" "[" Ident ("(" args ")")? "]"
fn          := attr* "fn" Ident ("<" params ">")? "(" params ")" ("->" type)? block
impl        := "impl" type "{" fn+ "}"
struct      := "struct" Ident ("<" idents ">")? "{" (Ident ":" type ",")* "}"
enum        := "enum" Ident ("<" idents ">")? "{" variant ("," variant)* "}"
variant     := Ident ("(" type ("," type)* ")" | "{" fields "}")?
binding     := Ident ":=" expr | Ident ":" type ("=" expr)? | "global" Ident ":" type "=" expr | "const" Ident ":" type "=" expr
cond        := expr | pattern ":=" expr              // if / while accept a pattern condition
type        := "*" type | "[]" type | "[" type ";" expr "]" | Ident ("<" types ">")? | "(" types ")" | "fn" sig
stmt        := binding | expr | "return" expr? | "defer" stmt | "break" | "continue" | block | attr_item
for         := "for" (c_for | iter | iter_indexed)? block
c_for       := binding "," expr "," expr            // for i := 1, i <= 100, i += 1
iter        := pattern "in" expr                     // for v in xs
iter_index  := Ident "," pattern "in" expr           // for i, v in xs
iter_range  := pattern "in" expr (".." | "..=") expr // for _ in 0..n
match       := "match" expr "{" (pattern ("if" expr)? "=>" (expr | block)) ","* "}"
pattern     := Ident ("::" Ident ("(" idents ")" | "{" fields "}")?)? | "_" | literal | range | ".."
```

### 3.4 Error tolerance

The parser never panics and always produces an AST containing `Error` nodes, so `dux check`
and the LSP report more than one error per file. Every node carries a `Span` (32-bit `FileId`
+ 32-bit packed offset/len). Parsing is one-pass recursive descent, arena-allocated, no
backtracking beyond a two-token `peek`. Immutable AST in arenas is a paid-for consequence of
**LSP being a stated goal** (D23): hover/goto/diagnostics need re-queryable nodes.

---

## 4. Type system

### 4.1 Primitives

| Dux | LLVM | Size | Notes |
|---|---|---|---|
| `bool` | `i1` (`i8` at ABI edges) | 1 | invalid states are UB |
| `u8 u16 u32 u64 u128` | same | 1/2/4/8/16 | |
| `i8 i16 i32 i64 i128` | same | ″ | |
| `usize isize` | `i64`/`i32` per target | ptr | |
| `int uint` | alias of `i32`/`u32` | 4 | **decided: `int` = `i32`**, matches C `int` and LLVM expectations |
| `f16 f32 f64 f128` | `half float double fp128` | 2/4/8/16 | C `long double` is `c_long_double`, not `f128` |
| `rune` | `i32` | 4 | distinct Unicode code point type |
| `rawptr` | `ptr` | ptr | opaque, no arithmetic until cast |
| `string` | `{ ptr, len }` | 16 | immutable UTF-8 view, no NUL |
| `never` | `void` | 0 | bottom type: `panic`, `return`, `loop` without break |

- **Conversions (D17).** Implicit exactly when the destination's value set is a superset of
  the source's: `i8→i16/i32/i64`, `u16→i32`, `u32→i64`, `f32→f64`, `u32→usize` on LP64, etc.
  Never implicit: signed→unsigned (negatives), narrowing, and *anything* to or from a `c_*`
  type or a distinct newtype — `c_long → i64` is written `x as i64` even when both are 8 bytes,
  because that is where LLP64 and 32-bit `time_t` corruption hides.
- Integer literals are untyped constants that unify with any integer type whose range holds
  them, then default to `i32` (float literals default to `f64`). Constants keep their
  untyped-ness through arithmetic, so `KI + 1` adapts to `usize`.
- Character literals are untyped integer constants too (`'0'` adapts to `u8` when compared to
  a byte). `[OPEN-13]` confirm `rune` vs `u8` default.
- **Overflow:** trap in debug (`dux run`, default), wrap in release (`--release`), flag
  `-overflow=check|wrap` to override. No separate operator set (`+%` etc.).

### 4.2 Aggregates

- `[T; N]` fixed array: value type, copy is a memcpy, no implicit decay to pointer; coerces to
  `[]T` when passed where a slice is expected.
- `[]T` slice: `{ data: *T, len: usize }`, 16 bytes, no capacity. `&arr`, `&arr[a..b]` and
  `vec.slice()` produce one. Bounds checks in debug builds only (`-bounds-check=on|off|auto`).
- `Vec<T>`, `String`, `HashMap<K, V>` are **stdlib** types written against `context`, not
  built-in syntax. (Reverses the earlier `[dynamic]T` / `map[K]V` default; see D13.)
- `struct` is nominal by declaration; `type Alias = T` is structural and creates no new type.
  No `distinct` keyword — Rust-style nominality made it redundant.
- `(A, B)` tuples: anonymous, positional access `t.0`, and `-> (A, B)` keeps "one value in,
  one value out" while still returning several things.
- Empty struct is zero-sized; recursion must pass through `*T` or `Option<*T>`, size cycles are
  a hard error.
- Layout attributes: `#[repr(C)]` (required for any struct handed to C), `#[repr(align(n))]`,
  `#[repr(packed)]`. Bit fields: `[OPEN-14]` — needed for C fidelity, syntax unspelled; when
  added, layout is copied from clang metadata, never re-derived.

### 4.3 Pointers, null, optionality

- `*T`: non-null pointer to one `T`. `&x` makes one, `*p` reads/writes, `p.field`
  auto-dereferences, `p + n` arithmetic is allowed (game dev + no borrow checker ⇒ no
  `unsafe` theatre).
- **`Option<*T>` is the only nullable pointer and `None` is the only null literal.** There is
  no `nil`, no null comparison, and no pointer truthiness. Because `None` must be representable
  as an invalid address, the niche optimisation is **not** optional: `Option<*T>` lowers to a
  single pointer word, tag encoded in the reserved address space (`0`, and the all-ones
  pattern). This also covers `Option<enum>` where the variant count fits the spare tag bits.
- `*T` where `T` is `rawptr`-ish or opaque stays 8 bytes; `rawptr` needs an explicit `as *T`.
- `[]T`/`string` with a null `data` and `len == 0` is a legal empty view; that is not null.

### 4.4 Sum types (the flagship)

```
enum Shape { Circle(f32), Rect { w: f32, h: f32 }, Empty }
```

- Layout: `{ tag: T, payload: align(N) [max_size]byte }`, tag defaults to `u16`, overridable
  with `#[repr(u8)]` / `#[repr(i32)]`. `[OPEN-2]` default width vs smallest-fitting inference.
- Variant indices are declaration order, stable per source file.
- `match` must be exhaustive; `_ =>` or an `else =>` tail makes it total. Non-exhaustive
  without a wildcard is an error (`E0133`), demotable to a warning by flag only.
- Pattern conditions — `if P := e { … }`, `while P := e { … }` — desugar to `match` with a
  non-matching fallthrough arm. They are in v1 because `Option` is the only null, so single
  variant probing is the most common shape in the language. (Owner's syntax, round 2.)
- Checked / unchecked variant access: `v.to(Circle) -> Option<f32>` and
  `unsafe`-free but documented `v.as(Circle) -> f32` (wrong tag = trap). `[OPEN-3]` naming.
- Zero-payload variants do not get a niche of their own (except the `Option`/`Result`
  specialisations), so `enum Shape` above is tag + max payload.
- Nominal identity: two enums with identical variants are different types.
- 256-variant cap under `#[repr(u8)]` is a hard error, never silent widening.
- Payload-less integer-backed `enum` (`enum Flags { Read = 1 }`, C-enum / bitflags base) is
  **deferred by instruction** `[OPEN-4]`. C enums in the meantime import as "distinct integer
  type of clang's chosen width + named constants" (D20).

### 4.5 `Option<T>` and `Result<T, E>`

```
enum Option<T> { Some(T), None }
enum Result<T, E> { Ok(T), Err(E) }
```

- Predeclared in universe scope as *ordinary* enums: same layout rules, same `match` rules, no
  codegen special case beyond the niche in §4.3. `Vec`, `String` and `HashMap` arrive via the
  prelude (§3.1) and are plain stdlib code.
- **No error sugar in v1** (D11): propagation is `match … { Err(e) => return Err(…), … }`, and
  `return` type-checks as `never` so it fits an arm. `?`, `or else`, `or_return`, `or_break`,
  `or_continue` are a deliberate later, additive step — `[OPEN-5]` which spelling, which
  milestone.
- `Result<T, E>` may carry `E = ()` for fallible-but-stateless operations; no global error set
  exists, so a function that cannot fail never lies about its return type.
- Unused `Result` / `Option` result = **warning** by default, error under `-D unused-result`.
- `unwrap()` / `expect(msg)` panic. `unwrap_or`, `unwrap_or_else`, `map`, `map_err`,
  `and_then` are stdlib methods on the predeclared enums (no traits needed: they are
  monomorphic-per-instantiation inherent impls).

### 4.6 Generics

- Monomorphisation only. Type-check the body once against symbolic parameters, instantiate on
  demand per distinct argument list, cache per instantiation, never codegen an uninstantiated
  generic.
- Syntax `f<T>`, `struct P<A, B>`, `enum Either<L, R>`, call-site inference with explicit
  override (`max<i64>(9, 4)`).
- `const N: usize` parameters are the only value parameters (array sizes, capacities). That is
  the whole comptime story: no type-level computation, no metaprogramming.
- No constraints. An operation that is invalid for a substitution is diagnosed at the
  instantiation site with a "in instantiation of `max<T>` with `T = string`" note. The
  `docs/diagnostics/` corpus is where those messages get pinned down.

### 4.7 Functions, methods, contexts

- `fn name(a: T, b: U) -> R`; no overloading, no default arguments; `_` parameter means unused.
- Methods in `impl` blocks. Receivers: `self` (by value copy) and `self: *T` (pointer). A call
  on an addressable mutable binding auto-takes the address; calling a pointer receiver on a
  temporary is an error. `[OPEN-6]` receiver spelling (`self: *T` vs Rust's `&mut self` vs
  Odin's `^T`).
- Function pointer type: `fn(i32) -> i32`, obtained with `&f`. No closures/captures in v1: a
  captured environment is a heap allocation with a lifetime problem, and we have the lifetime
  problem without the capture sugar. `[OPEN-7]` revisit as "struct with data pointer".
- **Implicit context (D10).** Every `fn` that (transitively) allocates gets a hidden trailing
  parameter `context: *Context`, where `Context { allocator, location, user }`. `context` is an
  ordinary rebindable name; rebinding is lexically scoped to the enclosing block, and the
  compiler restores the outer value on exit. Functions that never allocate carry no context, so
  they are already ABI-identical to C.
- `#[no_context]` forces the hidden parameter away for anything passed to C as a callback. It
  is an error to allocate in a `#[no_context]` function. This is how the
  context-vs-C-ABI tension (the old `[Q47]` risk) is resolved: opt out at the boundary rather
  than pass up the ergonomic win.
- Calling convention defaults to C. `#[cc("stdcall")]`, `#[cc("thiscall")]` etc. for the Win32
  stragglers.

### 4.8 Memory

- `new(T) -> *T`, `make(T, n) -> []T` (zero-initialised), `free(p)`, `del(s)`, all reading
  `context.allocator`. No `Drop`, no RAII: `defer free(p)` is the idiom and it is checked for
  the cheap cases (allocation with no `free`/`del`/escape in the same block = warning).
- Stdlib allocators: bump/arena (`mem.Arena`), frame allocator, and a **debug allocator**
  (guard pages, double-free detection, per-`dux test` leak table) — enabled by default under
  `dux run`/`dux test`, off in `--release`.
- Slices dangle exactly as much as the user deserves; documented, unchecked. Opt-in
  `-san=address` (LLVM ASan instrumentation) is post-v1. `[OPEN-8]`
- Value semantics everywhere; copying an aggregate is a bitwise copy of its bytes.

---

## 5. C interop (headline feature)

Goal: compile `example/c_interop.dux` and nothing else — no generated files, no bindings
crate, no build step the user writes.

```dux
package main

import "std/c"

#[c_include("SDL2/SDL.h")]
#[c_pkgconfig("sdl2")]
mod sdl;

fn main() {
    match sdl.SDL_Init(sdl.SDL_INIT_VIDEO) {            // c_int; the literal adapts to it
        0 => println("SDL ready"),
        _ => println($"SDL_Init failed: {}", c.to_string_lossy(sdl.SDL_GetError())),
    }
}
```

### 5.1 Mechanism

1. `#[c_include("path")] mod ns;` records a *lazy* import bound to a namespace. Nothing is
   parsed at the declaration site.
2. On the first use of a name under `ns`, the driver builds a synthetic translation unit —
   `#include "path"` plus that namespace's accumulated `#[c_flag]` / `#[c_define]` /
   `#[c_include]` entries — and runs **libclang** over it once per (header-set, flags) pair,
   caching the result in `.dux/cache/cimport/<hash>.bin`.
3. Cursor walk (`CXCursor_inclusion` → `FunctionDecl`, `StructDecl`, `UnionDecl`,
   `EnumDecl`, `Typedef`, `Var`, macro through `clang_getPreprocessingResult`) normalises into
   an internal `CDecl` IR: `CFun`, `CTypedef`, `CStruct`, `CUnion`, `CEnum`, `CMacro`, `CVar`.
   **Layout is read, never re-derived**: `clang_Type_getSizeOf`, `alignOf`, `offsetOf`,
   `numFields`, bitfield widths, packed-ness, enum integer type.
4. `CDecl` → Dux decls via §5.2. Names are suffixed (`new` → `new_`) only on keyword clash.
5. Unused decls are never emitted (dead-decl elimination = "don't generate it").
6. `static inline` / header-only libraries: the translation unit is also compiled with
   `clang -c -emit-llvm` into a cached bitcode archive and linked in, so `static inline`
   functions and single-implementation-header libraries (stb, cglm, cxx) are callable
   (D19). This is the difference between "painless interop" and "half the ecosystem is
   unreachable".

### 5.2 Mapping table

| C | Dux | Note |
|---|---|---|
| `int8_t`, `uint64_t` | `i8`, `u64` | exact-width types map straight through |
| `int`, `short`, `long`, `long long` | `c_int`, `c_short`, `c_long`, `c_long_long` | **distinct types** (D18): on LLP64 `c_long` ≠ `c_int`; `as` is required to cross into `i32`/`i64` |
| `unsigned` variants | `c_uint`, `c_ulong`, … | same rule |
| `size_t`, `ssize_t`, `ptrdiff_t` | `usize`, `isize`, `isize` | |
| `char` | `c_char` | signedness taken from clang's target info |
| `unsigned char` | `c_uchar` | |
| `_Bool` | `c_bool` (1 byte) | distinct from Dux `bool`, which is `i1` internally |
| `float`, `double`, `long double` | `f32`, `f64`, `c_long_double` (`x86_fp80` on x86-64) | `c_long_double` is a distinct 16-byte newtype, not `f128` |
| `void *` | `rawptr` | |
| `T *` (param) | `Option<*T>` | accepts `&x`, a slice's data pointer, `c"…"`, `string`, or `None` |
| `T *` (return) | `Option<*T>` | NULL means "no value" far more often than it means "crash now" |
| `const char *` (param) | `Option<*c_char>`, with implicit `string → c*` at the boundary | conversion helper `std/c.to_cstr(context, s)` for the owned case |
| `char *` (return) | `Option<*c_char>` + `std/c.to_string(Option<*c_char>) -> Option<string>` | we do not guess the length or the ownership; `to_string_lossy` for the don't-care case |
| `typedef` of an integer type (`time_t`, `off_t`) | distinct integer newtype of clang's underlying width | `c_time_t` is not `i64` even when both are 8 bytes: a 32-bit `time_t` build must not type-check like a 64-bit one |
| `struct S { … }` | `#[repr(C)] struct S` with **layout copied from clang** | bitfields, anonymous members inlined, packing all from metadata |
| `union U { … }` | `#[c_union] union U` | C overlapping members need a spelling `[OPEN-9]` |
| `enum E` | distinct integer newtype of clang's width + named constants | C enums are not type-safe; we do not pretend otherwise |
| `#define MAX 42` | `MAX: <inferred constant type>` | integer / float / string / simple arithmetic macros; the rest are skipped and listed by `dux cimports` |
| `#define f(x) …` | not imported | reported by `dux cimports --macros`; `-macros=expand` (per-call `clang -E`) stays experimental |
| `static inline` | imported, via the cached bitcode archive (D19) | |
| `typedef void (*cb)(void *, int)` | `fn(*rawptr, c_int)` | usable with `#[no_context]` procs only |
| `extern` variables | `extern "C"`-style global in `ns` | |
| opaque `typedef struct S S;` | `struct S` with unknown layout, usable only behind `Option<*S>` | `sizeof` is a compile error, by design |
| `__attribute__((visibility/dllimport))` | honoured | other attributes ignored, `[OPEN-10]` |
| `#pragma pack` | honoured via clang-reported packing | |
| varargs `printf(fmt, …)` | callable directly, `c"…"` literal as format | no type checking of varargs, as in C |

### 5.3 Build flags

```dux
#[c_include("GL/gl.h")]
#[c_link("GL")]
#[c_flag("-DGL_GLEXT_PROTOTYPES")]
#[c_define("GL_VERSION_4_6", "1")]
#[c_pkgconfig("gl")]
mod gl;
```

- Both per-namespace attributes **and** `dux.mod` `[cdeps]` with pkg-config, so a project can
  declare C dependencies once and get reproducible builds.
- Include search: system defaults, `C_INCLUDE_PATH`, `-I` on the CLI, `[cdeps]` cflags.
- `-l`/`-L`/`--link-arg` pass through to the final link step.

### 5.4 Escape hatches (because real headers are hostile)

- `#[c_filter("glm_*", "glm_vec*")]` / `#[c_exclude("*_deprecated")]` so importing
  `windows.h` does not inject 10 000 decls into the namespace.
- `#[c_name("real_c_symbol")]` on a hand-written `extern "C"` decl: the 2% the importer gets
  wrong is fixed **in place**, in the same file, with no bindings file to maintain. This is the
  pressure valve that makes "no bindings" survivable.
- Hand-written path is first class:
  ```dux
  #[link(name = "m")]
  extern "C" {
      fn fma(x: c_double, y: c_double, z: c_double) -> c_double
  }
  ```
- `dux cimports <file> [--json]` dumps exactly what the importer saw, per namespace, including
  everything skipped and why.
- Hostile-header corpus in CI: `pthread.h`, `time.h`, `OpenGL/gl.h`, `SDL2/SDL.h`, a
  bitfield-heavy header, an anonymous-union header, and a generated `windows.h`-shaped monster.

---

## 6. Back end

### 6.1 Pipeline

```
dux run x.dux
  ├─ locate & read source                       (driver)
  ├─ lex → parse → AST                          (error tolerant, arena)
  ├─ resolve (names, imports, def order)        → HIR
  ├─ type check + const eval + context analysis → typed HIR
  ├─ lazy C import (libclang → CDecl → decls)
  ├─ monomorphise generics                      → instantiated bodies
  ├─ lower to LLVM IR                           (dux_codegen via inkwell, llvm23-1)
  ├─ new pass manager, -O0..-O3
  ├─ TargetMachine → .o (PIC per target)
  ├─ link: clang *.o <syslibs> (CRT/crtbegin ordering for free)   [OPEN-15]
  └─ exec + capture exit code
```

`dux check` = everything up to typed HIR (fast; the LSP's backend). v1 verbs:
`run build check test fmt cimports version lsp` (LSP ships when the front end is stable,
`dux doc`/`bench`/`repl` are not v1).

### 6.2 Codegen rules

- Opaque pointers throughout, typed at the Dux level (`Context::ptr_type()`; inkwell's
  `IntType::ptr_type` is deprecated).
- **Verified against inkwell 0.11.0 / LLVM 23.1.5 on this box** (scratch spike: module →
  `default<O2>` → `.o` → `clang` link → correct stdout). Facts worth not re-learning:
  - Optimiser entry point is `Module::run_passes("default<O2>", &tm, PassBuilderOptions)`, the
    textual new-PM pipeline. The legacy `PassManager` per-pass helpers are gone in LLVM 23, so
    `-O` levels map to pipeline strings, not to Rust method calls.
  - The `TargetMachine` has to exist before passes run, and
    `tm.set_data_layout(&module)` must be called explicitly — a module's own data layout is
    empty until then, which would silently mis-layout `Option<T>`.
  - **Emit `main` as `i32 main()` returning 0.** A `void` main leaves the previous call's
    return value in `rax`: the spike's hello exited 34 (the byte count `puts` reported).
  - DWARF comes from `module.create_debug_info_builder(...)`, which takes the full CU
    description; `DWARFSourceLanguage::C99` for C-interop frames, `DWARFEmissionKind::LineTablesOnly`
    for the early milestone.
- Aggregates pass by value and LLVM legalises per the target ABI (no hand-rolled `CCState`).
- `bool` is `i1` in locals, `i8` in any C-ABI-visible signature, with truncation at the edge.
- Sum-type lowering: `{ tag, padding, payload }` struct; `match` → `switch` on the tag with a
  jump table; payloads via `gep` + `bitcast`. `Option<*T>` and spare-bits `Option<enum>` use
  the niche representation (§4.3), so `Option<*T>` is one word and `Result<*T, E>` stays
  pointer-sized when `E` fits the spare bits.
- Slices are `{ ptr, len }`; bounds checks only in debug builds.
- Context parameter: appended as the last LLVM parameter, `ptr`, non-null; `#[no_context]`
  omits it. Calls to context-carrying functions pass `context.current`.
- Mangling: `pkg$name`, generic instantiations get `$gen$<hash>`; `main` keeps the symbol
  `main`.
- Debug info: line tables + subroutine DIEs from M2 (needed to debug the compiler itself),
  full type DIEs for user types by M5, `DW_LANG_C99` for imported C types so lldb prints
  them correctly inside C frames. `[OPEN-16]` exact milestone split.
- `-emit=ir` prints textual IR (debug aid, not a build mode), `-emit=obj`, `-emit=asm`.

### 6.3 Targets

- v1: `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`.
- `--target=<triple>` exists from day one so the plumbing is never retrofitted;
  `x86_64-w64-mingw32` and `wasm32-wasi` (via the now-installed `lld`) are post-v1.
- Final link is delegated to `clang --target=…` whenever clang is present — correct CRT
  ordering for free `[OPEN-15]`.

---

## 7. Tooling UX

```
dux run <file|dir> [--args…]        compile + execute
dux build [-o out] [--release]      compile
dux check <dir|file>                type check only (LSP backend)
dux test [-v]                       run #[test] functions, exit≠0 on failure, leak report
dux fmt [paths]                     canonical formatter (formatter = AST printer)
dux cimports <file> [--json]        what did the C importer see?
dux lsp                             language server (hover, goto-def, diagnostics)
dux version | --target=<triple> | -I | -L | -l | -D | -D unused-result
```

- Manifest `dux.mod`, TOML: `[package] name, version, dux = "0.1"`, `[deps]`, `[cdeps]`.
  Single-file programs need no manifest. `[OPEN-17]`
- Diagnostics are machine-parseable and stable:
  `src/app.dux:12:9: error[E0133]: match is not exhaustive: variant \`Shape::Polygon\` is not handled`
  with a source snippet and caret spans (`miette` + `codespan-reporting`).
- Every diagnostic code gets a file in `docs/diagnostics/E0133.md`, which doubles as the
  language spec by example and as the negative-test index.
- LSP: `initialize`, `textDocument/didOpen|didChange`, `publishDiagnostics`, `hover`,
  `definition`. The front end already pays for this (immutable arenas, tolerant parse,
  check-only mode, no mutable global checker state).

---

## 8. Testing strategy (repo requirement)

`AGENTS.md` requires `.dux` → compile → run → stdout verified, plus unit tests per crate.

### 8.1 E2E harness (`tests/e2e/`)

```
tests/e2e/
  hello.dux / hello.expected
  sum_types.dux / sum_types.expected
  result_ok.dux / result.expected
  c_import.dux / c_import.expected        + native/*.h, native/*.c compiled by the harness
```

- Each `.dux` is self-contained: no network, no `$HOME` writes, no absolute paths. Scratch output
  goes to `CARGO_TARGET_TMPDIR` (`target/tmp`), never next to the fixture.
- Directive comments inside the `.dux` select expectations (implemented in
  `crates/duxc/tests/e2e.rs`):
  - `//@exit 1` — expected exit code (default 0)
  - `//@expect-error E0100` — the build must fail and stderr must contain that code
  - `//@stderr "unused"` — stderr substring for warnings that must survive
  - `//@release` — build with `--release`, i.e. through `default<O2>`
  - `//@args a b` — arguments forwarded to the program after `--`
  stdout is compared against `<fixture>.expected`; a run fixture without one fails the gate.
- `UPDATE_EXPECTED=1 cargo test` regenerates `.expected`.
- Two permanent gates beyond the corpus: the normative `example/hello.dux` must compile, run and
  print exactly `Hello, world!\n`, and no `example/*.dux` may make the compiler hang or panic
  (checked under `timeout 20`) — that test has already paid for itself once.
- **`example/*.dux` are promoted into `tests/e2e/` as milestones land**: a file moves when the
  language covers everything it demonstrates, so the review samples become the regression
  suite. Files that are not yet fully supported stay in `example/`.
- Gate: `cargo test` = unit tests + e2e, all green before any commit (`AGENTS.md`).

### 8.1a Diagnostic codes in use

| Code | Meaning | Phase |
|---|---|---|
| `E0001` | unexpected character | lexer |
| `E0002` | unterminated string | lexer |
| `E0003` | bad escape | lexer |
| `E0004` | number out of range | lexer |
| `E0005` | unexpected token | parser |
| `E0006` | unexpected end of input | parser |
| `E0007` | syntax is valid but this build cannot compile it yet — *milestone* limitation, never a language verdict | parser/codegen |
| `E0008` | statement not terminated (`;` or newline required) | parser |
| `E0100` | no `main` | program shape |
| `E0101` | `main` has the wrong signature | program shape |
| `E0110` | name is not declared in this scope | resolver |
| `E0111` | duplicate definition (top level or parameter) | resolver |
| `E0112` | call supplies the wrong number of arguments | resolver |
| `E0200` | implicit conversion would lose information; write `as` | checker |
| `E0201` | type mismatch (argument, return, operand, arms) | checker |
| `E0202` | `bool` required — condition, `!`, `&&`, `||` — but the value is not | checker |
| `E0203` | assignment to a `const` | checker |
| `E0204` | literal or constant does not fit its target integer type | checker |
| `E0900` | link failure | driver |
| `E0901` | source/output I/O failure | driver |
| `E0902` | LLVM failure | codegen |

### 8.2 Unit tests

- Lexer: token-stream snapshots. Parser: AST S-expression snapshots.
- Ty: `check(src) -> Vec<Diagnostic>` snapshots; **every negative type rule gets a test that
  asserts its diagnostic code** — that is how §4 stops being prose and becomes executable.
- Codegen: `--emit=ir` snapshots, so lowering regressions show up as a diff.
- C interop: golden `clang -ast-dump=json` output per fixture header → `CDecl` snapshot, so
  the libclang cursor walk and the `CDecl` layer are testable independently of each other.

---

## 9. Repository layout

```
dux/
  Cargo.toml                  # workspace; LLVM version pinned in [workspace.metadata.llvm]
  DESIGN.md  todo.md  ADMIN.md  history.md
  crates/
    duxc/                     # CLI binary
    dux_source/               # SourceMap, FileId, Span
    dux_lexer/
    dux_ast/
    dux_parser/
    dux_hir/                  # resolved + typed: DefId, TypeId, TypeArena
    dux_ty/                   # unification, const eval, monomorphisation
    dux_cinterop/             # libclang reader (clang-sys) + CDecl + mapping
    dux_codegen/              # HIR → LLVM IR (the ONLY inkwell/llvm-sys user)
    dux_driver/               # pipeline, cache, linking, run, lsp
    dux_diag/                 # diagnostic ids, codes, rendering
  tests/
    e2e/                      # *.dux + *.expected + native fixtures
  stdlib/                     # std/mem, std/io, std/fmt, std/collections, std/c, …
  example/                    # owner-facing syntax samples (see example/README.md)
  docs/
    diagnostics/              # E0133.md …
    spec/                     # 01-lexing.md …
    status.md                 # which example/ file is not yet supported, and why
```

Dependency direction is strictly downward. `dux_codegen` is the only crate allowed to know
about LLVM; `dux_cinterop` is the only crate allowed to know about clang.

---

## 10. Milestones

Each milestone ends green (`cargo test` + e2e) before the next starts.

| # | Deliverable | E2E proof |
|---|---|---|
| M0 | **Done.** Workspace, `duxc version/check/build/run`, e2e harness, `inkwell llvm23-1` linked against system LLVM 23.1.5 (**binding risk retired**: verified on this box), `hello.dux` codegen'd through libLLVM and linked | `hello.dux` → `Hello, world!` |
| M1 | **Done:** lexer, parser and AST cover the whole declared surface — declarations (`const`/`global`/`struct`/`enum`/`type`/`impl`/`mod`/`extern`), attributes, expressions (struct values, paths, arrays, ranges, tuple members, function-pointer types) — with error-tolerant recovery. `--emit=ast` landed (`dux_ast::dump`, milestone gaps do not block the dump). `dux fmt` landed: the canonical formatter is an AST printer (`dux_ast::fmt`), round-trip- and idempotency-tested against the whole corpus; it refuses any file whose parse carried an error, so a milestone gap can never be silently rewritten away. Remaining: the resolver/checker, which M2 picks up | every `example/*.dux` parses (`every_example_parses_so_the_review_samples_stay_the_target_surface`) |
| M2 | **Done:** **resolver** (`dux_hir`): names, definition order, arity, duplicates, program shape. **checker** (`dux_hir::ty`): D25 conversion lattice, untyped literals that adopt and fold, `fn` argument/return contracts, conditions, `const` assignment — gated into `check`/`build`, contract-silent on the whole corpus. **Value `if`**: both arms parse as value blocks, the checker unifies them, codegen meets them at a phi (`else if` chains included). **Fixed arrays**: literals, repeated `[0; N]`, zero-init annotations, index read/write, copy-on-assign, `for .. in` over arrays (`i, v`) and ranges (`a..b`, `a..=b`). **`defer`**: eager argument capture, LIFO replay at every exit; nested-block `defer` waits for the M3 runtime defer chain. **Line-table debug info**: `DISubprogram` per function, per-statement `DILocation`, `gdb` stops on Dux source lines. Remaining gaps are deliberate milestones: tuples (`pair.0`), bounds-trap runtime, nested `defer` | `if_value.dux`, `arrays_and_loops.dux`, `defer.dux` run with stdout pinned; `hello.dux` ships `.debug_line` |
| M3 | **Done:** **records** are values — `struct` literals, nominal LLVM types (pointer equality on the info, so two `Point`s that look alike are still different types), field access, assignment that copies. **Pointers** reach machine code — `&x`, `*p`, `p.field` auto-dereferences, `p + n` is a scaled GEP, `rawptr` needs a cast before it is read or stepped. **Methods**: `impl` blocks, both receiver kinds (a value receiver copies, a pointer receiver borrows a place), the call site adds `&` when the method wants one, method symbols carry their owner so two records may share a name. **`[]T`** is `{ data, len }`: `&x` of an array is a view, `[a..b]` subslices, an array passed where a view is expected borrows its storage, `.len()` answers for both, `for v in xs` walks either. **`string`** is the same shape over bytes — literals carry their count, `+` allocates, `==` compares bytes then lengths, `for b in s` walks the bytes, `c".."` stays a bare address and `p as string` measures it. **The context**: every function that allocates — directly or through a call — carries a hidden trailing `context`, `main` creates the first one, a block that rebinds it puts the outer one back. `new`/`make`/`free`/`del` and `defer free`/`defer del` go through it. **The debug allocator** heads blocks with a magic word and a size, catches double frees, and reports what is still live at exit; `--release` swaps it for malloc/free with no bookkeeping at all. Remaining gaps belong to later milestones: `match`/`Option` (M4), generics and the stdlib that `Vec`/`mem.Arena`/`.bytes()` need (M5), guard pages and a per-block location table | `records.dux`, `methods.dux`, `pointer_basics.dux`, `slices.dux`, `heap.dux`, `make_and_del.dux`, `strings.dux`, `leaks.dux`, `double_free.dux` (must trap), `release_allocator.dux` (must say nothing) run with stdout pinned; `structs.dux`, `pointers.dux`, `strings.dux` typecheck clean and run until a later milestone |
| M4 | `enum` sum types + `match`, `Option`, `Result`, pattern conditions (`if P := e`), niche layout, exhaustiveness | `sum_types.dux`, `option_result.dux` + negative tests |
| M5 | **Done:** generics are code, not functions — a generic `fn` or `impl` is emitted once per distinct list of types a call names (`dux.max.i32`, `Vec<i32>.push`), `const N` travels as a value, and an instantiation nobody asked for is never emitted. `Vec<T>` (growing through the context) and `HashMap<K, V>` (open addressing probed by the builtin `hash`) are prelude Dux beside `Option::unwrap_or`; `std/strings` carries `less`, `starts_with`, `ends_with`, `find`, `contains`, `repeat`; `std/mem` carries `Arena` — bump allocation whose context is a Dux record whose first field is the context itself, so `context = arena.ctx()` redirects every later allocation, memory the arena cannot serve falls through to the context behind it, and one `destroy` takes the block back. **The context now dispatches through its own slots**: allocation and free read the two function words of whatever context is current, which is what a rebindable name needed to become real. A `fn` value (`cb := &doubling`, `cb(21)`) is one word and an indirect call, `&` takes the address of a `#[no_context]` function, and untyped arithmetic adopts the type a call asks for (`take(4 * 1024)`). `defer` reaches plain blocks: a block fires what it captured on the way out, a `return` replays the whole chain along its path, and a `defer` in a branch or loop body still reports E0007 honestly. Remaining gaps belong to later milestones: `.runes()`' UTF-8 decode, and `free(NULL)` is a silent no-op | `generics.dux`, `memory.dux`, `vec.dux`, `hashmap.dux`, `hash_keys.dux`, `strings_lib.dux`, `arena.dux` run with stdout pinned |
| M6 | `#[c_include]` via libclang: structs, enums, typedefs, funcs, varargs, macros, `static inline` bitcode archive, `#[c_link]`/`#[c_flag]`/`#[c_pkgconfig]`, `dux cimports` | `c_interop.dux` against harness-shipped headers |
| M7 | Full DWARF type DIEs, `-O2` pipeline, `dux test` runner + leak report, negative-test corpus | `lldb -batch` smoke; release stdout identical |
| M8 | `dux.mod`, pkg-config cdeps, `--target`, LSP diagnostics/hover/definition | cross-target e2e; LSP integration test |

Not scheduled: async, SIMD, reflection, C++, comptime interpreter, self-hosting, package
registry, hot reload.

---

## 11. Risks

1. **C interop is the moat and the trap.** Zig spends a large share of its codebase on clang
   AST compatibility. Mitigations: the internal `CDecl` layer, layout read from clang metadata
   only, `#[c_name]` in-place override, `dux cimports`, and the hostile-header corpus (§5.4).
2. **Bleeding-edge LLVM bindings.** `inkwell 0.11.0` + `llvm23-1` matches system LLVM 23.1.5
   today **and has been measured on this box** (module → `default<O2>` → `.o` → `clang` →
   correct stdout); a Fedora LLVM bump is still the risk. Mitigation: LLVM version pinned
   in workspace metadata, `llvm-sys 231` pinned, `ADMIN.md` records the pairing, and
   `dux_codegen` stays the only crate that touches LLVM so a bump is one crate's diff.
3. **Context parameter vs C ABI.** Resolved by `#[no_context]` (D10) + a checker rule that
   allocation in a `#[no_context]` fn is an error; M6 has an e2e test that passes a Dux
   callback into C.
4. **Generics without traits ⇒ opaque errors at instantiation sites.** Mitigated by
   "in instantiation of …" notes and the diagnostics corpus.
5. **Scope creep from Zig/Odin envy** (comptime, `@embed`, inline asm, allocator interfaces,
   `unsafe`). Each is a §1.1 non-goal until it has a written spec and a decision.
6. **Syntax churn after M1.** `example/` is the single review surface precisely so this dies
   now rather than at M4.

---

## 12. Decision log

| ID | Decision | Status |
|---|---|---|
| D1 | Host implementation language: Rust | decided 2026-11-23 |
| D2 | LLVM 23 (system 23.1.5), version pinned in workspace metadata | decided |
| D3 | **Link libLLVM via `inkwell` (feature `llvm23-1` / `llvm-sys 231`)**; textual IR is a debug output only | decided |
| D4 | C interop: lazy `#[c_include] mod ns;` + **libclang** reader; never binding files | decided |
| D5 | No borrow checker, no GC, no runtime, no async | decided |
| D6 | Layout of C types is read from clang, never re-derived | decided |
| D7 | `Option`/`Result` are predeclared ordinary enums in universe scope | decided |
| D8 | Rust-shaped sum types: **one `enum` keyword** with payloads; `match`; payload-less integer-backed `enum` deferred | decided (deferred part `[OPEN-4]`) |
| D9 | **Single return value**; multiple results via tuple | decided |
| D10 | **Odin-style implicit `context`** with hidden parameter; `#[no_context]` opt-out for C callbacks | decided |
| D11 | **No error sugar in v1.** `Result<T, E>` + `match`/`return`. `?` / `or else` are additive later | decided |
| D12 | Errors parameterised per function; no global error set | decided |
| D13 | `Vec<T>`/`String`/`HashMap<K, V>` are stdlib types (not built-in syntax); `[]T` slice is the only built-in container type | decided |
| D14 | `panic` = print + trap, no unwinding, defers do not run | decided (confirmed) |
| D15 | **No metaprogramming**: constant folding, `when`, `const` generic params only | decided (confirmed) |
| D16 | `*T` is never null; `Option<*T>` is the only nullable pointer ⇒ niche optimisation is mandatory from M4 | decided |
| D17 | Implicit numeric conversion iff lossless; always explicit for `c_*`/newtypes; `int` = `i32`; trap-in-debug / wrap-in-release overflow | decided |
| D18 | Distinct `c_int`/`c_long`/`c_char`/`c_long_double`/`c_bool` types kept; Dux code uses Rust-family types and converts with `as` | decided |
| D19 | `static inline` and header-only libraries supported by compiling the TU to a cached bitcode archive | decided |
| D20 | C enums import as distinct integer newtype + named constants (Dux `enum` backing deferred) | decided |
| D21 | Domain: games / game engines. Stdlib and interop test corpus follow that | decided |
| D22 | Generics are monomorphisation-only, no constraints | decided |
| D23 | LSP is a stated goal; front end is arena/immutable/error-tolerant to serve it | decided |
| D24 | No `unsafe` keyword | decided |
| D25 | E2E harness with `.expected` files gate every commit; `example/*.dux` graduate into it | decided |
| D26 | Declaration forms `x := e` / `x: T = e` / `x: T`, plain `=` assigns, **no `let`, no `mut`**; `const` + `global` at module scope | decided (owner, round 2) |
| D27 | Pattern conditions `if P := e` / `while P := e` instead of `if let` / `while let` | decided (owner, round 2) |
| D28 | Statements end at a newline, `;` or `}` — no semicolon required, none forbidden; `;` is mandatory only between clauses on one line. The lexer flags "newline preceded this token", the parser applies it at statement boundaries | decided (implemented in M0) |
| D29 | `println(s)` lowers to `puts(s)`, `print(s)` to `fputs(s, stdout)` — the compiler never appends a newline to a literal it is already emitting | decided (M0) |

---

## 13. Open items

### 13.1 Syntax, needing the owner's ruling (mirrors `example/README.md`)

1. `[OPEN-1]` Two leftovers from the owner's edit pass: (a) typed declarations normalised to
   `x: T = value` (one place read `x: T := value` — intentional?) and (b) module storage
   spelled `global hits: i32 = 0` (was `static`, deleted in review; confirm `global`).
2. `[OPEN-2]` Sum-type tag: default `u16` vs smallest fitting width.
3. `[OPEN-3]` Checked/unchecked variant access naming (`v.to(Variant)`, `v.as(Variant)`).
4. `[OPEN-4]` Integer-backed `enum` (discriminants, bitflags) — when does it come back?
5. `[OPEN-5]` Which error sugar, and in which milestone (`?` / `or else` / `or_return` / `??`).
6. `[OPEN-6]` Receiver spelling: `self: *T` vs `&mut self` vs `^T`; and auto-`&` at call sites.
7. `[OPEN-7]` Function pointer type spelling `fn(i32) -> i32` vs Odin's `proc(…)`; closures ever?
8. `[OPEN-8]` `loop { }` vs `for { }`; ranges `0..n` / `0..=n` in `for`.
9. `[OPEN-9]` C `union` spelling (`#[c_union] union U { … }`).
10. `[OPEN-10]` Interpolation: positional `{}` + scope-captured `{name}`, mixed in one literal —
    or positional holes only?

### 13.2 Language / tooling, non-blocking (defaults will be adopted)

11. `[OPEN-11]` `fn main() -> i32` and a freestanding `_start` mode.
12. `[OPEN-12]` Non-ASCII identifiers: never, or Unicode XID later?
13. `[OPEN-13]` Character literal default type: `rune` or untyped-integer-adopting (examples assume the latter).
14. `[OPEN-14]` Bit-field syntax (`x: u32 : 5`?) and `#[repr(packed)]` semantics.
15. `[OPEN-15]` Link driver: shell out to `clang` (recommended) vs drive `lld` directly now
    that `lld` is installed.
16. `[OPEN-16]` Debug-info milestone split (line tables M2, full type DIEs M5, `lldb` smoke M7).
17. `[OPEN-17]` `dux.mod` format (TOML recommended) and dependency syntax.
18. `[OPEN-18]` Visibility: is `pub` needed, or is package-per-directory enough for v1?
19. `[OPEN-19]` Test + assert spelling: `#[test] fn …`, `assert(cond)`, `debug_assert(cond)`.
20. `[OPEN-20]` Which C libraries the interop corpus must survive — proposal: SDL2, GLFW,
    cglm, stb_image, SDL2_mixer, sqlite3, zlib, pthreads, and one `windows.h`-shaped stress
    header (Windows headers via `-D` stubs, since the box is Linux).

### 13.3 Closed on instruction (do not reopen without a decision)

Comptime interpreter / metaprogramming · traits · multi-return · global error set · nullable
`*T` · textual-IR-only build · `clang -ast-dump-json`-only importer · "no LSP" · "domain
unknown" · `unsafe` blocks · exceptions/unwinding · `[dynamic]T` and `map[K]V` built-in syntax.
