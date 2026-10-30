# Contributing to `lo`

Thank you for contributing to the `lo` compiler (`loc`), `lomake` build system, and `std` standard library!

## Core Principles

1. **Specification Fidelity**: [`lo.md`](lo.md) is the single source of truth for syntax, operators, precedence, type rules, and runtime semantics. Every language change must match `lo.md`.
2. **Zero Placeholders**: Never merge `todo!()`, `unimplemented!()`, or stubbed functions. Every feature must be backed by real static analysis, code generation, and automated tests.
3. **Zero External Bloat**: Both `loc` and `lomake` are written in dependency-free Rust (`std` only) for instant, reproducible offline builds across Linux, macOS, and Android/Termux.

## Git & Commit Protocol (Conventional Commits)

Make small, atomic commits using Conventional Commit prefixes:
- `feat(lexer): ...`
- `feat(parser): ...`
- `feat(typechecker): ...`
- `feat(comptime): ...`
- `feat(codegen): ...`
- `feat(lomake): ...`
- `feat(std): ...`
- `fix(...): ...`
- `test: ...`
- `chore: ...`
- `docs: ...`

## Running Tests Locally

Before submitting a Pull Request, verify all four test tiers pass:

```bash
# Run all unit, golden AST, negative compile_fail, e2e, std, and lomake tests
cargo test --workspace

# Check formatting and lints
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

### Test Suites in `tests/`
- **`tests/lexer_tests/`**: Tokenization, nested `#[ ... ]#` comments, `1b`/`0b` literals, compound operators, and `"{expr}"` string interpolation.
- **`tests/parser_golden/`**: Parses `.lo` inputs and compares pretty-printed AST output against `.ast` golden snapshots.
- **`tests/compile_fail/`**: Verifies that `loc` rejects invalid programs (assigning `~` to non-null `[T]`, mutating `=` bindings, mutating `@` in non-`!` methods, `~=` at top level, type mismatches) with expected diagnostic messages.
- **`tests/e2e/`**: Compiles `.lo` programs through the C11 backend into native binaries, executes them, and compares `stdout` against `.stdout` expectations.
