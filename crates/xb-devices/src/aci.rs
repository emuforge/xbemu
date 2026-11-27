//! The MCPX Audio Codec Interface: an AC'97 controller (Intel ICH register
//! set) at [`ACI_BASE`], [`Irq::Aci`], with a STAC9700 codec. Mixer (NAM)
//! registers at +0x000, bus master (NABM) at +0x100.
//!
//! DirectSound waits for codec 0 to report ready and runs PCM out through a
//! buffer descriptor list. The sound itself comes from the APU, so PCM out is
//! consumed at the real-time rate (48 kHz, 16-bit stereo) and dropped: the
//! list advances, completions set status and interrupt as on hardware.
//! Register behaviour follows the AC'97 model the Xbox titles run against in
//! xemu (QEMU's ac97 behind mcpx-aci); S/PDIF out does not advance there and
//! does not here. No samples leave this device, so it has no audio sink.
//!
//! Pacing comes from [`Aci::advance`]: every [`PERIOD_NS`] of guest time the
//! PCM out channel consumes [`PERIOD_BYTES`] bytes.

use std::sync::Arc;

use xb_types::irq::{Irq, IrqHub};
use xb_types::mem::PhysMem;

/// Physical address of the interface's registers.
pub const ACI_BASE: u32 = 0xFEC0_0000;
/// Length of the register window.
pub const ACI_SIZE: u32 = 0x1000;
/// Offset of the bus master (NABM) registers; the mixer (NAM) is below.
pub const NABM_OFFSET: u32 = 0x100;
/// How often PCM out consumes samples.
pub const PERIOD_NS: u64 = 5_000_000;
/// Bytes PCM out consumes per period: 240 frames of 16-bit stereo, 5 ms at
/// 48 kHz.
pub const PERIOD_BYTES: u32 = 240 * 4;

/// Most periods one [`Aci::advance`] runs to catch up; a longer gap (the
/// machine was stopped) is dropped rather than replayed.
const MAX_CATCH_UP_PERIODS: u32 = 20;

/// Bus master channels: the index of a channel's 16-byte register block.
pub mod channel {
    /// PCM in.
    pub const PI: usize = 0;
    /// PCM out.
    pub const PO: usize = 1;
    /// Microphone in.
    pub const MC: usize = 2;
    /// S/PDIF out.
    pub const SO: usize = 7;
    /// Number of channel register blocks.
    pub const COUNT: usize = 8;
}

/// Bus master register offsets, from [`NABM_OFFSET`]. The channel registers
/// are relative to `channel * 16`.
pub mod nabm {
    /// Buffer descriptor list base address (32 bits).
    pub const BDBAR: u32 = 0x0;
    /// Current index value (8 bits).
    pub const CIV: u32 = 0x4;
    /// Last valid index (8 bits).
    pub const LVI: u32 = 0x5;
    /// Status (16 bits).
    pub const SR: u32 = 0x6;
    /// Position in current buffer, in samples (16 bits).
    pub const PICB: u32 = 0x8;
    /// Prefetched index value (8 bits).
    pub const PIV: u32 = 0xA;
    /// Control (8 bits).
    pub const CR: u32 = 0xB;
    /// Global control (32 bits).
    pub const GLOB_CNT: u32 = 0x2C;
    /// Global status (32 bits).
    pub const GLOB_STA: u32 = 0x30;
    /// Codec access semaphore (8 bits).
    pub const CAS: u32 = 0x34;
}

/// Channel status register bits.
pub mod sr {
    /// DMA controller halted.
    pub const DCH: u16 = 0x01;
    /// Current equals last valid.
    pub const CELV: u16 = 0x02;
    /// Last valid buffer completion interrupt.
    pub const LVBCI: u16 = 0x04;
    /// Buffer completion interrupt status.
    pub const BCIS: u16 = 0x08;
    /// FIFO error.
    pub const FIFOE: u16 = 0x10;
}

/// Channel control register bits.
pub mod cr {
    /// Run / pause bus master.
    pub const RPBM: u8 = 0x01;
    /// Reset registers.
    pub const RR: u8 = 0x02;
    /// Last valid buffer interrupt enable.
    pub const LVBIE: u8 = 0x04;
    /// FIFO error interrupt enable.
    pub const FEIE: u8 = 0x08;
    /// Interrupt on completion enable.
    pub const IOCE: u8 = 0x10;
}

