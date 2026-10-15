//! Host display window of the virtio-gpu device. The window shows
//! the GPU framebuffer and forwards keyboard and pointer events to
//! the virtio-input devices.

use super::gpu::{GpuDisplay, GpuHandle};
use super::input::*;
use crate::cpu::HartSignals;
use softbuffer::{Context, Surface};
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{
    DeviceEvent, DeviceId, ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent,
};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use winit::window::{CursorGrabMode, Window, WindowId};

/// Interval for checking the hart shutdown.
const SHUTDOWN_POLL: Duration = Duration::from_millis(100);

/// Pixels per scroll line for pixel based scroll deltas.
const SCROLL_LINE_PIXELS: f64 = 20.0;

/// Runs the display event loop until the window is closed or the
/// hart shuts down. Returns an error if the display is not
/// available.
pub fn run(gpu: GpuHandle, signals: Arc<HartSignals>) -> Result<(), String> {
    let event_loop = EventLoop::with_user_event()
        .build()
        .map_err(|e| e.to_string())?;
    let proxy = event_loop.create_proxy();
    gpu.display.set_waker(Box::new(move || {
        let _ = proxy.send_event(());
    }));
    let mut app = App {
        gpu,
        signals,
        window: None,
        surface: None,
        error: None,
        modifiers: ModifiersState::empty(),
        cursor: (0.0, 0.0),
        entering: false,
        last_x: 0,
        last_y: 0,
        scroll_x: 0.0,
        scroll_y: 0.0,
        grabbed: false,
        locked: false,
        left_alt: false,
        right_alt: false,
    };
    event_loop.run_app(&mut app).map_err(|e| e.to_string())?;
    app.error.map_or(Ok(()), Err)
}

struct App {
    gpu: GpuHandle,
    signals: Arc<HartSignals>,
    window: Option<Rc<Window>>,
    surface: Option<Surface<Rc<Window>, Rc<Window>>>,
    /// Window creation error.
    error: Option<String>,
    modifiers: ModifiersState,
    /// The pointer position in framebuffer coordinates. When the
    /// pointer is grabbed, this is a virtual position accumulated
    /// from the relative motion events.
    cursor: (f64, f64),
    /// The pointer entered the window and the next move reports its
    /// position.
    entering: bool,
    last_x: i32,
    last_y: i32,
    scroll_x: f64,
    scroll_y: f64,
    grabbed: bool,
    /// The grabbed pointer is locked and reports relative motion.
    /// Otherwise it is confined to the window and warped back to the
    /// window center after each move.
    locked: bool,
    left_alt: bool,
    right_alt: bool,
}

impl App {
    fn display(&self) -> &GpuDisplay {
        &self.gpu.display
    }

    fn grabbable(&self) -> bool {
        self.gpu.pointer == InputType::Mouse
    }

    fn set_title(&self) {
        let Some(window) = &self.window else {
            return;
        };
        let mut title = "GoEMU".to_string();
        if !self.gpu.title.is_empty() {
            title += " \u{2014} ";
            title += &self.gpu.title;
        }
        if self.grabbed {
            title += " (\u{2325} ESC)";
        }
        window.set_title(&title);
    }

    fn set_grab(&mut self, grab: bool) {
        let Some(window) = self.window.clone() else {
            return;
        };
        if grab {
            self.locked = window.set_cursor_grab(CursorGrabMode::Locked).is_ok();
            if !self.locked {
                if let Err(e) = window.set_cursor_grab(CursorGrabMode::Confined) {
                    eprintln!("virtio-gpu: pointer grab: {}", e);
                }
                self.warp_to_center();
            }
            // Continue relative motion from the current position.
            if let Some(mouse) = &self.gpu.mouse {
                mouse.mouse_enter(true, self.cursor.0 as i32, self.cursor.1 as i32);
            }
        } else {
            let _ = window.set_cursor_grab(CursorGrabMode::None);
        }
        window.set_cursor_visible(!grab);
        self.grabbed = grab;
        self.set_title();
    }

