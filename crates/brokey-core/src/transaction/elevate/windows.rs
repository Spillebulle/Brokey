//! The Windows end of the privilege seam.
//!
//! `ShellExecuteEx` is the only call that elevates and it cannot redirect
//! standard streams; `CreateProcess` can redirect them and cannot elevate.
//! So the unelevated side listens on a named pipe first, hands the name to
//! the elevated helper on its command line, and the helper connects back.
//! The pipe's DACL admits this user and the Administrators group and
//! nobody else, so nothing else on the machine can answer in the helper's
//! place or listen to what passes.

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, IntoRawHandle, OwnedHandle, RawHandle};
use std::path::Path;
use std::ptr;

use windows_sys::Win32::Foundation::{
    ERROR_CANCELLED, ERROR_PIPE_CONNECTED, HANDLE, HLOCAL, INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetExitCodeProcess, INFINITE, OpenProcessToken, WaitForSingleObject,
};
use windows_sys::Win32::UI::Shell::{
    SEE_MASK_FLAG_NO_UI, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

use super::Elevated;

/// A pipe name no other run will choose. The process id alone is not
/// enough: one session can run two plans.
pub fn pipe_name() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(r"\\.\pipe\brokey-{}-{n}-{nanos}", std::process::id())
}

/// `D:` then one allow-all entry for this user and one for the local
/// Administrators group, which is the `BA` alias. A DACL that names nobody
/// else denies everybody else, which is the point of writing one.
pub fn sddl_for_current_user() -> io::Result<String> {
    let sid = current_user_sid()?;
    Ok(format!("D:(A;;GA;;;{sid})(A;;GA;;;BA)"))
}

/// Encodes a Rust string as a null-terminated UTF-16 buffer, the form every
/// wide Win32 entry point below expects.
fn wide_null(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Reads a null-terminated UTF-16 string out of a raw pointer such as
/// `ConvertSidToStringSidW` hands back.
fn string_from_wide_ptr(ptr: *const u16) -> String {
    // SAFETY: `ptr` is non-null and null-terminated, as guaranteed by the
    // Win32 convention that produced it (checked by the caller before this
    // is reached); this scans to the terminator and reads no further.
    let slice = unsafe {
        let mut len = 0usize;
        while *ptr.add(len) != 0 {
            len += 1;
        }
        std::slice::from_raw_parts(ptr, len)
    };
    String::from_utf16_lossy(slice)
}

/// The current process's user SID, rendered the way `ConvertSidToStringSidW`
/// does (`S-1-5-21-...`). This is what names "this user" in the pipe's DACL.
fn current_user_sid() -> io::Result<String> {
    // SAFETY: GetCurrentProcess takes no arguments and returns a pseudo
    // handle that is always valid and never needs closing.
    let process = unsafe { GetCurrentProcess() };

    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: `process` is the valid pseudo handle above and `token` is a
    // writable out-parameter; on success it is set to a handle this call
    // gives us sole ownership of.
    let opened = unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) };
    if opened == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `token` was just set to a fresh, uniquely owned handle by the
    // successful call above.
    let token = unsafe { OwnedHandle::from_raw_handle(token as RawHandle) };

    // The first call asks only for the required buffer size: a null buffer
    // and zero length always make it fail, and the failure is not an error,
    // only `len` (which Windows sets regardless) is used.
    let mut len: u32 = 0;
    // SAFETY: a null buffer and zero length are the documented way to ask
    // `GetTokenInformation` for the size it needs, which it writes to `len`
    // whether or not the call itself reports success.
    unsafe {
        GetTokenInformation(
            token.as_raw_handle() as HANDLE,
            TokenUser,
            ptr::null_mut(),
            0,
            &mut len,
        )
    };
    if len == 0 {
        return Err(io::Error::last_os_error());
    }

    // A `Vec<u8>` is only 1-byte aligned by its type, but `TOKEN_USER`
    // contains a pointer and needs 8-byte alignment to read from. A
    // `Vec<u64>`, sized in words rather than bytes, is 8-byte aligned by
    // construction, so casting its pointer to `*const TOKEN_USER` below is
    // sound regardless of what the allocator happens to hand back.
    let mut buffer = vec![0u64; (len as usize).div_ceil(8)];
    // SAFETY: `buffer` holds at least `len` bytes (rounded up to whole
    // `u64`s), the size the call above reported it needs, so this fills it
    // with one `TOKEN_USER` without overrunning it.
    let filled = unsafe {
        GetTokenInformation(
            token.as_raw_handle() as HANDLE,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            len,
            &mut len,
        )
    };
    if filled == 0 {
        return Err(io::Error::last_os_error());
    }

    // SAFETY: `buffer` now holds a `TOKEN_USER` written by the call above,
    // which is why it was sized to at least `size_of::<TOKEN_USER>()`
    // bytes, and it is 8-byte aligned because it is a `Vec<u64>`, which is
    // what a pointer-containing `TOKEN_USER` requires.
    let sid = unsafe { (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid };

    let mut sid_string: windows_sys::core::PWSTR = ptr::null_mut();
    // SAFETY: `sid` points inside `buffer`, which is still alive, and
    // `sid_string` is a valid out-parameter; on success it is set to memory
    // this call allocates, which is freed with `LocalFree` below.
    let converted = unsafe { ConvertSidToStringSidW(sid, &mut sid_string) };
    if converted == 0 {
        return Err(io::Error::last_os_error());
    }

    let result = string_from_wide_ptr(sid_string);

    // SAFETY: `sid_string` was allocated by `ConvertSidToStringSidW` above
    // and this frees it exactly once, now that its contents are copied.
    unsafe { LocalFree(sid_string as HLOCAL) };

    Ok(result)
}

