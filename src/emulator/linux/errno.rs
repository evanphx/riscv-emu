//! Linux error numbers.

use std::fmt;

/// A Linux error number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Errno(pub i64);

impl Errno {
    pub const EPERM: Errno = Errno(1);
    pub const ENOENT: Errno = Errno(2);
    pub const ESRCH: Errno = Errno(3);
    pub const EINTR: Errno = Errno(4);
    pub const EIO: Errno = Errno(5);
    pub const ENXIO: Errno = Errno(6);
    pub const E2BIG: Errno = Errno(7);
    pub const ENOEXEC: Errno = Errno(8);
    pub const EBADF: Errno = Errno(9);
    pub const ECHILD: Errno = Errno(10);
    pub const EAGAIN: Errno = Errno(11);
    pub const ENOMEM: Errno = Errno(12);
    pub const EACCES: Errno = Errno(13);
    pub const EFAULT: Errno = Errno(14);
    pub const ENOTBLK: Errno = Errno(15);
    pub const EBUSY: Errno = Errno(16);
    pub const EEXIST: Errno = Errno(17);
    pub const EXDEV: Errno = Errno(18);
    pub const ENODEV: Errno = Errno(19);
    pub const ENOTDIR: Errno = Errno(20);
    pub const EISDIR: Errno = Errno(21);
    pub const EINVAL: Errno = Errno(22);
    pub const ENFILE: Errno = Errno(23);
    pub const EMFILE: Errno = Errno(24);
    pub const ENOTTY: Errno = Errno(25);
    pub const ETXTBSY: Errno = Errno(26);
    pub const EFBIG: Errno = Errno(27);
    pub const ENOSPC: Errno = Errno(28);
    pub const ESPIPE: Errno = Errno(29);
    pub const EROFS: Errno = Errno(30);
    pub const EMLINK: Errno = Errno(31);
    pub const EPIPE: Errno = Errno(32);
    pub const EDOM: Errno = Errno(33);
    pub const ERANGE: Errno = Errno(34);

    /// Returns the syscall return value for the error.
    pub fn ret(self) -> u64 {
        self.0.wrapping_neg() as u64
    }

    fn message(self) -> Option<&'static str> {
        Some(match self.0 {
            1 => "Operation not permitted",
            2 => "No such file or directory",
            3 => "No such process",
            4 => "Interrupted system call",
            5 => "I/O error",
            6 => "No such device or address",
            7 => "Argument list too long",
            8 => "Exec format error",
            9 => "Bad file number",
            10 => "No child processes",
            11 => "Try again",
            12 => "Out of memory",
            13 => "Permission denied",
            14 => "Bad address",
            15 => "Block device required",
            16 => "Device or resource busy",
            17 => "File exists",
            18 => "Cross-device link",
            19 => "No such device",
            20 => "Not a directory",
            21 => "Is a directory",
            22 => "Invalid argument",
            23 => "File table overflow",
            24 => "Too many open files",
            25 => "Not a typewriter",
            26 => "Text file busy",
            27 => "File too large",
            28 => "No space left on device",
            29 => "Illegal seek",
            30 => "Read-only file system",
            31 => "Too many links",
            32 => "Broken pipe",
            33 => "Math argument out of domain of func",
            34 => "Math result not representable",
            _ => return None,
        })
    }
}

impl fmt::Display for Errno {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.message() {
            Some(msg) => f.write_str(msg),
            None => write!(f, "{{Errno {}}}", self.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errno() {
        assert_eq!(Errno::ENOENT.ret() as i64, -2);
        assert_eq!(Errno::EFAULT.to_string(), "Bad address");
        assert_eq!(Errno(99).to_string(), "{Errno 99}");
    }
}
