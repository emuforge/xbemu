//! Library code that waits on emulated hardware, patched as it loads.
//!
//! DirectSound hands the APU's GP DSP a command through a mailbox in memory
//! and spins until the DSP takes it:
//!
//! ```text
//!     add eax, 0x800
//!     cmp dword [eax+0x10], 0     ; the previous command still pending
//!     jne $-6
//! ```
//!
//! The emulated GP runs a slice of its program per audio frame unless a CPU
//! waits on it, so the DSP reaches its command check only every ~40 ms, and
//! the spinning thread (PGR2's audio thread, above the main thread's
//! priority) holds the CPU all that time. Here the loop jumps to a stub that
//! enters the kernel (the APU-spin trap address), which asks the machine to
//! hurry the APU and wait for the mailbox
//! ([`crate::Platform::apu_mailbox_wait`]). The library is the same in every
//! title seen (one copy, in DSOUND).
//!
//! The stub `call`s the trap address and the kernel returns to the pushed
//! address, the registers as they were.

use crate::kernel::{Kernel, ORD_APU_SPIN};

const MAILBOX_WAIT: [u8; 11] = [
    0x05, 0x00, 0x08, 0x00, 0x00, // add eax, 0x800
    0x83, 0x78, 0x10, 0x00, // cmp dword [eax+0x10], 0
    0x75, 0xFA, // jne the cmp
];
/// The cmp.
const LOOP_OFF: u32 = 5;

// XMV's decoder close waits for each of its audio streams to stop before it
// frees the decoder, where its packets' pdwCompletedSize and pdwStatus are:
//
//     call [ecx+0Ch]               ; IDirectSoundStream_GetStatus
//     test byte [ebp+0Ah], 1       ; DSSTREAMSTATUS_PLAYING
//     je next
//     push 1
//     call Sleep
//     call DirectSoundDoWork       ; the flushed packets complete here
//     dec edi                      ; the same stream again
//   next:
//     inc edi
//
// It counts on the stream still playing at its first look: the console's APU
// releases a voice turned off at its next frame, 0.67 ms on, and the CPU
// gets from the flush to here in microseconds. Here the guest can take
// milliseconds, the release's DPC came first, GetStatus said stopped,
// DirectSoundDoWork never ran and the decoder was freed with the flushed
// packets still to complete; their completion in the stream's last Release
// wrote into the freed heap (Soldier of Fortune II, an intro movie skipped).
// The je goes to a stub that runs DirectSoundDoWork before the next stream.
// Patched as the code loads: at boot and in XeLoadSection (XMV is a section
// of its own in some titles, loaded for a movie).
const XMV_WAIT: [u8; 12] = [
    0xFF, 0x51, 0x0C, // call [ecx+0Ch]
    0xF6, 0x45, 0x0A, 0x01, // test byte [ebp+0Ah], 1
    0x74, 0x0D, // je next
    0x6A, 0x01, // push 1
    0xE8, // call Sleep
];
const XMV_JE: u32 = 7; // the je, patched with a jmp to the stub
const XMV_SLEEP: u32 = 11; // call Sleep (rel32)
const XMV_DOWORK: u32 = 16; // call DirectSoundDoWork (rel32)
const XMV_DEC: u32 = 21; // dec edi: the same stream again
const XMV_NEXT: u32 = 22; // inc edi: the next stream
const XMV_STUB: u32 = 2 + 2 + 5 + 5 + 5 + 5 + 5;

