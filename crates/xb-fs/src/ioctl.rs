//! The device control codes answered at the storage level.
//!
//! The codes are the ones titles send: the disk's geometry and partition
//! info, the DVD drive's SCSI pass-through (the Xbox security page), and a
//! format's volume lock, unlock and dismount. Any other fails with
//! `STATUS_INVALID_DEVICE_REQUEST`: zeros and success would let a title carry
//! on with them.

use crate::host::{HDD_BYTES, HDD_SECTOR, PARTITIONS};
use crate::status::NtStatus;

/// `IOCTL_DISK_GET_DRIVE_GEOMETRY`.
pub const IOCTL_DISK_GET_DRIVE_GEOMETRY: u32 = 0x0007_0000;
/// `IOCTL_DISK_GET_PARTITION_INFO`.
pub const IOCTL_DISK_GET_PARTITION_INFO: u32 = 0x0007_4004;
/// `IOCTL_SCSI_PASS_THROUGH_DIRECT`.
pub const IOCTL_SCSI_PASS_THROUGH_DIRECT: u32 = 0x0004_D014;
/// `FSCTL_LOCK_VOLUME`.
pub const FSCTL_LOCK_VOLUME: u32 = 0x0009_0018;
/// `FSCTL_UNLOCK_VOLUME`.
pub const FSCTL_UNLOCK_VOLUME: u32 = 0x0009_001C;
/// `FSCTL_DISMOUNT_VOLUME`.
pub const FSCTL_DISMOUNT_VOLUME: u32 = 0x0009_0020;

/// `DISK_GEOMETRY` of the hard disk: `Cylinders` (every sector a cylinder)
/// +0, `MediaType` (12, FixedMedia) +8, `TracksPerCylinder` 1 +0xC,
/// `SectorsPerTrack` 1 +0x10, `BytesPerSector` 512 +0x14.
#[must_use]
pub fn disk_geometry() -> [u8; 24] {
    let mut out = [0; 24];
    out[..8].copy_from_slice(&(HDD_BYTES / u64::from(HDD_SECTOR)).to_le_bytes());
    out[8..12].copy_from_slice(&12u32.to_le_bytes());
    out[12..16].copy_from_slice(&1u32.to_le_bytes());
    out[16..20].copy_from_slice(&1u32.to_le_bytes());
    out[20..24].copy_from_slice(&HDD_SECTOR.to_le_bytes());
    out
}

/// `PARTITION_INFORMATION` of partition 0 to 5: `StartingOffset` +0,
/// `PartitionLength` +8 (the whole disk for partition 0), `HiddenSectors`
/// +0x10, `PartitionNumber` +0x14, `RecognizedPartition` +0x1A. `None` for
/// other partitions.
#[must_use]
pub fn partition_info(partition: u8) -> Option<[u8; 32]> {
    let &(start, length) = PARTITIONS.get(usize::from(partition))?;
    let mut out = [0; 32];
    out[..8].copy_from_slice(&start.to_le_bytes());
    out[8..16].copy_from_slice(&(if partition == 0 { HDD_BYTES } else { length }).to_le_bytes());
    out[16..20].copy_from_slice(&((start / u64::from(HDD_SECTOR)) as u32).to_le_bytes());
    out[20..24].copy_from_slice(&u32::from(partition).to_le_bytes());
    out[26] = 1;
    Some(out)
}

/// What a control code asks of the caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Control {
    /// Done: these bytes go to the output buffer, and their count is the
    /// IoStatusBlock's information (empty for the volume controls).
    Output(Vec<u8>),
    /// `IOCTL_SCSI_PASS_THROUGH_DIRECT`: the input buffer is a
    /// [`ScsiPassThroughDirect`] with guest pointers. The kernel reads it,
    /// calls [`scsi_command`], writes the data and a good SCSI status, and
    /// reports success with no information.
    ScsiPassThrough,
}

