//! One error type for the whole tool: a message for a person (or an agent),
//! and the exit code it ends with.

use std::fmt;

#[derive(Debug)]
pub struct Error {
    pub msg: String,
    pub code: i32,
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn new(msg: impl Into<String>) -> Error {
        Error { msg: msg.into(), code: 1 }
    }

    /// A mistake in how the command was called.
    pub fn usage(msg: impl Into<String>) -> Error {
        Error { msg: msg.into(), code: 2 }
    }

    /// The person said no (Ctrl-C, Esc at a question): exit 130, and nothing
    /// more to say, the question already shows it.
    pub fn cancelled() -> Error {
        Error { msg: String::new(), code: 130 }
    }

    pub fn context(self, what: &str) -> Error {
        Error { msg: format!("{what}: {}", self.msg), code: self.code }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.msg)
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Error {
        Error::new(e.to_string())
    }
}

impl From<String> for Error {
    fn from(msg: String) -> Error {
        Error::new(msg)
    }
}

impl From<&str> for Error {
    fn from(msg: &str) -> Error {
        Error::new(msg)
    }
}

#[macro_export]
macro_rules! fail {
    ($($t:tt)*) => { return Err($crate::error::Error::new(format!($($t)*))) };
}
