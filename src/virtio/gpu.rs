//! VirtIO GPU device with a 2D scanout and blob resources.

use super::input::{InputHandle, InputType};
use super::mmio::*;
use crate::cpu::HartSignals;
use crate::dev::Plic;
use crate::isa::elf::hex_dump;
use crate::logger::{Level, Log};
use crate::memory::Memory;
use crate::system::Gpu as GpuConfig;
use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, Weak};

pub const GPU_DEVICE_ID: u32 = 16;
pub const GPU_SIZE: u64 = 4096;

// Feature bits.
/// Virgl 3D mode is supported.
pub const VIRTIO_GPU_F_VIRGL: u32 = 0;
/// EDID is supported.
pub const VIRTIO_GPU_F_EDID: u32 = 1;
/// Assigning resources UUIDs for export to other virtio devices is
/// supported.
pub const VIRTIO_GPU_F_RESOURCE_UUID: u32 = 2;
/// Creating and using size-based blob resources is supported.
pub const VIRTIO_GPU_F_RESOURCE_BLOB: u32 = 3;
/// Multiple context types and synchronization timelines supported.
pub const VIRTIO_GPU_F_CONTEXT_INIT: u32 = 4;

// Shared memory region IDs.
pub const VIRTIO_GPU_SHM_ID_UNDEFINED: u32 = 0;
pub const VIRTIO_GPU_SHM_ID_HOST_VISIBLE: u32 = 1;

// Control header flags.
pub const VIRTIO_GPU_FLAG_FENCE: u32 = 1 << 0;
pub const VIRTIO_GPU_FLAG_INFO_RING_IDX: u32 = 1 << 1;

// Blob resource flags.
pub const VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE: u32 = 0x0001;
pub const VIRTIO_GPU_BLOB_FLAG_USE_SHAREABLE: u32 = 0x0002;
pub const VIRTIO_GPU_BLOB_FLAG_USE_CROSS_DEVICE: u32 = 0x0004;

// Capability sets.
pub const VIRTIO_GPU_CAPSET_VIRGL: u32 = 1;
pub const VIRTIO_GPU_CAPSET_VIRGL2: u32 = 2;
pub const VIRTIO_GPU_CAPSET_GFXSTREAM: u32 = 3;
pub const VIRTIO_GPU_CAPSET_VENUS: u32 = 4;
pub const VIRTIO_GPU_CAPSET_CROSS_DOMAIN: u32 = 5;

/// Defines a u32 newtype with named constants that formats as the
/// constant name.
macro_rules! named_u32 {
    ($(#[$meta:meta])* $name:ident { $($cname:ident = $v:expr),* $(,)? }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub struct $name(pub u32);

        $(pub const $cname: $name = $name($v);)*

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                match *self {
                    $($cname => f.write_str(stringify!($cname)),)*
                    _ => write!(f, "{}({})", stringify!($name), self.0),
                }
            }
        }
    };
}

named_u32!(
    /// Control command and response types.
    GpuCtrlType {
        // 2D commands.
        VIRTIO_GPU_CMD_GET_DISPLAY_INFO = 0x0100,
        VIRTIO_GPU_CMD_RESOURCE_CREATE_2D = 0x0101,
        VIRTIO_GPU_CMD_RESOURCE_UNREF = 0x0102,
        VIRTIO_GPU_CMD_SET_SCANOUT = 0x0103,
        VIRTIO_GPU_CMD_RESOURCE_FLUSH = 0x0104,
        VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D = 0x0105,
        VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING = 0x0106,
        VIRTIO_GPU_CMD_RESOURCE_DETACH_BACKING = 0x0107,
        VIRTIO_GPU_CMD_GET_CAPSET_INFO = 0x0108,
        VIRTIO_GPU_CMD_GET_CAPSET = 0x0109,
        VIRTIO_GPU_CMD_GET_EDID = 0x010a,
        VIRTIO_GPU_CMD_RESOURCE_ASSIGN_UUID = 0x010b,
        VIRTIO_GPU_CMD_RESOURCE_CREATE_BLOB = 0x010c,
        VIRTIO_GPU_CMD_SET_SCANOUT_BLOB = 0x010d,
        // 3D commands.
        VIRTIO_GPU_CMD_CTX_CREATE = 0x0200,
        VIRTIO_GPU_CMD_CTX_DESTROY = 0x0201,
        VIRTIO_GPU_CMD_CTX_ATTACH_RESOURCE = 0x0202,
        VIRTIO_GPU_CMD_CTX_DETACH_RESOURCE = 0x0203,
        VIRTIO_GPU_CMD_RESOURCE_CREATE_3D = 0x0204,
        VIRTIO_GPU_CMD_TRANSFER_TO_HOST_3D = 0x0205,
        VIRTIO_GPU_CMD_TRANSFER_FROM_HOST_3D = 0x0206,
        VIRTIO_GPU_CMD_SUBMIT_3D = 0x0207,
        VIRTIO_GPU_CMD_RESOURCE_MAP_BLOB = 0x0208,
        VIRTIO_GPU_CMD_RESOURCE_UNMAP_BLOB = 0x0209,
        // Cursor commands.
        VIRTIO_GPU_CMD_UPDATE_CURSOR = 0x0300,
        VIRTIO_GPU_CMD_MOVE_CURSOR = 0x0301,
        // Success responses.
        VIRTIO_GPU_RESP_OK_NODATA = 0x1100,
        VIRTIO_GPU_RESP_OK_DISPLAY_INFO = 0x1101,
        VIRTIO_GPU_RESP_OK_CAPSET_INFO = 0x1102,
        VIRTIO_GPU_RESP_OK_CAPSET = 0x1103,
        VIRTIO_GPU_RESP_OK_EDID = 0x1104,
        VIRTIO_GPU_RESP_OK_RESOURCE_UUID = 0x1105,
        VIRTIO_GPU_RESP_OK_MAP_INFO = 0x1106,
        // Error responses.
        VIRTIO_GPU_RESP_ERR_UNSPEC = 0x1200,
        VIRTIO_GPU_RESP_ERR_OUT_OF_MEMORY = 0x1201,
        VIRTIO_GPU_RESP_ERR_INVALID_SCANOUT_ID = 0x1202,
        VIRTIO_GPU_RESP_ERR_INVALID_RESOURCE_ID = 0x1203,
        VIRTIO_GPU_RESP_ERR_INVALID_CONTEXT_ID = 0x1204,
        VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER = 0x1205,
    }
);

