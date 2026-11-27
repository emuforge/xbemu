//! The crate's error type.

use std::fmt;
use std::io;
use std::path::PathBuf;

/// Result with this crate's [`Error`].
pub type Result<T> = std::result::Result<T, Error>;

/// Why a read or a parse failed.
///
/// Malformed input always ends here. The `String` payloads are for the log;
/// callers match on the variant.
#[derive(Debug)]
pub enum Error {
    /// The host file could not be opened or read.
    Io(io::Error),
    /// A read asked for bytes outside its source.
    OutOfRange {
        /// First byte asked for.
        offset: u64,
        /// Bytes asked for.
        len: u64,
        /// Size of the source.
        size: u64,
    },
    /// The file is not a CCI this reader understands, or a block of it does
    /// not decode.
    Cci(String),
    /// A split CCI's part is missing or unreadable.
    MissingPart(PathBuf),
    /// No XDVDFS volume descriptor at any known offset of the image.
    NoVolume,
    /// A directory table or entry of the file system is damaged.
    Xdvdfs(String),
    /// The path names nothing on the disc.
    NotFound,
    /// A path component that must be a directory is a file, or a directory
    /// listing was asked of a file.
    NotADirectory,
    /// The executable's headers are damaged or describe an image the console
    /// could not load.
    Xbe(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::OutOfRange { offset, len, size } => {
                write!(
                    f,
                    "read of {len} bytes at {offset:#x} is outside a source of {size:#x} bytes"
                )
            }
            Self::Cci(s) => write!(f, "CCI image: {s}"),
            Self::MissingPart(p) => {
                write!(f, "split CCI image: part {} is missing or unreadable", p.display())
            }
            Self::NoVolume => f.write_str("no XDVDFS volume found"),
            Self::Xdvdfs(s) => write!(f, "XDVDFS: {s}"),
            Self::NotFound => f.write_str("no such file or directory on the disc"),
            Self::NotADirectory => f.write_str("not a directory"),
            Self::Xbe(s) => write!(f, "XBE: {s}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}
