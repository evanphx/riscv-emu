//! Control socket of a running system (`-control`). A program
//! connects to the Unix socket and sends commands, one per line, each
//! answered with a line `OK [details]` or `ERR message`:
//!
//! - `checkpoint FILE` stops the CPU, saves the system to the snapshot
//!   FILE and leaves the CPU stopped. The devices keep running, but the
//!   guest does not, and its clock does not advance.
//! - `resume` continues a stopped CPU.
//! - `quit` stops the system like a power-off: the emulator saves its
//!   JIT code cache (`-jit-cache`) and exits.

use crate::cpu::HartSignals;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};

/// A checkpoint request for the CPU thread.
pub struct Checkpoint {
    pub path: PathBuf,
    reply: mpsc::Sender<Result<String, String>>,
}

impl Checkpoint {
    /// Answers the request with the result of the save.
    pub fn reply(self, r: Result<String, String>) {
        let _ = self.reply.send(r);
    }
}

pub struct Control {
    pending: Mutex<Option<Checkpoint>>,
    resume: Mutex<mpsc::Receiver<()>>,
    quit: AtomicBool,
}

impl Control {
    /// Creates the control socket at path and starts serving it.
    pub fn start(path: &str, signals: Arc<HartSignals>) -> Result<Arc<Control>, String> {
        if std::fs::symlink_metadata(path)
            .is_ok_and(|m| std::os::unix::fs::FileTypeExt::is_socket(&m.file_type()))
        {
            let _ = std::fs::remove_file(path);
        }
        let listener = UnixListener::bind(path).map_err(|e| format!("{}: {}", path, e))?;
        let (resume_tx, resume_rx) = mpsc::channel();
        let control = Arc::new(Control {
            pending: Mutex::new(None),
            resume: Mutex::new(resume_rx),
            quit: AtomicBool::new(false),
        });
        let c = control.clone();
        std::thread::Builder::new()
            .name("control".to_string())
            .spawn(move || {
                for stream in listener.incoming().flatten() {
                    c.serve(stream, &signals, &resume_tx);
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(control)
    }

    fn serve(&self, stream: UnixStream, signals: &HartSignals, resume: &mpsc::Sender<()>) {
        let Ok(mut out) = stream.try_clone() else {
            return;
        };
        for line in BufReader::new(stream).lines() {
            let Ok(line) = line else {
                return;
            };
            let reply = match line.trim().split_once(' ') {
                Some(("checkpoint", path)) => self.checkpoint(path.trim(), signals),
                None if line.trim() == "resume" => {
                    let _ = resume.send(());
                    Ok(String::new())
                }
                None if line.trim() == "quit" => {
                    self.quit.store(true, Ordering::SeqCst);
                    signals.shutdown();
                    // A CPU stopped at a checkpoint wakes up to quit.
                    let _ = resume.send(());
                    Ok(String::new())
                }
                _ => Err(format!("unknown command: {}", line.trim())),
            };
            let reply = match reply {
                Ok(s) if s.is_empty() => "OK\n".to_string(),
                Ok(s) => format!("OK {}\n", s),
                Err(e) => format!("ERR {}\n", e),
            };
            if out.write_all(reply.as_bytes()).is_err() {
                return;
            }
        }
    }

    /// Asks the CPU thread to save the system and waits for the result.
    fn checkpoint(&self, path: &str, signals: &HartSignals) -> Result<String, String> {
        let (tx, rx) = mpsc::channel();
        {
            let mut pending = self.pending.lock().unwrap();
            if pending.is_some() {
                return Err("a checkpoint is in progress".to_string());
            }
            *pending = Some(Checkpoint {
                path: PathBuf::from(path),
                reply: tx,
            });
        }
        signals.suspend();
        rx.recv()
            .unwrap_or_else(|_| Err("the system stopped".to_string()))
    }

    /// Returns the pending checkpoint request. Called by the stopped
    /// CPU thread on a suspend request.
    pub fn take(&self) -> Option<Checkpoint> {
        let cp = self.pending.lock().unwrap().take();
        if cp.is_some() {
            // Forget resume commands sent while the CPU was running.
            while self.resume.lock().unwrap().try_recv().is_ok() {}
        }
        cp
    }

    /// Blocks the CPU thread until a resume or quit command. Returns
    /// true to quit.
    pub fn wait_resume(&self) -> bool {
        if !self.quitting() {
            let _ = self.resume.lock().unwrap().recv();
        }
        self.quitting()
    }

    /// Tests if a quit command was received.
    pub fn quitting(&self) -> bool {
        self.quit.load(Ordering::SeqCst)
    }
}
