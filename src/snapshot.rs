//! VM snapshots: suspending a running system to a file and restoring
//! it.
//!
//! A snapshot file has three parts:
//!
//! ```text
//! 0               header (HEADER_SIZE bytes)
//! RAM_OFFSET      guest RAM image (ram_size bytes)
//! state_offset    machine state: configuration, CPU, and devices
//! ```
//!
//! The RAM image is stored at its RAM offset, so restoring maps the
//! file into the guest address space with one copy-on-write mmap
//! instead of reading it: a restore takes the same time for any RAM
//! size, and the host reads pages when the guest first touches them.
//! Zero pages are not written and stay holes in the file, so the file
//! takes only as much disk space as the guest RAM in use.
//!
//! Suspending a VM to the file it was restored from writes back only
//! the pages the guest modified since the restore. They are the
//! copied-on-write pages of the private file mapping, which
//! /proc/self/pagemap reports as anonymous memory. Another emulator
//! that has the same file mapped keeps a shared lock on it; then the
//! file is not modified in place but replaced with a new file.

use crate::cpu::Cpu;
use crate::memory::{Backing, Memory, PAGE_SIZE};
use crate::system::SystemConfig;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const MAGIC: &[u8; 8] = b"RSEMUSNP";
const VERSION: u32 = 1;
const HEADER_SIZE: usize = 4096;

/// File offset of the RAM image: a multiple of the page size of all
/// hosts, so that the image can be mapped.
pub const RAM_OFFSET: u64 = 1 << 20;

/// Longest write of consecutive RAM pages.
const MAX_RUN: usize = 4 << 20;

/// The snapshot file header.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Header {
    /// Set when the file holds a complete snapshot. It is cleared
    /// while the file is updated in place.
    complete: bool,
    ram_base: u64,
    ram_size: u64,
    state_offset: u64,
    state_len: u64,
    state_hash: u64,
}

impl Header {
    fn encode(&self) -> Vec<u8> {
        let mut b = vec![0u8; HEADER_SIZE];
        b[0..8].copy_from_slice(MAGIC);
        b[8..12].copy_from_slice(&VERSION.to_le_bytes());
        b[12..16].copy_from_slice(&(self.complete as u32).to_le_bytes());
        let fields = [
            RAM_OFFSET,
            self.ram_base,
            self.ram_size,
            self.state_offset,
            self.state_len,
            self.state_hash,
        ];
        for (i, v) in fields.iter().enumerate() {
            b[16 + i * 8..24 + i * 8].copy_from_slice(&v.to_le_bytes());
        }
        b
    }

    fn decode(b: &[u8]) -> Result<Header, String> {
        if b.len() < HEADER_SIZE || &b[0..8] != MAGIC {
            return Err("not a snapshot file".to_string());
        }
        let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let u64_at = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
        let version = u32_at(8);
        if version != VERSION {
            return Err(format!("unsupported snapshot version {}", version));
        }
        if u64_at(16) != RAM_OFFSET {
            return Err(format!("unsupported RAM offset {:x}", u64_at(16)));
        }
        Ok(Header {
            complete: u32_at(12) == 1,
            ram_base: u64_at(24),
            ram_size: u64_at(32),
            state_offset: u64_at(40),
            state_len: u64_at(48),
            state_hash: u64_at(56),
        })
    }
}

/// FNV-1a hash of the machine state.
fn hash(data: &[u8]) -> u64 {
    data.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ b as u64).wrapping_mul(0x100_0000_01b3)
    })
}

/// Serializes the machine state.
#[derive(Default)]
pub struct StateWriter {
    buf: Vec<u8>,
}

