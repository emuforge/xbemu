//! The command line: a hand-rolled parser (eight options do not justify a
//! dependency) and the usage text.

use std::ffi::OsString;
use std::path::PathBuf;

use log::LevelFilter;
use xb_core::CpuBackend;

/// The usage text, printed for `--help` and after a usage error.
pub const USAGE: &str = "\
usage: xbemu <disc image> [options]

  <disc image>          the game to boot (.cci, .iso)

options:
  --data-dir DIR        where the console's hard disk, caches and settings
                        live (default: $XDG_DATA_HOME/xbemu or
                        ~/.local/share/xbemu)
  --fullscreen          start in fullscreen and keep the setting (F11, Alt+Enter, a
                        double click or the menu toggles)
  --log LEVEL           off, error, warn, info (default), debug or trace
  --cpu BACKEND         auto, kvm or translator, for this run
  --state STATE         start from a savestate: a slot's number (0 to 9) or
                        a state file's path
  --frames N            exit with status 0 after N presented frames
  --screenshot-dir DIR  where F12's screenshots go (default: the data
                        directory's screenshots folder)
  --screenshot-at N     save a screenshot when N frames were presented
  -V, --version         the version and the commit it was built from
  -h, --help            this text

F1, Escape or a gamepad's Guide button (the Xbox button) opens the in-game
menu, where every setting lives. F12 saves a screenshot. F2 saves a state
to the current slot and F4 loads it; F6 and F7 choose the slot.
";

/// What the user asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    /// The disc image; `None` only with `selftest`.
    pub disc: Option<PathBuf>,
    /// `--data-dir`.
    pub data_dir: Option<PathBuf>,
    /// `--fullscreen`.
    pub fullscreen: bool,
    /// `--state`: the savestate to start from, a slot's number or a path.
    pub state: Option<String>,
    /// `--log`.
    pub log: LevelFilter,
    /// `--cpu`.
    pub cpu: Option<CpuBackend>,
    /// `--frames`.
    pub frames: Option<u64>,
    /// `--screenshot-dir`.
    pub screenshot_dir: Option<PathBuf>,
    /// `--screenshot-at`.
    pub screenshot_at: Option<u64>,
    /// `--selftest` (hidden): run the window, input and audio without a core.
    pub selftest: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            disc: None,
            data_dir: None,
            fullscreen: false,
            state: None,
            log: LevelFilter::Info,
            cpu: None,
            frames: None,
            screenshot_dir: None,
            screenshot_at: None,
            selftest: false,
        }
    }
}

/// A parsed command line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// Run with these options.
    Run(Options),
    /// Print the usage text and exit successfully.
    Help,
    /// Print [`version`] and exit successfully.
    Version,
}

/// The version line: the package's version and, in a build that was told it
/// (`XBEMU_BUILD_COMMIT` in the build's environment, which the release build
/// sets), the commit it was built from. A pre-release is named by that
/// commit, so a report can say exactly which build it is about.
pub fn version() -> String {
    match option_env!("XBEMU_BUILD_COMMIT") {
        Some(commit) if !commit.is_empty() => {
            format!("xbemu {} ({commit})", env!("CARGO_PKG_VERSION"))
        }
        _ => format!("xbemu {}", env!("CARGO_PKG_VERSION")),
    }
}

