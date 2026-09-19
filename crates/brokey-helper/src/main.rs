//! The one part of Brokey that runs as root, or as Administrator. It reads
//! one JSON plan on the first line of its input, checks every step against
//! the closed list in `brokey_core::transaction::allow` before running any,
//! runs them one after another, and writes one JSON `Event` per line back.
//! It has no network code, no search and no idea what a package is; the
//! runner in `brokey-core` does the thinking, and this binary stays small
//! enough to read in one sitting, which is the point of it.
//!
//! Sub-commands:
//!
//! - `run`: as above. Refuses to start unless it is privileged (exit 4), so
//!   a confused caller cannot use it to run things as the user. After the
//!   plan line, a line reading `cancel` stops it after the step that is
//!   running: a privileged child cannot be killed by the user, and killing a
//!   package manager mid-transaction is the one thing worse than waiting.
//! - `check`: validates the plan on stdin and prints `ok`. Works as anyone;
//!   this is the runner's dry run and the tests' way in.
//! - `--version`.
//!
//! Exit codes: 0 finished, 1 a step failed or the run was cancelled, 2 the
//! plan could not be read, 3 the plan was refused, 4 not privileged.
//!
//! One body for both platforms, with three things that genuinely differ.
//!
//! *How the plan arrives and the events leave.* Linux is started as
//! `pkexec brokey-helper run` and uses the stdin and stdout `pkexec` gave
//! it. Windows is started as `brokey-helper run --pipe <name>`, because
//! `ShellExecuteEx` is the only call that elevates and it cannot redirect
//! standard streams; it opens that pipe, which the unelevated side is
//! already listening on, and reads and writes both ends of it.
//!
//! *How "am I privileged" is answered.* Linux asks the kernel for its
//! effective user id. Windows asks its own process token whether it is
//! elevated.
//!
//! *How a step's program is found.* Linux looks it up in `CHILD_PATH`, a
//! fixed list, so a plan cannot steer resolution. Windows has no equivalent
//! fixed location, so it searches nothing at all: the unelevated side
//! resolves the program to a full path while it still has the user's own
//! environment, and `run_step` refuses anything that is not one.

use brokey_core::Step;
use brokey_core::transaction::allow::{self, Allowed};
use brokey_core::{Event, Plan};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

const EXIT_OK: i32 = 0;
const EXIT_STEP_FAILED: i32 = 1;
const EXIT_BAD_INPUT: i32 = 2;
const EXIT_REFUSED: i32 = 3;
const EXIT_NOT_ROOT: i32 = 4;

/// Where a child's program is looked for, and the `PATH` it is given. A
/// fixed list rather than the inherited one so a plan cannot pick up a
/// program from anywhere else. Linux-specific: there is no Windows
/// equivalent list in this plan, so every use of it stays behind
/// `#[cfg(unix)]` rather than growing a Windows policy nobody has decided on
/// yet.
#[cfg(unix)]
const CHILD_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

#[cfg(unix)]
const USAGE: &str = "Usage: brokey-helper run | check | --version. \
Reads a JSON plan on stdin; run needs root and is meant to be started through pkexec.";

/// The same sentence for Windows, where `run` is told which pipe to answer
/// on rather than reading stdin, and needs Administrator rather than root.
#[cfg(windows)]
const USAGE: &str = "Usage: brokey-helper run --pipe <name> | check | --version. \
run needs Administrator and is started by Brokey, which gives it the pipe to answer on; \
check reads a JSON plan on stdin.";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        Some("run") => run(&args),
        Some("check") => check(),
        Some("--version") | Some("-V") => {
            println!("brokey-helper {}", env!("CARGO_PKG_VERSION"));
            EXIT_OK
        }
        _ => {
            eprintln!("{USAGE}");
            EXIT_BAD_INPUT
        }
    };
    std::process::exit(code);
}

