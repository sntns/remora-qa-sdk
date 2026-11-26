use std::fmt;

/// The error a plugin returns: a message for the agent.
///
/// Any [`std::error::Error`] converts into it with `?`, its sources
/// appended (`opening /x: No such file or directory`). Build one from a
/// message with [`Error::msg`] or the [`bail!`](crate::bail) macro.
pub struct Error {
    message: String,
}

impl Error {
    /// An error carrying this message.
    pub fn msg(message: impl fmt::Display) -> Self {
        Self {
            message: message.to_string(),
        }
    }

    /// Prefixes the message with what was being done.
    pub fn context(self, context: impl fmt::Display) -> Self {
        Self {
            message: format!("{context}: {}", self.message),
        }
    }

    /// The message, as reported in `failed`.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl<E: std::error::Error> From<E> for Error {
    fn from(error: E) -> Self {
        let mut message = error.to_string();
        let mut source = error.source();
        while let Some(cause) = source {
            message.push_str(": ");
            message.push_str(&cause.to_string());
            source = cause.source();
        }
        Self { message }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.message, f)
    }
}

/// `Result` with [`Error`] as its default error.
pub type Result<T = (), E = Error> = std::result::Result<T, E>;

/// Adds what was being done to an error: `fs::read(path).context("reading")?`.
pub trait Context<T> {
    fn context(self, context: impl fmt::Display) -> Result<T>;
    fn with_context<C: fmt::Display>(self, context: impl FnOnce() -> C) -> Result<T>;
}

impl<T, E: Into<Error>> Context<T> for std::result::Result<T, E> {
    fn context(self, context: impl fmt::Display) -> Result<T> {
        self.map_err(|e| e.into().context(context))
    }

    fn with_context<C: fmt::Display>(self, context: impl FnOnce() -> C) -> Result<T> {
        self.map_err(|e| e.into().context(context()))
    }
}

/// Returns early with an [`Error`] built like `format!`.
#[macro_export]
macro_rules! bail {
    ($($arg:tt)*) => {
        return ::core::result::Result::Err($crate::Error::msg(::std::format!($($arg)*)))
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_error_converts_with_context() {
        let r: Result<Vec<u8>> = std::fs::read("/nonexistent/remora-qa").context("reading x");
        let msg = r.unwrap_err().to_string();
        assert!(msg.starts_with("reading x: "), "{msg}");
    }

    #[test]
    fn bail_formats() {
        fn f(n: u32) -> Result<u32> {
            if n > 1 {
                bail!("too big: {n}");
            }
            Ok(n)
        }
        assert_eq!(f(3).unwrap_err().message(), "too big: 3");
    }
}
