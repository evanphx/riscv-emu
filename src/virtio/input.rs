//! VirtIO input device: keyboard, relative mouse, and absolute
//! touchpad.

use super::mmio::*;
use crate::dev::Plic;
use crate::isa::elf::hex_dump;
use crate::logger::Level;
use crate::memory::Memory;
use crate::system::PointerType;
use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Mutex};

pub const INPUT_DEVICE_ID: u32 = 18;
pub const INPUT_SIZE: u64 = 4096;

/// Input device types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputType {
    Keyboard,
    Mouse,
    Touchpad,
}

impl fmt::Display for InputType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            InputType::Keyboard => "keyboard",
            InputType::Mouse => "mouse",
            InputType::Touchpad => "touchpad",
        })
    }
}

impl From<PointerType> for InputType {
    fn from(p: PointerType) -> InputType {
        match p {
            PointerType::Mouse => InputType::Mouse,
            PointerType::Touchpad => InputType::Touchpad,
        }
    }
}

/// Key event values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyAction {
    Release = 0,
    Press = 1,
    Repeat = 2,
}

// Event types.
pub const EV_SYN: u16 = 0x00;
pub const EV_KEY: u16 = 0x01;
pub const EV_REL: u16 = 0x02;
pub const EV_ABS: u16 = 0x03;

pub const SYN_REPORT: u16 = 0;

// Relative axes.
pub const REL_X: u16 = 0x00;
pub const REL_Y: u16 = 0x01;
pub const REL_HWHEEL: u16 = 0x06;
pub const REL_WHEEL: u16 = 0x08;

// Absolute axes.
pub const ABS_X: u16 = 0x00;
pub const ABS_Y: u16 = 0x01;

pub const BUS_VIRTUAL: u8 = 0x06;

