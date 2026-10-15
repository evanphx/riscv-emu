//! Linux TUN virtual interfaces.

use std::ffi::CStr;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::io::AsRawFd;
use std::process::Command;

/// Tunnel configuration.
#[derive(Clone, Debug, Default)]
pub struct Config {
    /// Local (host) address of the point-to-point link.
    pub local_ip: String,
    /// Remote (guest) address of the point-to-point link.
    pub remote_ip: String,
}

/// TUN interface carrying raw IP packets.
pub struct Tunnel {
    file: File,
    pub name: String,
}

impl Tunnel {
    /// Creates a new TUN interface.
    pub fn create() -> Result<Tunnel, String> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/net/tun")
            .map_err(|e| format!("tun module not present: {}", e))?;

        // SAFETY: ifreq is plain old data and the ioctl writes the
        // interface name into it.
        let name = unsafe {
            let mut ifr: libc::ifreq = std::mem::zeroed();
            ifr.ifr_ifru.ifru_flags = (libc::IFF_TUN | libc::IFF_NO_PI) as libc::c_short;
            if libc::ioctl(file.as_raw_fd(), libc::TUNSETIFF, &mut ifr) != 0 {
                return Err(io::Error::last_os_error().to_string());
            }
            CStr::from_ptr(ifr.ifr_name.as_ptr())
                .to_string_lossy()
                .into_owned()
        };
        Ok(Tunnel { file, name })
    }

    /// Configures the interface address and brings it up.
    pub fn configure(&self, cfg: &Config) -> Result<(), String> {
        let commands = [
            format!(
                "ip addr add {} peer {} dev {}",
                cfg.local_ip, cfg.remote_ip, self.name
            ),
            format!("ip link set dev {} up", self.name),
        ];
        for command in &commands {
            println!("Command: {}", command);
            let args: Vec<&str> = command.split(' ').collect();
            let status = Command::new(args[0])
                .args(&args[1..])
                .status()
                .map_err(|e| format!("{}: {}", args[0], e))?;
            if !status.success() {
                return Err(format!("{}: {}", command, status));
            }
        }
        Ok(())
    }

    /// Reads a raw IP packet from the interface.
    pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match (&self.file).read(buf) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                r => return r,
            }
        }
    }

    /// Writes a raw IP packet to the interface.
    pub fn write(&self, packet: &[u8]) -> io::Result<()> {
        (&self.file).write_all(packet)
    }
}
