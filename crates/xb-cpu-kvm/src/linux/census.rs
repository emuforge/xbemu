//! The exit census: how, why and for how long the guest leaves `KVM_RUN`.
//!
//! On with `XBEMU_CENSUS=1`; one INFO line group a second under `cpu`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// Whether the census runs.
pub(crate) fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("XBEMU_CENSUS").is_some_and(|v| v != "0"))
}

/// Why `KVM_RUN` ended (or was not entered).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(usize)]
pub(crate) enum Why {
    Trap,
    Halt,
    Exception,
    SoftInt,
    GuestPageFault,
    KickPending,
    KickSignal,
    SignalNoKick,
    IrqWindow,
    Interruptible,
    Mmio,
    MmioRam,
    Pio,
    Efault,
    EfaultUnexplained,
    Flush,
    Msr,
    Other,
    Count,
}

const NAMES: [&str; Why::Count as usize] = [
    "trap(kernel call)",
    "halt(idle)",
    "exception",
    "soft-int",
    "guest #PF resolved",
    "kick already pending (no entry)",
    "kick signal",
    "signal, no kick",
    "irq window",
    "interruptible (no entry)",
    "mmio",
    "mmio-on-ram (emulator)",
    "pio",
    "EFAULT write-track",
    "EFAULT unexplained",
    "tlb flush stub",
    "msr",
    "other",
];

#[derive(Default, Clone, Copy)]
struct Slot {
    n: u64,
    wall_ns: u64,
    cpu_ns: u64,
    /// The `KVM_RUN` that ended this way: total, shortest, and how many of
    /// them took under 8 microseconds with their total (mostly exit cost).
    run_ns: u64,
    run_min: u64,
    short_n: u64,
    short_ns: u64,
}

/// Per-thread counters shared by kicks and protects.
#[derive(Default)]
pub(crate) struct Shared {
    /// thread name -> (kicks, kicks that sent a signal)
    kicks: Mutex<HashMap<String, [u64; 2]>>,
    /// thread name -> (calls, calls that reached mprotect, pages, ns in mprotect, to-RW calls)
    protects: Mutex<HashMap<String, [u64; 5]>>,
}

pub(crate) fn shared() -> &'static Shared {
    static S: OnceLock<Shared> = OnceLock::new();
    S.get_or_init(Shared::default)
}

fn thread_name() -> String {
    std::thread::current().name().unwrap_or("?").to_owned()
}

/// Counts a kick from the calling thread.
pub(crate) fn note_kick(signalled: bool) {
    if !enabled() {
        return;
    }
    let mut k = shared().kicks.lock().unwrap();
    let e = k.entry(thread_name()).or_default();
    e[0] += 1;
    e[1] += u64::from(signalled);
}

/// Counts a protect call from the calling thread.
pub(crate) fn note_protect(syscall: bool, pages: u64, ns: u64, to_rw: bool) {
    let mut k = shared().protects.lock().unwrap();
    let e = k.entry(thread_name()).or_default();
    e[0] += 1;
    e[1] += u64::from(syscall);
    e[2] += pages;
    e[3] += ns;
    e[4] += u64::from(to_rw && syscall);
}

/// Extra counters anybody may bump.
pub(crate) static FPU_SETS: AtomicU64 = AtomicU64::new(0);
pub(crate) static FPU_GETS: AtomicU64 = AtomicU64::new(0);
pub(crate) static FS_SETS: AtomicU64 = AtomicU64::new(0);

fn thread_cpu_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: a plain system call writing one timespec.
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