/// Global status bits.
pub mod glob_sta {
    /// PCM in interrupt.
    pub const PIINT: u32 = 1 << 5;
    /// PCM out interrupt.
    pub const POINT: u32 = 1 << 6;
    /// Microphone in interrupt.
    pub const MINT: u32 = 1 << 7;
    /// Codec 0 ready.
    pub const S0CR: u32 = 1 << 8;
}

/// Buffer descriptor control bits (the descriptor's second word; its low 16
/// bits are the buffer's length in samples).
pub mod bd {
    /// Interrupt on completion.
    pub const IOC: u32 = 1 << 31;
    /// Buffer underrun policy.
    pub const BUP: u32 = 1 << 30;
}

/// Codec (mixer) register offsets.
pub mod mixer {
    /// Reset.
    pub const RESET: u32 = 0x00;
    /// Master volume / mute.
    pub const MASTER_VOLUME_MUTE: u32 = 0x02;
    /// PCM out volume / mute.
    pub const PCM_OUT_VOLUME_MUTE: u32 = 0x18;
    /// Record select.
    pub const RECORD_SELECT: u32 = 0x1A;
    /// Record gain / mute.
    pub const RECORD_GAIN_MUTE: u32 = 0x1C;
    /// Powerdown control / status.
    pub const POWERDOWN_CTRL_STAT: u32 = 0x26;
    /// Extended audio ID.
    pub const EXTENDED_AUDIO_ID: u32 = 0x28;
    /// Extended audio control / status.
    pub const EXTENDED_AUDIO_CTRL_STAT: u32 = 0x2A;
    /// PCM front DAC rate.
    pub const PCM_FRONT_DAC_RATE: u32 = 0x2C;
    /// PCM surround DAC rate.
    pub const PCM_SURROUND_DAC_RATE: u32 = 0x2E;
    /// PCM LFE DAC rate.
    pub const PCM_LFE_DAC_RATE: u32 = 0x30;
    /// PCM L/R ADC rate.
    pub const PCM_LR_ADC_RATE: u32 = 0x32;
    /// Microphone ADC rate.
    pub const MIC_ADC_RATE: u32 = 0x34;
    /// Vendor ID 1.
    pub const VENDOR_ID1: u32 = 0x7C;
    /// Vendor ID 2.
    pub const VENDOR_ID2: u32 = 0x7E;
    /// Extended audio control: variable rate audio.
    pub const EACS_VRA: u16 = 1;
    /// Extended audio control: variable rate microphone.
    pub const EACS_VRM: u16 = 8;
}

const SR_RO_MASK: u16 = sr::DCH | sr::CELV;
const SR_WCLEAR_MASK: u16 = sr::FIFOE | sr::BCIS | sr::LVBCI;
const SR_INT_MASK: u16 = sr::FIFOE | sr::BCIS | sr::LVBCI;

const CR_VALID_MASK: u8 = 0x1F;
const CR_DONT_CLEAR_MASK: u8 = cr::IOCE | cr::FEIE | cr::LVBIE;

const GC_WR: u32 = 4;
const GC_CR: u32 = 2;
const GC_VALID_MASK: u32 = 0x3F;
const GS_RO_MASK: u32 = (1 << 14) | (1 << 13) | (1 << 12) | (1 << 9) | (1 << 8) | 0xF6;
const GS_WCLEAR_MASK: u32 = (1 << 15) | (1 << 11) | (1 << 10) | 1;
const GS_VALID_MASK: u32 = (1 << 18) - 1;

#[derive(Clone, Copy, Default)]
struct Channel {
    bdbar: u32,
    civ: u8,
    lvi: u8,
    piv: u8,
    cr: u8,
    sr: u16,
    picb: u16,
    bd_valid: bool,
    bd_addr: u32,
    bd_ctl: u32,
}

/// The audio codec interface.
pub struct Aci {
    mem: Arc<dyn PhysMem>,
    irq: Arc<IrqHub>,
    glob_cnt: u32,
    glob_sta: u32,
    cas: u32,
    ch: [Channel; channel::COUNT],
    mixer: [u8; 256],
    irq_level: bool,
    po_bytes: u64,
    /// When the next period is due; `None` until the first `advance`.
    next_period_ns: Option<u64>,
}

impl Aci {
    /// The interface in its power-on state. `mem` is guest RAM (the buffer
    /// descriptor lists are read there), `irq` the hub [`Irq::Aci`] is
    /// asserted on.
    #[must_use]
    pub fn new(mem: Arc<dyn PhysMem>, irq: Arc<IrqHub>) -> Self {
        let mut aci = Self {
            mem,
            irq,
            glob_cnt: 0,
            glob_sta: 0,
            cas: 0,
            ch: [Channel::default(); channel::COUNT],
            mixer: [0; 256],
            irq_level: false,
            po_bytes: 0,
            next_period_ns: None,
        };
        aci.reset();
        aci
    }

