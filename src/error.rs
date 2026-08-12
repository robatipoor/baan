#![allow(dead_code)]
use std::fmt;

/// Convenience type alias used throughout the project.
pub type Result<T> = std::result::Result<T, BaanError>;

#[derive(Debug)]
pub enum BaanError {
    /// Cannot open a file or device (EOPEN)
    Open {
        path: String,
        source: std::io::Error,
    },
    /// Cannot read from a device (EREAD)
    Read {
        path: String,
        source: std::io::Error,
    },
    /// Cannot write to a device (EWRITE)
    Write {
        detail: String,
        source: std::io::Error,
    },
    /// Invalid character in trigger (EINVCH)
    InvalidCharacter {
        ch: char,
    },
    /// Error initializing virtual input device (EINIT)
    InitVirtualInput {
        detail: String,
    },
    /// Error adding key to virtual input device (EADD)
    AddKey {
        keycode: u32,
        detail: String,
    },
    /// Error setting up virtual device (ESETUP)
    SetupVirtualDevice {
        detail: String,
    },
    /// Error creating virtual device (ECREATE)
    CreateVirtualDevice {
        detail: String,
    },
    /// Insufficient privileges (EPERM)
    Privileges,
    /// Command construction error (ECMD)
    Command {
        detail: String,
    },
    Signal,
    DestroyVirtualDevice {
        detail: String,
    },

    InvalidTrigger {
        line: usize,
        trigger: String,
        detail: String,
    },
    ParseConfigFile {
        line: usize,
        detail: String,
    },
    /// Error getting UID (EUID)
    Uid,
    /// Ioctl error wrapper
    Ioctl {
        detail: String,
    },
    /// Clipboard initialization error
    Clipboard,
}

impl fmt::Display for BaanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open { path, source } => write!(f, "Error opening '{}': {}", path, source),
            Self::Read { path, source } => write!(f, "Error reading from '{}': {}", path, source),
            Self::Write { detail, source } => write!(f, "Error writing: {} ({})", detail, source),
            Self::InvalidCharacter { ch } => write!(f, "Invalid character in trigger: {:?}", ch),
            Self::InitVirtualInput { detail } => {
                write!(f, "Error initializing virtual input: {}", detail)
            }
            Self::AddKey { keycode, detail } => {
                write!(
                    f,
                    "Error adding key to virtual input: {},detail: {}",
                    keycode, detail
                )
            }
            Self::SetupVirtualDevice { detail } => {
                write!(f, "Error setting up virtual device: {}", detail)
            }
            Self::CreateVirtualDevice { detail } => {
                write!(f, "Error creating virtual device: {}", detail)
            }
            Self::Privileges => write!(f, "Need sudo privileges"),
            Self::Command { detail } => write!(f, "Command error: {}", detail),
            Self::Signal => write!(f, "Failed to install signal handler"),
            Self::DestroyVirtualDevice { detail } => write!(f, "Error destroy device: {}", detail),
            Self::InvalidTrigger {
                line,
                trigger,
                detail,
            } => write!(
                f,
                "Invalid trigger line: {}, trigger: {}, detail: {}",
                line, trigger, detail
            ),
            Self::ParseConfigFile { line, detail } => {
                write!(f, "Invalid trigger line: {}, detail: {}", line, detail)
            }
            Self::Uid => write!(f, "Error getting current user's UID"),
            Self::Ioctl { detail } => write!(f, "Ioctl error: {}", detail),
            Self::Clipboard => write!(f, "Failed to initialize clipboard"),
        }
    }
}

impl std::error::Error for BaanError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Open { source, .. } => Some(source),
            Self::Read { source, .. } => Some(source),
            Self::Write { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn io_err() -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::NotFound, "boom")
    }

    #[test]
    fn error_trait_source_returns_io_cause_only() {
        let open = BaanError::Open {
            path: "x".into(),
            source: io_err(),
        };
        assert!((&open as &dyn std::error::Error).source().is_some());

        let privileged = BaanError::Privileges;
        assert!((&privileged as &dyn std::error::Error).source().is_none());
    }
}
