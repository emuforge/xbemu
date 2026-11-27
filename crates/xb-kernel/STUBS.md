# Kernel exports that don't do the Xbox kernel's work

Some exports are accepted here without doing what the Xbox kernel does. Each
row says what happens instead, and why that is enough or what is missing.

An export this kernel doesn't implement at all stops the guest when called
(a fatal stop whose reason starts with `kunimpl` and names the export, its
caller and its arguments). `Kernel::implemented(ordinal)` tells which
function exports have an implementation.

| Export | Here | Why |
|---|---|---|
| `HalRegisterShutdownNotification`, `HalIsResetOrShutdownPending` | nothing, 0 | No shutdown in the guest: the machine ends the run. |
| `HalEnableSecureTrayEject` | nothing | The tray never opens. |
| `HalReadSMCTrayState` | closed, with or without a disc; change count 0 | The tray never moves: the disc is the one the title booted with. |
| `HalReadSMBusValue` | the system controller (0x20) answers its version ("P01", a letter a read), the tray state and the AV pack; its other registers read 0. Any other device is asked of the machine (`Platform::smbus`, 0 by default). Always success | The titles read the AV pack and the tray; temperatures, the video encoder and the EEPROM chip are not on a bus here (the EEPROM is `ExQueryNonVolatileSetting`'s). |
| `HalWriteSMBusValue` | handed to the machine (`Platform::smbus`), which ignores it by default; success | The SMBus devices' settings (the SMC's LED and fan, the video encoder) have no effect in the kernel. |
| `HalReadWritePCISpace` | reads zeros, writes ignored, logged | The PCI devices are emulated by their registers, not their config space. |
| `AvSendTVEncoderOption` | `AV_QUERY_AV_CAPABILITIES` (6) answered from the AV pack and the EEPROM's video standard and flags; every other option does nothing and answers 0, logged once an option | No TV encoder: the machine shows the frame buffer whatever the encoder was told (macrovision, CGMS, flicker filter...). |
| `FscSetCacheSize` | keeps the size for `FscGetCacheSize`, allocates nothing | No file-system cache of its own: the host's page cache serves the disc and the drives. |
| `DbgBreakPoint`, `DbgBreakPointWithStatus` | logged, the title goes on | No debugger to break into. |
| `KeSaveFloatingPointState`, `KeRestoreFloatingPointState` | success | A thread's FPU state stays as it is across kernel calls. |
| `KeEnterCriticalRegion`, `KeLeaveCriticalRegion` | nothing | APCs are delivered only in alertable waits, never into a region. |
| `KeReleaseSemaphore` past its limit | not released, logged once | The Xbox kernel raises STATUS_SEMAPHORE_LIMIT_EXCEEDED. |
| `RtlAssert`, `RtlRip` | logged with the caller's frames, the title goes on | No debugger to prompt or break into. |
| `RtlEnterCriticalSectionAndRegion`, `RtlLeaveCriticalSectionAndRegion` | the critical section only | The region is nothing here (see `KeEnterCriticalRegion`). |
| `RtlExtendedLargeIntegerDivide` by zero | stops the guest | The Xbox kernel raises STATUS_INTEGER_DIVIDE_BY_ZERO. |
| `XcVerifyPKCS1Signature` | accepts the digest in either byte order of the decrypted block | The block's byte order isn't pinned down (as the reference). |
| `RtlUnicodeToMultiByteN`, `RtlMultiByteToUnicodeN` | STATUS_SUCCESS when the output is truncated | As the reference's RtlUpcaseUnicodeToMultiByteN; later NT kernels return STATUS_BUFFER_OVERFLOW, the Xbox kernel's choice is unverified. |
| `NtProtectVirtualMemory`, `MmSetAddressProtect` | read-only enforced on virtual memory outside the image; every other protection (PAGE_NOACCESS too) is recorded only | `NtQueryVirtualMemory` reports what was set. |
| `MmUnmapIoSpace` | nothing | I/O space is never mapped. |
| `XcCryptService`, `XcUpdateCrypto` | 0, logged | No replaceable crypto vector. |
| `IoSynchronousDeviceIoControlRequest` | zeros and success, logged | No device objects behind it. |
| `IoDismountVolumeByName` | STATUS_SUCCESS, nothing dismounted | Volumes are host folders (as the reference). |
| `IoDeleteDevice` | forgets the device's name and sets DeletePending; the object is not freed | As the reference. |
| `ObReferenceObjectByName` | device objects only (created ones by name; the disc and partitions get an object on first reference); no reference is counted | As the reference. |
| `IoCreateFile` | `NtCreateFile` with its `Options` argument ignored | Not in the reference; the extra argument only selects kernel-internal open behavior. |
| `NtDeviceIoControlFile`, `NtFsControlFile` | known codes answered; any other fails (invalid device request), logged once | The known ones are what titles send: the disk's geometry and partition info, the DVD drive's SCSI pass-through (the Xbox security page), and a format's volume lock, unlock and dismount. |