// Linux input key codes.
pub const KEY_ESC: u16 = 1;
pub const KEY_1: u16 = 2;
pub const KEY_2: u16 = 3;
pub const KEY_3: u16 = 4;
pub const KEY_4: u16 = 5;
pub const KEY_5: u16 = 6;
pub const KEY_6: u16 = 7;
pub const KEY_7: u16 = 8;
pub const KEY_8: u16 = 9;
pub const KEY_9: u16 = 10;
pub const KEY_0: u16 = 11;
pub const KEY_MINUS: u16 = 12;
pub const KEY_EQUAL: u16 = 13;
pub const KEY_BACKSPACE: u16 = 14;
pub const KEY_TAB: u16 = 15;
pub const KEY_Q: u16 = 16;
pub const KEY_W: u16 = 17;
pub const KEY_E: u16 = 18;
pub const KEY_R: u16 = 19;
pub const KEY_T: u16 = 20;
pub const KEY_Y: u16 = 21;
pub const KEY_U: u16 = 22;
pub const KEY_I: u16 = 23;
pub const KEY_O: u16 = 24;
pub const KEY_P: u16 = 25;
pub const KEY_LEFTBRACE: u16 = 26;
pub const KEY_RIGHTBRACE: u16 = 27;
pub const KEY_ENTER: u16 = 28;
pub const KEY_LEFTCTRL: u16 = 29;
pub const KEY_A: u16 = 30;
pub const KEY_S: u16 = 31;
pub const KEY_D: u16 = 32;
pub const KEY_F: u16 = 33;
pub const KEY_G: u16 = 34;
pub const KEY_H: u16 = 35;
pub const KEY_J: u16 = 36;
pub const KEY_K: u16 = 37;
pub const KEY_L: u16 = 38;
pub const KEY_SEMICOLON: u16 = 39;
pub const KEY_APOSTROPHE: u16 = 40;
pub const KEY_GRAVE: u16 = 41;
pub const KEY_LEFTSHIFT: u16 = 42;
pub const KEY_BACKSLASH: u16 = 43;
pub const KEY_Z: u16 = 44;
pub const KEY_X: u16 = 45;
pub const KEY_C: u16 = 46;
pub const KEY_V: u16 = 47;
pub const KEY_B: u16 = 48;
pub const KEY_N: u16 = 49;
pub const KEY_M: u16 = 50;
pub const KEY_COMMA: u16 = 51;
pub const KEY_DOT: u16 = 52;
pub const KEY_SLASH: u16 = 53;
pub const KEY_RIGHTSHIFT: u16 = 54;
pub const KEY_KPASTERISK: u16 = 55;
pub const KEY_LEFTALT: u16 = 56;
pub const KEY_SPACE: u16 = 57;
pub const KEY_CAPSLOCK: u16 = 58;
pub const KEY_F1: u16 = 59;
pub const KEY_F2: u16 = 60;
pub const KEY_F3: u16 = 61;
pub const KEY_F4: u16 = 62;
pub const KEY_F5: u16 = 63;
pub const KEY_F6: u16 = 64;
pub const KEY_F7: u16 = 65;
pub const KEY_F8: u16 = 66;
pub const KEY_F9: u16 = 67;
pub const KEY_F10: u16 = 68;
pub const KEY_NUMLOCK: u16 = 69;
pub const KEY_SCROLLLOCK: u16 = 70;
pub const KEY_KP7: u16 = 71;
pub const KEY_KP8: u16 = 72;
pub const KEY_KP9: u16 = 73;
pub const KEY_KPMINUS: u16 = 74;
pub const KEY_KP4: u16 = 75;
pub const KEY_KP5: u16 = 76;
pub const KEY_KP6: u16 = 77;
pub const KEY_KPPLUS: u16 = 78;
pub const KEY_KP1: u16 = 79;
pub const KEY_KP2: u16 = 80;
pub const KEY_KP3: u16 = 81;
pub const KEY_KP0: u16 = 82;
pub const KEY_KPDOT: u16 = 83;
pub const KEY_F11: u16 = 87;
pub const KEY_F12: u16 = 88;
pub const KEY_KPENTER: u16 = 96;
pub const KEY_RIGHTCTRL: u16 = 97;
pub const KEY_KPSLASH: u16 = 98;
pub const KEY_RIGHTALT: u16 = 100;
pub const KEY_HOME: u16 = 102;
pub const KEY_UP: u16 = 103;
pub const KEY_PAGEUP: u16 = 104;
pub const KEY_LEFT: u16 = 105;
pub const KEY_RIGHT: u16 = 106;
pub const KEY_END: u16 = 107;
pub const KEY_DOWN: u16 = 108;
pub const KEY_PAGEDOWN: u16 = 109;
pub const KEY_INSERT: u16 = 110;
pub const KEY_DELETE: u16 = 111;
pub const KEY_KPEQUAL: u16 = 117;
pub const KEY_PAUSE: u16 = 119;
pub const KEY_LEFTMETA: u16 = 125;
pub const KEY_RIGHTMETA: u16 = 126;
pub const KEY_MENU: u16 = 139;
pub const KEY_F13: u16 = 183;
pub const KEY_F14: u16 = 184;
pub const KEY_F15: u16 = 185;
pub const KEY_F16: u16 = 186;
pub const KEY_F17: u16 = 187;
pub const KEY_F18: u16 = 188;
pub const KEY_F19: u16 = 189;
pub const KEY_F20: u16 = 190;
pub const KEY_F21: u16 = 191;
pub const KEY_F22: u16 = 192;
pub const KEY_F23: u16 = 193;
pub const KEY_F24: u16 = 194;
pub const KEY_PRINT: u16 = 210;
pub const KEY_UNKNOWN: u16 = 240;

// Mouse buttons.
pub const BTN_LEFT: u16 = 0x110;
pub const BTN_RIGHT: u16 = 0x111;
pub const BTN_MIDDLE: u16 = 0x112;

