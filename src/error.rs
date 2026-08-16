use std::{fmt, io, path::PathBuf, process::ExitStatus, time::Duration};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    Io {
        operation: &'static str,
        path: Option<PathBuf>,
        source: io::Error,
    },
    CommandFailed {
        command: String,
        status: ExitStatus,
        stderr: Vec<u8>,
    },
    EmptyPipeline,
    Timeout {
        limit: Duration,
    },
    Script {
        line: usize,
        message: String,
    },
    Message(String),
}

impl Error {
    pub(crate) fn io(operation: &'static str, path: Option<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            operation,
            path,
            source,
        }
    }

    pub fn message(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io {
                operation,
                path: Some(path),
                source,
            } => write!(f, "{operation} {}: {source}", path.display()),
            Self::Io {
                operation,
                path: None,
                source,
            } => write!(f, "{operation}: {source}"),
            Self::CommandFailed {
                command,
                status,
                stderr,
            } => {
                write!(f, "command `{command}` failed with {status}")?;
                if !stderr.is_empty() {
                    write!(f, ": {}", String::from_utf8_lossy(stderr).trim())?;
                }
                Ok(())
            }
            Self::EmptyPipeline => f.write_str("cannot run an empty pipeline"),
            Self::Timeout { limit } => write!(f, "task exceeded timeout of {limit:?}"),
            Self::Script { line, message } => write!(f, "script line {line}: {message}"),
            Self::Message(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}
