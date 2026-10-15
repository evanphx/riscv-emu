//! File status.

use std::fs::Metadata;
use std::os::unix::fs::{FileTypeExt, MetadataExt};

/// The dirfd value for the current working directory in the *at(2)
/// family of syscalls.
pub const AT_FDCWD: i64 = -100;

/// Type of file.
pub const S_IFMT: u32 = 0o170000;
/// Named pipe (fifo).
pub const S_IFIFO: u32 = 0o010000;
/// Character special.
pub const S_IFCHR: u32 = 0o020000;
/// Directory.
pub const S_IFDIR: u32 = 0o040000;
/// Block special.
pub const S_IFBLK: u32 = 0o060000;
/// Regular.
pub const S_IFREG: u32 = 0o100000;
/// Symbolic link.
pub const S_IFLNK: u32 = 0o120000;
/// Socket.
pub const S_IFSOCK: u32 = 0o140000;
/// Whiteout.
pub const S_IFWHT: u32 = 0o160000;

/// Set user id on execution.
pub const S_ISUID: u32 = 0o004000;
/// Set group id on execution.
pub const S_ISGID: u32 = 0o002000;
/// Save swapped text even after use.
pub const S_ISVTX: u32 = 0o001000;
/// Read permission, owner.
pub const S_IRUSR: u32 = 0o000400;
/// Write permission, owner.
pub const S_IWUSR: u32 = 0o000200;
/// Execute/search permission, owner.
pub const S_IXUSR: u32 = 0o000100;

/// Size of the RISC-V Linux struct stat.
pub const STAT_SIZE: usize = 128;

/// Marshals the file metadata into the RISC-V Linux struct stat.
/// Devices are reported as owned by uid and gid 1000 with the tty
/// device number.
pub fn marshal_file_info(fi: &Metadata) -> [u8; STAT_SIZE] {
    let mut stat = [0u8; STAT_SIZE];
    let ft = fi.file_type();

    let mut mode = fi.mode() & 0o777;
    // Character devices are also devices.
    let device = ft.is_block_device() || ft.is_char_device();
    if ft.is_fifo() {
        mode |= S_IFIFO;
    } else if ft.is_char_device() {
        mode |= S_IFCHR | S_IFBLK;
    } else if ft.is_dir() {
        mode |= S_IFDIR;
    } else if ft.is_block_device() {
        mode |= S_IFBLK;
    } else if ft.is_symlink() {
        mode |= S_IFLNK;
    } else if ft.is_socket() {
        mode |= S_IFSOCK;
    } else {
        mode |= S_IFREG;
    }

    let put32 = |stat: &mut [u8; STAT_SIZE], ofs: usize, v: u32| {
        stat[ofs..ofs + 4].copy_from_slice(&v.to_le_bytes());
    };
    let put64 = |stat: &mut [u8; STAT_SIZE], ofs: usize, v: u64| {
        stat[ofs..ofs + 8].copy_from_slice(&v.to_le_bytes());
    };

    put64(&mut stat, 8, fi.ino()); // st_ino
    put32(&mut stat, 16, mode); // st_mode
    put32(&mut stat, 20, 1); // st_nlink
    put32(&mut stat, 24, 1000); // st_uid
    put32(&mut stat, 28, 1000); // st_gid
    if device {
        put64(&mut stat, 32, 34816); // st_rdev
    }
    let size = fi.len();
    put64(&mut stat, 48, size); // st_size
    put64(&mut stat, 56, 1024); // st_blksize
    put64(&mut stat, 64, size.div_ceil(1024)); // st_blocks

    let mtime = fi.mtime() as u64;
    put64(&mut stat, 72, mtime); // st_atime
    put64(&mut stat, 88, mtime); // st_mtime
    put64(&mut stat, 104, mtime); // st_ctime

    stat
}
