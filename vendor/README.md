# Vendored crates

## cranelift-codegen 0.136.3

Patched in `src/isa/x64/abi.rs` so that the `tail` calling convention has
no callee-saved registers, and calls to `tail` functions clobber all
registers. Compiled blocks use `tail` and chain to each other with tail
calls, so every block entry and exit saved and restored up to five
registers that only the enter trampoline needs to preserve. The
trampoline (SystemV) now saves them once.

To update Cranelift, copy the new release here and reapply the two hunks
in `src/isa/x64/abi.rs` (search for `CallConv::Tail`).