fn check() -> i32 {
    let mut text = String::new();
    if let Err(e) = std::io::stdin().lock().read_to_string(&mut text) {
        eprintln!("The helper could not read the plan: {e}.");
        return EXIT_BAD_INPUT;
    }
    let plan: Plan = match serde_json::from_str(&text) {
        Ok(plan) => plan,
        Err(e) => {
            eprintln!("The helper could not read the plan: {e}.");
            return EXIT_BAD_INPUT;
        }
    };
    match allow::validate_with(&plan, &allowed()) {
        Ok(()) => {
            println!("ok");
            EXIT_OK
        }
        Err(message) => {
            eprintln!("{message}");
            EXIT_REFUSED
        }
    }
}

/// `run`'s own transport on Linux: the stdin and stdout `pkexec` gave it.
#[cfg(unix)]
fn run(_args: &[String]) -> i32 {
    // `Stdin` rather than its lock: the reader moves to the cancel thread
    // after the plan line, and a `StdinLock` cannot cross threads.
    run_with(BufReader::new(std::io::stdin()), std::io::stdout())
}

/// `run`'s own transport on Windows: the named pipe whose name was given on
/// the command line. `ShellExecuteEx` is the only call that elevates and it
/// cannot redirect standard streams, so the unelevated side listens on a
/// pipe first and this connects back to it. The pipe is duplex, so one
/// handle carries the plan in and a clone of it carries the events out.
///
/// Nothing here can be reported as an `Event`, because an event needs the
/// pipe these failures are about. They go to stderr and an exit code, which
/// is what the unelevated side reads when no event ever arrives.
#[cfg(windows)]
fn run(args: &[String]) -> i32 {
    let name = match pipe_argument(args) {
        Ok(name) => name,
        Err(message) => {
            eprintln!("{message}");
            return EXIT_BAD_INPUT;
        }
    };
    let writer = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&name)
    {
        Ok(pipe) => pipe,
        Err(e) => {
            eprintln!(
                "The helper could not open {name}: {e}. Start the operation again from Brokey, which makes the pipe before it starts the helper."
            );
            return EXIT_BAD_INPUT;
        }
    };
    let reader = match writer.try_clone() {
        Ok(reader) => reader,
        Err(e) => {
            eprintln!(
                "The helper could not open a second handle on {name}: {e}. Start the operation again from Brokey."
            );
            return EXIT_BAD_INPUT;
        }
    };
    run_with(BufReader::new(reader), writer)
}

/// The pipe to answer on, from `run --pipe <name>`.
#[cfg(windows)]
fn pipe_argument(args: &[String]) -> Result<String, String> {
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        if arg == "--pipe" {
            return rest
                .next()
                .cloned()
                .ok_or_else(|| "--pipe was given with no name after it.".to_string());
        }
    }
    Err(
        "The helper was started without --pipe, so it has no way to answer. \
         Start Brokey rather than running the helper yourself."
            .to_string(),
    )
}

/// The body of `run`, taking its plan from `reader` and writing its events
/// to `writer` instead of assuming stdin and stdout, so a transport that is
/// not a process's own standard streams can supply both. Watches `reader`
/// for a line reading `cancel` after the plan line, exactly as `run` always
/// has.
fn run_with(reader: impl BufRead + Send + 'static, writer: impl Write) -> i32 {
    if !is_privileged() {
        #[cfg(unix)]
        eprintln!(
            "brokey-helper run only works as root and is started through pkexec by Brokey. It refuses to run as an ordinary user."
        );
        #[cfg(windows)]
        eprintln!(
            "brokey-helper run only works as Administrator and is elevated by Brokey when a plan needs it. Start the operation from Brokey rather than running the helper yourself."
        );
        return EXIT_NOT_ROOT;
    }
    serve(reader, writer)
}

/// What this platform calls the privilege the helper runs with. The two
/// platforms say the same thing and differ only by this word, so the
/// sentences below are written once.
#[cfg(unix)]
const PRIVILEGE: &str = "root";
#[cfg(windows)]
const PRIVILEGE: &str = "Administrator";

