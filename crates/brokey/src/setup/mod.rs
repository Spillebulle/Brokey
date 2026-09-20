//! The setup executable: Brokey's own binary with an MSI on the end of it.
//!
//! Run normally this is just Brokey. Run with `--install` it lifts the MSI
//! back out of its own file and installs it through a window of Brokey's own,
//! so somebody installing Brokey for the first time sees Brokey rather than
//! Windows Installer.

pub mod payload;

#[cfg(windows)]
use std::path::{Path, PathBuf};

/// What a run of `--install` came to: the sentence to show, and the exit code
/// to return.
///
/// One value rather than a `println!` at each failure, because the same words
/// go to two places, the console and the line under the window's title, and
/// two statements of a sentence is how the two come to disagree.
#[cfg(windows)]
pub struct Outcome {
    /// What happened, worded for whoever double-clicked the file.
    pub sentence: String,
    /// 0 when Brokey is installed, 1 when it is not.
    pub code: i32,
}

#[cfg(windows)]
impl Outcome {
    fn installed(sentence: String) -> Self {
        Self { sentence, code: 0 }
    }

    fn failed(sentence: String) -> Self {
        Self { sentence, code: 1 }
    }
}

/// `--install`: lift the MSI out of this file, put it somewhere msiexec can
/// read, and run it while the window says what is happening.
///
/// The version comes from this file's own stem, so
/// `brokey-setup-0.1.5-x64.exe` says "Install Brokey 0.1.5" at the top of
/// the window and a renamed copy says whatever it was renamed to. That is
/// the caller's business and it is documented rather than prevented: the
/// file in front of the user is the one they should be told about.
#[cfg(windows)]
pub fn install() -> i32 {
    let outcome = match std::env::current_exe() {
        Ok(path) => unpack_and_install(&path),
        Err(e) => Outcome::failed(format!(
            "Brokey could not find its own file, so it has nothing to unpack: {e}. \
             Run the setup executable from a folder you can read."
        )),
    };
    if outcome.code == 0 {
        println!("{}", outcome.sentence);
    } else {
        eprintln!("{}", outcome.sentence);
    }
    outcome.code
}

/// The whole of `--install` with the file named rather than asked for, which
/// is what lets a test reach the "carries no installer" answer without a
/// payload anywhere near it.
#[cfg(windows)]
fn unpack_and_install(executable: &Path) -> Outcome {
    let version = version_in(executable);
    let bytes = match std::fs::read(executable) {
        Ok(bytes) => bytes,
        Err(e) => {
            return Outcome::failed(format!(
                "Brokey could not read {}: {e}. Copy the setup executable somewhere you can \
                 read it, such as your Downloads folder, and run it again.",
                executable.display()
            ));
        }
    };
    let Some(package) = payload::read(&bytes) else {
        return Outcome::failed(NO_PACKAGE.to_string());
    };
    let staged = match stage(package, &version) {
        Ok(path) => path,
        Err(e) => {
            return Outcome::failed(format!(
                "Brokey could not write the installer to a temporary folder: {e}. \
                 Check that there is room on the drive holding {}.",
                std::env::temp_dir().display()
            ));
        }
    };
    let outcome = run_installer(&staged, &version);
    // Windows Installer keeps its own copy of a package it has accepted, so
    // this one has no reader left once the wait above has returned. Best
    // effort: a file left in the temporary folder is untidy, not a failure,
    // and it is not worth overriding an outcome the user needs to read.
    let _ = std::fs::remove_file(&staged);
    if let Some(folder) = staged.parent() {
        let _ = std::fs::remove_dir(folder);
    }
    outcome
}

/// The ordinary answer for a plain `brokey.exe`, which carries nothing and is
/// the file most copies of Brokey are. Not a failure of Brokey's and not a
/// panic: a sentence saying which file does have the installer in it.
#[cfg(windows)]
const NO_PACKAGE: &str = "This copy of Brokey carries no installer, so there is nothing to \
                          unpack. The file that does is the one named \
                          brokey-setup-<version>-<architecture>.exe, from the releases page.";

