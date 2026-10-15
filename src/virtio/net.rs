//! VirtIO network device (stub, implemented separately).

use super::mmio::VirtioMmio;
use crate::dev::Plic;
use crate::memory::Memory;
use crate::system::Netdev;
use std::sync::Arc;

pub const NET_DEVICE_ID: u32 = 1;

pub struct Net;

impl Net {
    /// Creates a new network device and returns its bus segment.
    pub fn create(
        _start: u64,
        _plic: Plic,
        _irq: u32,
        _mem: Arc<Memory>,
        _netdev: &Netdev,
    ) -> Result<VirtioMmio, String> {
        Err("virtio-net not implemented".to_string())
    }
}