named_u32!(
    /// Pixel formats.
    GpuFormat {
        VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM = 1,
        VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM = 2,
        VIRTIO_GPU_FORMAT_A8R8G8B8_UNORM = 3,
        VIRTIO_GPU_FORMAT_X8R8G8B8_UNORM = 4,
        VIRTIO_GPU_FORMAT_R8G8B8A8_UNORM = 67,
        VIRTIO_GPU_FORMAT_X8B8G8R8_UNORM = 68,
        VIRTIO_GPU_FORMAT_A8B8G8R8_UNORM = 121,
        VIRTIO_GPU_FORMAT_R8G8B8X8_UNORM = 134,
    }
);

named_u32!(
    /// Blob resource memory types.
    GpuBlobMem {
        VIRTIO_GPU_BLOB_MEM_GUEST = 0x0001,
        VIRTIO_GPU_BLOB_MEM_HOST3D = 0x0002,
        VIRTIO_GPU_BLOB_MEM_HOST3D_GUEST = 0x0003,
    }
);

/// EDID 1.3 block advertised for the scanout.
pub const STANDARD_EDID: [u8; 128] = {
    let mut edid = [0u8; 128];
    let head: [u8; 24] = [
        0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00, // Header.
        0x4c, 0x2d, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // Vendor/Product ID.
        0x23, 0x24, // Week/Year of manufacture: 35, 36 + 1990 = 2026
        0x01, 0x03, // EDID Version 1.3.
        0x80, 0x10, 0x35, 0x78, // Basic display parameters.
    ];
    let mut i = 0;
    while i < head.len() {
        edid[i] = head[i];
        i += 1;
    }
    edid[127] = 0x05; // Checksum.
    edid
};

/// The GPU logo shown when the guest has not configured the display.
const LOGO_PNG: &[u8] = include_bytes!("../../resources/goemu-small.png");

fn le32(b: &[u8], ofs: usize) -> u32 {
    u32::from_le_bytes(b[ofs..ofs + 4].try_into().unwrap())
}

fn le64(b: &[u8], ofs: usize) -> u64 {
    u64::from_le_bytes(b[ofs..ofs + 8].try_into().unwrap())
}

fn put32(b: &mut [u8], ofs: usize, v: u32) {
    b[ofs..ofs + 4].copy_from_slice(&v.to_le_bytes());
}

fn put64(b: &mut [u8], ofs: usize, v: u64) {
    b[ofs..ofs + 8].copy_from_slice(&v.to_le_bytes());
}

/// A rectangle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GpuRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl GpuRect {
    fn decode(b: &[u8], ofs: usize) -> GpuRect {
        GpuRect {
            x: le32(b, ofs),
            y: le32(b, ofs + 4),
            width: le32(b, ofs + 8),
            height: le32(b, ofs + 12),
        }
    }
}

impl fmt::Display for GpuRect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{{x={}, y={}, w={}, h={}}}",
            self.x, self.y, self.width, self.height
        )
    }
}

/// struct virtio_gpu_ctrl_hdr.
#[derive(Clone, Copy, Debug)]
pub struct GpuCtrlHdr {
    pub typ: GpuCtrlType,
    pub flags: u32,
    pub fence_id: u64,
    pub ctx_id: u32,
    pub ring_idx: u8,
}

impl GpuCtrlHdr {
    fn decode(b: &[u8]) -> GpuCtrlHdr {
        GpuCtrlHdr {
            typ: GpuCtrlType(le32(b, 0)),
            flags: le32(b, 4),
            fence_id: le64(b, 8),
            ctx_id: le32(b, 16),
            ring_idx: b[20],
        }
    }

    /// Writes the response header into buf.
    fn response(&self, code: GpuCtrlType, buf: &mut [u8]) {
        if buf.len() < 24 {
            return;
        }
        put32(buf, 0, code.0);
        if self.flags & VIRTIO_GPU_FLAG_FENCE != 0 {
            put32(buf, 4, VIRTIO_GPU_FLAG_FENCE);
            put64(buf, 8, self.fence_id);
        }
    }
}

impl fmt::Display for GpuCtrlHdr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: flags={:x}, fenceID={}, ctxID={}, ringIdx={}",
            self.typ, self.flags, self.fence_id, self.ctx_id, self.ring_idx
        )
    }
}

/// Blob resource parameters.
#[derive(Clone, Copy, Debug)]
pub struct GpuBlob {
    pub mem: GpuBlobMem,
    pub flags: u32,
    pub id: u64,
    pub size: u64,
}

/// A GPU resource and its guest memory backing.
#[derive(Clone, Debug)]
pub struct GpuResource {
    pub id: u32,
    pub format: GpuFormat,
    pub width: u32,
    pub height: u32,
    /// Backing store as guest physical address ranges.
    pub pages: Vec<(u64, u64)>,
    pub blob: Option<GpuBlob>,
}

impl GpuResource {
    fn new(id: u32) -> GpuResource {
        GpuResource {
            id,
            format: GpuFormat(0),
            width: 0,
            height: 0,
            pages: Vec::new(),
            blob: None,
        }
    }

    /// Copies the backing store bytes starting at offset into buf.
    /// Returns the number of bytes copied, which is less than the
    /// buffer length if the backing store ends.
    fn read(&self, guest: &GuestMem, mut offset: u64, buf: &mut [u8]) -> Result<usize, String> {
        let mut pos = 0;
        for &(addr, len) in &self.pages {
            if pos == buf.len() {
                break;
            }
            if offset >= len {
                offset -= len;
                continue;
            }
            let n = ((len - offset) as usize).min(buf.len() - pos);
            guest.read(addr + offset, &mut buf[pos..pos + n])?;
            pos += n;
            offset = 0;
        }
        Ok(pos)
    }
}

