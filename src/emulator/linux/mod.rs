//! Linux RISC-V syscall interface and types.

pub mod auxv;
pub mod errno;
pub mod futex;
pub mod mman;
pub mod sched;
pub mod stat;
mod syscall;
pub mod syscall_info;

pub use syscall::syscall;
