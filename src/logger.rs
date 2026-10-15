//! Leveled logging for devices.

use std::fmt;

/// Log levels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    All,
    Error,
    Info,
    Verbose,
    Debug,
    Trace,
}

/// A named logger with a level.
#[derive(Clone, Debug)]
pub struct Log {
    pub level: Level,
    pub name: String,
}

impl Log {
    pub fn new(name: &str, level: Level) -> Log {
        Log {
            level,
            name: name.to_string(),
        }
    }

    fn logf(&self, level: &str, args: fmt::Arguments) {
        if level.is_empty() {
            eprintln!("{}: {}", self.name, args);
        } else {
            eprintln!("{}: {}: {}", self.name, level, args);
        }
    }

    #[inline]
    pub fn enabled(&self, level: Level) -> bool {
        self.level >= level
    }

    pub fn trace(&self, args: fmt::Arguments) {
        if self.level >= Level::Trace {
            self.logf("TRACE", args);
        }
    }

    pub fn debug(&self, args: fmt::Arguments) {
        if self.level >= Level::Debug {
            self.logf("DEBUG", args);
        }
    }

    pub fn verbose(&self, args: fmt::Arguments) {
        if self.level >= Level::Verbose {
            self.logf("VERBOSE", args);
        }
    }

    pub fn info(&self, args: fmt::Arguments) {
        if self.level >= Level::Info {
            self.logf("INFO", args);
        }
    }

    /// Logs an error message and returns it as a string.
    pub fn error(&self, args: fmt::Arguments) -> String {
        let msg = args.to_string();
        if self.level >= Level::Error {
            self.logf("ERROR", format_args!("{}", msg));
        }
        msg
    }

    pub fn log(&self, args: fmt::Arguments) {
        self.logf("", args);
    }
}

/// Logs a debug message if the level is enabled. The arguments are
/// not evaluated otherwise.
#[macro_export]
macro_rules! log_debug {
    ($log:expr, $($arg:tt)*) => {
        if $log.enabled($crate::logger::Level::Debug) {
            $log.debug(format_args!($($arg)*));
        }
    };
}

/// Logs an info message if the level is enabled.
#[macro_export]
macro_rules! log_info {
    ($log:expr, $($arg:tt)*) => {
        if $log.enabled($crate::logger::Level::Info) {
            $log.info(format_args!($($arg)*));
        }
    };
}

/// Logs an error message.
#[macro_export]
macro_rules! log_error {
    ($log:expr, $($arg:tt)*) => {
        $log.error(format_args!($($arg)*))
    };
}