/// One named pipe, listening for the one client this run expects.
pub struct PipeServer {
    /// `None` once `accept` has handed the connected handle to a `File`.
    handle: Option<OwnedHandle>,
}

/// Builds the pipe's DACL, creates the pipe, and returns a server ready to
/// accept the one client this run expects.
pub fn listen(name: &str) -> io::Result<PipeServer> {
    let sddl = sddl_for_current_user()?;
    let sddl_wide = wide_null(&sddl);
    let name_wide = wide_null(name);

    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: `sddl_wide` is a valid null-terminated wide string and
    // `descriptor` is a valid out-parameter; on success it is set to memory
    // this call allocates, which is freed with `LocalFree` below.
    let converted = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl_wide.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            ptr::null_mut(),
        )
    };
    if converted == 0 {
        return Err(io::Error::last_os_error());
    }

    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };

    // SAFETY: `name_wide` is a valid null-terminated wide string and
    // `attributes` is a valid `SECURITY_ATTRIBUTES` whose descriptor
    // `CreateNamedPipeW` copies into the pipe object before returning.
    let handle = unsafe {
        CreateNamedPipeW(
            name_wide.as_ptr(),
            PIPE_ACCESS_DUPLEX,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            1,
            4096,
            4096,
            0,
            &attributes,
        )
    };

    // The pipe now holds its own copy of the descriptor (or the call
    // failed and nothing needs it), so this run's copy is freed either way.
    // SAFETY: `descriptor` was allocated by
    // `ConvertStringSecurityDescriptorToSecurityDescriptorW` above and this
    // frees it exactly once.
    unsafe { LocalFree(descriptor as HLOCAL) };

    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }

    // SAFETY: `handle` is a valid, freshly created handle from the
    // successful call above, and nothing else has taken ownership of it.
    let handle = unsafe { OwnedHandle::from_raw_handle(handle as RawHandle) };

    Ok(PipeServer {
        handle: Some(handle),
    })
}

