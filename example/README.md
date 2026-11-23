# Syntax examples — please edit these

Each file is a syntax probe for one area of the language. Nothing compiles yet
(no compiler exists), so the fastest way to change the language is to change a
file here and tell me it is wrong. `fizzbuzz.dux` is yours and is the anchor:
`fn`, `for init, cond, step`, `println`, `$"..{}.."`.

| file | demonstrates |
|---|---|
| `hello.dux` | minimal program, universe-scope `println` |
| `types.dux` | `const`/`static`, `:=`/`let`/`mut`, `[T; N]`, tuples, implicit widening, `as` |
| `strings.dux` | `string` slices, `$"{}"` vs `$"{name}"`, `c".."`, `r".."`, bytes/runes, `+` allocating |
| `structs.dux` | `struct`, named-field literals, `impl` + receivers, auto-`&`, value copy, auto-deref |
| `sum_types.dux` | Rust-shaped `enum`, `match` expression, guards, or-patterns, ranges, `..` |
| `option_result.dux` | `Option`/`Result` as predeclared enums, per-function `E`, `if let`, `never` arms |
| `pointers.dux` | non-null `*T`, `Option<*T>` as the only nullable, `while let`, `[]T` slices, `rawptr` |
| `memory.dux` | hidden `context`, `new`/`make`/`free`/`del`, arena rebinding, `#[no_context]`, `fn` pointers |
| `control_flow.dux` | all loop forms, `loop`, ranges, `if`/`match` as expressions, `defer` order |
| `generics.dux` | `fn f<T>`, `struct P<A, B>`, `enum Either<L, R>`, `const N: usize` parameters |
| `c_interop.dux` | `#[c_include] mod ns;`, distinct `c_*` types, `T *` → `Option<*T>`, `extern "C"`, varargs |

## The rules these files assume

Decided in the last review round, written down here so a disagreement is easy to point at:

1. Domain is games/engine work; no GC, no runtime, no borrow checker, no async in v1.
2. Odin-style implicit `context` (hidden trailing parameter) carries the allocator.
3. `*T` is never null. The only null pointer is `Option<*T>`, spelled `None`.
4. Sum types are Rust-shaped: one `enum` keyword, payload per variant, `match`.
5. One return value. Multiple results travel as a tuple.
6. Errors are `Result<T, E>` with a per-function `E`. No global error set.
7. No error sugar yet — propagation is `match` + `return`. Sugar is a later, additive step.
8. `c_int`/`c_long`/`c_char`/`c_long_double` stay distinct types; Dux code uses
   `i32`/`i64`/… and converts with `as`. Header-only C libraries are compiled, not skipped.
9. `panic()` prints and traps. No unwinding, defers do not run.
10. No metaprogramming: constant folding, `when`, and `const` generic parameters only.
11. libLLVM is linked through `inkwell` (feature `llvm23-1`, matches the installed LLVM 23.1.5);
    C headers are read through libclang. Both `-devel` packages and `lld` are now installed.
12. Implicit numeric conversion exactly when the destination is lossless for the
    source (`i32 -> i64`, `u16 -> i32` free; `i64 -> i32`, `i32 -> u32` need `as`).
13. LSP is a goal, so the front end is arena-allocated, immutable and error tolerant.
14. Prelude: `Option`, `Result`, `Vec`, `String`, `HashMap` and the methods on primitives need
    no import. Everything else (`mem`, `fmt`, `math`, `fs`, `c`) is a namespace from an import.
15. `mut` / `:=` describe whether **the binding** can be reassigned. Writing through a pointer,
    or calling a pointer-receiver method on an immutable binding, is allowed either way.

## Open items — rule on these (all defaults are fine, "1-9 defaults" is a valid answer)

1. **Binding spellings.** `x := e` (mutable, inferred) / `let x = e` (immutable,
   inferred) / `let x: T = e` (immutable, explicit) / `mut x: T = e` (mutable,
   explicit). Alternative: drop `mut` and make `:=` the only declaration form.
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
7. **Deferred sugar.** `?`, `or else`, `or_return`, `??`, `if let`, `while let`:
   `if let`/`while let` are used in the examples because `Option` is the only
   null. Which of the *error* forms do you want, and in which milestone?
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