/// Whether every step in `plan` is one the helper has any business running.
///
/// The runner sends the helper a sub-plan of exactly one root run, and a
/// run is a maximal stretch of steps whose `needs_root` is true, so every
/// step that legitimately arrives here needs root. A plan that carries one
/// which says otherwise was not built by the runner, and it is refused
/// whole before any step starts.
///
/// This is load-bearing on Windows. The Windows closed list checks only the
/// steps that need root, because a step that does not is a session step the
/// runner runs itself, and a per-user removal from `HKCU` is one: checking
/// those here would refuse every one of them before the user was ever
/// asked. Without this guard a forged plan whose steps all said
/// `needs_root: false` would be validated against nothing at all and then
/// run, every step of it, as Administrator. On Linux the list checks every
/// step either way, so this is a tightening there rather than a hole
/// closed: such a plan is now refused before it is checked.
fn every_step_needs_root(plan: &Plan) -> Result<(), String> {
    if plan.steps.iter().all(|step| step.needs_root) {
        return Ok(());
    }
    Err(format!(
        "The helper was sent a step that is not marked as needing {PRIVILEGE}, and it only ever \
         runs steps that are. Start the operation again from Brokey, which decides what needs \
         elevating and what does not."
    ))
}

/// `run_with` once the caller is known to be privileged: read the plan,
/// refuse it if the closed list or [`every_step_needs_root`] says so, then
/// run its steps in order. Separate from the privilege check so that a test
/// can drive the refusal paths, which a test could not otherwise do without
/// being elevated.
fn serve(mut reader: impl BufRead + Send + 'static, mut writer: impl Write) -> i32 {
    let mut first = String::new();
    if let Err(e) = reader.read_line(&mut first) {
        emit(
            &mut writer,
            &Event::PlanFinished {
                plan: String::new(),
                ok: false,
                message: format!("The helper could not read the plan: {e}."),
            },
        );
        return EXIT_BAD_INPUT;
    }
    let plan: Plan = match serde_json::from_str(&first) {
        Ok(plan) => plan,
        Err(e) => {
            emit(
                &mut writer,
                &Event::PlanFinished {
                    plan: String::new(),
                    ok: false,
                    message: format!("The helper could not read the plan: {e}."),
                },
            );
            return EXIT_BAD_INPUT;
        }
    };
    if let Err(message) = every_step_needs_root(&plan) {
        emit(
            &mut writer,
            &Event::PlanFinished {
                plan: plan.id.clone(),
                ok: false,
                message,
            },
        );
        return EXIT_REFUSED;
    }
    if let Err(message) = allow::validate_with(&plan, &allowed()) {
        emit(
            &mut writer,
            &Event::PlanFinished {
                plan: plan.id.clone(),
                ok: false,
                message,
            },
        );
        return EXIT_REFUSED;
    }
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let cancel = cancel.clone();
        std::thread::spawn(move || {
            for line in reader.lines().map_while(Result::ok) {
                if line.trim() == "cancel" {
                    cancel.store(true, Ordering::SeqCst);
                }
            }
        });
    }
    for (index, step) in plan.steps.iter().enumerate() {
        if cancel.load(Ordering::SeqCst) {
            emit(
                &mut writer,
                &Event::PlanFinished {
                    plan: plan.id.clone(),
                    ok: false,
                    message: "Cancelled. The step that was running finished first; nothing after it was started.".to_string(),
                },
            );
            return EXIT_STEP_FAILED;
        }
        emit(
            &mut writer,
            &Event::StepStarted {
                plan: plan.id.clone(),
                step: index,
                title: step.title.clone(),
            },
        );
        let outcome = run_step(&plan.id, index, step, &mut writer);
        match outcome {
            Ok(()) => emit(
                &mut writer,
                &Event::StepFinished {
                    plan: plan.id.clone(),
                    step: index,
                    ok: true,
                    message: None,
                },
            ),
            Err(message) => {
                emit(
                    &mut writer,
                    &Event::StepFinished {
                        plan: plan.id.clone(),
                        step: index,
                        ok: false,
                        message: Some(message.clone()),
                    },
                );
                emit(
                    &mut writer,
                    &Event::PlanFinished {
                        plan: plan.id.clone(),
                        ok: false,
                        message,
                    },
                );
                return EXIT_STEP_FAILED;
            }
        }
    }
    emit(
        &mut writer,
        &Event::PlanFinished {
            plan: plan.id.clone(),
            ok: true,
            message: "Finished.".to_string(),
        },
    );
    EXIT_OK
}

