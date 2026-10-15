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
    /// default.
    pub jit_threshold: u16,
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
