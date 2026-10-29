use std::fmt;
use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Error,
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Level::Info => write!(f, "INFO"),
            Level::Error => write!(f, "ERROR"),
        }
    }
}

// Where log messages go -- None discards them
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Output {
    None,
    Stdout,
}

pub struct Logger {
    output: AtomicU8,
}

// The logger used by the log_info! and log_error! macros
pub static LOGGER: Logger = Logger::new();

impl Logger {
    // Starts out discarding everything
    pub const fn new() -> Self {
        Logger {
            output: AtomicU8::new(Output::None as u8),
        }
    }

    pub fn print_to_stdout(&self) {
        self.set_output(Output::Stdout);
    }

    pub fn set_output(&self, output: Output) {
        self.output.store(output as u8, Ordering::Relaxed);
    }

    pub fn output(&self) -> Output {
        match self.output.load(Ordering::Relaxed) {
            x if x == Output::Stdout as u8 => Output::Stdout,
            _ => Output::None,
        }
    }

    // args is only formatted if there is somewhere to send it
    pub fn log(&self, level: Level, args: fmt::Arguments<'_>) {
        match self.output() {
            Output::None => {}
            Output::Stdout => println!("[{level}] {args}"),
        }
    }
}

impl Default for Logger {
    fn default() -> Self {
        Self::new()
    }
}

// Logs to the global logger with println! style formatting
#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => {
        $crate::logger::LOGGER.log($crate::logger::Level::Info, format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => {
        $crate::logger::LOGGER.log($crate::logger::Level::Error, format_args!($($arg)*))
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_silent() {
        assert_eq!(Logger::new().output(), Output::None);
    }

    #[test]
    fn print_to_stdout_enables_output() {
        let logger = Logger::new();
        logger.print_to_stdout();
        assert_eq!(logger.output(), Output::Stdout);

        logger.set_output(Output::None);
        assert_eq!(logger.output(), Output::None);
    }
}
