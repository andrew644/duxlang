# Compiler status per example

Which `example/*.dux` files the current build compiles and runs, and what
each remaining gap is waiting for. A gap is a milestone, never a verdict.

| File | Status | Waits on |
|---|---|---|
| `hello.dux` | compiles, runs, ships `.debug_line` | — |
| `control_flow.dux` | compiles, runs; `match` in value position reads ranges, wildcards and guards | — |
| `types.dux` | parses; fixed arrays now lower | tuples `pair.0` (M3) |
| `structs.dux` | compiles, runs, `math.sqrt` and all — `std/math` is a file in this repo now | — |
| `pointers.dux` | compiles, runs; `Option<*Node>` is one word and the list walk is a pattern condition | — |
| `memory.dux` | parses | `std/mem` (the arena the context rebinding wants) and `Vec<string>` returning through a call |
| `strings.dux` | compiles and runs until `"héllo".runes()`: concatenation, byte slicing, raw and escaped literals, `c".."`, `.len()`, `.bytes()` and `fmt.println` all lower | `.runes()`, which decodes UTF-8 and needs the string a rune prints as |
| `sum_types.dux` | compiles, runs; variants, payloads, guards, or-patterns, ranges and exhaustiveness all reach machine code, and the dispatch is the `switch` on the tag | — |
| `option_result.dux` | the predeclared `Option`/`Result` lower and match today, and `std/prelude` now hands every program `unwrap_or`, `is_some` and `is_none` | `std/fs` (file reading over `extern`), `.bytes()`, `.trim()`, and `unwrap`, which stops the program (the panic runtime) |
| `generics.dux` | compiles, runs — `max<i32>`, `Pair<i32, string>`, `Either<string, i32>` matched, `zeros<4>()`, `sum(&[1, 2, 3])`, and `HashMap<string, i32>` with `insert`, `get(..).unwrap_or(..)` and `len`, the table and the method being prelude Dux | — |
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
- **The jump table is for the plain shape.** A `match` whose arms each name one
  variant, and ask for nothing below it but names, becomes one `switch` on the
  tag. A guard, a literal, a range, an or-pattern, a variant written twice or a
  variant taken apart beyond its names is a chain of tests in the order written
  — correct, and one jump slower. Nested `match` lowers the same way, one switch
  per level where a switch can say it.
- **A record is matched whole.** `P { x }` names a variant, not a record: a
  `match` on a record takes a binder, a `_`, or an `else`, and destructuring a
  record through patterns is not in the design.
- **The standard library is a directory, not a build.** `std/prelude` is read
  into every program that lives under a tree carrying one — its declarations
  arrive bare, not under a namespace — and `$DUX_STDLIB` names the root for
  programs elsewhere. Written there, in Dux: `Option::unwrap_or`, `is_some`
  and `is_none`; `Vec<T>`, growing through the context; `HashMap<K, V>`, open
  addressing probed by the builtin `hash`. `std/strings` carries `less`,
  `starts_with`, `ends_with`, `find`, `contains` and `repeat` under its name.
  The library grows a piece at a time: `unwrap` stops the program, which
  waits on the runtime that says why, and a call asking for a method a sum
  type does not have stays silent for that reason.
- **A method reads its receiver through an address, so a receiver a call built
  gets one.** `make().scale(2.0)` puts the record in a slot of its own and
  borrows that; `"dux".len()` and `"dux".bytes()` do the same to a view that
  would otherwise live in registers. The temporary lives as long as the block
  that made it, which is where the value would have lived anyway.
- **A library is named by the declaration that needs it.** `#[link(name = "m")]
  extern "C" { fn sqrtf(x: f32) -> f32 }` is how `std/math` says where its
  symbol lives, and the link line carries `-l m` because of it. A name a C
  header owns and a namespace wants for something else (`sqrt` for `f64` and
  `f32` both) waits on `#[c_name]` (M6).
- **A module is a place names live in.** `mod gfx { … }` carries its
  declarations, `import "gfx/shapes"` reads the file beside the one that asked
  (or below it, or in `$DUX_STDLIB`), and what it declares is known as
  `shapes.Size`, `shapes.area`, `shapes.LIMIT` — inside the module the same
  names are bare. What waits: visibility (`pub` is out of scope, OPEN-18),
  `Shape::Circle` of a module's own enum through the module's name, and the
  `std/..` files, which are ordinary modules now: `math.sqrt`, `fmt.println`
  and `strings.less` are what an `import "std/.."` brings.
- **No bounds traps.** `dims[i]` is unchecked; the debug-mode bounds check
  waits on the panic runtime.
- **`defer` is body-level.** Correct per-path behavior inside nested blocks
  needs a runtime defer chain (M3); nested `defer` reports E0007 honestly.
- **A fixed array is not a parameter.** A result crosses the boundary whole:
  the callee reads the array out of wherever it was standing, the caller
  builds it standing in a slot of its own, so `zeros<4>() -> [f32; N]` answers
  and `p[0]` goes on reading it in place. An array written as a *parameter*
  still reports E0007; pass `&xs`, which is a view.
- **A generic is code, not a function.** A generic `fn` is emitted once per
  distinct list of arguments some call gave it — `dux.max.i32`,
  `dux.max.f64` — and not at all for a generic no call asked for. A parameter
  the call cannot say is said at the call, since nothing vaguer can be
  emitted in its place.
- **A method's own parameters are not there yet.** `impl Pair<A, B> { … }` and
  `impl Option<T> { … }` parse — a sum type is a type an `impl` extends too,
  and the predeclared enums are ordinary enums, so their methods are ordinary
  code — and each receiver that calls for a method gets that method emitted in
  its own types (`dux.Cell.i32.shown`, `dux.Option.string.shown`). A method
  written over parameters of its own, beyond the `impl`'s, reports E0007, and
  so does `impl<T> Option<T>`, which states the parameters twice.
- **Ranges are loop-only.** `0..3` exists as a `for .. in` iterable, not as a
  value.
- **An interpolated hole holds no quotes.** `{f("x")}` does not lex; `{x}` and
  `{}` with trailing arguments do.
- **Full DWARF type DIEs land at M7.** Today's debug info is the line table
  plus subprograms with primitive parameter types — enough for `gdb` to stop
  on Dux lines and print source.