## Implemented, with a difference worth knowing

These do their work, in this kernel's way:

- `HalReturnToFirmware` ends the run. A quick reboot (routine 2) with a
  launch data page naming an XBE on the disc or the hard disk (or asking an
  installed dashboard for one of its pages) is `TitleExit::Relaunch` with the
  page: the kernel can't reboot in place, and the machine boots the next XBE
  with a new kernel. Anything else is `TitleExit::Firmware`.
  `HalInitiateShutdown` is `TitleExit::Shutdown`.
- `AvSetDisplayMode` hands the mode, format, pitch and frame buffer to the
  machine with the refresh rate (50 Hz for a mode with bit 30, else 60) and
  writes no encoder or CRTC register. The title's picture shape (16:9 or 4:3)
  is looked for in its D3D presentation parameters, on the callers' stack and
  then in RAM.
- `PhyGetLinkState` and `PhyInitialize` are the machine's (`Platform`): no
  link by default.
- `ExAllocatePoolWithTag` ignores the tag, `ExInterlockedAddLargeInteger` its
  lock (one CPU, and nothing preempts a kernel call).
- `ExSaveNonVolatileSetting` writes the user section's settings and the AV
  region whatever `Type` says; any other setting is access denied.
- `DbgPrint` logs the text: `%s %c %d %i %u %x %X %p` with flags, width and
  precision, anything else printed as `<%spec>`, at most 511 bytes.
- `KeBugCheck` and `KeBugCheckEx` are a fatal stop with the code.
- `KeStallExecutionProcessor` waits on the host that long, a tenth of a
  second at most (its callers ask for microseconds).

## Data exports

- `XboxHardwareInfo`, `XboxEEPROMKey`, `XboxSignatureKey`, `XboxLANKey` and
  `XboxAlternateSignatureKeys` are zeros, as the reference leaves them.
- `XboxHDKey` is the EEPROM's HD key; a new console's is the one every
  reference console had, so saves locked to the console stay valid.
- `HalDiskModelNumber` is "XBASED HDD" and `HalDiskSerialNumber` "XB" with
  the console's serial number: no drive to identify, and the serial stays
  the same on every run.
- `HalBootSMCVideoMode` is the configured AV pack, as the system controller
  would have reported it at boot.
- `LaunchDataPage` is 0 until the title allocates one, or the page the last
  title launched this one with.

## The EEPROM

`ExQueryNonVolatileSetting` serves one 256-byte image, kept as `eeprom.bin`
next to the hard disk's folder (in memory only for a kernel without one). A
new image is an NTSC or PAL console's by the first title's region, with a
random serial number, MAC address (00:50:F2:xx:xx:xx) and online key, and
its encrypted section under the MCPX 1.0 EEPROM key. On every run the game
region (the configuration's, when it names one), the video standard and the
DVD region follow the running title, and the video flags, audio flags,
language and time zone are the configuration's (`ConsoleConfig`); those never
reach the file, whatever a title saved. An image whose section checks under
no known key is used as it is, its section in the clear.
