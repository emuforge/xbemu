# USB: OHCI controller, controller hubs and the XID gamepad

Implemented in `crates/xb-devices/src/usb`.

## Who drives it

XAPI's USB stack is linked into the title. It programs the first OHCI
controller at register level, builds endpoint and transfer descriptors in
contiguous memory, and connects an interrupt service routine to IRQ 1 through
the kernel. Nothing of it is HLE.

## Controller

| | |
|---|---|
| Registers | physical `0xFED0_0000`, `0x1000` bytes (`OHCI_BASE`, `OHCI_SIZE`) |
| Interrupt | IRQ 1 (`Irq::Usb0`), level: asserted while `HcInterruptEnable.MIE` is set and an enabled bit of `HcInterruptStatus` is set |
| Revision | OpenHCI 1.0 (`HcRevision` = 0x10) |
| Root hub | 4 ports, no power switching (`HcRhDescriptorA` = NPS, NDP 4) |

Registers are 32 bits. Reads of 1 and 2 bytes return the addressed bytes;
writes are taken whole at the register's offset.

A host controller reset (`HcCommandStatus.HCR`) completes at once and leaves
the controller suspended; it does not touch the root hub ports.

### Frames

Each millisecond of guest time while `HcControl.HCFS` is operational is one
frame, run by `Ohci::advance`:

1. the frame number goes up and is written to the HCCA (+0x80);
2. with `PLE`, the periodic list of this frame's HCCA slot (`frame & 31`):
   one TD per endpoint;
3. with `CLE` and `CLF`, the control list, each endpoint until it is empty
   or stops; `CLF` clears when the list had nothing queued; likewise bulk;
4. if the done queue is not empty, its interrupt delay has run down and
   `WDH` is clear: the done head is written to the HCCA (+0x84, bit 0 set
   if another enabled interrupt is pending) and `WDH` is set;
5. `SF` is set and the interrupt line updated.

A transfer completes in the frame that services it: the devices are in
process. A NAK leaves the TD at the head of its endpoint for a later frame.
On completion the TD's condition code and `CBP` are written back (`CBP` = 0
when the whole buffer moved, else the first byte not moved), the toggle
carry goes to the ED, and the TD is linked onto the done queue. An error
(`STALL`, `DEVICENOTRESPONDING` for an address nobody has, `DATAUNDERRUN`
for a short IN without buffer rounding) halts the endpoint and makes the done
queue go out in the same frame.

Isochronous endpoint descriptors (format bit set) are skipped.

### Root ports

| Controller port (API, 0-based) | Player | Root port | `HcRhPortStatus` offset |
|---|---|---|---|
| 0 | 1 | 3 | 0x5C |
| 1 | 2 | 4 | 0x60 |
| 2 | 3 | 1 | 0x54 |
| 3 | 4 | 2 | 0x58 |

Only ports with a controller plugged in show a connection. A controller
nobody holds is not plugged in: a title that listens to the first controller
it finds (AirForce Delta Storm) would otherwise latch an empty one.

Plugging and unplugging at run time (`PadPorts::set_connected`) is applied at
the next `advance`: the root port's `CCS` follows, `CSC` is set and `RHSC`
raised, and the title's stack enumerates or drops the device.

A port reset (`PRS`) completes at once: `PES` and `PRSC` are set and the
device behind the port returns to address 0.

## Topology

```
root port ── hub (3 ports, 0409:55aa, full speed)
              ├─ port 1: gamepad (045e:0202)
              ├─ port 2: expansion slot, empty
              └─ port 3: expansion slot, empty
```