/// Answers `NtDeviceIoControlFile` / `NtFsControlFile` for a file on
/// `partition` (`None` for the disc). `output_len` is the output buffer's
/// length, 0 when the title passed none.
///
/// # Errors
/// `STATUS_BUFFER_TOO_SMALL` for a disk control whose answer does not fit,
/// `STATUS_INVALID_DEVICE_REQUEST` for a code not known here.
pub fn device_control(
    partition: Option<u8>,
    code: u32,
    output_len: usize,
) -> Result<Control, NtStatus> {
    if let Some(partition) = partition
        && code >> 16 == 7
    {
        // FILE_DEVICE_DISK
        let answer = match code {
            _ if partition > 5 => return Err(NtStatus::INVALID_DEVICE_REQUEST),
            IOCTL_DISK_GET_DRIVE_GEOMETRY => disk_geometry().to_vec(),
            IOCTL_DISK_GET_PARTITION_INFO => {
                partition_info(partition).ok_or(NtStatus::INVALID_DEVICE_REQUEST)?.to_vec()
            }
            _ => return Err(NtStatus::INVALID_DEVICE_REQUEST),
        };
        return if output_len < answer.len() {
            Err(NtStatus::BUFFER_TOO_SMALL)
        } else {
            Ok(Control::Output(answer))
        };
    }
    match code {
        IOCTL_SCSI_PASS_THROUGH_DIRECT => Ok(Control::ScsiPassThrough),
        // A cache drive's format (XMountUtilityDrive) locks and dismounts its
        // partition: there is no volume state here to hold or drop.
        FSCTL_LOCK_VOLUME | FSCTL_UNLOCK_VOLUME | FSCTL_DISMOUNT_VOLUME => {
            Ok(Control::Output(Vec::new()))
        }
        _ => Err(NtStatus::INVALID_DEVICE_REQUEST),
    }
}

/// The fields of `SCSI_PASS_THROUGH_DIRECT` (0x2C bytes) the DVD drive's
/// pass-through uses: `ScsiStatus` +2, `DataTransferLength` +0xC,
/// `DataBuffer` +0x14, `Cdb` +0x1C.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScsiPassThroughDirect {
    /// Bytes of the data buffer.
    pub data_transfer_length: u32,
    /// Guest address of the data buffer.
    pub data_buffer: u32,
    /// The command.
    pub cdb: [u8; 16],
}

impl ScsiPassThroughDirect {
    /// Size in guest memory; a shorter input buffer is ignored.
    pub const SIZE: usize = 0x2C;
    /// Offset of `ScsiStatus`, which the kernel sets to 0 (good).
    pub const SCSI_STATUS_OFFSET: usize = 2;

    /// Parses the guest layout; `None` if `b` is too short.
    #[must_use]
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() < Self::SIZE {
            return None;
        }
        let rd = |at: usize| u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]);
        let mut cdb = [0; 16];
        cdb.copy_from_slice(&b[0x1C..0x2C]);
        Some(Self { data_transfer_length: rd(0xC), data_buffer: rd(0x14), cdb })
    }

    /// The guest layout, other fields zero.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut b = [0; Self::SIZE];
        b[0xC..0x10].copy_from_slice(&self.data_transfer_length.to_le_bytes());
        b[0x14..0x18].copy_from_slice(&self.data_buffer.to_le_bytes());
        b[0x1C..0x2C].copy_from_slice(&self.cdb);
        b
    }
}

