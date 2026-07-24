//! A minimal error story.
//!
//! Everything fallible in this crate returns a boxed error with a human
//! sentence attached. That is enough for a CLI: there is no library consumer
//! that needs to match on error variants, so a full error enum would be
//! ceremony without a payoff.

use std::fmt::Display;

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Build an [`Error`] from a format string.
#[macro_export]
macro_rules! err {
    ($($arg:tt)*) => {
        $crate::error::Error::from(format!($($arg)*))
    };
}

/// Return early with a formatted [`Error`].
#[macro_export]
macro_rules! bail {
    ($($arg:tt)*) => {
        return Err($crate::err!($($arg)*))
    };
}

/// Attach a human-readable sentence to a failure, the way `anyhow::Context`
/// does, so the final message reads as a chain of what we were trying to do.
pub trait Context<T> {
    fn context(self, msg: impl Display) -> Result<T>;
    fn with_context<F, D>(self, f: F) -> Result<T>
    where
        F: FnOnce() -> D,
        D: Display;
}

/// Bound on `Into<Error>` rather than `Error` so this also covers results that
/// are already boxed, which is most of them once errors start propagating.
impl<T, E> Context<T> for std::result::Result<T, E>
where
    E: Into<Error>,
{
    fn context(self, msg: impl Display) -> Result<T> {
        self.map_err(|e| Error::from(format!("{msg}: {}", e.into())))
    }

    fn with_context<F, D>(self, f: F) -> Result<T>
    where
        F: FnOnce() -> D,
        D: Display,
    {
        self.map_err(|e| Error::from(format!("{}: {}", f(), e.into())))
    }
}

impl<T> Context<T> for Option<T> {
    fn context(self, msg: impl Display) -> Result<T> {
        self.ok_or_else(|| Error::from(msg.to_string()))
    }

    fn with_context<F, D>(self, f: F) -> Result<T>
    where
        F: FnOnce() -> D,
        D: Display,
    {
        self.ok_or_else(|| Error::from(f().to_string()))
    }
}
