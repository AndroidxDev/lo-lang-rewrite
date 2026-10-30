# lo — Keyword-Free Systems & Application Programming Language

`lo` is a statically-typed, native-compiled programming language built on the **Four-Bracket Architecture** with **zero reserved keywords**, **Kotlin-style strict null-safety**, **Perceus reference counting with in-place memory reuse**, **compile-time metaprogramming (`<{ ... }>`)**, and **built-in structured concurrency**.

This repository contains the complete Rust implementation of:
- **`loc`** — Industrial compiler (`compiler/`) translating `.lo` source into optimized C11 and native machine binaries via `clang`/`gcc`/`cc`.
- **`lomake`** — Project manager, incremental dependency-graph builder, and test runner (`lomake/`), configured natively in `Lomake.lo`.
- **`std`** — Standard library (`std/`) written in `lo` on top of zero-overhead `<"c" ...>` FFI (`io`, `math`, `fs`, `str`, `time`).

---

## Core Language Philosophy (Four Brackets, Zero Keywords)

Every syntactic construct in `lo` maps deterministically to one of four bracket pairs (see [`lo.md`](lo.md) for the complete v1.0 specification):

| Bracket | Domain | Purpose |
|---|---|---|
| `[ ... ]` | Data & Types | Arrays, slices, structs, ADTs/enums, HashMaps (`=>`), HashSets (`%`), type annotations, `match` scrutinees |
| `{ ... }` | Logic & Blocks | Functions, conditional/match arms, loop bodies, pipeline stages, `try`/`catch`/`finally`, `defer` |
| `( ... )` | Evaluation & Args | Function parameters/calls, logical conditions, guards, grouping |
| `< ... >` | Meta & External | Module imports (`<"std/math">`), C ABI FFI (`<"c" ...>`), generics (`<T>`), `comptime` (`<{ ... }>`) |

### Key Highlights
- **Strict Null-Safety**: Non-null by default (`[T]`), explicit nullable types (`[T?]`), literal `~`, safe calls (`?.`, `?[]`, `?()`), Elvis (`??`), safe pipe (`?~>`), assertion (`!!`), safe cast (`?[T]`), and flow-sensitive Smart Casts.
- **Immutability & Perceus RC**: Immutable bindings (`=`), function-local mutable bindings (`~=`), explicit mutations (`<-`, `+<-`, `-<-`, `*<-`, `/<-`), and Perceus reference counting with in-place structural reuse (`[..user, hp = 75]`).
- **Control Flow & Generators**: Chained conditionals (`? (c1) {} (c2) {} {}`), deep pattern matching (`? [val] { ... }`), early/guard/local/labeled returns (`^`, `?^`, `^@`, `^#label`), and lazy stackful-callback generators (`^~`).
- **Structured Errors & Resource Safety**: Protected `!{ ... }` blocks, typed/pattern `?! [...] {}` catches, guaranteed `?> {}` finally blocks, `!^` throw, postfix `?` propagation, `~{}` defer, and `?!{}` errdefer.
- **Pipelines & Concurrency**: Loop-fused functional streams (`~>`), multi-core parallel pipelines (`~>>`), async tasks (`~>{ ... }`), `<~` await, and typed MPMC channels (`[~T]()`).

---

## Repository Architecture

```text
lo-lang/
├── compiler/                    # Source code of the `loc` compiler (Rust)
│   ├── lexer/                   # Full token scanner & string interpolation lexer
│   ├── parser/                  # Recursive descent + Pratt AST parser (100% EBNF)
│   ├── sema/                    # Type inference, Smart Casts, Null-safety & mutability checker
│   ├── comptime/                # Compile-time interpreter for `<{ ... }>` blocks
│   ├── codegen/                 # AST -> C11 backend + native clang/gcc driver
│   └── runtime/                 # C11 header runtime (Perceus RC, threads, channels, maps, errors)
├── lomake/                      # `lomake` build system & package/test runner (Rust)
├── std/                         # Standard library written in `lo` + C FFI
│   ├── io.lo                    # Console I/O and formatted printing
│   ├── math.lo                  # Trigonometry, constants, PRNG, Vec2 & Vec3
│   ├── fs.lo                    # File I/O with structured error handling
│   ├── str.lo                   # UTF-8 string operations, splitting & number parsing
│   └── time.lo                  # Monotonic clocks, sleep, and block benchmarking
├── tests/                       # Multi-tier verification suite
│   ├── lexer_tests/             # Token & interpolation unit tests
│   ├── parser_golden/           # Golden `.lo` -> `.ast` snapshot tests
│   ├── compile_fail/            # Negative tests verifying static null/type/mutability rejection
│   └── e2e/                     # End-to-end native compilation & stdout verification
├── examples/                    # Sample `lo` applications with `Lomake.lo` manifests
├── lo.md                        # Authoritative `lo` v1.0 language specification
└── CONTRIBUTING.md              # Contributor workflow & test suite guide
```

---

## Quick Start

### 1. Build Compiler & Build Tool
```bash
cargo build --release
```
This produces `./target/release/loc` and `./target/release/lomake`.

### 2. Compile & Run a `.lo` Program Directly
```bash
./target/release/loc run examples/hello/src/main.lo
```
Or emit a native binary / inspect generated C11 or AST:
```bash
./target/release/loc build app.lo -o app -O3
./target/release/loc ast app.lo
./target/release/loc emit-c app.lo -o app.c
```

### 3. Manage Projects with `lomake`
```bash
./target/release/lomake init my_app
cd my_app
../target/release/lomake run
../target/release/lomake test
```

### 4. Run the Full Test Suite
```bash
cargo test --workspace
```
