//! Linux userspace emulator (stub, implemented separately).

use crate::Params;

pub struct Emulator {
    _params: Params,
}

impl Emulator {
    pub fn new(params: Params) -> Result<Emulator, String> {
        Ok(Emulator { _params: params })
    }

    pub fn load_elf(&mut self, _file: &str) -> Result<(), String> {
        Err("userspace emulation not implemented".to_string())
    }

    /// Runs the loaded program. Returns the program's exit code.
    pub fn run(&mut self, _argv: &[String], _envp: &[String]) -> Result<i32, String> {
        Err("userspace emulation not implemented".to_string())
    }
}
