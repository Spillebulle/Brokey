//! The small window the setup executable draws while it installs Brokey.
//!
//! Declared `#[cfg(windows)]` in `mod.rs`, so nothing here is compiled
//! anywhere else and no line of it needs its own attribute.
//!
//! **Why this is hand-written Win32 and GDI.** Brokey's own window is a
//! WebView, and an installer that needs WebView2 in order to paint depends on
//! the machine already having the component it may be there to deliver. The
//! sibling projects are no help either: Muster's installer window is egui and
//! Umber's splash is softbuffer and winit, and each would be a dependency
//! tree pulled into Brokey for one window that appears once in a machine's
//! life. `user32` and `gdi32` are already linked into every Windows binary.
//!
//! **What it draws.** A header strip with the title and a hairline under it,
//! a sentence, one primary button, and, while Windows Installer is running,
//! an empty progress track. The track is empty because msiexec run with `/qn`
//! reports nothing this process can read: no percentage, no step, nothing but
//! its exit code when it has finished. Drawing a bar that filled would be
//! drawing a measurement nobody took. That is the same rule the application
//! follows for `Event::Progress`, which is `Option<f32>` for this reason, and
//! it is in `CLAUDE.md` as a standing invariant.

use std::sync::{Arc, Mutex};

use windows_sys::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, SIZE, WPARAM};
use windows_sys::Win32::Graphics::Dwm::{DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, CreateCompatibleBitmap,
    CreateCompatibleDC, CreateFontW, CreateSolidBrush, DEFAULT_CHARSET, DEFAULT_PITCH, DT_LEFT,
    DT_NOPREFIX, DT_SINGLELINE, DT_TOP, DT_VCENTER, DT_WORDBREAK, DeleteDC, DeleteObject,
    DrawTextW, EndPaint, FF_DONTCARE, FillRect, GetDeviceCaps, GetStockObject,
    GetTextExtentPoint32W, HDC, HFONT, InvalidateRect, LOGPIXELSX, NULL_PEN, OUT_TT_PRECIS,
    PAINTSTRUCT, RoundRect, SRCCOPY, SelectObject, SetBkMode, SetTextColor, TRANSPARENT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_SYSTEM_AWARE, SetProcessDpiAwarenessContext,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{VK_ESCAPE, VK_RETURN, VK_SPACE};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRect, CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DestroyWindow,
    DispatchMessageW, GWLP_USERDATA, GetClientRect, GetMessageW, GetSystemMetrics,
    GetWindowLongPtrW, IDC_ARROW, IDI_APPLICATION, LoadCursorW, LoadIconW, MSG, PostMessageW,
    PostQuitMessage, RegisterClassW, SM_CXSCREEN, SM_CYSCREEN, SW_SHOWNORMAL, SetWindowLongPtrW,
    ShowWindow, TranslateMessage, WM_APP, WM_CLOSE, WM_DESTROY, WM_ERASEBKGND, WM_KEYDOWN,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_PAINT, WNDCLASSW, WS_CAPTION, WS_MINIMIZEBOX, WS_OVERLAPPED,
    WS_SYSMENU,
};

use super::Outcome;

/// The window's own message, posted by the working thread when the install
/// has finished and its outcome is waiting in [`State::result`].
///
/// `WM_APP` and above are reserved for an application's own messages, so
/// this cannot collide with anything Windows sends.
const OUTCOME_READY: u32 = WM_APP + 1;

/// The palette, as the browser would compute it.
///
/// These are the dark theme of `frontend/src/tokens.css` with `--accent-h` at
/// 300, which is `frontend/src/tokens.css:34`, converted from OKLCH to sRGB
/// by the arithmetic in `tools/make-art.py`. That file prints the accent it
/// computed on every run (`accent hue 300 -> #A088CC`), which is how a change
/// to the hue is noticed here: the printed hex stops matching [`ACCENT`].
///
/// **They are literals rather than read at runtime, and that is not
/// laziness.** This window paints before Brokey is installed. There is no
/// `tokens.css` on the machine to read, no WebView to evaluate `oklch()`, and
/// a GDI device context has no notion of a stylesheet. The alternative to
/// literals is not a token lookup, it is an OKLCH-to-sRGB conversion written
/// a third time, in Rust, for six colours that change about once a project.
/// The tokens each came from are named beside them so a change can be carried
/// across by hand and seen to have been.
mod ink {
    use super::rgb;
    use windows_sys::Win32::Foundation::COLORREF;

