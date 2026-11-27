//! The drives' worker threads and the close-time sync thread.
//!
//! A worker never touches guest memory: a read fills a host buffer the
//! guest-CPU thread stores into guest memory when it completes the request,
//! and a write carries the bytes the guest-CPU thread read at the call. Nor
//! does a worker wait out the drive's time: the request carries a guest-time
//! due and the kernel holds the finished request until then.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

use xb_fs::{File, NtStatus};

use crate::clock::WakeHandle;

/// The DVD drive's worker.
pub(super) const DISC: usize = 0;
/// The hard disk's worker.
pub(super) const HARD_DISK: usize = 1;

/// A read or write on its way through a drive's worker.
pub(super) struct Job {
    /// The open file. The worker reads and writes through it; the handle may
    /// close meanwhile (the close waits for the request).
    pub file: Arc<File>,
    /// The FILE_OBJECT and the open's serial: whose requests in flight this
    /// one counts among.
    pub object: u32,
    pub serial: u64,
    /// A write of `data`, not a read into it.
    pub write: bool,
    pub off: u64,
    pub n: u32,
    /// A write's bytes; a read's, once the worker has them.
    pub data: Vec<u8>,
    /// The guest buffer as runs (va, len), filled in order when a read
    /// completes.
    pub runs: Vec<(u32, u32)>,
    /// The guest time it completes no sooner than (console timing).
    pub due: Option<u64>,
    pub iosb: u32,
    /// The event object to set, or 0.
    pub event: u32,
    pub apc: u32,
    pub apc_ctx: u32,
    /// Its completion port's object and key.
    pub port: Option<(u32, u32)>,
    /// The thread that issued it and its id then: the slot may hold another
    /// thread by the end.
    pub thread: Option<(usize, u32)>,
    /// A synchronous handle's: the thread waits for it.
    pub sync: bool,
    /// An overlapped one's place among them all, 0 for a synchronous one.
    pub seq: u64,
    /// Which drive's worker has it.
    pub drive: usize,
    /// The canonical path, for the log.
    pub path: String,
    /// What the drive made of it: the bytes transferred.
    pub result: Result<usize, NtStatus>,
    /// The drive's time charged for it (console timing), for the statistics.
    pub cost_ns: u64,
    /// When the call queued it, and how long the worker's host read or
    /// write took: for the statistics.
    pub issued: Instant,
    pub host_ns: u64,
}

/// Work for the closer thread, done in the order it was queued.
pub(super) enum CloseJob {
    /// A file the title wrote, closed or flushed: synced to the host's
    /// storage through a descriptor of the closer's own (the host path, and
    /// the Xbox path for the log).
    Sync(PathBuf, String),
    /// A written file with no host path to open again: synced through its
    /// own handle, then closed.
    Close(File),
}

/// What the workers finished, for the guest-CPU thread.
#[derive(Default)]
struct Shared {
    done: Mutex<Vec<Job>>,
    /// Set after a push to `done`: `io_poll`'s cheap test.
    pending: AtomicBool,
}

/// A thread fed through a channel; it ends when the sender is dropped.
struct Lane<T> {
    tx: Option<Sender<T>>,
    thread: Option<JoinHandle<()>>,
}

impl<T: Send + 'static> Lane<T> {
    fn spawn(name: &str, mut work: impl FnMut(T) + Send + 'static) -> Option<Self> {
        let (tx, rx) = mpsc::channel::<T>();
        let thread = std::thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || {
                for item in rx {
                    work(item);
                }
            })
            .map_err(|e| log::warn!(target: "kernel", "io: no {name} thread: {e}"))
            .ok()?;
        Some(Self { tx: Some(tx), thread: Some(thread) })
    }

    /// Queues `item`; gives it back if the thread is gone.
    fn send(&self, item: T) -> Result<(), T> {
        match &self.tx {
            Some(tx) => tx.send(item).map_err(|e| e.0),
            None => Err(item),
        }
    }
}

impl<T> Drop for Lane<T> {
    fn drop(&mut self) {
        // The thread finishes what is queued (a save's sync) and ends.
        drop(self.tx.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The I/O threads, each started when first needed: the DVD drive's and the
/// hard disk's workers, side by side as the console's two drives work (a slow
/// hard disk's writes do not hold the disc's reads up), and the closer.
pub(super) struct Workers {
    shared: Arc<Shared>,
    wake: WakeHandle,
    drives: [Option<Lane<Job>>; 2],
    closer: Option<Lane<CloseJob>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Workers {
    /// No thread yet. `wake` brings the guest-CPU thread to `io_poll` when a
    /// request is done.
    pub(super) fn new(wake: WakeHandle) -> Self {
        Self { shared: Arc::default(), wake, drives: [None, None], closer: None }
    }

    /// Hands a request to its drive's worker; gives it back when there is no
    /// worker (the request then runs in place).
    pub(super) fn submit(&mut self, job: Job) -> Result<(), Box<Job>> {
        let drive = job.drive;
        if self.drives[drive].is_none() {
            let shared = Arc::clone(&self.shared);
            let wake = self.wake.clone();
            let name = if drive == DISC { "xb-io-read" } else { "xb-io-read-hdd" };
            self.drives[drive] = Lane::spawn(name, move |mut job: Job| {
                let t0 = Instant::now();
                job.result = if job.write {
                    job.file.write(job.off, &job.data)
                } else {
                    job.data.resize(job.n as usize, 0);
                    job.file.read(job.off, &mut job.data)
                };
                job.host_ns = u64::try_from(t0.elapsed().as_nanos()).unwrap_or(u64::MAX);
                lock(&shared.done).push(job);
                shared.pending.store(true, Ordering::Release);
                wake.wake();
            });
        }
        match &self.drives[drive] {
            Some(lane) => lane.send(job).map_err(Box::new),
            None => Err(Box::new(job)),
        }
    }

    /// Whether a worker finished something since the last look; cleared.
    pub(super) fn take_pending(&self) -> bool {
        self.shared.pending.swap(false, Ordering::AcqRel)
    }

    /// The requests the workers finished, in the order they did.
    pub(super) fn take_done(&self) -> Vec<Job> {
        std::mem::take(&mut *lock(&self.shared.done))
    }

    /// Queues work for the closer thread; done in place if there is none.
    pub(super) fn close(&mut self, job: CloseJob) {
        if self.closer.is_none() {
            self.closer = Lane::spawn("xb-io-close", close_now);
        }
        let left = match &self.closer {
            Some(lane) => lane.send(job).err(),
            None => Some(job),
        };
        if let Some(job) = left {
            close_now(job);
        }
    }
}

/// What the title wrote is on the host's storage soon after it closes the
/// file, so an app killed or hung later cannot leave a half-written save. On
/// the guest's CPU the sync took 10-110 ms per file with nothing else
/// running, interrupts included: Forza's frames came out late while it wrote
/// its caches and replays.
fn close_now(job: CloseJob) {
    match job {
        CloseJob::Sync(host, path) => {
            // A descriptor of our own: the title's is closed, and its share
            // of the file with it, as the handle's slot is free at once in
            // the reference. A file deleted or renamed meanwhile has nothing
            // left to sync under this name.
            match std::fs::OpenOptions::new().write(true).open(&host) {
                Ok(file) => {
                    if let Err(e) = file.sync_all() {
                        log::info!(target: "fs", "sync {path}: {e}");
                    }
                }
                Err(e) => log::debug!(target: "fs", "sync {path}: {e}"),
            }
        }
        CloseJob::Close(file) => {
            file.flush();
        }
    }
}
