---
name: Bug Report
about: Report a compiler bug, wrong codegen, false positive/negative diagnostic, or runtime crash
title: "bug: "
labels: bug
---

## Minimal Reproducer (`.lo`)

```lo
+main = {
  # paste minimal reproducing code here
}
```

## Expected Behavior

Describe what should happen according to `lo.md`.

## Actual Behavior

Paste compiler diagnostic, generated C snippet (`loc emit-c`), or runtime output.

## Environment

- OS / Architecture:
- `loc --version`:
- C Compiler (`clang --version` or `gcc --version`):
