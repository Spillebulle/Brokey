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
//! Linux only for now: `pkexec` and the closed list it checks against
//! (`brokey_core::transaction::allow`) are both Linux-specific. A later
//! plan gives Windows its own privilege path and its own helper body.

#[cfg(windows)]
fn main() {
    eprintln!("This build of brokey-helper does not run on Windows yet.");
    std::process::exit(1);
}

#[cfg(unix)]
use brokey_core::transaction::allow::{self, Allowed};
#[cfg(unix)]
use brokey_core::{Event, Plan, Step};
#[cfg(unix)]
use std::io::{BufRead, BufReader, Read, Write};
#[cfg(unix)]
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::{Command, Stdio};
#[cfg(unix)]
use std::sync::Arc;
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(unix)]
use std::sync::mpsc;

#[cfg(unix)]
const EXIT_OK: i32 = 0;
#[cfg(unix)]
const EXIT_STEP_FAILED: i32 = 1;
#[cfg(unix)]
const EXIT_BAD_INPUT: i32 = 2;
#[cfg(unix)]
const EXIT_REFUSED: i32 = 3;
#[cfg(unix)]
const EXIT_NOT_ROOT: i32 = 4;

/// Where a child's program is looked for, and the `PATH` it is given. A
/// fixed list rather than the inherited one so a plan cannot pick up a
/// program from anywhere else.
#[cfg(unix)]
const CHILD_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

#[cfg(unix)]
const USAGE: &str = "Usage: brokey-helper run | check | --version. \
Reads a JSON plan on stdin; run needs root and is meant to be started through pkexec.";

#[cfg(unix)]
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

#[cfg(unix)]
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

#[cfg(unix)]
fn run() -> i32 {
    if geteuid() != 0 {
        eprintln!(
            "brokey-helper run only works as root and is started through pkexec by Brokey. It refuses to run as an ordinary user."
        );
        return EXIT_NOT_ROOT;
    }
    // `Stdin` rather than its lock: the reader moves to the cancel thread
    // after the plan line, and a `StdinLock` cannot cross threads.
    let mut reader = BufReader::new(std::io::stdin());
    let mut first = String::new();
    if let Err(e) = reader.read_line(&mut first) {
        emit(&Event::PlanFinished {
            plan: String::new(),
            ok: false,
            message: format!("The helper could not read the plan: {e}."),
        });
        return EXIT_BAD_INPUT;
    }
    let plan: Plan = match serde_json::from_str(&first) {
        Ok(plan) => plan,
        Err(e) => {
            emit(&Event::PlanFinished {
                plan: String::new(),
                ok: false,
                message: format!("The helper could not read the plan: {e}."),
            });
            return EXIT_BAD_INPUT;
        }
    };
    if let Err(message) = allow::validate_with(&plan, &allowed()) {
        emit(&Event::PlanFinished {
            plan: plan.id.clone(),
            ok: false,
            message,
        });
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
            emit(&Event::PlanFinished {
                plan: plan.id.clone(),
                ok: false,
                message: "Cancelled. The step that was running finished first; nothing after it was started.".to_string(),
            });
            return EXIT_STEP_FAILED;
        }
        emit(&Event::StepStarted {
            plan: plan.id.clone(),
            step: index,
            title: step.title.clone(),
        });
        match run_step(&plan.id, index, step) {
            Ok(()) => emit(&Event::StepFinished {
                plan: plan.id.clone(),
                step: index,
                ok: true,
                message: None,
            }),
            Err(message) => {
                emit(&Event::StepFinished {
                    plan: plan.id.clone(),
                    step: index,
                    ok: false,
                    message: Some(message.clone()),
                });
                emit(&Event::PlanFinished {
                    plan: plan.id.clone(),
                    ok: false,
                    message,
                });
                return EXIT_STEP_FAILED;
            }
        }
    }
    emit(&Event::PlanFinished {
        plan: plan.id.clone(),
        ok: true,
        message: "Finished.".to_string(),
    });
    EXIT_OK
}

/// Run one step with a minimal environment, streaming its output as log
/// events. `Err` is the sentence for the step's failure.
#[cfg(unix)]
fn run_step(plan: &str, index: usize, step: &Step) -> Result<(), String> {
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
        emit(&Event::Log {
            plan: plan.to_string(),
            step: index,
            line,
            stderr,
        });
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
#[cfg(unix)]
fn emit(event: &Event) {
    let mut out = std::io::stdout().lock();
    if let Ok(text) = serde_json::to_string(event) {
        let _ = out.write_all(text.as_bytes());
        let _ = out.write_all(b"\n");
        let _ = out.flush();
    }
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn home_comes_from_passwd() {
        let passwd = "root:x:0:0::/root:/bin/bash\nnobody:x:65534:65534:Kernel Overflow User::/usr/bin/nologin\nme:x:1000:1000::/home/me:/bin/fish\n";
        assert_eq!(home_of(passwd, 1000), Some(PathBuf::from("/home/me")));
        assert_eq!(home_of(passwd, 0), Some(PathBuf::from("/root")));
        assert_eq!(home_of(passwd, 65534), None, "an empty home is no home");
        assert_eq!(home_of(passwd, 42), None);
    }

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
}
