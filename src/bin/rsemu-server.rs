//! rsemu-server: an HTTP API that runs shell commands in VMs and
//! continues them from snapshots.
//!
//! `POST /v1/exec` with `{"command": "..."}` restores the base snapshot
//! of a booted guest, runs the command in the guest's shell through
//! the vmagent program (guest/vmagent) over vsock, and streams the
//! output back as newline-delimited JSON. When the command has
//! finished, the VM is suspended to a new snapshot whose id is the
//! last event of the stream. Passing it as `"continuation"` in the
//! next request restores that VM, and the next command runs in the
//! same shell: its working directory, variables, files and background
//! processes are there. A continuation can be continued any number of
//! times; each run makes a new one.
//!
//! After a command, the VM is not stopped: it stays warm, paused right
//! after its snapshot, for `-warm-ttl` seconds. A request for its
//! continuation in that time resumes it instead of restoring the
//! snapshot. At most `-warm` VMs are kept; the one idle the longest
//! makes room for a new one. The snapshot stays, so the continuation
//! remains valid when its VM is in use or gone.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// The vsock port of vmagent.
const AGENT_PORT: u32 = 1024;
const GUEST_CID: u64 = 3;
/// Time for a restored VM to accept the agent connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Time for a booting VM to start the agent.
const BOOT_TIMEOUT: Duration = Duration::from_secs(300);
/// Time for a VM to answer a control command, such as writing a
/// snapshot.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(120);
/// Interval of checking that the client of a quiet command is still
/// connected.
const CLIENT_CHECK: Duration = Duration::from_millis(250);
/// Largest request body.
const MAX_BODY: usize = 16 << 20;

struct Server {
    rsemu: PathBuf,
    /// The system configuration.
    config: PathBuf,
    /// The directory the VMs run in, which the relative paths of the
    /// configuration are relative to: the current directory, as for
    /// rsemu.
    work_dir: PathBuf,
    snapshots: PathBuf,
    runs: PathBuf,
    jit: bool,
    jit_backend: String,
    default_timeout: Duration,
    next_run: AtomicU64,
    /// Most warm VMs kept, and how long they are kept.
    warm_max: usize,
    warm_ttl: Duration,
    /// Paused VMs by the continuation id of their state.
    warm: Mutex<Vec<WarmVm>>,
    spawner: Spawner,
    /// Command run in the base VM before its snapshot.
    warmup: Option<String>,
    /// The JIT code cache of the VMs, made by the base VM.
    jit_cache: PathBuf,
}

/// Starts the VM processes from one thread that lives as long as the
/// server: the processes are killed when the thread that started them
/// exits (PR_SET_PDEATHSIG), and a warm VM outlives its request.
struct Spawner {
    requests: Mutex<std::sync::mpsc::Sender<SpawnRequest>>,
}

type SpawnRequest = (Command, std::sync::mpsc::Sender<std::io::Result<Child>>);

impl Spawner {
    fn new() -> Spawner {
        let (tx, rx) = std::sync::mpsc::channel::<SpawnRequest>();
        std::thread::spawn(move || {
            for (mut cmd, reply) in rx {
                let _ = reply.send(cmd.spawn());
            }
        });
        Spawner {
            requests: Mutex::new(tx),
        }
    }

    fn spawn(&self, mut cmd: Command) -> std::io::Result<Child> {
        use std::os::unix::process::CommandExt;
        // SAFETY: prctl is async-signal-safe.
        unsafe {
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                Ok(())
            });
        }
        let (tx, rx) = std::sync::mpsc::channel();
        self.requests
            .lock()
            .unwrap()
            .send((cmd, tx))
            .expect("spawner thread");
        rx.recv().expect("spawner thread")
    }
}

struct WarmVm {
    /// The continuation id, or "base".
    id: String,
    vm: Vm,
    since: Instant,
}

/// Options of the server.
struct Options {
    listen: String,
    state: PathBuf,
    config: PathBuf,
    rsemu: PathBuf,
    jit: bool,
    jit_backend: String,
    timeout: u64,
    warm: usize,
    warm_ttl: f64,
    warmup: Option<String>,
}