impl StateWriter {
    pub fn new() -> StateWriter {
        StateWriter::default()
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    pub fn bool(&mut self, v: bool) {
        self.buf.push(v as u8);
    }

    pub fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// Writes a length-prefixed byte string.
    pub fn bytes(&mut self, v: &[u8]) {
        self.u64(v.len() as u64);
        self.buf.extend_from_slice(v);
    }

    /// Writes a named section with the contents written by f. A
    /// section is read back with [`StateReader::section`], which checks
    /// the name and that the reader consumes the whole section.
    pub fn section<F>(&mut self, name: &str, f: F) -> Result<(), String>
    where
        F: FnOnce(&mut StateWriter) -> Result<(), String>,
    {
        self.bytes(name.as_bytes());
        let at = self.buf.len();
        self.u64(0);
        f(self)?;
        let len = (self.buf.len() - at - 8) as u64;
        self.buf[at..at + 8].copy_from_slice(&len.to_le_bytes());
        Ok(())
    }
}

/// Deserializes the machine state written by a [`StateWriter`].
pub struct StateReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> StateReader<'a> {
    pub fn new(buf: &'a [u8]) -> StateReader<'a> {
        StateReader { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        if self.buf.len() - self.pos < n {
            return Err("truncated snapshot state".to_string());
        }
        let b = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(b)
    }

    pub fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    pub fn bool(&mut self) -> Result<bool, String> {
        Ok(self.u8()? != 0)
    }

    pub fn u16(&mut self) -> Result<u16, String> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    pub fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub fn bytes(&mut self) -> Result<&'a [u8], String> {
        let n = self.u64()?;
        self.take(usize::try_from(n).map_err(|_| "truncated snapshot state")?)
    }

    /// Reads the section written by [`StateWriter::section`].
    pub fn section<T, F>(&mut self, name: &str, f: F) -> Result<T, String>
    where
        F: FnOnce(&mut StateReader<'a>) -> Result<T, String>,
    {
        let found = self.bytes()?;
        if found != name.as_bytes() {
            return Err(format!(
                "snapshot: expected {} state, found {}",
                name,
                String::from_utf8_lossy(found)
            ));
        }
        let data = self.bytes()?;
        let mut r = StateReader::new(data);
        let v = f(&mut r).map_err(|e| format!("{}: {}", name, e))?;
        if r.pos != data.len() {
            return Err(format!(
                "snapshot: {} bytes of unknown {} state",
                data.len() - r.pos,
                name
            ));
        }
        Ok(v)
    }
}

/// A disk image file attached to the VM. A restored guest expects the
/// disk contents it had when it was suspended.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskInfo {
    pub file: String,
    pub size: u64,
    /// Modification time in nanoseconds since the Unix epoch.
    pub mtime: i128,
}

impl DiskInfo {
    pub fn of(file: &str) -> Result<DiskInfo, String> {
        let md = std::fs::metadata(file).map_err(|e| format!("{}: {}", file, e))?;
        Ok(DiskInfo {
            file: file.to_string(),
            size: md.len(),
            mtime: md.mtime() as i128 * 1_000_000_000 + md.mtime_nsec() as i128,
        })
    }
}

/// The configuration of the suspended system.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Meta {
    pub config: SystemConfig,
    /// Disk image files given as command line arguments.
    pub args: Vec<String>,
    pub disks: Vec<DiskInfo>,
}

/// Statistics of a save.
#[derive(Clone, Copy, Debug, Default)]
pub struct SaveStats {
    /// The file was updated in place with the pages modified since
    /// the restore.
    pub incremental: bool,
    /// RAM pages written.
    pub pages: u64,
    pub time: Duration,
}

impl std::fmt::Display for SaveStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:.1} MiB of RAM{} in {:.1?}",
            (self.pages * PAGE_SIZE) as f64 / (1 << 20) as f64,
            if self.incremental {
                " modified since restore"
            } else {
                ""
            },
            self.time
        )
    }
}

/// A snapshot file opened for restoring.
pub struct Snapshot {
    pub path: PathBuf,
    file: File,
    /// The file was opened for writing.
    writable: bool,
    header: Header,
    state: Vec<u8>,
    pub meta: Meta,
}

