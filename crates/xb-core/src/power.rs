//! The host's power profile while a title runs.
//!
//! A title's threads sleep and wake many times a frame, and a host that
//! saves power sees an idle machine: its clocks stay low and frames come
//! late. Where the host has a power profiles service the core asks it to
//! hold the performance profile ([`Power::set`]), as a game would; the
//! service gives the hold back when the core lets go of it or the process
//! ends. Hosts without one are left as they are.
//!
//! Linux: the `HoldProfile` call of power-profiles-daemon (and of the
//! services that answer for it) on the system bus. The bus is spoken to
//! directly, a socket and one call, so no front end needs a D-Bus library.

use std::sync::{Arc, Mutex, PoisonError};

/// What is wanted and what is had.
#[derive(Default)]
struct State {
    /// The setting: the performance profile is wanted.
    wanted: bool,
    /// A request is on its way.
    asking: bool,
    /// The hold; letting go of it is dropping it.
    hold: Option<os::Hold>,
}

/// The core's hold on the host's performance profile.
#[derive(Default)]
pub(crate) struct Power {
    state: Arc<Mutex<State>>,
}

impl Power {
    /// Asks for the performance profile (`true`) or lets go of it.
    ///
    /// The request is made on a thread of its own: the service may ask the
    /// host's policy first, and a boot does not wait for that. A request
    /// answered after the setting was turned off is let go at once.
    pub(crate) fn set(&self, wanted: bool) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.wanted = wanted;
        if !wanted {
            if state.hold.take().is_some() {
                log::info!(target: "core", "power: the host's performance profile is let go");
            }
            return;
        }
        if state.hold.is_some() || state.asking {
            return;
        }
        state.asking = true;
        drop(state);
        let shared = Arc::clone(&self.state);
        let ask = move || {
            let answer = os::hold();
            let mut state = shared.lock().unwrap_or_else(PoisonError::into_inner);
            state.asking = false;
            match answer {
                Ok(hold) if state.wanted => {
                    log::info!(target: "core", "power: the host holds its performance profile while the title runs ({})", hold.service());
                    state.hold = Some(hold);
                }
                Ok(_) => {}
                Err(why) => {
                    log::info!(target: "core", "power: the host's power profile is left as it is: {why}");
                }
            }
        };
        if let Err(e) = std::thread::Builder::new().name("xb-power".into()).spawn(ask) {
            self.state.lock().unwrap_or_else(PoisonError::into_inner).asking = false;
            log::warn!(target: "core", "power: no thread to ask the host with: {e}");
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod os {
    //! Hosts with no power profiles service the core knows how to ask.

    /// Never made on these hosts.
    pub(super) struct Hold;

    impl Hold {
        pub(super) fn service(&self) -> &'static str {
            ""
        }
    }

    pub(super) fn hold() -> Result<Hold, String> {
        Err("this host has no power profiles service the core asks".into())
    }
}

#[cfg(target_os = "linux")]
mod os {
    //! `HoldProfile("performance", reason, application)` on the system bus.
    //!
    //! The service keeps a hold for as long as its caller stays on the bus,
    //! so the hold is the connection: [`Hold`] keeps the socket and dropping
    //! it gives the profile back. Messages are little-endian, as this side
    //! writes them and as a bus on the same machine answers.

    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    /// The services that take the call, the current name first: (bus name
    /// and interface, object path).
    const SERVICES: [(&str, &str); 2] = [
        ("org.freedesktop.UPower.PowerProfiles", "/org/freedesktop/UPower/PowerProfiles"),
        ("net.hadess.PowerProfiles", "/net/hadess/PowerProfiles"),
    ];
    /// How long a write or a read of the bus may take.
    const TIMEOUT: Duration = Duration::from_secs(3);
    /// The largest message read; the answers wanted are under a kilobyte.
    const MAX_MESSAGE: usize = 1 << 16;
    /// Messages read while waiting for one answer (the bus sends a signal
    /// or two of its own first).
    const MAX_UNRELATED: usize = 32;

    const METHOD_RETURN: u8 = 2;
    const ERROR: u8 = 3;
    const FIELD_PATH: u8 = 1;
    const FIELD_INTERFACE: u8 = 2;
    const FIELD_MEMBER: u8 = 3;
    const FIELD_REPLY_SERIAL: u8 = 5;
    const FIELD_DESTINATION: u8 = 6;
    const FIELD_SIGNATURE: u8 = 8;