/// The keys the host display can generate; the keyboard device
/// advertises them in its EV_KEY bitmap.
#[rustfmt::skip]
pub const KEYBOARD_KEYS: &[u16] = &[
    KEY_UNKNOWN, KEY_SPACE, KEY_APOSTROPHE, KEY_COMMA, KEY_MINUS, KEY_DOT, KEY_SLASH, KEY_0,
    KEY_1, KEY_2, KEY_3, KEY_4, KEY_5, KEY_6, KEY_7, KEY_8, KEY_9, KEY_SEMICOLON, KEY_EQUAL,
    KEY_A, KEY_B, KEY_C, KEY_D, KEY_E, KEY_F, KEY_G, KEY_H, KEY_I, KEY_J, KEY_K, KEY_L, KEY_M,
    KEY_N, KEY_O, KEY_P, KEY_Q, KEY_R, KEY_S, KEY_T, KEY_U, KEY_V, KEY_W, KEY_X, KEY_Y, KEY_Z,
    KEY_LEFTBRACE, KEY_BACKSLASH, KEY_RIGHTBRACE, KEY_GRAVE, KEY_ESC, KEY_ENTER, KEY_TAB,
    KEY_BACKSPACE, KEY_INSERT, KEY_DELETE, KEY_RIGHT, KEY_LEFT, KEY_DOWN, KEY_UP, KEY_PAGEUP,
    KEY_PAGEDOWN, KEY_HOME, KEY_END, KEY_CAPSLOCK, KEY_SCROLLLOCK, KEY_NUMLOCK, KEY_PRINT,
    KEY_PAUSE, KEY_F1, KEY_F2, KEY_F3, KEY_F4, KEY_F5, KEY_F6, KEY_F7, KEY_F8, KEY_F9, KEY_F10,
    KEY_F11, KEY_F12, KEY_F13, KEY_F14, KEY_F15, KEY_F16, KEY_F17, KEY_F18, KEY_F19, KEY_F20,
    KEY_F21, KEY_F22, KEY_F23, KEY_F24, KEY_KP0, KEY_KP1, KEY_KP2, KEY_KP3, KEY_KP4, KEY_KP5,
    KEY_KP6, KEY_KP7, KEY_KP8, KEY_KP9, KEY_KPDOT, KEY_KPSLASH, KEY_KPASTERISK, KEY_KPMINUS,
    KEY_KPPLUS, KEY_KPENTER, KEY_KPEQUAL, KEY_LEFTSHIFT, KEY_LEFTCTRL, KEY_LEFTALT,
    KEY_LEFTMETA, KEY_RIGHTSHIFT, KEY_RIGHTCTRL, KEY_RIGHTALT, KEY_RIGHTMETA, KEY_MENU,
];

/// The mouse buttons the host display can generate.
pub const MOUSE_BUTTONS: &[u16] = &[BTN_LEFT, BTN_RIGHT, BTN_MIDDLE];

/// Maximum number of pending events. Further events are dropped
/// until the driver provides event buffers.
const MAX_PENDING_EVENTS: usize = QUEUE_NUM_MAX as usize;

/// Returns a 1024 bit bitmap with the codes set.
fn bitmap(codes: &[u16]) -> Vec<u8> {
    let mut bm = vec![0u8; 128];
    for &c in codes {
        bm[c as usize / 8] |= 1 << (c % 8);
    }
    bm
}

/// Returns struct virtio_input_absinfo with the max value.
fn abs_info(max: u32) -> Vec<u8> {
    let mut info = vec![0u8; 20];
    info[4..8].copy_from_slice(&max.to_le_bytes());
    info
}

/// An input event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct InputEvent {
    typ: u16,
    code: u16,
    value: u32,
}

/// The device configuration space contents.
struct InputConfig {
    dev_ids: Vec<u8>,
    prop_bitmap: Vec<u8>,
    cat_bitmap: Vec<u8>,
    key_bitmap: Vec<u8>,
    rel_bitmap: Vec<u8>,
    abs_bitmap: Vec<u8>,
    abs_x: Vec<u8>,
    abs_y: Vec<u8>,
}

