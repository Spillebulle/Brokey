//! `brokey` opens the window. `brokey <subcommand>` is the text mode:
//! the same core with a table printer, which is how the sources are exercised
//! on a machine without a display.
//!
//! **One binary, and on Windows no console behind it.** A console-subsystem
//! binary is given a console window at every launch, so the Start menu
//! shortcut opened a black window behind Brokey and the setup executable
//! opened one behind its own. A release build is therefore a
//! windows-subsystem binary, which is given no console at all, and the text
//! mode takes the console of the terminal that started it instead.
//! `console.rs` is where that is done and why.

// Windows decides whether to give a process a console from this, and it is
// read out of the binary at launch: a windows-subsystem binary never flashes
// one, which is the whole point. Debug builds stay console binaries so that
// `npm run app:dev` still shows the window's log lines in the terminal that
// started it. `console::attach_to_the_terminal_that_started_this` is what
// keeps the text mode printing in the release build, and it is a no-op in a
// build that already has a console, so both builds take the same path.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[cfg(windows)]
mod console;

/// The flags that `cli::run` answers rather than the window. Without these,
/// anything beginning with a dash opens the window, so `brokey --help`
/// opened a window rather than printing the help that `cli::run` has always
/// had an arm for.
///
/// `--install` is the odd one: it draws a window of its own on Windows
/// rather than printing a table. What it shares with the other four is the
/// only thing this list is about, which is that it must not reach Tauri.
const TEXT_FLAGS: [&str; 5] = ["--help", "-h", "--version", "-V", "--install"];

/// Whether these arguments are a text-mode command rather than the window.
///
/// A separate function so it can be tested: `main` cannot be, and the rule
/// it encodes has already been wrong once.
fn is_text_mode(args: &[String]) -> bool {
    let Some(first) = args.first() else {
        return false;
    };
    first != "--" && (!first.starts_with('-') || TEXT_FLAGS.contains(&first.as_str()))
}

/// Whether this launch is the setup executable installing Brokey.
///
/// **The payload decides, and an argument does not.** A setup executable is
/// downloaded and double-clicked, so it is started with no command line at
/// all; a rule that waited for `--install` would leave it with no way to reach
/// its own installer, and it would open Brokey out of a Downloads folder
/// instead. `setup/payload.rs` states that rule and
/// `a_file_carrying_a_package_is_recognised_by_its_last_bytes_alone` pins it.
/// This is where it is acted on.
///
/// **Nothing changes for an ordinary copy of Brokey.** A `brokey.exe` from the
/// MSI, from a build, or from anywhere else carries no package, so this is
/// false and the window opens as it always has. The cost on that path is a
/// seek and sixteen bytes, which is what `payload::carried_by` reads.
///
/// A separate function, taking the two answers rather than working them out,
/// so the rule can be tested without a payload-carrying binary to hand.
#[cfg(windows)]
fn is_setup_run(has_arguments: bool, carries_a_package: bool) -> bool {
    !has_arguments && carries_a_package
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    #[cfg(windows)]
    {
        // Before anything prints. A text-mode run is the only one with
        // something to say to a terminal: the window and the setup window say
        // it on the screen, and a setup run reaches neither of the two
        // branches below.
        if is_text_mode(&args) {
            console::attach_to_the_terminal_that_started_this();
        }
        let carries_a_package = std::env::current_exe()
            .map(|path| brokey_lib::setup::payload::carried_by(&path))
            .unwrap_or(false);
        if is_setup_run(!args.is_empty(), carries_a_package) {
            std::process::exit(brokey_lib::setup::install());
        }
    }
    if is_text_mode(&args) {
        std::process::exit(brokey_lib::cli::run(&args));
    }
    brokey_lib::run();
}

#[cfg(test)]
mod tests {
    use super::is_text_mode;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn no_arguments_opens_the_window() {
        assert!(!is_text_mode(&args(&[])));
    }

    #[test]
    fn a_command_is_text_mode() {
        for command in ["search", "sources", "plan", "help"] {
            assert!(is_text_mode(&args(&[command])), "{command}");
        }
    }

    /// The bug this rule had: every one of these begins with a dash, so all
    /// four opened the window instead of printing what they ask for.
    #[test]
    fn the_help_and_version_flags_are_text_mode() {
        for flag in ["--help", "-h", "--version", "-V"] {
            assert!(is_text_mode(&args(&[flag])), "{flag}");
        }
    }

    /// `--install` is the setup executable's own flag and must never open the
    /// window: the window is what it is there to install.
    #[test]
    fn the_install_flag_is_not_the_window() {
        assert!(is_text_mode(&args(&["--install"])));
    }

    /// **What makes a binary the installer, at the one place it is decided.**
    ///
    /// The setup executable is double-clicked, which means no arguments at
    /// all, so the package on the end of the file is the only signal there
    /// is. An ordinary `brokey.exe` carries none, and must still open the
    /// window however it is started.
    #[cfg(windows)]
    #[test]
    fn a_double_clicked_setup_executable_installs_and_nothing_else_does() {
        use super::is_setup_run;

        // Double-clicked setup: no arguments, a package on the end.
        assert!(is_setup_run(false, true));

        // Every ordinary copy of Brokey, however it is started. This is the
        // half that says no existing user's Brokey changes.
        assert!(!is_setup_run(false, false), "brokey.exe opens the window");
        assert!(
            !is_setup_run(true, false),
            "brokey search steam is text mode"
        );

        // A setup executable given a command line is not a double-click, so it
        // is whatever that command line says: `brokey-setup.exe --version`
        // prints a version, and `--install` reaches the installer through
        // `is_text_mode` and `cli::run` rather than through here.
        assert!(!is_setup_run(true, true));
    }

    #[test]
    fn any_other_flag_still_opens_the_window() {
        for flag in ["--", "--some-tauri-flag", "-x"] {
            assert!(!is_text_mode(&args(&[flag])), "{flag}");
        }
    }
}
