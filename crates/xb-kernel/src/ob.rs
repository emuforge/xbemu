//! Objects and handles.
//!
//! Objects live in guest memory as on the Xbox: an OBJECT_HEADER
//! (PointerCount, HandleCount, Type, Flags) followed by the body; handles
//! (multiples of 4) map to the body. Type is the VA of the kernel's type
//! object (PsThreadObjectType, ExEventObjectType, ...).

use crate::gm::KResult;
use crate::kernel::{
    Args, K_OB_NAMES, KERNEL_VA, Kernel, OB_TYPES, ObType, STATUS_INSUFFICIENT_RESOURCES,
    STATUS_INVALID_HANDLE, STATUS_OBJECT_TYPE_MISMATCH, STATUS_SUCCESS,
};
use crate::platform::Platform;

const OB_HEADER: u32 = 0x10;
/// The Xbox kernel's handle table grows as it fills. Super Monkey Ball
/// Deluxe opens its disc's ~4100 files as it boots, most of them twice, and
/// keeps them open.
const MAX_HANDLES: usize = 16384;

const OB_NAMES: u32 = KERNEL_VA + K_OB_NAMES;
const OBJ_OPENIF: u32 = 0x80;
const STATUS_OBJECT_NAME_EXISTS: u32 = 0x4000_0000;
const STATUS_OBJECT_NAME_NOT_FOUND: u32 = 0xC000_0034;
const STATUS_OBJECT_NAME_COLLISION: u32 = 0xC000_0035;

/// The handle table and what freeing objects needs.
pub(crate) struct State {
    /// Per handle index: the object body's VA (0: free) and its type.
    handles: Vec<(u32, ObType)>,
    /// Handles are taken round the table from the one after the last given.
    next: usize,
    /// Body size of each freeable type's objects (`u32::MAX`: sizes mixed,
    /// never freed).
    size: [u32; OB_TYPES],
    open_told: u32,
}

impl Default for State {
    fn default() -> Self {
        Self {
            handles: vec![(0, ObType::Thread); MAX_HANDLES],
            next: 1,
            size: [0; OB_TYPES],
            open_told: 0,
        }
    }
}

/// File and symbolic-link objects, all one size, whose host state goes with
/// them, and timers. Other types stay: the scheduler and waits keep their
/// addresses.
fn freeable(ty: ObType) -> bool {
    matches!(ty, ObType::File | ObType::Symlink | ObType::Timer)
}

impl Kernel {
    fn ob_type_of(&self, object: u32) -> Option<ObType> {
        let tva = self.gm.r32(object.wrapping_sub(OB_HEADER) + 8).ok()?;
        ObType::ALL.into_iter().find(|t| self.type_va[*t as usize] == tva)
    }

    /// Frees an object no handle or reference holds any more (PointerCount 1
    /// is the one it was created with).
    fn ob_release(&mut self, object: u32, ty: Option<ObType>) {
        let hdr = object - OB_HEADER;
        let Some(ty) = ty.filter(|t| freeable(*t)) else { return };
        let size = self.ob.size[ty as usize];
        if size == 0
            || size == u32::MAX
            || self.gm.r32(hdr + 4).unwrap_or(1) != 0
            || self.gm.r32(hdr).unwrap_or(2) > 1
        {
            return;
        }
        if ty == ObType::Timer {
            // A timer goes as the console's does with its last handle (its
            // delete routine cancels it): out of the timer queue first, and
            // kept while a thread still waits on it. Kept for good, the
            // timers of a title that makes one per wait filled RAM (The
            // Nightmare Before Christmas: Oogie's Revenge's 171,000 by its
            // title movie).
            if self.object_waited(object) {
                return;
            }
            self.timer_forget(object);
        }
        self.gm.kw32(hdr + 8, 0); // a stale lookup of it fails the type check
        self.pool.ob = self.pool.ob.saturating_sub(OB_HEADER + size);
        self.pool_free(hdr, OB_HEADER + size);
    }

    /// ObfDereferenceObject: an object no one holds any more is freed.
    pub(crate) fn ob_dereference(&mut self, object: u32) {
        let hdr = object.wrapping_sub(OB_HEADER);
        let Ok(count) = self.gm.r32(hdr) else { return };
        if count != 0 {
            let _ = self.gm.w32(hdr, count - 1);
        }
        self.ob_release(object, self.ob_type_of(object));
    }

