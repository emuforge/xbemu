# The kernel: guest-memory layout and export table

`xb-kernel` reimplements `xboxkrnl.exe` on the host. This page says where the
kernel's pieces sit in the guest's address space and what each export does
here. Sources: the reference implementation's kernel for the layout, and
nxdk's `xboxkrnl.exe.def` for the table.

## How a title enters the kernel

The loader replaces each entry of the XBE's kernel thunk table
(`0x8000_0000 | ordinal`) with an address:

- a **function export**: `TRAP_VA + ordinal`, an address in the trap range
  (`Cpu::set_trap_range`). Executing it returns `Exit::Trap` to the kernel,
  which reads the arguments for the export's convention (stdcall: on the
  stack; fastcall: ECX, EDX, then the stack), runs the implementation, sets
  EDX:EAX, pops the return address and the arguments and resumes at the
  caller. Twenty hot exports (critical sections, IRQL, interlocked and memory
  routines; `src/fast.rs`) instead resolve to x86 code in the kernel's area
  that handles the common case in the guest and ends in `jmp TRAP_VA +
  ordinal` for the rest.
- a **data export**: the address of the variable in the kernel's area.

Guest routines the kernel calls (thread start routines, DPCs, ISRs, APCs,
callouts such as `KeSynchronizeExecution`'s routine, exception handlers)
return to sentinels in the same range:

| Address | Reached when |
|---|---|
| `TRAP_VA + 1 ..= 378` | an export is called |
| `TRAP_VA + 0x3F5` | DirectSound's mailbox wait stub asks for the APU (`fixups.rs`) |
| `TRAP_VA + 0x3F7` | the kernel's own SEH registration is called during an unwind (collided) |
| `TRAP_VA + 0x3F8` | the same during a dispatch (nested) |
| `TRAP_VA + 0x3F9` | an exception handler returned |
| `TRAP_VA + 0x3FB` | an APC routine returned |
| `TRAP_VA + 0x3FC` | an interrupt service routine returned |
| `TRAP_VA + 0x3FD` | a callout routine returned |
| `TRAP_VA + 0x3FE` | a DPC routine returned |
| `TRAP_VA + 0x3FF` | a thread's start routine returned |

A function export without an implementation stops the guest with
`kunimpl <ordinal> <name> caller=... args=[...]`. A title's unimplemented
imports are named when it loads (`imports: title ... imports N kernel
functions not implemented here: ...`).

## Guest virtual layout

| Range | What |
|---|---|
| `0x0001_0000 .. 0x7FFE_0000` | the title: its XBE image at `0x0001_0000`, and `NtAllocateVirtualMemory` regions (64 KiB grain, bottom-up or top-down) |
| `0x8000_0000 + pa` | all of RAM, contiguous. `MmAllocateContiguousMemory`, `MmAllocateSystemMemory`, the kernel pool and kernel objects are addresses here |
| `0x8001_0000 .. 0x8003_0000` | the kernel's area (below) |
| `0x8005_0000 .. 0x8006_0000` | the kernel pool's first chunk (physical `0x50000`); later chunks come from the top of RAM |
| `0x8FF0_0000 .. 0x8FF0_1000` | the trap range (`TRAP_VA`); nothing is ever mapped here |
| `0xD000_0000 .. 0xF000_0000` | kernel stacks (threads, `MmCreateKernelStack`): 64 KiB slots, each stack above an unmapped 64 KiB guard, 16 KiB of zeroed pages over a thread's stack top |
| `0xF000_0000 + pa` | the NV2A's view of RAM |
| `0xFD00_0000` and up | device registers, mapped by the machine (`Kernel::map_device`) |
| `0xFFFE_0000 .. 0xFFFE_3000` | the KVM backend's own pages; the kernel never maps there |

A thread's stack, from the top: the FX save area (`0x210` bytes; the TIB's
`StackBase`, `fs:[4]`, is its bottom), the TLS block (`TlsDataSize`, its
first dword pointing past itself), then the thread's frames.

## The kernel's area (`KERNEL_VA = 0x8001_0000`, physical `0x10000`)

| Offset | What |
|---|---|
| `0x0000` | a PE header with an export directory (`AddressOfFunctions` at `0x300`: entry points and data by ordinal), for code that looks exports up in the image |
| `0x2000 + ordinal * 0x40` | data exports, one 64-byte slot each |
| `0xA000` | `XboxAlternateSignatureKeys` (16 keys of 16 bytes) |
| `0xA100` | `XePublicKeyData` (`0x11C` bytes) |
| `0xA400` | head of the named-object list |
| `0x10000` | the KPCR (`fs` base `0x8002_0000`): `ExceptionList` 0, `StackBase` 4, `StackLimit` 8, `SelfPcr` 0x1C, `Prcb` 0x20, `Irql` 0x24, `CurrentThread` 0x28; `0x800`: 1 while DPCs are queued (read by the guest-code `KfLowerIrql`) |
| `0x11800` | a GDT image as the Xbox kernel's (null, code 08, data 10, TSS 18, KPCR 20 and 28), `0x11900` a TSS image: data only. `SIDT`/`SGDT` report the CPU backend's tables, and a title cannot hook an interrupt vector |
| `0x12000 .. 0x13000` | exports that run as guest code, then stubs (`fixups.rs`, xbdm stubs) |

