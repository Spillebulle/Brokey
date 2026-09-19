//! What machine this is: the distribution or the Windows edition, the
//! desktop, which tools exist and where things live. Read once and cheap.
//!
//! Both halves are compiled on both platforms. Only the functions that
//! actually touch the machine are selected by `#[cfg]`; the parsers that
//! turn what was read into a `SystemInfo` are pure and are built
//! everywhere, so each one's tests run on either machine and the page's
//! contract test can check both shapes in one run.

pub mod linux;
pub mod windows;

#[cfg(unix)]
pub use linux::{detect, is_executable, which};
#[cfg(windows)]
pub use windows::detect;

// Pure on both platforms, so it is always in scope.
pub use linux::from_os_release;

// Also pure: no OS-specific API, just environment variables and the
// portable `directories` crate. Read by `http`, `commands` and `settings`
// on both platforms, so they stay in scope here rather than at
// `system::linux`.
pub use linux::{Dirs, cache_dir_for, has_nvidia};

/// Run a program and return its stdout as text, or the sentence that
/// explains why not. For read-only queries of tools like `flatpak` and
/// `fwupdmgr`; never for anything that changes the machine (that is a Step).
pub fn run(program: &str, args: &[&str]) -> crate::Result<String> {
    let out = std::process::Command::new(program)
        .args(args)
        .env("LC_ALL", "C.UTF-8")
        .output()
        .map_err(|e| crate::Error::new(format!("Could not run {program}: {e}.")))?;
    if !out.status.success() {
        return Err(crate::Error::new(run_failure(
            program,
            args.first().copied().unwrap_or(""),
            &String::from_utf8_lossy(&out.stderr),
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The sentence for a tool that exited with an error: the program, its
/// verb and the first line it printed. A full sentence, because three
/// callers show it to the user as it is.
fn run_failure(program: &str, verb: &str, stderr: &str) -> String {
    let first = stderr.lines().next().unwrap_or("").trim();
    let verb = if verb.is_empty() {
        String::new()
    } else {
        format!(" {verb}")
    };
    if first.is_empty() {
        format!("{program}{verb} failed.")
    } else {
        format!("{program}{verb} failed: {}.", first.trim_end_matches('.'))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_failures_are_sentences() {
        assert_eq!(
            run_failure(
                "fwupdmgr",
                "get-devices",
                "error: failed to connect to daemon\nmore\n"
            ),
            "fwupdmgr get-devices failed: error: failed to connect to daemon."
        );
        assert_eq!(
            run_failure("flatpak", "remotes", "error: Unable to load summary.\n"),
            "flatpak remotes failed: error: Unable to load summary.",
            "one full stop, not two"
        );
        assert_eq!(run_failure("chwd", "-i", "\n"), "chwd -i failed.");
        assert_eq!(run_failure("chwd", "", ""), "chwd failed.");
    }
}
