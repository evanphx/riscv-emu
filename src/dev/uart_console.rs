//! UART peer connected to the host terminal.

use super::{Uart, UartPeer};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::sync::Mutex;

/// Console peer reading the host stdin and writing to stdout.
pub struct UartConsole {
    pub cooked: bool,
    input: Mutex<VecDeque<u8>>,
    old_state: Mutex<Option<libc::termios>>,
}

impl UartConsole {
    pub fn new(cooked: bool) -> UartConsole {
        UartConsole {
            cooked,
            input: Mutex::new(VecDeque::new()),
            old_state: Mutex::new(None),
        }
    }

    fn make_raw(&self) {
        // SAFETY: termios calls on the stdin file descriptor.
        unsafe {
            let fd = libc::STDIN_FILENO;
            if libc::isatty(fd) == 0 {
                return;
            }
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut t) != 0 {
                eprintln!("tcgetattr: {}", std::io::Error::last_os_error());
                return;
            }
            *self.old_state.lock().unwrap() = Some(t);
            libc::cfmakeraw(&mut t);
            if libc::tcsetattr(fd, libc::TCSANOW, &t) != 0 {
                eprintln!("tcsetattr: {}", std::io::Error::last_os_error());
            }
        }
    }
}

impl UartPeer for UartConsole {
    fn run(&self, uart: Uart) {
        if !self.cooked {
            self.make_raw();
        }
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 256];
        loop {
            let n = match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            self.input.lock().unwrap().extend(&buf[..n]);
            uart.input_available();
        }
    }

    fn halt(&self) -> Result<(), String> {
        if let Some(t) = self.old_state.lock().unwrap().take() {
            // SAFETY: restores the saved terminal state.
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &t);
            }
        }
        Ok(())
    }

    fn receive(&self) -> (u8, bool) {
        let mut input = self.input.lock().unwrap();
        match input.pop_front() {
            Some(v) => (v, !input.is_empty()),
            None => (0xff, false),
        }
    }

    fn send(&self, b: u8) {
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(&[b]);
        let _ = out.flush();
    }
}
