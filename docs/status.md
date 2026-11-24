# Compiler status per example

Which `example/*.dux` files the current build compiles and runs, and what
each remaining gap is waiting for. A gap is a milestone, never a verdict.

| File | Status | Waits on |
|---|---|---|
| `hello.dux` | compiles, runs, ships `.debug_line` | — |
| `control_flow.dux` | parses; value `if` and `for .. in` now lower | `match` (M4) |
| `types.dux` | parses; fixed arrays now lower | tuples `pair.0` (M3) |
| `structs.dux` | parses | `struct`/`impl`/methods (M3) |
| `pointers.dux` | parses | pointer targets, `*p = v` (M3) |
| `memory.dux` | parses | `new`/`make`/`free`, context (M3) |
| `strings.dux` | parses | `string` methods, slices, `for b in s` (M3) |
| `sum_types.dux` | parses | `enum` sum types, `match` (M4) |
| `option_result.dux` | parses | `Option`/`Result` patterns (M4) |
| `generics.dux` | parses | monomorphisation (M5) |
| `c_interop.dux` | parses | libclang importer (M6) |

## Deliberate runtime gaps today

- **No bounds traps.** `dims[i]` is unchecked; the debug-mode bounds check
  waits on the panic runtime (no traps exist yet at all).
- **`defer` is body-level.** Correct per-path behavior inside nested blocks
  needs a runtime defer chain (M3); nested `defer` reports E0007 honestly.
- **Arrays do not cross function boundaries.** By-value array parameters and
  results report E0007; they return in M3 with slices.
- **Ranges are loop-only.** `0..3` exists as a `for .. in` iterable, not as a
  value.
- **Full DWARF type DIEs land at M7.** Today's debug info is the line table
  plus subprograms with primitive parameter types — enough for `gdb` to stop
  on Dux lines and print source.
