# I/O ports and the other small devices

Implemented in `crates/xb-devices`: `ports.rs`, `aci.rs`, `nic.rs`.

## I/O ports

A PC has an interrupt controller at 0x20/0xA0, a timer at 0x40, CMOS at
0x70 and PCI configuration at 0xCF8; the Xbox adds an SMBus controller with
the system controller, the EEPROM and the video encoder behind it. On a
console their user is the kernel, and here the kernel is implemented on the
host: a title asks for those services through kernel exports and guest code
never executes the `IN` / `OUT` instructions the real kernel would.
`HalReadSMBusValue`, `HalReadSMCTrayState`, `HalReadWritePCISpace`,
`ExQueryNonVolatileSetting`, `ExSaveNonVolatileSetting`,
`KeStallExecutionProcessor` and `KeQueryPerformanceCounter` are all answered
on the host, and interrupts are delivered by the kernel from its line mask
rather than a PIC. So none of them is a device here: there is no EEPROM, no
SMC and no PCI configuration space to emulate.

As in the reference, exactly two ports are claimed, the ones titles read
(`Ports`):

| Port | Access | Behaviour |
|---|---|---|
| `0x8008` | read | ACPI power-management timer: the guest's clock in ticks of 3.375 MHz, truncated to the access size. The same rate as `KeQueryPerformanceCounter`. |
| `0x80C0` | read | MCPX GPIO. Bit 5 is the video field: set when the GPU's vblank count is odd. The other bits read as ones. D3D's vblank DPC and `GetDisplayFieldStatus` read it; with a constant field the XMV player never starts a movie (Riddick). |
| any other | read | all ones at the access size, as an unclaimed port on the bus |
| any | write | dropped |

The first four accesses to each of the first 64 unclaimed ports are logged at
DEBUG (target `dev`), so a title that reaches for something new shows up.
Halo's boot touches one port besides the two reads above: it writes 0x01 to
`0x80C0`, which is dropped.

`Ports` takes the guest clock (`Clock`) and the vblank count
(`VblankCounter`) from the machine.

## AC'97 codec interface (ACI)

| | |
|---|---|
| Registers | physical `0xFEC0_0000`, `0x1000` bytes: mixer (NAM) at +0x000, bus master (NABM) at +0x100 |
| Interrupt | IRQ 6 (`Irq::Aci`), level: asserted while a PCM in, PCM out or microphone interrupt bit is set in global status |
| Codec | SigmaTel STAC9700 (vendor id 0x8384 0x7600), always ready |

DirectSound waits for codec 0 to be ready, programs the mixer and runs the
PCM out channel through a buffer descriptor list (32 entries of address and
sample count, flags `IOC` and `BUP`). The audible sound comes from the APU;
what DirectSound feeds the ACI is consumed and dropped, so this device has
no audio output. What matters is that the list advances in real time:
`Aci::advance` consumes 960 bytes (240 stereo frames, 5 ms at 48 kHz) per
5 ms of guest time, and each completed buffer sets the channel's status
(`BCIS` for `IOC` buffers, `LVBCI`, `DCH` and `CELV` at the last valid
index) and, with the matching enable in the control register, the
interrupt. The guest clears status bits by writing them back. Only PCM out
(channel 1) runs; S/PDIF out (channel 7) holds its registers and does not
advance, as in the reference.

Mixer registers are 16 bits wide. Volume, record select, the powerdown and
extended audio control registers and the sample rates (writable only with
variable rate enabled) are held; the vendor and extended audio ids are read
only; the rest read as zero and ignore writes. Writing the reset register
restores the defaults.

## Ethernet controller (nvnet), no link

| | |
|---|---|
| Registers | physical `0xFEF0_0000`, `0x400` bytes |
| Interrupt | IRQ 4 (`Irq::Nic`): `IRQ_STATUS & IRQ_MASK`; never asserted without a link |
| PHY | MII address 1 |

There is no network, so the controller behaves as one with the cable out,
the reference's "network off":

- the PHY reports auto-negotiation complete and link down (`BMSR`), no
  partner acknowledge (`ANLPAR`), and keeps doing so after a PHY reset or a
  negotiation restart, so XNet's start-up never waits;
- `ADAPTER_CONTROL` never shows link up; no link-change interrupt comes;
- MDIO accesses complete at once (the in-use bit clears); other MII
  addresses read as all ones;
- a transmit kick sends nothing and completes nothing; nothing is received;
- the reset sequence forcedeth-style drivers use is acknowledged (ring
  pointers reset, setup register 5 bit 31, the idle bit);
- every other register holds what is written.

The kernel exports `PhyGetLinkState` and `PhyInitialize` map to
`Nic::link_state` (0) and `Nic::phy_initialize` (nothing).