impl InputConfig {
    fn new(typ: InputType, width: u32, height: u32) -> InputConfig {
        let mut cfg = InputConfig {
            // struct virtio_input_devids: bustype, vendor, product,
            // version.
            dev_ids: vec![BUS_VIRTUAL, 0x00, 0x01, 0x00, 0x01, 0x00, 0x01, 0x00],
            prop_bitmap: Vec::new(),
            cat_bitmap: Vec::new(),
            key_bitmap: Vec::new(),
            rel_bitmap: Vec::new(),
            abs_bitmap: Vec::new(),
            abs_x: Vec::new(),
            abs_y: Vec::new(),
        };
        match typ {
            InputType::Keyboard => {
                cfg.cat_bitmap = vec![0x02]; // EV_KEY
                cfg.key_bitmap = bitmap(KEYBOARD_KEYS);
            }
            InputType::Mouse => {
                cfg.prop_bitmap = vec![0x01]; // INPUT_PROP_POINTER
                cfg.cat_bitmap = vec![0x06]; // EV_KEY | EV_REL
                cfg.rel_bitmap = vec![
                    0x43, // REL_X | REL_Y | REL_HWHEEL
                    0x01, // REL_WHEEL
                ];
                cfg.key_bitmap = bitmap(MOUSE_BUTTONS);
            }
            InputType::Touchpad => {
                cfg.prop_bitmap = vec![0x02]; // INPUT_PROP_DIRECT
                cfg.cat_bitmap = vec![0x0e]; // EV_KEY | EV_REL | EV_ABS
                cfg.abs_bitmap = vec![0x03]; // ABS_X | ABS_Y
                cfg.rel_bitmap = vec![
                    0x40, // REL_HWHEEL
                    0x01, // REL_WHEEL
                ];
                cfg.key_bitmap = bitmap(MOUSE_BUTTONS);
                cfg.abs_x = abs_info(width.saturating_sub(1));
                cfg.abs_y = abs_info(height.saturating_sub(1));
            }
        }
        cfg
    }
}

/// The input device state shared between the device handler and the
/// input handles.
struct InputState {
    typ: InputType,
    log: crate::logger::Log,
    config: InputConfig,
    sel: u8,
    subsel: u8,
    last_x: i32,
    last_y: i32,
    /// Event queue buffers available for events.
    buffers: VecDeque<u16>,
    /// Events waiting for event queue buffers.
    events: VecDeque<InputEvent>,
    /// Number of eventq and statusq requests.
    stats: [u64; 2],
}

impl InputState {
    /// Returns the configuration data selected by sel and subsel.
    fn cfg(&self) -> &[u8] {
        let c = &self.config;
        match (self.sel as u16) << 8 | self.subsel as u16 {
            0x0100 => b"GoEMU Input Device", // VIRTIO_INPUT_CFG_ID_NAME
            0x0200 => b"424242",             // VIRTIO_INPUT_CFG_ID_SERIAL
            0x0300 => &c.dev_ids,            // VIRTIO_INPUT_CFG_ID_DEVIDS
            0x1000 => &c.prop_bitmap,        // VIRTIO_INPUT_CFG_PROP_BITS
            0x1100 => &c.cat_bitmap,         // VIRTIO_INPUT_CFG_EV_BITS: categories
            0x1101 => &c.key_bitmap,         // VIRTIO_INPUT_CFG_EV_BITS | EV_KEY
            0x1102 => &c.rel_bitmap,         // VIRTIO_INPUT_CFG_EV_BITS | EV_REL
            0x1103 => &c.abs_bitmap,         // VIRTIO_INPUT_CFG_EV_BITS | EV_ABS
            0x1200 => &c.abs_x,              // VIRTIO_INPUT_CFG_ABS_INFO | ABS_X
            0x1201 => &c.abs_y,              // VIRTIO_INPUT_CFG_ABS_INFO | ABS_Y
            _ => {
                crate::log_debug!(
                    self.log,
                    "cfg: skipping: sel={:02x}, subsel={:02x}",
                    self.sel,
                    self.subsel
                );
                &[]
            }
        }
    }

    fn add_event(&mut self, typ: u16, code: u16, value: u32) {
        if self.events.len() >= MAX_PENDING_EVENTS {
            self.log.error(format_args!(
                "event queue full: dropping event {}/{}/{}",
                typ, code, value
            ));
            return;
        }
        self.events.push_back(InputEvent { typ, code, value });
    }

