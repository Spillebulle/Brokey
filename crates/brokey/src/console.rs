//! The terminal that started a text-mode run, on Windows.
//!
//! A release `brokey.exe` is a windows-subsystem binary, so that the window
//! and the setup window open with nothing behind them. The price is that
//! Windows gives such a process no console at all: its standard output goes
//! nowhere, and `brokey search steam` typed at a prompt would print not one
//! line. This is the other half of that trade. A text-mode run attaches to
//! the console of the terminal that started it and points the standard
//! handles at it, so the table lands where it was asked for.
//!
//! **A redirected run must not be touched.** `brokey --version | cat` and
//! `brokey --version > file.txt` are started with a pipe or a file already
//! on standard output, and replacing that with the console would send the
//! output to the screen and leave the pipe empty. So each handle is read
//! **before** the attach and put back afterwards if it led anywhere, and only
//! a handle that led nowhere is given the console. Reading first is the part
//! that matters: `AttachConsole` may set the standard handles to the console's
//! own, so a pipe asked for after the attach can already have been replaced,
//! and the redirection would be lost without a word. This is the case these
//! attachments most often break, and it is why [`the_handle_is_usable`] and
//! the order of [`attach_to_the_terminal_that_started_this`] are what they
//! are.
//!
//! Nothing here runs on Linux: the whole module is `#[cfg(windows)]` at its
//! one use in `main.rs`.

use std::fs::{File, OpenOptions};
use std::os::windows::io::{FromRawHandle, IntoRawHandle, RawHandle};

use windows_sys::Win32::Foundation::{GetHandleInformation, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Console::{
    ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_ERROR_HANDLE, STD_HANDLE,
    STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
};

/// Take the console of the terminal that started this process, so that
/// `println!` reaches it.
///
/// Silent when there is no such console: a run from the Start menu, from
/// Explorer, or from a terminal that has none of its own, such as the MSYS
/// shells, whose pipes are inherited and already work. In each of those cases
/// the standard handles are left exactly as they were found.
///
/// Call it once, before anything prints. It is only meaningful for a text-mode
/// run; the window has nothing to say to a terminal.
pub fn attach_to_the_terminal_that_started_this() {
    let wanted = [
        (STD_OUTPUT_HANDLE, Stream::Output),
        (STD_ERROR_HANDLE, Stream::Output),
        (STD_INPUT_HANDLE, Stream::Input),
    ];
    // Read them first. The attach can replace them, and what this process was
    // started with is what the user asked for.
    // SAFETY: three reads of this process's own standard handles.
    let before = wanted.map(|(which, _)| unsafe { GetStdHandle(which) });

    // SAFETY: a call with no arguments but a constant, which either attaches
    // this process to the parent's console or fails and changes nothing.
    // Failure is the ordinary case for a double-click and is not an error.
    if unsafe { AttachConsole(ATTACH_PARENT_PROCESS) } == 0 {
        return;
    }

    for ((which, stream), started_with) in wanted.into_iter().zip(before) {
        point_at_the_console(which, stream, started_with);
    }
}

/// Which of the console's two files a standard handle wants.
#[derive(Clone, Copy)]
enum Stream {
    Output,
    Input,
}

impl Stream {
    /// The name the console answers to. These are not files on a disk: the
    /// console device gives a handle to the active screen buffer for
    /// `CONOUT$` and to the input buffer for `CONIN$`.
    fn name(self) -> &'static str {
        match self {
            Stream::Output => "CONOUT$",
            Stream::Input => "CONIN$",
        }
    }
}

/// Give one standard handle the console, unless it already has somewhere to
/// go.
///
/// `started_with` is what the handle was before the attach, and it wins
/// whenever it is usable, because that is the pipe or the file the user asked
/// for and the attach may since have replaced it. Only a handle that led
/// nowhere, which is what a windows-subsystem process is given at a bare
/// prompt, is given the console.
fn point_at_the_console(which: STD_HANDLE, stream: Stream, started_with: HANDLE) {
    if the_handle_is_usable(started_with) {
        // SAFETY: putting back a handle this process started with and still
        // owns. It is a no-op when the attach left it alone.
        unsafe { SetStdHandle(which, started_with) };
        return;
    }
    // SAFETY: a read of one of this process's own standard handles. The
    // attach may have given it the console already, in which case there is
    // nothing left to do.
    if the_handle_is_usable(unsafe { GetStdHandle(which) }) {
        return;
    }
    let Ok(file) = OpenOptions::new()
        .read(true)
        .write(true)
        .open(stream.name())
    else {
        return;
    };
    let handle = file.into_raw_handle();
    // SAFETY: `handle` is a live handle this process owns, of the kind
    // `SetStdHandle` takes. Ownership of it passes to the process's standard
    // handle, which lasts as long as the process, so it is deliberately never
    // closed: `into_raw_handle` is what stops the `File` closing it here.
    if unsafe { SetStdHandle(which, handle as HANDLE) } == 0 {
        // SAFETY: the handle was not taken, so this is still the only owner
        // of it and closing it is what a dropped `File` would have done.
        drop(unsafe { File::from_raw_handle(handle as RawHandle) });
    }
}

/// Whether a standard handle leads somewhere this process can write.
///
/// Null is what a windows-subsystem process is given when nothing was
/// redirected, and `INVALID_HANDLE_VALUE` is what a failed lookup returns.
/// Anything else is asked of the handle table, because a process can also
/// inherit a handle value that is no longer, or never was, a handle of its
/// own; `GetHandleInformation` is the cheapest question that tells the
/// difference between that and a real pipe or file.
fn the_handle_is_usable(handle: HANDLE) -> bool {
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return false;
    }
    let mut flags = 0u32;
    // SAFETY: a read-only question about a handle value, which answers false
    // rather than misbehaving when the value is not a handle of this process.
    unsafe { GetHandleInformation(handle, &mut flags) != 0 }
}

#[cfg(test)]
mod tests {
    use super::{attach_to_the_terminal_that_started_this, the_handle_is_usable};
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};

    /// The two answers that mean "this process was given nowhere to print",
    /// which are the only two that may lead to a handle being replaced.
    #[test]
    fn nothing_and_the_invalid_handle_are_not_somewhere_to_print() {
        assert!(!the_handle_is_usable(std::ptr::null_mut()));
        assert!(!the_handle_is_usable(INVALID_HANDLE_VALUE));
    }

    /// The half that keeps a redirected run working: a real handle is usable
    /// and must therefore be left alone. A file stands in for the pipe of
    /// `brokey --version | cat`, which is the same question to the handle
    /// table.
    #[test]
    fn a_real_open_file_is_somewhere_to_print() {
        let file =
            std::fs::File::create(std::env::temp_dir().join("brokey-console-handle-test.txt"))
                .expect("a file in the temporary directory");
        assert!(the_handle_is_usable(file.as_raw_handle() as HANDLE));
    }

    /// Attaching when there is already a console, which is what a test binary
    /// has, does nothing and breaks nothing. `cargo test` capturing this line
    /// is the assertion.
    #[test]
    fn attaching_when_there_is_already_a_console_leaves_printing_alone() {
        attach_to_the_terminal_that_started_this();
        println!("The test binary can still print after attaching.");
    }
}