impl Snapshot {
    /// Opens the snapshot file and reads the machine state. Keeps a
    /// shared lock on the file while the snapshot is open, so that the
    /// RAM image of a running VM is not modified.
    pub fn open(path: &Path) -> Result<Snapshot, String> {
        let err = |e: std::io::Error| format!("{}: {}", path.display(), e);
        let (file, writable) = match OpenOptions::new().read(true).write(true).open(path) {
            Ok(f) => (f, true),
            Err(_) => (File::open(path).map_err(err)?, false),
        };
        // SAFETY: flock on an open file.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH) } != 0 {
            return Err(err(std::io::Error::last_os_error()));
        }
        let mut hdr = vec![0u8; HEADER_SIZE];
        file.read_exact_at(&mut hdr, 0).map_err(err)?;
        let header = Header::decode(&hdr).map_err(|e| format!("{}: {}", path.display(), e))?;
        if !header.complete {
            return Err(format!(
                "{}: incomplete snapshot (the emulator stopped while saving it)",
                path.display()
            ));
        }
        let mut state = vec![0u8; header.state_len as usize];
        file.read_exact_at(&mut state, header.state_offset)
            .map_err(err)?;
        if hash(&state) != header.state_hash {
            return Err(format!("{}: corrupted snapshot state", path.display()));
        }
        let meta = StateReader::new(&state).section("meta", |r| {
            serde_json::from_slice::<Meta>(r.bytes()?).map_err(|e| e.to_string())
        })?;
        Ok(Snapshot {
            path: path.to_path_buf(),
            file,
            writable,
            header,
            state,
            meta,
        })
    }

    /// Returns the guest RAM, mapped copy-on-write from the file.
    pub fn memory(&self) -> Result<Memory, String> {
        Memory::from_file(
            self.header.ram_base,
            self.header.ram_size,
            &self.file,
            RAM_OFFSET,
        )
        .map_err(|e| format!("{}: mapping RAM: {}", self.path.display(), e))
    }

    /// Restores the CPU and device state. The CPU must have been
    /// created with the memory from [`Snapshot::memory`] and the
    /// devices of the saved configuration.
    pub fn restore(&self, cpu: &mut Cpu) -> Result<(), String> {
        let mut r = StateReader::new(&self.state);
        r.section("meta", |r| r.bytes().map(|_| ()))?;
        r.section("cpu", |r| cpu.load_state(r))?;
        let bus = cpu.bus.as_mut().ok_or("no system bus")?;
        r.section("devices", |r| bus.load_state(r))
    }

    /// Tests if the memory is the RAM image of this file.
    fn backs(&self, mem: &Memory) -> bool {
        mem.ram_base == self.header.ram_base
            && mem.size() == self.header.ram_size
            && matches!(mem.backing(), Backing::File { dev, ino }
                if self.file.metadata().is_ok_and(|m| m.dev() == dev && m.ino() == ino))
    }

    /// Tests if the path names this file.
    fn is_file(&self, path: &Path) -> bool {
        match (std::fs::metadata(path), self.file.metadata()) {
            (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
            _ => false,
        }
    }
}