    /// Stores pending events into the available event buffers.
    fn deliver(&mut self, st: &mut VirtioState) {
        while !self.events.is_empty() && !self.buffers.is_empty() {
            let head = self.buffers.pop_front().unwrap();
            let ev = self.events.pop_front().unwrap();
            let tx = match store_event(st, head, ev) {
                Ok(n) => n,
                Err(e) => {
                    self.log.error(format_args!("store event failed: {}", e));
                    0
                }
            };
            st.complete_descriptor(0, head, tx);
        }
    }
}

/// Writes the event into the event queue buffer.
fn store_event(st: &VirtioState, idx: u16, ev: InputEvent) -> Result<u32, String> {
    let desc = st.guest.load_desc(&st.queues[0], idx)?;
    if desc.len < 8 || desc.flags & VIRTQ_DESC_F_WRITE == 0 {
        return Err(format!("invalid eventq desc: {}", desc));
    }
    let mut buf = [0u8; 8];
    buf[0..2].copy_from_slice(&ev.typ.to_le_bytes());
    buf[2..4].copy_from_slice(&ev.code.to_le_bytes());
    buf[4..8].copy_from_slice(&ev.value.to_le_bytes());
    st.guest.write(desc.addr, &buf)?;
    Ok(desc.len)
}

/// Reads size bytes little-endian from data at offset.
fn read_le(data: &[u8], offset: usize, size: usize) -> u64 {
    data[offset..offset + size]
        .iter()
        .rev()
        .fold(0, |v, &b| v << 8 | b as u64)
}

/// VirtIO input device handler.
struct InputHandler {
    state: Arc<Mutex<InputState>>,
}

impl Handler for InputHandler {
    fn reset(&mut self, _st: &mut VirtioState) -> Result<(), String> {
        let mut s = self.state.lock().unwrap();
        s.buffers.clear();
        s.events.clear();
        Ok(())
    }

    fn device_stats(&self, st: &VirtioState) {
        let s = self.state.lock().unwrap();
        println!("{}: event : {}", st.log.name, s.stats[0]);
        println!("{}: status: {}", st.log.name, s.stats[1]);
    }

    fn execute_descriptor_chain(
        &mut self,
        st: &mut VirtioState,
        q: usize,
        idx: u16,
    ) -> Result<u32, String> {
        crate::log_debug!(st.log, "vq{}: chain: idx={}", q, idx);
        let mut s = self.state.lock().unwrap();
        s.stats[q] += 1;
        if q == 0 {
            s.buffers.push_back(idx);
            s.deliver(st);
            return Ok(0);
        }

        // Status queue.
        let vq = st.queues[q];
        let mut desc = st.guest.load_desc(&vq, idx)?;
        let mut transmitted = 0u32;
        loop {
            let mut buf = vec![0u8; desc.len as usize];
            st.guest.read(desc.addr, &mut buf)?;
            st.log.info(format_args!("statusq:\n{}", hex_dump(&buf)));
            transmitted += desc.len;
            if desc.flags & VIRTQ_DESC_F_NEXT == 0 {
                break;
            }
            desc = st.guest.load_desc(&vq, desc.next)?;
        }
        Ok(transmitted)
    }

    fn config_load(&mut self, _st: &mut VirtioState, offset: u64, size: u64) -> Option<u64> {
        let s = self.state.lock().unwrap();
        let cfg = s.cfg();
        if size == 1 {
            match offset {
                0x100 => return Some(s.sel as u64),
                0x101 => return Some(s.subsel as u64),
                0x102 => return Some(cfg.len() as u64),
                _ => {}
            }
        }
        if offset >= 0x108 && offset + size <= 0x108 + cfg.len() as u64 {
            return Some(read_le(cfg, (offset - 0x108) as usize, size as usize));
        }
        None
    }

    fn config_store(&mut self, st: &mut VirtioState, offset: u64, size: u64, v: u64) -> bool {
        let mut s = self.state.lock().unwrap();
        match (offset, size) {
            (0x100, 1) => s.sel = v as u8,
            (0x101, 1) => s.subsel = v as u8,
            _ => {
                if size != 1 {
                    st.log
                        .error(format_args!("Store{}({:x},{})", size * 8, offset, v));
                }
                return false;
            }
        }
        true
    }
}