impl PipeServer {
    /// Waits for the helper to connect, then hands back an ordinary stream.
    ///
    /// Treats `ERROR_PIPE_CONNECTED` as success, not failure: a client that
    /// opened the pipe before this call is made gets that error instead of
    /// a zero return, and it means exactly the same thing.
    pub fn accept(&mut self) -> io::Result<std::fs::File> {
        let Some(handle) = self.handle.as_ref() else {
            return Err(io::Error::other(
                "This pipe has already accepted its client. Call listen() again for another connection.",
            ));
        };

        // SAFETY: `handle` is the pipe's handle, valid for the duration of
        // this call because `self.handle` still owns it; a null overlapped
        // pointer requests the blocking form of `ConnectNamedPipe`.
        let connected =
            unsafe { ConnectNamedPipe(handle.as_raw_handle() as HANDLE, ptr::null_mut()) };
        if connected == 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(ERROR_PIPE_CONNECTED as i32) {
                return Err(err);
            }
        }

        // The handle moves from here into the `File`: `self.handle` is
        // `None` from this point, so nothing keeps a copy that could close
        // it a second time.
        let owned = self.handle.take().expect("checked Some above");
        let raw = owned.into_raw_handle();
        // SAFETY: `raw` came from the `OwnedHandle` this `PipeServer` held,
        // which has just given up ownership by converting into it; the new
        // `File` becomes the sole owner and closes it exactly once when
        // dropped.
        let file = unsafe { std::fs::File::from_raw_handle(raw) };
        Ok(file)
    }
}

/// What the elevated helper is started with. Separate from the call that
/// elevates so it can be tested without raising a prompt.
pub fn helper_arguments(pipe: &str) -> Vec<String> {
    vec!["run".to_string(), "--pipe".to_string(), pipe.to_string()]
}

/// `SHELLEXECUTEINFOW::cbSize`, measured. Never a literal: it is 112 bytes
/// on x86-64 and need not be on every target.
pub fn shell_execute_info_size() -> u32 {
    std::mem::size_of::<SHELLEXECUTEINFOW>() as u32
}

