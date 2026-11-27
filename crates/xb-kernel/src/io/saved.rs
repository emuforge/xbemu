//! The open files and the drives' finished requests in a savestate (the
//! FILE section; see `crate::state`).

use std::sync::Arc;
use std::time::{Duration, Instant};

use xb_fs::{File, FileState, NtStatus};
use xb_types::state::{StateIo, Stateful};

use super::worker::Job;
use super::{Kind, Open};
use crate::kernel::Kernel;

/// An open object in a state: its address, serial and completion port.
type OpenKey = (u32, u64, Option<(u32, u32)>);

/// How long a save waits for the drives to finish what they hold.
const DRIVE_WAIT: Duration = Duration::from_secs(3);

impl Kernel {
    /// Before a save: the requests still with a drive come back and are
    /// held, so nothing of the state is on another thread.
    ///
    /// # Errors
    /// A drive did not finish in time.
    pub fn state_quiesce(&mut self) -> Result<(), String> {
        let end = Instant::now() + DRIVE_WAIT;
        loop {
            if let Some(workers) = &self.io.workers {
                let mut done = workers.take_done();
                self.io.held.append(&mut done);
            }
            let queued = (self.io.busy[0] + self.io.busy[1]) as usize;
            if self.io.held.len() >= queued {
                return Ok(());
            }
            if Instant::now() >= end {
                return Err(format!(
                    "{} disc or hard disk request(s) did not finish",
                    queued - self.io.held.len()
                ));
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// The open files and links, and the requests the drives finished.
    pub(crate) fn io_state(&mut self, io: &mut StateIo) {
        let fs = self.io_fs();
        let mut links = fs.symbolic_links();
        links.state(io);
        if io.loading() {
            fs.set_symbolic_links(&links);
        }

        // Open objects: address, serial, completion port, then the file or
        // the link's target.
        let mut opens: Vec<OpenKey> = Vec::new();
        let mut files: Vec<(bool, FileState, String)> = Vec::new();
        if !io.loading() {
            let mut objs: Vec<&u32> = self.io.files.keys().collect();
            objs.sort_unstable();
            for obj in objs {
                let o = &self.io.files[obj];
                opens.push((*obj, o.serial, o.port));
                files.push(match &o.kind {
                    Kind::File(f) => (true, f.saved(), String::new()),
                    Kind::Link(target) => (false, FileState::default(), target.clone()),
                });
            }
        }
        opens.state(io);
        files.state(io);
        self.io.serial.state(io);
        self.io.drive_free.state(io);
        self.io.async_seq.state(io);
        self.io.async_done.state(io);
        self.io.next_due.state(io);

        if io.loading() {
            if io.failed().is_some() || files.len() != opens.len() {
                io.fail("the state's open files are cut short");
                return;
            }
            self.io.files.clear();
            for ((obj, serial, port), (is_file, state, target)) in opens.into_iter().zip(files) {
                let kind = if is_file {
                    match fs.reopen(&state) {
                        Ok(file) => Kind::File(Arc::new(file)),
                        Err(status) => {
                            // The title's handle stays; its next use of it fails.
                            log::warn!(target: "kernel", "state: {} cannot be opened again ({status})", state.path);
                            continue;
                        }
                    }
                } else {
                    Kind::Link(target)
                };
                self.io.files.insert(obj, Open { kind, serial, port, jobs: 0 });
            }
        }

        // Requests finished and held for their drive's time.
        let mut n = self.io.held.len();
        n.state(io);
        let mut held: Vec<SavedJob> = if io.loading() {
            (0..n.min(1 << 16)).map(|_| SavedJob::default()).collect()
        } else {
            self.io.held.iter().map(SavedJob::of).collect()
        };
        for j in &mut held {
            j.state(io);
        }
        if io.loading() {
            if io.failed().is_some() {
                return;
            }
            self.io.held.clear();
            self.io.busy = [0, 0];
            for j in held {
                let Some(Open { kind: Kind::File(file), .. }) = self.io.files.get(&j.object) else {
                    log::warn!(target: "kernel", "state: a finished request's file ({}) is gone; the request is dropped", j.path);
                    continue;
                };
                let file = Arc::clone(file);
                self.io.busy[j.drive.min(1)] += 1;
                if let Some(open) = self.io.files.get_mut(&j.object) {
                    open.jobs += 1;
                }
                self.io.held.push(j.into_job(file));
            }
        }
    }
}

/// A held [`Job`] without its file.
#[derive(Default)]
struct SavedJob {
    object: u32,
    serial: u64,
    write: bool,
    off: u64,
    n: u32,
    data: Vec<u8>,
    runs: Vec<(u32, u32)>,
    due: Option<u64>,
    iosb: u32,
    event: u32,
    apc: u32,
    apc_ctx: u32,
    port: Option<(u32, u32)>,
    thread: Option<(usize, u32)>,
    sync: bool,
    seq: u64,
    drive: usize,
    path: String,
    ok: bool,
    /// Bytes done, or the status it failed with.
    value: u64,
    cost_ns: u64,
}

xb_types::state_fields!(SavedJob {
    object,
    serial,
    write,
    off,
    n,
    runs,
    due,
    iosb,
    event,
    apc,
    apc_ctx,
    port,
    thread,
    sync,
    seq,
    drive,
    path,
    ok,
    value,
    cost_ns
});

impl SavedJob {
    fn of(j: &Job) -> Self {
        let (ok, value) = match j.result {
            Ok(n) => (true, n as u64),
            Err(status) => (false, u64::from(status.0)),
        };
        Self {
            object: j.object,
            serial: j.serial,
            write: j.write,
            off: j.off,
            n: j.n,
            data: j.data.clone(),
            runs: j.runs.clone(),
            due: j.due,
            iosb: j.iosb,
            event: j.event,
            apc: j.apc,
            apc_ctx: j.apc_ctx,
            port: j.port,
            thread: j.thread,
            sync: j.sync,
            seq: j.seq,
            drive: j.drive,
            path: j.path.clone(),
            ok,
            value,
            cost_ns: j.cost_ns,
        }
    }

    fn state(&mut self, io: &mut StateIo) {
        Stateful::state(self, io);
        io.blob(&mut self.data);
    }

    fn into_job(self, file: Arc<File>) -> Job {
        Job {
            file,
            object: self.object,
            serial: self.serial,
            write: self.write,
            off: self.off,
            n: self.n,
            data: self.data,
            runs: self.runs,
            due: self.due,
            iosb: self.iosb,
            event: self.event,
            apc: self.apc,
            apc_ctx: self.apc_ctx,
            port: self.port,
            thread: self.thread,
            sync: self.sync,
            seq: self.seq,
            drive: self.drive,
            path: self.path,
            result: if self.ok {
                Ok(self.value as usize)
            } else {
                Err(NtStatus(self.value as u32))
            },
            cost_ns: self.cost_ns,
            host_ns: 0,
            issued: std::time::Instant::now(),
        }
    }
}