    /// Power-on state: every channel halted, the codec at its defaults, the
    /// interrupt line low.
    pub fn reset(&mut self) {
        self.glob_cnt = 0;
        self.glob_sta = 0;
        self.cas = 0;
        self.ch = [Channel { sr: sr::DCH, ..Channel::default() }; channel::COUNT];
        self.mixer_reset();
        self.po_bytes = 0;
        self.next_period_ns = None;
        self.update_irq();
    }

    /// Bytes of PCM out consumed since reset.
    #[must_use]
    pub fn pcm_out_bytes(&self) -> u64 {
        self.po_bytes
    }

    /// When the next PCM period is due on the guest's clock (`None` before
    /// the first [`Aci::advance`]).
    #[must_use]
    pub fn next_deadline_ns(&self) -> Option<u64> {
        self.next_period_ns
    }

    /// Whether PCM out runs: a period then moves the buffer list and can
    /// interrupt.
    #[must_use]
    pub fn po_running(&self) -> bool {
        self.ch[channel::PO].sr & sr::DCH == 0 && self.ch[channel::PO].cr & cr::RPBM != 0
    }

    /// Consumes the PCM out due up to `now_ns` (the guest's clock):
    /// [`PERIOD_BYTES`] for each [`PERIOD_NS`] that passed. Call it at least
    /// every period while the guest runs.
    pub fn advance(&mut self, now_ns: u64) {
        let next = self.next_period_ns.get_or_insert(now_ns + PERIOD_NS);
        let mut ran = 0;
        while now_ns >= *next {
            if ran == MAX_CATCH_UP_PERIODS {
                *next = now_ns + PERIOD_NS;
                break;
            }
            *next += PERIOD_NS;
            ran += 1;
        }
        for _ in 0..ran {
            self.transfer_po(PERIOD_BYTES);
        }
    }

    /// Reads `size` (1, 2 or 4) bytes at `offset` from [`ACI_BASE`]. Mixer
    /// registers are 16 bits wide and read as all ones at other sizes.
    #[must_use]
    pub fn mmio_read(&mut self, offset: u32, size: u8) -> u32 {
        if offset < NABM_OFFSET {
            self.cas = 0;
            return if size == 2 {
                u32::from(self.mixer_load(offset))
            } else {
                crate::size_mask(size)
            };
        }
        let o = offset - NABM_OFFSET;
        match size {
            1 => self.nabm_readb(o),
            2 => self.nabm_readw(o),
            _ => self.nabm_readl(o),
        }
    }

    /// Writes `size` (1, 2 or 4) bytes at `offset` from [`ACI_BASE`]. Mixer
    /// registers take 16-bit writes only.
    pub fn mmio_write(&mut self, offset: u32, size: u8, value: u32) {
        if offset < NABM_OFFSET {
            self.cas = 0;
            if size == 2 {
                self.nam_write(offset, value as u16);
            }
            return;
        }
        let o = offset - NABM_OFFSET;
        match size {
            1 => self.nabm_writeb(o, value),
            2 => self.nabm_writew(o, value),
            _ => self.nabm_writel(o, value),
        }
    }

    fn update_irq(&mut self) {
        let level = self.glob_sta & (glob_sta::PIINT | glob_sta::POINT | glob_sta::MINT) != 0;
        if level != self.irq_level {
            log::trace!(target: "dev", "aci: irq {level} (glob_sta {:08x})", self.glob_sta);
            self.irq_level = level;
            self.irq.set(Irq::Aci, level);
        }
    }

    fn fetch_bd(&mut self, i: usize) {
        let c = &mut self.ch[i];
        let pa = c.bdbar.wrapping_add(u32::from(c.civ) * 8);
        let mut b = [0u8; 8];
        if self.mem.read(pa, &mut b) {
            c.bd_addr = u32::from_le_bytes([b[0], b[1], b[2], b[3]]) & !3;
            c.bd_ctl = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
        } else {
            c.bd_addr = 0;
            c.bd_ctl = 0;
        }
        c.bd_valid = true;
        c.picb = c.bd_ctl as u16;
        log::trace!(
            target: "dev",
            "aci: ch {i} bd {} at {pa:08x}: addr {:08x} ctl {:08x} ({} samples)",
            c.civ, c.bd_addr, c.bd_ctl, c.picb
        );
    }