    /// Returns the window center in window coordinates.
    fn center(&self) -> PhysicalPosition<f64> {
        let size = self
            .window
            .as_ref()
            .map(|w| w.inner_size())
            .unwrap_or_default();
        PhysicalPosition::new((size.width / 2) as f64, (size.height / 2) as f64)
    }

    fn warp_to_center(&self) {
        if let Some(window) = &self.window {
            let _ = window.set_cursor_position(self.center());
        }
    }

    /// Moves the virtual cursor of a grabbed pointer by the window
    /// delta.
    fn move_grabbed(&mut self, dx: f64, dy: f64) {
        let (fx, fy) = self.to_framebuffer(PhysicalPosition::new(dx, dy));
        self.cursor.0 += fx;
        self.cursor.1 += fy;
        self.cursor_pos(self.cursor.0, self.cursor.1);
    }

    fn create_window(&mut self, event_loop: &ActiveEventLoop) -> Result<(), String> {
        let attrs = Window::default_attributes().with_inner_size(LogicalSize::new(
            self.display().width,
            self.display().height,
        ));
        let window = Rc::new(event_loop.create_window(attrs).map_err(|e| e.to_string())?);
        let context = Context::new(window.clone()).map_err(|e| e.to_string())?;
        let surface = Surface::new(&context, window.clone()).map_err(|e| e.to_string())?;
        self.window = Some(window);
        self.surface = Some(surface);
        self.set_title();
        Ok(())
    }

    /// Draws the framebuffer scaled to the window size.
    fn redraw(&mut self) -> Result<(), String> {
        let (Some(window), Some(surface)) = (&self.window, &mut self.surface) else {
            return Ok(());
        };
        let size = window.inner_size();
        let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else {
            return Ok(());
        };
        surface.resize(w, h).map_err(|e| e.to_string())?;
        let mut buffer = surface.buffer_mut().map_err(|e| e.to_string())?;

        let display = &self.gpu.display;
        let (sw, sh) = (display.width as usize, display.height as usize);
        let (w, h) = (w.get() as usize, h.get() as usize);
        let xmap: Vec<usize> = (0..w).map(|x| x * sw / w * 4).collect();
        let pix = display.pixels.lock().unwrap();
        for (y, row) in buffer.chunks_exact_mut(w).enumerate() {
            let src = &pix[y * sh / h * sw * 4..];
            for (d, &sx) in row.iter_mut().zip(&xmap) {
                *d = u32::from_le_bytes([src[sx], src[sx + 1], src[sx + 2], 0]);
            }
        }
        drop(pix);
        buffer.present().map_err(|e| e.to_string())
    }

    /// Converts the window position to framebuffer coordinates.
    fn to_framebuffer(&self, pos: PhysicalPosition<f64>) -> (f64, f64) {
        let Some(window) = &self.window else {
            return (pos.x, pos.y);
        };
        let size = window.inner_size();
        let d = self.display();
        (
            pos.x * d.width as f64 / size.width.max(1) as f64,
            pos.y * d.height as f64 / size.height.max(1) as f64,
        )
    }

    fn cursor_pos(&mut self, x: f64, y: f64) {
        let Some(mouse) = &self.gpu.mouse else {
            return;
        };
        if self.grabbable() && !self.grabbed {
            return;
        }
        let (ix, iy) = (x as i32, y as i32);
        if ix == self.last_x && iy == self.last_y {
            return;
        }
        self.last_x = ix;
        self.last_y = iy;

        let d = self.display();
        if self.gpu.pointer == InputType::Touchpad
            && (ix < 0 || ix >= d.width as i32 || iy < 0 || iy >= d.height as i32)
        {
            return;
        }
        mouse.mouse_move(ix, iy);
    }

    fn scroll(&mut self, xoff: f64, yoff: f64) {
        let Some(mouse) = &self.gpu.mouse else {
            return;
        };
        if self.grabbable() && !self.grabbed {
            return;
        }
        self.scroll_x += xoff;
        self.scroll_y += yoff;
        let (x, y) = (self.scroll_x as i32, self.scroll_y as i32);
        self.scroll_x -= x as f64;
        self.scroll_y -= y as f64;
        if x == 0 && y == 0 {
            return;
        }
        mouse.mouse_scroll(x, y);
    }

