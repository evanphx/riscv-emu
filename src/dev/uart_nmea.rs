//! UART peer emulating a GPS receiver that sends NMEA GPRMC sentences
//! with the host time once a second.

use super::{Uart, UartPeer};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Default)]
pub struct UartNmea {
    halted: AtomicBool,
    pending: AtomicBool,
    input: Mutex<VecDeque<u8>>,
}

impl UartNmea {
    pub fn new() -> UartNmea {
        UartNmea::default()
    }
}

/// Converts days since the Unix epoch to (year, month, day).
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Formats the NMEA GPRMC sentence for the Unix time.
pub fn format_gprmc(t: Duration) -> String {
    let secs = t.as_secs() as i64;
    let days = secs.div_euclid(86400);
    let sod = secs.rem_euclid(86400);
    let (y, mo, d) = civil_from_days(days);
    let time = format!(
        "{:02}{:02}{:02}.{:02}",
        sod / 3600,
        (sod / 60) % 60,
        sod % 60,
        t.subsec_millis() / 10
    );
    let date = format!("{:02}{:02}{:02}", d, mo, y % 100);
    let raw = format!(
        "GPRMC,{},A,0000.000,N,00000.000,E,0.0,0.0,{},,,A",
        time, date
    );
    let checksum = raw.bytes().fold(0u8, |a, b| a ^ b);
    format!("${}*{:02X}\r\n", raw, checksum)
}

impl UartPeer for UartNmea {
    fn run(&self, uart: Uart) {
        while !self.halted.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_secs(1));
            self.pending.store(true, Ordering::SeqCst);
            uart.input_available();
        }
    }

    fn halt(&self) -> Result<(), String> {
        self.halted.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn receive(&self) -> (u8, bool) {
        let mut input = self.input.lock().unwrap();
        if input.is_empty() {
            if !self.pending.swap(false, Ordering::SeqCst) {
                return (0xff, false);
            }
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default();
            input.extend(format_gprmc(now).bytes());
        }
        let v = input.pop_front().unwrap_or(0xff);
        (v, !input.is_empty())
    }

    fn send(&self, _b: u8) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gprmc() {
        // 2026-06-08 04:55:00 UTC
        let t = Duration::from_secs(1780894500);
        let s = format_gprmc(t);
        assert!(s.starts_with("$GPRMC,045500.00,A,"), "{}", s);
        assert!(s.contains(",080626,"), "{}", s);
        assert!(s.ends_with("\r\n"));
    }
}
