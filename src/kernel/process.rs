//! Processes and their file descriptors.

use super::Kernel;
use std::fs::File;
use std::sync::{Arc, Mutex, MutexGuard};

/// An open file descriptor.
#[derive(Clone)]
pub struct Fd {
    pub refcount: i32,
    pub native: Arc<File>,
}

/// A process or a thread.
pub struct Process {
    pub kernel: Arc<Mutex<Kernel>>,
    pub pid: u64,
    pub tgid: u64,
    pub fds: Vec<Option<Fd>>,
    pub ktrace: bool,
}

impl Process {
    /// Locks and returns the kernel.
    pub fn kernel(&self) -> MutexGuard<'_, Kernel> {
        self.kernel.lock().unwrap()
    }

    /// Allocates the lowest free file descriptor for the file.
    pub fn alloc_fd(&mut self, f: File) -> i64 {
        let fd = Fd {
            refcount: 1,
            native: Arc::new(f),
        };
        if let Some(idx) = self.fds.iter().position(|f| f.is_none()) {
            self.fds[idx] = Some(fd);
            return idx as i64;
        }
        self.fds.push(Some(fd));
        self.fds.len() as i64 - 1
    }

    fn fd_mut(&mut self, fd: i64) -> Option<&mut Fd> {
        usize::try_from(fd)
            .ok()
            .and_then(|i| self.fds.get_mut(i))
            .and_then(|f| f.as_mut())
    }

    /// Releases a reference to the file descriptor. The file
    /// descriptor is freed when its last reference is released.
    /// Returns false if the file descriptor is not open.
    pub fn close_fd(&mut self, fd: i64) -> bool {
        let Some(f) = self.fd_mut(fd) else {
            return false;
        };
        f.refcount -= 1;
        if f.refcount <= 0 {
            self.fds[fd as usize] = None;
        }
        true
    }

    /// Returns the file of the file descriptor.
    pub fn get_fd(&self, fd: i64) -> Option<Arc<File>> {
        usize::try_from(fd)
            .ok()
            .and_then(|i| self.fds.get(i))
            .and_then(|f| f.as_ref())
            .map(|f| f.native.clone())
    }

    /// Adds a reference to the file descriptor. Returns false if the
    /// file descriptor is not open.
    pub fn ref_fd(&mut self, fd: i64) -> bool {
        match self.fd_mut(fd) {
            Some(f) => {
                f.refcount += 1;
                true
            }
            None => false,
        }
    }
}