/// Saves the system to the snapshot file at path. The CPU must be
/// stopped. The devices are quiesced first: they finish their
/// in-flight requests and stop taking input from the host. If the
/// system was restored from `origin` and path names the same file,
/// the file is updated in place.
pub fn save(
    path: &Path,
    cpu: &mut Cpu,
    meta: &Meta,
    origin: Option<&Snapshot>,
) -> Result<SaveStats, String> {
    let start = Instant::now();
    let bus = cpu.bus.as_mut().ok_or("no system bus")?;
    bus.quiesce()?;

    let mut w = StateWriter::new();
    w.section("meta", |w| {
        let json = serde_json::to_vec(meta).map_err(|e| e.to_string())?;
        w.bytes(&json);
        Ok(())
    })?;
    w.section("cpu", |w| {
        cpu.save_state(w);
        Ok(())
    })?;
    let bus = cpu.bus.as_mut().ok_or("no system bus")?;
    w.section("devices", |w| bus.save_state(w))?;
    let state = w.into_bytes();

    let mem = &cpu.mem;
    let mut header = Header {
        complete: true,
        ram_base: mem.ram_base,
        ram_size: mem.size(),
        state_offset: RAM_OFFSET + mem.size(),
        state_len: state.len() as u64,
        state_hash: hash(&state),
    };
    let err = |e: std::io::Error| format!("{}: {}", path.display(), e);

    if let Some(origin) = origin.filter(|o| o.writable && o.is_file(path) && o.backs(mem)) {
        // The other VMs with the file mapped hold shared locks.
        // SAFETY: flock on an open file.
        let fd = origin.file.as_raw_fd();
        let locked = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } == 0;
        if !locked {
            // A failed conversion may have dropped the shared lock.
            // SAFETY: flock on an open file.
            unsafe { libc::flock(fd, libc::LOCK_SH) };
        }
        if let (true, Some(pagemap)) = (locked, PageMap::read(mem)) {
            let file = &origin.file;
            header.complete = false;
            file.write_all_at(&header.encode(), 0).map_err(err)?;
            // SAFETY: the CPU and the devices are stopped.
            let ram = unsafe { mem.ram() };
            let pages = write_pages(file, ram, |page| pagemap.modified(page)).map_err(err)?;
            file.write_all_at(&state, header.state_offset)
                .map_err(err)?;
            file.set_len(header.state_offset + header.state_len)
                .map_err(err)?;
            header.complete = true;
            file.write_all_at(&header.encode(), 0).map_err(err)?;
            // SAFETY: flock on an open file.
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH) };
            return Ok(SaveStats {
                incremental: true,
                pages,
                time: start.elapsed(),
            });
        }
    }

    // Write a new file and replace the old one, which may be mapped by
    // this or another VM.
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".tmp{}", std::process::id()));
    let tmp = PathBuf::from(tmp);
    let result = (|| {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        // SAFETY: the CPU and the devices are stopped.
        let ram = unsafe { mem.ram() };
        let pagemap = PageMap::read(mem);
        let origin = origin.filter(|o| o.backs(mem));
        let (incremental, pages) = match (origin, &pagemap, mem.backing()) {
            // On filesystems that share data between files, the copy of
            // the origin needs only the modified pages.
            (Some(o), Some(pm), _) if clone_file(&file, &o.file) => {
                file.set_len(RAM_OFFSET + mem.size())?;
                (true, write_pages(&file, ram, |page| pm.modified(page))?)
            }
            // Anonymous pages that were never touched are zero.
            (_, Some(pm), Backing::Anonymous) => {
                file.set_len(RAM_OFFSET + mem.size())?;
                let pages = write_pages(&file, ram, |page| {
                    pm.touched(page) && !is_zero(page_data(ram, page))
                })?;
                (false, pages)
            }
            _ => {
                file.set_len(RAM_OFFSET + mem.size())?;
                let pages = write_pages(&file, ram, |page| !is_zero(page_data(ram, page)))?;
                (false, pages)
            }
        };
        file.write_all_at(&state, header.state_offset)?;
        file.write_all_at(&header.encode(), 0)?;
        std::fs::rename(&tmp, path)?;
        Ok((incremental, pages))
    })();
    match result {
        Ok((incremental, pages)) => Ok(SaveStats {
            incremental,
            pages,
            time: start.elapsed(),
        }),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(err(e))
        }
    }
}

/// Makes dst a copy of src that shares the data blocks with src (a
/// reflink). Returns false if the filesystem does not support it.
fn clone_file(dst: &File, src: &File) -> bool {
    // SAFETY: the ioctl takes the source descriptor as its argument.
    unsafe { libc::ioctl(dst.as_raw_fd(), libc::FICLONE, src.as_raw_fd()) == 0 }
}

