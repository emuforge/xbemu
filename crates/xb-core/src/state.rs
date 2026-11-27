//! Savestates: the whole machine as bytes, taken and put back while a title
//! runs.
//!
//! The order of work and of the sections is the reference's; each part of
//! the machine writes its own section (`xb_types::state`).
//!
//! Both happen on the guest-CPU thread at a pause, asked for by the front
//! end's thread ([`crate::Core::save_state`], [`crate::Core::load_state`]):
//!
//! - **Save.** The renderer's decoder parks (what the GPU drew goes back to
//!   guest memory and the GPU finishes), the drives' requests in flight come
//!   back, then RAM, the kernel, the devices, the GPU and the APU are
//!   written.
//! - **Load.** Into a machine that booted the same title: the decoder parks,
//!   every section replaces what is there, the CPU's mappings follow, and
//!   the renderer makes its targets and textures again from the loaded
//!   memory.
//!
//! The bytes are a header (magic, title, sizes) and the sections, LZ4
//! compressed. A state is refused before anything is replaced when it is of
//! another title, another RAM size, cut short, or written by a newer build.
//!
//! The hard disk: the title's cache partitions are part of a state (the
//! `DISK` section, `xb_kernel::DiskState`) and a load puts them back before
//! the title's files are opened again, because what the title has in memory
//! is only true of the disk it was saved with. A slot keeps the files'
//! contents in a store beside the slots, named by their hash; a state made
//! as bytes or written to a path carries them itself. A state from before
//! this is refused. The saved games are not put back: a load says when they
//! are no longer the ones the state was taken with.
//!
//! Not in a state: which gamepads are plugged in.

use std::path::PathBuf;
use std::sync::mpsc::Sender;

use xb_cpu::Cpu;
use xb_kernel::{DiskHash, DiskState};
use xb_types::state::{MAGIC, StateIo, Stateful, check};

use crate::guest::Machine;

/// What the front end's thread asks of the guest-CPU thread at a pause.
pub(crate) enum StateJob {
    /// Take a state. With a folder, the hard disk's files are kept there by
    /// their hash; without one they go into the state.
    Save(Option<PathBuf>, Sender<Result<Saved, String>>),
    /// Put this one back; the folder is where files the state does not
    /// carry are looked for. The answer is what differs in the saved games.
    Load(Vec<u8>, Option<PathBuf>, Sender<Result<Vec<String>, String>>),
}

/// A state taken.
pub(crate) struct Saved {
    /// Its bytes.
    pub bytes: Vec<u8>,
    /// The hard disk files it needs, by hash.
    pub disk: Vec<DiskHash>,
}

/// The state's header, after the magic.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct StateInfo {
    /// The title the state is of.
    pub title_id: u32,
    /// Its name.
    pub title: String,
    /// The machine's RAM, in bytes.
    pub ram_size: u32,
    /// Guest time when it was taken, in nanoseconds since boot.
    pub guest_ns: u64,
    /// The identity of the booted machine it was taken on
    /// (`xb_kernel::Kernel::state_layout`): it loads only into one with the
    /// same. 0: a state from before states carried it.
    pub layout: u64,
}

/// The header's version: 2 added `layout`.
const HEAD_VERSION: u32 = 2;

/// The section that holds the hard disk, after the header.
const DISK: [u8; 4] = *b"DISK";
const DISK_VERSION: u32 = 1;

impl StateInfo {
    /// The header's fields as version `v` of it has them.
    fn state(&mut self, io: &mut StateIo, v: u32) {
        self.title_id.state(io);
        self.title.state(io);
        self.ram_size.state(io);
        self.guest_ns.state(io);
        if v >= 2 {
            self.layout.state(io);
        }
    }
}

/// A state's bytes opened: its header, the uncompressed sections, and
/// whether the hard disk is among them.
fn open(bytes: &[u8]) -> Result<(StateInfo, Vec<u8>, bool), String> {
    let rest = bytes.strip_prefix(&MAGIC).ok_or("not an xbemu savestate")?;
    let raw = lz4_flex::block::decompress_size_prepended(rest)
        .map_err(|e| format!("the state does not decompress: {e}"))?;
    let tags = check(&raw)?;
    if tags.first() != Some(b"HEAD") {
        return Err("the state has no header".into());
    }
    let mut io = StateIo::load(raw);
    let mut info = StateInfo::default();
    io.section(*b"HEAD", HEAD_VERSION, |io, v| info.state(io, v));
    if let Some(why) = io.failed() {
        return Err(why.to_owned());
    }
    Ok((info, io.into_bytes(), tags.get(1) == Some(&DISK)))
}