/// ShellExecuteEx takes one parameter string, so `helper_arguments` is
/// joined into one before it goes into `lpParameters`. The helper path
/// itself is not part of this: it goes into the discrete `lpFile` field and
/// needs no quoting at all. Nothing this function is actually called on
/// today contains a space, since `helper_arguments` is two literals and a
/// generated pipe name; the quoting is defensive, for whatever a joined
/// parameter string carries next.
fn join_arguments(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            if a.contains(' ') {
                format!("\"{a}\"")
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A running elevated helper, waited on but never killed: see `Elevated::wait`.
pub struct Inner {
    process: OwnedHandle,
}

impl Inner {
    /// Waits for the helper to exit and reports its exit code. There is no
    /// `kill`: a helper in the middle of a step is stopped by writing
    /// `CANCEL_LINE` down the pipe, exactly as Linux writes it to stdin.
    pub fn wait(&mut self) -> io::Result<Option<i32>> {
        // SAFETY: `self.process` owns a valid process handle, and `INFINITE`
        // is a documented timeout value meaning "wait forever"; this blocks
        // until the process exits and touches no memory.
        let waited =
            unsafe { WaitForSingleObject(self.process.as_raw_handle() as HANDLE, INFINITE) };
        if waited == windows_sys::Win32::Foundation::WAIT_FAILED {
            return Err(io::Error::last_os_error());
        }
        let mut code: u32 = 0;
        // SAFETY: `self.process` is the same valid, now-exited process
        // handle and `code` is a writable out-parameter.
        let read = unsafe { GetExitCodeProcess(self.process.as_raw_handle() as HANDLE, &mut code) };
        if read == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Some(code as i32))
    }
}

/// Starts `helper` elevated with the `runas` verb and meets it on a named
/// pipe. The pipe is listened on before the helper is started: the helper
/// connects back the instant it launches, and a pipe that does not yet exist
/// gets it nothing.
pub fn start(helper: &Path, wrapper: &[String]) -> io::Result<Elevated> {
    let _ = wrapper;
    let name = pipe_name();
    let mut server = listen(&name)?;

    let helper_wide = wide_null(&helper.to_string_lossy());
    let verb_wide = wide_null("runas");
    let parameters = join_arguments(&helper_arguments(&name));
    let parameters_wide = wide_null(&parameters);

    let mut info = SHELLEXECUTEINFOW {
        cbSize: shell_execute_info_size(),
        // `NOCLOSEPROCESS` so `hProcess` comes back to wait on;
        // `FLAG_NO_UI` so a failure (a missing helper file, say) is reported
        // through our own `Err` rather than a native Windows error dialog
        // Brokey did not write and cannot style.
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_FLAG_NO_UI,
        lpVerb: verb_wide.as_ptr(),
        lpFile: helper_wide.as_ptr(),
        lpParameters: parameters_wide.as_ptr(),
        nShow: SW_HIDE,
        ..Default::default()
    };

    // SAFETY: `info` is a fully initialised `SHELLEXECUTEINFOW` whose string
    // pointers (`helper_wide`, `verb_wide`, `parameters_wide`) are all still
    // alive in bindings above this call, so none of them is dangling.
    let ok = unsafe { ShellExecuteExW(&mut info) };
    if ok == 0 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(ERROR_CANCELLED as i32) {
            return Err(io::Error::other(
                "You did not allow the change, so nothing was done.",
            ));
        }
        return Err(err);
    }

    // SAFETY: `ShellExecuteExW` succeeded above with `SEE_MASK_NOCLOSEPROCESS`
    // set, which is documented to fill `hProcess` with a fresh handle this
    // call now owns exclusively.
    let process = unsafe { OwnedHandle::from_raw_handle(info.hProcess as RawHandle) };
    // Built immediately so every error path below, not just the success
    // path, owns the process and can wait on it rather than abandoning it.
    let mut inner = Inner { process };

    let stream = match server.accept() {
        Ok(stream) => stream,
        Err(e) => {
            // `server` is dropped when this returns, closing the pipe before
            // the helper ever connects to it; its connect then fails and it
            // exits on its own, so this wait is bounded, not indefinite.
            let _ = inner.wait();
            return Err(e);
        }
    };
    let reader = match stream.try_clone() {
        Ok(reader) => reader,
        Err(e) => {
            // `stream` is dropped when this returns, closing the pipe the
            // helper is already connected to; its next read or write then
            // fails and it exits on its own, so this wait is bounded too.
            let _ = inner.wait();
            return Err(e);
        }
    };
    let lines = crate::transaction::runner::stream_lines_from(reader);

    Ok(Elevated {
        input: Some(Box::new(stream)),
        lines,
        inner,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two runs never collide. A stale pipe of the same name would make the
    /// second run fail to listen, and one session can run two plans.
    #[test]
    fn every_pipe_has_its_own_name() {
        let a = pipe_name();
        let b = pipe_name();
        assert_ne!(a, b);
        assert!(a.starts_with(r"\\.\pipe\brokey-"), "{a}");
    }

    /// The current user's SID, read independently of `current_user_sid` so
    /// the test below proves the descriptor names the real SID rather than
    /// merely agreeing with itself. Shells out to the real `whoami.exe`
    /// (found via `%SystemRoot%`, not whatever `whoami` a Git Bash or other
    /// shell might have put earlier on `PATH`, which does not understand
    /// `/user`), matching this repo's existing practice of checking parsers
    /// against real programs (`vercmp`, `desktop-file-validate`).
    fn expected_sid() -> String {
        let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string());
        let whoami = format!(r"{system_root}\System32\whoami.exe");
        let output = std::process::Command::new(&whoami)
            .args(["/user", "/fo", "csv", "/nh"])
            .output()
            .unwrap_or_else(|e| panic!("{whoami} runs: {e}"));
        assert!(
            output.status.success(),
            "{whoami} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        // CSV, no header: "DOMAIN\user","S-1-5-21-...". The SID is the last
        // field and contains no comma, so splitting on the last comma and
        // trimming the quotes is exact.
        let text = String::from_utf8_lossy(&output.stdout);
        text.trim()
            .rsplit(',')
            .next()
            .expect("whoami.exe prints a SID column")
            .trim_matches('"')
            .to_string()
    }

    /// The descriptor names this user and the Administrators group and
    /// nobody else. `BA` is the Administrators alias; the user's SID is
    /// checked against `whoami.exe`'s own answer, not against
    /// `current_user_sid` again, which would only prove that function
    /// equals itself. The negative assertions cover both the well-known
    /// aliases for Everyone/Anonymous (`WD`/`AN`) and the raw SIDs those
    /// aliases stand for (`S-1-1-0`/`S-1-5-7`): the code builds the SDDL
    /// itself today, so neither form appears, but a future edit could add
    /// either and only checking one form would miss it.
    #[test]
    fn the_descriptor_admits_this_user_and_administrators() {
        let sddl = sddl_for_current_user().expect("this user has a SID");
        assert!(sddl.starts_with("D:"), "{sddl}");
        assert!(sddl.contains("(A;;GA;;;BA)"), "{sddl}");
        let sid = expected_sid();
        assert!(
            sddl.contains(&format!("(A;;GA;;;{sid})")),
            "expected the real SID {sid} in {sddl}"
        );
        assert!(!sddl.contains(";;;WD)"), "everyone can open it: {sddl}");
        assert!(!sddl.contains(";;;AN)"), "anonymous can open it: {sddl}");
        assert!(
            !sddl.contains(";;;S-1-1-0)"),
            "everyone as a raw SID can open it: {sddl}"
        );
        assert!(
            !sddl.contains(";;;S-1-5-7)"),
            "anonymous as a raw SID can open it: {sddl}"
        );
    }

    /// The descriptor is not merely a plausible string: Windows parses it
    /// and creates the pipe, or this fails.
    #[test]
    fn windows_accepts_the_descriptor() {
        let name = pipe_name();
        let server = listen(&name).expect("the pipe is created");
        drop(server);
    }

    /// What the elevated helper is started with. The pipe name has to reach
    /// it: it is the only way back. This is checked without elevating, because
    /// a real run raises a UAC prompt and no test may do that.
    #[test]
    fn the_helper_is_told_where_to_connect() {
        assert_eq!(
            helper_arguments(r"\\.\pipe\brokey-1-0-2"),
            vec![
                "run".to_string(),
                "--pipe".to_string(),
                r"\\.\pipe\brokey-1-0-2".to_string(),
            ]
        );
    }

    /// `cbSize` must be the real size of the struct. A wrong one makes
    /// ShellExecuteEx fail with a message that says nothing about size.
    #[test]
    fn the_shell_execute_struct_is_measured_not_guessed() {
        assert_eq!(
            shell_execute_info_size() as usize,
            std::mem::size_of::<windows_sys::Win32::UI::Shell::SHELLEXECUTEINFOW>()
        );
    }

    /// The arguments are one string to ShellExecuteEx, so a path with a space
    /// in it must survive being joined. Every real helper path has one.
    #[test]
    fn an_argument_with_a_space_is_quoted() {
        let joined = join_arguments(&["run".to_string(), r"C:\Program Files\x".to_string()]);
        assert_eq!(joined, r#"run "C:\Program Files\x""#);
    }

    /// A client connects and the two sides speak. This is the whole
    /// contract the helper relies on.
    #[test]
    fn a_client_can_connect_and_be_read() {
        use std::io::{BufRead, BufReader, Write};
        let name = pipe_name();
        let mut server = listen(&name).expect("the pipe is created");
        let client_name = name.clone();
        // The server is already listening: `listen` returns after
        // CreateNamedPipeW, and a client may open a pipe that has no
        // pending ConnectNamedPipe.
        let client = std::thread::spawn(move || {
            let mut f = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&client_name)
                .expect("the client opens the pipe");
            writeln!(f, "from the helper").expect("the client writes");
            f.flush().expect("the client flushes");
        });
        let stream = server.accept().expect("the client connects");
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).expect("a line arrives");
        assert_eq!(line.trim_end(), "from the helper");
        client.join().expect("the client thread finished");
    }
}
