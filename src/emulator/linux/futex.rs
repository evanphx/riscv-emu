//! Futex operations.

pub const FUTEX_WAIT: u64 = 0;
pub const FUTEX_WAKE: u64 = 1;
pub const FUTEX_FD: u64 = 2;
pub const FUTEX_REQUEUE: u64 = 3;
pub const FUTEX_CMP_REQUEUE: u64 = 4;
pub const FUTEX_WAKE_OP: u64 = 5;
pub const FUTEX_LOCK_PI: u64 = 6;
pub const FUTEX_UNLOCK_PI: u64 = 7;
pub const FUTEX_TRYLOCK_PI: u64 = 8;
pub const FUTEX_WAIT_BITSET: u64 = 9;
pub const FUTEX_WAKE_BITSET: u64 = 10;
pub const FUTEX_WAIT_REQUEUE_PI: u64 = 11;
pub const FUTEX_CMP_REQUEUE_PI: u64 = 12;
pub const FUTEX_LOCK_PI2: u64 = 13;

pub const FUTEX_PRIVATE_FLAG: u64 = 128;
pub const FUTEX_CLOCK_REALTIME: u64 = 256;
pub const FUTEX_CMD_MASK: u64 = !(FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME);

/// Returns the name of the futex operation, or an empty string for
/// unknown operations.
pub fn op_name(op: u64) -> &'static str {
    match op & 127 {
        FUTEX_WAIT => "FUTEX_WAIT",
        FUTEX_WAKE => "FUTEX_WAKE",
        FUTEX_FD => "FUTEX_FD",
        FUTEX_REQUEUE => "FUTEX_REQUEUE",
        FUTEX_CMP_REQUEUE => "FUTEX_CMP_REQUEUE",
        FUTEX_WAKE_OP => "FUTEX_WAKE_OP",
        FUTEX_LOCK_PI => "FUTEX_LOCK_PI",
        FUTEX_UNLOCK_PI => "FUTEX_UNLOCK_PI",
        FUTEX_TRYLOCK_PI => "FUTEX_TRYLOCK_PI",
        FUTEX_WAIT_BITSET => "FUTEX_WAIT_BITSET",
        FUTEX_WAKE_BITSET => "FUTEX_WAKE_BITSET",
        FUTEX_WAIT_REQUEUE_PI => "FUTEX_WAIT_REQUEUE_PI",
        FUTEX_CMP_REQUEUE_PI => "FUTEX_CMP_REQUEUE_PI",
        FUTEX_LOCK_PI2 => "FUTEX_LOCK_PI2",
        _ => "",
    }
}