/// The header of a state's bytes, without loading it (a menu's list).
///
/// # Errors
/// The bytes are not a whole savestate.
pub fn info(bytes: &[u8]) -> Result<StateInfo, String> {
    open(bytes).map(|(info, _, _)| info)
}

impl Machine {
    /// Runs a job at a pause.
    pub(crate) fn state_job(&mut self, job: StateJob) {
        match job {
            StateJob::Save(store, reply) => {
                let _ = reply.send(self.save_state(store.as_deref()));
            }
            StateJob::Load(bytes, store, reply) => {
                let _ = reply.send(self.load_state(&bytes, store.as_deref()));
            }
        }
    }

    /// Every section, either way; the decoder is parked.
    fn sections(&mut self, io: &mut StateIo) {
        io.section(*b"RAM ", 1, |io, _| self.shared.ram.state(io));
        if io.failed().is_some() {
            return;
        }
        self.kernel.state(io);
        io.section(*b"DEVS", 2, |io, v| self.bus.devices_state(io, v));
        if let Some(gpu) = self.shared.gpu.get() {
            gpu.state(io);
        }
        if let Some(apu) = self.shared.apu() {
            apu.state(io);
        }
    }

    fn save_state(&mut self, store: Option<&std::path::Path>) -> Result<Saved, String> {
        let t0 = std::time::Instant::now();
        let shared = self.shared.clone();
        let gpu = shared.gpu.get();
        if gpu.is_some_and(|g| !g.park()) {
            return Err("the renderer did not stop for the state".into());
        }
        let quiet = self.kernel.state_quiesce().and_then(|()| self.kernel.disk_state_save(store));
        let result = quiet.map(|mut disk| {
            let hashes = disk.hashes();
            let mut io = StateIo::save();
            let mut info = StateInfo {
                title_id: self.title_id,
                title: self.title.clone(),
                ram_size: self.shared.ram.size(),
                guest_ns: self.shared.clock.now_ns(),
                layout: self.kernel.state_layout(),
            };
            io.section(*b"HEAD", HEAD_VERSION, |io, v| info.state(io, v));
            io.section(DISK, DISK_VERSION, |io, _| disk.state(io));
            self.sections(&mut io);
            (io.into_bytes(), hashes)
        });
        if let Some(gpu) = gpu {
            gpu.unpark();
        }
        let (raw, disk) = result?;
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&lz4_flex::block::compress_prepend_size(&raw));
        log::info!(
            target: "core",
            "state saved: {} MiB ({} MiB before compression) in {:.0} ms",
            bytes.len() >> 20,
            raw.len() >> 20,
            t0.elapsed().as_secs_f64() * 1e3
        );
        Ok(Saved { bytes, disk })
    }

    fn load_state(
        &mut self,
        bytes: &[u8],
        store: Option<&std::path::Path>,
    ) -> Result<Vec<String>, String> {
        let t0 = std::time::Instant::now();
        let (info, raw, has_disk) = open(bytes)?;
        if !has_disk {
            return Err("the state is from before states held the hard disk's cache".into());
        }
        if info.title_id != self.title_id {
            return Err(format!(
                "the state is of '{}' ({:08x}), not of the running title",
                info.title, info.title_id
            ));
        }
        if info.layout != self.kernel.state_layout() {
            return Err(if info.layout == 0 {
                "the state is from before states said which build's memory layout they are of"
                    .to_owned()
            } else {
                "the state is of a build that lays the console's memory out differently".to_owned()
            });
        }
        if info.ram_size != self.shared.ram.size() {
            return Err(format!(
                "the state is of a machine with {} MiB of RAM",
                info.ram_size >> 20
            ));
        }
        let shared = self.shared.clone();
        let gpu = shared.gpu.get();
        if gpu.is_some_and(|g| !g.park()) {
            return Err("the renderer did not stop for the state".into());
        }
        let mut io = StateIo::load(raw);
        io.section(*b"HEAD", HEAD_VERSION, |_, _| {});
        // The disk first: the kernel's section opens the title's files again.
        let mut disk = DiskState::default();
        io.section(DISK, DISK_VERSION, |io, _| disk.state(io));
        let put_back = match io.failed() {
            Some(why) => Err(why.to_owned()),
            None => self.kernel.disk_state_load(&disk, store),
        };
        let saves = match put_back {
            Ok(saves) => saves,
            Err(why) => {
                if let Some(gpu) = gpu {
                    gpu.unpark();
                }
                return Err(why);
            }
        };
        self.sections(&mut io);
        // The CPU's view of the mappings, and of guest code it translated.
        self.kernel.sync_cpu(&mut self.cpu);
        self.cpu.invalidate_code(0, self.shared.ram.size());
        self.shared.tracker.set_dirty(0, self.shared.ram.size());
        if let Some(gpu) = gpu {
            gpu.unpark();
        }
        if let Some(why) = io.failed() {
            // Part of the machine is the state's and part is not.
            return Err(format!("the state did not load and the title cannot go on: {why}"));
        }
        log::info!(
            target: "core",
            "state loaded: guest time {:.1} s, in {:.0} ms",
            info.guest_ns as f64 / 1e9,
            t0.elapsed().as_secs_f64() * 1e3
        );
        for line in &saves {
            log::warn!(target: "core", "state: the saved games are not the state's: {line}");
        }
        Ok(saves)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A state's bytes with `head` as its header section and nothing else.
    fn bytes_of(head: impl FnOnce(&mut StateIo)) -> Vec<u8> {
        let mut io = StateIo::save();
        head(&mut io);
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&lz4_flex::block::compress_prepend_size(&io.into_bytes()));
        bytes
    }

    #[test]
    fn a_header_reads_back_and_one_of_the_first_version_reads_without_a_layout() {
        let mut now = StateInfo {
            title_id: 0x4D53_0002,
            title: "Fuzion Frenzy".into(),
            ram_size: 64 << 20,
            guest_ns: 40_500_000_000,
            layout: 0x1234_5678_9ABC_DEF0,
        };
        let bytes = bytes_of(|io| io.section(*b"HEAD", HEAD_VERSION, |io, v| now.state(io, v)));
        assert_eq!(info(&bytes).unwrap(), now);

        // Version 1, as the first builds wrote it: the four fields only.
        let bytes = bytes_of(|io| {
            io.section(*b"HEAD", 1, |io, _| {
                io.io(&mut 0x4D53_0002u32);
                io.io(&mut String::from("Fuzion Frenzy"));
                io.io(&mut (64u32 << 20));
                io.io(&mut 40_500_000_000u64);
            });
        });
        let old = info(&bytes).unwrap();
        assert_eq!(old, StateInfo { layout: 0, ..now });
    }

    #[test]
    fn bytes_that_are_not_a_state_are_refused_with_a_reason() {
        assert!(info(b"").is_err());
        assert!(info(b"XBEMUST\x01garbage").is_err());
        let mut cut = bytes_of(|io| io.section(*b"HEAD", HEAD_VERSION, |io, _| io.io(&mut 1u32)));
        cut.truncate(cut.len() - 3);
        assert!(info(&cut).is_err());
        // A header from a build newer than this one.
        let newer = bytes_of(|io| io.section(*b"HEAD", HEAD_VERSION + 1, |io, _| io.io(&mut 1u32)));
        assert!(info(&newer).unwrap_err().contains("version"));
        // No header at all.
        let other = bytes_of(|io| io.section(*b"RAM ", 1, |io, _| io.io(&mut 1u32)));
        assert!(info(&other).unwrap_err().contains("no header"));
    }

    /// Pins what a savestate holds of this: see `xb_types::state::StateIo::probe`.
    #[test]
    fn the_saved_layout_of_the_header_is_the_recorded_one() {
        let mut io = xb_types::state::StateIo::probe();
        let mut v = StateInfo::default();
        io.section(*b"HEAD", HEAD_VERSION, |io, ver| v.state(io, ver));
        assert_eq!(
            io.shape_hash(),
            0x8FD3_67C7_B13F_812C,
            "the saved layout changed. Older states would be misread: give the section a new version, \
             read the old one where it differs, then record {:#018x} here",
            io.shape_hash()
        );
    }
}
