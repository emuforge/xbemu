//! NTSTATUS values, as the Xbox kernel returns them from its I/O calls.

use std::fmt;

/// An NTSTATUS value.
///
/// The top two bits are the severity: 0 success, 1 informational, 2 warning,
/// 3 error. Titles (through XAPI) test `>= 0` on the signed value for
/// success, so warnings such as [`NtStatus::NO_MORE_FILES`] fail there.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct NtStatus(pub u32);

macro_rules! statuses {
    ($($(#[$doc:meta])* $name:ident = $value:literal;)*) => {
        impl NtStatus {
            $($(#[$doc])* pub const $name: NtStatus = NtStatus($value);)*

            /// The constant's name, for logs; `None` for a value not listed here.
            #[must_use]
            pub fn name(self) -> Option<&'static str> {
                match self.0 {
                    $($value => Some(stringify!($name)),)*
                    _ => None,
                }
            }
        }
    };
}

statuses! {
    /// The operation completed.
    SUCCESS = 0x0000_0000;
    /// The request was queued; its result arrives in the IoStatusBlock.
    PENDING = 0x0000_0103;
    /// A directory enumeration has returned every match.
    NO_MORE_FILES = 0x8000_0006;
    /// The information class is not one the file system answers.
    INVALID_INFO_CLASS = 0xC000_0003;
    /// The handle does not name an object of the kind needed.
    INVALID_HANDLE = 0xC000_0008;
    /// An argument is out of range.
    INVALID_PARAMETER = 0xC000_000D;
    /// A directory enumeration's first call matched nothing.
    NO_SUCH_FILE = 0xC000_000F;
    /// The device does not take this request (a read of a directory, an
    /// unknown control code).
    INVALID_DEVICE_REQUEST = 0xC000_0010;
    /// A read started at or past the end of the file.
    END_OF_FILE = 0xC000_0011;
    /// The DVD drive's tray is empty.
    NO_MEDIA_IN_DEVICE = 0xC000_0013;
    /// The handle's access does not allow the operation, or the host refused it.
    ACCESS_DENIED = 0xC000_0022;
    /// The caller's buffer cannot hold the answer.
    BUFFER_TOO_SMALL = 0xC000_0023;
    /// The name has characters or a length the file system does not take.
    OBJECT_NAME_INVALID = 0xC000_0033;
    /// The last component of the path does not exist.
    OBJECT_NAME_NOT_FOUND = 0xC000_0034;
    /// The name exists and a new one was asked for.
    OBJECT_NAME_COLLISION = 0xC000_0035;
    /// A component before the last does not exist.
    OBJECT_PATH_NOT_FOUND = 0xC000_003A;
    /// Another handle's share access excludes this open.
    SHARING_VIOLATION = 0xC000_0043;
    /// The file is deleted and waits for its last handle to close.
    DELETE_PENDING = 0xC000_0056;
    /// A write could not be stored.
    DISK_FULL = 0xC000_007F;
    /// A table is full (the symbolic links').
    INSUFFICIENT_RESOURCES = 0xC000_009A;
    /// A read failed on the host.
    DEVICE_DATA_ERROR = 0xC000_009C;
    /// A write or create on the disc.
    MEDIA_WRITE_PROTECTED = 0xC000_00A2;
    /// `FILE_NON_DIRECTORY_FILE` on a directory.
    FILE_IS_A_DIRECTORY = 0xC000_00BA;
    /// A directory that still has entries was deleted.
    DIRECTORY_NOT_EMPTY = 0xC000_0101;
    /// `FILE_DIRECTORY_FILE` on a file.
    NOT_A_DIRECTORY = 0xC000_0103;
}

impl NtStatus {
    /// True for severity 0 and 1 (`NT_SUCCESS`): the signed value is not negative.
    #[must_use]
    pub const fn is_success(self) -> bool {
        self.0 < 0x8000_0000
    }

    /// True for severity 3 (`NT_ERROR`).
    #[must_use]
    pub const fn is_error(self) -> bool {
        self.0 >= 0xC000_0000
    }
}

impl fmt::Debug for NtStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.name() {
            Some(name) => write!(f, "{name}({:08X})", self.0),
            None => write!(f, "NtStatus({:08X})", self.0),
        }
    }
}

impl fmt::Display for NtStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl From<NtStatus> for u32 {
    fn from(s: NtStatus) -> u32 {
        s.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_follows_the_top_bits() {
        assert!(NtStatus::SUCCESS.is_success());
        assert!(NtStatus::PENDING.is_success());
        assert!(!NtStatus::NO_MORE_FILES.is_success());
        assert!(!NtStatus::NO_MORE_FILES.is_error());
        assert!(NtStatus::END_OF_FILE.is_error());
    }

    #[test]
    fn debug_names_known_values() {
        assert_eq!(format!("{:?}", NtStatus::END_OF_FILE), "END_OF_FILE(C0000011)");
        assert_eq!(format!("{:?}", NtStatus(0xC0FF_EE00)), "NtStatus(C0FFEE00)");
    }
}
