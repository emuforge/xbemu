//! Choosing a disc image when the app is started without one.
//!
//! A macOS app is started from the Finder or the Dock with no arguments, so
//! it asks for the disc with the system's Open panel. Other hosts have no
//! picker: started without a disc image, the app prints its usage.

use std::path::PathBuf;

/// What the person chose.
#[cfg_attr(
    not(target_os = "macos"),
    expect(dead_code, reason = "only the macOS Open panel chooses a disc or is cancelled")
)]
pub enum Choice {
    /// This disc image.
    Disc(PathBuf),
    /// Nothing: the panel was cancelled.
    Cancelled,
    /// This host has no picker.
    Unavailable,
}

/// Asks for a disc image. Call on the main thread, before the window's event
/// loop exists.
pub fn choose_disc() -> Choice {
    #[cfg(target_os = "macos")]
    {
        macos::choose_disc()
    }
    #[cfg(not(target_os = "macos"))]
    {
        Choice::Unavailable
    }
}

#[cfg(target_os = "macos")]
mod macos {
    //! The AppKit Open panel.

    use objc2::MainThreadMarker;
    use objc2_app_kit::{
        NSApplication, NSApplicationActivationPolicy, NSModalResponseOK, NSOpenPanel,
    };
    use objc2_foundation::NSString;

    use super::Choice;

    pub(super) fn choose_disc() -> Choice {
        let Some(mtm) = MainThreadMarker::new() else {
            return Choice::Unavailable;
        };
        // The app is not running its event loop yet: it has to come to the
        // front itself for the panel to be seen.
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
        #[expect(deprecated, reason = "activate() needs macOS 14; the app supports 12")]
        app.activateIgnoringOtherApps(true);
        let panel = NSOpenPanel::openPanel(mtm);
        panel.setCanChooseFiles(true);
        panel.setCanChooseDirectories(false);
        panel.setAllowsMultipleSelection(false);
        panel.setMessage(Some(&NSString::from_str(
            "Choose a disc image (.iso, .xiso or .cci) to play",
        )));
        if panel.runModal() != NSModalResponseOK {
            return Choice::Cancelled;
        }
        let urls = panel.URLs();
        match urls.firstObject().and_then(|url| url.to_file_path()) {
            Some(path) => Choice::Disc(path),
            None => Choice::Cancelled,
        }
    }
}
