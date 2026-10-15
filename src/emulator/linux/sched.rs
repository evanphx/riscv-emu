//! Clone flags.

pub const CSIGNAL: u64 = 0x000000ff;
pub const CLONE_VM: u64 = 0x00000100;
pub const CLONE_FS: u64 = 0x00000200;
pub const CLONE_FILES: u64 = 0x00000400;
pub const CLONE_SIGHAND: u64 = 0x00000800;
pub const CLONE_PIDFD: u64 = 0x00001000;
pub const CLONE_PTRACE: u64 = 0x00002000;
pub const CLONE_VFORK: u64 = 0x00004000;
pub const CLONE_PARENT: u64 = 0x00008000;
pub const CLONE_THREAD: u64 = 0x00010000;
pub const CLONE_NEWNS: u64 = 0x00020000;
pub const CLONE_SYSVSEM: u64 = 0x00040000;
pub const CLONE_SETTLS: u64 = 0x00080000;
pub const CLONE_PARENT_SETTID: u64 = 0x00100000;
pub const CLONE_CHILD_CLEARTID: u64 = 0x00200000;
pub const CLONE_DETACHED: u64 = 0x00400000;
pub const CLONE_UNTRACED: u64 = 0x00800000;
pub const CLONE_CHILD_SETTID: u64 = 0x01000000;
pub const CLONE_NEWCGROUP: u64 = 0x02000000;
pub const CLONE_NEWUTS: u64 = 0x04000000;
pub const CLONE_NEWIPC: u64 = 0x08000000;
pub const CLONE_NEWUSER: u64 = 0x10000000;
pub const CLONE_NEWPID: u64 = 0x20000000;
pub const CLONE_NEWNET: u64 = 0x40000000;
pub const CLONE_IO: u64 = 0x80000000;

const CLONE_FLAG_NAMES: [(u64, &str); 25] = [
    (CSIGNAL, "CSIGNAL"),
    (CLONE_VM, "VM"),
    (CLONE_FS, "FS"),
    (CLONE_FILES, "FILES"),
    (CLONE_SIGHAND, "SIGHAND"),
    (CLONE_PIDFD, "PIDFD"),
    (CLONE_PTRACE, "PTRACE"),
    (CLONE_VFORK, "VFORK"),
    (CLONE_PARENT, "PARENT"),
    (CLONE_THREAD, "THREAD"),
    (CLONE_NEWNS, "NEWNS"),
    (CLONE_SYSVSEM, "SYSVSEM"),
    (CLONE_SETTLS, "SETTLS"),
    (CLONE_PARENT_SETTID, "SETTID"),
    (CLONE_CHILD_CLEARTID, "CLEARTID"),
    (CLONE_DETACHED, "DETACHED"),
    (CLONE_UNTRACED, "UNTRACED"),
    (CLONE_CHILD_SETTID, "CHILDSETTID"),
    (CLONE_NEWCGROUP, "NEWCGROUP"),
    (CLONE_NEWUTS, "NEWUTS"),
    (CLONE_NEWIPC, "NEWIPC"),
    (CLONE_NEWUSER, "NEWUSER"),
    (CLONE_NEWPID, "NEWPID"),
    (CLONE_NEWNET, "NEWNET"),
    (CLONE_IO, "IO"),
];

/// Formats the clone flags as a comma-separated list of names.
pub fn clone_flags(flags: u64) -> String {
    CLONE_FLAG_NAMES
        .iter()
        .filter(|(bit, _)| flags & bit != 0)
        .map(|(_, name)| *name)
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags() {
        // glibc pthread_create flags.
        let flags = CLONE_VM
            | CLONE_FS
            | CLONE_FILES
            | CLONE_SIGHAND
            | CLONE_THREAD
            | CLONE_SYSVSEM
            | CLONE_SETTLS
            | CLONE_PARENT_SETTID
            | CLONE_CHILD_CLEARTID;
        assert_eq!(
            clone_flags(flags),
            "VM,FS,FILES,SIGHAND,THREAD,SYSVSEM,SETTLS,SETTID,CLEARTID"
        );
        assert_eq!(clone_flags(17), "CSIGNAL");
    }
}