/// The display shared between the GPU device and the host window.
pub struct GpuDisplay {
    pub width: u32,
    pub height: u32,
    /// Framebuffer pixels in B, G, R, A order. This is also the host
    /// visible shared memory region of the device.
    pub pixels: Arc<Mutex<Vec<u8>>>,
    dirty: AtomicBool,
    waker: Mutex<Option<Box<dyn Fn() + Send>>>,
}

impl GpuDisplay {
    fn new(width: u32, height: u32) -> GpuDisplay {
        GpuDisplay {
            width,
            height,
            pixels: Arc::new(Mutex::new(vec![0; width as usize * height as usize * 4])),
            dirty: AtomicBool::new(true),
            waker: Mutex::new(None),
        }
    }

    /// Sets the function that wakes up the display event loop.
    pub fn set_waker(&self, waker: Box<dyn Fn() + Send>) {
        *self.waker.lock().unwrap() = Some(waker);
    }

    /// Marks the frame dirty and wakes up the display event loop.
    fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::SeqCst);
        if let Some(w) = self.waker.lock().unwrap().as_ref() {
            w();
        }
    }

    /// Tests and clears the frame dirty flag.
    pub fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::SeqCst)
    }

    /// Clears the display white and draws the logo centered on it.
    fn draw_logo(&self, logo: &Logo) {
        let mut pix = self.pixels.lock().unwrap();
        pix.fill(0xff);
        let (w, h) = (self.width as i64, self.height as i64);
        let x0 = (w - logo.width as i64) / 2;
        let y0 = (h - logo.height as i64) / 2;
        for y in 0..logo.height as i64 {
            for x in 0..logo.width as i64 {
                let (dx, dy) = (x0 + x, y0 + y);
                if dx < 0 || dy < 0 || dx >= w || dy >= h {
                    continue;
                }
                let s = &logo.rgba[((y * logo.width as i64 + x) * 4) as usize..][..4];
                let d = &mut pix[((dy * w + dx) * 4) as usize..][..4];
                // Source over the destination, BGRA order.
                let a = s[3] as u32;
                for (di, si) in [(0, 2), (1, 1), (2, 0)] {
                    d[di] = ((s[si] as u32 * a + d[di] as u32 * (255 - a)) / 255) as u8;
                }
                d[3] = 0xff;
            }
        }
    }
}

/// A decoded RGBA image.
struct Logo {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

impl Logo {
    fn load() -> Result<Logo, String> {
        let mut decoder = png::Decoder::new(LOGO_PNG);
        decoder.set_transformations(png::Transformations::normalize_to_color8());
        let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
        let mut buf = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
        buf.truncate(info.buffer_size());
        let rgba = match info.color_type {
            png::ColorType::Rgba => buf,
            png::ColorType::Rgb => buf
                .chunks(3)
                .flat_map(|c| [c[0], c[1], c[2], 0xff])
                .collect(),
            t => return Err(format!("unsupported logo color type {:?}", t)),
        };
        Ok(Logo {
            width: info.width,
            height: info.height,
            rgba,
        })
    }
}

/// A guest buffer of a descriptor chain.
struct Buf {
    addr: u64,
    data: Vec<u8>,
    writable: bool,
}

/// The GPU state shared by the queue workers.
struct GpuCore {
    log: Log,
    guest: GuestMem,
    display: Arc<GpuDisplay>,
    logo: Logo,
    resources: HashMap<u32, GpuResource>,
}

type CmdResult = Result<u32, String>;

impl GpuCore {
    /// Processes the descriptor chain idx of the queue. Returns the
    /// number of bytes written to the guest buffers.
    fn process_desc(&mut self, vq: &Queue, idx: u16) -> CmdResult {
        let g = self.guest.clone();
        let mut desc = g.load_desc(vq, idx)?;
        crate::log_debug!(self.log, "desc: {}", desc);
        if desc.len < 24 {
            return Err(format!("truncated virtio_gpu_ctrl_hdr: {}", desc.len));
        }
        let mut chunk = vec![0u8; desc.len as usize];
        g.read(desc.addr, &mut chunk)?;
        if self.log.enabled(Level::Trace) {
            self.log
                .trace(format_args!("virtio_gpu_ctrl_hdr:\n{}", hex_dump(&chunk)));
        }
        let hdr = GpuCtrlHdr::decode(&chunk);
        crate::log_debug!(self.log, "hdr: {}", hdr);

        let mut bufs = Vec::new();
        while desc.flags & VIRTQ_DESC_F_NEXT != 0 {
            desc = g.load_desc(vq, desc.next)?;
            crate::log_debug!(self.log, "desc: {}", desc);
            let mut data = vec![0u8; desc.len as usize];
            g.read(desc.addr, &mut data)?;
            bufs.push(Buf {
                addr: desc.addr,
                data,
                writable: desc.flags & VIRTQ_DESC_F_WRITE != 0,
            });
        }

        let h = &hdr;
        let c = &chunk[..];
        let b = &mut bufs[..];
        let transferred = match hdr.typ {
            // 2D commands.
            VIRTIO_GPU_CMD_GET_DISPLAY_INFO => self.cmd_get_display_info(h, c, b),
            VIRTIO_GPU_CMD_RESOURCE_CREATE_2D => self.cmd_resource_create_2d(h, c, b),
            VIRTIO_GPU_CMD_RESOURCE_UNREF => self.cmd_resource_unref(h, c, b),
            VIRTIO_GPU_CMD_SET_SCANOUT => self.cmd_set_scanout(h, c, b),
            VIRTIO_GPU_CMD_RESOURCE_FLUSH => self.cmd_resource_flush(h, c, b),
            VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D => self.cmd_transfer_to_host_2d(h, c, b),
            VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING => self.cmd_resource_attach_backing(h, c, b),
            VIRTIO_GPU_CMD_GET_CAPSET_INFO => self.cmd_get_capset_info(h, c, b),
            VIRTIO_GPU_CMD_GET_CAPSET => self.cmd_get_capset(h, c, b),
            VIRTIO_GPU_CMD_GET_EDID => self.cmd_get_edid(h, c, b),
            VIRTIO_GPU_CMD_RESOURCE_CREATE_BLOB => self.cmd_resource_create_blob(h, c, b),
            VIRTIO_GPU_CMD_SET_SCANOUT_BLOB => self.cmd_set_scanout_blob(h, c, b),
            // 3D commands.
            VIRTIO_GPU_CMD_CTX_CREATE => self.cmd_ctx_create(h, c, b),
            VIRTIO_GPU_CMD_CTX_ATTACH_RESOURCE | VIRTIO_GPU_CMD_CTX_DETACH_RESOURCE => {
                self.cmd_ctx_resource(h, c, b)
            }
            // Cursor commands.
            VIRTIO_GPU_CMD_UPDATE_CURSOR | VIRTIO_GPU_CMD_MOVE_CURSOR => self.cmd_cursor(h, c, b),
            t => Err(format!("{}: not implemented yet", t)),
        }?;

        for buf in bufs.iter().filter(|b| b.writable) {
            g.write(buf.addr, &buf.data)?;
        }
        Ok(transferred)
    }