    /// Allocates an object (OBJECT_HEADER + zeroed body) and returns the
    /// body's VA, or 0.
    pub(crate) fn ob_create(&mut self, ty: ObType, body_size: u32) -> u32 {
        let s = &mut self.ob.size[ty as usize];
        if freeable(ty) && *s != u32::MAX {
            if *s != 0 && *s != body_size {
                log::warn!(target: "kernel", "ob: type {ty:?} objects of {} and {body_size} bytes: not freed", *s);
                *s = u32::MAX; // for good: the sizes are mixed
            } else {
                *s = body_size;
            }
        }
        let h = self.pool_alloc(OB_HEADER + body_size);
        if h == 0 {
            return 0;
        }
        self.pool.ob += OB_HEADER + body_size;
        self.gm.kw32(h, 1); // PointerCount
        self.gm.kw32(h + 8, self.type_va[ty as usize]); // Type
        h + OB_HEADER
    }

    /// A new handle to `object`, or 0. Handles are taken round the table
    /// from the one after the last given, so a handle just closed isn't the
    /// next one opened (Test Drive closes an archive's file handle a second
    /// time as it frees its slots; that number was its loader thread's by
    /// then).
    pub(crate) fn ob_handle(&mut self, object: u32, ty: ObType) -> u32 {
        for _ in 1..MAX_HANDLES {
            let i = if self.ob.next >= MAX_HANDLES { 1 } else { self.ob.next };
            self.ob.next = i + 1;
            if self.ob.handles[i].0 == 0 {
                self.ob.handles[i] = (object, ty);
                let hdr = object - OB_HEADER;
                let n = self.gm.r32(hdr + 4).unwrap_or(0);
                let _ = self.gm.w32(hdr + 4, n + 1); // HandleCount
                return i as u32 * 4;
            }
        }
        log::error!(target: "kernel", "ob: out of handles");
        0
    }

    /// The object a handle names, and its type.
    pub(crate) fn ob_lookup(&self, handle: u32) -> Option<(u32, ObType)> {
        let i = (handle / 4) as usize;
        if handle & 3 != 0 || i == 0 || i >= MAX_HANDLES || self.ob.handles[i].0 == 0 {
            return None;
        }
        Some(self.ob.handles[i])
    }

    /// Closes a handle; a file's host state goes with its last handle.
    pub(crate) fn ob_close(&mut self, handle: u32) -> bool {
        let Some((object, ty)) = self.ob_lookup(handle) else { return false };
        let hdr = object - OB_HEADER;
        let count = self.gm.r32(hdr + 4).unwrap_or(1).wrapping_sub(1);
        let _ = self.gm.w32(hdr + 4, count);
        self.ob.handles[(handle / 4) as usize].0 = 0;
        if count == 0 {
            self.ob_unname(object); // a named object goes from the namespace with its last handle
            if matches!(ty, ObType::File | ObType::Symlink) {
                self.io_close(object);
            }
            self.ob_release(object, Some(ty));
        }
        true
    }

    // ------------------------------------------------------- named objects
    //
    // Events, semaphores and mutants created with a name (the named objects
    // XAPI's CreateEvent makes and OpenEvent opens by ObOpenObjectByName),
    // in a list in the kernel's memory: the first node at OB_NAMES, a node
    // {next, object, type, the name's length, the name}.

    /// An OBJECT_ATTRIBUTES' name ("" none).
    pub(crate) fn ob_attr_name(&self, attrs: u32) -> KResult<Vec<u8>> {
        if attrs == 0 {
            return Ok(Vec::new());
        }
        let s = self.gm.r32(attrs + 4)?;
        if s == 0 {
            return Ok(Vec::new());
        }
        let mut name = self.gm.ansi_string(s)?;
        name.truncate(255);
        if let Some(n) = name.iter().position(|&c| c == 0) {
            name.truncate(n);
        }
        Ok(name)
    }

    fn ob_named(&self, name: &[u8]) -> Option<(u32, ObType)> {
        let mut node = self.gm.kr32(OB_NAMES);
        for _ in 0..4096 {
            if node == 0 {
                break;
            }
            let len = self.gm.kr32(node + 12) as usize;
            if len == name.len() {
                let have = self.gm.read_vec(node + 16, len as u32).unwrap_or_default();
                if have.eq_ignore_ascii_case(name) {
                    // the list is guest memory: a type that is none (the
                    // title's stray write) is no object
                    let t = self.gm.kr32(node + 8) as usize;
                    return ObType::ALL.get(t).map(|ty| (self.gm.kr32(node + 4), *ty));
                }
            }
            node = self.gm.kr32(node);
        }
        None
    }

    /// A new object's name kept.
    pub(crate) fn ob_name(&mut self, object: u32, ty: ObType, name: &[u8]) {
        let node = self.pool_alloc(16 + name.len() as u32 + 1);
        if node == 0 {
            return;
        }
        self.gm.kw32(node, self.gm.kr32(OB_NAMES));
        self.gm.kw32(node + 4, object);
        self.gm.kw32(node + 8, ty as u32);
        self.gm.kw32(node + 12, name.len() as u32);
        self.gm.kwrite(node + 16, name);
        self.gm.kw32(OB_NAMES, node);
    }

