//! Emulator parameters.

use std::path::PathBuf;

/// Emulator parameters.
#[derive(Clone, Debug, Default)]
pub struct Params {
    pub verbose: bool,
    pub ktrace: bool,
    pub cpu_trace: bool,
    pub csr802: String,
    pub profile: bool,
    pub color: bool,
    pub cooked: bool,
    pub fs_root: String,
    /// Enables the JIT compiler.
    pub jit: bool,
    /// Number of executions before a block is compiled; 0 selects the
    /// default, which adapts to the backlog of the compiler threads.
    pub jit_threshold: u16,
    /// Number of background compiler threads; 0 selects the default.
    pub jit_threads: usize,
    /// Compiles blocks synchronously instead of on a background
    /// thread, which makes runs deterministic.
    pub jit_sync: bool,
    /// File for saving and loading JIT compiled code.
    pub jit_cache: String,
    /// Compiles with Cranelift's optimizations.
    pub jit_opt: bool,
    /// JIT backend: empty or "cranelift", "direct", or "direct-mmap".
    pub jit_backend: String,
}

impl Params {
    /// Maps the guest path name under the filesystem root.
    pub fn make_path(&self, pathname: &str) -> PathBuf {
        let rel = pathname.trim_start_matches('/');
        if self.fs_root.is_empty() {
            if pathname.starts_with('/') {
                return PathBuf::from(pathname);
            }
            return PathBuf::from(rel);
        }
        PathBuf::from(&self.fs_root).join(rel)
    }
}
