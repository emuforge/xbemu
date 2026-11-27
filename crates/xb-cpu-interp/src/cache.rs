//! Decoded blocks: runs of guest instructions decoded once and kept.
//!
//! A block starts at a guest address and ends after a control transfer,
//! after [`MAX_LEN`] instructions, at the end of its page, or before the
//! trap range. An instruction that straddles two pages is the block's last
//! and the block depends on both pages.
//!
//! A block is valid while the page-table entries it was decoded under are
//! unchanged (checked on every lookup, so `map` and `unmap` need do
//! nothing) and its physical pages have not been written. A guest store to
//! a page holding decoded bytes is seen by [`crate::mem::Mem`]; the run loop
//! then drops that page's blocks. The host's own writes to code arrive
//! through `Cpu::invalidate_code`.

use std::collections::HashMap;

use iced_x86::{Decoder, DecoderError, DecoderOptions, FlowControl};

use crate::mem::{Mem, PageFault};

/// Instructions per block at most.
const MAX_LEN: usize = 64;
/// Blocks kept before the cache starts again from nothing.
const MAX_BLOCKS: usize = 1 << 17;
/// Entries in the direct-mapped lookup in front of the map.
const FAST: usize = 1 << 14;
/// Runs after which a block is worth compiling to host code.
const HOT_AFTER: u32 = 3;
/// Times a page's blocks may be dropped before its code is no longer
/// compiled: a page whose data sits beside its code is written all the
/// time, and compiling its blocks again each time costs more than
/// interpreting them.
const CHURN_MAX: u8 = 16;

/// One decoded block.
pub(crate) struct Block {
    pub(crate) start: u32,
    /// Page-table entries of the first page and, for a block whose last
    /// instruction crosses into the next page, of that one (else 0).
    pte: [u32; 2],
    /// The block's instructions, compiled (`fast`); shared so the run loop
    /// can hold them while the cache changes.
    pub(crate) ops: Box<[crate::fast::Uop]>,
    valid: bool,
    /// The physical page of the first instruction.
    page: u32,
    /// Times the block has run.
    runs: u32,
    /// Its host code (`jit`), once compiled.
    entry: Option<usize>,
}

/// Why no block could be made.
pub(crate) enum DecodeError {
    /// The first instruction's bytes are not readable.
    Fault(PageFault),
    /// The first instruction is not a valid one (`#UD`).
    Invalid,
    /// Code in device memory.
    Device,
}

/// All blocks, by start address.
pub(crate) struct Cache {
    blocks: Vec<Block>,
    by_start: HashMap<u32, u32>,
    /// `(eip, index + 1)`; 0 is an empty slot.
    fast: Box<[(u32, u32)]>,
    by_page: HashMap<u32, Vec<u32>>,
    /// Per physical page: times its blocks were dropped.
    churn: HashMap<u32, u8>,
    /// Runs after which a block is compiled; 0 never compiles (the tests
    /// set it).
    pub(crate) hot_after: u32,
}

impl Cache {
    pub(crate) fn new() -> Self {
        Self {
            blocks: Vec::new(),
            by_start: HashMap::new(),
            fast: vec![(0, 0); FAST].into_boxed_slice(),
            by_page: HashMap::new(),
            churn: HashMap::new(),
            hot_after: HOT_AFTER,
        }
    }

    /// Block `i`'s host code, if it has been compiled.
    pub(crate) fn entry(&self, i: u32) -> Option<usize> {
        self.blocks[i as usize].entry
    }

    pub(crate) fn set_entry(&mut self, i: u32, entry: usize) {
        self.blocks[i as usize].entry = Some(entry);
    }

    /// Counts a run of block `i`; true when it has now run often enough to
    /// compile and its page's code is stable enough.
    pub(crate) fn became_hot(&mut self, i: u32) -> bool {
        let b = &mut self.blocks[i as usize];
        b.runs = b.runs.saturating_add(1);
        b.runs == self.hot_after && self.churn.get(&b.page).is_none_or(|&n| n < CHURN_MAX)
    }

    /// Moves block `i`'s ops out, for running them.
    pub(crate) fn take_ops(&mut self, i: u32) -> Box<[crate::fast::Uop]> {
        std::mem::take(&mut self.blocks[i as usize].ops)
    }

    /// Gives block `i` its ops back, unless it was dropped meanwhile (or the
    /// cache started again).
    pub(crate) fn put_ops(&mut self, i: u32, ops: Box<[crate::fast::Uop]>) {
        if let Some(b) = self.blocks.get_mut(i as usize)
            && b.valid
            && b.ops.is_empty()
        {
            b.ops = ops;
        }
    }

    pub(crate) fn is_valid(&self, i: u32) -> bool {
        self.blocks[i as usize].valid
    }

    fn current(&self, i: u32, mem: &Mem) -> bool {
        let b = &self.blocks[i as usize];
        b.valid
            && mem.entry(b.start) == b.pte[0]
            && (b.pte[1] == 0 || mem.entry(b.end_page()) == b.pte[1])
    }