/// Handle to an input device for injecting events.
#[derive(Clone)]
pub struct InputHandle {
    mmio: VirtioMmio,
    state: Arc<Mutex<InputState>>,
}

impl InputHandle {
    /// Returns the bus segment of the device.
    pub fn mmio(&self) -> VirtioMmio {
        self.mmio.clone()
    }

    /// Queues events added by f and delivers them to the driver. The
    /// events are dropped if the driver is not ready.
    fn inject(&self, f: impl FnOnce(&mut InputState)) {
        let mut d = self.mmio.dev.lock().unwrap();
        if !d.state.driver_ok() {
            return;
        }
        f(&mut self.state.lock().unwrap());
        d.process_queue(0);
        let d = &mut *d;
        self.state.lock().unwrap().deliver(&mut d.state);
    }

    /// Sends a key or button event.
    pub fn key_event(&self, code: u16, action: KeyAction) {
        self.inject(|s| {
            s.add_event(EV_KEY, code, action as u32);
            s.add_event(EV_SYN, SYN_REPORT, 0);
        });
    }

    /// Notifies the pointer entered (or left) the display at x, y.
    pub fn mouse_enter(&self, entered: bool, x: i32, y: i32) {
        if entered {
            let mut s = self.state.lock().unwrap();
            s.last_x = x;
            s.last_y = y;
        }
    }

    /// Sends a pointer move to x, y: absolute for touchpads and
    /// relative to the previous position for mice.
    pub fn mouse_move(&self, x: i32, y: i32) {
        self.inject(|s| {
            match s.typ {
                InputType::Touchpad => {
                    s.add_event(EV_ABS, ABS_X, x as u32);
                    s.add_event(EV_ABS, ABS_Y, y as u32);
                }
                InputType::Mouse => {
                    let (dx, dy) = (x.wrapping_sub(s.last_x), y.wrapping_sub(s.last_y));
                    s.add_event(EV_REL, REL_X, dx as u32);
                    s.add_event(EV_REL, REL_Y, dy as u32);
                    s.last_x = x;
                    s.last_y = y;
                }
                InputType::Keyboard => return,
            }
            s.add_event(EV_SYN, SYN_REPORT, 0);
        });
    }

    /// Sends a scroll event.
    pub fn mouse_scroll(&self, x: i32, y: i32) {
        self.inject(|s| {
            if s.typ == InputType::Keyboard {
                return;
            }
            if x != 0 {
                s.add_event(EV_REL, REL_HWHEEL, x as u32);
            }
            if y != 0 {
                s.add_event(EV_REL, REL_WHEEL, y as u32);
            }
            s.add_event(EV_SYN, SYN_REPORT, 0);
        });
    }
}

/// VirtIO input device.
pub struct Input;