/// Run one step with a minimal environment, streaming its output as log
/// events to `writer`. `Err` is the sentence for the step's failure. The
/// environment scrub and `CHILD_PATH` resolution below are the Linux half
/// of how a step's program is found and what it is given; the Windows half
/// is the function beneath this one and is deliberately different.
#[cfg(unix)]
fn run_step(plan: &str, index: usize, step: &Step, writer: &mut impl Write) -> Result<(), String> {
    let Some(program) = resolve(&step.command.program) else {
        return Err(format!(
            "{} is not installed in any of {}.",
            step.command.program,
            CHILD_PATH.replace(':', ", ")
        ));
    };
    let mut command = Command::new(program);
    command
        .args(&step.command.args)
        .env_clear()
        .env("PATH", CHILD_PATH)
        .env("LC_ALL", "C.UTF-8")
        .env("HOME", "/root")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Only the keys the closed list names; validation refused anything else
    // already, so this is belt and braces rather than a second policy.
    for (key, value) in &step.command.env {
        if allow::ALLOWED_ENV.contains(&key.as_str()) {
            command.env(key, value);
        }
    }
    let child = command
        .spawn()
        .map_err(|e| format!("Could not start {}: {e}.", step.command.program))?;
    stream_and_wait(plan, index, step, child, writer)
}