Each controller is the hub with the gamepad on its port 1, as the real
controller is. The sequence a title performs (Halo's boot):

```
ohci: state 3 -> 2          controller started
hub: address 1              SET_ADDRESS
hub@1: configuration 1      SET_CONFIGURATION
gamepad: address 2
gamepad@2: configuration 1
gamepad 0: polled           first interrupt IN on endpoint 2
```

### Hub

Class 9, one interrupt IN endpoint (0x81, 2 bytes, interval 255) that
reports a bitmap of ports with a change (bit n = port n) and NAKs when there
is none. Ports are powered from reset and report a connection change for an
attached device. Class requests: `GET_DESCRIPTOR` (hub, type 0x29),
`GET_STATUS` (hub and port), `SET_FEATURE` / `CLEAR_FEATURE` (port: enable,
suspend, reset, power and the five change bits; hub: accepted). `PORT_RESET`
completes at once: the port is enabled, `C_PORT_RESET` set and the child
returns to address 0. A device is reachable only through an enabled,
unsuspended port.

### Standard requests

Every device answers `GET_DESCRIPTOR` (device, configuration, string; string
0 is the language table, en-US), `SET_ADDRESS` (applied after the status
stage), `SET_CONFIGURATION`, `GET_CONFIGURATION`, `GET_STATUS`,
`GET_INTERFACE`, and accepts `SET_INTERFACE`, `SET_FEATURE`,
`CLEAR_FEATURE`. Anything else stalls.

## XID gamepad

The standard controller (Duke / Controller S): vendor 0x045E, product
0x0202, one interface of class 0x58 subclass 0x42, two interrupt endpoints,
32-byte packets, interval 4 ms.

| Endpoint | Direction | Carries |
|---|---|---|
| 0x82 | IN | input report |
| 0x02 | OUT | output (rumble) report |

Class and vendor requests:

| bmRequestType | bRequest | wValue | Answer |
|---|---|---|---|
| 0xC1 | 0x06 | 0x4200 | XID descriptor (16 bytes: length, type 0x42, bcdXid 1.00, type 1, subtype 1, input report length 20, output report length 6, 8 bytes 0xFF) |
| 0xC1 | 0x01 | 0x0100 | input capabilities: a report with every bit and axis set |
| 0xC1 | 0x01 | 0x0200 | output capabilities, likewise |
| 0xA1 | 0x01 | 0x0100 | `GET_REPORT`: the current input report |
| 0x21 | 0x09 | 0x0200 | `SET_REPORT`: an output report |

### Reports

The layouts are defined once, in `xb_devices::usb::xid::report`
(`encode_input`, `decode_input`, `encode_output`, `decode_output`). All
fields are little endian.

Input report, 20 bytes:

| Offset | Size | Field |
|---|---|---|
| 0 | 1 | report id, 0 |
| 1 | 1 | length, 20 |
| 2 | 2 | digital buttons: bit 0 D-pad up, 1 down, 2 left, 3 right, 4 Start, 5 Back, 6 left stick, 7 right stick |
| 4 | 1 | A (0-255, pressure) |
| 5 | 1 | B |
| 6 | 1 | X |
| 7 | 1 | Y |
| 8 | 1 | Black |
| 9 | 1 | White |
| 10 | 1 | left trigger |
| 11 | 1 | right trigger |
| 12 | 2 | left stick X, signed |
| 14 | 2 | left stick Y, signed (up positive) |
| 16 | 2 | right stick X |
| 18 | 2 | right stick Y |

`xb_types::pad::PadState` has the same fields; `button` and `analog` there
name the bits and the order.

Output report, 6 bytes:

| Offset | Size | Field |
|---|---|---|
| 0 | 1 | report id, 0 |
| 1 | 1 | length, 6 |
| 2 | 2 | left (low frequency) motor, 0-65535 |
| 4 | 2 | right (high frequency) motor |

### When the pad reports

The interrupt IN endpoint answers only when the report differs from the last
one sent, and NAKs otherwise. XInput's packet number then moves only when
the state does; a title waiting for the pad to be idle (an attract mode)
depends on that. A bus reset, or `PadPorts::resend` (for a savestate load),
makes the next poll report unconditionally.

## Front-end path

```
front end ── PadPorts::set_pad(port, &PadState) ──▶ gamepad's next IN poll
front end ◀─ PadPorts::rumble(port) ── last output report the title sent
front end ── PadPorts::set_connected(port, bool) ──▶ root port at next frame
```

`PadPorts` is a cloneable handle usable from any thread. The menu's "the
title sees a released pad" is done by the machine passing a default
`PadState` while the menu is open.

## Not implemented

Isochronous transfers and the Xbox Communicator; memory units; the 1.0
console's front-panel hub; the second OHCI controller (IRQ 9), which the
reference does not emulate either.
