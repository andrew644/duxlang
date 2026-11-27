# TODO

## Language gaps (the compiler refuses these today, usually with E0007)

- [ ] Bounds checks in debug builds with a trap and a source location (`-bounds-check=on|off|auto`, DESIGN §243; M2 gap)
- [ ] Debug allocator: guard pages and a per-block allocation location table (M3 gap)
- [ ] `free(NULL)` as a silent no-op (M5 gap) — pin the chosen semantics with an e2e test

## C interop (M6 "remaining: reported, none silent")

- [ ] Import `long double` as `c_long_double`
- [ ] Import function-like macros
- [ ] Import global variables from headers

## M7 — toolchain maturity

- [ ] Full DWARF type DIEs for user types; `DW_LANG_C99` DIEs for imported C types so lldb prints them
- [ ] `-O2` release pipeline; prove release stdout identical to debug on the corpus
- [ ] `dux test` subcommand: test discovery, runner, per-run leak report
- [ ] `lldb -batch` smoke test in the suite
- [ ] Grow the negative-test corpus (bad `match` arms, layout, conversions all have seeds; pointers/allocations do not)

## M8 — ecosystem

- [ ] `dux.mod` manifest (multi-file/module programs beyond same-directory imports)
- [ ] pkg-config as a cdep source for libraries, not just flags
- [ ] `--target` cross-compilation (mingw toolchain is present on this box; `--target=x86_64-pc-windows-gnu` e2e)
- [ ] LSP: diagnostics, hover, go-to-definition, with an integration test

## Test & docs housekeeping

- [ ] Update the M4 row of the DESIGN.md milestone table — the work has landed (sum types, `match`, `Option`/`Result`, niche, exhaustiveness, negative tests) but the row still reads open
- [ ] Add pinned-stdout e2e tests for `sum_types.dux` and `option_result.dux` (named as M4's acceptance; neither is in `tests/e2e`); make `option_result.dux` self-contained first — it reads `slot1.txt`, which is not in the repo
- [ ] Run `example/raylib/hello.dux` on a desktop box to verify the window actually opens (build/link already verified headless; see ADMIN.md §5)
