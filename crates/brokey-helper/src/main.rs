//! The one part of Brokey that runs as root. Started as
//! `pkexec brokey-helper run`, it reads one JSON plan on the first line of
//! stdin, checks every step against the closed list in
//! `brokey_core::transaction::allow` before running any, runs them one after
//! another with a scrubbed environment, and writes one JSON `Event` per line
//! on stdout. It has no network code, no search and no idea what a package
//! is; the runner in `brokey-core` does the thinking, and this binary stays
//! small enough to read in one sitting, which is the point of it.
//!
//! Sub-commands:
//!
//! - `run`: as above. Refuses to start unless it is root (exit 4), so a
//!   confused caller cannot use it to run things as the user. After the plan
//!   line, a line reading `cancel` on stdin stops it after the step that is
//!   running: a root child cannot be killed by the user, and killing a
//!   package manager mid-transaction is the one thing worse than waiting.
//! - `check`: validates the plan on stdin and prints `ok`. Works as anyone;
//!   this is the runner's dry run and the tests' way in.
//! - `--version`.
//!
//! Exit codes: 0 finished, 1 a step failed or the run was cancelled, 2 the
//! plan could not be read, 3 the plan was refused, 4 not root.
//!
//! This binary is one body for both platforms now, not two. The closed list
//! it checks against (`brokey_core::transaction::allow`) already has a
//! Windows arm as well as a Linux one, so `check` works on either. `run`
//! still does not, because the two things that genuinely differ by platform
//! have not been answered for Windows yet: how the plan arrives and the
//! events leave (Linux uses `pkexec`'s stdin and stdout; a later plan gives
//! Windows a pipe), and how "am I privileged" is answered (Linux asks the
//! kernel for its effective user id; Windows has no check yet, so it always
//! says no, and `run` always refuses there until that plan gives it a real
//! one).

#[cfg(unix)]
use brokey_core::Step;
use brokey_core::transaction::allow::{self, Allowed};
use brokey_core::{Event, Plan};
use std::io::{BufRead, BufReader, Read, Write};
#[cfg(unix)]
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(unix)]
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

const USAGE: &str = "Usage: brokey-helper run | check | --version. \
Reads a JSON plan on stdin; run needs root and is meant to be started through pkexec.";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        Some("run") => run(),
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

/// `run`'s own transport: Linux uses the stdin and stdout `pkexec` gave it,
/// which is also what this passes for now on Windows, since there is no
/// pipe to hand `run_with` until a later plan builds one. That plan is the
/// only thing that needs to change here.
fn run() -> i32 {
    // `Stdin` rather than its lock: the reader moves to the cancel thread
    // after the plan line, and a `StdinLock` cannot cross threads.
    run_with(BufReader::new(std::io::stdin()), std::io::stdout())
}

/// The body of `run`, taking its plan from `reader` and writing its events
/// to `writer` instead of assuming stdin and stdout, so a transport that is
/// not a process's own standard streams can supply both. Watches `reader`
/// for a line reading `cancel` after the plan line, exactly as `run` always
/// has.
fn run_with(mut reader: impl BufRead + Send + 'static, mut writer: impl Write) -> i32 {
    if !is_privileged() {
        #[cfg(unix)]
        eprintln!(
            "brokey-helper run only works as root and is started through pkexec by Brokey. It refuses to run as an ordinary user."
        );
        #[cfg(windows)]
        eprintln!(
            "brokey-helper run cannot tell whether it is elevated on Windows yet, so it refuses rather than guessing. A later plan answers that for real."
        );
        return EXIT_NOT_ROOT;
    }
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
        // Actually running a step is the one piece of `run` that is not
        // shared yet: it is where `CHILD_PATH` and the environment scrub
        // live on Linux, and Windows has no counterpart to either in this
        // plan. `is_privileged` above already keeps this arm unreachable on
        // Windows; it exists so the loop around it can be one body rather
        // than two.
        #[cfg(unix)]
        let outcome = run_step(&plan.id, index, step, &mut writer);
        #[cfg(windows)]
        let outcome: Result<(), String> =
            Err("brokey-helper does not run steps on Windows yet.".to_string());
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
/// events to `writer`. `Err` is the sentence for the step's failure.
/// Genuinely Linux-only: the environment scrub and `CHILD_PATH` resolution
/// below are exactly the two things this plan does not give Windows an
/// answer for.
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
    let mut child = command
        .spawn()
        .map_err(|e| format!("Could not start {}: {e}.", step.command.program))?;
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

#[cfg(unix)]
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
/// all. Windows has no check wired up yet, so this always says no there,
/// which is what makes `run` refuse on Windows rather than guess; a later
/// plan gives it a real answer.
#[cfg(unix)]
fn is_privileged() -> bool {
    geteuid() == 0
}
#[cfg(windows)]
fn is_privileged() -> bool {
    false
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

/// Windows has no invoking user's home to derive yet, and the Windows arm
/// of the closed list ignores `allowed` entirely (it has nothing shaped
/// like a package-file directory to check), so the system directories
/// stand in until a later plan gives the Windows helper its own answer.
#[cfg(windows)]
fn allowed() -> Allowed {
    Allowed::system()
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