// D3DDevice_SetTimerCallback(time, callback, context) refuses a time the TSC
// has passed (D3DERR_TIME_IN_PAST) and schedules nothing:
//
//     rdtsc ; add edi(eax), 40 ; adc ...      ; now + 40 ticks
//     cmp esi, ecx ; ja ok ; jb past ; cmp ecx, edi ; ja ok
//   past:
//     mov cl, bl ; call [KfLowerIrql] ; pop edi ; pop esi ; pop ebp
//     mov eax, 8876082Ah ; pop ebx ; ret 16
//   ok:
//     sub ecx, eax ; sbb esi, edx ; mov edi, ecx ; mov eax, [esp+32] ...
//
// A title's periodic callback that schedules its next time from the last
// (Def Jam: Fight for NY's 60 Hz input timer, its main loop's only update
// source) stops for good when one call runs a period late: the console's
// DPC is never that late, the emulation can be. The refusal becomes a delay
// of 0 ticks: the callback comes at once and the title catches up.
const TIMER_PAST_PAT: [u8; 33] = [
    0x0F, 0x31, 0x8B, 0xF8, 0x83, 0xC7, 0x28, 0x8B, 0xCA, 0x13, 0xCE, 0x8B, 0x74, 0x24, 0x18, 0x3B,
    0xF1, 0x8B, 0x4C, 0x24, 0x14, 0x77, 0x1A, 0x72, 0x04, 0x3B, 0xCF, 0x77, 0x14, 0x8A, 0xCB, 0xFF,
    0x15,
];
const TIMER_PAST_TAIL: [u8; 22] = [
    0x5F, 0x5E, 0x5D, 0xB8, 0x2A, 0x08, 0x76, 0x88, 0x5B, 0xC2, 0x10, 0x00, 0x2B, 0xC8, 0x1B, 0xF2,
    0x8B, 0xF9, 0x8B, 0x44, 0x24, 0x20,
];
const TIMER_PAST: usize = 0x1D; // past: mov cl, bl
const TIMER_TAIL: usize = 0x25; // after the call [KfLowerIrql]
const TIMER_PUSH: usize = 0x37; // ok's mov eax, [esp+32]: the arguments pushed

/// Every offset of `pat` in `hay`, not overlapping.
fn find_all(hay: &[u8], pat: &[u8]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + pat.len() <= hay.len() {
        match hay[i..].windows(pat.len()).position(|w| w == pat) {
            Some(p) => {
                out.push(i + p);
                i += p + pat.len();
            }
            None => break,
        }
    }
    out
}

fn put_rel(b: &mut Vec<u8>, op: u8, stub: u32, to: u32) {
    b.push(op);
    let rel = to.wrapping_sub(stub + b.len() as u32 + 4);
    b.extend_from_slice(&rel.to_le_bytes());
}

impl Kernel {
    /// The stub for the loop at `va` (its cmp; 6 bytes):
    ///   L: cmp dword [eax+0x10], 0 ; je out ; call (APU-spin trap) ; jmp L
    ///   out: jmp va+6
    fn mailbox_stub(&mut self, va: u32) -> u32 {
        const OUT: u32 = 13;
        let stub = self.code_alloc(OUT + 5);
        if stub == 0 {
            return 0;
        }
        let mut b = vec![0x83, 0x78, 0x10, 0x00, 0x74, 7]; // cmp ; je out
        put_rel(&mut b, 0xE8, stub, Self::trap(ORD_APU_SPIN));
        b.extend_from_slice(&[0xEB, (-13i8) as u8]); // jmp L
        put_rel(&mut b, 0xE9, stub, va + 6);
        self.gm.kwrite(stub, &b);
        stub
    }

    /// The stub for the XMV loop at `m` (the call [ecx+0Ch]); the flags of
    /// the test still set:
    ///   je np ; push 1 ; call Sleep ; call DirectSoundDoWork ; jmp m+21 (dec edi)
    ///   np: call DirectSoundDoWork ; jmp m+22 (inc edi)
    fn xmv_stub(&mut self, m: u32, code: &[u8], at: usize) -> u32 {
        let rel_target = |off: u32| {
            let i = at + off as usize + 1;
            let rel = u32::from_le_bytes([code[i], code[i + 1], code[i + 2], code[i + 3]]);
            (m + off + 5).wrapping_add(rel)
        };
        let (sleep, dowork) = (rel_target(XMV_SLEEP), rel_target(XMV_DOWORK));
        let stub = self.code_alloc(XMV_STUB);
        if stub == 0 {
            log::warn!(target: "kernel", "fixups: XMV's stream wait at {m:08x} left as it is (no room for its stub)");
            return 0;
        }
        let mut b = vec![0x74, 2 + 5 + 5 + 5, 0x6A, 0x01]; // je np ; push 1
        put_rel(&mut b, 0xE8, stub, sleep);
        put_rel(&mut b, 0xE8, stub, dowork);
        put_rel(&mut b, 0xE9, stub, m + XMV_DEC);
        put_rel(&mut b, 0xE8, stub, dowork); // np:
        put_rel(&mut b, 0xE9, stub, m + XMV_NEXT);
        self.gm.kwrite(stub, &b);
        stub
    }