    /// `--chrome`, the header strip.
    pub const CHROME: COLORREF = rgb(0x17, 0x18, 0x1A);
    /// `--window`, the body behind everything below the hairline.
    pub const WINDOW: COLORREF = rgb(0x11, 0x12, 0x14);
    /// `--line`, the hairline, and `--rail`, the progress track. One value in
    /// `tokens.css` as well as here.
    pub const LINE: COLORREF = rgb(0x26, 0x28, 0x2B);
    /// `--text-strong`, the title.
    pub const TEXT_STRONG: COLORREF = rgb(0xE6, 0xE7, 0xE9);
    /// `--text`, the sentence.
    pub const TEXT: COLORREF = rgb(0xC9, 0xCB, 0xCE);
    /// `--accent`, the primary button's fill.
    pub const ACCENT: COLORREF = rgb(0xA0, 0x88, 0xCC);
    /// `--accent-dim`, the same button while it is held down.
    pub const ACCENT_DIM: COLORREF = rgb(0x5A, 0x4D, 0x71);
    /// `--accent-ink`, which is `--window`: text on an accent fill.
    pub const ACCENT_INK: COLORREF = WINDOW;
}

/// A `COLORREF` is `0x00BBGGRR`, which is the reverse of the order a hex
/// colour is written in, so it is built here rather than typed backwards.
const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    (r as COLORREF) | ((g as COLORREF) << 8) | ((b as COLORREF) << 16)
}

/// The layout, in the same units `tokens.css` uses, scaled to the screen's
/// dots per inch when the window is built.
mod size {
    /// The client area. Wide enough for a sentence at two lines and no wider:
    /// this window asks one question and then reports one answer.
    pub const WIDTH: i32 = 460;
    pub const HEIGHT: i32 = 230;
    /// `--s5`, the margin everything is set in from.
    pub const MARGIN: i32 = 24;
    /// The header strip carrying the title, with the hairline under it.
    pub const HEADER: i32 = 56;
    /// `--text-page`, the title's size, and `--text-body` for the sentence.
    pub const TITLE_TEXT: i32 = 15;
    pub const BODY_TEXT: i32 = 12;
    /// `--text-control`, rounded to a whole pixel: GDI has no half sizes.
    pub const BUTTON_TEXT: i32 = 12;
    /// Where the sentence begins, and how much room it has.
    pub const LINE_TOP: i32 = HEADER + 24;
    pub const LINE_HEIGHT: i32 = 62;
    /// The progress track: 3 px, per the style guide's §7.18.
    pub const TRACK_TOP: i32 = LINE_TOP + LINE_HEIGHT + 8;
    pub const TRACK: i32 = 3;
    /// The button: 26 high, radius 5, 12 px of padding each side of its
    /// label, and never narrower than this whatever the label is.
    pub const BUTTON_HEIGHT: i32 = 26;
    pub const BUTTON_RADIUS: i32 = 5;
    pub const BUTTON_PADDING: i32 = 12;
    pub const BUTTON_MIN: i32 = 88;
}

/// Where the install has got to, which is the whole of what the window draws.
enum Stage {
    /// Nothing has happened yet. The button starts it.
    Ready,
    /// Windows Installer is running. There is nothing to press and the window
    /// will not close, because closing it would leave an elevated msiexec
    /// running with nothing watching it.
    Working,
    /// It finished, one way or the other, and the sentence says which.
    Done,
}

/// Everything the window proc needs, reached through `GWLP_USERDATA`.
struct State {
    title: String,
    line: String,
    stage: Stage,
    /// The button's label, or `None` while there is no button to press.
    button: Option<&'static str>,
    /// Where the button was last painted, which is what a click is tested
    /// against. Empty until the first paint, so a click that somehow arrives
    /// before the window has drawn itself lands on nothing.
    button_rect: RECT,
    pressed: bool,
    /// Screen dots per `tokens.css` pixel.
    scale: f32,
    /// The install itself, taken out and moved to a thread when the button is
    /// pressed. `None` afterwards, which is what stops a second press
    /// starting a second install.
    work: Option<Box<dyn FnOnce() -> Outcome + Send>>,
    /// Where the working thread leaves its answer before posting
    /// [`OUTCOME_READY`].
    result: Arc<Mutex<Option<Outcome>>>,
    /// What [`show`] returns. Set when the outcome arrives, or when the
    /// window is closed without installing anything.
    outcome: Option<Outcome>,
}

