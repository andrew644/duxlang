# Syntax examples — please edit these

Each file is a syntax probe for one area of the language. Nothing compiles yet
(no compiler exists), so the fastest way to change the language is to change a
file here and tell me it is wrong. `fizzbuzz.dux` is the anchor: `fn`,
`for init, cond, step`, `println`, `$"..{}.."`.

**Your edits are folded in everywhere** (round 2): no `let`, no `mut`, `x := e` /
`x: T = e` / `x: T`, assignment is plain `=`, and pattern conditions
(`if Some(x) := opt`, `while Some(node) := cursor`) replace `if let` / `while let`.

| file | demonstrates |
|---|---|
| `hello.dux` | minimal program, universe-scope `println` |
| `types.dux` | `const`/`global`, `:=` / `: T =` / `: T`, `[T; N]`, tuples, implicit widening, `as` |
| `strings.dux` | `string` slices, `$"{}"` vs `$"{name}"`, `c".."`, `r".."`, bytes/runes, `+` allocating |
| `structs.dux` | `struct`, named-field literals, `impl` + receivers, auto-`&`, value copy, auto-deref |
| `sum_types.dux` | Rust-shaped `enum`, `match` expression, guards, or-patterns, ranges, `..` |
| `option_result.dux` | `Option`/`Result` as predeclared enums, per-function `E`, `if P := e`, `never` arms |
| `pointers.dux` | non-null `*T`, `Option<*T>` as the only nullable, `while P := e`, `[]T` slices, `rawptr` |
| `memory.dux` | hidden `context`, `new`/`make`/`free`/`del`, arena rebinding, `#[no_context]`, `fn` pointers |
| `control_flow.dux` | all loop forms, `loop`, ranges, `if`/`match` as expressions, `defer` order |
| `generics.dux` | `fn f<T>`, `struct P<A, B>`, `enum Either<L, R>`, `const N: usize` parameters |
| `c_interop.dux` | `#[c_include] mod ns;`, distinct `c_*` types, `T *` → `Option<*T>`, `extern "C"`, varargs |

## The rules these files assume

Decided across two review rounds, written down so a disagreement is easy to point at:

1. **Bindings (your edit pass, round 2).** `x := e` infers the type, `x: T = e` states it,
   `x: T` declares zero-initialised storage, and a bare `x = e` assigns to an existing
   binding. No `let`, no `mut` — bindings are mutable, and `p.field = v` through a pointer
   is ordinary assignment. Shadowing is legal in a nested block, illegal in the same one.
2. Condition position takes a **pattern**: `if Some(x) := opt { }`, `while Some(node) := cursor { }`.
   The block runs when the pattern matches (and binds its payload); otherwise control passes
   to the `else` block or the next statement. This replaced `if let` / `while let`.
3. Domain is games/engine work; no GC, no runtime, no borrow checker, no async in v1.
4. Odin-style implicit `context` (hidden trailing parameter) carries the allocator.
5. `*T` is never null. The only null pointer is `Option<*T>`, spelled `None`.
6. Sum types are Rust-shaped: one `enum` keyword, payload per variant, `match`.
7. One return value. Multiple results travel as a tuple.
8. Errors are `Result<T, E>` with a per-function `E`. No global error set, and no error sugar
   yet: propagation is `match` + `return`.
9. `c_int`/`c_long`/`c_char`/`c_long_double` stay distinct types; Dux code uses
   `i32`/`i64`/… and converts with `as`. Header-only C libraries are compiled, not skipped.
10. `panic()` prints and traps. No unwinding, defers do not run.
11. No metaprogramming: constant folding, `when`, and `const` generic parameters only.
12. libLLVM is linked through `inkwell` (feature `llvm23-1`, matches the installed LLVM 23.1.5);
    C headers are read through libclang. Both `-devel` packages and `lld` are installed.
13. Implicit numeric conversion exactly when the destination is lossless for the
    source (`i32 -> i64`, `u16 -> i32` free; `i64 -> i32`, `i32 -> u32` need `as`).
14. LSP is a goal, so the front end is arena-allocated, immutable and error tolerant.
15. Prelude: `Option`, `Result`, `Vec`, `String`, `HashMap` and the methods on primitives need
    no import. Everything else (`mem`, `fmt`, `math`, `fs`, `c`) is a namespace from an import.