fn usage() -> ! {
    eprintln!(
        "usage: rsemu-server -config FILE.goemu [-listen ADDR] [-state DIR] [-rsemu PATH]
                    [-timeout SECONDS] [-warm N] [-warm-ttl SECONDS]
                    [-warmup COMMAND] [-nojit] [-jit-backend cranelift|direct|direct-mmap]

  -config   system configuration with the vmagent initramfs
            (configs/vmagent.goemu); relative paths in it are relative
            to the current directory
  -listen   HTTP listen address (default 127.0.0.1:8080)
  -state    directory for snapshots and VM logs (default ./rsemu-server)
  -rsemu    emulator binary (default: rsemu next to this program)
  -timeout  default command timeout in seconds (default 600)
  -warm     number of paused VMs kept after their commands, ready to
            continue without a restore (default 4; 0 turns it off)
  -warm-ttl seconds a VM is kept warm (default 60)
  -warmup   shell command run in the base VM before its snapshot, so
            that the code it runs is in the JIT code cache that all
            VMs load, e.g. 'node -e 0; npm --version'
  -nojit    run the VMs without the JIT compiler"
    );
    std::process::exit(2);
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut opts = Options {
        listen: "127.0.0.1:8080".to_string(),
        state: PathBuf::from("rsemu-server"),
        config: PathBuf::new(),
        rsemu: std::env::current_exe()
            .expect("current executable")
            .with_file_name("rsemu"),
        jit: true,
        jit_backend: "cranelift".into(),
        timeout: 600,
        warm: 4,
        warm_ttl: 60.0,
        warmup: None,
    };
    while let Some(a) = args.next() {
        let mut value = || args.next().unwrap_or_else(|| usage());
        match a.trim_start_matches('-') {
            "listen" => opts.listen = value(),
            "state" => opts.state = PathBuf::from(value()),
            "config" => opts.config = PathBuf::from(value()),
            "rsemu" => opts.rsemu = PathBuf::from(value()),
            "jit-backend" => {
                opts.jit_backend = value();
                if !matches!(
                    opts.jit_backend.as_str(),
                    "cranelift" | "direct" | "direct-mmap"
                ) {
                    usage();
                }
            }
            "timeout" => opts.timeout = value().parse().unwrap_or_else(|_| usage()),
            "warm" => opts.warm = value().parse().unwrap_or_else(|_| usage()),
            "warm-ttl" => {
                opts.warm_ttl = value()
                    .parse()
                    .ok()
                    .filter(|t: &f64| *t >= 0.0)
                    .unwrap_or_else(|| usage())
            }
            "warmup" => opts.warmup = Some(value()),
            "nojit" => opts.jit = false,
            _ => usage(),
        }
    }
    if opts.config.as_os_str().is_empty() {
        usage();
    }
    if let Err(e) = run(opts) {
        eprintln!("rsemu-server: {}", e);
        std::process::exit(1);
    }
}