    /// Returns the single writable output buffer of the command.
    fn output<'a>(
        hdr: &GpuCtrlHdr,
        bufs: &'a mut [Buf],
        min: usize,
    ) -> Result<&'a mut [u8], String> {
        match bufs {
            [b] if b.writable && b.data.len() >= min => Ok(&mut b.data),
            _ => Err(format!("{}: invalid output buffers", hdr.typ)),
        }
    }

    fn check_len(hdr: &GpuCtrlHdr, req: &[u8], min: usize) -> Result<(), String> {
        if req.len() < min {
            return Err(format!("{}: truncated request", hdr.typ));
        }
        Ok(())
    }

    fn resource(&mut self, id: u32) -> Result<&mut GpuResource, String> {
        match self.resources.get_mut(&id) {
            Some(r) => Ok(r),
            None => Err(self.log.error(format_args!("unknown resource {}", id))),
        }
    }

    fn cmd_get_display_info(
        &mut self,
        hdr: &GpuCtrlHdr,
        _req: &[u8],
        bufs: &mut [Buf],
    ) -> CmdResult {
        let buf = Self::output(hdr, bufs, 48)?;
        hdr.response(VIRTIO_GPU_RESP_OK_DISPLAY_INFO, buf);

        // struct virtio_gpu_display_info for scanout 0.
        put32(buf, 24, 0); // x
        put32(buf, 28, 0); // y
        put32(buf, 32, self.display.width);
        put32(buf, 36, self.display.height);
        put32(buf, 40, 1); // enabled
        put32(buf, 44, 0); // flags
        Ok(48)
    }

    fn cmd_resource_create_2d(
        &mut self,
        hdr: &GpuCtrlHdr,
        req: &[u8],
        bufs: &mut [Buf],
    ) -> CmdResult {
        let buf = Self::output(hdr, bufs, 0)?;
        Self::check_len(hdr, req, 40)?;
        let resource = GpuResource {
            format: GpuFormat(le32(req, 28)),
            width: le32(req, 32),
            height: le32(req, 36),
            ..GpuResource::new(le32(req, 24))
        };
        self.log.info(format_args!(
            "{}: resourceID={}, format={}, width={}, height={}",
            hdr.typ, resource.id, resource.format, resource.width, resource.height
        ));
        self.resources.insert(resource.id, resource);
        hdr.response(VIRTIO_GPU_RESP_OK_NODATA, buf);
        Ok(24)
    }

    fn cmd_resource_unref(&mut self, hdr: &GpuCtrlHdr, req: &[u8], bufs: &mut [Buf]) -> CmdResult {
        let buf = Self::output(hdr, bufs, 0)?;
        Self::check_len(hdr, req, 32)?;
        let id = le32(req, 24);
        self.resource(id)?;
        self.resources.remove(&id);
        hdr.response(VIRTIO_GPU_RESP_OK_NODATA, buf);
        Ok(24)
    }

    fn cmd_set_scanout(&mut self, hdr: &GpuCtrlHdr, req: &[u8], bufs: &mut [Buf]) -> CmdResult {
        let buf = Self::output(hdr, bufs, 0)?;
        Self::check_len(hdr, req, 48)?;
        let rect = GpuRect::decode(req, 24);
        let scanout_id = le32(req, 40);
        let resource_id = le32(req, 44);
        crate::log_debug!(
            self.log,
            "setScanout rect={}, scanoutID={}, resourceID={}",
            rect,
            scanout_id,
            resource_id
        );
        if resource_id == 0 {
            crate::log_debug!(self.log, "disabling scanout");
        } else {
            self.resource(resource_id)?;
            crate::log_debug!(self.log, "enabling scanout {}", scanout_id);
        }
        hdr.response(VIRTIO_GPU_RESP_OK_NODATA, buf);
        Ok(24)
    }

    fn cmd_resource_flush(&mut self, hdr: &GpuCtrlHdr, req: &[u8], bufs: &mut [Buf]) -> CmdResult {
        let buf = Self::output(hdr, bufs, 0)?;
        Self::check_len(hdr, req, 48)?;
        let rect = GpuRect::decode(req, 24);
        let resource_id = le32(req, 40);
        crate::log_debug!(
            self.log,
            "cmdResourceFlush rect={}, resourceID={}",
            rect,
            resource_id
        );
        self.resource(resource_id)?;
        let r = &self.resources[&resource_id];
        crate::log_debug!(
            self.log,
            "resource: id={}, format={}, blob={:?}",
            r.id,
            r.format,
            r.blob
        );
        self.display.mark_dirty();
        hdr.response(VIRTIO_GPU_RESP_OK_NODATA, buf);
        Ok(24)
    }

    fn cmd_transfer_to_host_2d(
        &mut self,
        hdr: &GpuCtrlHdr,
        req: &[u8],
        bufs: &mut [Buf],
    ) -> CmdResult {
        let buf = Self::output(hdr, bufs, 0)?;
        Self::check_len(hdr, req, 56)?;
        let rect = GpuRect::decode(req, 24);
        let offset = le64(req, 40);
        let resource_id = le32(req, 48);
        crate::log_debug!(
            self.log,
            "transferToHost2D rect={}, offset={}, resourceID={}",
            rect,
            offset,
            resource_id
        );
        self.resource(resource_id)?;
        if let Err(e) = self.converter(rect, offset, &self.resources[&resource_id]) {
            self.log.error(format_args!("converter: {}", e));
        }
        hdr.response(VIRTIO_GPU_RESP_OK_NODATA, buf);
        Ok(24)
    }

    fn cmd_resource_attach_backing(
        &mut self,
        hdr: &GpuCtrlHdr,
        req: &[u8],
        bufs: &mut [Buf],
    ) -> CmdResult {
        if bufs.is_empty() {
            return Err(format!("{}: no buffers", hdr.typ));
        }
        Self::check_len(hdr, req, 32)?;
        let resource_id = le32(req, 24);
        let nr_entries = le32(req, 28);
        crate::log_debug!(
            self.log,
            "attach backing: resource={}, #entries={}",
            resource_id,
            nr_entries
        );
        self.resource(resource_id)?;
        let pages = self.collect_mem_entries(nr_entries, &req[32..], bufs)?;
        self.resource(resource_id)?.pages.extend(pages);

        let last = bufs.len() - 1;
        hdr.response(VIRTIO_GPU_RESP_OK_NODATA, &mut bufs[last].data);
        Ok(24)
    }

    fn cmd_get_capset_info(&mut self, hdr: &GpuCtrlHdr, req: &[u8], bufs: &mut [Buf]) -> CmdResult {
        let buf = Self::output(hdr, bufs, 40)?;
        Self::check_len(hdr, req, 28)?;
        let capset_index = le32(req, 24);
        crate::log_debug!(self.log, "getCapsetInfo: index={}", capset_index);
        hdr.response(VIRTIO_GPU_RESP_OK_CAPSET_INFO, buf);

        // struct virtio_gpu_resp_capset_info: capset_id,
        // capset_max_version, capset_max_size, padding.
        if capset_index != 0 {
            return Err(format!("invalid capset index {}", capset_index));
        }
        put32(buf, 24, VIRTIO_GPU_CAPSET_CROSS_DOMAIN);
        put32(buf, 28, 1); // capset_max_version
        put32(buf, 32, 16); // capset_max_size
        Ok(40)
    }

    fn cmd_get_capset(&mut self, hdr: &GpuCtrlHdr, req: &[u8], bufs: &mut [Buf]) -> CmdResult {
        let buf = Self::output(hdr, bufs, 40)?;
        Self::check_len(hdr, req, 32)?;
        crate::log_debug!(
            self.log,
            "getCapset: id={}, version={}",
            le32(req, 24),
            le32(req, 28)
        );
        hdr.response(VIRTIO_GPU_RESP_OK_CAPSET, buf);

        // struct CrossDomainCapabilities.
        put32(buf, 24, 1); // version
        put32(buf, 28, 0); // supported_channels (0 for basic shm)
        put32(buf, 32, 1); // supports_dmabuf
        put32(buf, 36, 1); // supports_external_gpu_memory
        Ok(40)
    }

    fn cmd_get_edid(&mut self, hdr: &GpuCtrlHdr, _req: &[u8], bufs: &mut [Buf]) -> CmdResult {
        let buf = Self::output(hdr, bufs, 24 + 8 + 128)?;
        hdr.response(VIRTIO_GPU_RESP_OK_EDID, buf);

        // struct virtio_gpu_resp_edid.
        put32(buf, 24, 128);
        put32(buf, 28, 0);
        buf[32..32 + 128].copy_from_slice(&STANDARD_EDID);
        Ok(24 + 8 + 128)
    }

    fn cmd_resource_create_blob(
        &mut self,
        hdr: &GpuCtrlHdr,
        req: &[u8],
        bufs: &mut [Buf],
    ) -> CmdResult {
        let writable: Vec<bool> = bufs.iter().map(|b| b.writable).collect();
        self.log.log(format_args!(
            "{}: len(bufs)={}, writable={:?}",
            hdr.typ,
            bufs.len(),
            writable
        ));
        Self::check_len(hdr, req, 56)?;
        if bufs.is_empty() {
            return Err(format!("{}: no buffers", hdr.typ));
        }
        let resource_id = le32(req, 24);
        let blob = GpuBlob {
            mem: GpuBlobMem(le32(req, 28)),
            flags: le32(req, 32),
            id: le64(req, 40),
            size: le64(req, 48),
        };
        let nr_entries = le32(req, 36);

        let flag_names = [
            (
                VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE,
                "VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE",
            ),
            (
                VIRTIO_GPU_BLOB_FLAG_USE_SHAREABLE,
                "VIRTIO_GPU_BLOB_FLAG_USE_SHAREABLE",
            ),
            (
                VIRTIO_GPU_BLOB_FLAG_USE_CROSS_DEVICE,
                "VIRTIO_GPU_BLOB_FLAG_USE_CROSS_DEVICE",
            ),
        ];
        let flags: Vec<&str> = flag_names
            .iter()
            .filter(|(f, _)| blob.flags & f != 0)
            .map(|(_, n)| *n)
            .collect();
        self.log.log(format_args!(" - ResourceID={}", resource_id));
        self.log.log(format_args!(" - BlobMem={}", blob.mem));
        self.log
            .log(format_args!(" - BlobFlags={}", flags.join("|")));
        self.log.log(format_args!(" - NREntries={}", nr_entries));
        self.log.log(format_args!(" - BlobID={}", blob.id));
        self.log.log(format_args!(" - Size={}", blob.size));

        if self.resources.contains_key(&resource_id) {
            return Err(self
                .log
                .error(format_args!("resource {} already defined", resource_id)));
        }
        let resource = GpuResource {
            pages: self.collect_mem_entries(nr_entries, &req[56..], bufs)?,
            blob: Some(blob),
            ..GpuResource::new(resource_id)
        };
        self.resources.insert(resource_id, resource);

        let last = bufs.len() - 1;
        hdr.response(VIRTIO_GPU_RESP_OK_NODATA, &mut bufs[last].data);
        Ok(24)
    }

    /// Collects the struct virtio_gpu_mem_entry entries from the
    /// request tail and the readable buffers. The last buffer is the
    /// writable response buffer.
    fn collect_mem_entries(
        &self,
        nr_entries: u32,
        hdr_tail: &[u8],
        bufs: &[Buf],
    ) -> Result<Vec<(u64, u64)>, String> {
        let mut input = hdr_tail.to_vec();
        for (idx, buf) in bufs.iter().enumerate() {
            if idx + 1 >= bufs.len() {
                if !buf.writable {
                    return Err("last buffer is not writable".to_string());
                }
            } else {
                if buf.writable {
                    return Err("descriptor buffer is writable".to_string());
                }
                input.extend_from_slice(&buf.data);
            }
        }
        // struct virtio_gpu_mem_entry: le64 addr, le32 length, le32
        // padding.
        if input.len() as u64 != nr_entries as u64 * 16 {
            return Err(format!(
                "invalid memory entries: len={}, nrEntries*16={}",
                input.len(),
                nr_entries as u64 * 16
            ));
        }
        input
            .chunks(16)
            .map(|e| {
                let (addr, len) = (le64(e, 0), le32(e, 8) as u64);
                if !self.guest.mem.contains_range(addr, len) {
                    return Err(format!("invalid addr {:x}", addr));
                }
                Ok((addr, len))
            })
            .collect()
    }

    fn cmd_set_scanout_blob(
        &mut self,
        hdr: &GpuCtrlHdr,
        req: &[u8],
        bufs: &mut [Buf],
    ) -> CmdResult {
        // struct virtio_gpu_set_scanout_blob {
        //        struct virtio_gpu_ctrl_hdr hdr;  //  0:  24 bytes
        //        struct virtio_gpu_rect r;        // 24:  16 bytes
        //        le32 scanout_id;                 // 40:   4 bytes
        //        le32 resource_id;                // 44:   4 bytes
        //        le32 width;                      // 48:   4 bytes
        //        le32 height;                     // 52:   4 bytes
        //        le32 format;                     // 56:   4 bytes
        //        le32 padding;                    // 60:   4 bytes
        //        le32 strides[4];                 // 64:   16 bytes
        //        le32 offsets[4];                 // 80:   16 bytes
        // };                                      // 96 bytes
        let buf = Self::output(hdr, bufs, 0)?;
        Self::check_len(hdr, req, 60)?;
        let rect = GpuRect::decode(req, 24);
        let scanout_id = le32(req, 40);
        let resource_id = le32(req, 44);
        let width = le32(req, 48);
        let height = le32(req, 52);
        let format = GpuFormat(le32(req, 56));
        crate::log_debug!(
            self.log,
            "setScanoutBlob: rect={}, scanoutID={}, resourceID={}",
            rect,
            scanout_id,
            resource_id
        );
        crate::log_debug!(
            self.log,
            "setScanoutBlob: width={}, height={}, format={}",
            width,
            height,
            format
        );
        if resource_id == 0 {
            crate::log_debug!(self.log, "disabling scanout");
        } else {
            let r = self.resource(resource_id)?;
            r.format = format;
            r.width = width;
            r.height = height;
            crate::log_debug!(self.log, "enabling scanout {}", scanout_id);
        }
        hdr.response(VIRTIO_GPU_RESP_OK_NODATA, buf);
        Ok(24)
    }

    fn cmd_ctx_create(&mut self, hdr: &GpuCtrlHdr, req: &[u8], bufs: &mut [Buf]) -> CmdResult {
        let buf = Self::output(hdr, bufs, 0)?;
        // struct virtio_gpu_ctx_create: hdr, le32 nlen, le32
        // context_init, char debug_name[64].
        Self::check_len(hdr, req, 96)?;
        let nlen = (le32(req, 24) as usize).min(64);
        let context_init = le32(req, 28);
        let name = String::from_utf8_lossy(&req[32..32 + nlen]);
        crate::log_debug!(
            self.log,
            "ctxCreate: ctxID={}, nlen={}, contextInit={:x}, name={}",
            hdr.ctx_id,
            nlen,
            context_init,
            name.trim_end_matches('\0')
        );
        hdr.response(VIRTIO_GPU_RESP_OK_NODATA, buf);
        Ok(24)
    }

    fn cmd_ctx_resource(&mut self, hdr: &GpuCtrlHdr, req: &[u8], bufs: &mut [Buf]) -> CmdResult {
        let buf = Self::output(hdr, bufs, 0)?;
        Self::check_len(hdr, req, 32)?;
        crate::log_debug!(
            self.log,
            "{}: ctxID={}, resourceID={}",
            hdr.typ,
            hdr.ctx_id,
            le32(req, 24)
        );
        hdr.response(VIRTIO_GPU_RESP_OK_NODATA, buf);
        Ok(24)
    }

    fn cmd_cursor(&mut self, hdr: &GpuCtrlHdr, _req: &[u8], bufs: &mut [Buf]) -> CmdResult {
        if let Some(b) = bufs.first_mut().filter(|b| b.writable) {
            hdr.response(VIRTIO_GPU_RESP_OK_NODATA, &mut b.data);
        }
        Ok(24)
    }

    /// Copies the rectangle of the resource into the framebuffer. The
    /// offset is the backing store offset of the rectangle's top-left
    /// corner, and rows are resource width × 4 bytes apart.
    fn converter(&self, rect: GpuRect, offset: u64, resource: &GpuResource) -> Result<(), String> {
        crate::log_debug!(self.log, "converter: rect: {}", rect);
        let swap = match resource.format {
            GpuFormat(0) => {
                crate::log_debug!(self.log, "converter: ignoring format {}", resource.format);
                return Ok(());
            }
            VIRTIO_GPU_FORMAT_R8G8B8X8_UNORM | VIRTIO_GPU_FORMAT_R8G8B8A8_UNORM => true,
            VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM | VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM => false,
            f => return Err(format!("format {} not supported", f)),
        };
        let (sw, sh) = (self.display.width as u64, self.display.height as u64);
        let (x, y) = (rect.x as u64, rect.y as u64);
        if x >= sw || y >= sh {
            return Ok(());
        }
        let width = (rect.width as u64).min(sw - x) as usize * 4;
        let height = (rect.height as u64).min(sh - y);
        let stride = resource.width as u64 * 4;

        let mut pix = self.display.pixels.lock().unwrap();
        for row in 0..height {
            let dst = (((y + row) * sw + x) * 4) as usize;
            let line = &mut pix[dst..dst + width];
            let n = resource.read(&self.guest, offset + row * stride, line)?;
            if swap {
                for p in line[..n].as_chunks_mut::<4>().0 {
                    p.swap(0, 2);
                    p[3] = 0xff;
                }
            }
            if n < width {
                return Err("transfer beyond backing store".to_string());
            }
        }
        Ok(())
    }
}