    /// A hold on the performance profile: the bus connection it was asked
    /// over.
    pub(super) struct Hold {
        _bus: UnixStream,
        service: &'static str,
    }

    impl Hold {
        /// The service that gave it.
        pub(super) fn service(&self) -> &'static str {
            self.service
        }
    }

    /// The system bus's socket: `DBUS_SYSTEM_BUS_ADDRESS` when it names a
    /// path, else the standard one.
    fn bus_path() -> PathBuf {
        std::env::var("DBUS_SYSTEM_BUS_ADDRESS")
            .ok()
            .and_then(|a| {
                a.strip_prefix("unix:path=").map(|p| p.split(',').next().unwrap_or(p).into())
            })
            .unwrap_or_else(|| "/run/dbus/system_bus_socket".into())
    }

    /// Asks the host's power profiles service for the performance profile.
    pub(super) fn hold() -> Result<Hold, String> {
        hold_on(&bus_path())
    }

    /// [`hold`] on the bus at `path`.
    pub(super) fn hold_on(path: &Path) -> Result<Hold, String> {
        let mut bus = UnixStream::connect(path)
            .map_err(|e| format!("no system bus at {}: {e}", path.display()))?;
        let io = |e: std::io::Error| format!("the system bus: {e}");
        bus.set_read_timeout(Some(TIMEOUT)).map_err(io)?;
        bus.set_write_timeout(Some(TIMEOUT)).map_err(io)?;
        authenticate(&mut bus)?;
        let hello = Call {
            destination: "org.freedesktop.DBus",
            path: "/org/freedesktop/DBus",
            interface: "org.freedesktop.DBus",
            member: "Hello",
            strings: &[],
        };
        call(&mut bus, 1, &hello)?;
        let mut last = String::new();
        for (serial, (service, object)) in (2..).zip(SERVICES) {
            let ask = Call {
                destination: service,
                path: object,
                interface: service,
                member: "HoldProfile",
                strings: &["performance", "A title is running", "xbemu"],
            };
            match call(&mut bus, serial, &ask) {
                Ok(_) => return Ok(Hold { _bus: bus, service }),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    /// The bus's handshake, as the process's own user: the bus reads who
    /// that is from the socket, so no name is sent.
    fn authenticate(bus: &mut UnixStream) -> Result<(), String> {
        let io = |e: std::io::Error| format!("the system bus's handshake: {e}");
        bus.write_all(b"\0AUTH EXTERNAL\r\n").map_err(io)?;
        let mut line = read_line(bus)?;
        if line.starts_with("DATA") {
            bus.write_all(b"DATA\r\n").map_err(io)?;
            line = read_line(bus)?;
        }
        if !line.starts_with("OK") {
            return Err(format!("the system bus refused the handshake ({line})"));
        }
        bus.write_all(b"BEGIN\r\n").map_err(io)
    }

    /// One line of the handshake, read a byte at a time so nothing of the
    /// messages after it is taken.
    fn read_line(bus: &mut UnixStream) -> Result<String, String> {
        let mut line = Vec::new();
        let mut byte = [0u8];
        while !line.ends_with(b"\r\n") {
            bus.read_exact(&mut byte).map_err(|e| format!("the system bus's handshake: {e}"))?;
            line.push(byte[0]);
            if line.len() > 512 {
                return Err("the system bus's handshake: a line without an end".into());
            }
        }
        Ok(String::from_utf8_lossy(&line).trim_end().to_owned())
    }

    /// A method call whose arguments are all strings.
    pub(super) struct Call<'a> {
        pub(super) destination: &'a str,
        pub(super) path: &'a str,
        pub(super) interface: &'a str,
        pub(super) member: &'a str,
        pub(super) strings: &'a [&'a str],
    }

    fn pad(buf: &mut Vec<u8>, align: usize) {
        while !buf.len().is_multiple_of(align) {
            buf.push(0);
        }
    }

    fn put_u32(buf: &mut Vec<u8>, v: u32) {
        pad(buf, 4);
        buf.extend_from_slice(&v.to_le_bytes());
    }

    fn put_string(buf: &mut Vec<u8>, s: &str) {
        put_u32(buf, s.len() as u32);
        buf.extend_from_slice(s.as_bytes());
        buf.push(0);
    }

    /// A header field: its code and a variant holding a string of type
    /// `kind` (`s`, `o` or `g`).
    fn put_field(buf: &mut Vec<u8>, code: u8, kind: u8, value: &str) {
        pad(buf, 8);
        buf.extend_from_slice(&[code, 1, kind, 0]);
        if kind == b'g' {
            buf.push(value.len() as u8);
            buf.extend_from_slice(value.as_bytes());
            buf.push(0);
        } else {
            put_string(buf, value);
        }
    }

    /// The call as the bus takes it.
    pub(super) fn marshal(serial: u32, call: &Call<'_>) -> Vec<u8> {
        let mut body = Vec::new();
        for s in call.strings {
            put_string(&mut body, s);
        }
        // Little-endian, a method call, no flags, protocol 1.
        let mut m = vec![b'l', 1, 0, 1];
        put_u32(&mut m, body.len() as u32);
        put_u32(&mut m, serial);
        put_u32(&mut m, 0); // the fields' length, known below
        let fields = m.len();
        put_field(&mut m, FIELD_PATH, b'o', call.path);
        put_field(&mut m, FIELD_INTERFACE, b's', call.interface);
        put_field(&mut m, FIELD_MEMBER, b's', call.member);
        put_field(&mut m, FIELD_DESTINATION, b's', call.destination);
        if !call.strings.is_empty() {
            put_field(&mut m, FIELD_SIGNATURE, b'g', &"s".repeat(call.strings.len()));
        }
        let len = (m.len() - fields) as u32;
        m[12..16].copy_from_slice(&len.to_le_bytes());
        pad(&mut m, 8);
        m.extend_from_slice(&body);
        m
    }

    /// A message as it was read: what kind it is, what it answers, and its
    /// strings (header fields by code, and the body's leading strings).
    #[derive(Debug, Default, PartialEq, Eq)]
    pub(super) struct Message {
        pub(super) kind: u8,
        pub(super) serial: u32,
        pub(super) reply_serial: Option<u32>,
        pub(super) fields: Vec<(u8, String)>,
        pub(super) body: Vec<String>,
    }

    #[cfg(test)]
    impl Message {
        /// The header field `code`, when it is a string.
        pub(super) fn field(&self, code: u8) -> Option<&str> {
            self.fields.iter().find(|(c, _)| *c == code).map(|(_, v)| v.as_str())
        }
    }

    /// A cursor over a message's bytes; every read is checked.
    struct Cursor<'a> {
        bytes: &'a [u8],
        at: usize,
    }

