# Compiler status per example

Which `example/*.dux` files the current build compiles and runs, and what
each remaining gap is waiting for. A gap is a milestone, never a verdict.

| File | Status | Waits on |
|---|---|---|
| `hello.dux` | compiles, runs, ships `.debug_line` | — |
| `control_flow.dux` | compiles, runs; `match` in value position reads ranges, wildcards and guards | — |
| `types.dux` | parses; fixed arrays now lower | tuples `pair.0` (M3) |
| `structs.dux` | checks clean; `math.sqrt` and an expression body still run | `math` package (M5), expression bodies |
| `pointers.dux` | compiles, runs; `Option<*Node>` is one word and the list walk is a pattern condition | — |
| `memory.dux` | parses | `Vec<string>` on a call path | generics (M5), `mem.Arena` |
| `strings.dux` | checks clean; runs until `.bytes()` | `string` methods (`.bytes()`, `.runes()`) | the standard library (M5) |
| `sum_types.dux` | compiles, runs; variants, payloads, guards, or-patterns, ranges and exhaustiveness all reach machine code | — |
| `option_result.dux` | checks until `fs.read_to_string`; the predeclared `Option`/`Result` lower and match today | `std/fs`, `.bytes()`, `.trim()`, and the `Option` methods (`unwrap`, `unwrap_or`), which are prelude code | the standard library (M5) |
| `generics.dux` | parses | monomorphisation (M5) |
| `c_interop.dux` | parses | libclang importer (M6) |

## Deliberate runtime gaps today

- **Concatenation temporaries are not reclaimed.** `a + b` and `$"{a} {b}"`
  allocate from the context, and only what is bound can be `del`'d. An arena
  behind the context — which is what the context is *for* — reclaims them in a
  block; the malloc-backed debug allocator reports them at exit instead.
- **No guard pages, no allocation table.** The debug allocator catches a
  double free and reports what is still live at exit, but a wild pointer that
  happens to look like one of ours is not caught, and the leak report gives a
  count rather than the locations that leaked. Guard pages and a per-block
  location table wait on the same milestone as the panic runtime.
- **One trap, and it is the compiler's own.** A `match` whose arms could all
  decline — which the checker rules out, so it means a bug — stops the program
  with `dux: no \`match\` arm matched`. `panic`, `unwrap` and the debug-mode
  bounds check wait on the same runtime as their messages.
- **A sum type is not printable yet.** A `$".."` hole takes numbers, `bool`s,
  text, pointers and records (a record says itself field by field, since the
  compiler knows its shape); a variant prints nothing until the prelude gives
  the language a formatter.
- **The niche is `Option<*T>`'s alone.** DESIGN 4.3 also allows the spare tag
  bits of a small `Option<enum>` to hide a tag; only the pointer case is built,
  and only for the two predeclared enums, which is what 4.4 asks of the rest.
- **The tag is a `u16`, always.** `#[repr(u8)]` and `#[repr(i32)]` are how DESIGN
  says to override it, and an attribute argument that is not a literal does not
  parse yet, so neither the narrower tag nor its 256-variant error exists.
- **A record is matched whole.** `P { x }` names a variant, not a record: a
  `match` on a record takes a binder, a `_`, or an `else`, and destructuring a
  record through patterns is not in the design.
- **`Option` has no methods.** `unwrap`, `unwrap_or`, `map` and the rest are
  prelude code (DESIGN 4.5), and the prelude arrives with the standard library.
- **No bounds traps.** `dims[i]` is unchecked; the debug-mode bounds check
  waits on the panic runtime.
- **`defer` is body-level.** Correct per-path behavior inside nested blocks
  needs a runtime defer chain (M3); nested `defer` reports E0007 honestly.
- **Arrays do not cross function boundaries.** By-value array parameters and
  results report E0007; they return in M3 with slices.
- **Ranges are loop-only.** `0..3` exists as a `for .. in` iterable, not as a
  value.
- **An interpolated hole holds no quotes.** `{f("x")}` does not lex; `{x}` and
  `{}` with trailing arguments do.
- **Full DWARF type DIEs land at M7.** Today's debug info is the line table
  plus subprograms with primitive parameter types — enough for `gdb` to stop
  on Dux lines and print source.