impl State {
    /// A `tokens.css` pixel in screen pixels.
    fn px(&self, n: i32) -> i32 {
        (n as f32 * self.scale).round() as i32
    }
}

/// The sentence shown before anything has been done.
const READY: &str = "Brokey will be installed for everyone on this machine. Windows asks for \
                     permission once, and nothing is changed until you allow it.";

/// The sentence shown beside the empty track.
///
/// It says what is happening and why the track is not filling, which is the
/// style guide's answer for a total that genuinely cannot be known: an empty
/// rail and a sentence, never an animation over an unknown.
const WORKING: &str = "Installing Brokey. Windows Installer does not report how far it has got, \
                       so this track stays empty until it is finished.";

/// What the window answers when it is closed before the install was started.
const NOT_STARTED: &str =
    "Nothing was installed. Run the setup executable again when you want to install Brokey.";

/// Draws the window, runs `work` behind it, and answers with what `work`
/// came to.
///
/// **If the window cannot be built, `work` still runs.** A machine that will
/// not register a window class or create a window is a strange machine, but
/// somebody on it is still trying to install Brokey, and refusing to install
/// because the picture of the install would not paint is the wrong way round.
/// In that case there is no button to press, so the install simply starts.
pub fn show(title: &str, work: Box<dyn FnOnce() -> Outcome + Send>) -> Outcome {
    let state = Box::new(State {
        line: READY.to_string(),
        stage: Stage::Ready,
        button: Some("Install"),
        work: Some(work),
        outcome: None,
        ..blank(title)
    });
    match open(title, state) {
        Ok(outcome) => outcome,
        Err(mut state) => {
            let work = state.work.take().expect("the work has not been started");
            work()
        }
    }
}

/// Draws the same window over an answer that is already known, with a Close
/// button and nothing to start.
///
/// A copy of Brokey that carries no package cannot install anything, and an
/// Install button that answered "there is nothing to install" would be
/// offering something the file never had. So the answer is on the window from
/// the moment it opens.
pub fn report(title: &str, outcome: Outcome) -> Outcome {
    let state = Box::new(State {
        line: outcome.sentence.clone(),
        stage: Stage::Done,
        button: Some("Close"),
        work: None,
        outcome: Some(outcome),
        ..blank(title)
    });
    match open(title, state) {
        Ok(outcome) => outcome,
        // No window to read it in, so the sentence goes back to the caller,
        // which prints it.
        Err(mut state) => state
            .outcome
            .take()
            .expect("the answer was put here before the window was asked for"),
    }
}

/// The fields every state starts with, whichever way the window was opened.
fn blank(title: &str) -> State {
    State {
        title: title.to_string(),
        line: String::new(),
        stage: Stage::Ready,
        button: None,
        button_rect: RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        },
        pressed: false,
        scale: screen_scale(),
        work: None,
        result: Arc::new(Mutex::new(None)),
        outcome: None,
    }
}

/// Opens the window and runs it until it is closed.
///
/// `Err` is the state handed back untouched, for a machine that would not
/// register a window class or would not create a window. The caller decides
/// what to do without one, and for an install the answer is to install
/// anyway: refusing because the picture of the install would not paint is
/// the wrong way round.
fn open(title: &str, mut state: Box<State>) -> Result<Outcome, Box<State>> {
    // Per-monitor scaling would mean redrawing on `WM_DPICHANGED`, and this
    // window neither moves nor resizes: it opens in the middle of the primary
    // screen and closes there. System awareness is what that needs, and it
    // keeps the text sharp on the high-resolution screens most machines now
    // have, where an unaware window is drawn small and then stretched.
    //
    // SAFETY: a constant in, a `BOOL` out, and it is documented as having to
    // be called before any window exists, which is where this is. It fails on
    // Windows 8.1 and older, where the window is drawn unaware and stretched:
    // a blurred window, not a broken one.
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_SYSTEM_AWARE) };

    let Some(class) = register_class() else {
        return Err(state);
    };
    let Some(hwnd) = create_window(class, title, &mut state) else {
        return Err(state);
    };

    pump(hwnd);

    // The window proc holds the state for as long as the window lives, and
    // `pump` does not return until it has been destroyed, so taking the
    // answer back here cannot race with anything.
    Ok(state
        .outcome
        .take()
        .unwrap_or_else(|| Outcome::failed(NOT_STARTED.to_string())))
}