/// A GPU request handed to a queue worker.
struct Request {
    generation: u64,
    queue: Queue,
    head: u16,
}

/// VirtIO GPU device handler.
struct GpuHandler {
    core: Arc<Mutex<GpuCore>>,
    log: Log,
    /// Request channels of the control and cursor queue workers.
    txs: Vec<mpsc::Sender<Request>>,
    /// Number of control and cursor queue requests.
    events: Arc<[AtomicU64; 2]>,
}

impl Handler for GpuHandler {
    fn attach(&mut self, dev: Weak<Mutex<VirtioDevice>>) {
        for (q, name) in ["virtio-gpu-ctrl", "virtio-gpu-cursor"].iter().enumerate() {
            let (tx, rx) = mpsc::channel::<Request>();
            self.txs.push(tx);
            let core = self.core.clone();
            let log = self.log.clone();
            let events = self.events.clone();
            let dev = dev.clone();
            std::thread::Builder::new()
                .name(name.to_string())
                .spawn(move || {
                    for req in rx {
                        events[q].fetch_add(1, Ordering::Relaxed);
                        let tx = core
                            .lock()
                            .unwrap()
                            .process_desc(&req.queue, req.head)
                            .unwrap_or_else(|e| {
                                log.error(format_args!("process failed: {}", e));
                                0
                            });
                        let Some(dev) = dev.upgrade() else {
                            return;
                        };
                        let mut d = dev.lock().unwrap();
                        if d.state.generation == req.generation {
                            d.state.complete_descriptor(q, req.head, tx);
                        }
                    }
                })
                .expect("failed to start virtio-gpu worker");
        }
    }

