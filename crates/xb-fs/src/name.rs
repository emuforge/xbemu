//! Names: wildcard matching, FATX's rules, and how a console name is kept on
//! a host file system.

/// Longest file or directory name FATX holds, also what
/// `FileFsAttributeInformation` reports for every volume.
pub const FATX_MAX_NAME: usize = 42;

/// Matches `name` against a directory enumeration's mask, ignoring ASCII
/// case: `*` is any run of characters (none too), `?` exactly one.
///
/// This is the whole of it: no DOS rules, so `*.*` needs a dot in the name.
/// Riddick deletes its save's fillers with `_filler???`, which must not match
/// anything else.
#[must_use]
pub fn wildcard_match(mask: &str, name: &str) -> bool {
    let (m, n): (Vec<char>, Vec<char>) = (mask.chars().collect(), name.chars().collect());
    // `star`: the last `*` seen and the name position it was tried at.
    let (mut mi, mut ni, mut star) = (0, 0, None);
    while ni < n.len() {
        if mi < m.len() && m[mi] == '*' {
            star = Some((mi, ni));
            mi += 1;
        } else if mi < m.len() && (m[mi] == '?' || m[mi].eq_ignore_ascii_case(&n[ni])) {
            mi += 1;
            ni += 1;
        } else if let Some((sm, sn)) = star {
            star = Some((sm, sn + 1));
            mi = sm + 1;
            ni = sn + 1;
        } else {
            return false;
        }
    }
    m[mi..].iter().all(|&c| c == '*')
}

/// True if FATX takes `name` for a file or directory: 1 to 42 characters,
/// each printable ASCII other than `" * + , / : ; < = > ? \ |`, and not `.`
/// or `..`.
///
/// The reference does not check names (its host folders take what the host
/// takes); the rule is the console's FATX driver's as documented by the
/// homebrew community, offered for callers that want it and for
/// [`HostDisk::set_enforce_fatx_names`](crate::HostDisk::set_enforce_fatx_names).
/// A name may end in spaces or dots: RalliSport Challenge makes a folder
/// named `" "` for each profile.
#[must_use]
pub fn fatx_name_valid(name: &str) -> bool {
    let n = name.chars().count();
    (1..=FATX_MAX_NAME).contains(&n)
        && name != "."
        && name != ".."
        && name.chars().all(|c| {
            matches!(c, ' '..='~')
                && !matches!(
                    c,
                    '"' | '*' | '+' | ',' | '/' | ':' | ';' | '<' | '=' | '>' | '?' | '\\' | '|'
                )
        })
}

/// True if `name` is one of Windows' device names (`CON`, `NUL`, `COM1` and
/// the like), with or without an extension.
fn dos_device(name: &str) -> bool {
    const DEVICES: [&str; 6] = ["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"];
    let stem = name.split('.').next().unwrap_or("");
    if DEVICES.iter().any(|d| d.eq_ignore_ascii_case(stem)) {
        return true;
    }
    let b = stem.as_bytes();
    b.len() == 4
        && (b[..3].eq_ignore_ascii_case(b"COM") || b[..3].eq_ignore_ascii_case(b"LPT"))
        && b[3].is_ascii_digit()
}

/// The host file name for a console name.
///
/// FATX takes names a Windows host cannot hold: one ending in spaces or dots
/// (Windows drops them, and the name becomes another) and the device names.
/// Those characters are kept as `=XX` (hex), and `=` itself, which FATX does
/// not take, as `=3D`, on every host alike so a hard disk folder moves
/// between them. RalliSport Challenge's `" "` folder is `=20` on the host.
#[must_use]
pub fn name_to_host(guest: &str) -> String {
    if guest == "." || guest == ".." {
        return guest.to_owned();
    }
    let chars: Vec<char> = guest.chars().collect();
    let mut keep = chars.len();
    while keep > 0 && matches!(chars[keep - 1], ' ' | '.') {
        keep -= 1;
    }
    let device = dos_device(guest);
    let mut host = String::with_capacity(guest.len());
    for (i, &c) in chars.iter().enumerate() {
        if (c == '=' || i >= keep || (i == 0 && device)) && u32::from(c) <= 0xFF {
            host.push_str(&format!("={:02X}", u32::from(c)));
        } else {
            host.push(c);
        }
    }
    host
}