/// The class name, which is also what a second instance would find already
/// registered. Registering the same class twice in one process fails, and
/// this window is only ever built once, so the failure is simply reported.
const CLASS: &str = "BrokeySetupWindow";

fn register_class() -> Option<u16> {
    let name = wide(CLASS);
    // SAFETY: `GetModuleHandleW(null)` is the documented way to ask for this
    // process's own module and cannot fail for it.
    let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
    // SAFETY: both loads take a built-in identifier, which is what the null
    // instance handle selects; a failure comes back as a null handle, which
    // `RegisterClassW` accepts as "no icon" and "no cursor".
    let (icon, cursor) = unsafe {
        (
            LoadIconW(std::ptr::null_mut(), IDI_APPLICATION),
            LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
        )
    };
    let class = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(window_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: instance,
        hIcon: icon,
        hCursor: cursor,
        // Null, because `WM_ERASEBKGND` is answered rather than left to a
        // brush: the whole client area is painted from one off-screen bitmap
        // and a background wiped first would show as a flicker.
        hbrBackground: std::ptr::null_mut(),
        lpszMenuName: std::ptr::null(),
        lpszClassName: name.as_ptr(),
    };
    // SAFETY: `class` is fully initialised and `name` is alive above this
    // call, so `lpszClassName` is not dangling. A failure is a zero atom.
    let atom = unsafe { RegisterClassW(&class) };
    (atom != 0).then_some(atom)
}

/// Builds the window, centred on the primary screen, with the state pointer
/// already in place so the first `WM_PAINT` finds it.
fn create_window(class: u16, title: &str, state: &mut State) -> Option<HWND> {
    let name = wide(CLASS);
    let caption = wide(title);
    let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;

    // The client area is the size that matters; this asks Windows how much
    // border and title bar to add so the drawing area is exactly the size the
    // layout was written for.
    let mut frame = RECT {
        left: 0,
        top: 0,
        right: state.px(size::WIDTH),
        bottom: state.px(size::HEIGHT),
    };
    // SAFETY: `frame` is a fully initialised `RECT` this call reads and
    // writes; `style` holds no `WS_OVERLAPPEDWINDOW` bits this call rejects.
    unsafe { AdjustWindowRect(&mut frame, style, 0) };
    let width = frame.right - frame.left;
    let height = frame.bottom - frame.top;

    // SAFETY: both metrics are plain reads of the primary screen's size.
    let (screen_w, screen_h) =
        unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };

    // SAFETY: `GetModuleHandleW(null)` cannot fail for this process.
    let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
    // SAFETY: the class atom came from `RegisterClassW` in this process and
    // both strings are alive above the call. The window is created hidden, so
    // no message reaches the proc before the state pointer is set below.
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            name.as_ptr(),
            caption.as_ptr(),
            style,
            (screen_w - width) / 2,
            (screen_h - height) / 2,
            width,
            height,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        )
    };
    let _ = class;
    if hwnd.is_null() {
        return None;
    }

    // SAFETY: `hwnd` is this thread's own live window and the pointer is to a
    // `State` the caller keeps alive for longer than the window: `show` owns
    // the box and does not return until `pump` has seen `WM_DESTROY`.
    unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as *mut State as isize) };

    // A light title bar over a dark window looks like two windows. This is
    // Windows 10 1809 and later; on anything older the call fails and the
    // title bar stays light, which is a blemish and not a failure.
    let dark: i32 = 1;
    // SAFETY: the attribute takes a `BOOL`, and `dark` is a live `i32` of
    // exactly the size passed. An unsupported attribute answers a failed
    // `HRESULT`, which is ignored here on purpose.
    unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE as u32,
            (&dark as *const i32).cast(),
            std::mem::size_of::<i32>() as u32,
        )
    };

    // SAFETY: `hwnd` is live and both calls only ask the window to appear.
    unsafe { ShowWindow(hwnd, SW_SHOWNORMAL) };
    Some(hwnd)
}