/// The version to say, read off this file's own stem.
///
/// `brokey-setup-0.1.4-x64.exe` gives "0.1.4": the first dash-separated part
/// that begins with a digit, which neither `brokey` nor `setup` nor `x64` nor
/// `arm64` does. A stem holding no version at all, which is what renaming the
/// file to `installer.exe` leaves, falls back to the version this binary was
/// built as, because a title with no version in it would be worse than one
/// that is a release behind.
#[cfg(windows)]
fn version_in(executable: &Path) -> String {
    executable
        .file_stem()
        .and_then(|stem| stem.to_str())
        .and_then(version_from_stem)
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string())
}

/// [`version_in`]'s rule, over the stem alone, so it can be tested without a
/// file on the disk.
#[cfg(windows)]
fn version_from_stem(stem: &str) -> Option<String> {
    stem.split('-')
        .find(|part| part.starts_with(|c: char| c.is_ascii_digit()))
        .map(str::to_string)
}

/// Writes the package where msiexec can read it, and answers with the path.
///
/// A folder of this run's own under the user's temporary directory, keyed by
/// process id, so two setup executables started at once do not write over
/// each other's package. The user's temporary folder rather than a public
/// one: the elevated msiexec can read it, and nothing else on the machine
/// can put a different MSI there for it to read instead.
#[cfg(windows)]
fn stage(package: &[u8], version: &str) -> std::io::Result<PathBuf> {
    let folder = std::env::temp_dir().join(format!("brokey-setup-{}", std::process::id()));
    std::fs::create_dir_all(&folder)?;
    let file = folder.join(format!("brokey-{version}.msi"));
    std::fs::write(&file, package)?;
    Ok(file)
}

