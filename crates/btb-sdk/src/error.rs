//! `Result`-style errors for entry points.
//!
//! An entry point declared `-> btb_sdk::Result<T>` returns `T` on `Ok` and aborts the call with
//! the error's message on `Err` (nothing is committed; the caller sees the message).
//!
//! ```ignore
//! #[btb::mutation]
//! pub fn add(text: String) -> btb_sdk::Result<u64> {
//!     ensure!(!text.is_empty(), "text must not be empty");
//!     let Some(user) = find_user() else { bail!("unknown user {}", 7) };
//!     Ok(Todo { /* .. */ }.insert())
//! }
//! ```

use alloc::string::String;
use core::fmt;

/// An error with a message for the caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    message: String,
}

impl Error {
    pub fn new(message: impl Into<String>) -> Self {
        Error {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<&str> for Error {
    fn from(m: &str) -> Self {
        Error::new(m)
    }
}

impl From<String> for Error {
    fn from(m: String) -> Self {
        Error::new(m)
    }
}

impl From<btb_codec::DecodeError> for Error {
    fn from(e: btb_codec::DecodeError) -> Self {
        Error::new(alloc::format!("decode error: {e:?}"))
    }
}

/// `Result` whose error is [`Error`].
pub type Result<T> = core::result::Result<T, Error>;

/// Return `Err(Error)` from the function unless `cond` holds. The message takes `format!`
/// arguments: `ensure!(n < 10, "{n} is too big")`.
#[macro_export]
macro_rules! ensure {
    ($cond:expr, $($msg:tt)+) => {
        if !($cond) {
            return ::core::result::Result::Err($crate::Error::new($crate::__private::format!($($msg)+)));
        }
    };
}

/// Return `Err(Error)` with a formatted message.
#[macro_export]
macro_rules! bail {
    ($($msg:tt)+) => {
        return ::core::result::Result::Err($crate::Error::new($crate::__private::format!($($msg)+)))
    };
}
