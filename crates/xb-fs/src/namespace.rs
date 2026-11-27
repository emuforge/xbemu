//! The object namespace: symbolic links, canonical device paths and the
//! devices a path can be on.

use crate::status::NtStatus;

/// Symbolic links the table holds at most; `IoCreateSymbolicLink` past it
/// fails with `STATUS_INSUFFICIENT_RESOURCES`.
pub const MAX_LINKS: usize = 64;

/// Components a canonical path keeps; the reference drops the rest.
const MAX_COMPONENTS: usize = 64;

/// A storage device of the namespace.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Device {
    /// `\Device\CdRom0`: the DVD drive.
    CdRom0,
    /// `\Device\Harddisk0\PartitionN`, N from 0 to 9. 0 is the whole disk
    /// (raw), 1 the data drive (E:), 2 the system drive (C:), 3 to 5 the
    /// cache drives (X: Y: Z:), 6 and up a larger disk's extra drives.
    Partition(u8),
}

impl Device {
    /// The device's object name.
    #[must_use]
    pub fn path(self) -> String {
        match self {
            Device::CdRom0 => r"\Device\CdRom0".to_owned(),
            Device::Partition(n) => format!(r"\Device\Harddisk0\Partition{n}"),
        }
    }

    /// The partition number, `None` for the DVD drive.
    #[must_use]
    pub const fn partition(self) -> Option<u8> {
        match self {
            Device::CdRom0 => None,
            Device::Partition(n) => Some(n),
        }
    }
}

/// Which kind of drive a device is, for timing and for what the volume
/// information classes report.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DeviceKind {
    /// The DVD drive: GDFX, read-only, removable.
    Disc,
    /// The hard disk: FATX.
    HardDisk,
}

fn prefix_ci(s: &str, prefix: &str) -> bool {
    s.len() >= prefix.len() && s.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

/// The device a canonical path is on and the path inside it (`""` for the
/// device itself, else starting with `\`). `None` for a path on no storage
/// device. Device names match in any case; the partition is one digit.
#[must_use]
pub fn device_of(path: &str) -> Option<(Device, &str)> {
    const CD: &str = r"\Device\CdRom0";
    const HD: &str = r"\Device\Harddisk0\Partition";
    if prefix_ci(path, CD) {
        let rel = &path[CD.len()..];
        if rel.is_empty() || rel.starts_with('\\') {
            return Some((Device::CdRom0, rel));
        }
    }
    if prefix_ci(path, HD) {
        let digit = *path.as_bytes().get(HD.len())?;
        let rel = &path[HD.len() + 1..];
        if digit.is_ascii_digit() && (rel.is_empty() || rel.starts_with('\\')) {
            return Some((Device::Partition(digit - b'0'), rel));
        }
    }
    None
}

/// The status of an open whose path is on no device, as the object manager's
/// lookup has it: a name missing from an object directory (`\`, `\??`,
/// `\Device`) is a missing name, one under a missing component a missing
/// path. GTA III probes a bare file name (`\??\x.dds`) and shows its bad-disc
/// screen on a bad path.
#[must_use]
pub fn unknown_device_status(path: &str) -> NtStatus {
    let at = path.rfind('\\').unwrap_or(0);
    let parent = &path[..at];
    if at == 0 || parent == r"\??" || parent.eq_ignore_ascii_case(r"\Device") {
        NtStatus::OBJECT_NAME_NOT_FOUND
    } else {
        NtStatus::OBJECT_PATH_NOT_FOUND
    }
}

/// The symbolic links of `\??`.
///
/// Names are kept as given (`\??\D:`, `\??\CdRom0:`) and found in any case;
/// targets are canonical device paths.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LinkTable {
    links: Vec<(String, String)>,
}