impl Input {
    /// Creates a new input device of the type for a width×height
    /// display.
    pub fn create(
        start: u64,
        plic: Plic,
        irq: u32,
        mem: Arc<Memory>,
        width: u32,
        height: u32,
        typ: InputType,
    ) -> Result<InputHandle, String> {
        plic.register_irq(irq, &typ.to_string());
        let mut state = VirtioState::new(
            "virtio-input",
            INPUT_DEVICE_ID,
            0,
            start,
            INPUT_SIZE,
            plic,
            irq,
            mem,
            2,
        );
        state.log.level = Level::Info;
        state.log.info(format_args!(
            "input {}: screen {}\u{d7}{}",
            typ, width, height
        ));
        let input = Arc::new(Mutex::new(InputState {
            typ,
            log: state.log.clone(),
            config: InputConfig::new(typ, width, height),
            sel: 0,
            subsel: 0,
            last_x: 0,
            last_y: 0,
            buffers: VecDeque::new(),
            events: VecDeque::new(),
            stats: [0; 2],
        }));
        let dev = VirtioDevice::new(
            state,
            Box::new(InputHandler {
                state: input.clone(),
            }),
        );
        Ok(InputHandle {
            mmio: VirtioMmio::new("virtio", dev),
            state: input,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handle(typ: InputType) -> InputHandle {
        let mem = Arc::new(Memory::new(0x8000_0000, 0x10_0000));
        let plic = Plic::new(vec![Arc::new(crate::cpu::HartSignals::new())], 0x0c00_0000);
        Input::create(0x1000_0000, plic, 1, mem, 640, 480, typ).unwrap()
    }

    fn cfg_load(h: &InputHandle, sel: u8, subsel: u8) -> Vec<u8> {
        let mut d = h.mmio.dev.lock().unwrap();
        let d = &mut *d;
        assert!(d.handler.config_store(&mut d.state, 0x100, 1, sel as u64));
        assert!(d
            .handler
            .config_store(&mut d.state, 0x101, 1, subsel as u64));
        let size = d.handler.config_load(&mut d.state, 0x102, 1).unwrap();
        (0..size)
            .map(|i| d.handler.config_load(&mut d.state, 0x108 + i, 1).unwrap() as u8)
            .collect()
    }

    #[test]
    fn config_space() {
        let kbd = handle(InputType::Keyboard);
        assert_eq!(cfg_load(&kbd, 0x01, 0), b"GoEMU Input Device");
        assert_eq!(cfg_load(&kbd, 0x11, 0), vec![0x02]);
        let keys = cfg_load(&kbd, 0x11, 1);
        assert_eq!(keys.len(), 128);
        assert_ne!(keys[KEY_A as usize / 8] & 1 << (KEY_A % 8), 0);
        assert!(cfg_load(&kbd, 0x11, 2).is_empty());

        let tp = handle(InputType::Touchpad);
        let abs_x = cfg_load(&tp, 0x12, 0);
        assert_eq!(read_le(&abs_x, 4, 4), 639);
        let abs_y = cfg_load(&tp, 0x12, 1);
        assert_eq!(read_le(&abs_y, 4, 4), 479);
        let buttons = cfg_load(&tp, 0x11, 1);
        assert_eq!(buttons[BTN_LEFT as usize / 8], 0x07);
    }

    #[test]
    fn events_wait_for_buffers() {
        let h = handle(InputType::Mouse);
        let mem = h.mmio.dev.lock().unwrap().state.guest.clone();
        // Queue 0 with 4 entries: descriptors at 0x8000_0000, avail
        // ring at 0x8000_1000, used ring at 0x8000_2000, and event
        // buffers at 0x8000_3000.
        {
            let mut d = h.mmio.dev.lock().unwrap();
            let q = &mut d.state.queues[0];
            q.num = 4;
            q.ready = 1;
            q.desc_phys = 0x8000_0000;
            q.avail_phys = 0x8000_1000;
            q.used_phys = 0x8000_2000;
            d.state.status = DRIVER_OK;
        }
        h.mouse_enter(true, 10, 10);
        h.mouse_move(13, 8);

        for i in 0..3u64 {
            let d = 0x8000_0000 + i * 16;
            mem.mem.store_u64(d, 0x8000_3000 + i * 8).unwrap();
            mem.write_u32(d + 8, 8).unwrap();
            mem.write_u16(d + 12, VIRTQ_DESC_F_WRITE).unwrap();
            mem.write_u16(0x8000_1004 + i * 2, i as u16).unwrap();
        }
        mem.write_u16(0x8000_1002, 3).unwrap();
        h.mmio.dev.lock().unwrap().process_queue(0);

        assert_eq!(mem.read_u16(0x8000_2002).unwrap(), 3);
        let ev = |i: u64| {
            let a = 0x8000_3000 + i * 8;
            (
                mem.read_u16(a).unwrap(),
                mem.read_u16(a + 2).unwrap(),
                mem.read_u32(a + 4).unwrap() as i32,
            )
        };
        assert_eq!(ev(0), (EV_REL, REL_X, 3));
        assert_eq!(ev(1), (EV_REL, REL_Y, -2));
        assert_eq!(ev(2), (EV_SYN, SYN_REPORT, 0));
    }
}