fn page_data(ram: &[u8], page: usize) -> &[u8] {
    let ofs = page * PAGE_SIZE as usize;
    &ram[ofs..ofs + PAGE_SIZE as usize]
}

/// Tests if the page is all zeros.
fn is_zero(page: &[u8]) -> bool {
    // SAFETY: u128 has no invalid bit patterns.
    let (head, mid, tail) = unsafe { page.align_to::<u128>() };
    head.iter().all(|&b| b == 0)
        && tail.iter().all(|&b| b == 0)
        && mid.chunks(16).all(|c| c.iter().fold(0, |a, &v| a | v) == 0)
}

/// Writes the RAM pages selected by the predicate to their offsets in
/// the RAM image of the file, on several threads. Consecutive pages
/// are written together. Returns the number of pages written.
fn write_pages<F>(file: &File, ram: &[u8], select: F) -> std::io::Result<u64>
where
    F: Fn(usize) -> bool + Sync,
{
    let npages = ram.len() / PAGE_SIZE as usize;
    let threads = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(4)
        .min(npages.div_ceil(4096))
        .max(1);
    let per_thread = npages.div_ceil(threads);
    let write_range = |first: usize, last: usize| -> std::io::Result<u64> {
        let max_pages = MAX_RUN / PAGE_SIZE as usize;
        let mut written = 0u64;
        let mut run: Option<usize> = None;
        for page in first..=last {
            let selected = page < last && select(page);
            if let Some(start) = run {
                if !selected || page - start == max_pages {
                    let (a, b) = (start * PAGE_SIZE as usize, page * PAGE_SIZE as usize);
                    file.write_all_at(&ram[a..b], RAM_OFFSET + a as u64)?;
                    written += (page - start) as u64;
                    run = None;
                }
            }
            if selected && run.is_none() {
                run = Some(page);
            }
        }
        Ok(written)
    };
    std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|t| {
                let first = (t * per_thread).min(npages);
                let last = ((t + 1) * per_thread).min(npages);
                let write_range = &write_range;
                s.spawn(move || write_range(first, last))
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("snapshot writer panicked"))
            .sum()
    })
}

/// The host page table state of the guest RAM, from
/// /proc/self/pagemap.
struct PageMap {
    /// One entry per host page.
    entries: Vec<u64>,
    host_page: usize,
}

const PM_PRESENT: u64 = 1 << 63;
const PM_SWAP: u64 = 1 << 62;
const PM_FILE: u64 = 1 << 61;

impl PageMap {
    fn read(mem: &Memory) -> Option<PageMap> {
        // SAFETY: sysconf has no preconditions.
        let host_page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        if host_page == 0 || mem.size() == 0 {
            return None;
        }
        // SAFETY: the address of the RAM mapping, not dereferenced.
        let addr = unsafe { mem.ram() }.as_ptr() as usize;
        let n = (mem.size() as usize).div_ceil(host_page);
        let file = File::open("/proc/self/pagemap").ok()?;
        let mut buf = vec![0u8; n * 8];
        file.read_exact_at(&mut buf, (addr / host_page * 8) as u64)
            .ok()?;
        let entries = buf
            .as_chunks::<8>()
            .0
            .iter()
            .map(|c| u64::from_le_bytes(*c))
            .collect();
        Some(PageMap { entries, host_page })
    }

    /// Returns the entries of the host pages of the guest page.
    fn entries(&self, page: usize) -> &[u64] {
        let ofs = page * PAGE_SIZE as usize;
        let first = ofs / self.host_page;
        let last = (ofs + PAGE_SIZE as usize - 1) / self.host_page;
        &self.entries[first..=last]
    }

    /// Tests if the page has been accessed. Untouched anonymous pages
    /// are zero.
    fn touched(&self, page: usize) -> bool {
        self.entries(page)
            .iter()
            .any(|e| e & (PM_PRESENT | PM_SWAP) != 0)
    }