    /// The status register changes; the channel's interrupt follows its
    /// enables.
    fn update_sr(&mut self, i: usize, new_sr: u16) {
        let c = &mut self.ch[i];
        let new_mask = new_sr & SR_INT_MASK;
        let old_mask = c.sr & SR_INT_MASK;
        c.sr = new_sr;
        if new_mask == old_mask {
            return;
        }
        let bit = match i {
            channel::PI => glob_sta::PIINT,
            channel::PO => glob_sta::POINT,
            channel::MC => glob_sta::MINT,
            _ => 0,
        };
        if new_mask == 0 {
            self.glob_sta &= !bit;
        } else if (new_mask & sr::LVBCI != 0 && c.cr & cr::LVBIE != 0)
            || (new_mask & sr::BCIS != 0 && c.cr & cr::IOCE != 0)
        {
            self.glob_sta |= bit;
        } else {
            return;
        }
        self.update_irq();
    }

    fn reset_channel(&mut self, i: usize) {
        let c = &mut self.ch[i];
        c.bdbar = 0;
        c.civ = 0;
        c.lvi = 0;
        c.piv = 0;
        self.update_sr(i, sr::DCH);
        let c = &mut self.ch[i];
        c.picb = 0;
        c.cr &= CR_DONT_CLEAR_MASK;
        c.bd_valid = false;
    }

    /// The channel's next descriptor becomes current.
    fn next_bd(&mut self, i: usize) {
        let c = &mut self.ch[i];
        c.civ = c.piv;
        c.piv = (c.piv + 1) % 32;
        self.fetch_bd(i);
    }

    // ---------------------------------------------------------------- mixer

    fn mixer_store(&mut self, i: u32, v: u16) {
        if let Some(b) = self.mixer.get_mut(i as usize..i as usize + 2) {
            b.copy_from_slice(&v.to_le_bytes());
        }
    }

    fn mixer_load(&self, i: u32) -> u16 {
        match self.mixer.get(i as usize..i as usize + 2) {
            Some(b) => u16::from_le_bytes([b[0], b[1]]),
            None => 0xFFFF,
        }
    }

    fn mixer_reset(&mut self) {
        self.mixer = [0; 256];
        self.mixer_store(mixer::POWERDOWN_CTRL_STAT, 0x000F); // ADC, DAC, analog, Vref ready
        self.mixer_store(mixer::VENDOR_ID1, 0x8384); // SigmaTel STAC9700
        self.mixer_store(mixer::VENDOR_ID2, 0x7600);
        self.mixer_store(mixer::EXTENDED_AUDIO_ID, 0x0809);
        self.mixer_store(mixer::EXTENDED_AUDIO_CTRL_STAT, 0x0009);
        self.mixer_store(mixer::PCM_FRONT_DAC_RATE, 0xBB80);
        self.mixer_store(mixer::PCM_SURROUND_DAC_RATE, 0xBB80);
        self.mixer_store(mixer::PCM_LFE_DAC_RATE, 0xBB80);
        self.mixer_store(mixer::PCM_LR_ADC_RATE, 0xBB80);
        self.mixer_store(mixer::MIC_ADC_RATE, 0xBB80);
        self.mixer_store(mixer::MASTER_VOLUME_MUTE, 0x8000);
        self.mixer_store(mixer::PCM_OUT_VOLUME_MUTE, 0x8808);
        self.mixer_store(mixer::RECORD_GAIN_MUTE, 0x8808);
    }

    fn nam_write(&mut self, off: u32, v: u16) {
        match off {
            mixer::RESET => self.mixer_reset(),
            mixer::POWERDOWN_CTRL_STAT => {
                let ready = self.mixer_load(off) & 0xF;
                self.mixer_store(off, (v & !0x800F) | ready);
            }
            // Read only.
            mixer::VENDOR_ID1 | mixer::VENDOR_ID2 | mixer::EXTENDED_AUDIO_ID => {}
            mixer::EXTENDED_AUDIO_CTRL_STAT => {
                if v & mixer::EACS_VRA == 0 {
                    self.mixer_store(mixer::PCM_FRONT_DAC_RATE, 0xBB80);
                    self.mixer_store(mixer::PCM_LR_ADC_RATE, 0xBB80);
                }
                if v & mixer::EACS_VRM == 0 {
                    self.mixer_store(mixer::MIC_ADC_RATE, 0xBB80);
                }
                self.mixer_store(off, v);
            }
            mixer::PCM_FRONT_DAC_RATE | mixer::PCM_LR_ADC_RATE => {
                if self.mixer_load(mixer::EXTENDED_AUDIO_CTRL_STAT) & mixer::EACS_VRA != 0 {
                    self.mixer_store(off, v);
                }
            }
            mixer::MIC_ADC_RATE => {
                if self.mixer_load(mixer::EXTENDED_AUDIO_CTRL_STAT) & mixer::EACS_VRM != 0 {
                    self.mixer_store(off, v);
                }
            }
            mixer::MASTER_VOLUME_MUTE
            | mixer::PCM_OUT_VOLUME_MUTE
            | mixer::RECORD_GAIN_MUTE
            | mixer::RECORD_SELECT => self.mixer_store(off, v),
            // The codec's other features are not emulated: read only.
            _ => {}
        }
    }