    fn xmv_fixups(&mut self, va: u32, code: &[u8], name: &str) -> u32 {
        let mut patched = 0;
        for at in find_all(code, &XMV_WAIT) {
            if code.len() - at <= XMV_NEXT as usize
                || code[at + XMV_DOWORK as usize] != 0xE8
                || code[at + XMV_DEC as usize] != 0x4F
                || code[at + XMV_NEXT as usize] != 0x47
            {
                continue;
            }
            let m = va + at as u32;
            let stub = self.xmv_stub(m, code, at);
            if stub == 0 {
                continue;
            }
            let site = m + XMV_JE;
            let mut patch = vec![0xE9];
            patch.extend_from_slice(&stub.wrapping_sub(site + 5).to_le_bytes());
            patch.resize((XMV_DEC - XMV_JE) as usize, 0x90);
            if self.gm.write(site, &patch).is_ok() {
                patched += 1;
                log::info!(target: "kernel", "fixups: XMV's stream wait at {m:08x} ({name}) runs DirectSoundDoWork for a stopped stream too (stub {stub:08x})");
            }
        }
        patched
    }

    /// past: xor edi, edi ; xor esi, esi ; jmp ok's push (0 ticks from now)
    fn timer_fixups(&mut self, va: u32, code: &[u8], name: &str) -> u32 {
        let mut patched = 0;
        for at in find_all(code, &TIMER_PAST_PAT) {
            let tail = at + TIMER_TAIL;
            if code.len() < tail + TIMER_PAST_TAIL.len()
                || code[tail..tail + TIMER_PAST_TAIL.len()] != TIMER_PAST_TAIL
            {
                continue;
            }
            let patch = [0x31, 0xFF, 0x31, 0xF6, 0xEB, (TIMER_PUSH - (TIMER_PAST + 6)) as u8];
            if self.gm.write(va + (at + TIMER_PAST) as u32, &patch).is_ok() {
                patched += 1;
                log::info!(target: "kernel", "fixups: D3D timer callback at {:08x} ({name}) comes at once when its time has passed", va + at as u32);
            }
        }
        patched
    }

    /// The library code a section holds is patched as it loads, in
    /// `[va, va + len)` (XMV's stream wait, D3D's timer callback): at boot
    /// and as XeLoadSection loads one. Translations of the code it changed are
    /// dropped.
    pub(crate) fn section_fixups(&mut self, va: u32, len: u32, name: &str) {
        let Ok(code) = self.gm.read_vec(va, len) else { return };
        let n = self.xmv_fixups(va, &code, name) + self.timer_fixups(va, &code, name);
        if n != 0 {
            self.gm.invalidate_code(va, len);
        }
    }

