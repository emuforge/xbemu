//! Export lookup: every `xboxkrnl.exe` export by ordinal and by name.

#[path = "exports_table.rs"]
mod table;

/// How an export is called.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Abi {
    /// Arguments on the stack, popped by the callee.
    Stdcall,
    /// The first two arguments in ECX and EDX, the rest on the stack, popped
    /// by the callee.
    Fastcall,
    /// Arguments on the stack, popped by the caller (the `printf` family).
    Cdecl,
    /// Not a function: a variable in kernel memory.
    Data,
}

/// One entry of the kernel's export table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Export {
    /// The ordinal a title imports it by.
    pub ordinal: u16,
    /// Its name in `xboxkrnl.exe.def`.
    pub name: &'static str,
    /// Calling convention, or [`Abi::Data`].
    pub abi: Abi,
    /// Bytes of arguments (fastcall: the register arguments included).
    pub arg_bytes: u8,
}

/// One past the highest ordinal the kernel's tables hold.
pub const ORDINAL_MAX: usize = 512;

/// Every export, in ordinal order.
#[must_use]
pub fn all() -> &'static [Export] {
    table::EXPORTS
}

/// The export with this ordinal.
#[must_use]
pub fn by_ordinal(ordinal: u32) -> Option<&'static Export> {
    let t = table::EXPORTS;
    t.binary_search_by_key(&ordinal, |e| u32::from(e.ordinal)).ok().map(|i| &t[i])
}

/// The export with this name.
#[must_use]
pub fn by_name(name: &str) -> Option<&'static Export> {
    table::EXPORTS.iter().find(|e| e.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_is_sorted_and_within_the_ordinal_space() {
        let t = all();
        assert_eq!(t.len(), 371);
        assert!(t.windows(2).all(|w| w[0].ordinal < w[1].ordinal));
        assert!(t.iter().all(|e| usize::from(e.ordinal) < ORDINAL_MAX));
    }

    #[test]
    fn lookups_by_ordinal_and_name_agree() {
        for e in all() {
            assert_eq!(by_ordinal(u32::from(e.ordinal)), Some(e));
            assert_eq!(by_name(e.name).map(|x| x.ordinal), Some(e.ordinal));
        }
        assert_eq!(by_ordinal(0), None);
        assert_eq!(by_name("PsCreateSystemThreadEx").map(|e| e.ordinal), Some(255));
    }
}