    // ----------------------------------------------------------- bus master

    fn nabm_readb(&mut self, off: u32) -> u32 {
        if off == nabm::CAS {
            let v = self.cas;
            self.cas = 1;
            return v;
        }
        let c = &self.ch[(off >> 4) as usize & 7];
        match off & 0xF {
            nabm::CIV => u32::from(c.civ),
            nabm::LVI => u32::from(c.lvi),
            nabm::PIV => u32::from(c.piv),
            nabm::CR => u32::from(c.cr),
            nabm::SR => u32::from(c.sr & 0xFF),
            _ => 0xFF,
        }
    }

    fn nabm_readw(&mut self, off: u32) -> u32 {
        let c = &self.ch[(off >> 4) as usize & 7];
        if off < 0x80 && off & 0xF == nabm::SR {
            return u32::from(c.sr);
        }
        if off < 0x80 && off & 0xF == nabm::PICB {
            return u32::from(c.picb);
        }
        self.nabm_readb(off) | self.nabm_readb(off + 1) << 8
    }

    fn nabm_readl(&mut self, off: u32) -> u32 {
        if off == nabm::GLOB_CNT {
            return self.glob_cnt;
        }
        if off == nabm::GLOB_STA {
            return self.glob_sta | glob_sta::S0CR; // codec 0 always ready
        }
        let c = &self.ch[(off >> 4) as usize & 7];
        match off & 0xF {
            nabm::BDBAR => c.bdbar,
            nabm::CIV => u32::from(c.civ) | u32::from(c.lvi) << 8 | u32::from(c.sr) << 16,
            nabm::PICB => u32::from(c.picb) | u32::from(c.piv) << 16 | u32::from(c.cr) << 24,
            _ => self.nabm_readw(off) | self.nabm_readw(off + 2) << 16,
        }
    }

    fn nabm_writeb(&mut self, off: u32, v: u32) {
        if off >= 0x80 {
            return;
        }
        let i = (off >> 4) as usize & 7;
        match off & 0xF {
            nabm::LVI => {
                let c = &mut self.ch[i];
                if c.cr & cr::RPBM != 0 && c.sr & sr::DCH != 0 {
                    // A halted channel with new buffers resumes.
                    c.sr &= !(sr::DCH | sr::CELV);
                    self.next_bd(i);
                }
                self.ch[i].lvi = (v % 32) as u8;
            }
            nabm::CR => {
                if v as u8 & cr::RR != 0 {
                    self.reset_channel(i);
                } else {
                    self.ch[i].cr = v as u8 & CR_VALID_MASK;
                    if self.ch[i].cr & cr::RPBM == 0 {
                        self.ch[i].sr |= sr::DCH;
                    } else {
                        self.next_bd(i);
                        self.ch[i].sr &= !sr::DCH;
                    }
                }
            }
            nabm::SR => self.write_sr(i, v as u16),
            _ => {}
        }
    }

    fn write_sr(&mut self, i: usize, v: u16) {
        self.ch[i].sr |= v & !(SR_RO_MASK | SR_WCLEAR_MASK);
        self.update_sr(i, self.ch[i].sr & !(v & SR_WCLEAR_MASK));
    }

    fn nabm_writew(&mut self, off: u32, v: u32) {
        if off < 0x80 && off & 0xF == nabm::SR {
            self.write_sr((off >> 4) as usize & 7, v as u16);
            return;
        }
        self.nabm_writeb(off, v & 0xFF);
        self.nabm_writeb(off + 1, (v >> 8) & 0xFF);
    }

