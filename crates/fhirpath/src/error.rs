use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    pub message: String,
    pub kind: ErrorKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    Parse,
    Eval,
    /// The expression needs a host service that is not configured.
    Unsupported,
}

impl Error {
    pub fn parse(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: ErrorKind::Parse,
        }
    }

    pub fn eval(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: ErrorKind::Eval,
        }
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: ErrorKind::Unsupported,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for Error {}