/// Runs `msiexec /i <package> /qn` with one administrator prompt, and waits
/// for it to finish.
///
/// **Why the prompt is raised here rather than left to Windows Installer.**
/// `packaging/windows/brokey.wxs` declares `Scope="perMachine"`, so the
/// install writes to Program Files and needs administrator rights. Windows
/// Installer asks for them itself when it is showing an interface, but `/qn`
/// is the mode in which it shows none, and in that mode it cannot ask: a
/// silent per-machine install started without those rights stops at 1925 and
/// installs nothing. The alternative, a basic interface, would ask by putting
/// Windows Installer's own progress dialog on the screen in front of the
/// window this module exists to draw. So the prompt is raised once, here, for
/// the one process that needs it.
///
/// **Why this is not the elevation call site `CLAUDE.md` forbids.** That rule
/// is about the application: Brokey's window builds plans and `brokey-helper`
/// runs the privileged steps, so the window never holds administrator rights.
/// None of that is available here. `brokey-helper` is one of the files this
/// package is on its way to putting on the machine, so it cannot be what
/// installs it, and the window started afterwards is started by Windows from
/// the Start menu with the user's own token, never by this process. What is
/// elevated here is msiexec, for as long as msiexec runs, and nothing else.
#[cfg(windows)]
fn run_installer(package: &Path, version: &str) -> Outcome {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
    use windows_sys::Win32::Foundation::ERROR_CANCELLED;
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, INFINITE, WaitForSingleObject,
    };
    use windows_sys::Win32::UI::Shell::{
        SEE_MASK_FLAG_NO_UI, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

    let file = wide_null("msiexec.exe");
    let verb = wide_null("runas");
    // The path is quoted because the user's temporary folder sits under their
    // profile, and profile names have spaces in them more often than not.
    let parameters = wide_null(&format!("/i \"{}\" /qn", package.display()));

    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        // `NOCLOSEPROCESS` so `hProcess` comes back to wait on; `FLAG_NO_UI`
        // so a failure to start msiexec at all is reported in the sentence
        // below rather than by a native Windows error dialog Brokey did not
        // write and cannot style.
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_FLAG_NO_UI,
        lpVerb: verb.as_ptr(),
        lpFile: file.as_ptr(),
        lpParameters: parameters.as_ptr(),
        // Nothing to show: `/qn` means msiexec draws no interface, and this
        // window is the interface.
        nShow: SW_HIDE,
        ..Default::default()
    };

    // SAFETY: `info` is a fully initialised `SHELLEXECUTEINFOW` whose three
    // string pointers (`file`, `verb`, `parameters`) are all still alive in
    // bindings above this call, so none of them is dangling.
    let started = unsafe { ShellExecuteExW(&mut info) };
    if started == 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_CANCELLED as i32) {
            return Outcome::failed(
                "You did not allow the change, so nothing was installed. Brokey installs for \
                 everyone on this machine, which is what Windows asks about. Run the setup \
                 executable again and choose Yes."
                    .to_string(),
            );
        }
        return Outcome::failed(format!(
            "Brokey could not start Windows Installer: {error}. Check that msiexec.exe is in \
             the System32 folder of this Windows installation."
        ));
    }

    // SAFETY: `ShellExecuteExW` succeeded above with `SEE_MASK_NOCLOSEPROCESS`
    // set, which is documented to fill `hProcess` with a fresh handle this
    // call now owns exclusively. Owning it here is what closes it.
    let process = unsafe { OwnedHandle::from_raw_handle(info.hProcess as RawHandle) };

    // SAFETY: `process` owns a live process handle for the whole of this call,
    // and `INFINITE` is the documented "wait until it exits" value. The wait
    // is bounded by msiexec exiting, which it does whether the install
    // succeeds, fails, or is stopped.
    unsafe { WaitForSingleObject(process.as_raw_handle() as _, INFINITE) };

    let mut code: u32 = 0;
    // SAFETY: the same live handle, and `code` is a `u32` this call fills.
    // `ShellExecuteExW` opened the handle with the access this query needs,
    // and the process has exited, so what comes back is the real exit code
    // rather than the still-running placeholder.
    let read = unsafe { GetExitCodeProcess(process.as_raw_handle() as _, &mut code) };
    if read == 0 {
        return Outcome::failed(format!(
            "Windows Installer finished but Brokey could not read what it answered: {}. \
             Look in Add or remove programs to see whether Brokey {version} is there.",
            std::io::Error::last_os_error()
        ));
    }
    outcome_of(code, version)
}

/// Windows Installer's exit code, as a sentence.
///
/// The six named here are the ones somebody installing Brokey actually meets;
/// everything else is the number and a place to look, which is more use than
/// a guess at what it meant.
#[cfg(windows)]
fn outcome_of(code: u32, version: &str) -> Outcome {
    /// Installed, and Windows wants a restart before everything is in place.
    const REBOOT_REQUIRED: u32 = 3010;
    /// Stopped rather than finished.
    const USER_EXIT: u32 = 1602;
    /// Another install is holding the Windows Installer service.
    const ALREADY_RUNNING: u32 = 1618;
    /// The package will not replace the version already on the machine.
    const WRONG_VERSION: u32 = 1638;
    /// The rights the prompt was for did not arrive.
    const NO_PRIVILEGE: u32 = 1925;

    match code {
        0 => Outcome::installed(format!(
            "Brokey {version} is installed. It is in the Start menu."
        )),
        REBOOT_REQUIRED => Outcome::installed(format!(
            "Brokey {version} is installed. Windows asked for a restart to finish putting it \
             in place, so restart when it suits you."
        )),
        USER_EXIT => Outcome::failed(
            "The installation was stopped before it finished, so nothing was changed. Run the \
             setup executable again to install Brokey."
                .to_string(),
        ),
        ALREADY_RUNNING => Outcome::failed(
            "Another installation is already running on this machine. Wait for it to finish, \
             then run the setup executable again."
                .to_string(),
        ),
        WRONG_VERSION => Outcome::failed(format!(
            "Another version of Brokey is already installed and this package will not replace \
             it. Remove the installed copy through Add or remove programs, then run the setup \
             executable for {version} again."
        )),
        NO_PRIVILEGE => Outcome::failed(
            "Windows Installer did not get the administrator rights it needs, so Brokey is not \
             installed. Brokey installs for everyone on this machine. Run the setup executable \
             again and choose Yes when Windows asks."
                .to_string(),
        ),
        other => Outcome::failed(format!(
            "Windows Installer stopped with code {other}, so Brokey is not installed. Run the \
             setup executable again; if it stops again, Windows Installer writes the reason \
             into the Application event log as an MsiInstaller entry."
        )),
    }
}