    fn nabm_writel(&mut self, off: u32, v: u32) {
        if off == nabm::GLOB_CNT {
            if v & (GC_WR | GC_CR) == 0 {
                self.glob_cnt = v & GC_VALID_MASK;
            }
            return;
        }
        if off == nabm::GLOB_STA {
            self.glob_sta &= !(v & GS_WCLEAR_MASK);
            self.glob_sta |= (v & !(GS_WCLEAR_MASK | GS_RO_MASK)) & GS_VALID_MASK;
            return;
        }
        if off < 0x80 && off & 0xF == nabm::BDBAR {
            self.ch[(off >> 4) as usize & 7].bdbar = v & !3;
            return;
        }
        self.nabm_writew(off, v & 0xFFFF);
        self.nabm_writew(off + 2, v >> 16);
    }

    // -------------------------------------------------------------- PCM out

    /// Consumes up to `bytes` of PCM out; completions as the hardware
    /// reports them.
    fn transfer_po(&mut self, mut bytes: u32) {
        const PO: usize = channel::PO;
        if self.ch[PO].sr & sr::DCH != 0 || self.ch[PO].cr & cr::RPBM == 0 {
            return;
        }
        for _ in 0..64 {
            if bytes < 2 {
                break;
            }
            if !self.ch[PO].bd_valid {
                self.fetch_bd(PO);
            }
            if self.ch[PO].picb == 0 {
                if self.ch[PO].civ == self.ch[PO].lvi {
                    self.ch[PO].sr |= sr::DCH;
                    break;
                }
                self.ch[PO].sr &= !sr::CELV;
                self.next_bd(PO);
                continue;
            }
            let c = &mut self.ch[PO];
            let take = bytes.min(u32::from(c.picb) << 1);
            c.bd_addr = c.bd_addr.wrapping_add(take);
            c.picb -= (take >> 1) as u16;
            bytes -= take;
            self.po_bytes += u64::from(take);
            if c.picb == 0 {
                log::trace!(target: "dev", "aci: ch {PO} bd {} done (ctl {:08x}, lvi {})", c.civ, c.bd_ctl, c.lvi);
                let mut new_sr = c.sr & !sr::CELV;
                if c.bd_ctl & bd::IOC != 0 {
                    new_sr |= sr::BCIS;
                }
                if c.civ == c.lvi {
                    new_sr |= sr::LVBCI | sr::DCH | sr::CELV;
                    self.update_sr(PO, new_sr);
                    break;
                }
                self.next_bd(PO);
                self.update_sr(PO, new_sr);
            }
        }
    }
}

