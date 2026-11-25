# Admin requests

## 1. LLVM / clang development packages — **DONE, thank you** (verified 2026-11-23)

```
sudo dnf install llvm-devel clang-devel lld        # installed
```

Verified state on this machine:

- LLVM **23.1.5** with `llvm-devel`: `libLLVM-23.so`, static libs, headers and cmake files
  under `/usr/lib64/llvm23`, `llvm-config` on `PATH` → `inkwell` / `llvm-sys` can link.
- clang **23.1.5** with `clang-devel`: `libclang.so.23.1` + `/usr/include/clang-c/*.h`
  → the libclang C API works, which is what `#c_include` (DESIGN.md D4) is built on.
- `lld` / `ld.lld` **23.1.5**, `lldb` **23.1.5**, `valgrind` 3.28, `mingw64-gcc-c++` 16.2.1
  (so a Windows cross-link target is available later).
- Rust 1.100.0 / cargo 1.100.0, `ninja`, `cmake`, `gcc`, `g++` present.

Binding pairing chosen (no action needed, recorded so a future LLVM bump is obvious):
`inkwell 0.11.0` with feature `llvm23-1`, which pulls `llvm-sys 231.0.2` — an exact match for
system LLVM 23.1.5. If Fedora bumps LLVM to 24 the compiler will not build until an
`llvm24-0`-capable `llvm-sys` exists; that is the moment to re-run this decision (DESIGN.md §11.2).

## 2. Header-only and game libraries for the C-interop test corpus (needed at M6)

The M6 tests must prove that `static inline` / header-only libraries work (DESIGN.md D19), and
the domain is games, so the corpus wants a real game-facing header library:

```
sudo dnf install stb-devel glfw-devel
```

- `stb-devel` — `stb_image.h` is the canonical header-only library and the direct test for D19
  (already referenced in `example/c_interop.dux`).
- `glfw-devel` — a normal, well-behaved C API to import and call from a Dux window/input sample.

Already present, no action: `SDL2/SDL.h` (+ pkg-config `sdl2`), `GL/gl.h`, `zlib.h`,
`sqlite3.h`, `pthread.h`.

## 3. Reference compilers (nice to have, not required)

Useful only for A/B checking a language decision against the language Dux borrows from:

```
sudo dnf install odin        # probably not packaged; else: git clone --depth 1 https://github.com/Odin-Programming-Language/odin && ./build_vbs.sh
# zig is not on crates.io: https://ziglang.org/download/ or sudo dnf install zig
```

Nothing in the plan depends on them.

## 4. Network notes (no action needed, will save future confusion)

`crates.io`'s REST API returns **403** from this box, so `cargo search` and any API-based tool
fails. `index.crates.io` (sparse index) and `static.crates.io` both work, so `cargo add`,
`cargo fetch` and `cargo build` are fine. Look crate versions up via the sparse index
(`https://index.crates.io/ll/vm/llvm-sys`) instead of the API.

E2E tests are designed to be self-contained: no network, no `$HOME` writes.

## 5. raylib, so the window example can run (not needed by any test)

`example/raylib/hello.dux` is the shown-to-humans proof that a real game library
crosses `#[c_include]` with nothing written by hand — including records by value
(`Color` in, `Vector2` back). It is excluded from the test corpus (the suite must
stay self-contained), so nothing verifies it on this box yet:

```
sudo dnf install raylib-devel        # DONE, thank you — raylib 5.5.0 verified
```

Verified on this box (2026-11-25): `duxc build example/raylib/hello.dux`
compiles and links against `libraylib.so.550` with zero unresolved symbols,
and the binary runs into raylib's own platform init. This machine is headless,
so opening the actual window — a maroon circle following the mouse under the
text `move the mouse` — needs a desktop:
