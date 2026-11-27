//! Test doubles the device tests share: guest RAM and a settable clock.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use xb_types::mem::PhysMem;

use crate::Clock;

/// Guest RAM backed by a vector.
pub(crate) struct TestRam(Mutex<Vec<u8>>);

impl TestRam {
    pub(crate) fn new(size: usize) -> Arc<Self> {
        Arc::new(Self(Mutex::new(vec![0; size])))
    }

    pub(crate) fn read_u16(&self, pa: u32) -> u16 {
        let mut b = [0u8; 2];
        assert!(self.read(pa, &mut b));
        u16::from_le_bytes(b)
    }

    pub(crate) fn bytes(&self, pa: u32, len: usize) -> Vec<u8> {
        let mut v = vec![0; len];
        assert!(self.read(pa, &mut v));
        v
    }
}

impl PhysMem for TestRam {
    fn size(&self) -> u32 {
        self.0.lock().unwrap().len() as u32
    }

    fn read(&self, pa: u32, buf: &mut [u8]) -> bool {
        let ram = self.0.lock().unwrap();
        match ram.get(pa as usize..pa as usize + buf.len()) {
            Some(src) => {
                buf.copy_from_slice(src);
                true
            }
            None => false,
        }
    }

    fn write(&self, pa: u32, data: &[u8]) -> bool {
        let mut ram = self.0.lock().unwrap();
        match ram.get_mut(pa as usize..pa as usize + data.len()) {
            Some(dst) => {
                dst.copy_from_slice(data);
                true
            }
            None => false,
        }
    }
}

/// A clock the test moves by hand.
#[derive(Default)]
pub(crate) struct TestClock(AtomicU64);

impl TestClock {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub(crate) fn step(&self, ns: u64) -> u64 {
        self.0.fetch_add(ns, Ordering::SeqCst) + ns
    }
}

impl Clock for TestClock {
    fn now_ns(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}