    fn reset(&mut self, _st: &mut VirtioState) -> Result<(), String> {
        let mut core = self.core.lock().unwrap();
        core.resources.clear();
        core.display.draw_logo(&core.logo);
        core.display.mark_dirty();
        Ok(())
    }

    fn device_stats(&self, st: &VirtioState) {
        println!(
            "{}: control : {}",
            st.log.name,
            self.events[0].load(Ordering::Relaxed)
        );
        println!(
            "{}: cursor  : {}",
            st.log.name,
            self.events[1].load(Ordering::Relaxed)
        );
    }

    fn execute_descriptor_chain(
        &mut self,
        st: &mut VirtioState,
        q: usize,
        idx: u16,
    ) -> Result<u32, String> {
        crate::log_debug!(st.log, "vq{}: chain: idx={}", q, idx);
        let req = Request {
            generation: st.generation,
            queue: st.queues[q],
            head: idx,
        };
        if let Some(tx) = self.txs.get(q) {
            tx.send(req).map_err(|e| e.to_string())?;
        }
        Ok(0)
    }

    fn config_load(&mut self, st: &mut VirtioState, offset: u64, size: u64) -> Option<u64> {
        // 5.7.4 Device configuration layout.
        let v = match offset {
            0x100 => 0, // events_read
            0x104 => 0, // events_clear
            0x108 => 1, // num_scanouts
            0x10c => 1, // num_capsets
            _ => return None,
        };
        if size != 4 {
            return None;
        }
        crate::log_debug!(st.log, "Load32({})", gpu_reg(offset));
        Some(v)
    }