    impl Cursor<'_> {
        fn align(&mut self, to: usize) {
            self.at = self.at.next_multiple_of(to);
        }

        fn take(&mut self, n: usize) -> Option<&[u8]> {
            let slice = self.bytes.get(self.at..self.at.checked_add(n)?)?;
            self.at += n;
            Some(slice)
        }

        fn u8(&mut self) -> Option<u8> {
            self.take(1).map(|b| b[0])
        }

        fn u32(&mut self) -> Option<u32> {
            self.align(4);
            self.take(4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        }

        /// A string of `len` bytes and its terminator.
        fn text(&mut self, len: usize) -> Option<String> {
            let s = String::from_utf8_lossy(self.take(len)?).into_owned();
            self.take(1)?;
            Some(s)
        }

        fn string(&mut self) -> Option<String> {
            let len = self.u32()? as usize;
            self.text(len)
        }

        fn signature(&mut self) -> Option<String> {
            let len = usize::from(self.u8()?);
            self.text(len)
        }
    }

    /// Reads a whole message's bytes apart. `None`: not a message this side
    /// reads (big-endian, cut short, or a field of a type no header has).
    pub(super) fn parse(bytes: &[u8]) -> Option<Message> {
        let mut c = Cursor { bytes, at: 0 };
        let head = c.take(4)?;
        if head[0] != b'l' {
            return None;
        }
        let mut m = Message { kind: head[1], ..Message::default() };
        let body_len = c.u32()? as usize;
        m.serial = c.u32()?;
        let fields_end = c.u32()? as usize + 16;
        let mut signature = String::new();
        while c.at < fields_end {
            c.align(8);
            if c.at >= fields_end {
                break;
            }
            let code = c.u8()?;
            match c.signature()?.as_str() {
                "u" => {
                    let v = c.u32()?;
                    if code == FIELD_REPLY_SERIAL {
                        m.reply_serial = Some(v);
                    }
                }
                "s" | "o" => {
                    let v = c.string()?;
                    m.fields.push((code, v));
                }
                "g" => {
                    let v = c.signature()?;
                    if code == FIELD_SIGNATURE {
                        signature.clone_from(&v);
                    }
                    m.fields.push((code, v));
                }
                _ => return None,
            }
        }
        c.at = fields_end;
        c.align(8);
        let body_end = c.at.checked_add(body_len)?;
        if body_end > bytes.len() {
            return None;
        }
        for kind in signature.bytes() {
            if kind != b's' || c.at >= body_end {
                break;
            }
            m.body.push(c.string()?);
        }
        Some(m)
    }