impl LinkTable {
    /// An empty table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The target of the link `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.links.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, t)| t.as_str())
    }

    /// Sets the link `name` to `target`, replacing one of that name. False
    /// when the table is full.
    pub fn set(&mut self, name: &str, target: &str) -> bool {
        if let Some(link) = self.links.iter_mut().find(|(n, _)| n.eq_ignore_ascii_case(name)) {
            *link = (name.to_owned(), target.to_owned());
        } else if self.links.len() < MAX_LINKS {
            self.links.push((name.to_owned(), target.to_owned()));
        } else {
            return false;
        }
        true
    }

    /// Removes the link `name`; false if there is none.
    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.links.len();
        self.links.retain(|(n, _)| !n.eq_ignore_ascii_case(name));
        self.links.len() != before
    }

    /// Every link as (name, target).
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.links.iter().map(|(n, t)| (n.as_str(), t.as_str()))
    }

    /// The canonical device path of `input`: `/` read as `\`, a leading
    /// `\??\` link resolved (once: targets are canonical already), `.` and
    /// `..` folded, duplicate and trailing separators dropped. The root is
    /// `\`.
    ///
    /// `None` when the path names a drive letter (`\??\X:`) with no link. A
    /// longer link name that does not exist (`\??\CDROM1:`) is left as it
    /// is, for the open to fail on. Xbox Music Mixer probes `\??\CDROM0:\…`
    /// in upper case to see what disc is in the drive.
    #[must_use]
    pub fn canonicalize(&self, input: &str) -> Option<String> {
        let mut path = input.replace('/', "\\");
        if prefix_ci(&path, r"\??\") {
            let b = path.as_bytes();
            if b.len() >= 6 && b[4].is_ascii_alphabetic() && b[5] == b':' {
                let target = self.get(&path[..6])?;
                path = format!("{target}\\{}", &path[6..]);
            } else {
                let rest = &path[4..];
                let end = rest.find('\\').unwrap_or(rest.len());
                if end > 0
                    && let Some(target) = self.get(&path[..4 + end])
                {
                    path = format!("{target}{}", &rest[end..]);
                }
            }
        }
        let mut parts: Vec<&str> = Vec::new();
        for part in path.split('\\') {
            match part {
                "" | "." => {}
                ".." => {
                    parts.pop();
                }
                _ if parts.len() < MAX_COMPONENTS => parts.push(part),
                _ => {}
            }
        }
        if parts.is_empty() {
            return Some("\\".to_owned());
        }
        Some(parts.iter().fold(String::new(), |mut out, part| {
            out.push('\\');
            out.push_str(part);
            out
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn links() -> LinkTable {
        let mut l = LinkTable::new();
        assert!(l.set(r"\??\D:", r"\Device\CdRom0"));
        assert!(l.set(r"\??\CdRom0:", r"\Device\CdRom0"));
        assert!(l.set(r"\??\T:", r"\Device\Harddisk0\partition1\TDATA\4d530004"));
        l
    }

    #[test]
    fn drive_letters_resolve_through_their_links_in_any_case() {
        let l = links();
        assert_eq!(l.canonicalize(r"\??\D:\default.xbe").unwrap(), r"\Device\CdRom0\default.xbe");
        assert_eq!(l.canonicalize(r"\??\d:\maps\a.map").unwrap(), r"\Device\CdRom0\maps\a.map");
        assert_eq!(l.canonicalize(r"\??\D:").unwrap(), r"\Device\CdRom0");
        assert_eq!(l.canonicalize(r"\??\D:\").unwrap(), r"\Device\CdRom0");
        assert_eq!(
            l.canonicalize(r"\??\T:\x.bin").unwrap(),
            r"\Device\Harddisk0\partition1\TDATA\4d530004\x.bin"
        );
    }

    #[test]
    fn a_drive_letter_without_a_link_does_not_resolve() {
        assert_eq!(links().canonicalize(r"\??\Q:\x"), None);
    }

    #[test]
    fn longer_link_names_resolve_and_unknown_ones_stay() {
        let l = links();
        assert_eq!(
            l.canonicalize(r"\??\CDROM0:\default.xbe").unwrap(),
            r"\Device\CdRom0\default.xbe"
        );
        assert_eq!(l.canonicalize(r"\??\CdRom0:").unwrap(), r"\Device\CdRom0");
        assert_eq!(l.canonicalize(r"\??\CDROM1:\x").unwrap(), r"\??\CDROM1:\x");
        assert_eq!(l.canonicalize(r"\??\x.dds").unwrap(), r"\??\x.dds");
    }

    #[test]
    fn separators_and_dots_fold() {
        let l = links();
        assert_eq!(l.canonicalize(r"\Device\CdRom0\\a\.\b\..\c\").unwrap(), r"\Device\CdRom0\a\c");
        assert_eq!(l.canonicalize("/Device/CdRom0/a").unwrap(), r"\Device\CdRom0\a");
        assert_eq!(l.canonicalize(r"\..\..").unwrap(), "\\");
        assert_eq!(l.canonicalize("").unwrap(), "\\");
        assert_eq!(l.canonicalize(r"\??\D:\a\..\..\..\x").unwrap(), r"\x");
    }

    #[test]
    fn canonical_paths_are_fixed_points() {
        let l = links();
        for p in [r"\??\D:\a\b", r"\Device\Harddisk0\Partition5\saved\hdmu.map", r"\??\T:\", "x"] {
            let once = l.canonicalize(p).unwrap();
            assert_eq!(l.canonicalize(&once).unwrap(), once);
        }
    }

    #[test]
    fn devices_are_found_in_any_case() {
        assert_eq!(device_of(r"\Device\CdRom0"), Some((Device::CdRom0, "")));
        assert_eq!(device_of(r"\device\cdrom0\a\b"), Some((Device::CdRom0, r"\a\b")));
        assert_eq!(device_of(r"\Device\CdRom01"), None);
        assert_eq!(
            device_of(r"\Device\Harddisk0\partition1\UDATA"),
            Some((Device::Partition(1), r"\UDATA"))
        );
        assert_eq!(device_of(r"\Device\Harddisk0\Partition0"), Some((Device::Partition(0), "")));
        assert_eq!(device_of(r"\Device\Harddisk0\Partition9"), Some((Device::Partition(9), "")));
        assert_eq!(device_of(r"\Device\Harddisk0\Partition10"), None);
        assert_eq!(device_of(r"\Device\Harddisk0\Partition"), None);
        assert_eq!(device_of(r"\Device\Harddisk1\Partition1"), None);
        assert_eq!(device_of(r"\Device\MU_0"), None);
        for d in [Device::CdRom0, Device::Partition(0), Device::Partition(7)] {
            assert_eq!(device_of(&d.path()), Some((d, "")));
        }
    }

    #[test]
    fn a_missing_object_is_a_name_in_a_directory_or_a_path_below_one() {
        assert_eq!(unknown_device_status(r"\x"), NtStatus::OBJECT_NAME_NOT_FOUND);
        assert_eq!(unknown_device_status(r"\??\x.dds"), NtStatus::OBJECT_NAME_NOT_FOUND);
        assert_eq!(unknown_device_status(r"\device\Nothing"), NtStatus::OBJECT_NAME_NOT_FOUND);
        assert_eq!(unknown_device_status(r"\Device\Nothing\x"), NtStatus::OBJECT_PATH_NOT_FOUND);
        assert_eq!(unknown_device_status(r"\??\Q\x"), NtStatus::OBJECT_PATH_NOT_FOUND);
    }

    #[test]
    fn the_link_table_replaces_removes_and_fills_up() {
        let mut l = links();
        assert!(l.set(r"\??\d:", r"\Device\Harddisk0\Partition2"));
        assert_eq!(l.get(r"\??\D:"), Some(r"\Device\Harddisk0\Partition2"));
        assert_eq!(l.iter().count(), 3);
        assert!(l.remove(r"\??\t:"));
        assert!(!l.remove(r"\??\T:"));
        for i in 0..MAX_LINKS {
            l.set(&format!(r"\??\Link{i}:"), "\\");
        }
        assert_eq!(l.iter().count(), MAX_LINKS);
        assert!(!l.set(r"\??\OneMore:", "\\"));
        assert!(l.set(r"\??\Link3:", r"\x"));
    }
}
