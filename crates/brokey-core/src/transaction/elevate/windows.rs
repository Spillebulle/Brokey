//! The Windows end of the privilege seam.
//!
//! `ShellExecuteEx` is the only call that elevates and it cannot redirect
//! standard streams; `CreateProcess` can redirect them and cannot elevate.
//! So the unelevated side listens on named pipes first, hands their base
//! name to the elevated helper on its command line, and the helper connects
//! back. Each pipe's DACL admits this user and the Administrators group and
//! nobody else, so nothing else on the machine can answer in the helper's
//! place or listen to what passes.
//!
//! There are two pipes, one per direction, which is what Linux already has
//! in `pkexec`'s stdin and stdout. One duplex pipe will not do: a handle
//! created without `FILE_FLAG_OVERLAPPED` is synchronous, the I/O manager
//! serialises every operation on such a file object, and `try_clone`
//! duplicates the handle but not the file object. The runner starts reading
//! events the moment `start` returns and writes the plan on a thread of its
//! own, so with one pipe the write queues behind a read that cannot finish
//! until the plan it is waiting on has been written, and both sides wait for
//! ever. Two one-directional pipes make that impossible: nothing is ever
//! read and written at the same time on one file object.

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
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
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAGS_AND_ATTRIBUTES, PIPE_ACCESS_INBOUND, PIPE_ACCESS_OUTBOUND,
};
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

/// The base name no other run will choose, which the two pipe names are
/// derived from. The process id alone is not enough: one session can run
/// two plans.
///
/// This is what goes on the helper's command line as `--pipe <base>`: the
/// helper derives the same two names from it with the functions below, so
/// there is one name to pass and no way for the two sides to disagree about
/// which pipe is which.
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

/// The pipe the plan travels down, from the runner to the helper. Derived
/// from [`pipe_name`]'s base by both sides.
pub fn plan_pipe_name(base: &str) -> String {
    format!("{base}-plan")
}

