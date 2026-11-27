//! The `log` backend: one line per record on stderr, with the time since the
//! program started, the level and the target.

use std::io::Write;
use std::sync::OnceLock;
use std::time::Instant;

use log::{Level, LevelFilter, Log, Metadata, Record};

/// Writes records to stderr. The time is seconds since [`init`], which is
/// what a session log is read by; it needs no clock or time zone code.
struct StderrLogger {
    start: Instant,
}

impl Log for StderrLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        enabled(metadata.level(), metadata.target(), log::max_level())
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) || is_close_noise(record) {
            return;
        }
        let line = format_line(
            self.start.elapsed().as_secs_f64(),
            record.level(),
            record.target(),
            record.args(),
        );
        // One write per record, so lines from several threads do not mix.
        let _ = std::io::stderr().lock().write_all(line.as_bytes());
    }

    fn flush(&self) {}
}

/// The log targets of the emulator's own crates (`AGENTS.md`, "Logs").
const OWN_TARGETS: [&str; 11] =
    ["disc", "fs", "cpu", "mem", "kernel", "usb", "dev", "gpu", "apu", "core", "app"];

/// Whether a record is written at `max`. The emulator's own targets get the
/// level asked for; libraries (the window, audio and gamepad crates, which
/// log every protocol message at DEBUG) get one level less, so `--log debug`
/// stays readable and `--log trace` still shows everything they have below
/// TRACE.
fn enabled(level: Level, target: &str, max: LevelFilter) -> bool {
    let own = OWN_TARGETS.contains(&target) || target.starts_with("xb");
    let max = match max {
        _ if own => max,
        LevelFilter::Trace => LevelFilter::Debug,
        LevelFilter::Debug => LevelFilter::Info,
        other => other,
    };
    level <= max
}

/// The PulseAudio client reports its own orderly disconnection, when the
/// audio stream is closed at exit, as an error. Real stream errors reach the
/// app through `cpal`'s error callback and are logged there.
fn is_close_noise(record: &Record<'_>) -> bool {
    record.target() == "pulseaudio::client::reactor"
        && record.args().to_string().ends_with("Client disconnected")
}

/// One log line: `[   12.345] INFO  app: text`.
fn format_line(seconds: f64, level: Level, target: &str, args: &std::fmt::Arguments<'_>) -> String {
    format!("[{seconds:9.3}] {level:<5} {target}: {args}\n")
}

/// Installs the logger at `level`. Later calls do nothing.
pub fn init(level: LevelFilter) {
    static LOGGER: OnceLock<StderrLogger> = OnceLock::new();
    let logger = LOGGER.get_or_init(|| StderrLogger { start: Instant::now() });
    if log::set_logger(logger).is_ok() {
        log::set_max_level(level);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_carries_the_time_the_level_the_target_and_the_text() {
        let line = format_line(1.5, Level::Warn, "app", &format_args!("no pad {}", 2));
        assert_eq!(line, "[    1.500] WARN  app: no pad 2\n");
    }

    #[test]
    fn libraries_log_one_level_less_than_the_emulator_below_info() {
        assert!(enabled(Level::Debug, "gpu", LevelFilter::Debug));
        assert!(enabled(Level::Trace, "xb_core::boot", LevelFilter::Trace));
        assert!(!enabled(Level::Debug, "pulseaudio::client", LevelFilter::Debug));
        assert!(enabled(Level::Debug, "pulseaudio::client", LevelFilter::Trace));
        assert!(!enabled(Level::Trace, "sctk", LevelFilter::Trace));
        for target in ["app", "winit"] {
            assert!(enabled(Level::Info, target, LevelFilter::Info));
            assert!(!enabled(Level::Debug, target, LevelFilter::Info));
            assert!(!enabled(Level::Error, target, LevelFilter::Off));
        }
    }
}