/// Parses the arguments after the program name. Options take their value as
/// the next argument or after `=`; `--` ends the options.
///
/// # Errors
/// A sentence saying what is wrong with the command line.
pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Command, String> {
    let mut o = Options::default();
    let mut args = args.into_iter();
    let mut options_done = false;
    while let Some(arg) = args.next() {
        let text = arg.to_string_lossy().into_owned();
        if options_done || !text.starts_with('-') || text == "-" {
            if o.disc.is_some() {
                return Err(format!("more than one disc image given ('{text}')"));
            }
            o.disc = Some(PathBuf::from(arg));
            continue;
        }
        let (name, inline) = match text.split_once('=') {
            // A path that is not Unicode survives only as its own argument.
            Some((n, v)) => (n.to_owned(), Some(OsString::from(v))),
            None => (text.clone(), None),
        };
        let value = |args: &mut dyn Iterator<Item = OsString>| {
            inline.clone().or_else(|| args.next()).ok_or(format!("{name} needs a value"))
        };
        let flag = |inline: &Option<OsString>| match inline {
            Some(_) => Err(format!("{name} takes no value")),
            None => Ok(()),
        };
        match name.as_str() {
            "--" => options_done = true,
            "-h" | "--help" => return Ok(Command::Help),
            "-V" | "--version" => return Ok(Command::Version),
            "--fullscreen" => {
                flag(&inline)?;
                o.fullscreen = true;
            }
            "--selftest" => {
                flag(&inline)?;
                o.selftest = true;
            }
            "--data-dir" => o.data_dir = Some(PathBuf::from(value(&mut args)?)),
            "--state" => o.state = Some(value(&mut args)?.to_string_lossy().into_owned()),
            "--screenshot-dir" => o.screenshot_dir = Some(PathBuf::from(value(&mut args)?)),
            "--log" => {
                let v = value(&mut args)?;
                o.log = v
                    .to_string_lossy()
                    .parse()
                    .map_err(|_| format!("--log: '{}' is not a log level", v.to_string_lossy()))?;
            }
            "--cpu" => {
                let v = value(&mut args)?;
                o.cpu = Some(match v.to_string_lossy().as_ref() {
                    "auto" => CpuBackend::Auto,
                    "kvm" => CpuBackend::Kvm,
                    "translator" => CpuBackend::Translator,
                    other => {
                        return Err(format!("--cpu: '{other}' is not auto, kvm or translator"));
                    }
                });
            }
            "--frames" => {
                let v = value(&mut args)?;
                o.frames =
                    Some(v.to_string_lossy().parse().map_err(|_| {
                        format!("--frames: '{}' is not a number", v.to_string_lossy())
                    })?);
            }
            "--screenshot-at" => {
                let v = value(&mut args)?;
                o.screenshot_at = Some(v.to_string_lossy().parse().map_err(|_| {
                    format!("--screenshot-at: '{}' is not a number", v.to_string_lossy())
                })?);
            }
            other => return Err(format!("unknown option '{other}'")),
        }
    }
    if o.disc.is_none() && !o.selftest {
        return Err("no disc image given".into());
    }
    Ok(Command::Run(o))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str]) -> Result<Command, String> {
        parse(args.iter().map(OsString::from))
    }

    fn options(args: &[&str]) -> Options {
        match run(args) {
            Ok(Command::Run(o)) => o,
            other => panic!("{args:?} gave {other:?}"),
        }
    }

    #[test]
    fn a_lone_image_runs_with_the_defaults() {
        let o = options(&["Halo (USA).cci"]);
        assert_eq!(o, Options { disc: Some("Halo (USA).cci".into()), ..Options::default() });
        assert_eq!(o.log, LevelFilter::Info);
    }

    #[test]
    fn every_option_is_read_wherever_it_stands() {
        let o = options(&[
            "--data-dir",
            "/d",
            "--fullscreen",
            "game.iso",
            "--log",
            "debug",
            "--cpu",
            "translator",
            "--frames",
            "600",
            "--screenshot-dir",
            "/s",
            "--screenshot-at",
            "90",
        ]);
        assert_eq!(
            o,
            Options {
                disc: Some("game.iso".into()),
                data_dir: Some("/d".into()),
                fullscreen: true,
                state: None,
                log: LevelFilter::Debug,
                cpu: Some(CpuBackend::Translator),
                frames: Some(600),
                screenshot_dir: Some("/s".into()),
                screenshot_at: Some(90),
                selftest: false,
            }
        );
    }

    #[test]
    fn a_value_after_an_equals_sign_means_the_same_as_the_next_argument() {
        let spaced = options(&["g.cci", "--cpu", "kvm", "--frames", "3", "--data-dir", "/x=y"]);
        let joined = options(&["g.cci", "--cpu=kvm", "--frames=3", "--data-dir=/x=y"]);
        assert_eq!(spaced, joined);
    }

    #[test]
    fn a_double_dash_lets_an_image_name_start_with_a_dash() {
        assert_eq!(options(&["--", "-odd.cci"]).disc, Some("-odd.cci".into()));
    }

    #[test]
    fn the_version_is_asked_for_like_help_and_names_the_package() {
        assert_eq!(run(&["--version"]), Ok(Command::Version));
        assert_eq!(run(&["g.cci", "-V"]), Ok(Command::Version));
        assert!(version().starts_with(concat!("xbemu ", env!("CARGO_PKG_VERSION"))));
    }

    #[test]
    fn help_wins_over_everything_else() {
        assert_eq!(run(&["--help"]), Ok(Command::Help));
        assert_eq!(run(&["g.cci", "-h", "--bogus"]), Ok(Command::Help));
    }

    #[test]
    fn bad_command_lines_are_usage_errors() {
        for bad in [
            &[][..],
            &["--fullscreen"],
            &["a.cci", "b.cci"],
            &["a.cci", "--bogus"],
            &["a.cci", "--cpu"],
            &["a.cci", "--cpu", "arm"],
            &["a.cci", "--log", "loud"],
            &["a.cci", "--frames", "many"],
            &["a.cci", "--frames", "-1"],
            &["a.cci", "--screenshot-at", "soon"],
            &["a.cci", "--fullscreen=yes"],
        ] {
            assert!(run(bad).is_err(), "{bad:?} was accepted");
        }
    }

    #[test]
    fn the_self_test_needs_no_image() {
        let o = options(&["--selftest"]);
        assert!(o.selftest && o.disc.is_none());
    }
}
