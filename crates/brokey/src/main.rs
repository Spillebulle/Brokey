//! `brokey` opens the window. `brokey <subcommand>` is the text mode:
//! the same core with a table printer, which is how the sources are exercised
//! on a machine without a display.

/// The flags that are a text-mode command in their own right. Without
/// these, anything beginning with a dash opens the window, so `brokey
/// --help` opened a window rather than printing the help that `cli::run`
/// has always had an arm for.
const TEXT_FLAGS: [&str; 4] = ["--help", "-h", "--version", "-V"];

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

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
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

    #[test]
    fn any_other_flag_still_opens_the_window() {
        for flag in ["--", "--some-tauri-flag", "-x"] {
            assert!(!is_text_mode(&args(&[flag])), "{flag}");
        }
    }
}
