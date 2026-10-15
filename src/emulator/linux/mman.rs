//! Memory mapping protections and flags.

pub const PROT_NONE: u64 = 0;
pub const PROT_READ: u64 = 1;
pub const PROT_WRITE: u64 = 2;
pub const PROT_EXEC: u64 = 4;

pub const MAP_TYPE: u64 = 0x00f;
pub const MAP_FIXED: u64 = 0x010;
pub const MAP_NORESERVE: u64 = 0x0400;
pub const MAP_ANONYMOUS: u64 = 0x0800;
pub const MAP_GROWSDOWN: u64 = 0x1000;
pub const MAP_DENYWRITE: u64 = 0x2000;
pub const MAP_EXECUTABLE: u64 = 0x4000;
pub const MAP_LOCKED: u64 = 0x8000;
pub const MAP_POPULATE: u64 = 0x10000;
pub const MAP_NONBLOCK: u64 = 0x20000;
pub const MAP_STACK: u64 = 0x40000;
pub const MAP_HUGETLB: u64 = 0x80000;
pub const MAP_FIXED_NOREPLACE: u64 = 0x100000;

/// Names of the mmap flags.
pub const MAP_FLAG_NAMES: [(u64, &str); 12] = [
    (MAP_FIXED, "FIXED"),
    (MAP_NORESERVE, "NORESERVE"),
    (MAP_ANONYMOUS, "ANONYMOUS"),
    (MAP_GROWSDOWN, "GROWSDOWN"),
    (MAP_DENYWRITE, "DENYWRITE"),
    (MAP_EXECUTABLE, "EXECUTABLE"),
    (MAP_LOCKED, "LOCKED"),
    (MAP_POPULATE, "POPULATE"),
    (MAP_NONBLOCK, "NONBLOCK"),
    (MAP_STACK, "STACK"),
    (MAP_HUGETLB, "HUGETLB"),
    (MAP_FIXED_NOREPLACE, "FIXED_NOREPLACE"),
];
