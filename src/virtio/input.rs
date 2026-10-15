//! VirtIO input device (stub, implemented separately).

use super::mmio::VirtioMmio;
use crate::dev::Plic;
use crate::memory::Memory;
use std::sync::Arc;

pub const INPUT_DEVICE_ID: u32 = 18;

/// Input device types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputType {
    Keyboard,
    Mouse,
    Touchpad,
}

/// Handle to an input device for injecting events.
#[derive(Clone)]
pub struct InputHandle {
    mmio: VirtioMmio,
}

impl InputHandle {
    /// Returns the bus segment of the device.
    pub fn mmio(&self) -> VirtioMmio {
        self.mmio.clone()
    }
}

pub struct Input;

impl Input {
    /// Creates a new input device.
    pub fn create(
        _start: u64,
        _plic: Plic,
        _irq: u32,
        _mem: Arc<Memory>,
        _width: u32,
        _height: u32,
        _typ: InputType,
    ) -> Result<InputHandle, String> {
        Err("virtio-input not implemented".to_string())
    }
}