    fn config_store(&mut self, st: &mut VirtioState, offset: u64, size: u64, v: u64) -> bool {
        if size != 4 {
            return false;
        }
        match offset {
            0x100 | 0x108 | 0x10c => {
                st.log
                    .error(format_args!("read-only register 0x{:03x}", offset));
                true
            }
            0x104 => {
                crate::log_debug!(st.log, "Store32({}, {})", gpu_reg(offset), v);
                true
            }
            _ => false,
        }
    }
}

/// Returns the name of the GPU configuration register.
fn gpu_reg(offset: u64) -> String {
    let name = match offset {
        0x100 => "EventsRead",
        0x104 => "EventsClear",
        0x108 => "NumScanouts",
        0x10c => "NumCapsets",
        _ => "?",
    };
    format!("{}[0x{:03x}]", name, offset)
}

/// Handle to the GPU display.
pub struct GpuHandle {
    pub width: u32,
    pub height: u32,
    pub pointer: InputType,
    pub title: String,
    pub display: Arc<GpuDisplay>,
    pub keyboard: Option<InputHandle>,
    pub mouse: Option<InputHandle>,
}

impl GpuHandle {
    /// Sets the keyboard device receiving the display key events.
    pub fn set_keyboard(&mut self, input: InputHandle) {
        self.keyboard = Some(input);
    }

    /// Sets the pointer device receiving the display pointer events.
    pub fn set_pointer(&mut self, input: InputHandle) {
        self.mouse = Some(input);
    }