/// The CPU thread's census.
pub(crate) struct Census {
    on: bool,
    slots: [Slot; Why::Count as usize],
    /// Exceptions and software interrupts by vector.
    vectors: HashMap<(bool, u8), u64>,
    /// MMIO by (address, write): count, ns.
    mmio: HashMap<(u32, bool), [u64; 2]>,
    /// Ports by (port, write): count, ns.
    pio: HashMap<(u16, bool), [u64; 2]>,
    /// EFAULT by faulting physical page: count.
    efault_pages: HashMap<u32, u64>,
    /// Trap exits by EIP (which kernel entry): count, wall ns, cpu ns.
    traps: HashMap<u32, [u64; 3]>,
    in_guest_ns: u64,
    runs: u64,
    last: Option<(Why, Instant, u64)>,
    last_key: u32,
    last_write: bool,
    last_run_ns: u64,
    hist: [u64; 24],
    enter_at: Instant,
    dump_at: Instant,
    cpu_at_dump: u64,
}

impl Census {
    pub(crate) fn new() -> Self {
        let now = Instant::now();
        Self {
            on: enabled(),
            slots: [Slot::default(); Why::Count as usize],
            vectors: HashMap::new(),
            mmio: HashMap::new(),
            pio: HashMap::new(),
            efault_pages: HashMap::new(),
            traps: HashMap::new(),
            in_guest_ns: 0,
            runs: 0,
            last: None,
            last_key: 0,
            last_write: false,
            last_run_ns: 0,
            hist: [0; 24],
            enter_at: now,
            dump_at: now,
            cpu_at_dump: 0,
        }
    }

    /// Just before `KVM_RUN`: closes the time outside the guest.
    #[inline]
    pub(crate) fn enter(&mut self) {
        if !self.on {
            return;
        }
        let now = Instant::now();
        self.close(now);
        self.enter_at = now;
        self.runs += 1;
    }

    fn close(&mut self, now: Instant) {
        if let Some((why, at, cpu0)) = self.last.take() {
            let ns = now.duration_since(at).as_nanos() as u64;
            let s = &mut self.slots[why as usize];
            s.wall_ns += ns;
            let mut cpu = 0;
            if cpu0 != 0 {
                cpu = thread_cpu_ns().saturating_sub(cpu0);
                s.cpu_ns += cpu;
            }
            match why {
                Why::Mmio => {
                    self.mmio.entry((self.last_key, self.last_write)).or_default()[1] += ns;
                }
                Why::Pio => {
                    self.pio.entry((self.last_key as u16, self.last_write)).or_default()[1] += ns;
                }
                Why::Trap => {
                    let e = self.traps.entry(self.last_key).or_default();
                    e[1] += ns;
                    e[2] += cpu;
                }
                _ => {}
            }
        }
    }

    /// Just after `KVM_RUN` returned.
    #[inline]
    pub(crate) fn left(&mut self) {
        if !self.on {
            return;
        }
        let now = Instant::now();
        let d = now.duration_since(self.enter_at).as_nanos() as u64;
        self.in_guest_ns += d;
        self.last_run_ns = d;
        self.hist[(63 - d.max(1).leading_zeros() as usize).min(23)] += 1;
        // Until classified.
        self.last = Some((Why::Other, now, 0));
    }

    /// Names the exit `left` saw. `returns`: control goes back to the caller
    /// of `run`, so CPU time is sampled too (the wall time may hold a sleep).
    #[inline]
    pub(crate) fn why(&mut self, why: Why, returns: bool) {
        if !self.on {
            return;
        }
        self.slots[why as usize].n += 1;
        match &mut self.last {
            Some(l) => {
                let s = &mut self.slots[why as usize];
                let d = self.last_run_ns;
                s.run_ns += d;
                if s.run_min == 0 || d < s.run_min {
                    s.run_min = d;
                }
                if d < 8000 {
                    s.short_n += 1;
                    s.short_ns += d;
                }
                l.0 = why;
                if returns {
                    l.2 = thread_cpu_ns();
                }
            }
            None => {
                // No guest entry happened (pending kick and the like).
                self.last = Some((why, Instant::now(), if returns { thread_cpu_ns() } else { 0 }));
            }
        }
    }

    pub(crate) fn vector(&mut self, soft: bool, v: u8) {
        if self.on {
            *self.vectors.entry((soft, v)).or_default() += 1;
        }
    }