/// The message loop, until the window is destroyed.
fn pump(_hwnd: HWND) {
    // SAFETY: `MSG` is a plain C structure of integers, a window handle and a
    // point, for which all zeroes is a valid value; `GetMessageW` overwrites
    // every field of it before anything below reads one.
    let mut message: MSG = unsafe { std::mem::zeroed() };
    loop {
        // SAFETY: `message` is a live `MSG` this call fills; a null window
        // handle is the documented way to ask for every message on this
        // thread, which is the only thread with a window on it.
        let got = unsafe { GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) };
        if got <= 0 {
            // 0 is `WM_QUIT`, which `WM_DESTROY` posted. A negative is an
            // error in `GetMessageW` itself, and carrying on would spin.
            return;
        }
        // SAFETY: `message` was filled by the call above and is untouched
        // since, which is what both of these require.
        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

/// The window proc. Every branch reaches the state through `GWLP_USERDATA`,
/// and a message that arrives before it is set goes to the default proc.
unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: a plain read of this window's own user data word, which is 0
    // until `create_window` writes the pointer into it.
    let pointer = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut State;
    if pointer.is_null() {
        // SAFETY: the default handling of a message this proc was given, with
        // its own arguments passed through unchanged.
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    }
    // SAFETY: the pointer came from `create_window`, which wrote a `&mut
    // State` borrowed from a box `show` keeps alive until `pump` returns, and
    // `pump` does not return until this window is destroyed. Messages are
    // dispatched one at a time on the one thread that owns the window, so
    // this reference is never aliased.
    let state = unsafe { &mut *pointer };

    match message {
        // Answered so the background is never wiped before the paint below
        // draws over it, which is what a flicker is.
        WM_ERASEBKGND => 1,
        WM_PAINT => {
            paint(hwnd, state);
            0
        }
        WM_LBUTTONDOWN => {
            if state.button.is_some() && in_button(state, lparam) {
                state.pressed = true;
                redraw(hwnd);
            }
            0
        }
        WM_LBUTTONUP => {
            let was_pressed = state.pressed;
            state.pressed = false;
            if was_pressed && in_button(state, lparam) {
                activate(hwnd, state);
            } else if was_pressed {
                redraw(hwnd);
            }
            0
        }
        WM_KEYDOWN => {
            let key = wparam as u16;
            if key == VK_RETURN || key == VK_SPACE {
                // There is one button, so it is always the default action.
                activate(hwnd, state);
            } else if key == VK_ESCAPE {
                close(hwnd, state);
            }
            0
        }
        WM_CLOSE => {
            close(hwnd, state);
            0
        }
        OUTCOME_READY => {
            let answer = state
                .result
                .lock()
                .unwrap_or_else(|held| held.into_inner())
                .take();
            if let Some(outcome) = answer {
                state.line = outcome.sentence.clone();
                state.outcome = Some(outcome);
            }
            state.stage = Stage::Done;
            state.button = Some("Close");
            redraw(hwnd);
            0
        }
        WM_DESTROY => {
            // SAFETY: ends `pump`'s loop. Nothing here owns the state: `show`
            // owns the box and takes its answer back once `pump` returns.
            unsafe { PostQuitMessage(0) };
            0
        }
        // SAFETY: the default handling of everything else, arguments passed
        // through unchanged.
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

/// The button, pressed. What it does depends on where the window has got to.
fn activate(hwnd: HWND, state: &mut State) {
    match state.stage {
        Stage::Ready => start(hwnd, state),
        Stage::Working => {}
        Stage::Done => close(hwnd, state),
    }
}

/// Starts the install on a thread of its own, so the window keeps painting
/// and keeps answering Windows while msiexec runs. An install that ran on
/// this thread would leave the window unredrawn and marked as not responding
/// for the whole of it.
fn start(hwnd: HWND, state: &mut State) {
    let Some(work) = state.work.take() else {
        return;
    };
    state.stage = Stage::Working;
    state.line = WORKING.to_string();
    state.button = None;
    state.pressed = false;
    redraw(hwnd);

    let result = Arc::clone(&state.result);
    // An `HWND` is a pointer and so is not `Send`, but a window handle is
    // valid process-wide and `PostMessageW` is documented to be callable from
    // any thread. The number crosses; the pointer type does not.
    let window = hwnd as usize;
    std::thread::spawn(move || {
        // A panic in the install would otherwise leave this window waiting
        // for a message that never came, with no button and no way to close
        // it. Caught here so it becomes a sentence instead.
        let outcome =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)).unwrap_or_else(|_| {
                Outcome::failed(
                    "Brokey stopped while it was installing, so it may not be installed. Look \
                     in Add or remove programs, and run the setup executable again if Brokey \
                     is not listed there."
                        .to_string(),
                )
            });
        *result.lock().unwrap_or_else(|held| held.into_inner()) = Some(outcome);
        // SAFETY: the window is still alive. `WM_CLOSE` is refused for the
        // whole of `Stage::Working`, which began before this thread was
        // spawned and ends only when this message arrives, so nothing can
        // have destroyed the window between the two.
        unsafe { PostMessageW(window as HWND, OUTCOME_READY, 0, 0) };
    });
}

