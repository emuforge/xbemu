# The MCPX APU

The audio processor in the Xbox's MCPX southbridge, as xbemu emulates it.
Source for everything here: the vendored C
(`vendor/xbased-apu/native/dev/apu`), cited by file. The Rust side is
`crates/xb-apu`.

## What a title sees

DirectSound is statically linked into every title and drives the device
directly: there is no kernel audio API.

| | |
|---|---|
| Registers | guest physical `0xFE80_0000`, `0x8_0000` bytes (`xb_apu::MMIO_BASE`, `MMIO_LEN`) |
| Interrupt | line 5 (`xb_types::irq::Irq::Apu`), level triggered |
| Guest RAM | voice table, buffer page tables, notifiers, DSP scratch and FIFOs, all by physical address |

The window has four parts (`apu.c` `route`):

| Offset | Part | Handler |
|---|---|---|
| `0x00000` | device registers (`NV_PAPU_*`, `apu_regs.h`) | `apu.c` |
| `0x20000` | voice processor methods (`NV1BA0_PIO_*`) | `vp/vp.c` |
| `0x30000` | global processor: its X/Y/P memories, mix buffer, reset | `dsp/gp_ep.c` |
| `0x50000` | encode processor: its memories, reset | `dsp/gp_ep.c` |

Accesses are 32-bit in practice; `Apu::mmio_read`/`mmio_write` take 1, 2 or 4
bytes.

### Registers that matter for bring-up

| Register | Offset | Role |
|---|---|---|
| `ISTS` | `0x1000` | interrupt status; writing a 1 clears that cause. Bit 0 general, 4 front-end trap, 5 notify, 6 voice |
| `IEN` | `0x1004` | interrupt enable, same bits. The line is up while `IEN` bit 0 is set and an enabled cause is pending |
| `FECTL` | `0x1100` | front end mode (bits 7:5): free running `0x00`, halted `0x80`, trapped `0xE0`; trap reason in bits 11:8 |
| `FEDECMETH`, `FEDECPARAM` | `0x1300`, `0x1304` | the method and argument of a trap, for its handler |
| `FENADDR` | `0x115C` | notifier array in RAM (16 bytes each, four per voice after two globals; byte 15 is the status) |
| `FETFORCE1` | `0x1504` | bit 15: trap on an idle voice found on a list |
| `SECTL` | `0x2000` | bits 4:3 `XCNTMODE`: 0 stops the frames |
| `XGSCNT` | `0x200C` | 48 kHz sample counter, derived from the guest's clock |
| `VPVADDR`, `VPSGEADDR`, `VPSSLADDR` | `0x202C`, `0x2030`, `0x2034` | voice table (256 voices of `0x80` bytes), buffer page table, stream segment page table |
| `GPSADDR`, `GPFADDR`, `EPSADDR`, `EPFADDR` | `0x2040`… | page tables of the DSPs' scratch memory and FIFOs |
| `TVL2D`, `TVL3D`, `TVLMP` | `0x2054`, `0x2060`, `0x206C` | heads of the three voice lists (`0xFFFF`: empty) |

After reset every register is zero, so the frames are stopped. In outline,
what the C requires of a title's start-up is: point the tables at RAM, empty
the lists, enable causes in `IEN`, set `FECTL` free running, turn `XCNTMODE`
on, then load the global processor's program and take it out of reset
(`GPRST`). The integration test (`crates/xb-apu/tests/apu.rs` `bring_up`)
does all but the program load.

## The three stages

One **frame** is 32 samples (1500 a second). Each frame (`apu.c` `se_frame`):

1. **Voice processor** (`vp/vp.c`). Walks the 2D, 3D and multipass voice
   lists. For each active voice: steps its envelopes and LFOs, fetches
   samples from guest RAM (8/16/24/32-bit PCM or Xbox ADPCM; a looping buffer
   or a stream of segments), resamples to 48 kHz, filters (state-variable
   filter, or HRTF for 3D voices), and mixes into up to eight of 32
   **mixbins**. It writes the voice's state back to the voice table, posts
   notifications (a status byte in RAM plus an interrupt) when a buffer or
   segment ends, and reports idle voices to the front end as a trap.
2. **Global processor** (`dsp/gp_ep.c`, `dsp/dsp*.c`). A DSP56300 running
   the title's effects program (DirectSound's default image or the title's
   own) over the mixbins: reverb, the I3DL2 chain, the mix down to speakers.
   Its six speaker outputs appear at X:`0x1400`, 32 samples each.
3. **Encode processor**. A second DSP56300, run every eighth frame, that
   encodes the speaker outputs for the digital output (Dolby Digital). Its
   first output FIFO is what the full-DSP mode plays; by default the global
   processor's outputs are played instead and the encoder gets only a slice
   of each frame.

Which stage is heard (`gp_ep.c` `mcpx_apu_update_dsp_preference`):

- before the title's DSP program runs, or if it fails to finish its frames:
  the voice processor's own mix ("the voices' own mix");