/// Run one step as Administrator, streaming its output as log events to
/// `writer`. `Err` is the sentence for the step's failure.
///
/// **Nothing here searches for the program.** It must already be a full
/// path, and a step that names anything else is refused. `PATH` inside an
/// elevated process is not the user's: winget in particular is reached
/// through a per-user app execution alias under `%LOCALAPPDATA%`, and when
/// elevation is answered with an administrator's credentials the elevated
/// process has that administrator's profile, so a bare name looked up here
/// would search the wrong profile entirely. Resolution therefore happens on
/// the unelevated side, where the user's own environment is the right one,
/// and this end is handed something with no ambiguity left in it. That is a
/// stronger rule than Linux's `CHILD_PATH`, not a weaker one: the elevated
/// process makes no choice at all.
///
/// **The environment is inherited, and that is weaker than Linux's.** Linux
/// clears the child's environment down to a fixed three variables. Windows
/// cannot: winget and MSI uninstallers need a working environment
/// (`SystemRoot`, `TEMP`, `ProgramData` and the rest), and an empty one
/// breaks them rather than hardening them. There is also no Windows
/// equivalent of `CHILD_PATH` to scrub back to, and inventing one would be
/// a security decision made in passing. This is accepted because the
/// environment is not what decides which program runs here: the closed list
/// in `allow.rs` decides that, and the absolute-path rule above decides
/// where it is found. A future reader comparing the two platforms should
/// read this as a deliberate difference, not an oversight.
///
/// The step's own `env` entries are applied unfiltered, where Linux passes
/// them through `allow::ALLOWED_ENV`. Nothing on Windows sets any, and
/// nothing can: both `winget::operation_step` and `arp::removal_step` build
/// a `Command` with an empty `env`, and the closed list compares a whole
/// command against one of those, `env` and `cwd` included. A forged step
/// carrying a variable is therefore refused before it reaches here, which
/// is why there is no Windows `ALLOWED_ENV` to consult: the list has
/// already asked the stronger question.
#[cfg(windows)]
fn run_step(plan: &str, index: usize, step: &Step, writer: &mut impl Write) -> Result<(), String> {
    if !Path::new(&step.command.program).is_absolute() {
        return Err(format!(
            "The helper was asked to run {}, which is not a full path, and it never searches for a program itself. Start the operation again from Brokey, which resolves the program before it elevates.",
            step.command.program
        ));
    }
    let mut command = Command::new(&step.command.program);
    command
        .args(&step.command.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in &step.command.env {
        command.env(key, value);
    }
    let child = command
        .spawn()
        .map_err(|e| format!("Could not start {}: {e}.", step.command.program))?;
    stream_and_wait(plan, index, step, child, writer)
}

/// Everything a step does after its child has started: stream both of the
/// child's output streams as log events, in the order they arrive, then
/// wait for it and turn its exit into a sentence. Shared, because a
/// running child behaves the same on either platform; only how it was
/// started differs.
fn stream_and_wait(
    plan: &str,
    index: usize,
    step: &Step,
    mut child: std::process::Child,
    writer: &mut impl Write,
) -> Result<(), String> {
    let (tx, rx) = mpsc::channel::<(String, bool)>();
    if let Some(out) = child.stdout.take() {
        let tx = tx.clone();
        std::thread::spawn(move || pump(out, false, tx));
    }
    if let Some(err) = child.stderr.take() {
        std::thread::spawn(move || pump(err, true, tx));
    } else {
        drop(tx);
    }
    for (line, stderr) in rx {
        emit(
            writer,
            &Event::Log {
                plan: plan.to_string(),
                step: index,
                line,
                stderr,
            },
        );
    }
    let status = child
        .wait()
        .map_err(|e| format!("Could not wait for {}: {e}.", step.command.program))?;
    if status.success() {
        Ok(())
    } else {
        Err(match status.code() {
            Some(code) => format!(
                "{} failed with exit code {code}. The log has the details.",
                step.title
            ),
            None => format!("{} was stopped by a signal.", step.title),
        })
    }
}

/// One of a child's output streams, line by line, onto the channel
/// `stream_and_wait` drains. Carriage returns are stripped and a line that
/// redrew itself with them keeps only its last state, which is what a
/// progress bar in a terminal amounts to.
fn pump(reader: impl Read, stderr: bool, tx: mpsc::Sender<(String, bool)>) {
    let mut reader = BufReader::new(reader);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let text = String::from_utf8_lossy(&buf);
                let text = text.trim_end_matches(['\n', '\r']);
                let text = text.rsplit('\r').next().unwrap_or(text);
                if tx.send((text.to_string(), stderr)).is_err() {
                    break;
                }
            }
        }
    }
}

/// The first executable named `program` in [`CHILD_PATH`].
#[cfg(unix)]
fn resolve(program: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    CHILD_PATH
        .split(':')
        .map(|dir| Path::new(dir).join(program))
        .find(|p| {
            std::fs::metadata(p)
                .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        })
}

/// One event, one line, flushed at once so the runner sees it as it
/// happens rather than when the buffer fills.
fn emit(writer: &mut impl Write, event: &Event) {
    if let Ok(text) = serde_json::to_string(event) {
        let _ = writer.write_all(text.as_bytes());
        let _ = writer.write_all(b"\n");
        let _ = writer.flush();
    }
}

/// Whether this process may run a plan. Linux asks the kernel for its
/// effective user id, since `pkexec` is what got it started as anyone at
/// all.
#[cfg(unix)]
fn is_privileged() -> bool {
    geteuid() == 0
}