/// The DVD drive's answer to a SCSI command: `data_len` bytes for the data
/// buffer.
///
/// XAPI checks the disc with MODE SENSE(10) of the Xbox security page (0x3E,
/// the DVDX2 authentication page) and reboots to the dashboard unless the
/// drive reports the Xbox partition authenticated (Panzer Dragoon Orta):
/// CDFValid, PartitionArea 1, Authentication. READ DVD STRUCTURE of the
/// security sector asks `security_sector` to fill the buffer (the kernel
/// makes one from the title's certificate; false leaves zeros). Other
/// commands read zeros.
pub fn scsi_command(
    cdb: &[u8; 16],
    data_len: usize,
    security_sector: impl FnOnce(&mut [u8]) -> bool,
) -> Vec<u8> {
    let mut d = vec![0u8; data_len];
    let address = u32::from_be_bytes([cdb[2], cdb[3], cdb[4], cdb[5]]);
    if cdb[0] == 0xAD && address == 0xFF02_FDFF && cdb[6] == 0xFE {
        if security_sector(&mut d) {
            log::info!(target: "fs", "scsi ad: the disc's security sector ({data_len} bytes), the drive's own");
        } else {
            d.fill(0);
            log::info!(target: "fs", "scsi ad ({data_len} bytes): zeros");
        }
    } else if cdb[0] == 0x5A && cdb[2] & 0x3F == 0x3E && data_len >= 13 {
        d[1] = if data_len < 28 { (data_len - 2) as u8 } else { 26 }; // mode data length: past itself
        d[8] = 0x3E; // the page, 0x12 bytes past its header
        d[9] = 0x12;
        d[10] = 1; // CDFValid
        d[11] = 1; // PartitionArea: the Xbox partition
        d[12] = 1; // Authentication
    } else {
        log::info!(target: "fs", "scsi {:02x} ({data_len} bytes): zeros", cdb[0]);
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry_describes_the_retail_disk_in_sectors() {
        let g = disk_geometry();
        assert_eq!(u64::from_le_bytes(g[..8].try_into().unwrap()) * 512, HDD_BYTES);
        assert_eq!(g[8], 12);
        assert_eq!(&g[20..], &512u32.to_le_bytes());
    }

    #[test]
    fn partition_info_matches_the_layout_table() {
        for part in 0..6u8 {
            let p = partition_info(part).unwrap();
            let start = u64::from_le_bytes(p[..8].try_into().unwrap());
            let length = u64::from_le_bytes(p[8..16].try_into().unwrap());
            assert_eq!(start, PARTITIONS[usize::from(part)].0);
            assert_eq!(length, if part == 0 { HDD_BYTES } else { PARTITIONS[usize::from(part)].1 });
            assert_eq!(u64::from(u32::from_le_bytes(p[16..20].try_into().unwrap())) * 512, start);
            assert_eq!((p[20], p[26]), (part, 1));
            assert!(start + PARTITIONS[usize::from(part)].1 <= HDD_BYTES);
        }
        assert!(partition_info(6).is_none());
    }

    #[test]
    fn controls_are_answered_or_refused_as_the_reference_does() {
        assert_eq!(
            device_control(Some(1), IOCTL_DISK_GET_DRIVE_GEOMETRY, 24),
            Ok(Control::Output(disk_geometry().to_vec()))
        );
        assert_eq!(
            device_control(Some(1), IOCTL_DISK_GET_DRIVE_GEOMETRY, 23),
            Err(NtStatus::BUFFER_TOO_SMALL)
        );
        assert_eq!(
            device_control(Some(0), IOCTL_DISK_GET_PARTITION_INFO, 32),
            Ok(Control::Output(partition_info(0).unwrap().to_vec()))
        );
        assert_eq!(
            device_control(Some(6), IOCTL_DISK_GET_PARTITION_INFO, 32),
            Err(NtStatus::INVALID_DEVICE_REQUEST)
        );
        assert_eq!(device_control(Some(1), 0x0007_0024, 64), Err(NtStatus::INVALID_DEVICE_REQUEST));
        assert_eq!(
            device_control(None, IOCTL_DISK_GET_DRIVE_GEOMETRY, 24),
            Err(NtStatus::INVALID_DEVICE_REQUEST)
        );
        assert_eq!(
            device_control(None, IOCTL_SCSI_PASS_THROUGH_DIRECT, 0),
            Ok(Control::ScsiPassThrough)
        );
        for code in [FSCTL_LOCK_VOLUME, FSCTL_UNLOCK_VOLUME, FSCTL_DISMOUNT_VOLUME] {
            assert_eq!(device_control(Some(5), code, 0), Ok(Control::Output(Vec::new())));
        }
        assert_eq!(device_control(Some(5), 0x0009_0028, 0), Err(NtStatus::INVALID_DEVICE_REQUEST));
    }

    #[test]
    fn mode_sense_of_the_security_page_reports_an_authenticated_xbox_disc() {
        let mut cdb = [0u8; 16];
        cdb[0] = 0x5A;
        cdb[2] = 0x3E;
        let d = scsi_command(&cdb, 28, |_| panic!("not the security sector"));
        assert_eq!(d.len(), 28);
        assert_eq!((d[1], d[8], d[9], d[10], d[11], d[12]), (26, 0x3E, 0x12, 1, 1, 1));
        assert_eq!(scsi_command(&cdb, 20, |_| false)[1], 18);
        assert_eq!(scsi_command(&cdb, 12, |_| false), vec![0; 12]);
    }

    #[test]
    fn the_security_sector_comes_from_the_caller_and_other_commands_read_zeros() {
        let mut cdb = [0u8; 16];
        cdb[0] = 0xAD;
        cdb[2..6].copy_from_slice(&[0xFF, 0x02, 0xFD, 0xFF]);
        cdb[6] = 0xFE;
        let d = scsi_command(&cdb, 8, |d| {
            d.fill(0xA5);
            true
        });
        assert_eq!(d, vec![0xA5; 8]);
        assert_eq!(
            scsi_command(&cdb, 8, |d| {
                d[0] = 1;
                false
            }),
            vec![0; 8]
        );
        cdb[0] = 0x12;
        assert_eq!(scsi_command(&cdb, 4, |_| panic!("not asked")), vec![0; 4]);
    }

    #[test]
    fn the_pass_through_structure_round_trips() {
        let mut cdb = [0u8; 16];
        cdb[0] = 0x5A;
        let s = ScsiPassThroughDirect { data_transfer_length: 28, data_buffer: 0x8000_1000, cdb };
        assert_eq!(ScsiPassThroughDirect::from_bytes(&s.to_bytes()), Some(s));
        assert_eq!(ScsiPassThroughDirect::from_bytes(&[0; 0x2B]), None);
    }
}
