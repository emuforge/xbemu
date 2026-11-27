# xb-devices

Register-level devices a title's own libraries drive directly. Layer L2;
depends on `xb-types` and `log` only. `#![forbid(unsafe_code)]`.

| Device | Type | Address | Interrupt |
|---|---|---|---|
| USB OHCI controller 0, controller hubs, XID gamepads | `Ohci`, `usb::hub::Hub`, `usb::xid::Gamepad` | MMIO `0xFED0_0000`, `0x1000` bytes | `Irq::Usb0` (1) |
| AC'97 codec interface | `Aci` | MMIO `0xFEC0_0000`, `0x1000` bytes | `Irq::Aci` (6) |
| nForce Ethernet, no link | `Nic` | MMIO `0xFEF0_0000`, `0x400` bytes | `Irq::Nic` (4) |
| I/O ports | `Ports` | the whole port space; claims `0x8008`, `0x80C0` | none |

Details: `docs/hw/usb-xid.md`, `docs/hw/legacy-ports.md`.

## Interface

Every device is a plain `Send` value the machine owns. The machine maps the
MMIO windows (`Mapper::map_mmio`), and in its `xb_cpu::Bus` subtracts the base
and calls `mmio_read(offset, size)` / `mmio_write(offset, size, value)`; port
accesses go to `Ports::io_read` / `io_write`. Sizes are 1, 2 or 4. For an
access that reaches past the end of a window the reference hands the device
only the bytes inside, one at a time; the rest read as ones.

What a device needs from the machine it takes at construction:

- `Arc<dyn PhysMem>`: guest RAM for DMA (OHCI, ACI);
- `Arc<IrqHub>`: its interrupt line;
- `Arc<dyn Clock>`: the guest's time in nanoseconds, standing still while the
  emulation is paused (OHCI's `HcFmRemaining`, the ACPI timer port);
- `Arc<dyn VblankCounter>`: the GPU's vblank count (the video field bit).

Pad input does not go through the device value: `PadPorts` is a cloneable
handle the front end keeps (`set_pad`, `rumble`, `set_connected`, `resend`).

## Time: `advance`, no threads

No device owns a thread; the machine calls

- `Ohci::advance(now_ns)` about every millisecond (one 1 ms frame per call),
- `Aci::advance(now_ns)` at least every 5 ms (one 5 ms PCM period per call)

on the guest-CPU thread between runs of guest code, with the guest's clock.
`next_deadline_ns()` on each says when the next call does work. Reasons:

- A frame is atomic with respect to register accesses, as in the reference,
  whose frame thread holds the device lock for the whole frame.
- Latency: a frame can run at most one guest block late. Input is sampled
  when the pad's interrupt endpoint is polled (every 4 ms by its descriptor),
  so this is far below what a title can observe.
- Frames stop when the guest stops (menu open, pause) with no pause protocol,
  and tests are deterministic.

The machine therefore needs a 1 ms wake-up while the OHCI is operational (the
kernel's 1 kHz timer tick is the natural place). If `advance` is called late
it runs the frames it missed, up to 32 (OHCI) or 20 periods (ACI); a longer
gap is dropped.

The NIC without a link does nothing with time.

## Invariants

- Interrupt lines are level triggered and follow the registers: a device
  calls `IrqHub::set` only when the level its registers imply changes, and
  the line stays up until the guest clears the cause.
- A device never panics on what the guest wrote: guest addresses go through
  `PhysMem` (bounds checked), list walks are bounded, register offsets
  outside the implemented set read 0 (OHCI, NIC) or all ones (ACI) and drop
  writes.
- Device DMA writes (TD buffers and status, HCCA) are not reported to the
  write tracker, as in the reference; none of that memory is GPU-visible.
- The XID report layouts are in `usb::xid::report` and nowhere else.
- Hardware behaviour comes from the reference.

## Not here

- Isochronous endpoints and the Xbox Communicator (voice headset): their
  endpoint descriptors are skipped.
- The 1.0 console's front-panel hub (off by default in the reference).
- Memory units: the hubs' expansion ports are empty.
- NIC with a link: transmit, receive and auto-negotiation timing.
- PIC, PIT, CMOS, SMBus, PCI configuration ports: the HLE kernel owns those
  services and guest code never reaches the ports (see
  `docs/hw/legacy-ports.md`).
- Scripted pad input (`NC_PAD_SCRIPT`) and zeroing rumble while paused: the
  machine's and front end's business.

## Logs

USB logs under target `usb`; the ACI, NIC and ports under `dev`.

## Tests

```
cargo test -p xb-devices
```

`src/usb/tests.rs` holds a small host-side OHCI driver (HCCA, EDs and TDs in
a test `PhysMem`) that enumerates the hub and gamepad, polls reports and
sends rumble. No test needs a game image.