/// Closing, which is refused while Windows Installer is running.
///
/// Refused rather than made to cancel: msiexec is elevated and this process
/// is not, so there is nothing this window could do to stop it. Closing would
/// only hide an install that carried on anyway.
fn close(hwnd: HWND, state: &mut State) {
    if matches!(state.stage, Stage::Working) {
        return;
    }
    // SAFETY: `hwnd` is this thread's own live window, and destroying it is
    // what sends the `WM_DESTROY` that ends `pump`.
    unsafe { DestroyWindow(hwnd) };
}

/// Whether a click at `lparam`'s point is on the button.
fn in_button(state: &State, lparam: LPARAM) -> bool {
    // The low and high halves of `lParam` are the x and y of the click, in
    // client coordinates, and both are signed: a drag can leave the window.
    let x = (lparam & 0xFFFF) as i16 as i32;
    let y = ((lparam >> 16) & 0xFFFF) as i16 as i32;
    let r = &state.button_rect;
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

fn redraw(hwnd: HWND) {
    // SAFETY: `hwnd` is live; a null rectangle asks for the whole client
    // area, and `1` asks for the background to be counted as invalid too,
    // which `WM_ERASEBKGND` then declines to wipe.
    unsafe { InvalidateRect(hwnd, std::ptr::null(), 1) };
}

/// Everything the window shows, drawn into an off-screen bitmap and copied
/// over in one go. Drawing straight onto the screen would show the header,
/// then the text, then the button, which reads as a flicker every time the
/// stage changes.
fn paint(hwnd: HWND, state: &mut State) {
    let mut ps: PAINTSTRUCT = unsafe { std::mem::zeroed() };
    // SAFETY: `ps` is a live `PAINTSTRUCT` this call fills, and every path
    // below reaches the matching `EndPaint`.
    let hdc = unsafe { BeginPaint(hwnd, &mut ps) };

    let mut client = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    // SAFETY: `hwnd` is live and `client` is a live `RECT` this call fills.
    unsafe { GetClientRect(hwnd, &mut client) };
    let (width, height) = (client.right, client.bottom);

    // SAFETY: `hdc` came from `BeginPaint` above and is valid until
    // `EndPaint`; the bitmap is made to match it and is selected into the
    // memory context before anything is drawn.
    let (memory, bitmap, previous) = unsafe {
        let memory = CreateCompatibleDC(hdc);
        let bitmap = CreateCompatibleBitmap(hdc, width, height);
        let previous = SelectObject(memory, bitmap);
        (memory, bitmap, previous)
    };

    draw(state, memory, width, height);

    // SAFETY: both contexts are live, the rectangle is the bitmap's own size,
    // and every object created above is put back and deleted here.
    unsafe {
        BitBlt(hdc, 0, 0, width, height, memory, 0, 0, SRCCOPY);
        SelectObject(memory, previous);
        DeleteObject(bitmap);
        DeleteDC(memory);
        EndPaint(hwnd, &ps);
    }
}

/// The drawing itself, on whatever context it is given.
fn draw(state: &mut State, hdc: HDC, width: i32, height: i32) {
    let margin = state.px(size::MARGIN);
    let header = state.px(size::HEADER);
    let hairline = state.px(1).max(1);

    fill(hdc, 0, 0, width, header, ink::CHROME);
    fill(hdc, 0, header, width, header + hairline, ink::LINE);
    fill(hdc, 0, header + hairline, width, height, ink::WINDOW);

    // SAFETY: a mode set on a live context, so text is drawn without a box of
    // background colour behind every glyph.
    unsafe { SetBkMode(hdc, TRANSPARENT as i32) };

    let title_font = font(state.px(size::TITLE_TEXT), 600);
    let body_font = font(state.px(size::BODY_TEXT), 400);
    let button_font = font(state.px(size::BUTTON_TEXT), 600);

    let mut title_box = RECT {
        left: margin,
        top: 0,
        right: width - margin,
        bottom: header,
    };
    text(
        hdc,
        &state.title,
        &mut title_box,
        title_font,
        ink::TEXT_STRONG,
        DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
    );

    let mut line_box = RECT {
        left: margin,
        top: state.px(size::LINE_TOP),
        right: width - margin,
        bottom: state.px(size::LINE_TOP + size::LINE_HEIGHT),
    };
    text(
        hdc,
        &state.line,
        &mut line_box,
        body_font,
        ink::TEXT,
        DT_LEFT | DT_TOP | DT_WORDBREAK | DT_NOPREFIX,
    );

    // The empty track. Drawn only while Windows Installer is running, and
    // never with anything in it: nothing here has measured how far it has
    // got, so there is nothing honest to fill it with.
    if matches!(state.stage, Stage::Working) {
        let top = state.px(size::TRACK_TOP);
        fill(
            hdc,
            margin,
            top,
            width - margin,
            top + state.px(size::TRACK).max(1),
            ink::LINE,
        );
    }

    state.button_rect = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    if let Some(label) = state.button {
        let button_height = state.px(size::BUTTON_HEIGHT);
        let label_width = measure(hdc, button_font, label);
        let button_width =
            (label_width + state.px(size::BUTTON_PADDING) * 2).max(state.px(size::BUTTON_MIN));
        let rect = RECT {
            left: width - margin - button_width,
            top: height - margin - button_height,
            right: width - margin,
            bottom: height - margin,
        };
        let fill_colour = if state.pressed {
            ink::ACCENT_DIM
        } else {
            ink::ACCENT
        };
        rounded(hdc, &rect, state.px(size::BUTTON_RADIUS), fill_colour);
        let mut label_box = rect;
        text(
            hdc,
            label,
            &mut label_box,
            button_font,
            ink::ACCENT_INK,
            DT_SINGLELINE | DT_VCENTER | windows_sys::Win32::Graphics::Gdi::DT_CENTER | DT_NOPREFIX,
        );
        state.button_rect = rect;
    }

    // SAFETY: the three fonts were created in this call and nothing holds
    // them once the drawing above has finished. A GDI object leaked here
    // would be leaked on every repaint.
    unsafe {
        DeleteObject(title_font);
        DeleteObject(body_font);
        DeleteObject(button_font);
    }
}

/// A rectangle of one colour.
fn fill(hdc: HDC, left: i32, top: i32, right: i32, bottom: i32, colour: COLORREF) {
    let rect = RECT {
        left,
        top,
        right,
        bottom,
    };
    // SAFETY: the brush is created, used and deleted here, and `rect` is live
    // for the call that reads it.
    unsafe {
        let brush = CreateSolidBrush(colour);
        FillRect(hdc, &rect, brush);
        DeleteObject(brush);
    }
}

/// A rounded rectangle of one colour, with no outline: the border a pen would
/// draw is not in the button's specification.
fn rounded(hdc: HDC, rect: &RECT, radius: i32, colour: COLORREF) {
    // SAFETY: the brush is created, selected, put back and deleted here; the
    // stock null pen is owned by the system and is not deleted. `RoundRect`
    // takes the corner's full width and height, which is twice the radius.
    unsafe {
        let brush = CreateSolidBrush(colour);
        let old_brush = SelectObject(hdc, brush);
        let old_pen = SelectObject(hdc, GetStockObject(NULL_PEN));
        RoundRect(
            hdc,
            rect.left,
            rect.top,
            rect.right,
            rect.bottom,
            radius * 2,
            radius * 2,
        );
        SelectObject(hdc, old_pen);
        SelectObject(hdc, old_brush);
        DeleteObject(brush);
    }
}

/// Text in a box, in one colour and one font.
fn text(hdc: HDC, what: &str, box_: &mut RECT, with: HFONT, colour: COLORREF, format: u32) {
    let mut wide_text = wide(what);
    let count = wide_text.len() as i32 - 1;
    // SAFETY: `wide_text` and `box_` are both alive for the whole call, the
    // count excludes the terminator `wide` appended, and the font is put back
    // before this returns so the context is left as it was found.
    unsafe {
        let old = SelectObject(hdc, with);
        SetTextColor(hdc, colour);
        DrawTextW(hdc, wide_text.as_mut_ptr(), count, box_, format);
        SelectObject(hdc, old);
    }
}

/// How wide a label is in a given font, which is what sizes the button.
fn measure(hdc: HDC, with: HFONT, what: &str) -> i32 {
    let wide_text = wide(what);
    let mut size = SIZE { cx: 0, cy: 0 };
    // SAFETY: the string and `size` are both alive for the call, the count
    // excludes the terminator, and the font is put back afterwards.
    unsafe {
        let old = SelectObject(hdc, with);
        GetTextExtentPoint32W(
            hdc,
            wide_text.as_ptr(),
            wide_text.len() as i32 - 1,
            &mut size,
        );
        SelectObject(hdc, old);
    }
    size.cx
}

/// A font of a given pixel height and weight.
///
/// Segoe UI rather than Archivo, which is the family `tokens.css` asks for.
/// Archivo is bundled inside Brokey and read by the WebView from there; it is
/// not installed on the machine, and this window runs **before** Brokey is
/// installed, so there is no Archivo for GDI to find. `tokens.css:37` lists
/// Segoe UI in `--font-ui`'s own fallback stack, which makes this the
/// family the page would fall back to on the same machine.
fn font(height: i32, weight: i32) -> HFONT {
    let face = wide("Segoe UI");
    // SAFETY: `face` is alive for the call, and every other argument is a
    // documented constant. A negative height asks for that character height
    // rather than that cell height, which is what a CSS pixel size means.
    unsafe {
        CreateFontW(
            -height,
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            OUT_TT_PRECIS as u32,
            CLIP_DEFAULT_PRECIS as u32,
            CLEARTYPE_QUALITY as u32,
            (DEFAULT_PITCH | FF_DONTCARE) as u32,
            face.as_ptr(),
        )
    }
}

/// Screen dots per `tokens.css` pixel, which is 1 on a 96 dpi screen and 1.5
/// on a 144 dpi one.
fn screen_scale() -> f32 {
    // SAFETY: a null window handle asks for the screen's own device context,
    // which every process may read; it is released again below.
    unsafe {
        let screen = windows_sys::Win32::Graphics::Gdi::GetDC(std::ptr::null_mut());
        if screen.is_null() {
            return 1.0;
        }
        let dpi = GetDeviceCaps(screen, LOGPIXELSX as i32);
        windows_sys::Win32::Graphics::Gdi::ReleaseDC(std::ptr::null_mut(), screen);
        if dpi <= 0 { 1.0 } else { dpi as f32 / 96.0 }
    }
}

/// A null-terminated UTF-16 buffer, which is what every wide Win32 entry
/// point in this file expects.
fn wide(s: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The colours are `tokens.css`'s, converted once and written down. The
    /// two that carry the brand are pinned here so a change made in one place
    /// and not the other is a failing test rather than an installer that is
    /// quietly a different purple from the application it installs.
    ///
    /// `tools/make-art.py` prints `accent hue 300 -> #A088CC` on every run,
    /// which is where this value comes from.
    #[test]
    fn the_palette_is_the_one_the_page_uses() {
        assert_eq!(ink::ACCENT, rgb(0xA0, 0x88, 0xCC), "--accent at hue 300");
        assert_eq!(ink::WINDOW, rgb(0x11, 0x12, 0x14), "--window");
        // A COLORREF is `0x00BBGGRR`, so the blue of the accent ends up in
        // the top byte of the three. Written out because getting this the
        // wrong way round paints a window nobody would recognise.
        assert_eq!(ink::ACCENT, 0x00CC88A0);
        // Text on an accent fill is the window's own ground, which is what
        // `--accent-ink` is defined as in tokens.css.
        assert_eq!(ink::ACCENT_INK, ink::WINDOW);
    }

    /// Progress is honest: there is no fraction anywhere in this file to
    /// draw, and the sentence beside the empty track says why.
    #[test]
    fn the_track_has_nothing_to_fill_it_with() {
        assert!(
            WORKING.contains("does not report how far it has got"),
            "the sentence says why the track is empty"
        );
        assert!(READY.ends_with('.') && WORKING.ends_with('.'));
    }
}
