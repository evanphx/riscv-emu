//! VirtIO entropy device.

use super::mmio::*;
use crate::dev::Plic;
use crate::memory::Memory;
use std::fs::File;
use std::io::Read;
use std::sync::Arc;

pub const RNG_DEVICE_ID: u32 = 4;
pub const RNG_SIZE: u64 = 0x100;

/// VirtIO RNG device reading the host /dev/urandom.
pub struct Rng {
    urandom: File,
    st_read: u64,
}

impl Rng {
    /// Creates a new RNG device and returns its bus segment.
    pub fn create(start: u64, plic: Plic, irq: u32, mem: Arc<Memory>) -> Result<VirtioMmio, String> {
        plic.register_irq(irq, "rng");
        let state = VirtioState::new(
            "virtio-rng",
            RNG_DEVICE_ID,
            0,
            start,
            RNG_SIZE,
            plic,
            irq,
            mem,
            1,
        );
        let urandom = File::open("/dev/urandom").map_err(|e| format!("/dev/urandom: {}", e))?;
        let dev = VirtioDevice::new(
            state,
            Box::new(Rng {
                urandom,
                st_read: 0,
            }),
        );
        Ok(VirtioMmio::new("virtio", dev))
    }
}

impl Handler for Rng {
    fn device_stats(&self, st: &VirtioState) {
        println!("{}: read    : {}", st.log.name, FileSize(self.st_read));
    }

    fn execute_descriptor_chain(
        &mut self,
        st: &mut VirtioState,
        q: usize,
        mut idx: u16,
    ) -> Result<u32, String> {
        let vq = st.queues[q];
        let mut transferred = 0u32;
        loop {
            let req = st.guest.load_desc(&vq, idx)?;
            if req.flags & VIRTQ_DESC_F_WRITE != 0 {
                let mut buf = vec![0u8; req.len as usize];
                self.urandom
                    .read_exact(&mut buf)
                    .map_err(|e| format!("rand read({}) failed: {}", req.len, e))?;
                st.guest.write(req.addr, &buf)?;
                self.st_read += buf.len() as u64;
                transferred += req.len;
            }
            if req.flags & VIRTQ_DESC_F_NEXT == 0 {
                break;
            }
            idx = req.next;
        }
        Ok(transferred)
    }
}