- once the global processor runs a program and the recompiler has it: the
  global processor's speaker outputs ("the title's DSP program (GP),
  compiled");
- with `ApuOptions::full_dsp`: both DSPs run to completion every frame and
  the encode processor's output is played.

Without `full_dsp` and without a boost (below), a DSP that is not the heard
stage gets 1000 cycles a frame, not a whole frame's worth.

### Stereo and 5.1

Every eighth frame the accumulated 256 samples are pushed to the sink.

- **Stereo** (`monitor.frame_buf`, `int16_t[256][2]`): the six speaker
  outputs folded as the console's analog output does, centre and surrounds
  into the front pair at -3 dB, LFE dropped (`gp_ep.c`); the voice
  processor's own mix folds the same way (`vp.c`). A frame whose six outputs
  are the stereo pair three times over is passed as the pair.
- **5.1** (`g_apu_frame6`, `int16_t[256][6]`, order FL FR C LFE BL BR): the
  six speaker outputs unfolded, taken when `g_apu_out_channels` is 6.
  `ApuOptions::channels = Channels::Surround51` selects it; the sink then
  gets six-channel blocks.

## The frame thread and its pacing

`apu.c` `mcpx_apu_frame_thread`, one loop iteration per frame:

1. If held (not started, paused, resetting): park.
2. If the front end is trapped or halted or the counter is off, or a voice
   posted a cause: recompute the interrupt line from `ISTS` and `IEN`.
3. If the frames are stopped (`XCNTMODE` off, or the front end halted; a
   *trap* does not stop them): wait 5.33 ms for a register write and go
   round. Pending voice-off notifications are still delivered after 64
   frames' time.
4. `throttle`, then `se_frame`.

`throttle` is where the output's consumption sets the pace. At each block
boundary it compares the sink's queue with two watermarks derived from the
device period (`monitor_fit`): high = 4 periods (capped by the ring), low =
2 periods.

- queue >= high: wait (woken by the sink's drain signal, or every 5.33 ms);
- queue > low: sleep to the next deadline on the host clock, 5.33 ms a
  block, the eight frames of the block spread 667 µs apart, the deadline
  nudged by 1 µs a block towards the midpoint;
- queue <= low: no sleep (refill).

So the device's clock sets the long-run rate and the host clock only spaces
the frames. Spacing matters: the global processor takes DirectSound's
commands once a frame, and a title spins on the CPU waiting for it. If the
device's callbacks are larger than the period announced, the watermarks
grow to follow (`apu_out_asked_max`). On Linux the period is 2048 frames a
callback, as in the reference, because a smaller one underruns on a desktop
sound server.

In xbemu the queue is an `AudioSink` (`crates/xb-apu/src/sink.rs`):
`ring_sink` for a real device, `NullSink` when there is none. A pause leaves
the queue as it is and the device plays silence; on resume the thread takes
its deadlines from the present, so nothing is made up for the pause.

## Waiting on the APU: the DirectSound mailbox

DirectSound gives the global processor a command through a word in RAM and
spins until the DSP clears it:

```
add eax, 0x800
cmp dword [eax+0x10], 0
jne $-6
```

With the DSP getting a slice per frame, that spin holds the CPU for several
frames. As in the reference, the kernel finds this byte sequence in the
title's sections at load, redirects the loop to a stub that traps into the
kernel (the "fixups: DirectSound mailbox wait at … waits on the APU" log
line), and the trap handler calls the APU with the address of the word
(`eax + 0x10`):

- `Apu::mailbox_wait(word_pa)`: the global processor runs whole frames for
  the next 3 ms (`GP_BOOST_US`), and the call polls the word up to 1000
  times with `usleep(1)` in between. With the boost the DSP normally clears
  it within a frame or two. The stub checks the word again whatever the
  call returns.

The kernel crate owns the pattern scan, the stub and the virtual-to-physical
translation; `xb-apu` only provides the wait.

Two related hooks:

- `Apu::try_mmio_write`: a voice method that cannot get its voice (the frame
  thread is mixing it) gives up after 0.2 ms and asks for the store to be
  executed again, so the guest takes interrupts in between (`vp.c`
  `voice_spin_lock`). `Apu::mmio_write` waits instead.
- `Apu::guest_time_held(ns)`: the guest's clock skipped while the APU played
  on; `XGSCNT` is moved on by as much.

## What the APU writes to guest RAM

Voice state in the voice table (every frame, per active voice), notifier
status bytes, stream segment entries (a method), the DSPs' scratch memory and
output FIFOs, and the word at `FEMEMADDR` (the `FEMEMDATA` "magic write").
Every write is bounds checked against the RAM size and reported
(`ApuHost::ram_written`) for the GPU's write tracking, as the C does with
`hle_host_mem_set_dirty`.

## Not emulated or not done here

- The method FIFO (`NV1BA0_PIO_FREE` always reads empty) and method timing.
- The AC'97 link: the APU's output goes straight to the sink.
- A device reset does not clear the DSPs' memories (as in the C).
- Savestates: `state.c` is vendored but not built.