    /// Before the title runs, its library code that spins on emulated
    /// hardware is pointed at the kernel. `sections`: (name, va, size) of the
    /// XBE's sections.
    pub(crate) fn title_fixups(&mut self, sections: &[(String, u32, u32)]) {
        let loaded: Vec<&(String, u32, u32)> = sections
            .iter()
            .filter(|(_, va, size)| *size != 0 && self.gm.mapped_range(*va, *size))
            .collect();
        for (name, va, size) in &loaded {
            self.section_fixups(*va, *size, name);
        }
        let mut patched = 0;
        for (name, sva, size) in &loaded {
            let Ok(code) = self.gm.read_vec(*sva, *size) else { continue };
            let mut changed = false;
            for at in find_all(&code, &MAILBOX_WAIT) {
                let va = sva + at as u32 + LOOP_OFF;
                let stub = self.mailbox_stub(va);
                if stub == 0 {
                    continue;
                }
                let mut patch = vec![0xE9];
                patch.extend_from_slice(&stub.wrapping_sub(va + 5).to_le_bytes());
                patch.push(0x90);
                if self.gm.write(va, &patch).is_ok() {
                    patched += 1;
                    changed = true;
                    log::info!(target: "kernel", "fixups: DirectSound mailbox wait at {va:08x} ({name}) waits on the APU (stub {stub:08x})");
                }
            }
            if changed {
                self.gm.invalidate_code(*sva, *size);
            }
        }
        if patched == 0 {
            log::info!(target: "kernel", "fixups: no DirectSound mailbox wait found");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing;

    #[test]
    fn the_mailbox_wait_is_redirected_to_a_stub_that_calls_the_apu_trap() {
        let mut t = testing::kernel();
        let va = t.alloc(0x1000);
        let mut code = vec![0x90u8; 0x40];
        code.extend_from_slice(&MAILBOX_WAIT);
        code.extend_from_slice(&[0xC3; 8]);
        t.k.gm.write(va, &code).unwrap();
        t.k.title_fixups(&[("DSOUND".into(), va, 0x1000)]);
        let site = va + 0x40 + LOOP_OFF;
        assert_eq!(t.k.gm.r8(site).unwrap(), 0xE9);
        let stub = (site + 5).wrapping_add(t.k.gm.r32(site + 1).unwrap());
        assert_eq!(t.k.gm.r8(site + 5).unwrap(), 0x90);
        // the stub: cmp ; je out ; call trap ; jmp L ; out: jmp back
        assert_eq!(t.k.gm.read_vec(stub, 6).unwrap(), [0x83, 0x78, 0x10, 0x00, 0x74, 7]);
        let call = (stub + 11).wrapping_add(t.k.gm.r32(stub + 7).unwrap());
        assert_eq!(call, Kernel::trap(ORD_APU_SPIN));
        assert_eq!(t.k.gm.read_vec(stub + 11, 2).unwrap(), [0xEB, 0xF3]);
        let back = (stub + 18).wrapping_add(t.k.gm.r32(stub + 14).unwrap());
        assert_eq!(back, site + 6);
    }

    #[test]
    fn the_apu_trap_waits_on_the_mailbox_and_returns_to_the_stub() {
        let mut t = testing::kernel();
        let mailbox = t.alloc(0x1000);
        let esp = t.k.esp() - 4;
        t.k.gm.w32(esp, 0x8002_2345).unwrap();
        t.k.set_esp(esp);
        t.k.set_reg(xb_cpu::reg::EAX, mailbox);
        t.k.set_eip(Kernel::trap(ORD_APU_SPIN));
        t.k.kcall(&mut t.p, ORD_APU_SPIN);
        assert_eq!(t.p.apu_waits, [t.k.gm.phys_of(mailbox + 0x10).unwrap()]);
        assert_eq!(t.k.eip(), 0x8002_2345);
        assert_eq!(t.k.esp(), esp + 4);
        assert_eq!(t.k.reg(xb_cpu::reg::EAX), mailbox);
    }

    #[test]
    fn the_d3d_timer_refusal_becomes_a_zero_delay() {
        let mut t = testing::kernel();
        let va = t.alloc(0x1000);
        let mut code = vec![0xCCu8; 0x10];
        let at = code.len();
        code.extend_from_slice(&TIMER_PAST_PAT);
        code.resize(at + TIMER_TAIL, 0);
        code.extend_from_slice(&TIMER_PAST_TAIL);
        code.resize(0x100, 0xCC);
        t.k.gm.write(va, &code).unwrap();
        t.k.section_fixups(va, 0x1000, "D3D");
        let site = va + (at + TIMER_PAST) as u32;
        assert_eq!(
            t.k.gm.read_vec(site, 6).unwrap(),
            [0x31, 0xFF, 0x31, 0xF6, 0xEB, (TIMER_PUSH - TIMER_PAST - 6) as u8]
        );
        t.k.gm.flush(&mut t.cpu);
        assert!(!t.cpu.invalidated.is_empty());
    }
}
