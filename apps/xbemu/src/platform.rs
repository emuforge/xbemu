//! The host-specific odds and ends: where data lives by default and how the
//! program starts itself again. Each has one function with a `cfg` branch per
//! platform, so a new host is added here and nowhere else.

use std::ffi::OsString;
use std::path::PathBuf;

/// The default data directory: the platform's per-user application data
/// place, with `xbemu` under it. `None` if the environment names no home.
#[must_use]
pub fn default_data_dir() -> Option<PathBuf> {
    let var = |name: &str| std::env::var_os(name);
    #[cfg(target_os = "windows")]
    {
        var("APPDATA").filter(|v| !v.is_empty()).map(|v| PathBuf::from(v).join("xbemu"))
    }
    #[cfg(target_os = "macos")]
    {
        var("HOME")
            .filter(|v| !v.is_empty())
            .map(|v| PathBuf::from(v).join("Library/Application Support/xbemu"))
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        xdg_data_dir(var("XDG_DATA_HOME"), var("HOME"))
    }
}

/// The XDG base directory rule: `$XDG_DATA_HOME/xbemu` if that is set to an
/// absolute path, else `$HOME/.local/share/xbemu`.
#[cfg_attr(
    any(target_os = "windows", target_os = "macos"),
    expect(dead_code, reason = "XDG hosts")
)]
fn xdg_data_dir(xdg_data_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let absolute = |v: OsString| Some(PathBuf::from(v)).filter(|p| p.is_absolute());
    xdg_data_home
        .and_then(absolute)
        .or_else(|| home.and_then(absolute).map(|h| h.join(".local/share")))
        .map(|base| base.join("xbemu"))
}

/// Starts the program again with the same arguments, for the menu's Reset
/// (the renderer cannot be created twice in one process). On Unix the new
/// program replaces this one and the call returns only on failure; elsewhere
/// it is started as a child and its exit status is returned.
///
/// # Errors
/// The program could not be started.
pub fn restart() -> std::io::Result<i32> {
    let exe = std::env::current_exe()?;
    let mut command = std::process::Command::new(exe);
    command.args(std::env::args_os().skip(1));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(command.exec())
    }
    #[cfg(not(unix))]
    {
        Ok(command.status()?.code().unwrap_or(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(s: &str) -> Option<OsString> {
        Some(OsString::from(s))
    }

    #[test]
    fn xdg_data_home_is_used_when_it_is_an_absolute_path() {
        assert_eq!(xdg_data_dir(os("/x/data"), os("/home/u")), Some("/x/data/xbemu".into()));
    }

    #[test]
    fn home_is_the_fallback_when_xdg_data_home_is_unset_empty_or_relative() {
        let expected = Some(PathBuf::from("/home/u/.local/share/xbemu"));
        assert_eq!(xdg_data_dir(None, os("/home/u")), expected);
        assert_eq!(xdg_data_dir(os(""), os("/home/u")), expected);
        assert_eq!(xdg_data_dir(os("relative"), os("/home/u")), expected);
    }

    #[test]
    fn there_is_no_default_without_a_home() {
        assert_eq!(xdg_data_dir(None, None), None);
    }
}