    /// The valid block starting at `eip`.
    #[inline]
    pub(crate) fn lookup(&mut self, eip: u32, mem: &Mem) -> Option<u32> {
        let slot = (eip as usize) & (FAST - 1);
        let (s, i1) = self.fast[slot];
        if s == eip && i1 != 0 && self.current(i1 - 1, mem) {
            return Some(i1 - 1);
        }
        let i = *self.by_start.get(&eip)?;
        if self.current(i, mem) {
            self.fast[slot] = (eip, i + 1);
            Some(i)
        } else {
            self.drop_block(i, mem);
            None
        }
    }

    /// Decodes the block at `eip`. `trap` is the trap range.
    pub(crate) fn decode(
        &mut self,
        eip: u32,
        mem: &mut Mem,
        trap: (u32, u64),
    ) -> Result<u32, DecodeError> {
        if self.blocks.len() >= MAX_BLOCKS {
            self.clear(mem);
        }
        let mut bytes = match mem.fetch_page(eip) {
            Ok(Some(b)) => b,
            Ok(None) => return Err(DecodeError::Device),
            Err(f) => return Err(DecodeError::Fault(f)),
        };
        let first_page = mem.code_page(eip).ok_or(DecodeError::Device)?;
        let in_page = bytes.len();
        let next = eip.wrapping_add(in_page as u32);
        // Up to 15 bytes of the next page, for an instruction that crosses.
        let next_ok = match mem.fetch_page(next) {
            Ok(Some(more)) => {
                bytes.extend_from_slice(&more[..more.len().min(15)]);
                true
            }
            _ => false,
        };
        let mut dec = Decoder::with_ip(32, &bytes, u64::from(eip), DecoderOptions::NONE);
        let mut ins = Vec::new();
        let mut crosses = false;
        while dec.can_decode() && dec.position() < in_page {
            let i = dec.decode();
            if dec.last_error() != DecoderError::None {
                if ins.is_empty() {
                    if dec.last_error() == DecoderError::NoMoreBytes && !next_ok {
                        // The rest of the instruction is on a page the guest
                        // cannot read.
                        return match mem.fetch_page(next) {
                            Err(f) => Err(DecodeError::Fault(f)),
                            _ => Err(DecodeError::Device),
                        };
                    }
                    return Err(DecodeError::Invalid);
                }
                break;
            }
            let end = dec.position();
            ins.push(i);
            if end > in_page {
                crosses = true;
                break;
            }
            let nip = i.next_ip32();
            if i.flow_control() != FlowControl::Next
                || ins.len() >= MAX_LEN
                || (nip >= trap.0 && u64::from(nip) < trap.1)
            {
                break;
            }
        }
        let (pte1, page1) = if crosses {
            (mem.entry(next), mem.code_page(next).unwrap_or(u32::MAX))
        } else {
            (0, u32::MAX)
        };
        let idx = self.blocks.len() as u32;
        let ops = ins.iter().map(crate::fast::compile).collect();
        self.blocks.push(Block {
            start: eip,
            pte: [mem.entry(eip), pte1],
            ops,
            valid: true,
            page: first_page,
            runs: 0,
            entry: None,
        });
        for p in [first_page, page1] {
            if p != u32::MAX {
                self.by_page.entry(p).or_default().push(idx);
                mem.mark_code(p);
            }
        }
        if let Some(old) = self.by_start.insert(eip, idx) {
            self.blocks[old as usize].valid = false;
        }
        self.fast[(eip as usize) & (FAST - 1)] = (eip, idx + 1);
        Ok(idx)
    }

    fn drop_block(&mut self, i: u32, _mem: &Mem) {
        let b = &mut self.blocks[i as usize];
        b.valid = false;
        b.entry = None;
        b.ops = Box::new([]);
        let start = b.start;
        if self.by_start.get(&start) == Some(&i) {
            self.by_start.remove(&start);
        }
    }

    /// Drops every block holding bytes of physical page `page`.
    pub(crate) fn invalidate_page(&mut self, page: u32, mem: &mut Mem) {
        if let Some(list) = self.by_page.remove(&page) {
            if !list.is_empty() {
                let n = self.churn.entry(page).or_default();
                *n = n.saturating_add(1);
            }
            for i in list {
                self.drop_block(i, mem);
            }
        }
        mem.clear_code(page);
    }

    /// Drops everything.
    pub(crate) fn clear(&mut self, mem: &mut Mem) {
        for &p in self.by_page.keys() {
            mem.clear_code(p);
        }
        self.blocks.clear();
        self.by_start.clear();
        self.by_page.clear();
        self.fast.fill((0, 0));
    }
}

impl Block {
    /// The address of the page the block's last instruction crosses into.
    fn end_page(&self) -> u32 {
        (self.start & !0xFFF).wrapping_add(0x1000)
    }
}