/// Encodes a Rust string as a null-terminated UTF-16 buffer, the form every
/// wide Win32 entry point in this module expects.
#[cfg(windows)]
fn wide_null(s: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// **The ordinary case, and the only test that goes near the install
    /// path.** A file with no payload is every `brokey.exe` on every machine,
    /// so the answer has to be a sentence rather than a panic.
    ///
    /// The file this is given is one the test wrote, holding no magic bytes,
    /// so `payload::read` answers `None` and the run stops on that line.
    /// Nothing below it is reached: no package is staged, msiexec is never
    /// started, no prompt appears and nothing is installed. That is the whole
    /// reason the decision is a function of its own rather than the inside of
    /// `install`.
    #[test]
    fn a_copy_with_no_package_says_so_and_installs_nothing() {
        let folder = std::env::temp_dir().join(format!("brokey-setup-test-{}", std::process::id()));
        std::fs::create_dir_all(&folder).expect("the scratch folder");
        let plain = folder.join("brokey.exe");
        std::fs::write(&plain, b"MZ this is an ordinary program").expect("write the plain file");

        let outcome = unpack_and_install(&plain);
        assert_eq!(outcome.sentence, NO_PACKAGE);
        assert_eq!(outcome.code, 1, "a copy that cannot install answers 1");
        assert!(
            outcome.sentence.contains("brokey-setup-"),
            "the sentence names the file that does carry the installer"
        );

        let _ = std::fs::remove_dir_all(&folder);
    }

    /// The title is read off the file in front of the user, so a release
    /// renamed after it was downloaded says what it was renamed to.
    #[test]
    fn the_version_comes_off_the_file_name() {
        assert_eq!(
            version_from_stem("brokey-setup-0.1.4-x64"),
            Some("0.1.4".to_string())
        );
        assert_eq!(
            version_from_stem("brokey-setup-1.2.3-rc1-arm64"),
            Some("1.2.3".to_string())
        );
        // Neither an architecture nor a word is a version.
        assert_eq!(version_from_stem("brokey-setup-x64"), None);
        assert_eq!(version_from_stem("installer"), None);
        // A stem with no version falls back to what this binary was built as,
        // rather than leaving the title with no version in it at all.
        assert_eq!(
            version_in(Path::new(r"C:\Users\someone\Downloads\installer.exe")),
            env!("CARGO_PKG_VERSION")
        );
    }

    /// Two codes mean Brokey is on the machine and every other means it is
    /// not, and the exit code this process returns is that answer. A sentence
    /// saying one thing and a code saying the other is the failure this
    /// pins.
    #[test]
    fn only_a_finished_install_answers_zero() {
        assert_eq!(outcome_of(0, "0.1.4").code, 0);
        assert_eq!(outcome_of(3010, "0.1.4").code, 0);
        for stopped in [1602u32, 1618, 1638, 1925, 1603, 1] {
            let outcome = outcome_of(stopped, "0.1.4");
            assert_eq!(outcome.code, 1, "{stopped} is not an install");
            assert!(
                outcome.sentence.ends_with('.'),
                "{stopped} answers a sentence"
            );
        }
    }
}