    fn mouse_button(&mut self, button: MouseButton, state: ElementState) {
        let k = match button {
            MouseButton::Left => BTN_LEFT,
            MouseButton::Right => BTN_RIGHT,
            MouseButton::Middle => BTN_MIDDLE,
            _ => return,
        };
        if self.grabbable() && !self.grabbed {
            if state != ElementState::Pressed || k != BTN_LEFT {
                return;
            }
            self.set_grab(true);
            // Eat the grab click.
            return;
        }
        if let Some(mouse) = &self.gpu.mouse {
            mouse.key_event(k, key_action(state, false));
        }
    }

    fn key(&mut self, event: KeyEvent) {
        let pressed = event.state == ElementState::Pressed;
        if event.physical_key == PhysicalKey::Code(KeyCode::Escape)
            && self.modifiers.alt_key()
            && pressed
            && !event.repeat
            && self.grabbable()
            && self.grabbed
        {
            self.set_grab(false);
            // The alt press was passed to the guest, now release it.
            if let Some(kbd) = &self.gpu.keyboard {
                if self.left_alt {
                    kbd.key_event(KEY_LEFTALT, KeyAction::Release);
                }
                if self.right_alt {
                    kbd.key_event(KEY_RIGHTALT, KeyAction::Release);
                }
            }
            self.left_alt = false;
            self.right_alt = false;
        }
        if self.grabbable() && !self.grabbed {
            return;
        }
        let Some(kbd) = &self.gpu.keyboard else {
            return;
        };
        let Some(k) = linux_key(event.physical_key) else {
            return;
        };
        match k {
            KEY_LEFTALT => self.left_alt = pressed && !event.repeat,
            KEY_RIGHTALT => self.right_alt = pressed && !event.repeat,
            _ => {}
        }
        kbd.key_event(k, key_action(event.state, event.repeat));
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        if let Err(e) = self.create_window(event_loop) {
            self.error = Some(e);
            event_loop.exit();
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, _event: ()) {}

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.signals.is_shutdown() {
            event_loop.exit();
            return;
        }
        if self.display().take_dirty() {
            if let Some(window) = &self.window {
                window.request_redraw();
            }
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + SHUTDOWN_POLL));
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => {
                if let Err(e) = self.redraw() {
                    eprintln!("virtio-gpu: redraw: {}", e);
                }
            }
            WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),
            WindowEvent::KeyboardInput {
                event,
                is_synthetic,
                ..
            } => {
                // Keys held when the window gains focus generate
                // synthetic presses; only their releases are passed.
                if !(is_synthetic && event.state == ElementState::Pressed) {
                    self.key(event);
                }
            }
            WindowEvent::MouseInput { state, button, .. } => self.mouse_button(button, state),
            WindowEvent::MouseWheel { delta, .. } => {
                let (x, y) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x as f64, y as f64),
                    MouseScrollDelta::PixelDelta(p) => {
                        (p.x / SCROLL_LINE_PIXELS, p.y / SCROLL_LINE_PIXELS)
                    }
                };
                self.scroll(x, y);
            }
            WindowEvent::CursorEntered { .. } => self.entering = true,
            WindowEvent::CursorLeft { .. } => {
                if let Some(mouse) = &self.gpu.mouse {
                    mouse.mouse_enter(false, self.cursor.0 as i32, self.cursor.1 as i32);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                if self.grabbed {
                    let center = self.center();
                    if !self.locked && position != center {
                        self.move_grabbed(position.x - center.x, position.y - center.y);
                        self.warp_to_center();
                    }
                    return;
                }
                self.cursor = self.to_framebuffer(position);
                let (x, y) = self.cursor;
                if self.entering {
                    self.entering = false;
                    if let Some(mouse) = &self.gpu.mouse {
                        mouse.mouse_enter(true, x as i32, y as i32);
                    }
                }
                self.cursor_pos(x, y);
            }
            _ => {}
        }
    }

    fn device_event(&mut self, _event_loop: &ActiveEventLoop, _id: DeviceId, event: DeviceEvent) {
        // A locked pointer reports only relative motion.
        if let DeviceEvent::MouseMotion { delta } = event {
            if self.grabbed && self.locked {
                self.move_grabbed(delta.0, delta.1);
            }
        }
    }
}