16. **Statements end at a newline.** No semicolon is needed at the end of a line, and none is
    forbidden: `;` also ends a statement, and it is *required* between clauses that share one
    line (`for i := 1, i <= 100, i += 1`). A line that ends mid-expression continues on the next
    line (`x := 1 +` newline `2` is one statement), so wrapping long expressions is fine.

## What the M0 compiler already does

`cargo run -p duxc -- run example/hello.dux` compiles `hello.dux` through LLVM 23 and runs it.
`duxc check <file>` reports lexer/parser diagnostics only. Today the compiler lowers `fn main()`
with `print`/`println` of literals; everything else is reported as `E0007` — *"not supported by
this compiler build yet"*, which is a milestone gap and never a verdict on the syntax. Parsing of
`struct`/`enum`/`const`/`global`/`impl`/`match`/loops is M1-M4 work, tracked in DESIGN.md §10.

## Open items — rule on these ("all defaults" is a valid answer)

1. **Two leftovers from your edit pass.** (a) One typed declaration was written
   `mask: u64 := 0xff00` while the rest are `n: i64 = 4`; I normalised every typed
   declaration to `: T = value` and left `:=` for inference only — say the word if
   `: T :=` was intentional. (b) You deleted `static hits: i32 = 0`; module-level
   mutable storage now reads `global hits: i32 = 0` (Odin's keyword). Confirm
   `global`, or name it, or tell me globals are not a thing.
2. **Methods.** Rust `impl` blocks with `self` (copy) and `self: *T` (pointer
   receiver, call site auto-`&`). Alternatives: Odin's `fn (v: *Vec3) scale(…)`
   free functions, or Go's `fn (v *Vec3) scale(…)`.
3. **`&x` / `*p`.** Address-of and dereference read like C/Rust. Odin's `x^`
   postfix deref is the alternative. Also: no `unsafe` keyword anywhere — agree?
4. **Slices vs arrays.** `[T; N]` fixed arrays (Rust), `[]T` = `{ ptr, len }`
   value (Odin's `[^]T`), `&arr` and `&arr[1..3]` produce `[]T`. Note this
   differs from Rust, where `[T]` is unsized and `&[T]` is the fat pointer.
5. **`Vec<T>` / `HashMap<K, V>` as stdlib types** (with `context`), replacing the
   earlier `[dynamic]T` / `map[K]V` built-in-syntax default. Constructor
   `Vec<string>.new()` is clunky — candidates: `Vec::<string>.new()`,
   `new_vec(string)`, or make `new` a keyword operator: `new Vec<string>()`.
6. **Function pointer type** `fn(i32) -> i32` and taking one with `&f`.
   Alternative: keep Odin's `proc(i32) -> i32` for the *type*, since `fn` now
   declares functions.
7. **Error sugar, deferred.** `?`, `or else`, `or_return`, `??` — none of them are
   in the examples; propagation is `match` + `return`. Which form, and which
   milestone? (Pattern conditions already cover the single-variant case.)
8. **`loop { }` vs Odin's `for { }`**, and `0..n` / `0..=n` ranges in `for`.
9. **String interpolation.** `{}` = next trailing argument, `{name}` = read from
   scope, mixing allowed within one literal. Alternative: holes only, no capture.
10. **Integer-backed `enum`** (`enum Flags { Read = 1, Write = 2 }`, usable as a
    C enum / bitflags base) is deferred by your instruction. It stays deferred —
    but `c_interop.dux` needs *something* for C enums, so confirm: C enums import
    as "distinct integer type + named constants", and the Dux-side `enum`
    decision comes back later.

## Still missing from the surface (not decided, not invented here)

- Visibility (`pub`?), package/module layout beyond `package main` + `import "…"`.
- `dux test` attribute (`#[test]`) and `#assert`/`assert` spelling.
- Bit fields, `#[repr(C)]` / `#[repr(packed)]`, explicit alignment.
- `union` (C's overlapping members) — needed for C interop fidelity, currently unspelled.
- Char/escape rules for literals, `{}` vs `{0}` indexed holes, format specs (`{:>8.2}`).