xb_types::state_fields!(Channel { bdbar, civ, lvi, piv, cr, sr, picb, bd_valid, bd_addr, bd_ctl });
xb_types::state_fields!(Aci {
    glob_cnt,
    glob_sta,
    cas,
    ch,
    mixer,
    irq_level,
    po_bytes,
    next_period_ns
});

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TestRam;

    const PO: u32 = NABM_OFFSET + 0x10;
    const BDL: u32 = 0x1000;

    fn aci() -> (Aci, Arc<TestRam>, Arc<IrqHub>) {
        let ram = TestRam::new(0x10000);
        let irq = Arc::new(IrqHub::new());
        (Aci::new(ram.clone(), irq.clone()), ram, irq)
    }

    /// Queues `lens` (in samples) as the PCM out buffer list, the way
    /// DirectSound's AC'97 driver does, and starts the channel.
    fn start_pcm_out(a: &mut Aci, ram: &TestRam, lens: &[(u16, u32)], enables: u8) {
        for (i, &(samples, flags)) in lens.iter().enumerate() {
            let pa = BDL + 8 * i as u32;
            ram.write_u32(pa, 0x4000 + 0x1000 * i as u32);
            ram.write_u32(pa + 4, u32::from(samples) | flags);
        }
        a.mmio_write(PO + nabm::CR, 1, u32::from(cr::RR));
        a.mmio_write(PO + nabm::BDBAR, 4, BDL);
        a.mmio_write(PO + nabm::LVI, 1, lens.len() as u32 - 1);
        a.mmio_write(PO + nabm::CR, 1, u32::from(cr::RPBM | enables));
    }

    /// Runs `periods` 5 ms periods starting at guest time 0.
    fn run(a: &mut Aci, from_period: u64, periods: u64) {
        for p in from_period..=from_period + periods {
            a.advance(p * PERIOD_NS);
        }
    }

    #[test]
    fn codec_zero_reports_ready_and_identifies_as_a_stac9700() {
        let (mut a, _, _) = aci();
        assert_ne!(a.mmio_read(NABM_OFFSET + nabm::GLOB_STA, 4) & glob_sta::S0CR, 0);
        assert_eq!(a.mmio_read(mixer::VENDOR_ID1, 2), 0x8384);
        assert_eq!(a.mmio_read(mixer::VENDOR_ID2, 2), 0x7600);
        a.mmio_write(mixer::VENDOR_ID1, 2, 0x1234);
        assert_eq!(a.mmio_read(mixer::VENDOR_ID1, 2), 0x8384);
        assert_eq!(a.mmio_read(mixer::POWERDOWN_CTRL_STAT, 2) & 0xF, 0xF);
    }

    #[test]
    fn the_dac_rate_is_writable_only_with_variable_rate_audio_enabled() {
        let (mut a, _, _) = aci();
        let before = a.mmio_read(mixer::PCM_FRONT_DAC_RATE, 2);
        a.mmio_write(mixer::EXTENDED_AUDIO_CTRL_STAT, 2, 0);
        a.mmio_write(mixer::PCM_FRONT_DAC_RATE, 2, 44100);
        assert_eq!(a.mmio_read(mixer::PCM_FRONT_DAC_RATE, 2), before);
        a.mmio_write(mixer::EXTENDED_AUDIO_CTRL_STAT, 2, u32::from(mixer::EACS_VRA));
        a.mmio_write(mixer::PCM_FRONT_DAC_RATE, 2, 44100);
        assert_eq!(a.mmio_read(mixer::PCM_FRONT_DAC_RATE, 2), 44100);
        a.mmio_write(mixer::EXTENDED_AUDIO_CTRL_STAT, 2, 0);
        assert_eq!(a.mmio_read(mixer::PCM_FRONT_DAC_RATE, 2), before);
    }

    #[test]
    fn a_mixer_reset_restores_the_volume_defaults() {
        let (mut a, _, _) = aci();
        let default = a.mmio_read(mixer::MASTER_VOLUME_MUTE, 2);
        a.mmio_write(mixer::MASTER_VOLUME_MUTE, 2, 0x0101);
        assert_eq!(a.mmio_read(mixer::MASTER_VOLUME_MUTE, 2), 0x0101);
        a.mmio_write(mixer::RESET, 2, 0);
        assert_eq!(a.mmio_read(mixer::MASTER_VOLUME_MUTE, 2), default);
    }

    #[test]
    fn the_codec_semaphore_reads_free_once_then_busy_until_a_codec_access() {
        let (mut a, _, _) = aci();
        assert_eq!(a.mmio_read(NABM_OFFSET + nabm::CAS, 1), 0);
        assert_eq!(a.mmio_read(NABM_OFFSET + nabm::CAS, 1), 1);
        let _ = a.mmio_read(mixer::VENDOR_ID1, 2);
        assert_eq!(a.mmio_read(NABM_OFFSET + nabm::CAS, 1), 0);
    }

    #[test]
    fn pcm_out_is_consumed_at_the_real_time_rate() {
        let (mut a, ram, _) = aci();
        // Two buffers of 0x8000 samples: more than the test consumes.
        start_pcm_out(&mut a, &ram, &[(0x8000, 0), (0x8000, 0)], 0);
        assert_eq!(a.mmio_read(PO + nabm::SR, 2) & u32::from(sr::DCH), 0);
        let picb = a.mmio_read(PO + nabm::PICB, 2);
        run(&mut a, 0, 10);
        assert_eq!(a.pcm_out_bytes(), 10 * u64::from(PERIOD_BYTES));
        // PICB counts samples (two bytes each) left in the buffer.
        assert_eq!(picb - a.mmio_read(PO + nabm::PICB, 2), 10 * PERIOD_BYTES / 2);
        // 48 kHz stereo 16-bit is 192000 bytes a second.
        assert_eq!(u64::from(PERIOD_BYTES) * 1_000_000_000 / PERIOD_NS, 48_000 * 4);
    }

    #[test]
    fn a_buffer_completion_raises_the_interrupt_until_the_status_is_cleared() {
        let (mut a, ram, irq) = aci();
        // One period's worth with interrupt-on-completion, then a long one.
        start_pcm_out(&mut a, &ram, &[(PERIOD_BYTES as u16 / 2, bd::IOC), (0x8000, 0)], cr::IOCE);
        assert_eq!(irq.pending(), 0);
        run(&mut a, 0, 1);
        assert_eq!(a.mmio_read(PO + nabm::CIV, 1), 1);
        assert_ne!(a.mmio_read(PO + nabm::SR, 2) & u32::from(sr::BCIS), 0);
        assert_ne!(a.mmio_read(NABM_OFFSET + nabm::GLOB_STA, 4) & glob_sta::POINT, 0);
        assert_eq!(irq.pending(), Irq::Aci.mask());
        // The driver acknowledges by writing the status bit back.
        a.mmio_write(PO + nabm::SR, 2, u32::from(sr::BCIS));
        assert_eq!(a.mmio_read(PO + nabm::SR, 2) & u32::from(sr::BCIS), 0);
        assert_eq!(a.mmio_read(NABM_OFFSET + nabm::GLOB_STA, 4) & glob_sta::POINT, 0);
        assert_eq!(irq.pending(), 0);
    }

    #[test]
    fn a_completion_without_its_enable_sets_status_but_no_interrupt() {
        let (mut a, ram, irq) = aci();
        start_pcm_out(&mut a, &ram, &[(PERIOD_BYTES as u16 / 2, bd::IOC), (0x8000, 0)], 0);
        run(&mut a, 0, 1);
        assert_ne!(a.mmio_read(PO + nabm::SR, 2) & u32::from(sr::BCIS), 0);
        assert_eq!(irq.pending(), 0);
    }

    #[test]
    fn the_channel_halts_after_the_last_valid_buffer_and_resumes_when_more_are_queued() {
        let (mut a, ram, irq) = aci();
        start_pcm_out(&mut a, &ram, &[(PERIOD_BYTES as u16 / 2, 0)], cr::LVBIE);
        run(&mut a, 0, 1);
        let status = a.mmio_read(PO + nabm::SR, 2) as u16;
        assert_eq!(status & (sr::LVBCI | sr::DCH | sr::CELV), sr::LVBCI | sr::DCH | sr::CELV);
        assert_eq!(irq.pending(), Irq::Aci.mask());
        let consumed = a.pcm_out_bytes();
        run(&mut a, 2, 3);
        assert_eq!(a.pcm_out_bytes(), consumed, "a halted channel consumes nothing");

        a.mmio_write(PO + nabm::SR, 2, u32::from(sr::LVBCI));
        assert_eq!(irq.pending(), 0);
        ram.write_u32(BDL + 8, 0x5000);
        ram.write_u32(BDL + 12, 0x8000);
        a.mmio_write(PO + nabm::LVI, 1, 1);
        assert_eq!(a.mmio_read(PO + nabm::SR, 2) & u32::from(sr::DCH), 0);
        assert_eq!(a.mmio_read(PO + nabm::CIV, 1), 1);
        run(&mut a, 6, 1);
        assert!(a.pcm_out_bytes() > consumed);
    }

    #[test]
    fn a_channel_reset_halts_it_and_drops_its_interrupt() {
        let (mut a, ram, irq) = aci();
        start_pcm_out(&mut a, &ram, &[(PERIOD_BYTES as u16 / 2, bd::IOC), (0x8000, 0)], cr::IOCE);
        run(&mut a, 0, 1);
        assert_eq!(irq.pending(), Irq::Aci.mask());
        a.mmio_write(PO + nabm::CR, 1, u32::from(cr::RR));
        assert_eq!(irq.pending(), 0);
        assert_eq!(a.mmio_read(PO + nabm::SR, 2), u32::from(sr::DCH));
        assert_eq!(a.mmio_read(PO + nabm::BDBAR, 4), 0);
        // The interrupt enables survive a channel reset.
        assert_eq!(a.mmio_read(PO + nabm::CR, 1), u32::from(cr::IOCE));
    }

    #[test]
    fn a_long_stop_is_not_replayed() {
        let (mut a, ram, _) = aci();
        start_pcm_out(&mut a, &ram, &[(0xFFFE, 0), (0xFFFE, 0)], 0);
        a.advance(0);
        a.advance(3600 * 1_000_000_000);
        assert!(a.pcm_out_bytes() <= u64::from(MAX_CATCH_UP_PERIODS * PERIOD_BYTES));
        assert!(a.next_deadline_ns().unwrap() > 3600 * 1_000_000_000);
    }

    /// Pins what a savestate holds of this: see `xb_types::state::StateIo::probe`.
    #[test]
    fn the_saved_layout_of_the_codec_is_the_recorded_one() {
        let mut io = xb_types::state::StateIo::probe();
        let (mut v, _, _) = aci();
        io.io(&mut v);
        assert_eq!(
            io.shape_hash(),
            0xDC04_AFCF_20FD_EB1E,
            "the saved layout changed. Older states would be misread: give the section a new version, \
             read the old one where it differs, then record {:#018x} here",
            io.shape_hash()
        );
    }
}