fn key_action(state: ElementState, repeat: bool) -> KeyAction {
    match (state, repeat) {
        (ElementState::Released, _) => KeyAction::Release,
        (ElementState::Pressed, false) => KeyAction::Press,
        (ElementState::Pressed, true) => KeyAction::Repeat,
    }
}

/// Maps the host key to the Linux input key code.
fn linux_key(key: PhysicalKey) -> Option<u16> {
    let code = match key {
        PhysicalKey::Code(c) => c,
        PhysicalKey::Unidentified(_) => return Some(KEY_UNKNOWN),
    };
    Some(match code {
        KeyCode::Space => KEY_SPACE,
        KeyCode::Quote => KEY_APOSTROPHE,
        KeyCode::Comma => KEY_COMMA,
        KeyCode::Minus => KEY_MINUS,
        KeyCode::Period => KEY_DOT,
        KeyCode::Slash => KEY_SLASH,
        KeyCode::Digit0 => KEY_0,
        KeyCode::Digit1 => KEY_1,
        KeyCode::Digit2 => KEY_2,
        KeyCode::Digit3 => KEY_3,
        KeyCode::Digit4 => KEY_4,
        KeyCode::Digit5 => KEY_5,
        KeyCode::Digit6 => KEY_6,
        KeyCode::Digit7 => KEY_7,
        KeyCode::Digit8 => KEY_8,
        KeyCode::Digit9 => KEY_9,
        KeyCode::Semicolon => KEY_SEMICOLON,
        KeyCode::Equal => KEY_EQUAL,
        KeyCode::KeyA => KEY_A,
        KeyCode::KeyB => KEY_B,
        KeyCode::KeyC => KEY_C,
        KeyCode::KeyD => KEY_D,
        KeyCode::KeyE => KEY_E,
        KeyCode::KeyF => KEY_F,
        KeyCode::KeyG => KEY_G,
        KeyCode::KeyH => KEY_H,
        KeyCode::KeyI => KEY_I,
        KeyCode::KeyJ => KEY_J,
        KeyCode::KeyK => KEY_K,
        KeyCode::KeyL => KEY_L,
        KeyCode::KeyM => KEY_M,
        KeyCode::KeyN => KEY_N,
        KeyCode::KeyO => KEY_O,
        KeyCode::KeyP => KEY_P,
        KeyCode::KeyQ => KEY_Q,
        KeyCode::KeyR => KEY_R,
        KeyCode::KeyS => KEY_S,
        KeyCode::KeyT => KEY_T,
        KeyCode::KeyU => KEY_U,
        KeyCode::KeyV => KEY_V,
        KeyCode::KeyW => KEY_W,
        KeyCode::KeyX => KEY_X,
        KeyCode::KeyY => KEY_Y,
        KeyCode::KeyZ => KEY_Z,
        KeyCode::BracketLeft => KEY_LEFTBRACE,
        KeyCode::Backslash => KEY_BACKSLASH,
        KeyCode::BracketRight => KEY_RIGHTBRACE,
        KeyCode::Backquote => KEY_GRAVE,
        KeyCode::IntlBackslash => KEY_UNKNOWN,
        KeyCode::Escape => KEY_ESC,
        KeyCode::Enter => KEY_ENTER,
        KeyCode::Tab => KEY_TAB,
        KeyCode::Backspace => KEY_BACKSPACE,
        KeyCode::Insert => KEY_INSERT,
        KeyCode::Delete => KEY_DELETE,
        KeyCode::ArrowRight => KEY_RIGHT,
        KeyCode::ArrowLeft => KEY_LEFT,
        KeyCode::ArrowDown => KEY_DOWN,
        KeyCode::ArrowUp => KEY_UP,
        KeyCode::PageUp => KEY_PAGEUP,
        KeyCode::PageDown => KEY_PAGEDOWN,
        KeyCode::Home => KEY_HOME,
        KeyCode::End => KEY_END,
        KeyCode::CapsLock => KEY_CAPSLOCK,
        KeyCode::ScrollLock => KEY_SCROLLLOCK,
        KeyCode::NumLock => KEY_NUMLOCK,
        KeyCode::PrintScreen => KEY_PRINT,
        KeyCode::Pause => KEY_PAUSE,
        KeyCode::F1 => KEY_F1,
        KeyCode::F2 => KEY_F2,
        KeyCode::F3 => KEY_F3,
        KeyCode::F4 => KEY_F4,
        KeyCode::F5 => KEY_F5,
        KeyCode::F6 => KEY_F6,
        KeyCode::F7 => KEY_F7,
        KeyCode::F8 => KEY_F8,
        KeyCode::F9 => KEY_F9,
        KeyCode::F10 => KEY_F10,
        KeyCode::F11 => KEY_F11,
        KeyCode::F12 => KEY_F12,
        KeyCode::F13 => KEY_F13,
        KeyCode::F14 => KEY_F14,
        KeyCode::F15 => KEY_F15,
        KeyCode::F16 => KEY_F16,
        KeyCode::F17 => KEY_F17,
        KeyCode::F18 => KEY_F18,
        KeyCode::F19 => KEY_F19,
        KeyCode::F20 => KEY_F20,
        KeyCode::F21 => KEY_F21,
        KeyCode::F22 => KEY_F22,
        KeyCode::F23 => KEY_F23,
        KeyCode::F24 => KEY_F24,
        KeyCode::F25 => KEY_UNKNOWN,
        KeyCode::Numpad0 => KEY_KP0,
        KeyCode::Numpad1 => KEY_KP1,
        KeyCode::Numpad2 => KEY_KP2,
        KeyCode::Numpad3 => KEY_KP3,
        KeyCode::Numpad4 => KEY_KP4,
        KeyCode::Numpad5 => KEY_KP5,
        KeyCode::Numpad6 => KEY_KP6,
        KeyCode::Numpad7 => KEY_KP7,
        KeyCode::Numpad8 => KEY_KP8,
        KeyCode::Numpad9 => KEY_KP9,
        KeyCode::NumpadDecimal => KEY_KPDOT,
        KeyCode::NumpadDivide => KEY_KPSLASH,
        KeyCode::NumpadMultiply => KEY_KPASTERISK,
        KeyCode::NumpadSubtract => KEY_KPMINUS,
        KeyCode::NumpadAdd => KEY_KPPLUS,
        KeyCode::NumpadEnter => KEY_KPENTER,
        KeyCode::NumpadEqual => KEY_KPEQUAL,
        KeyCode::ShiftLeft => KEY_LEFTSHIFT,
        KeyCode::ControlLeft => KEY_LEFTCTRL,
        KeyCode::AltLeft => KEY_LEFTALT,
        KeyCode::SuperLeft => KEY_LEFTMETA,
        KeyCode::ShiftRight => KEY_RIGHTSHIFT,
        KeyCode::ControlRight => KEY_RIGHTCTRL,
        KeyCode::AltRight => KEY_RIGHTALT,
        KeyCode::SuperRight => KEY_RIGHTMETA,
        KeyCode::ContextMenu => KEY_MENU,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapped_keys_are_advertised() {
        let codes = [
            KeyCode::KeyA,
            KeyCode::Digit0,
            KeyCode::Enter,
            KeyCode::NumpadEqual,
            KeyCode::F24,
            KeyCode::ContextMenu,
            KeyCode::IntlBackslash,
        ];
        for c in codes {
            let k = linux_key(PhysicalKey::Code(c)).unwrap();
            assert!(
                KEYBOARD_KEYS.contains(&k),
                "{:?} => {} not advertised",
                c,
                k
            );
        }
        assert_eq!(linux_key(PhysicalKey::Code(KeyCode::KeyQ)), Some(KEY_Q));
        assert_eq!(linux_key(PhysicalKey::Code(KeyCode::MediaPlayPause)), None);
    }
}