/// Windows asks its own process token whether it is elevated, which is the
/// Windows question: the user may well be an administrator and still be
/// running with the filtered token that cannot write to `HKLM`, so group
/// membership is not the thing to ask about.
///
/// This process only ever exists because `ShellExecuteEx` elevated it, so a
/// `false` here means something is wrong rather than something is
/// unsupported, and every failure below is answered the same way: no, and
/// let the caller refuse. Refusing on a question that could not be answered
/// is the safe direction to fail.
#[cfg(windows)]
fn is_privileged() -> bool {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Security::{
        GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: `GetCurrentProcess` returns a pseudo handle that is always
    // valid and never needs closing, and `token` is a writable
    // out-parameter; on success it is set to a handle this call gives us
    // sole ownership of.
    let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
    if opened == 0 {
        return false;
    }
    // SAFETY: `token` was just set to a fresh, uniquely owned handle by the
    // successful call above, so this is the one owner and closes it once.
    let token = unsafe { OwnedHandle::from_raw_handle(token as RawHandle) };

    let mut elevation = TOKEN_ELEVATION::default();
    let mut written: u32 = 0;
    // SAFETY: `token` is the valid handle above, `elevation` is a writable
    // `TOKEN_ELEVATION` whose size is passed alongside it, which is the
    // buffer `TokenElevation` is documented to fill, and `written` is a
    // writable out-parameter.
    let read = unsafe {
        GetTokenInformation(
            token.as_raw_handle() as HANDLE,
            TokenElevation,
            (&raw mut elevation).cast(),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut written,
        )
    };
    read != 0 && elevation.TokenIsElevated != 0
}

/// The package directories this invocation may install files from: the
/// system caches plus the invoking user's `~/.cache/brokey`. Under
/// pkexec the invoking user is `PKEXEC_UID`; the home comes from passwd
/// because the user's environment has been scrubbed.
#[cfg(unix)]
fn allowed() -> Allowed {
    let home = std::env::var("PKEXEC_UID")
        .ok()
        .and_then(|uid| uid.parse::<u32>().ok())
        .and_then(|uid| {
            std::fs::read_to_string("/etc/passwd")
                .ok()
                .and_then(|text| home_of(&text, uid))
        })
        .or_else(|| {
            // `check` as an ordinary user: the caller's own home.
            (geteuid() != 0)
                .then(|| std::env::var_os("HOME").map(PathBuf::from))
                .flatten()
        });
    Allowed::for_home(home.as_deref())
}

/// The removal commands the registry records, which is the one fact the
/// Windows closed list cannot derive from a step's shape. Read here, in the
/// process that enforces the list, rather than taken on trust from the plan.
///
/// There is no invoking user's home to derive: Windows has nothing shaped
/// like a package-file directory for the list to guard, so `system()`'s
/// directories stand unused.
#[cfg(windows)]
fn allowed() -> Allowed {
    Allowed::with_registered_removals()
}

/// The home directory of `uid` in a passwd file.
#[cfg(unix)]
fn home_of(passwd: &str, uid: u32) -> Option<PathBuf> {
    passwd.lines().find_map(|line| {
        let fields: Vec<&str> = line.split(':').collect();
        match fields.as_slice() {
            [_, _, id, _, _, home, ..]
                if id.parse::<u32>().ok() == Some(uid) && !home.is_empty() =>
            {
                Some(PathBuf::from(home))
            }
            _ => None,
        }
    })
}

// libc's geteuid(2), declared here rather than through the libc crate the
// workspace does not carry. Reading /proc/self/status was the alternative
// and would have added a parser for one number.
#[cfg(unix)]
unsafe extern "C" {
    safe fn geteuid() -> u32;
}

#[cfg(test)]
mod tests {
    use super::*;
    use brokey_core::{Command, SourceKind, Step};

    #[cfg(unix)]
    #[test]
    fn home_comes_from_passwd() {
        let passwd = "root:x:0:0::/root:/bin/bash\nnobody:x:65534:65534:Kernel Overflow User::/usr/bin/nologin\nme:x:1000:1000::/home/me:/bin/fish\n";
        assert_eq!(home_of(passwd, 1000), Some(PathBuf::from("/home/me")));
        assert_eq!(home_of(passwd, 0), Some(PathBuf::from("/root")));
        assert_eq!(home_of(passwd, 65534), None, "an empty home is no home");
        assert_eq!(home_of(passwd, 42), None);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_finds_sh_and_not_nonsense() {
        assert!(resolve("sh").is_some());
        assert_eq!(resolve("definitely-not-a-program"), None);
        assert_eq!(resolve("../../tmp/x"), None);
    }

    #[test]
    fn events_are_one_json_object_per_line() {
        let text = serde_json::to_string(&Event::StepStarted {
            plan: "p".to_string(),
            step: 0,
            title: "Installing foo".to_string(),
        })
        .unwrap();
        assert!(!text.contains('\n'));
        assert!(text.contains("\"event\":\"step_started\""));
    }

    #[test]
    fn the_sentences_follow_the_copy_rules() {
        assert!(!USAGE.contains('\u{2014}'));
        assert!(USAGE.ends_with('.'));
    }

    /// The pipe name is required on Windows: without it the helper has no way
    /// to answer, so it says so rather than exiting silently.
    #[cfg(windows)]
    #[test]
    fn run_without_a_pipe_is_an_error() {
        let err = pipe_argument(&["run".to_string()]).expect_err("no pipe was given");
        assert!(err.contains("--pipe"), "{err}");
        assert!(err.ends_with('.'), "the reason is a sentence: {err}");
    }

    /// `--pipe` with nothing after it is a different mistake and says so.
    #[cfg(windows)]
    #[test]
    fn a_pipe_flag_with_no_name_is_an_error() {
        let err = pipe_argument(&["run".to_string(), "--pipe".to_string()])
            .expect_err("the flag had no name after it");
        assert!(err.contains("--pipe"), "{err}");
    }

    /// The spelling matches what `elevate` produces. These two must agree or
    /// the helper never connects, and nothing else in the system would say so.
    #[cfg(windows)]
    #[test]
    fn the_pipe_argument_matches_what_elevate_sends() {
        let sent = vec![
            "run".to_string(),
            "--pipe".to_string(),
            r"\\.\pipe\brokey-7-0-1".to_string(),
        ];
        assert_eq!(pipe_argument(&sent).unwrap(), r"\\.\pipe\brokey-7-0-1");
    }

    /// The helper never searches for a program, so a bare name is refused
    /// rather than looked up in whatever `PATH` an elevated process has.
    /// This is the control that makes "the unelevated side resolves it"
    /// true rather than merely intended.
    #[cfg(windows)]
    #[test]
    fn a_program_that_is_not_a_full_path_is_refused() {
        let step = Step {
            source: SourceKind::Winget,
            title: "Test".to_string(),
            command: Command {
                program: "winget.exe".to_string(),
                args: vec!["install".to_string()],
                env: Vec::new(),
                cwd: None,
            },
            needs_root: true,
            weight: 1,
        };
        let mut out = Vec::new();
        let err = run_step("p", 0, &step, &mut out).expect_err("a bare name is not a full path");
        assert!(err.contains("winget.exe"), "{err}");
        assert!(err.ends_with('.'), "the reason is a sentence: {err}");
        assert!(!err.contains('\u{2014}'), "no em dashes: {err}");
        assert!(out.is_empty(), "nothing was started, so nothing was logged");
    }

    /// A plan carrying a step that does not need root was not built by the
    /// runner, which only ever sends one root run at a time, and it is
    /// refused whole before any step starts. On Windows this is what keeps
    /// the closed list from being skipped: it checks only the steps that
    /// need root, so such a plan would otherwise be run without being
    /// checked at all. The step here is one the list would refuse anyway,
    /// but it never gets that far: no step is started and no list is read.
    ///
    /// This test runs on both platforms but is only real coverage on
    /// Windows. Remove the `every_step_needs_root` guard and, on Windows,
    /// this test fails: the Windows closed list checks nothing about a step
    /// whose `needs_root` is false, so the step runs and every assertion
    /// below breaks. On Linux the same removal leaves this test passing
    /// regardless, because the Linux `check_step` refuses a step whose
    /// `needs_root` is false on its own, with a message that contains
    /// "root" the same as the guard's does; the plan still comes back
    /// refused with `PRIVILEGE` in it, for a different reason than the one
    /// this test means to exercise. A reader must not take a green run of
    /// this test on Linux as evidence that the guard itself works there.
    #[test]
    fn a_step_that_does_not_need_root_refuses_the_whole_plan() {
        let mut plan = refusable_plan();
        plan.steps[0].needs_root = false;
        let line = format!(
            "{}\n",
            serde_json::to_string(&plan).expect("a plan is JSON")
        );
        let mut out = Vec::new();
        let code = serve(std::io::Cursor::new(line), &mut out);
        assert_eq!(code, EXIT_REFUSED);
        let events = String::from_utf8(out).expect("events are UTF-8");
        assert!(
            events.contains("\"event\":\"plan_finished\"") && events.contains("\"ok\":false"),
            "{events}"
        );
        assert!(
            !events.contains("step_started"),
            "nothing was started: {events}"
        );
        assert!(
            events.contains(PRIVILEGE),
            "the refusal names the privilege: {events}"
        );
    }

    /// The sentence itself, so the copy rules are checked on it rather than
    /// on whatever a JSON escape made of it.
    #[test]
    fn the_refusal_for_a_step_that_does_not_need_root_is_a_sentence() {
        let mut plan = refusable_plan();
        plan.steps[0].needs_root = false;
        let err = every_step_needs_root(&plan).expect_err("this plan was not built by the runner");
        assert!(err.ends_with('.'), "{err}");
        assert!(!err.contains('\u{2014}'), "no em dashes: {err}");
        assert_eq!(every_step_needs_root(&refusable_plan()), Ok(()));
    }

    /// The helper's closed list is the registry-backed one, not the bare
    /// system list, which would refuse every removal after the prompt. As
    /// on the runner's side, the assertion is the wiring and not a count:
    /// on a machine whose registry records no machine-wide removals the two
    /// lists are equal and this can only pass.
    #[cfg(windows)]
    #[test]
    fn the_helpers_closed_list_is_the_registry_backed_one() {
        assert_eq!(allowed(), Allowed::with_registered_removals());
    }

    /// A step the closed list on this platform does not admit: `curl` is
    /// not one of the programs the Linux list names, and `cmd.exe` is not
    /// `winget.exe` for the Windows one.
    #[cfg(unix)]
    fn refusable_plan() -> Plan {
        Plan {
            id: "test".to_string(),
            ops: Vec::new(),
            steps: vec![Step {
                source: SourceKind::Pacman,
                title: "Test".to_string(),
                command: Command {
                    program: "curl".to_string(),
                    args: vec!["https://example.org".to_string()],
                    env: Vec::new(),
                    cwd: None,
                },
                needs_root: true,
                weight: 1,
            }],
        }
    }

    #[cfg(windows)]
    fn refusable_plan() -> Plan {
        Plan {
            id: "test".to_string(),
            ops: Vec::new(),
            steps: vec![Step {
                source: SourceKind::Winget,
                title: "Test".to_string(),
                command: Command {
                    program: "cmd.exe".to_string(),
                    args: vec!["/c".to_string(), "whoami".to_string()],
                    env: Vec::new(),
                    cwd: None,
                },
                needs_root: true,
                weight: 1,
            }],
        }
    }

    /// The helper refuses a plan the closed list does not admit, on every
    /// platform. This is the invariant the whole binary exists to hold.
    #[test]
    fn a_plan_off_the_closed_list_is_refused() {
        let plan = refusable_plan();
        assert!(brokey_core::transaction::allow::validate(&plan).is_err());
    }
}
