//! Platform devices: interrupt controllers, UARTs, timers, and the
//! host-target interface.

mod clint;
mod htif;
mod plic;
mod rtc;
mod syscon;
mod uart;
mod uart_console;
mod uart_nmea;

pub use clint::*;
pub use htif::*;
pub use plic::*;
pub use rtc::*;
pub use syscon::*;
pub use uart::*;
pub use uart_console::*;
pub use uart_nmea::*;