Physical pages `0x10000 .. 0x30000` belong to the kernel; the rest of the
area is ordinary free memory, as on the console once the kernel's INIT
section is gone (Halo lays out its game state from `0x61000`). Physical
memory below `0x10000` is never handed out. The top `0x15000` bytes of RAM
are kept from boot for the GPU's instance memory and its pad
(`MmClaimGpuInstanceMemory`).

Data exports the kernel keeps current at every entry: `KeTickCount` (1 kHz),
`KeInterruptTime`, `KeSystemTime`. Set at boot or when the title loads:
`XboxKrnlVersion` (1.0.5838.1), `KeTimeIncrement`, `KdDebuggerNotPresent`,
`HalDiskCachePartitionCount`, `IdexChannelObject`'s empty device queue, the
object type objects, `XboxHardwareInfo`, `XboxEEPROMKey`, `XboxHDKey`,
`XboxSignatureKey`, `XboxLANKey`, `XboxAlternateSignatureKeys`,
`XePublicKeyData`, `XeImageFileName`, `LaunchDataPage`.

## Export table

Stand-ins are described in `crates/xb-kernel/STUBS.md`. The table below is
written by a test (`XB_KERNEL_WRITE_DOCS=1 cargo test -p xb-kernel
the_export_table`) from the kernel as built, and the same test fails when the
table is stale.

<!-- export table: generated by the test below; do not edit -->
219 function exports implemented, 36 documented stand-ins, 82 unimplemented (a call stops the guest and names the export); 34 data exports.