/// The console name of a host file name: [`name_to_host`] undone. A `=` not
/// followed by two hex digits stands for itself.
#[must_use]
pub fn name_from_host(host: &str) -> String {
    let chars: Vec<char> = host.chars().collect();
    let mut guest = String::with_capacity(host.len());
    let mut i = 0;
    while i < chars.len() {
        let hex = |at: usize| chars.get(at).and_then(|c| c.to_digit(16));
        if chars[i] == '='
            && let (Some(hi), Some(lo)) = (hex(i + 1), hex(i + 2))
        {
            guest.push(char::from((hi << 4 | lo) as u8));
            i += 3;
        } else {
            guest.push(chars[i]);
            i += 1;
        }
    }
    guest
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reference's recursive matcher, as the oracle for the iterative one.
    fn reference(pat: &[u8], s: &[u8]) -> bool {
        match pat.first() {
            None => s.is_empty(),
            Some(b'*') => (0..=s.len()).any(|skip| reference(&pat[1..], &s[skip..])),
            Some(&p) => {
                !s.is_empty()
                    && (p == b'?' || p.eq_ignore_ascii_case(&s[0]))
                    && reference(&pat[1..], &s[1..])
            }
        }
    }

    #[test]
    fn wildcards_agree_with_the_reference_matcher() {
        let masks = [
            "*",
            "",
            "?",
            "*.*",
            "*.sav",
            "_filler???",
            "a*b*c",
            "**",
            "*?",
            "?*?",
            "blam.*",
            "STAGE398.lz",
            "a?c",
            "*a",
            "a*",
        ];
        let names = [
            "",
            "a",
            "abc",
            "blam.sav",
            "BLAM.SAV",
            "_filler001",
            "_filler0012",
            "SaveMeta.xbx",
            "stage398.LZ",
            "noext",
            "a.b.c",
            "aXbYc",
            "ba",
        ];
        for mask in masks {
            for name in names {
                assert_eq!(
                    wildcard_match(mask, name),
                    reference(mask.as_bytes(), name.as_bytes()),
                    "mask {mask:?} name {name:?}"
                );
            }
        }
    }

    #[test]
    fn star_dot_star_needs_a_dot() {
        assert!(wildcard_match("*.*", "a.b"));
        assert!(!wildcard_match("*.*", "noext"));
        assert!(wildcard_match("*", "noext"));
    }

    #[test]
    fn fatx_names_are_checked_for_length_and_characters() {
        assert!(fatx_name_valid("default.xbe"));
        assert!(fatx_name_valid(" "));
        assert!(fatx_name_valid("save game."));
        assert!(fatx_name_valid(&"x".repeat(42)));
        assert!(!fatx_name_valid(&"x".repeat(43)));
        assert!(!fatx_name_valid(""));
        assert!(!fatx_name_valid("."));
        assert!(!fatx_name_valid(".."));
        for bad in ["a*b", "a?b", "a:b", "a<b", "a>b", "a|b", "a\"b", "a+b", "a,b", "a;b", "a=b"] {
            assert!(!fatx_name_valid(bad), "{bad}");
        }
        assert!(!fatx_name_valid("a\u{7f}"));
        assert!(!fatx_name_valid("caf\u{e9}"));
        assert!(!fatx_name_valid("tab\there"));
    }

    #[test]
    fn host_names_escape_what_windows_cannot_hold() {
        assert_eq!(name_to_host(" "), "=20");
        assert_eq!(name_to_host("save."), "save=2E");
        assert_eq!(name_to_host("a . ."), "a=20=2E=20=2E");
        assert_eq!(name_to_host("a=b"), "a=3Db");
        assert_eq!(name_to_host("CON"), "=43ON");
        assert_eq!(name_to_host("nul.txt"), "=6Eul.txt");
        assert_eq!(name_to_host("COM1"), "=43OM1");
        assert_eq!(name_to_host("COMX"), "COMX");
        assert_eq!(name_to_host("console"), "console");
        assert_eq!(name_to_host("plain.txt"), "plain.txt");
        assert_eq!(name_to_host(".."), "..");
    }

    #[test]
    fn host_names_round_trip() {
        for name in
            [" ", "...", "a=b", "CON", "aux.dat", "LPT9", "x. ", "plain", "=41", "caf\u{e9}", "="]
        {
            assert_eq!(name_from_host(&name_to_host(name)), name, "{name:?}");
        }
        assert_eq!(name_from_host("a=zz"), "a=zz");
        assert_eq!(name_from_host("tail="), "tail=");
    }
}