fn run(opts: Options) -> Result<(), String> {
    let abs = |p: &Path| std::fs::canonicalize(p).map_err(|e| format!("{}: {}", p.display(), e));
    let state = &opts.state;
    std::fs::create_dir_all(state).map_err(|e| format!("{}: {}", state.display(), e))?;
    let state = abs(state)?;
    let server = Arc::new(Server {
        rsemu: abs(&opts.rsemu)?,
        work_dir: std::env::current_dir().map_err(|e| e.to_string())?,
        config: abs(&opts.config)?,
        snapshots: state.join("snapshots"),
        runs: state.join("runs"),
        jit: opts.jit,
        jit_backend: opts.jit_backend,
        default_timeout: Duration::from_secs(opts.timeout),
        next_run: AtomicU64::new(0),
        warm_max: opts.warm,
        warm_ttl: Duration::from_secs_f64(opts.warm_ttl),
        warm: Mutex::new(Vec::new()),
        spawner: Spawner::new(),
        warmup: opts.warmup,
        jit_cache: state.join("jit.cache"),
    });
    for d in [&server.snapshots, &server.runs] {
        std::fs::create_dir_all(d).map_err(|e| format!("{}: {}", d.display(), e))?;
    }
    // Leftovers of VMs of an earlier server.
    if let Ok(entries) = std::fs::read_dir(&server.runs) {
        for e in entries.flatten() {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }

    if !server.base().exists() {
        eprintln!("rsemu-server: booting the base VM");
        let start = Instant::now();
        server.make_base()?;
        eprintln!(
            "rsemu-server: base snapshot {} in {:.1?}",
            server.base().display(),
            start.elapsed()
        );
    }

    let s = server.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(200));
        s.expire_warm();
    });

    let listen = &opts.listen;
    let listener = TcpListener::bind(listen).map_err(|e| format!("{}: {}", listen, e))?;
    eprintln!("rsemu-server: listening on http://{}", listen);
    for stream in listener.incoming() {
        let Ok(stream) = stream else {
            continue;
        };
        let server = server.clone();
        std::thread::spawn(move || {
            if let Err(e) = server.handle(stream) {
                eprintln!("rsemu-server: {}", e);
            }
        });
    }
    Ok(())
}

/// A VM process with the host end of its vsock device and its control
/// socket.
struct Vm {
    child: Child,
    dir: PathBuf,
    uds: PathBuf,
    control_path: PathBuf,
    control: Option<BufReader<UnixStream>>,
}