    /// The object's name, if it has one, out of the list: as on the Xbox, a
    /// named object that isn't permanent leaves the namespace when its last
    /// handle closes, and opening the name fails from then on (Chicago
    /// Enforcer's loading screen opens its events until they're gone).
    pub(crate) fn ob_unname(&mut self, object: u32) {
        let mut prev = OB_NAMES;
        let mut node = self.gm.kr32(OB_NAMES);
        for _ in 0..4096 {
            if node == 0 {
                break;
            }
            let next = self.gm.kr32(node);
            if self.gm.kr32(node + 4) == object {
                self.gm.kw32(prev, next);
                let len = self.gm.kr32(node + 12);
                self.pool_free(node, 16 + len + 1);
            } else {
                prev = node;
            }
            node = next;
        }
    }

    /// A create of a name that exists: the status (OBJ_OPENIF: a handle to
    /// it, STATUS_OBJECT_NAME_EXISTS); `None` when the name is free.
    pub(crate) fn ob_create_named(
        &mut self,
        attrs: u32,
        name: &[u8],
        ty: ObType,
        handle_out: u32,
    ) -> KResult<Option<u32>> {
        let Some((obj, have)) = self.ob_named(name) else { return Ok(None) };
        if have != ty {
            return Ok(Some(STATUS_OBJECT_TYPE_MISMATCH));
        }
        if self.gm.r32(attrs + 8)? & OBJ_OPENIF == 0 {
            return Ok(Some(STATUS_OBJECT_NAME_COLLISION));
        }
        let h = self.ob_handle(obj, ty);
        self.gm.w32(handle_out, h)?;
        Ok(Some(if h != 0 { STATUS_OBJECT_NAME_EXISTS } else { STATUS_INSUFFICIENT_RESOURCES }))
    }
}

/// NtClose(Handle): a file or symbolic link goes with its last handle and
/// reference.
fn nt_close(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    Ok(u64::from(if k.ob_close(a[0]) { STATUS_SUCCESS } else { STATUS_INVALID_HANDLE }))
}

/// NtDuplicateObject(SourceHandle, TargetHandle, Options): DUPLICATE_CLOSE_SOURCE is 1.
/// NtCurrentThread (-2) duplicates to a real handle of the running thread.
fn nt_duplicate_object(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let found = if a[0] == 0xFFFF_FFFE {
        k.current.map(|t| (k.threads[t].ethread, ObType::Thread))
    } else {
        k.ob_lookup(a[0])
    };
    let Some((obj, ty)) = found else {
        return Ok(u64::from(STATUS_INVALID_HANDLE));
    };
    let h = k.ob_handle(obj, ty);
    if h == 0 {
        return Ok(u64::from(STATUS_INSUFFICIENT_RESOURCES));
    }
    k.gm.w32(a[1], h)?;
    if a[2] & 1 != 0 && a[0] != 0xFFFF_FFFE {
        k.ob_close(a[0]);
    }
    Ok(u64::from(STATUS_SUCCESS))
}

/// ObOpenObjectByName(ObjectAttributes, ObjectType, ParseContext, Handle):
/// a named event, semaphore or mutant (OpenEvent, OpenSemaphore, OpenMutex;
/// Chicago Enforcer's loading screen opens its events by name).
fn ob_open_object_by_name(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let name = k.ob_attr_name(a[0])?;
    let found = if name.is_empty() { None } else { k.ob_named(&name) };
    let Some((obj, ty)) = found else {
        if k.ob.open_told < 50 {
            k.ob.open_told += 1;
            log::info!(target: "kernel", "ob: open {}: no such object",
                if name.is_empty() { "(no name)".into() } else { String::from_utf8_lossy(&name) });
        }
        return Ok(u64::from(STATUS_OBJECT_NAME_NOT_FOUND));
    };
    if a[1] != 0 && k.type_va[ty as usize] != a[1] {
        return Ok(u64::from(STATUS_OBJECT_TYPE_MISMATCH));
    }
    let h = k.ob_handle(obj, ty);
    if h == 0 {
        return Ok(u64::from(STATUS_INSUFFICIENT_RESOURCES));
    }
    k.gm.w32(a[3], h)?;
    Ok(u64::from(STATUS_SUCCESS))
}

pub(crate) fn register(k: &mut Kernel) {
    k.imp("NtClose", nt_close);
    k.imp("NtDuplicateObject", nt_duplicate_object);
    k.imp("ObOpenObjectByName", ob_open_object_by_name);
}

xb_types::state_fields!(State { handles, next, size });