    /// Tests if the page of a private file mapping was modified: it is
    /// a private anonymous copy of the file page.
    fn modified(&self, page: usize) -> bool {
        self.entries(page)
            .iter()
            .any(|e| e & PM_SWAP != 0 || (e & PM_PRESENT != 0 && e & PM_FILE == 0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::RAM_BASE;
    use crate::system::Bus;
    use std::sync::Arc;

    fn machine(mem: Memory) -> Cpu {
        let mut cpu = Cpu::new(Arc::new(mem));
        cpu.bus = Some(Box::new(Bus::default()));
        cpu
    }

    fn fill(mem: &Memory, page: u64, v: u8) {
        mem.fill(page * PAGE_SIZE, PAGE_SIZE, v);
    }

    #[test]
    fn save_restore() {
        let path = std::env::temp_dir().join(format!("rsemu-test-{}.snap", std::process::id()));
        // 8192 pages, which are written by two threads.
        let mem = Memory::new(RAM_BASE, 32 << 20);
        let data = [0, 1, 4095, 4096, 5000, 8191];
        for p in data {
            fill(&mem, p, (p % 200) as u8 + 1);
        }
        // A page that was written but is zero again.
        fill(&mem, 7000, 9);
        fill(&mem, 7000, 0);
        let mut cpu = machine(mem);
        cpu.x[5] = 0x1234;
        cpu.f[3] = 0x5678;
        cpu.pc = 0x8020_0000;
        cpu.time = 77;

        let meta = Meta::default();
        let stats = save(&path, &mut cpu, &meta, None).unwrap();
        assert!(!stats.incremental);
        assert_eq!(stats.pages, data.len() as u64);

        let snap = Snapshot::open(&path).unwrap();
        let mut cpu2 = machine(snap.memory().unwrap());
        snap.restore(&mut cpu2).unwrap();
        // SAFETY: the CPUs are not running.
        assert!(unsafe { cpu.mem.ram() == cpu2.mem.ram() });
        assert_eq!(
            (cpu2.x[5], cpu2.f[3], cpu2.pc, cpu2.time),
            (0x1234, 0x5678, 0x8020_0000, 77)
        );

        // Suspending the restored system to its snapshot file writes
        // only the modified page.
        fill(&cpu2.mem, 7000, 0xaa);
        fill(&cpu.mem, 7000, 0xaa);
        let pagemap = PageMap::read(&cpu2.mem).is_some();
        let stats = save(&path, &mut cpu2, &meta, Some(&snap)).unwrap();
        if pagemap {
            assert!(stats.incremental);
            assert_eq!(stats.pages, 1);
        }
        drop(snap);
        let snap = Snapshot::open(&path).unwrap();
        let mem3 = snap.memory().unwrap();
        // SAFETY: the CPU is not running.
        assert!(unsafe { cpu.mem.ram() == mem3.ram() });
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn state_sections() {
        let mut w = StateWriter::new();
        w.section("a", |w| {
            w.u64(0x0102_0304_0506_0708);
            w.bool(true);
            w.bytes(b"xyz");
            Ok(())
        })
        .unwrap();
        w.section("b", |w| {
            w.u32(7);
            Ok(())
        })
        .unwrap();
        let data = w.into_bytes();

        let mut r = StateReader::new(&data);
        r.section("a", |r| {
            assert_eq!(r.u64()?, 0x0102_0304_0506_0708);
            assert!(r.bool()?);
            assert_eq!(r.bytes()?, b"xyz");
            Ok(())
        })
        .unwrap();
        // Reading less than the section holds is an error.
        let mut r2 = StateReader::new(&data);
        assert!(r2.section("a", |r| r.u64()).is_err());
        // So is a wrong section name.
        assert!(r.section("c", |r| r.u32()).is_err());
    }
}