impl Vm {
    /// Waits for the agent of the VM and connects to it. Returns the
    /// connection after the agent's hello.
    fn connect(&mut self, timeout: Duration) -> Result<UnixStream, String> {
        let deadline = Instant::now() + timeout;
        let mut last_err = String::new();
        while Instant::now() < deadline {
            if let Ok(Some(status)) = self.child.try_wait() {
                return Err(format!("the VM stopped: {}{}", status, self.log_tail()));
            }
            match self.try_connect() {
                Ok(s) => return Ok(s),
                Err(e) => last_err = e,
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Err(format!("cannot connect to the guest agent: {}", last_err))
    }

    fn try_connect(&self) -> Result<UnixStream, String> {
        let mut s = UnixStream::connect(&self.uds).map_err(|e| e.to_string())?;
        s.set_read_timeout(Some(Duration::from_secs(10)))
            .map_err(|e| e.to_string())?;
        s.write_all(format!("CONNECT {}\n", AGENT_PORT).as_bytes())
            .map_err(|e| e.to_string())?;
        // The handshake line, read without buffering past it.
        let mut line = Vec::new();
        let mut b = [0u8; 1];
        loop {
            match s.read(&mut b) {
                Ok(1) if b[0] == b'\n' => break,
                Ok(1) => line.push(b[0]),
                // The guest refused: the agent does not listen yet.
                _ => return Err("connection refused".to_string()),
            }
        }
        if !line.starts_with(b"OK ") {
            return Err(String::from_utf8_lossy(&line).into_owned());
        }
        match read_frame(&mut s).map_err(|e| e.to_string())? {
            Some((b'H', _)) => Ok(s),
            Some((b'F', msg)) => Err(String::from_utf8_lossy(&msg).into_owned()),
            _ => Err("no hello from the agent".to_string()),
        }
    }

    /// Sends a command to the control socket of the emulator and
    /// returns the details of its OK answer.
    fn control(&mut self, cmd: &str) -> Result<String, String> {
        if self.control.is_none() {
            let deadline = Instant::now() + CONNECT_TIMEOUT;
            let s = loop {
                match UnixStream::connect(&self.control_path) {
                    Ok(s) => break s,
                    Err(_) if Instant::now() < deadline => {
                        if let Ok(Some(status)) = self.child.try_wait() {
                            return Err(format!("the VM stopped: {}{}", status, self.log_tail()));
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(e) => return Err(format!("control socket: {}", e)),
                }
            };
            s.set_read_timeout(Some(CONTROL_TIMEOUT))
                .map_err(|e| e.to_string())?;
            self.control = Some(BufReader::new(s));
        }
        let c = self.control.as_mut().unwrap();
        let mut line = String::new();
        let r = c
            .get_mut()
            .write_all(format!("{}\n", cmd).as_bytes())
            .and_then(|_| c.read_line(&mut line));
        match r {
            Ok(n) if n > 0 => {}
            _ => {
                self.control = None;
                return Err(format!("{}: the VM did not answer{}", cmd, self.log_tail()));
            }
        }
        let line = line.trim_end();
        match line.strip_prefix("OK") {
            Some(rest) => Ok(rest.trim().to_string()),
            None => Err(line.strip_prefix("ERR ").unwrap_or(line).to_string()),
        }
    }

    /// Saves the VM to the snapshot file. The VM stays paused.
    fn checkpoint(&mut self, path: &Path) -> Result<String, String> {
        self.control(&format!("checkpoint {}", path.display()))
    }

    /// Continues a paused VM.
    fn resume(&mut self) -> Result<(), String> {
        self.control("resume").map(|_| ())
    }

    fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// The last lines of the console output of a VM that stopped.
    fn log_tail(&mut self) -> String {
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            return String::new();
        }
        let log = std::fs::read(self.dir.join("console.log")).unwrap_or_default();
        let log = String::from_utf8_lossy(&log);
        let lines: Vec<&str> = log.lines().collect();
        let tail = lines[lines.len().saturating_sub(5)..].join("\n");
        if tail.is_empty() {
            tail
        } else {
            format!("\n{}", tail)
        }
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Reads an agent frame: the type and the payload. Returns None at
/// EOF.
fn read_frame(s: &mut UnixStream) -> std::io::Result<Option<(u8, Vec<u8>)>> {
    let mut hdr = [0u8; 5];
    match s.read_exact(&mut hdr) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(hdr[1..5].try_into().unwrap()) as usize;
    let mut data = vec![0u8; len];
    s.read_exact(&mut data)?;
    Ok(Some((hdr[0], data)))
}

/// Tests if s is a continuation id: 32 lowercase hex digits.
fn valid_id(s: &str) -> bool {
    s.len() == 32
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn new_id() -> String {
    let mut b = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .expect("/dev/urandom");
    b.iter().map(|x| format!("{:02x}", x)).collect()
}

impl Server {
    fn base(&self) -> PathBuf {
        self.snapshots.join("base.snap")
    }

    fn snapshot(&self, id: &str) -> PathBuf {
        self.snapshots.join(format!("{}.snap", id))
    }

    /// Starts a VM, restoring the snapshot file or booting if restore
    /// is None.
    fn start_vm(&self, restore: Option<&Path>) -> Result<Vm, String> {
        let n = self.next_run.fetch_add(1, Ordering::Relaxed);
        let dir = self.runs.join(n.to_string());
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {}", dir.display(), e))?;
        let uds = dir.join("v.sock");
        let control_path = dir.join("c.sock");
        if uds.as_os_str().len() >= 100 {
            return Err(format!("socket path {} is too long", uds.display()));
        }
        let log = std::fs::File::create(dir.join("console.log")).map_err(|e| e.to_string())?;
        let mut cmd = Command::new(&self.rsemu);
        cmd.current_dir(&self.work_dir)
            .stdin(Stdio::null())
            .stdout(log.try_clone().map_err(|e| e.to_string())?)
            .stderr(log);
        if self.jit {
            cmd.arg("-jit-cache").arg(&self.jit_cache);
            cmd.arg("-jit-backend").arg(&self.jit_backend);
        }
        cmd.arg("-cooked").arg("-control").arg(&control_path);
        match restore {
            Some(src) => {
                cmd.arg("-restore").arg(src).arg("-vsock-uds").arg(&uds);
            }
            None => {
                // The base VM compiles the code of the boot and the
                // warmup for the code cache that the other VMs load:
                // optimized regions, which take longer to compile but
                // run faster.
                if self.jit {
                    cmd.arg("-jit-opt");
                }
                cmd.arg("-device")
                    .arg(format!(
                        "virtio-vsock-device,cid={},uds={}",
                        GUEST_CID,
                        uds.display()
                    ))
                    .arg(&self.config);
            }
        }
        let child = self
            .spawner
            .spawn(cmd)
            .map_err(|e| format!("{}: {}", self.rsemu.display(), e))?;
        Ok(Vm {
            child,
            dir,
            uds,
            control_path,
            control: None,
        })
    }

    /// Boots the base VM, runs the warmup command, and saves the VM.
    /// With the JIT, the VM then exits and saves the code it compiled
    /// for the other VMs; without, it stays warm.
    fn make_base(&self) -> Result<(), String> {
        let mut vm = self.start_vm(None)?;
        let mut conn = vm.connect(BOOT_TIMEOUT)?;
        if let Some(cmd) = &self.warmup {
            let start = Instant::now();
            let (code, output) = run_quiet(&mut conn, cmd).map_err(|e| format!("warmup: {}", e))?;
            if code != 0 {
                return Err(format!(
                    "warmup exited with status {}:\n{}",
                    code,
                    String::from_utf8_lossy(&output)
                ));
            }
            eprintln!("rsemu-server: warmup in {:.1?}", start.elapsed());
        }
        drop(conn);
        vm.checkpoint(&self.base())?;
        if !self.jit {
            self.keep_warm("base", vm);
            return Ok(());
        }
        vm.control("quit")?;
        let deadline = Instant::now() + CONTROL_TIMEOUT;
        while vm.alive() {
            if Instant::now() >= deadline {
                return Err("the base VM did not exit".to_string());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    }

    /// Takes the warm VM of the continuation and resumes it.
    fn take_warm(&self, id: &str) -> Option<Vm> {
        let w = {
            let mut warm = self.warm.lock().unwrap();
            let i = warm.iter().position(|w| w.id == id)?;
            warm.swap_remove(i)
        };
        let mut vm = w.vm;
        if !vm.alive() {
            return None;
        }
        vm.resume().ok()?;
        Some(vm)
    }

    /// Keeps the paused VM warm for its continuation, making room by
    /// stopping the VM idle the longest.
    fn keep_warm(&self, id: &str, vm: Vm) {
        if self.warm_max == 0 || self.warm_ttl.is_zero() {
            return;
        }
        let evicted = {
            let mut warm = self.warm.lock().unwrap();
            let evicted = if warm.len() >= self.warm_max {
                let oldest = (0..warm.len()).min_by_key(|&i| warm[i].since).unwrap();
                Some(warm.remove(oldest))
            } else {
                None
            };
            warm.push(WarmVm {
                id: id.to_string(),
                vm,
                since: Instant::now(),
            });
            evicted
        };
        // Stopping the VM waits for the process.
        drop(evicted);
    }

    /// Stops the VMs that have been warm longer than the time limit.
    fn expire_warm(&self) {
        let expired: Vec<WarmVm> = {
            let mut warm = self.warm.lock().unwrap();
            let (expired, keep) = std::mem::take(&mut *warm)
                .into_iter()
                .partition(|w| w.since.elapsed() >= self.warm_ttl);
            *warm = keep;
            expired
        };
        drop(expired);
    }

    /// Stops the warm VM of the continuation. Returns true if there
    /// was one.
    fn drop_warm(&self, id: &str) -> bool {
        let w = {
            let mut warm = self.warm.lock().unwrap();
            warm.iter()
                .position(|w| w.id == id)
                .map(|i| warm.swap_remove(i))
        };
        w.is_some()
    }

    fn handle(&self, stream: TcpStream) -> Result<(), String> {
        let mut reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
        let mut out = stream;
        let req = match read_request(&mut reader, &mut out) {
            Ok(r) => r,
            Err((status, msg)) => return respond_error(&mut out, status, &msg),
        };
        let path = req.path.split('?').next().unwrap_or("");
        match (req.method.as_str(), path) {
            ("GET", "/v1/health") => {
                let warm = self.warm.lock().unwrap().len();
                respond_json(&mut out, 200, &json!({"status": "ok", "warm": warm}))
            }
            ("POST", "/v1/exec") => self.exec(&mut out, &req.body),
            ("DELETE", p) if p.starts_with("/v1/continuations/") => {
                let id = &p["/v1/continuations/".len()..];
                let warm = valid_id(id) && self.drop_warm(id);
                if valid_id(id) && (std::fs::remove_file(self.snapshot(id)).is_ok() || warm) {
                    respond_json(&mut out, 200, &json!({"deleted": id}))
                } else {
                    respond_error(&mut out, 404, "unknown continuation id")
                }
            }
            (_, "/v1/exec" | "/v1/health") => respond_error(&mut out, 405, "method not allowed"),
            _ => respond_error(&mut out, 404, "not found"),
        }
    }

    /// Runs a command and streams its output and the new continuation.
    fn exec(&self, out: &mut TcpStream, body: &[u8]) -> Result<(), String> {
        let req: Value = match serde_json::from_slice(body) {
            Ok(v) => v,
            Err(e) => return respond_error(out, 400, &format!("invalid JSON: {}", e)),
        };
        let Some(command) = req.get("command").and_then(|c| c.as_str()) else {
            return respond_error(out, 400, "\"command\" must be a string");
        };
        let (source_id, source) = match req.get("continuation") {
            None | Some(Value::Null) => ("base", self.base()),
            Some(Value::String(id)) => {
                let path = self.snapshot(id);
                if !valid_id(id) || !path.exists() {
                    return respond_error(out, 404, &format!("unknown continuation id: {}", id));
                }
                (id.as_str(), path)
            }
            Some(_) => return respond_error(out, 400, "\"continuation\" must be a string"),
        };
        let timeout = match req.get("timeout") {
            None | Some(Value::Null) => self.default_timeout,
            Some(t) => match t.as_f64().filter(|t| *t > 0.0) {
                Some(t) => Duration::from_secs_f64(t),
                None => return respond_error(out, 400, "\"timeout\" must be a positive number"),
            },
        };

        let id = new_id();
        let start = Instant::now();
        let (mut vm, warm) = match self.take_warm(source_id) {
            Some(vm) => (vm, true),
            None => match self.start_vm(Some(&source)) {
                Ok(vm) => (vm, false),
                Err(e) => return respond_error(out, 500, &e),
            },
        };
        let conn = match vm.connect(CONNECT_TIMEOUT) {
            Ok(c) => c,
            Err(e) => return respond_error(out, 500, &e),
        };
        eprintln!(
            "rsemu-server: {} -> {}: {} VM ready in {:.1?}",
            source_id,
            id,
            if warm { "warm" } else { "restored" },
            start.elapsed()
        );

        let head = "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\n\
                    Transfer-Encoding: chunked\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n";
        out.write_all(head.as_bytes()).map_err(|e| e.to_string())?;
        let mut events = EventWriter {
            out,
            carry: HashMap::new(),
        };
        // An error of the client connection ends the request: the VM
        // is stopped without a snapshot.
        let result = run_command(&mut events, conn, command, timeout);
        let event = match result {
            Ok(code) => {
                events.send(&json!({"type": "exit", "code": code}))?;
                match vm.checkpoint(&self.snapshot(&id)) {
                    Ok(_) => {
                        self.keep_warm(&id, vm);
                        json!({"type": "continuation", "id": id})
                    }
                    Err(e) => json!({"type": "error", "message": e}),
                }
            }
            Err(CommandError::Client(e)) => return Err(e),
            Err(CommandError::Vm(e)) => {
                let message = format!("{}{}", e, vm.log_tail());
                drop(vm);
                json!({"type": "error", "message": message})
            }
        };
        events.send(&event)?;
        events.finish()
    }
}

enum CommandError {
    /// The HTTP client went away.
    Client(String),
    /// The command did not finish in the VM.
    Vm(String),
}

/// Runs the command through the agent connection and returns its exit
/// status and output.
fn run_quiet(conn: &mut UnixStream, command: &str) -> Result<(i32, Vec<u8>), String> {
    let mut frame = vec![b'X'];
    frame.extend_from_slice(&(command.len() as u32).to_le_bytes());
    frame.extend_from_slice(command.as_bytes());
    conn.write_all(&frame).map_err(|e| e.to_string())?;
    conn.set_read_timeout(None).map_err(|e| e.to_string())?;
    let mut output = Vec::new();
    loop {
        match read_frame(conn).map_err(|e| e.to_string())? {
            Some((b'O' | b'E', data)) => output.extend_from_slice(&data),
            Some((b'R', data)) if data.len() == 4 => {
                return Ok((i32::from_le_bytes(data[..4].try_into().unwrap()), output))
            }
            Some((b'F', msg)) => return Err(String::from_utf8_lossy(&msg).into_owned()),
            _ => return Err("the agent closed the connection".to_string()),
        }
    }
}

/// Runs the command through the agent connection and streams its
/// output as events. Returns the exit status.
fn run_command(
    events: &mut EventWriter,
    mut conn: UnixStream,
    command: &str,
    timeout: Duration,
) -> Result<i32, CommandError> {
    let vm_err = |e: std::io::Error| CommandError::Vm(e.to_string());
    let mut frame = vec![b'X'];
    frame.extend_from_slice(&(command.len() as u32).to_le_bytes());
    frame.extend_from_slice(command.as_bytes());
    conn.write_all(&frame).map_err(vm_err)?;
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(CommandError::Vm(format!(
                "the command did not finish in {:?}",
                timeout
            )));
        }
        conn.set_read_timeout(Some(left.min(CLIENT_CHECK)))
            .map_err(vm_err)?;
        let (typ, data) = match read_frame(&mut conn) {
            Ok(Some(f)) => f,
            Ok(None) => return Err(CommandError::Vm("the VM closed the connection".into())),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                // A command without output does not notice that the
                // client went away from failing writes.
                if events.client_gone() {
                    return Err(CommandError::Client(
                        "the client closed the connection".into(),
                    ));
                }
                continue;
            }
            Err(e) => return Err(vm_err(e)),
        };
        match typ {
            b'O' => events
                .output("stdout", &data)
                .map_err(CommandError::Client)?,
            b'E' => events
                .output("stderr", &data)
                .map_err(CommandError::Client)?,
            b'R' if data.len() == 4 => {
                events.flush_carry().map_err(CommandError::Client)?;
                return Ok(i32::from_le_bytes(data[..4].try_into().unwrap()));
            }
            b'F' => {
                return Err(CommandError::Vm(format!(
                    "agent: {}",
                    String::from_utf8_lossy(&data)
                )))
            }
            t => {
                return Err(CommandError::Vm(format!(
                    "unexpected agent message {:?}",
                    t as char
                )))
            }
        }
    }
}

/// Writes JSON events as chunks of a chunked HTTP response.
struct EventWriter<'a> {
    out: &'a mut TcpStream,
    /// The incomplete UTF-8 sequence at the end of the last output of
    /// each stream.
    carry: HashMap<&'static str, Vec<u8>>,
}

impl EventWriter<'_> {
    fn send(&mut self, v: &Value) -> Result<(), String> {
        let mut line = serde_json::to_vec(v).map_err(|e| e.to_string())?;
        line.push(b'\n');
        let mut chunk = format!("{:x}\r\n", line.len()).into_bytes();
        chunk.extend_from_slice(&line);
        chunk.extend_from_slice(b"\r\n");
        self.out.write_all(&chunk).map_err(|e| e.to_string())
    }

    /// Tests if the client has closed the connection.
    fn client_gone(&mut self) -> bool {
        let mut b = [0u8; 1];
        if self.out.set_nonblocking(true).is_err() {
            return true;
        }
        let gone = match self.out.peek(&mut b) {
            Ok(n) => n == 0,
            Err(e) => e.kind() != std::io::ErrorKind::WouldBlock,
        };
        gone || self.out.set_nonblocking(false).is_err()
    }

    /// Sends output as a "data" string, or as "data_base64" if it is
    /// not UTF-8. A character split between two outputs is sent with
    /// the second.
    fn output(&mut self, stream: &'static str, data: &[u8]) -> Result<(), String> {
        let mut buf = self.carry.remove(stream).unwrap_or_default();
        buf.extend_from_slice(data);
        let (text, rest) = match std::str::from_utf8(&buf) {
            Ok(_) => (buf.len(), 0),
            // An incomplete character at the end.
            Err(e) if e.error_len().is_none() => (e.valid_up_to(), buf.len() - e.valid_up_to()),
            Err(_) => {
                return self.send(&json!({"type": stream, "data_base64": base64(&buf)}));
            }
        };
        if rest > 0 {
            self.carry.insert(stream, buf[text..].to_vec());
        }
        if text == 0 {
            return Ok(());
        }
        let s = std::str::from_utf8(&buf[..text]).unwrap();
        self.send(&json!({"type": stream, "data": s}))
    }

    /// Sends the incomplete characters left at the end of the output.
    fn flush_carry(&mut self) -> Result<(), String> {
        for (stream, data) in std::mem::take(&mut self.carry) {
            self.send(&json!({"type": stream, "data_base64": base64(&data)}))?;
        }
        Ok(())
    }

    fn finish(self) -> Result<(), String> {
        self.out.write_all(b"0\r\n\r\n").map_err(|e| e.to_string())
    }
}

fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                s.push(ALPHABET[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}

struct Request {
    method: String,
    path: String,
    body: Vec<u8>,
}

/// Reads an HTTP/1.1 request with an optional Content-Length body.
fn read_request(r: &mut impl BufRead, w: &mut impl Write) -> Result<Request, (u16, String)> {
    let bad = |m: &str| (400, m.to_string());
    let mut line = String::new();
    r.read_line(&mut line).map_err(|_| bad("bad request"))?;
    let mut parts = line.split_whitespace();
    let (Some(method), Some(path)) = (parts.next(), parts.next()) else {
        return Err(bad("bad request line"));
    };
    let (method, path) = (method.to_string(), path.to_string());
    let mut length = 0usize;
    let mut expect_continue = false;
    loop {
        let mut h = String::new();
        if r.read_line(&mut h).map_err(|_| bad("bad header"))? == 0 {
            return Err(bad("truncated request"));
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        let Some((name, value)) = h.split_once(':') else {
            return Err(bad("bad header"));
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            length = value.parse().map_err(|_| bad("bad Content-Length"))?;
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err((411, "chunked request bodies are not supported".to_string()));
        } else if name.eq_ignore_ascii_case("expect") && value.eq_ignore_ascii_case("100-continue")
        {
            expect_continue = true;
        }
    }
    if length > MAX_BODY {
        return Err((413, "request body too large".to_string()));
    }
    // curl asks before sending a large body.
    if expect_continue && length > 0 {
        let _ = w.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
    }
    let mut body = vec![0u8; length];
    r.read_exact(&mut body).map_err(|_| bad("truncated body"))?;
    Ok(Request { method, path, body })
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        411 => "Length Required",
        413 => "Payload Too Large",
        _ => "Internal Server Error",
    }
}

fn respond_json(out: &mut TcpStream, status: u16, v: &Value) -> Result<(), String> {
    let body = format!("{}\n", v);
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        status,
        reason(status),
        body.len()
    );
    out.write_all(head.as_bytes())
        .and_then(|_| out.write_all(body.as_bytes()))
        .map_err(|e| e.to_string())
}

fn respond_error(out: &mut TcpStream, status: u16, msg: &str) -> Result<(), String> {
    respond_json(out, status, &json!({"error": msg}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_encoding() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(&[0xff, 0xfe, 0x00, 0x80]), "//4AgA==");
    }
}