    /// The next message on the bus, whole.
    fn read_message(bus: &mut UnixStream) -> Result<Vec<u8>, String> {
        let io = |e: std::io::Error| format!("the system bus: {e}");
        let mut m = vec![0u8; 16];
        bus.read_exact(&mut m).map_err(io)?;
        if m[0] != b'l' {
            return Err(
                "the system bus answers in an order of bytes this side does not read".into()
            );
        }
        let body = u32::from_le_bytes([m[4], m[5], m[6], m[7]]) as usize;
        let fields = u32::from_le_bytes([m[12], m[13], m[14], m[15]]) as usize;
        let total = (16 + fields).next_multiple_of(8) + body;
        if total > MAX_MESSAGE {
            return Err(format!("the system bus sent a message of {total} bytes"));
        }
        m.resize(total, 0);
        bus.read_exact(&mut m[16..]).map_err(io)?;
        Ok(m)
    }

    /// Makes the call and waits for its answer. An error answer is the
    /// error's name and its text.
    fn call(bus: &mut UnixStream, serial: u32, call: &Call<'_>) -> Result<Message, String> {
        bus.write_all(&marshal(serial, call)).map_err(|e| format!("the system bus: {e}"))?;
        for _ in 0..MAX_UNRELATED {
            let bytes = read_message(bus)?;
            let Some(m) = parse(&bytes) else {
                continue;
            };
            if m.reply_serial != Some(serial) {
                continue;
            }
            return match m.kind {
                METHOD_RETURN => Ok(m),
                ERROR => Err(format!(
                    "{}.{} was refused: {}",
                    call.destination,
                    call.member,
                    m.body.first().map_or("no reason given", String::as_str)
                )),
                kind => Err(format!("the system bus answered with a message of kind {kind}")),
            };
        }
        Err(format!("{}.{} was not answered", call.destination, call.member))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::unix::net::UnixListener;

        /// A bus that knows `service` only: the handshake, `Hello`, and
        /// `HoldProfile` answered with a cookie or, for another name, an
        /// error. Returns the calls it was sent once its caller hangs up.
        fn bus(listener: UnixListener, service: &'static str) -> Vec<Message> {
            let (mut peer, _) = listener.accept().unwrap();
            let mut nul = [0u8];
            peer.read_exact(&mut nul).unwrap();
            assert_eq!(read_line(&mut peer).unwrap(), "AUTH EXTERNAL");
            peer.write_all(b"DATA\r\n").unwrap();
            assert_eq!(read_line(&mut peer).unwrap(), "DATA");
            peer.write_all(b"OK 0123456789abcdef\r\n").unwrap();
            assert_eq!(read_line(&mut peer).unwrap(), "BEGIN");
            let mut calls = Vec::new();
            while let Ok(bytes) = read_message(&mut peer) {
                let m = parse(&bytes).unwrap();
                let known = [Some("org.freedesktop.DBus"), Some(service)]
                    .contains(&m.field(FIELD_DESTINATION));
                // A signal first, as a bus sends: the caller passes it over.
                let mut out = vec![b'l', 4, 0, 1];
                put_u32(&mut out, 0);
                put_u32(&mut out, 90);
                put_u32(&mut out, 0);
                pad(&mut out, 8);
                // The answer: its kind, what it answers, and for an error
                // its text.
                let mut body = Vec::new();
                if !known {
                    put_string(&mut body, "no such name");
                }
                let start = out.len();
                out.extend_from_slice(&[b'l', if known { METHOD_RETURN } else { ERROR }, 0, 1]);
                out.extend_from_slice(&(body.len() as u32).to_le_bytes());
                out.extend_from_slice(&91u32.to_le_bytes());
                out.extend_from_slice(&0u32.to_le_bytes());
                let fields = out.len();
                out.extend_from_slice(&[FIELD_REPLY_SERIAL, 1, b'u', 0]);
                out.extend_from_slice(&m.serial.to_le_bytes());
                if !known {
                    while !(out.len() - start).is_multiple_of(8) {
                        out.push(0);
                    }
                    out.extend_from_slice(&[FIELD_SIGNATURE, 1, b'g', 0, 1, b's', 0]);
                }
                let len = (out.len() - fields) as u32;
                out[start + 12..start + 16].copy_from_slice(&len.to_le_bytes());
                while !(out.len() - start).is_multiple_of(8) {
                    out.push(0);
                }
                out.extend_from_slice(&body);
                peer.write_all(&out).unwrap();
                calls.push(m);
            }
            calls
        }

        fn socket(name: &str) -> (PathBuf, UnixListener) {
            let path =
                std::env::temp_dir().join(format!("xb-core-power-{name}-{}", std::process::id()));
            let _ = std::fs::remove_file(&path);
            let listener = UnixListener::bind(&path).unwrap();
            (path, listener)
        }

        #[test]
        fn a_call_reads_back_as_it_was_written() {
            let call = Call {
                destination: "org.example.Service",
                path: "/org/example/Object",
                interface: "org.example.Interface",
                member: "Member",
                strings: &["one", "", "three of them"],
            };
            let m = parse(&marshal(7, &call)).unwrap();
            assert_eq!(m.kind, 1);
            assert_eq!(m.serial, 7);
            assert_eq!(m.field(FIELD_DESTINATION), Some(call.destination));
            assert_eq!(m.field(FIELD_PATH), Some(call.path));
            assert_eq!(m.field(FIELD_INTERFACE), Some(call.interface));
            assert_eq!(m.field(FIELD_MEMBER), Some(call.member));
            assert_eq!(m.body, call.strings);
        }

        #[test]
        fn a_message_cut_short_anywhere_is_not_read() {
            let call = Call {
                destination: "a.b",
                path: "/a",
                interface: "a.b",
                member: "M",
                strings: &["text"],
            };
            let whole = marshal(1, &call);
            for len in 0..whole.len() {
                assert_eq!(parse(&whole[..len]), None, "{len} of {} bytes", whole.len());
            }
        }

        #[test]
        fn the_hold_is_asked_of_the_service_the_bus_knows_and_lasts_until_dropped() {
            for (i, (service, object)) in SERVICES.into_iter().enumerate() {
                let (path, listener) = socket(&format!("hold{i}"));
                let server = std::thread::spawn(move || bus(listener, service));
                let hold = hold_on(&path).unwrap();
                assert_eq!(hold.service(), service);
                // The bus's side ends only when the hold's connection does.
                assert!(!server.is_finished());
                drop(hold);
                let calls = server.join().unwrap();
                let last = calls.last().unwrap();
                assert_eq!(last.field(FIELD_MEMBER), Some("HoldProfile"));
                assert_eq!(last.field(FIELD_PATH), Some(object));
                assert_eq!(last.body.first().map(String::as_str), Some("performance"));
                assert_eq!(last.body.len(), 3);
                let _ = std::fs::remove_file(&path);
            }
        }

        #[test]
        fn a_bus_without_the_service_says_why_and_nothing_is_held() {
            let (path, listener) = socket("none");
            let server = std::thread::spawn(move || bus(listener, "org.example.Other"));
            let why = hold_on(&path).err().unwrap();
            assert!(why.contains("no such name"), "{why}");
            assert_eq!(server.join().unwrap().len(), 1 + SERVICES.len());
            let _ = std::fs::remove_file(&path);
        }

        #[test]
        fn no_bus_is_an_answer_not_a_failure() {
            let why = hold_on(Path::new("/nonexistent/xb-core-power")).err().unwrap();
            assert!(why.contains("no system bus"), "{why}");
        }
    }
}