    /// Runs the display event loop until the window is closed or the
    /// hart shuts down. Without a display, waits for the shutdown.
    pub fn event_loop(self, signals: Arc<HartSignals>) {
        #[cfg(feature = "gui")]
        match super::window::run(self, signals.clone()) {
            Ok(()) => return,
            Err(e) => eprintln!("virtio-gpu: no display: {}", e),
        }
        while !signals.is_shutdown() {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
}

/// VirtIO GPU device.
pub struct Gpu;

impl Gpu {
    /// Creates a new GPU device with a cfg.width×cfg.height scanout
    /// and the pointer input type. Returns the device bus segment and
    /// the display handle.
    pub fn create(
        start: u64,
        plic: Plic,
        irq: u32,
        mem: Arc<Memory>,
        cfg: &GpuConfig,
        pointer: InputType,
    ) -> Result<(VirtioMmio, GpuHandle), String> {
        if cfg.width == 0 || cfg.height == 0 {
            return Err(format!("invalid GPU size {}x{}", cfg.width, cfg.height));
        }
        let logo = Logo::load().map_err(|e| format!("GPU logo: {}", e))?;
        let display = Arc::new(GpuDisplay::new(cfg.width, cfg.height));
        display.draw_logo(&logo);

        plic.register_irq(irq, "gpu");
        let mut state = VirtioState::new(
            "virtio-gpu",
            GPU_DEVICE_ID,
            1 << VIRTIO_GPU_F_VIRGL
                | 1 << VIRTIO_GPU_F_EDID
                | 1 << VIRTIO_GPU_F_RESOURCE_BLOB
                | 1 << VIRTIO_GPU_F_CONTEXT_INIT,
            start,
            GPU_SIZE,
            plic,
            irq,
            mem.clone(),
            2,
        );
        state.log.level = Level::Info;
        state
            .shm
            .insert(VIRTIO_GPU_SHM_ID_HOST_VISIBLE, display.pixels.clone());
        state
            .log
            .info(format_args!("screen: {}\u{d7}{}", cfg.width, cfg.height));

        let log = state.log.clone();
        let core = GpuCore {
            log: log.clone(),
            guest: GuestMem { mem },
            display: display.clone(),
            logo,
            resources: HashMap::new(),
        };
        let dev = VirtioDevice::new(
            state,
            Box::new(GpuHandler {
                core: Arc::new(Mutex::new(core)),
                log,
                txs: Vec::new(),
                events: Arc::new([AtomicU64::new(0), AtomicU64::new(0)]),
            }),
        );
        let handle = GpuHandle {
            width: cfg.width,
            height: cfg.height,
            pointer,
            title: cfg.title.clone(),
            display,
            keyboard: None,
            mouse: None,
        };
        Ok((VirtioMmio::new("virtio", dev), handle))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edid_checksum() {
        let sum = STANDARD_EDID.iter().fold(0u8, |s, &b| s.wrapping_add(b));
        let expected = STANDARD_EDID[..127]
            .iter()
            .fold(0u8, |s, &b| s.wrapping_add(b))
            .wrapping_neg();
        assert_eq!(
            sum, 0,
            "EDID checksum error: checksum=0x{:02x}, expected=0x{:02x}",
            STANDARD_EDID[127], expected
        );
    }

    #[test]
    fn ctrl_type_names() {
        assert_eq!(
            VIRTIO_GPU_CMD_RESOURCE_CREATE_BLOB.to_string(),
            "VIRTIO_GPU_CMD_RESOURCE_CREATE_BLOB"
        );
        assert_eq!(GpuCtrlType(0x42).to_string(), "GpuCtrlType(66)");
        assert_eq!(
            VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM.to_string(),
            "VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM"
        );
        assert_eq!(GpuBlobMem(1).to_string(), "VIRTIO_GPU_BLOB_MEM_GUEST");
    }

    fn core(width: u32, height: u32) -> GpuCore {
        let mem = Arc::new(Memory::new(0x8000_0000, 0x10_0000));
        GpuCore {
            log: Log::new("virtio-gpu", Level::Error),
            guest: GuestMem { mem },
            display: Arc::new(GpuDisplay::new(width, height)),
            logo: Logo::load().unwrap(),
            resources: HashMap::new(),
        }
    }

    #[test]
    fn logo() {
        let c = core(320, 300);
        c.display.draw_logo(&c.logo);
        let pix = c.display.pixels.lock().unwrap();
        // Corners are white, the logo is drawn in the middle.
        assert_eq!(&pix[0..4], &[0xff; 4]);
        assert!(pix.chunks(4).any(|p| p != [0xff; 4]));
    }

    #[test]
    fn transfer_rect() {
        let c = core(4, 3);
        // A 4×3 resource backed by two pages split mid-row.
        let g = c.guest.clone();
        let data: Vec<u8> = (0..48).collect();
        g.write(0x8000_0000, &data[..20]).unwrap();
        g.write(0x8000_1000, &data[20..]).unwrap();
        let mut r = GpuResource {
            format: VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM,
            width: 4,
            height: 3,
            pages: vec![(0x8000_0000, 20), (0x8000_1000, 28)],
            ..GpuResource::new(1)
        };
        // Rectangle x=1, y=1, 2×2 at offset 1*16 + 1*4.
        let rect = GpuRect {
            x: 1,
            y: 1,
            width: 2,
            height: 2,
        };
        c.converter(rect, 20, &r).unwrap();
        {
            let pix = c.display.pixels.lock().unwrap();
            assert_eq!(&pix[16..20], &[0; 4]);
            assert_eq!(&pix[20..28], &data[20..28]);
            assert_eq!(&pix[36..44], &data[36..44]);
            assert_eq!(&pix[44..48], &[0; 4]);
        }

        r.format = VIRTIO_GPU_FORMAT_R8G8B8X8_UNORM;
        let full = GpuRect {
            x: 0,
            y: 0,
            width: 4,
            height: 3,
        };
        c.converter(full, 0, &r).unwrap();
        let pix = c.display.pixels.lock().unwrap();
        assert_eq!(&pix[0..4], &[2, 1, 0, 0xff]);
        assert_eq!(&pix[44..48], &[46, 45, 44, 0xff]);
    }
}