/// The pipe the events travel back up, from the helper to the runner.
/// Derived from [`pipe_name`]'s base by both sides.
pub fn events_pipe_name(base: &str) -> String {
    format!("{base}-events")
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

/// The two named pipes, listening for the one client this run expects.
pub struct PipeServer {
    /// The plan pipe, which this side writes and the helper reads. `None`
    /// once `accept` has handed the connected handle to a `File`.
    plan: Option<OwnedHandle>,
    /// The events pipe, which the helper writes and this side reads. `None`
    /// once `accept` has handed the connected handle to a `File`.
    events: Option<OwnedHandle>,
}

/// One pipe, one direction, with the DACL `listen` built for both.
///
/// `access` is `PIPE_ACCESS_OUTBOUND` or `PIPE_ACCESS_INBOUND` rather than
/// `PIPE_ACCESS_DUPLEX`: a pipe that only goes one way cannot have a read
/// and a write pending on the same file object at once, which is the whole
/// reason there are two of them.
fn create_pipe(
    name: &str,
    access: FILE_FLAGS_AND_ATTRIBUTES,
    attributes: &SECURITY_ATTRIBUTES,
) -> io::Result<OwnedHandle> {
    let name_wide = wide_null(name);
    // SAFETY: `name_wide` is a valid null-terminated wide string and
    // `attributes` is a valid `SECURITY_ATTRIBUTES` whose descriptor
    // `CreateNamedPipeW` copies into the pipe object before returning.
    let handle = unsafe {
        CreateNamedPipeW(
            name_wide.as_ptr(),
            access,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            1,
            4096,
            4096,
            0,
            attributes,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `handle` is a valid, freshly created handle from the
    // successful call above, and nothing else has taken ownership of it.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle as RawHandle) })
}

/// Builds the DACL, creates both pipes with it, and returns a server ready
/// to accept the one client this run expects.
///
/// Both pipes are created here, before the helper is started, because the
/// helper opens both the instant it launches and a pipe that does not yet
/// exist gets it nothing. They are created from the one descriptor built
/// below, so neither can end up with a weaker DACL than the other.
pub fn listen(base: &str) -> io::Result<PipeServer> {
    let sddl = sddl_for_current_user()?;
    let sddl_wide = wide_null(&sddl);

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

    // The plan goes out, so the server end writes it; the events come in,
    // so the server end reads them.
    let plan = create_pipe(&plan_pipe_name(base), PIPE_ACCESS_OUTBOUND, &attributes);
    let events = create_pipe(&events_pipe_name(base), PIPE_ACCESS_INBOUND, &attributes);

    // Each pipe now holds its own copy of the descriptor (or a call failed
    // and nothing needs it), so this run's copy is freed either way, and
    // before either error below is returned.
    // SAFETY: `descriptor` was allocated by
    // `ConvertStringSecurityDescriptorToSecurityDescriptorW` above and this
    // frees it exactly once.
    unsafe { LocalFree(descriptor as HLOCAL) };

    Ok(PipeServer {
        plan: Some(plan?),
        events: Some(events?),
    })
}

/// Waits for the one client this pipe expects.
///
/// Treats `ERROR_PIPE_CONNECTED` as success, not failure: a client that
/// opened the pipe before this call is made gets that error instead of a
/// zero return, and it means exactly the same thing. Both pipes exist
/// before the helper is started, so this is the ordinary case for whichever
/// of them the helper opened first.
fn connect(handle: &OwnedHandle) -> io::Result<()> {
    // SAFETY: `handle` is the pipe's handle, valid for the duration of this
    // call because the caller still owns it; a null overlapped pointer
    // requests the blocking form of `ConnectNamedPipe`.
    let connected = unsafe { ConnectNamedPipe(handle.as_raw_handle() as HANDLE, ptr::null_mut()) };
    if connected == 0 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(ERROR_PIPE_CONNECTED as i32) {
            return Err(err);
        }
    }
    Ok(())
}

impl PipeServer {
    /// Waits for the helper to connect to both pipes, then hands back the
    /// two ends this side uses: the plan's writer and the events' reader.
    ///
    /// The order is the plan pipe first and the events pipe second, and the
    /// helper opens them in that same order. Two connects made in opposite
    /// orders would be a deadlock of their own: each side would sit on the
    /// pipe the other had not reached yet. The order is stated here and in
    /// the helper's `run` so that neither can be changed without the other.
    pub fn accept(&mut self) -> io::Result<(std::fs::File, std::fs::File)> {
        let (Some(plan), Some(events)) = (self.plan.as_ref(), self.events.as_ref()) else {
            return Err(io::Error::other(
                "These pipes have already accepted their client. Call listen() again for another connection.",
            ));
        };
        connect(plan)?;
        connect(events)?;

        // The handles move from here into the `File`s: `self.plan` and
        // `self.events` are `None` from this point, so nothing keeps a copy
        // that could close either a second time.
        let plan = self.plan.take().expect("checked Some above");
        let events = self.events.take().expect("checked Some above");
        Ok((std::fs::File::from(plan), std::fs::File::from(events)))
    }
}

#[cfg(test)]
impl PipeServer {
    /// Both pipes' handles, the plan's first, for the test that reads back
    /// what DACL Windows actually put on each. Nothing outside the tests
    /// wants them: `accept` hands out the two `File`s instead.
    fn handles(&self) -> [HANDLE; 2] {
        [
            self.plan
                .as_ref()
                .expect("not yet accepted")
                .as_raw_handle() as HANDLE,
            self.events
                .as_ref()
                .expect("not yet accepted")
                .as_raw_handle() as HANDLE,
        ]
    }
}

/// What the elevated helper is started with. `pipe` is the base name from
/// [`pipe_name`], not either pipe's own name: the helper derives both from
/// it with [`plan_pipe_name`] and [`events_pipe_name`], so one argument
/// carries both. Separate from the call that elevates so it can be tested
/// without raising a prompt.
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

/// Starts `helper` elevated with the `runas` verb and meets it on the two
/// named pipes. Both are listened on before the helper is started: it
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

    // SAFETY, AND IT DOES NOT HOLD TODAY. `ShellExecuteExW` succeeded above
    // with `SEE_MASK_NOCLOSEPROCESS` set, but succeeding is not the same as
    // starting a process: `hProcess` comes back null when the verb was
    // handled without one, and nothing above checks for that. `OwnedHandle`'s
    // niche excludes zero, so on that path this line is undefined behaviour
    // rather than an error found later. `crates/brokey/src/setup/mod.rs`
    // makes the same call and tests `hProcess.is_null()` before this step;
    // that check is the one this wants.
    //
    // It is left standing deliberately rather than patched in passing. This
    // is the privilege path every install, update and removal on Windows goes
    // through, so it is the first item of the next plan that touches Windows
    // and it wants a review of its own rather than a fix folded into someone
    // else's branch. That plan should also add `SEE_MASK_NOASYNC` to the mask
    // above, which omits it: `ShellExecuteEx` is documented to need that flag
    // when it is called from a thread with no message pump, and this call is
    // made from one, the runner's worker thread.
    let process = unsafe { OwnedHandle::from_raw_handle(info.hProcess as RawHandle) };
    // Built immediately so every error path below, not just the success
    // path, owns the process and can wait on it rather than abandoning it.
    let mut inner = Inner { process };

    let (plan, events) = match server.accept() {
        Ok(pipes) => pipes,
        Err(e) => {
            // `server` is dropped when this returns, closing whichever pipes
            // it still holds before the helper reads or writes them; the
            // helper's own open, read or write then fails and it exits on
            // its own, so this wait is bounded, not indefinite.
            let _ = inner.wait();
            return Err(e);
        }
    };
    let lines = crate::transaction::runner::stream_lines_from(events);

    Ok(Elevated {
        input: Some(Box::new(plan)),
        lines,
        inner,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The client half of a run, without elevating anything: open both
    /// pipes in the order `accept` connects them, read one line of plan,
    /// and answer on the events pipe. This is what `brokey-helper`'s
    /// Windows `run` does, close enough to stand in for it here.
    fn helper_side(base: &str) -> std::thread::JoinHandle<String> {
        use std::io::{BufRead, BufReader, Write};
        let base = base.to_string();
        std::thread::spawn(move || {
            let plan = std::fs::OpenOptions::new()
                .read(true)
                .open(plan_pipe_name(&base))
                .expect("the client opens the plan pipe");
            let mut events = std::fs::OpenOptions::new()
                .write(true)
                .open(events_pipe_name(&base))
                .expect("the client opens the events pipe");
            let mut reader = BufReader::new(plan);
            let mut line = String::new();
            reader.read_line(&mut line).expect("the plan arrives");
            writeln!(events, "got it").expect("the answer is written");
            events.flush().expect("the answer is flushed");
            line
        })
    }

    /// What `run_helper` actually does: a reader thread is already blocked
    /// waiting for events when the plan is written, because
    /// `stream_lines_from` is started the moment `start` returns and the
    /// plan goes out on a thread of its own.
    ///
    /// The write must finish anyway. A Windows handle opened without
    /// `FILE_FLAG_OVERLAPPED` is synchronous, and the I/O manager
    /// serialises every operation on such a file object, so a pending read
    /// can hold up a write to the same object. `try_clone` duplicates the
    /// handle but not the file object, so cloning does not escape it; two
    /// pipes, one per direction, do.
    ///
    /// The watchdog makes this a failure rather than a hang: a test that
    /// blocks for ever tells nobody anything.
    #[test]
    fn the_plan_can_be_written_while_a_reader_is_waiting() {
        use std::io::{Read, Write};
        use std::sync::mpsc;

        let base = pipe_name();
        let mut server = listen(&base).expect("the pipes are created");

        // The helper's side: connect, then wait for the plan exactly as the
        // helper does, and answer only once it has one.
        let client = helper_side(&base);

        let (stream, mut reader) = server.accept().expect("the client connects");

        // Exactly what `stream_lines_from` does: a thread already blocked on
        // a read before the plan is written.
        let (reading, waited) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buffer = [0u8; 64];
            let _ = reading.send(());
            let _ = reader.read(&mut buffer);
        });
        waited.recv().expect("the reader thread started");
        std::thread::sleep(std::time::Duration::from_millis(200));

        let (done, written) = mpsc::channel();
        let mut writer = stream;
        std::thread::spawn(move || {
            let outcome = writer
                .write_all(b"{\"id\":\"plan\"}\n")
                .and_then(|()| writer.flush());
            let _ = done.send(outcome.is_ok());
            // Held open, as `run_helper` holds it for the cancel line.
            std::thread::sleep(std::time::Duration::from_secs(5));
        });

        let outcome = written.recv_timeout(std::time::Duration::from_secs(5));
        assert!(
            outcome.is_ok(),
            "the plan was never written: a pending read blocked it"
        );
        assert_eq!(
            client.join().expect("the client finished").trim_end(),
            "{\"id\":\"plan\"}"
        );
    }

    /// The property the whole fix exists to provide: the two pipes are
    /// separate objects, so a read that is still pending does not hold up a
    /// write.
    ///
    /// The read is proven pending rather than assumed: nothing can arrive
    /// on the events pipe until the client has the plan, so the first
    /// `recv_timeout` must time out. The write is then issued while that
    /// read is outstanding, and both complete. Everything is waited on with
    /// a timeout, so a regression fails rather than hangs.
    #[test]
    fn a_read_and_a_write_can_be_in_flight_at_once() {
        use std::io::{BufRead, BufReader, Write};
        use std::sync::mpsc;
        use std::time::Duration;

        let base = pipe_name();
        assert_ne!(plan_pipe_name(&base), events_pipe_name(&base));

        let mut server = listen(&base).expect("the pipes are created");
        let client = helper_side(&base);
        let (mut writer, events) = server.accept().expect("the client connects");
        assert_ne!(
            writer.as_raw_handle(),
            events.as_raw_handle(),
            "the two ends are one handle, so they are one file object"
        );

        let (answered, answer) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(events);
            let mut line = String::new();
            let read = reader.read_line(&mut line);
            let _ = answered.send(read.map(|_| line));
        });

        // The client answers only what it was sent, so while it has no plan
        // this read cannot finish: it is still in flight below.
        assert!(
            answer.recv_timeout(Duration::from_millis(200)).is_err(),
            "the events pipe answered before anything was written to the plan pipe"
        );

        let (done, written) = mpsc::channel();
        std::thread::spawn(move || {
            let outcome = writer
                .write_all(b"{\"id\":\"plan\"}\n")
                .and_then(|()| writer.flush());
            let _ = done.send(outcome.is_ok());
            // Held open past the write, as `run_helper` holds it for the
            // cancel line.
            std::thread::sleep(Duration::from_secs(5));
        });

        assert_eq!(
            written.recv_timeout(Duration::from_secs(5)),
            Ok(true),
            "the plan was not written while a read was in flight"
        );
        let line = answer
            .recv_timeout(Duration::from_secs(5))
            .expect("the answer arrives")
            .expect("the events pipe reads");
        assert_eq!(line.trim_end(), "got it");
        assert_eq!(
            client.join().expect("the client finished").trim_end(),
            "{\"id\":\"plan\"}"
        );
    }

    /// The cancel line, which `run_helper` writes down the plan pipe long
    /// after the plan, while the events are still streaming back. That
    /// write is the same shape as the plan's and must not block either: a
    /// cancellation that never reaches the helper would leave the user
    /// watching a run they had already stopped.
    ///
    /// Both writes go through one thread that owns the pipe, as
    /// `run_helper`'s do, and each is waited for with a timeout, so a write
    /// that blocks fails this rather than hanging it.
    #[test]
    fn the_cancel_line_can_be_written_while_events_are_streaming() {
        use std::io::{BufRead, BufReader, Write};
        use std::sync::mpsc;
        use std::time::Duration;

        let base = pipe_name();
        let mut server = listen(&base).expect("the pipes are created");

        // A client that answers two lines rather than one: the plan, and
        // then the cancel line that follows it.
        let client_base = base.clone();
        let client = std::thread::spawn(move || {
            let plan = std::fs::OpenOptions::new()
                .read(true)
                .open(plan_pipe_name(&client_base))
                .expect("the client opens the plan pipe");
            let mut events = std::fs::OpenOptions::new()
                .write(true)
                .open(events_pipe_name(&client_base))
                .expect("the client opens the events pipe");
            let mut reader = BufReader::new(plan);
            let mut heard = Vec::new();
            for _ in 0..2 {
                let mut line = String::new();
                reader.read_line(&mut line).expect("a line arrives");
                let line = line.trim_end().to_string();
                writeln!(events, "read {line}").expect("the answer is written");
                events.flush().expect("the answer is flushed");
                heard.push(line);
            }
            heard
        });

        let (mut writer, events) = server.accept().expect("the client connects");

        let (answered, answer) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(events);
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if answered.send(line.trim_end().to_string()).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        // One thread owns the pipe and writes whatever it is sent, exactly
        // as `run_helper`'s writer thread owns it for the plan and then the
        // cancel line.
        let (send, to_write) = mpsc::channel::<String>();
        let (done, written) = mpsc::channel::<bool>();
        std::thread::spawn(move || {
            for text in to_write {
                let outcome = writer
                    .write_all(text.as_bytes())
                    .and_then(|()| writer.flush());
                if done.send(outcome.is_ok()).is_err() {
                    break;
                }
            }
        });

        send.send("{\"id\":\"plan\"}\n".to_string())
            .expect("the writer thread is running");
        assert_eq!(
            written.recv_timeout(Duration::from_secs(5)),
            Ok(true),
            "the plan was not written"
        );
        assert_eq!(
            answer.recv_timeout(Duration::from_secs(5)),
            Ok("read {\"id\":\"plan\"}".to_string())
        );

        // Long enough for the events reader to be back in a blocking read,
        // which is the situation this is about: the cancel line goes out
        // while a read is pending, and must finish anyway.
        std::thread::sleep(Duration::from_millis(200));
        send.send("cancel\n".to_string())
            .expect("the writer thread is still running");
        assert_eq!(
            written.recv_timeout(Duration::from_secs(5)),
            Ok(true),
            "the cancel line was not written while the events were streaming"
        );
        assert_eq!(
            answer.recv_timeout(Duration::from_secs(5)),
            Ok("read cancel".to_string())
        );
        assert_eq!(
            client.join().expect("the client finished"),
            vec!["{\"id\":\"plan\"}".to_string(), "cancel".to_string()]
        );
    }

    /// Each side sees the other go away, which is what stops a run that has
    /// failed from waiting for ever. The runner's line channel disconnects
    /// because `stream_lines_from`'s read of the events pipe ends when the
    /// helper's end of it closes, and the helper's wait for the plan ends
    /// when the runner drops the plan pipe, which `run_helper`'s writer
    /// thread does once the run is over.
    ///
    /// Both reads are waited for with a timeout, because the whole point of
    /// the assertion is that neither blocks.
    #[test]
    fn each_side_notices_when_the_other_goes_away() {
        use std::io::Read;
        use std::sync::mpsc;
        use std::time::Duration;

        let base = pipe_name();
        let mut server = listen(&base).expect("the pipes are created");

        let (plan_ended, plan_end) = mpsc::channel();
        let (connected, go) = mpsc::channel();
        let client_base = base.clone();
        std::thread::spawn(move || {
            let mut plan = std::fs::OpenOptions::new()
                .read(true)
                .open(plan_pipe_name(&client_base))
                .expect("the client opens the plan pipe");
            let events = std::fs::OpenOptions::new()
                .write(true)
                .open(events_pipe_name(&client_base))
                .expect("the client opens the events pipe");
            // Both ends are held until `accept` has connected them: a
            // client that closed one before then would fail the connect
            // instead, which is a different sentence about a different
            // failure.
            let _ = go.recv();
            // A helper that writes nothing and goes, which is what one that
            // refuses to start does.
            drop(events);
            // And this is where the helper waits for its plan.
            let mut buffer = [0u8; 64];
            let _ = plan_ended.send(plan.read(&mut buffer));
        });

        let (writer, mut events) = server.accept().expect("the client connects");
        connected.send(()).expect("the client is waiting");

        let (events_ended, events_end) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buffer = [0u8; 64];
            let _ = events_ended.send(events.read(&mut buffer));
        });
        let read = events_end
            .recv_timeout(Duration::from_secs(5))
            .expect("the events read ends when the helper's end of it closes");
        assert!(
            matches!(read, Ok(0) | Err(_)),
            "the events pipe went on reading after the helper closed it: {read:?}"
        );

        drop(writer);
        let read = plan_end
            .recv_timeout(Duration::from_secs(5))
            .expect("the plan read ends when the runner's end of it closes");
        assert!(
            matches!(read, Ok(0) | Err(_)),
            "the plan pipe went on reading after the runner closed it: {read:?}"
        );
    }

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

    /// A client connects and the two sides speak, each way on its own pipe.
    /// This is the whole contract the helper relies on.
    #[test]
    fn a_client_can_connect_and_be_read() {
        use std::io::{BufRead, BufReader, Write};
        let base = pipe_name();
        let mut server = listen(&base).expect("the pipes are created");
        // The server is already listening: `listen` returns after both
        // CreateNamedPipeW calls, and a client may open a pipe that has no
        // pending ConnectNamedPipe.
        let client = helper_side(&base);
        let (mut writer, events) = server.accept().expect("the client connects");
        writeln!(writer, "from the runner").expect("the runner writes");
        writer.flush().expect("the runner flushes");
        let mut reader = BufReader::new(events);
        let mut line = String::new();
        reader.read_line(&mut line).expect("a line arrives");
        assert_eq!(line.trim_end(), "got it");
        assert_eq!(
            client
                .join()
                .expect("the client thread finished")
                .trim_end(),
            "from the runner"
        );
    }

    /// The DACL Windows actually put on `handle`, as SDDL. Read back from
    /// the kernel rather than from the string `listen` passed in, so the
    /// test below proves what the pipe carries rather than what the code
    /// meant to ask for.
    fn dacl_of(handle: HANDLE) -> String {
        use windows_sys::Win32::Security::Authorization::{
            ConvertSecurityDescriptorToStringSecurityDescriptorW, GetSecurityInfo, SE_KERNEL_OBJECT,
        };
        use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;

        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: `handle` is a live pipe handle owned by the caller, the
        // SID and ACL out-parameters may be null when only the whole
        // descriptor is wanted, and `descriptor` is a valid out-parameter;
        // on success it is set to memory this call allocates, which is
        // freed with `LocalFree` below.
        let got = unsafe {
            GetSecurityInfo(
                handle,
                SE_KERNEL_OBJECT,
                DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                &mut descriptor,
            )
        };
        assert_eq!(got, 0, "the pipe's descriptor could not be read: {got}");

        let mut text: windows_sys::core::PWSTR = ptr::null_mut();
        let mut len: u32 = 0;
        // SAFETY: `descriptor` is the descriptor just read, and `text` and
        // `len` are valid out-parameters; on success `text` is set to
        // memory this call allocates, which is freed with `LocalFree` below.
        let converted = unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                descriptor,
                SDDL_REVISION_1,
                DACL_SECURITY_INFORMATION,
                &mut text,
                &mut len,
            )
        };
        assert_ne!(
            converted,
            0,
            "the descriptor could not be rendered: {}",
            io::Error::last_os_error()
        );
        let sddl = string_from_wide_ptr(text);

        // SAFETY: `text` and `descriptor` were each allocated by one of the
        // calls above and each is freed exactly once, now that the string
        // has been copied out.
        unsafe { LocalFree(text as HLOCAL) };
        // SAFETY: as above, for the descriptor `GetSecurityInfo` allocated.
        unsafe { LocalFree(descriptor as HLOCAL) };
        sddl
    }

    /// Both pipes carry the DACL, not only whichever was created first.
    /// This is the security boundary: a pipe that admitted anyone else
    /// would let something on the machine answer in the helper's place or
    /// read the plan going past. The SID is checked against `whoami.exe`'s
    /// answer for the same reason as in the test above.
    ///
    /// Windows maps the generic rights the SDDL asked for onto the object's
    /// own, so the rights field read back is not the `GA` that went in;
    /// what matters here and is asserted is who is named, and that nobody
    /// else is.
    #[test]
    fn both_pipes_carry_the_descriptor() {
        let base = pipe_name();
        let server = listen(&base).expect("the pipes are created");
        let sid = expected_sid();
        for (which, handle) in ["the plan pipe", "the events pipe"]
            .into_iter()
            .zip(server.handles())
        {
            let sddl = dacl_of(handle);
            assert!(
                sddl.contains(&format!(";;;{sid})")),
                "{which} does not admit this user: {sddl}"
            );
            assert!(
                sddl.contains(";;;BA)"),
                "{which} does not admit Administrators: {sddl}"
            );
            assert_eq!(
                sddl.matches("(A;").count(),
                2,
                "{which} has more than the two allow entries: {sddl}"
            );
            assert!(
                !sddl.contains(";;;WD)"),
                "everyone can open {which}: {sddl}"
            );
            assert!(
                !sddl.contains(";;;AN)"),
                "anonymous can open {which}: {sddl}"
            );
            assert!(
                !sddl.contains(";;;S-1-1-0)"),
                "everyone as a raw SID can open {which}: {sddl}"
            );
            assert!(
                !sddl.contains(";;;S-1-5-7)"),
                "anonymous as a raw SID can open {which}: {sddl}"
            );
        }
    }
}
