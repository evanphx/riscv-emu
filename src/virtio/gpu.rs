//! VirtIO GPU device (stub, implemented separately).

use super::input::{InputHandle, InputType};
use super::mmio::VirtioMmio;
use crate::cpu::HartSignals;
use crate::dev::Plic;
use crate::memory::Memory;
use crate::system::Gpu as GpuConfig;
use std::sync::Arc;

pub const GPU_DEVICE_ID: u32 = 16;

/// Handle to the GPU display.
pub struct GpuHandle {
    pub width: u32,
    pub height: u32,
    pub pointer: InputType,
}

impl GpuHandle {
    pub fn set_keyboard(&mut self, _input: InputHandle) {}

    pub fn set_pointer(&mut self, _input: InputHandle) {}

    /// Runs the display event loop until the window is closed or the
    /// hart shuts down.
    pub fn event_loop(self, _signals: Arc<HartSignals>) {}
}

pub struct Gpu;

impl Gpu {
    /// Creates a new GPU device.
    pub fn create(
        _start: u64,
        _plic: Plic,
        _irq: u32,
        _mem: Arc<Memory>,
        _cfg: &GpuConfig,
    ) -> Result<(VirtioMmio, GpuHandle), String> {
        Err("virtio-gpu not implemented".to_string())
    }
}