| Ordinal | Export | Convention | Status |
|---|---|---|---|
| 1 | `AvGetSavedDataAddress` | stdcall, 0 bytes | implemented |
| 2 | `AvSendTVEncoderOption` | stdcall, 16 bytes | stand-in (STUBS.md) |
| 3 | `AvSetDisplayMode` | stdcall, 24 bytes | implemented |
| 4 | `AvSetSavedDataAddress` | stdcall, 4 bytes | implemented |
| 5 | `DbgBreakPoint` | stdcall, 0 bytes | stand-in (STUBS.md) |
| 6 | `DbgBreakPointWithStatus` | stdcall, 4 bytes | stand-in (STUBS.md) |
| 7 | `DbgLoadImageSymbols` | stdcall, 12 bytes | unimplemented |
| 8 | `DbgPrint` | cdecl | implemented |
| 9 | `HalReadSMCTrayState` | stdcall, 8 bytes | stand-in (STUBS.md) |
| 10 | `DbgPrompt` | stdcall, 12 bytes | unimplemented |
| 11 | `DbgUnLoadImageSymbols` | stdcall, 12 bytes | unimplemented |
| 12 | `ExAcquireReadWriteLockExclusive` | stdcall, 4 bytes | implemented |
| 13 | `ExAcquireReadWriteLockShared` | stdcall, 4 bytes | implemented |
| 14 | `ExAllocatePool` | stdcall, 4 bytes | implemented |
| 15 | `ExAllocatePoolWithTag` | stdcall, 8 bytes | implemented |
| 16 | `ExEventObjectType` | data | data at `80012400` |
| 17 | `ExFreePool` | stdcall, 4 bytes | implemented |
| 18 | `ExInitializeReadWriteLock` | stdcall, 4 bytes | implemented |
| 19 | `ExInterlockedAddLargeInteger` | stdcall, 16 bytes | implemented |
| 20 | `ExInterlockedAddLargeStatistic` | fastcall, 8 bytes | implemented |
| 21 | `ExInterlockedCompareExchange64` | fastcall, 12 bytes | implemented |
| 22 | `ExMutantObjectType` | data | data at `80012580` |
| 23 | `ExQueryPoolBlockSize` | stdcall, 4 bytes | implemented |
| 24 | `ExQueryNonVolatileSetting` | stdcall, 20 bytes | implemented |
| 25 | `ExReadWriteRefurbInfo` | stdcall, 12 bytes | implemented |
| 26 | `ExRaiseException` | stdcall, 4 bytes | unimplemented |
| 27 | `ExRaiseStatus` | stdcall, 4 bytes | unimplemented |
| 28 | `ExReleaseReadWriteLock` | stdcall, 4 bytes | implemented |
| 29 | `ExSaveNonVolatileSetting` | stdcall, 16 bytes | implemented |
| 30 | `ExSemaphoreObjectType` | data | data at `80012780` |
| 31 | `ExTimerObjectType` | data | data at `800127c0` |
| 32 | `ExfInterlockedInsertHeadList` | fastcall, 8 bytes | implemented |
| 33 | `ExfInterlockedInsertTailList` | fastcall, 8 bytes | implemented |
| 34 | `ExfInterlockedRemoveHeadList` | fastcall, 4 bytes | implemented |
| 35 | `FscGetCacheSize` | stdcall, 0 bytes | implemented |
| 36 | `FscInvalidateIdleBlocks` | stdcall, 0 bytes | unimplemented |
| 37 | `FscSetCacheSize` | stdcall, 4 bytes | stand-in (STUBS.md) |
| 38 | `HalClearSoftwareInterrupt` | fastcall, 4 bytes | unimplemented |
| 39 | `HalDisableSystemInterrupt` | stdcall, 4 bytes | unimplemented |
| 40 | `HalDiskCachePartitionCount` | data | data at `80012a00` |
| 41 | `HalDiskModelNumber` | data | data at `80012a40` |
| 42 | `HalDiskSerialNumber` | data | data at `80012a80` |
| 43 | `HalEnableSystemInterrupt` | stdcall, 8 bytes | unimplemented |
| 44 | `HalGetInterruptVector` | stdcall, 8 bytes | implemented |
| 45 | `HalReadSMBusValue` | stdcall, 16 bytes | stand-in (STUBS.md) |
| 46 | `HalReadWritePCISpace` | stdcall, 24 bytes | stand-in (STUBS.md) |
| 47 | `HalRegisterShutdownNotification` | stdcall, 8 bytes | stand-in (STUBS.md) |
| 48 | `HalRequestSoftwareInterrupt` | fastcall, 4 bytes | unimplemented |
| 49 | `HalReturnToFirmware` | stdcall, 4 bytes | implemented |
| 50 | `HalWriteSMBusValue` | stdcall, 16 bytes | stand-in (STUBS.md) |
| 51 | `InterlockedCompareExchange` | fastcall, 12 bytes | implemented (guest code, kernel for the rest) |
| 52 | `InterlockedDecrement` | fastcall, 4 bytes | implemented (guest code, kernel for the rest) |
| 53 | `InterlockedIncrement` | fastcall, 4 bytes | implemented (guest code, kernel for the rest) |
| 54 | `InterlockedExchange` | fastcall, 8 bytes | implemented (guest code, kernel for the rest) |
| 55 | `InterlockedExchangeAdd` | fastcall, 8 bytes | implemented (guest code, kernel for the rest) |
| 56 | `InterlockedFlushSList` | fastcall, 4 bytes | implemented |
| 57 | `InterlockedPopEntrySList` | fastcall, 4 bytes | implemented |
| 58 | `InterlockedPushEntrySList` | fastcall, 8 bytes | implemented |
| 59 | `IoAllocateIrp` | stdcall, 4 bytes | unimplemented |
| 60 | `IoBuildAsynchronousFsdRequest` | stdcall, 24 bytes | unimplemented |
| 61 | `IoBuildDeviceIoControlRequest` | stdcall, 36 bytes | unimplemented |
| 62 | `IoBuildSynchronousFsdRequest` | stdcall, 28 bytes | unimplemented |
| 63 | `IoCheckShareAccess` | stdcall, 20 bytes | unimplemented |
| 64 | `IoCompletionObjectType` | data | data at `80013000` |
| 65 | `IoCreateDevice` | stdcall, 24 bytes | implemented |
| 66 | `IoCreateFile` | stdcall, 40 bytes | stand-in (STUBS.md) |
| 67 | `IoCreateSymbolicLink` | stdcall, 8 bytes | implemented |
| 68 | `IoDeleteDevice` | stdcall, 4 bytes | stand-in (STUBS.md) |
| 69 | `IoDeleteSymbolicLink` | stdcall, 4 bytes | implemented |
| 70 | `IoDeviceObjectType` | data | data at `80013180` |
| 71 | `IoFileObjectType` | data | data at `800131c0` |
| 72 | `IoFreeIrp` | stdcall, 4 bytes | unimplemented |
| 73 | `IoInitializeIrp` | stdcall, 12 bytes | unimplemented |
| 74 | `IoInvalidDeviceRequest` | stdcall, 8 bytes | unimplemented |
| 75 | `IoQueryFileInformation` | stdcall, 20 bytes | unimplemented |
| 76 | `IoQueryVolumeInformation` | stdcall, 20 bytes | implemented |
| 77 | `IoQueueThreadIrp` | stdcall, 4 bytes | unimplemented |
| 78 | `IoRemoveShareAccess` | stdcall, 8 bytes | unimplemented |
| 79 | `IoSetIoCompletion` | stdcall, 20 bytes | unimplemented |
| 80 | `IoSetShareAccess` | stdcall, 16 bytes | unimplemented |
| 81 | `IoStartNextPacket` | stdcall, 4 bytes | unimplemented |
| 82 | `IoStartNextPacketByKey` | stdcall, 8 bytes | unimplemented |
| 83 | `IoStartPacket` | stdcall, 12 bytes | unimplemented |
| 84 | `IoSynchronousDeviceIoControlRequest` | stdcall, 32 bytes | stand-in (STUBS.md) |
| 85 | `IoSynchronousFsdRequest` | stdcall, 20 bytes | unimplemented |
| 86 | `IofCallDriver` | fastcall, 8 bytes | unimplemented |
| 87 | `IofCompleteRequest` | fastcall, 8 bytes | unimplemented |
| 88 | `KdDebuggerEnabled` | data | data at `80013600` |
| 89 | `KdDebuggerNotPresent` | data | data at `80013640` |
| 90 | `IoDismountVolume` | stdcall, 4 bytes | unimplemented |
| 91 | `IoDismountVolumeByName` | stdcall, 4 bytes | stand-in (STUBS.md) |
| 92 | `KeAlertResumeThread` | stdcall, 4 bytes | unimplemented |
| 93 | `KeAlertThread` | stdcall, 8 bytes | unimplemented |
| 94 | `KeBoostPriorityThread` | stdcall, 8 bytes | implemented |
| 95 | `KeBugCheck` | stdcall, 4 bytes | implemented |
| 96 | `KeBugCheckEx` | stdcall, 20 bytes | implemented |
| 97 | `KeCancelTimer` | stdcall, 4 bytes | implemented |
| 98 | `KeConnectInterrupt` | stdcall, 4 bytes | implemented |
| 99 | `KeDelayExecutionThread` | stdcall, 12 bytes | implemented |
| 100 | `KeDisconnectInterrupt` | stdcall, 4 bytes | implemented |
| 101 | `KeEnterCriticalRegion` | stdcall, 0 bytes | stand-in (STUBS.md) |
| 102 | `MmGlobalData` | data | data at `80013980` |
| 103 | `KeGetCurrentIrql` | stdcall, 0 bytes | implemented (guest code, kernel for the rest) |
| 104 | `KeGetCurrentThread` | stdcall, 0 bytes | implemented (guest code, kernel for the rest) |
| 105 | `KeInitializeApc` | stdcall, 28 bytes | implemented |
| 106 | `KeInitializeDeviceQueue` | stdcall, 4 bytes | unimplemented |
| 107 | `KeInitializeDpc` | stdcall, 12 bytes | implemented |
| 108 | `KeInitializeEvent` | stdcall, 12 bytes | implemented |
| 109 | `KeInitializeInterrupt` | stdcall, 28 bytes | implemented |
| 110 | `KeInitializeMutant` | stdcall, 8 bytes | implemented |
| 111 | `KeInitializeQueue` | stdcall, 8 bytes | unimplemented |
| 112 | `KeInitializeSemaphore` | stdcall, 12 bytes | implemented |
| 113 | `KeInitializeTimerEx` | stdcall, 8 bytes | implemented |
| 114 | `KeInsertByKeyDeviceQueue` | stdcall, 12 bytes | unimplemented |
| 115 | `KeInsertDeviceQueue` | stdcall, 8 bytes | unimplemented |
| 116 | `KeInsertHeadQueue` | stdcall, 8 bytes | unimplemented |
| 117 | `KeInsertQueue` | stdcall, 8 bytes | unimplemented |
| 118 | `KeInsertQueueApc` | stdcall, 16 bytes | implemented |
| 119 | `KeInsertQueueDpc` | stdcall, 12 bytes | implemented |
| 120 | `KeInterruptTime` | data | data at `80013e00` |
| 121 | `KeIsExecutingDpc` | stdcall, 0 bytes | implemented |
| 122 | `KeLeaveCriticalRegion` | stdcall, 0 bytes | stand-in (STUBS.md) |
| 123 | `KePulseEvent` | stdcall, 12 bytes | implemented |
| 124 | `KeQueryBasePriorityThread` | stdcall, 4 bytes | implemented |
| 125 | `KeQueryInterruptTime` | stdcall, 0 bytes | implemented |
| 126 | `KeQueryPerformanceCounter` | stdcall, 0 bytes | implemented |
| 127 | `KeQueryPerformanceFrequency` | stdcall, 0 bytes | implemented |
| 128 | `KeQuerySystemTime` | stdcall, 4 bytes | implemented |
| 129 | `KeRaiseIrqlToDpcLevel` | stdcall, 0 bytes | implemented (guest code, kernel for the rest) |
| 130 | `KeRaiseIrqlToSynchLevel` | stdcall, 0 bytes | implemented (guest code, kernel for the rest) |
| 131 | `KeReleaseMutant` | stdcall, 16 bytes | implemented |
| 132 | `KeReleaseSemaphore` | stdcall, 16 bytes | stand-in (STUBS.md) |
| 133 | `KeRemoveByKeyDeviceQueue` | stdcall, 8 bytes | unimplemented |
| 134 | `KeRemoveDeviceQueue` | stdcall, 4 bytes | unimplemented |
| 135 | `KeRemoveEntryDeviceQueue` | stdcall, 8 bytes | unimplemented |
| 136 | `KeRemoveQueue` | stdcall, 12 bytes | unimplemented |
| 137 | `KeRemoveQueueDpc` | stdcall, 4 bytes | implemented |
| 138 | `KeResetEvent` | stdcall, 4 bytes | implemented |
| 139 | `KeRestoreFloatingPointState` | stdcall, 4 bytes | stand-in (STUBS.md) |
| 140 | `KeResumeThread` | stdcall, 4 bytes | implemented |
| 141 | `KeRundownQueue` | stdcall, 4 bytes | unimplemented |
| 142 | `KeSaveFloatingPointState` | stdcall, 4 bytes | stand-in (STUBS.md) |
| 143 | `KeSetBasePriorityThread` | stdcall, 8 bytes | implemented |
| 144 | `KeSetDisableBoostThread` | stdcall, 8 bytes | implemented |
| 145 | `KeSetEvent` | stdcall, 12 bytes | implemented |
| 146 | `KeSetEventBoostPriority` | stdcall, 8 bytes | unimplemented |
| 147 | `KeSetPriorityProcess` | stdcall, 8 bytes | unimplemented |
| 148 | `KeSetPriorityThread` | stdcall, 8 bytes | implemented |
| 149 | `KeSetTimer` | stdcall, 16 bytes | implemented |
| 150 | `KeSetTimerEx` | stdcall, 20 bytes | implemented |
| 151 | `KeStallExecutionProcessor` | stdcall, 4 bytes | implemented |
| 152 | `KeSuspendThread` | stdcall, 4 bytes | implemented |
| 153 | `KeSynchronizeExecution` | stdcall, 12 bytes | implemented |
| 154 | `KeSystemTime` | data | data at `80014680` |
| 155 | `KeTestAlertThread` | stdcall, 4 bytes | implemented |
| 156 | `KeTickCount` | data | data at `80014700` |
| 157 | `KeTimeIncrement` | data | data at `80014740` |
| 158 | `KeWaitForMultipleObjects` | stdcall, 32 bytes | implemented |
| 159 | `KeWaitForSingleObject` | stdcall, 20 bytes | implemented |
| 160 | `KfRaiseIrql` | fastcall, 4 bytes | implemented (guest code, kernel for the rest) |
| 161 | `KfLowerIrql` | fastcall, 4 bytes | implemented (guest code, kernel for the rest) |
| 162 | `KiBugCheckData` | data | data at `80014880` |
| 163 | `KiUnlockDispatcherDatabase` | fastcall, 4 bytes | unimplemented |
| 164 | `LaunchDataPage` | data | data at `80014900` |
| 165 | `MmAllocateContiguousMemory` | stdcall, 4 bytes | implemented |
| 166 | `MmAllocateContiguousMemoryEx` | stdcall, 20 bytes | implemented |
| 167 | `MmAllocateSystemMemory` | stdcall, 8 bytes | implemented |
| 168 | `MmClaimGpuInstanceMemory` | stdcall, 8 bytes | implemented |
| 169 | `MmCreateKernelStack` | stdcall, 8 bytes | implemented |
| 170 | `MmDeleteKernelStack` | stdcall, 8 bytes | implemented |
| 171 | `MmFreeContiguousMemory` | stdcall, 4 bytes | implemented |
| 172 | `MmFreeSystemMemory` | stdcall, 8 bytes | implemented |
| 173 | `MmGetPhysicalAddress` | stdcall, 4 bytes | implemented |
| 174 | `MmIsAddressValid` | stdcall, 4 bytes | implemented |
| 175 | `MmLockUnlockBufferPages` | stdcall, 12 bytes | implemented |
| 176 | `MmLockUnlockPhysicalPage` | stdcall, 8 bytes | implemented |
| 177 | `MmMapIoSpace` | stdcall, 12 bytes | implemented |
| 178 | `MmPersistContiguousMemory` | stdcall, 12 bytes | implemented |
| 179 | `MmQueryAddressProtect` | stdcall, 4 bytes | implemented |
| 180 | `MmQueryAllocationSize` | stdcall, 4 bytes | implemented |
| 181 | `MmQueryStatistics` | stdcall, 4 bytes | implemented |
| 182 | `MmSetAddressProtect` | stdcall, 12 bytes | stand-in (STUBS.md) |
| 183 | `MmUnmapIoSpace` | stdcall, 8 bytes | stand-in (STUBS.md) |
| 184 | `NtAllocateVirtualMemory` | stdcall, 20 bytes | implemented |
| 185 | `NtCancelTimer` | stdcall, 8 bytes | implemented |
| 186 | `NtClearEvent` | stdcall, 4 bytes | implemented |
| 187 | `NtClose` | stdcall, 4 bytes | implemented |
| 188 | `NtCreateDirectoryObject` | stdcall, 8 bytes | unimplemented |
| 189 | `NtCreateEvent` | stdcall, 16 bytes | implemented |
| 190 | `NtCreateFile` | stdcall, 36 bytes | implemented |
| 191 | `NtCreateIoCompletion` | stdcall, 16 bytes | implemented |
| 192 | `NtCreateMutant` | stdcall, 12 bytes | implemented |
| 193 | `NtCreateSemaphore` | stdcall, 16 bytes | implemented |
| 194 | `NtCreateTimer` | stdcall, 12 bytes | implemented |
| 195 | `NtDeleteFile` | stdcall, 4 bytes | implemented |
| 196 | `NtDeviceIoControlFile` | stdcall, 40 bytes | stand-in (STUBS.md) |
| 197 | `NtDuplicateObject` | stdcall, 12 bytes | implemented |
| 198 | `NtFlushBuffersFile` | stdcall, 8 bytes | implemented |
| 199 | `NtFreeVirtualMemory` | stdcall, 12 bytes | implemented |
| 200 | `NtFsControlFile` | stdcall, 40 bytes | stand-in (STUBS.md) |
| 201 | `NtOpenDirectoryObject` | stdcall, 8 bytes | unimplemented |
| 202 | `NtOpenFile` | stdcall, 24 bytes | implemented |
| 203 | `NtOpenSymbolicLinkObject` | stdcall, 8 bytes | implemented |
| 204 | `NtProtectVirtualMemory` | stdcall, 16 bytes | stand-in (STUBS.md) |
| 205 | `NtPulseEvent` | stdcall, 8 bytes | implemented |
| 206 | `NtQueueApcThread` | stdcall, 20 bytes | implemented |
| 207 | `NtQueryDirectoryFile` | stdcall, 40 bytes | implemented |
| 208 | `NtQueryDirectoryObject` | stdcall, 24 bytes | unimplemented |
| 209 | `NtQueryEvent` | stdcall, 8 bytes | unimplemented |
| 210 | `NtQueryFullAttributesFile` | stdcall, 8 bytes | implemented |
| 211 | `NtQueryInformationFile` | stdcall, 20 bytes | implemented |
| 212 | `NtQueryIoCompletion` | stdcall, 8 bytes | implemented |
| 213 | `NtQueryMutant` | stdcall, 8 bytes | unimplemented |
| 214 | `NtQuerySemaphore` | stdcall, 8 bytes | unimplemented |
| 215 | `NtQuerySymbolicLinkObject` | stdcall, 12 bytes | implemented |
| 216 | `NtQueryTimer` | stdcall, 8 bytes | implemented |
| 217 | `NtQueryVirtualMemory` | stdcall, 8 bytes | implemented |
| 218 | `NtQueryVolumeInformationFile` | stdcall, 20 bytes | implemented |
| 219 | `NtReadFile` | stdcall, 32 bytes | implemented |
| 220 | `NtReadFileScatter` | stdcall, 32 bytes | implemented |
| 221 | `NtReleaseMutant` | stdcall, 8 bytes | implemented |
| 222 | `NtReleaseSemaphore` | stdcall, 12 bytes | implemented |
| 223 | `NtRemoveIoCompletion` | stdcall, 20 bytes | implemented |
| 224 | `NtResumeThread` | stdcall, 8 bytes | implemented |
| 225 | `NtSetEvent` | stdcall, 8 bytes | implemented |
| 226 | `NtSetInformationFile` | stdcall, 20 bytes | implemented |
| 227 | `NtSetIoCompletion` | stdcall, 20 bytes | implemented |
| 228 | `NtSetSystemTime` | stdcall, 8 bytes | implemented |
| 229 | `NtSetTimerEx` | stdcall, 32 bytes | implemented |
| 230 | `NtSignalAndWaitForSingleObjectEx` | stdcall, 20 bytes | implemented |
| 231 | `NtSuspendThread` | stdcall, 8 bytes | implemented |
| 232 | `NtUserIoApcDispatcher` | stdcall, 12 bytes | implemented |
| 233 | `NtWaitForSingleObject` | stdcall, 12 bytes | implemented |
| 234 | `NtWaitForSingleObjectEx` | stdcall, 16 bytes | implemented |
| 235 | `NtWaitForMultipleObjectsEx` | stdcall, 24 bytes | implemented |
| 236 | `NtWriteFile` | stdcall, 32 bytes | implemented |
| 237 | `NtWriteFileGather` | stdcall, 32 bytes | implemented |
| 238 | `NtYieldExecution` | stdcall, 0 bytes | implemented |
| 239 | `ObCreateObject` | stdcall, 16 bytes | unimplemented |
| 240 | `ObDirectoryObjectType` | data | data at `80015c00` |
| 241 | `ObInsertObject` | stdcall, 16 bytes | unimplemented |
| 242 | `ObMakeTemporaryObject` | stdcall, 4 bytes | unimplemented |
| 243 | `ObOpenObjectByName` | stdcall, 16 bytes | implemented |
| 244 | `ObOpenObjectByPointer` | stdcall, 12 bytes | unimplemented |
| 245 | `ObpObjectHandleTable` | data | data at `80015d40` |
| 246 | `ObReferenceObjectByHandle` | stdcall, 12 bytes | implemented |
| 247 | `ObReferenceObjectByName` | stdcall, 20 bytes | stand-in (STUBS.md) |
| 248 | `ObReferenceObjectByPointer` | stdcall, 8 bytes | implemented |
| 249 | `ObSymbolicLinkObjectType` | data | data at `80015e40` |
| 250 | `ObfDereferenceObject` | fastcall, 4 bytes | implemented |
| 251 | `ObfReferenceObject` | fastcall, 4 bytes | implemented |
| 252 | `PhyGetLinkState` | stdcall, 4 bytes | implemented |
| 253 | `PhyInitialize` | stdcall, 8 bytes | implemented |
| 254 | `PsCreateSystemThread` | stdcall, 20 bytes | unimplemented |
| 255 | `PsCreateSystemThreadEx` | stdcall, 40 bytes | implemented |
| 256 | `PsQueryStatistics` | stdcall, 4 bytes | unimplemented |
| 257 | `PsSetCreateThreadNotifyRoutine` | stdcall, 4 bytes | unimplemented |
| 258 | `PsTerminateSystemThread` | stdcall, 4 bytes | implemented |
| 259 | `PsThreadObjectType` | data | data at `800160c0` |
| 260 | `RtlAnsiStringToUnicodeString` | stdcall, 12 bytes | implemented |
| 261 | `RtlAppendStringToString` | stdcall, 8 bytes | implemented |
| 262 | `RtlAppendUnicodeStringToString` | stdcall, 8 bytes | implemented |
| 263 | `RtlAppendUnicodeToString` | stdcall, 8 bytes | implemented |
| 264 | `RtlAssert` | stdcall, 16 bytes | stand-in (STUBS.md) |
| 265 | `RtlCaptureContext` | stdcall, 4 bytes | unimplemented |
| 266 | `RtlCaptureStackBackTrace` | stdcall, 16 bytes | unimplemented |
| 267 | `RtlCharToInteger` | stdcall, 12 bytes | implemented |
| 268 | `RtlCompareMemory` | stdcall, 12 bytes | implemented (guest code, kernel for the rest) |
| 269 | `RtlCompareMemoryUlong` | stdcall, 12 bytes | implemented |
| 270 | `RtlCompareString` | stdcall, 12 bytes | implemented |
| 271 | `RtlCompareUnicodeString` | stdcall, 12 bytes | implemented |
| 272 | `RtlCopyString` | stdcall, 8 bytes | implemented |
| 273 | `RtlCopyUnicodeString` | stdcall, 8 bytes | implemented |
| 274 | `RtlCreateUnicodeString` | stdcall, 8 bytes | implemented |
| 275 | `RtlDowncaseUnicodeChar` | stdcall, 4 bytes | implemented |
| 276 | `RtlDowncaseUnicodeString` | stdcall, 12 bytes | implemented |
| 277 | `RtlEnterCriticalSection` | stdcall, 4 bytes | implemented (guest code, kernel for the rest) |
| 278 | `RtlEnterCriticalSectionAndRegion` | stdcall, 4 bytes | stand-in (STUBS.md) |
| 279 | `RtlEqualString` | stdcall, 12 bytes | implemented |
| 280 | `RtlEqualUnicodeString` | stdcall, 12 bytes | implemented |
| 281 | `RtlExtendedIntegerMultiply` | stdcall, 12 bytes | implemented |
| 282 | `RtlExtendedLargeIntegerDivide` | stdcall, 16 bytes | stand-in (STUBS.md) |
| 283 | `RtlExtendedMagicDivide` | stdcall, 20 bytes | implemented |
| 284 | `RtlFillMemory` | stdcall, 12 bytes | implemented (guest code, kernel for the rest) |
| 285 | `RtlFillMemoryUlong` | stdcall, 12 bytes | implemented (guest code, kernel for the rest) |
| 286 | `RtlFreeAnsiString` | stdcall, 4 bytes | implemented |
| 287 | `RtlFreeUnicodeString` | stdcall, 4 bytes | implemented |
| 288 | `RtlGetCallersAddress` | stdcall, 8 bytes | unimplemented |
| 289 | `RtlInitAnsiString` | stdcall, 8 bytes | implemented |
| 290 | `RtlInitUnicodeString` | stdcall, 8 bytes | implemented |
| 291 | `RtlInitializeCriticalSection` | stdcall, 4 bytes | implemented |
| 292 | `RtlIntegerToChar` | stdcall, 16 bytes | implemented |
| 293 | `RtlIntegerToUnicodeString` | stdcall, 12 bytes | implemented |
| 294 | `RtlLeaveCriticalSection` | stdcall, 4 bytes | implemented (guest code, kernel for the rest) |
| 295 | `RtlLeaveCriticalSectionAndRegion` | stdcall, 4 bytes | stand-in (STUBS.md) |
| 296 | `RtlLowerChar` | stdcall, 4 bytes | implemented (guest code, kernel for the rest) |
| 297 | `RtlMapGenericMask` | stdcall, 8 bytes | implemented |
| 298 | `RtlMoveMemory` | stdcall, 12 bytes | implemented (guest code, kernel for the rest) |
| 299 | `RtlMultiByteToUnicodeN` | stdcall, 20 bytes | stand-in (STUBS.md) |
| 300 | `RtlMultiByteToUnicodeSize` | stdcall, 12 bytes | implemented |
| 301 | `RtlNtStatusToDosError` | stdcall, 4 bytes | implemented |
| 302 | `RtlRaiseException` | stdcall, 4 bytes | implemented |
| 303 | `RtlRaiseStatus` | stdcall, 4 bytes | unimplemented |
| 304 | `RtlTimeFieldsToTime` | stdcall, 8 bytes | implemented |
| 305 | `RtlTimeToTimeFields` | stdcall, 8 bytes | implemented |
| 306 | `RtlTryEnterCriticalSection` | stdcall, 4 bytes | implemented |
| 307 | `RtlUlongByteSwap` | fastcall, 4 bytes | implemented |
| 308 | `RtlUnicodeStringToAnsiString` | stdcall, 12 bytes | implemented |
| 309 | `RtlUnicodeStringToInteger` | stdcall, 12 bytes | implemented |
| 310 | `RtlUnicodeToMultiByteN` | stdcall, 20 bytes | stand-in (STUBS.md) |
| 311 | `RtlUnicodeToMultiByteSize` | stdcall, 12 bytes | implemented |
| 312 | `RtlUnwind` | stdcall, 16 bytes | implemented |
| 313 | `RtlUpcaseUnicodeChar` | stdcall, 4 bytes | implemented |
| 314 | `RtlUpcaseUnicodeString` | stdcall, 12 bytes | implemented |
| 315 | `RtlUpcaseUnicodeToMultiByteN` | stdcall, 20 bytes | implemented |
| 316 | `RtlUpperChar` | stdcall, 4 bytes | implemented (guest code, kernel for the rest) |
| 317 | `RtlUpperString` | stdcall, 8 bytes | implemented |
| 318 | `RtlUshortByteSwap` | fastcall, 4 bytes | implemented |
| 319 | `RtlWalkFrameChain` | stdcall, 12 bytes | unimplemented |
| 320 | `RtlZeroMemory` | stdcall, 8 bytes | implemented (guest code, kernel for the rest) |
| 321 | `XboxEEPROMKey` | data | data at `80017040` |
| 322 | `XboxHardwareInfo` | data | data at `80017080` |
| 323 | `XboxHDKey` | data | data at `800170c0` |
| 324 | `XboxKrnlVersion` | data | data at `80017100` |
| 325 | `XboxSignatureKey` | data | data at `80017140` |
| 326 | `XeImageFileName` | data | data at `80017180` |
| 327 | `XeLoadSection` | stdcall, 4 bytes | implemented |
| 328 | `XeUnloadSection` | stdcall, 4 bytes | implemented |
| 329 | `READ_PORT_BUFFER_UCHAR` | stdcall, 12 bytes | unimplemented |
| 330 | `READ_PORT_BUFFER_USHORT` | stdcall, 12 bytes | unimplemented |
| 331 | `READ_PORT_BUFFER_ULONG` | stdcall, 12 bytes | unimplemented |
| 332 | `WRITE_PORT_BUFFER_UCHAR` | stdcall, 12 bytes | unimplemented |
| 333 | `WRITE_PORT_BUFFER_USHORT` | stdcall, 12 bytes | unimplemented |
| 334 | `WRITE_PORT_BUFFER_ULONG` | stdcall, 12 bytes | unimplemented |
| 335 | `XcSHAInit` | stdcall, 4 bytes | implemented |
| 336 | `XcSHAUpdate` | stdcall, 12 bytes | implemented |
| 337 | `XcSHAFinal` | stdcall, 8 bytes | implemented |
| 338 | `XcRC4Key` | stdcall, 12 bytes | implemented |
| 339 | `XcRC4Crypt` | stdcall, 12 bytes | implemented |
| 340 | `XcHMAC` | stdcall, 28 bytes | implemented |
| 341 | `XcPKEncPublic` | stdcall, 12 bytes | implemented |
| 342 | `XcPKDecPrivate` | stdcall, 12 bytes | unimplemented |
| 343 | `XcPKGetKeyLen` | stdcall, 4 bytes | implemented |
| 344 | `XcVerifyPKCS1Signature` | stdcall, 12 bytes | stand-in (STUBS.md) |
| 345 | `XcModExp` | stdcall, 20 bytes | implemented |
| 346 | `XcDESKeyParity` | stdcall, 8 bytes | implemented |
| 347 | `XcKeyTable` | stdcall, 12 bytes | implemented |
| 348 | `XcBlockCrypt` | stdcall, 20 bytes | implemented |
| 349 | `XcBlockCryptCBC` | stdcall, 28 bytes | implemented |
| 350 | `XcCryptService` | stdcall, 8 bytes | stand-in (STUBS.md) |
| 351 | `XcUpdateCrypto` | stdcall, 8 bytes | stand-in (STUBS.md) |
| 352 | `RtlRip` | stdcall, 12 bytes | stand-in (STUBS.md) |
| 353 | `XboxLANKey` | data | data at `80017840` |
| 354 | `XboxAlternateSignatureKeys` | data | data at `8001a000` |
| 355 | `XePublicKeyData` | data | data at `8001a100` |
| 356 | `HalBootSMCVideoMode` | data | data at `80017900` |
| 357 | `IdexChannelObject` | data | data at `80017940` |
| 358 | `HalIsResetOrShutdownPending` | stdcall, 0 bytes | stand-in (STUBS.md) |
| 359 | `IoMarkIrpMustComplete` | stdcall, 4 bytes | unimplemented |
| 360 | `HalInitiateShutdown` | stdcall, 0 bytes | implemented |
| 361 | `RtlSnprintf` | cdecl | unimplemented |
| 362 | `RtlSprintf` | cdecl | unimplemented |
| 363 | `RtlVsnprintf` | cdecl | unimplemented |
| 364 | `RtlVsprintf` | cdecl | unimplemented |
| 365 | `HalEnableSecureTrayEject` | stdcall, 0 bytes | stand-in (STUBS.md) |
| 366 | `HalWriteSMCScratchRegister` | stdcall, 4 bytes | unimplemented |
| 374 | `MmDbgAllocateMemory` | stdcall, 8 bytes | unimplemented |
| 375 | `MmDbgFreeMemory` | stdcall, 8 bytes | unimplemented |
| 376 | `MmDbgQueryAvailablePages` | stdcall, 0 bytes | unimplemented |
| 377 | `MmDbgReleaseAddress` | stdcall, 8 bytes | unimplemented |
| 378 | `MmDbgWriteCheck` | stdcall, 8 bytes | unimplemented |
<!-- end of the export table -->