    pub(crate) fn mmio(&mut self, addr: u32, write: bool) {
        if self.on {
            self.mmio.entry((addr, write)).or_default()[0] += 1;
            self.last_key = addr;
            self.last_write = write;
        }
    }

    pub(crate) fn pio(&mut self, port: u16, write: bool) {
        if self.on {
            self.pio.entry((port, write)).or_default()[0] += 1;
            self.last_key = u32::from(port);
            self.last_write = write;
        }
    }

    pub(crate) fn efault_page(&mut self, pa: u32) {
        if self.on {
            *self.efault_pages.entry(pa >> 12).or_default() += 1;
        }
    }

    pub(crate) fn trap(&mut self, eip: u32) {
        if self.on {
            self.traps.entry(eip).or_default()[0] += 1;
            self.last_key = eip;
        }
    }

    /// Logs and clears, once a second. Called at the end of `run`.
    pub(crate) fn maybe_dump(&mut self) {
        if !self.on {
            return;
        }
        let now = Instant::now();
        let dt = now.duration_since(self.dump_at);
        if dt.as_millis() < 1000 {
            return;
        }
        let s = dt.as_secs_f64();
        let cpu = thread_cpu_ns();
        let thread_cpu = cpu.saturating_sub(self.cpu_at_dump);
        self.cpu_at_dump = cpu;
        self.dump_at = now;
        let out_wall: u64 = self.slots.iter().map(|x| x.wall_ns).sum();
        log::info!(target: "cpu",
            "census: {:.0} KVM_RUN/s; thread cpu {:.1}%; in KVM_RUN (wall) {:.1}%; outside (wall) {:.1}%; fpu gets {:.0} sets {:.0} fs sets {:.0}",
            self.runs as f64 / s,
            thread_cpu as f64 / s / 1e7,
            self.in_guest_ns as f64 / s / 1e7,
            out_wall as f64 / s / 1e7,
            FPU_GETS.swap(0, Ordering::Relaxed) as f64 / s,
            FPU_SETS.swap(0, Ordering::Relaxed) as f64 / s,
            FS_SETS.swap(0, Ordering::Relaxed) as f64 / s,
        );
        for (i, x) in self.slots.iter().enumerate() {
            if x.n == 0 {
                continue;
            }
            log::info!(target: "cpu",
                "census:   {:<32} {:>8.0}/s  outside: wall {:>6.2}% ({:>7.0} ns each)  cpu {:>6.2}% | the KVM_RUN before it: {:>6.2}% total, min {} ns, {:.0}% of them under 8 us averaging {:.0} ns",
                NAMES[i], x.n as f64 / s, x.wall_ns as f64 / s / 1e7,
                x.wall_ns as f64 / x.n as f64, x.cpu_ns as f64 / s / 1e7,
                x.run_ns as f64 / s / 1e7, x.run_min,
                x.short_n as f64 * 100.0 / x.n as f64, x.short_ns as f64 / x.short_n.max(1) as f64);
        }
        let h: Vec<String> = self
            .hist
            .iter()
            .enumerate()
            .filter(|(_, n)| **n != 0)
            .map(|(i, n)| format!("2^{i}:{:.0}", *n as f64 / s))
            .collect();
        log::info!(target: "cpu", "census:   KVM_RUN length (ns, log2 buckets) per s: {}", h.join(" "));
        self.hist = [0; 24];
        let mut v: Vec<_> = self.vectors.drain().collect();
        v.sort_by_key(|x| std::cmp::Reverse(x.1));
        if !v.is_empty() {
            let t: Vec<String> = v
                .iter()
                .map(|((soft, n), c)| {
                    format!("{}{n}:{:.0}", if *soft { "int" } else { "exc" }, *c as f64 / s)
                })
                .collect();
            log::info!(target: "cpu", "census:   vectors/s: {}", t.join(" "));
        }
        // MMIO by 4 KiB block, then top registers.
        let mut blocks: HashMap<u32, [u64; 3]> = HashMap::new();
        for ((a, w), c) in &self.mmio {
            let e = blocks.entry(a >> 12).or_default();
            e[usize::from(*w)] += c[0];
            e[2] += c[1];
        }
        let mut b: Vec<_> = blocks.into_iter().collect();
        b.sort_by_key(|x| std::cmp::Reverse(x.1[2]));
        for (blk, c) in b.iter().take(10) {
            log::info!(target: "cpu",
                "census:   mmio block {:08x}: {:>6.0} rd/s {:>6.0} wr/s  {:>5.2}%",
                blk << 12, c[0] as f64 / s, c[1] as f64 / s, c[2] as f64 / s / 1e7);
        }
        let mut m: Vec<_> = self.mmio.drain().collect();
        m.sort_by_key(|x| std::cmp::Reverse(x.1[1]));
        for ((a, w), c) in m.iter().take(16) {
            log::info!(target: "cpu",
                "census:   mmio {} {:08x}: {:>6.0}/s  {:>5.2}%  ({:.0} ns each)",
                if *w { "wr" } else { "rd" }, a, c[0] as f64 / s, c[1] as f64 / s / 1e7,
                c[1] as f64 / c[0].max(1) as f64);
        }
        let mut p: Vec<_> = self.pio.drain().collect();
        p.sort_by_key(|x| std::cmp::Reverse(x.1[1]));
        for ((a, w), c) in p.iter().take(6) {
            log::info!(target: "cpu",
                "census:   pio {} {:04x}: {:>6.0}/s  {:>5.2}%",
                if *w { "out" } else { "in " }, a, c[0] as f64 / s, c[1] as f64 / s / 1e7);
        }
        let mut t: Vec<_> = self.traps.drain().collect();
        t.sort_by_key(|x| std::cmp::Reverse(x.1[2]));
        for (eip, c) in t.iter().take(12) {
            log::info!(target: "cpu",
                "census:   trap at {:08x}: {:>6.0}/s  wall {:>5.2}%  cpu {:>5.2}% ({:.0} ns cpu each)",
                eip, c[0] as f64 / s, c[1] as f64 / s / 1e7, c[2] as f64 / s / 1e7,
                c[2] as f64 / c[0].max(1) as f64);
        }
        let pages = self.efault_pages.len();
        let mut e: Vec<_> = self.efault_pages.drain().collect();
        e.sort_by_key(|x| std::cmp::Reverse(x.1));
        if pages != 0 {
            let t: Vec<String> =
                e.iter().take(12).map(|(p, c)| format!("{:05x}:{:.0}", p, *c as f64 / s)).collect();
            log::info!(target: "cpu", "census:   EFAULT on {pages} distinct pages; top pages/s: {}", t.join(" "));
        }
        let kicks: Vec<String> = shared()
            .kicks
            .lock()
            .unwrap()
            .drain()
            .map(|(n, c)| format!("{n}: {:.0} ({:.0} signalled)", c[0] as f64 / s, c[1] as f64 / s))
            .collect();
        log::info!(target: "cpu", "census:   kicks/s by thread: {}", kicks.join(", "));
        let prot: Vec<String> = shared()
            .protects
            .lock()
            .unwrap()
            .drain()
            .map(|(n, c)| {
                format!(
                    "{n}: {:.0} calls, {:.0} mprotects ({:.0} to RW), {:.0} pages, {:.2}% of a core in mprotect ({:.0} ns each)",
                    c[0] as f64 / s, c[1] as f64 / s, c[4] as f64 / s, c[2] as f64 / s,
                    c[3] as f64 / s / 1e7, c[3] as f64 / c[1].max(1) as f64
                )
            })
            .collect();
        log::info!(target: "cpu", "census:   protect by thread: {}", prot.join("; "));
        self.slots = [Slot::default(); Why::Count as usize];
        self.in_guest_ns = 0;
        self.runs = 0;
    }
}
