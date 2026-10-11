//! The runner end to end, without root and without the helper binary.
//! Session steps run real programs (`printf`, `sh`); the root path runs a
//! fake helper, a short Python script that speaks the helper's event
//! protocol, started through `env` rather than `pkexec`. What is checked is
//! the runner's side of the boundary: validation before any password prompt,
//! the split into helper runs, renumbering of the helper's events,
//! progress arithmetic, the pkexec exit codes, cancellation in both lanes.
//! The real helper's `check` mode is exercised too when a build of it is
//! beside the test binary, and skipped with a note when it is not.
//!
//! The fake helper needs `python3`, which every machine this store targets
//! has.

#![cfg(unix)]

use brokey_core::model::*;
use brokey_core::transaction::allow;
use brokey_core::transaction::elevate;
use brokey_core::transaction::progress::{Reading, parser_for};
use brokey_core::transaction::runner::{self, Outcome, Runner};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command as Process, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/transaction")
        .join(name)
}

fn command(program: &str, args: &[&str]) -> Command {
    Command {
        program: program.to_string(),
        args: args.iter().map(|a| a.to_string()).collect(),
        env: Vec::new(),
        cwd: None,
    }
}

fn step(title: &str, program: &str, args: &[&str], root: bool, weight: u32) -> Step {
    Step {
        source: SourceKind::Pacman,
        title: title.to_string(),
        command: command(program, args),
        needs_root: root,
        weight,
    }
}

fn session(title: &str, program: &str, args: &[&str]) -> Step {
    step(title, program, args, false, 1)
}

fn root(title: &str, program: &str, args: &[&str]) -> Step {
    step(title, program, args, true, 1)
}

fn plan(steps: Vec<Step>) -> Plan {
    Plan {
        id: "test-plan".to_string(),
        ops: Vec::new(),
        steps,
    }
}

/// Run and collect every event, in order.
fn run_collect(runner: &Runner, plan: &Plan) -> (Outcome, Vec<Event>) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let seen = events.clone();
    let mut sink = move |event: Event| seen.lock().unwrap().push(event);
    let outcome = runner.execute(plan, &mut sink);
    let events = events.lock().unwrap().clone();
    (outcome, events)
}

/// One line per event, so a whole run can be compared against a list that
/// reads like a transcript. Fractions are rounded to two places.
fn shape(event: &Event) -> String {
    let fraction = |f: &Option<f32>| {
        f.map(|f| format!("{f:.2}"))
            .unwrap_or_else(|| "-".to_string())
    };
    match event {
        Event::PlanStarted { steps, .. } => format!("plan_started {steps}"),
        Event::AuthRequired { .. } => "auth_required".to_string(),
        Event::StepStarted { step, title, .. } => format!("step_started {step} {title}"),
        Event::Progress {
            step,
            fraction: f,
            message,
            ..
        } => format!(
            "progress {step} {} {}",
            fraction(f),
            message.as_deref().unwrap_or("-")
        ),
        Event::Log {
            step, line, stderr, ..
        } => {
            format!("{} {step} {line}", if *stderr { "err" } else { "log" })
        }
        Event::StepFinished {
            step, ok, message, ..
        } => format!(
            "step_finished {step} {}{}",
            if *ok { "ok" } else { "failed" },
            message
                .as_ref()
                .map(|m| format!(" {m}"))
                .unwrap_or_default()
        ),
        Event::PlanFinished { ok, message, .. } => {
            format!(
                "plan_finished {} {message}",
                if *ok { "ok" } else { "failed" }
            )
        }
    }
}

fn shapes(events: &[Event]) -> Vec<String> {
    events.iter().map(shape).collect()
}

/// Put a script in place through a child process rather than by writing it
/// here: a file this process holds open for writing while another test's
/// spawn forks is inherited by that child, and executing it then fails with
/// "Text file busy". The staging copy is never executed, so it may be
/// written directly.
fn write_executable(path: &Path, text: &str) {
    let staging = path.with_extension("txt");
    std::fs::write(&staging, text).unwrap();
    let status = Process::new("install")
        .args(["-m", "755"])
        .arg(&staging)
        .arg(path)
        .status()
        .unwrap();
    assert!(
        status.success(),
        "install could not place {}",
        path.display()
    );
    assert_eq!(
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o755
    );
}

/// A helper stand-in. The prelude reads the plan line unbuffered (so a later
/// `cancel` line is still on the pipe for `select`), records what it was
/// given, and defines `emit`, `read_line` (the next line, `None` once the
/// runner closes the input) and `take` (make a line the current plan and
/// record it); `body` is the behaviour under test.
fn fake_helper(dir: &Path, body: &str) -> (PathBuf, PathBuf) {
    let record = dir.join("record.jsonl");
    let record_literal = format!("{:?}", record.display().to_string());
    let mut script = format!(
        r#"#!/usr/bin/env python3
import json, os, select, sys
RECORD = {record}
def emit(e):
    sys.stdout.write(json.dumps(e) + "\n")
    sys.stdout.flush()
buf = b""
def read_line():
    global buf
    while b"\n" not in buf:
        chunk = os.read(0, 65536)
        if not chunk:
            line, buf = buf, b""
            return line or None
        buf += chunk
    line, _, buf = buf.partition(b"\n")
    return line
def take(line):
    global plan, pid
    plan = json.loads(line)
    pid = plan["id"]
    with open(RECORD, "a") as f:
        f.write(json.dumps({{"argv": sys.argv[1:], "wrapped": os.environ.get("WRAPPED"), "process": os.getpid(), "steps": len(plan["steps"]), "programs": [s["command"]["program"] for s in plan["steps"]]}}) + "\n")
take(read_line())
rest = buf
"#,
        record = record_literal
    );
    script.push_str(body);
    let path = dir.join("fake-helper");
    write_executable(&path, &script);
    (path, record)
}

/// Runs every step of every plan it is sent, printing pacman-shaped lines
/// for pacman steps so the progress parser has something to read, until
/// the runner closes its input.
const WELL_BEHAVED: &str = r#"
LINES = {"pacman": ["(1/2) installing foo", "(2/2) installing bar"]}
while True:
    for i, step in enumerate(plan["steps"]):
        emit({"event": "step_started", "plan": pid, "step": i, "title": step["title"]})
        for line in LINES.get(step["command"]["program"], ["working"]):
            emit({"event": "log", "plan": pid, "step": i, "line": line, "stderr": False})
        emit({"event": "step_finished", "plan": pid, "step": i, "ok": True, "message": None})
    emit({"event": "plan_finished", "plan": pid, "ok": True, "message": "Finished."})
    line = read_line()
    if line is None:
        sys.exit(0)
    take(line)
"#;

/// The first step fails the way pacman does when a package has left the
/// repositories, and the helper reports it with its usual sentence.
const NOT_FOUND: &str = r#"
title = plan["steps"][0]["title"]
emit({"event": "step_started", "plan": pid, "step": 0, "title": title})
emit({"event": "log", "plan": pid, "step": 0, "line": "error: target not found: foo", "stderr": True})
msg = title + " failed with exit code 1. The log has the details."
emit({"event": "step_finished", "plan": pid, "step": 0, "ok": False, "message": msg})
emit({"event": "plan_finished", "plan": pid, "ok": False, "message": msg})
sys.exit(1)
"#;

/// Finishes one plan and then exits as an old, one-plan helper would, so a
/// second sub-plan is written to nobody.
const ONE_PLAN_ONLY: &str = r#"
for i, step in enumerate(plan["steps"]):
    emit({"event": "step_started", "plan": pid, "step": i, "title": step["title"]})
    emit({"event": "step_finished", "plan": pid, "step": i, "ok": True, "message": None})
emit({"event": "plan_finished", "plan": pid, "ok": True, "message": "Finished."})
sys.exit(0)
"#;

/// The first step fails the way the real helper reports it.
const FAILING: &str = r#"
title = plan["steps"][0]["title"]
emit({"event": "step_started", "plan": pid, "step": 0, "title": title})
sys.stderr.write("something went wrong\n")
sys.stderr.flush()
msg = title + " failed with exit code 1. The log has the details."
emit({"event": "step_finished", "plan": pid, "step": 0, "ok": False, "message": msg})
emit({"event": "plan_finished", "plan": pid, "ok": False, "message": msg})
sys.exit(1)
"#;

/// Starts the first step, then waits for the runner's `cancel` line the way
/// the real helper does: the step finishes, nothing after it starts.
const CANCELLABLE: &str = r#"
emit({"event": "step_started", "plan": pid, "step": 0, "title": plan["steps"][0]["title"]})
while b"cancel" not in rest:
    ready, _, _ = select.select([0], [], [], 10)
    if not ready:
        break
    chunk = os.read(0, 65536)
    if not chunk:
        break
    rest += chunk
emit({"event": "step_finished", "plan": pid, "step": 0, "ok": True, "message": None})
if b"cancel" in rest:
    emit({"event": "plan_finished", "plan": pid, "ok": False, "message": "Cancelled. The step that was running finished first; nothing after it was started."})
    sys.exit(1)
emit({"event": "plan_finished", "plan": pid, "ok": True, "message": "Finished without a cancel line."})
sys.exit(0)
"#;

fn runner_with(helper: &Path) -> Runner {
    Runner::new()
        .with_helper(Some(helper.to_path_buf()))
        .with_wrapper(vec!["env".to_string(), "WRAPPED=yes".to_string()])
}

fn starts(events: &[Event]) -> Vec<usize> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::StepStarted { step, .. } => Some(*step),
            _ => None,
        })
        .collect()
}

/// The seam gives back the same three things the runner used to take off
/// the child directly: somewhere to write the plan, somewhere to read
/// lines, and an exit code. If this breaks, the helper cannot be spoken
/// to at all.
#[test]
fn the_elevate_seam_carries_a_plan_and_brings_back_lines() {
    // `cat` stands in for the helper: whatever is written to it comes
    // straight back on its output, which is exactly the shape the seam has
    // to carry.
    //
    // It has to be `sh -c cat` rather than `cat` itself. The seam appends
    // the helper path and `run` to the wrapper, and plain `cat` would read
    // those as filenames and fail; after `sh -c cat` they land in `$0` and
    // `$1`, where nothing looks at them, and `cat` reads its standard input
    // as intended. The helper path is unused for the same reason, so it is
    // named to say so.
    let wrapper = ["sh".to_string(), "-c".to_string(), "cat".to_string()];
    let mut e = elevate::start(Path::new("unused-by-this-wrapper"), &wrapper)
        .expect("the seam starts a process");
    {
        use std::io::Write;
        let mut input = e.input.take().expect("there is somewhere to write");
        writeln!(input, "hello").expect("the plan is written");
    }
    let first = e.lines.recv().expect("a line comes back");
    assert_eq!(first.text, "hello");
    assert!(!first.stderr, "it came back on the output stream");
    assert_eq!(e.wait().expect("the child is waited on"), Some(0));
}

#[test]
fn a_session_only_plan_streams_events_in_order() {
    let mut second = session("Failing", "sh", &["-c", "echo oops >&2; exit 3"]);
    second.weight = 3;
    let plan = plan(vec![
        session("Saying hello", "printf", &["hello\\nworld\\n"]),
        second,
    ]);
    let runner = Runner::new().with_helper(None);
    let (outcome, events) = run_collect(&runner, &plan);
    assert!(!outcome.ok);
    assert_eq!(
        shapes(&events),
        [
            "plan_started 2",
            "step_started 0 Saying hello",
            "progress 0 - -",
            "log 0 hello",
            "log 0 world",
            "step_finished 0 ok",
            "progress 0 0.25 -",
            "step_started 1 Failing",
            "progress 1 0.25 -",
            "err 1 oops",
            "step_finished 1 failed Failing failed with exit code 3. The log has the details.",
            "plan_finished failed Failing failed with exit code 3. The log has the details.",
        ]
    );
}

#[test]
fn run_returns_the_outcome_as_a_result() {
    let mut sink = |_: Event| {};
    assert!(Runner::run(&plan(vec![session("Fine", "true", &[])]), &mut sink).is_ok());
    let err = Runner::run(&plan(vec![session("Broken", "false", &[])]), &mut sink).unwrap_err();
    assert_eq!(
        err.message,
        "Broken failed with exit code 1. The log has the details."
    );
}

#[test]
fn an_empty_plan_finishes_at_once() {
    let (outcome, events) = run_collect(&Runner::new().with_helper(None), &plan(Vec::new()));
    assert!(outcome.ok);
    assert_eq!(
        shapes(&events),
        ["plan_started 0", "plan_finished ok Nothing to do."]
    );
}

#[test]
fn a_session_step_gets_its_working_directory_and_environment() {
    let dir = tempfile::tempdir().unwrap();
    let mut step = session(
        "Looking around",
        "sh",
        &["-c", "echo $BROKEY_TEST_WORD; pwd"],
    );
    step.command
        .env
        .push(("BROKEY_TEST_WORD".to_string(), "hello".to_string()));
    step.command.cwd = Some(dir.path().to_path_buf());
    let (outcome, events) = run_collect(&Runner::new().with_helper(None), &plan(vec![step]));
    assert!(outcome.ok, "{}", outcome.message);
    let logs: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            Event::Log { line, .. } => Some(line.as_str()),
            _ => None,
        })
        .collect();
    let cwd = std::fs::canonicalize(dir.path()).unwrap();
    assert_eq!(logs, ["hello", cwd.to_str().unwrap()]);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::AuthRequired { .. }))
    );
}

#[test]
fn a_program_that_cannot_start_fails_its_step_with_a_sentence() {
    let plan = plan(vec![session(
        "Never",
        "definitely-not-a-program-brokey",
        &[],
    )]);
    let (outcome, events) = run_collect(&Runner::new().with_helper(None), &plan);
    assert!(!outcome.ok);
    assert!(
        outcome
            .message
            .starts_with("Never failed. Could not start definitely-not-a-program-brokey: "),
        "{}",
        outcome.message
    );
    assert!(outcome.message.ends_with('.'));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::StepFinished { ok: false, .. }))
    );
}

#[test]
fn root_steps_without_a_helper_fail_before_anything_runs() {
    let plan = plan(vec![
        session("First", "printf", &["never\\n"]),
        root("Installing foo", "pacman", &["-S", "--noconfirm", "foo"]),
    ]);
    for helper in [None, Some(PathBuf::from("/nonexistent/brokey-helper"))] {
        let (outcome, events) = run_collect(&Runner::new().with_helper(helper), &plan);
        assert!(!outcome.ok);
        assert_eq!(outcome.message, runner::HELPER_MISSING);
        assert_eq!(
            shapes(&events),
            [
                "plan_started 2".to_string(),
                format!("plan_finished failed {}", runner::HELPER_MISSING)
            ]
        );
    }
}

#[test]
fn a_refused_root_step_never_reaches_the_helper_or_the_password_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let (helper, record) = fake_helper(dir.path(), WELL_BEHAVED);
    let plan = plan(vec![
        session("First", "printf", &["never\\n"]),
        root("Installing foo", "pacman", &["-S", "--noconfirm", "foo"]),
        root("Cleaning", "rm", &["-rf", "/"]),
    ]);
    let (outcome, events) = run_collect(&runner_with(&helper), &plan);
    assert!(!outcome.ok);
    assert!(
        outcome
            .message
            .starts_with("The helper refused a step it does not allow: rm -rf /."),
        "{}",
        outcome.message
    );
    assert!(
        starts(&events).is_empty(),
        "nothing ran: {:?}",
        shapes(&events)
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::AuthRequired { .. }))
    );
    assert!(!record.exists(), "the helper was never started");
}

#[test]
fn root_runs_go_through_the_wrapper_and_come_back_renumbered() {
    let dir = tempfile::tempdir().unwrap();
    let (helper, record) = fake_helper(dir.path(), WELL_BEHAVED);
    let mut install = root(
        "Installing 2 packages",
        "pacman",
        &["-S", "--noconfirm", "--needed", "foo", "bar"],
    );
    install.weight = 2;
    let plan = plan(vec![
        session("Saying one", "printf", &["one\\n"]),
        install,
        root(
            "Installing org.x",
            "flatpak",
            &["install", "--system", "-y", "flathub", "org.x"],
        ),
        session("Saying two", "printf", &["two\\n"]),
        root("Removing baz", "pacman", &["-Rs", "--noconfirm", "baz"]),
    ]);
    let (outcome, events) = run_collect(&runner_with(&helper), &plan);
    assert!(outcome.ok, "{}", outcome.message);
    assert_eq!(
        shapes(&events),
        [
            "plan_started 5",
            "step_started 0 Saying one",
            "progress 0 - -",
            "log 0 one",
            "step_finished 0 ok",
            "progress 0 0.17 -",
            "auth_required",
            "step_started 1 Installing 2 packages",
            "progress 1 0.17 -",
            "log 1 (1/2) installing foo",
            "progress 1 0.33 Installing foo",
            "log 1 (2/2) installing bar",
            "progress 1 0.50 Installing bar",
            "step_finished 1 ok",
            "progress 1 0.50 -",
            "step_started 2 Installing org.x",
            "progress 2 0.50 -",
            "log 2 working",
            "step_finished 2 ok",
            "progress 2 0.67 -",
            "step_started 3 Saying two",
            "progress 3 0.67 -",
            "log 3 two",
            "step_finished 3 ok",
            "progress 3 0.83 -",
            "step_started 4 Removing baz",
            "progress 4 0.83 -",
            "log 4 (1/2) installing foo",
            "progress 4 0.92 Installing foo",
            "log 4 (2/2) installing bar",
            "progress 4 1.00 Installing bar",
            "step_finished 4 ok",
            "progress 4 1.00 -",
            "plan_finished ok Finished.",
        ]
    );
    let recorded = std::fs::read_to_string(&record).unwrap();
    let runs: Vec<serde_json::Value> = recorded
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(runs.len(), 2, "two stretches of root steps, two sub-plans");
    for run in &runs {
        assert_eq!(run["argv"], serde_json::json!(["run"]));
        assert_eq!(run["wrapped"], "yes", "started through the wrapper");
    }
    assert_eq!(
        runs[0]["process"], runs[1]["process"],
        "one helper, and so one password, for the whole plan"
    );
    assert_eq!(
        runs[0]["programs"],
        serde_json::json!(["pacman", "flatpak"])
    );
    assert_eq!(runs[1]["programs"], serde_json::json!(["pacman"]));
}

#[test]
fn a_step_the_helper_reports_as_failed_ends_the_plan() {
    let dir = tempfile::tempdir().unwrap();
    let (helper, _) = fake_helper(dir.path(), FAILING);
    let plan = plan(vec![
        root("Installing foo", "pacman", &["-S", "--noconfirm", "foo"]),
        session("Never", "printf", &["never\\n"]),
    ]);
    let (outcome, events) = run_collect(&runner_with(&helper), &plan);
    assert!(!outcome.ok);
    assert_eq!(
        outcome.message,
        "Installing foo failed with exit code 1. The log has the details."
    );
    assert_eq!(
        starts(&events),
        [0],
        "the session step after the failure never starts"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Log { step: 0, line, stderr: true, .. } if line == "something went wrong")),
        "the helper's own stderr is forwarded as a log line: {:?}",
        shapes(&events)
    );
}

#[test]
fn a_failure_the_output_explains_says_why() {
    let dir = tempfile::tempdir().unwrap();
    let (helper, _) = fake_helper(dir.path(), NOT_FOUND);
    let plan = plan(vec![root(
        "Installing foo",
        "pacman",
        &["-S", "--noconfirm", "foo"],
    )]);
    let (outcome, events) = run_collect(&runner_with(&helper), &plan);
    assert!(!outcome.ok);
    assert!(
        outcome
            .message
            .starts_with("Installing foo failed. foo is not in any repository"),
        "{}",
        outcome.message
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::StepFinished { ok: false, message: Some(m), .. } if *m == outcome.message
        )),
        "the step says the same: {:?}",
        shapes(&events)
    );
}

#[test]
fn a_session_failure_the_output_explains_says_why() {
    let plan = plan(vec![session(
        "Building foo",
        "sh",
        &[
            "-c",
            "echo '==> ERROR: A failure occurred in build().' >&2; exit 4",
        ],
    )]);
    let (outcome, _) = run_collect(&Runner::new().with_helper(None), &plan);
    assert!(!outcome.ok);
    assert!(
        outcome
            .message
            .starts_with("Building foo failed. The package failed to build."),
        "{}",
        outcome.message
    );
}

#[test]
fn a_helper_that_leaves_early_is_not_a_success() {
    let dir = tempfile::tempdir().unwrap();
    let (helper, _) = fake_helper(dir.path(), ONE_PLAN_ONLY);
    let plan = plan(vec![
        root("Installing foo", "pacman", &["-S", "--noconfirm", "foo"]),
        session("Between", "true", &[]),
        root("Removing baz", "pacman", &["-Rs", "--noconfirm", "baz"]),
    ]);
    let (outcome, events) = run_collect(&runner_with(&helper), &plan);
    assert!(!outcome.ok, "{}", outcome.message);
    assert!(
        outcome.message.starts_with("The helper stopped"),
        "{}",
        outcome.message
    );
    assert_eq!(starts(&events), [0, 1]);
}

#[test]
fn a_kernel_upgrade_asks_for_a_restart_at_the_end() {
    let dir = tempfile::tempdir().unwrap();
    let body = WELL_BEHAVED.replace("(1/2) installing foo", "(1/2) upgrading linux-cachyos");
    let (helper, _) = fake_helper(dir.path(), &body);
    let plan = plan(vec![root("Updating", "pacman", &["-Syu", "--noconfirm"])]);
    let (outcome, _) = run_collect(&runner_with(&helper), &plan);
    assert!(outcome.ok);
    assert!(
        outcome.message.contains("linux-cachyos changed.")
            && outcome.message.contains("restart the computer"),
        "{}",
        outcome.message
    );
}

#[test]
fn plans_sharing_a_queue_run_one_at_a_time() {
    let queue = Arc::new(Mutex::new(()));
    let first = Runner::new().with_helper(None).with_queue(queue.clone());
    let second = Runner::new().with_helper(None).with_queue(queue);
    let slow = plan(vec![session("Sleeping", "sleep", &["1"])]);
    let quick = plan(vec![session("Quick", "true", &[])]);
    let handle = std::thread::spawn(move || run_collect(&first, &slow));
    std::thread::sleep(Duration::from_millis(200));
    let started = Instant::now();
    let (outcome, events) = run_collect(&second, &quick);
    assert!(outcome.ok);
    assert!(
        started.elapsed() >= Duration::from_millis(500),
        "the second plan did not wait for the first"
    );
    assert_eq!(
        shapes(&events)[1],
        format!("progress 0 - {}", runner::WAITING_FOR_PLAN)
    );
    assert!(handle.join().unwrap().0.ok);
}

#[test]
fn a_held_pacman_lock_is_waited_for() {
    let dir = tempfile::tempdir().unwrap();
    let (helper, _) = fake_helper(dir.path(), WELL_BEHAVED);
    let lock = dir.path().join("db.lck");
    std::fs::write(&lock, "").unwrap();
    let runner = runner_with(&helper).with_pacman_lock(Some(lock.clone()));
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(800));
        std::fs::remove_file(lock).unwrap();
    });
    let started = Instant::now();
    let plan = plan(vec![root(
        "Installing foo",
        "pacman",
        &["-S", "--noconfirm", "foo"],
    )]);
    let (outcome, events) = run_collect(&runner, &plan);
    releaser.join().unwrap();
    assert!(outcome.ok, "{}", outcome.message);
    assert!(started.elapsed() >= Duration::from_millis(700));
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::Progress { message: Some(m), .. } if m == runner::WAITING_FOR_PACMAN
        )),
        "{:?}",
        shapes(&events)
    );
}

#[test]
fn waiting_for_the_pacman_lock_can_be_cancelled() {
    let dir = tempfile::tempdir().unwrap();
    let (helper, record) = fake_helper(dir.path(), WELL_BEHAVED);
    let lock = dir.path().join("db.lck");
    std::fs::write(&lock, "").unwrap();
    let runner = runner_with(&helper).with_pacman_lock(Some(lock));
    let token = runner.cancel_token();
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        token.cancel();
    });
    let plan = plan(vec![root(
        "Installing foo",
        "pacman",
        &["-S", "--noconfirm", "foo"],
    )]);
    let (outcome, events) = run_collect(&runner, &plan);
    canceller.join().unwrap();
    assert_eq!(outcome.message, runner::CANCELLED);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::AuthRequired { .. })),
        "nobody was asked for a password"
    );
    assert!(!record.exists(), "the helper was never started");
}

#[test]
fn pkexec_exit_codes_become_sentences() {
    let dir = tempfile::tempdir().unwrap();
    let helper = dir.path().join("brokey-helper");
    write_executable(&helper, "#!/bin/sh\nexit 0\n");
    // pkexec answers 127 for three different things and only its own output
    // says which, so each of the three has to read as itself: nothing was
    // there to ask, the session could not be authenticated at all, or the
    // caller really was refused. The first two are not refusals and must not
    // read as one.
    let no_terminal = "Error creating textual authentication agent: \
        Error opening current controlling terminal for the process: \
        No such device or address";
    let no_agent = format!("{} No authentication agent found.", runner::NO_AUTH_AGENT);
    let no_terminal_said = format!("{} {no_terminal}.", runner::NO_AUTH_AGENT);
    // The cookie line comes last and the banner above it matches nothing, so
    // this case also shows that the whole of stderr is read, not the first
    // line of it.
    let no_session = format!(
        "{} polkit-agent-helper-1: No session for cookie.",
        runner::NO_POLKIT_SESSION
    );
    for (code, said, sentence) in [
        (126, "", runner::AUTH_CANCELLED),
        (127, "", runner::NOT_AUTHORISED),
        (127, "No authentication agent found.", no_agent.as_str()),
        (127, no_terminal, no_terminal_said.as_str()),
        (
            127,
            "==== AUTHENTICATION FAILED ====\npolkit-agent-helper-1: No session for cookie",
            no_session.as_str(),
        ),
    ] {
        let pkexec = dir.path().join(format!("pkexec-{code}-{}", said.len()));
        let print: String = said
            .lines()
            .map(|line| format!("echo '{line}' >&2\n"))
            .collect();
        write_executable(&pkexec, &format!("#!/bin/sh\n{print}exit {code}\n"));
        let runner = Runner::new()
            .with_helper(Some(helper.clone()))
            .with_wrapper(vec![pkexec.to_str().unwrap().to_string()]);
        let plan = plan(vec![root(
            "Installing foo",
            "pacman",
            &["-S", "--noconfirm", "foo"],
        )]);
        let (outcome, events) = run_collect(&runner, &plan);
        assert!(!outcome.ok);
        assert_eq!(outcome.message, sentence);
        assert!(outcome.message.ends_with('.'));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::AuthRequired { .. })),
            "the page was told to expect a prompt"
        );
    }
}

#[test]
fn cancelling_a_session_step_kills_its_whole_process_group() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pid");
    let script = format!("sleep 30 & echo $! > {}; wait", pidfile.display());
    let plan = plan(vec![
        session("Sleeping", "sh", &["-c", &script]),
        session("Never", "printf", &["never\\n"]),
    ]);
    let runner = Runner::new().with_helper(None);
    let token = runner.cancel_token();
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        token.cancel();
    });
    let started = Instant::now();
    let (outcome, events) = run_collect(&runner, &plan);
    canceller.join().unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "cancellation did not wait for sleep"
    );
    assert!(!outcome.ok);
    assert_eq!(outcome.message, runner::CANCELLED);
    assert_eq!(starts(&events), [0]);
    let cancelling = events
        .iter()
        .position(|e| {
            matches!(
                e,
                Event::Progress { step: 0, message: Some(m), .. } if m == runner::CANCELLING_SESSION
            )
        })
        .expect("the page is told the step is being stopped before it is killed");
    let finished = events
        .iter()
        .position(|e| matches!(
            e,
            Event::StepFinished { step: 0, ok: false, message: Some(m), .. } if m == runner::CANCELLED
        ))
        .expect("the step reports cancelled");
    assert!(cancelling < finished, "{:?}", shapes(&events));
    let pid: u32 = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // Killed means gone, or a zombie nobody has reaped yet: inside a
    // container PID 1 is often not an init and never collects orphans, so
    // /proc/<pid> stays with state Z. Either way the sleep is no longer
    // running, which is what the kill was for.
    let gone = || {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"));
        match stat {
            Err(_) => true,
            Ok(text) => text
                .rsplit(')')
                .next()
                .and_then(|rest| rest.split_whitespace().next())
                .is_some_and(|state| state == "Z" || state == "X"),
        }
    };
    let deadline = Instant::now() + Duration::from_secs(3);
    while !gone() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        gone(),
        "the sleep inside the shell was killed with the group"
    );
}

#[test]
fn cancelling_during_a_root_run_tells_the_helper_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let (helper, _) = fake_helper(dir.path(), CANCELLABLE);
    let plan = plan(vec![
        root("Installing foo", "pacman", &["-S", "--noconfirm", "foo"]),
        session("Never", "printf", &["never\\n"]),
    ]);
    let runner = runner_with(&helper);
    let token = runner.cancel_token();
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        token.cancel();
    });
    let started = Instant::now();
    let (outcome, events) = run_collect(&runner, &plan);
    canceller.join().unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(9),
        "the helper never saw the cancel line"
    );
    assert!(!outcome.ok);
    assert!(
        outcome.message.starts_with("Cancelled."),
        "{}",
        outcome.message
    );
    assert_eq!(starts(&events), [0]);
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::Progress { message: Some(m), .. } if m == runner::CANCELLING_ROOT
        )),
        "the page is told the running root step cannot be stopped: {:?}",
        shapes(&events)
    );
    assert!(events.iter().any(|e| matches!(
        e,
        Event::StepFinished {
            step: 0,
            ok: true,
            ..
        }
    )));
}

#[test]
fn a_cancelled_token_stops_a_plan_before_it_starts() {
    let runner = Runner::new().with_helper(None);
    runner.cancel_token().cancel();
    let (outcome, events) = run_collect(
        &runner,
        &plan(vec![session("Never", "printf", &["never\\n"])]),
    );
    assert!(!outcome.ok);
    assert_eq!(outcome.message, runner::CANCELLED);
    assert!(starts(&events).is_empty());
}

#[test]
fn fixture_plans_deserialise_and_validate_as_expected() {
    let allowed: Plan =
        serde_json::from_str(&std::fs::read_to_string(fixture("plan-allowed.json")).unwrap())
            .unwrap();
    assert_eq!(allowed.steps.len(), 4);
    assert_eq!(
        allowed.ops[3],
        Op::UpdateAll {
            source: SourceKind::Pacman
        }
    );
    assert_eq!(allow::validate(&allowed), Ok(()));

    let refused: Plan =
        serde_json::from_str(&std::fs::read_to_string(fixture("plan-refused.json")).unwrap())
            .unwrap();
    let err = allow::validate(&refused).unwrap_err();
    assert!(
        err.starts_with(
            "The helper refused a step it does not allow: pacman -S --config /tmp/evil.conf foo."
        ),
        "the first bad step is the one named: {err}"
    );

    // A local bundle or a .flatpakref would install anything as root.
    let flatpak_path: Plan = serde_json::from_str(
        &std::fs::read_to_string(fixture("plan-refused-flatpak-path.json")).unwrap(),
    )
    .unwrap();
    for step in &flatpak_path.steps {
        let one = Plan {
            id: flatpak_path.id.clone(),
            ops: Vec::new(),
            steps: vec![step.clone()],
        };
        let err = allow::validate(&one).unwrap_err();
        assert!(
            err.ends_with(allow::FLATPAK_NOT_A_FILE),
            "{:?}: {err}",
            step.command.args
        );
    }

    // debconf evaluates DEBIAN_FRONTEND as Perl, as root.
    let injection: Plan = serde_json::from_str(
        &std::fs::read_to_string(fixture("plan-refused-env-injection.json")).unwrap(),
    )
    .unwrap();
    let err = allow::validate(&injection).unwrap_err();
    assert!(err.contains("The value of DEBIAN_FRONTEND"), "{err}");
}

fn readings(program: &str, transcript: &str) -> Vec<Reading> {
    let mut parser = parser_for(program);
    transcript
        .lines()
        .filter_map(|line| parser.line(line))
        .collect()
}

#[test]
fn a_pacman_transcript_reads_as_a_rail_that_never_jumps_back() {
    let text = std::fs::read_to_string(fixture("pacman-install.txt")).unwrap();
    let readings = readings("pacman", &text);
    let messages: Vec<&str> = readings
        .iter()
        .filter_map(|r| r.message.as_deref())
        .collect();
    assert_eq!(
        messages,
        [
            "Resolving dependencies",
            "Checking for conflicts",
            "Downloading packages",
            "Downloading lib32-libxcrypt",
            "Downloading lib32-openssl",
            "Downloading steam",
            "Checking keyring",
            "Checking package integrity",
            "Loading package files",
            "Checking for file conflicts",
            "Checking available disk space",
            "Applying changes",
            "Installing lib32-libxcrypt",
            "Installing lib32-openssl",
            "Installing steam",
            "Running post-transaction hooks",
            "Arming ConditionNeedsUpdate",
            "Updating icon theme caches",
            "Updating the desktop file MIME type cache",
            "Updating the info directory file",
        ]
    );
    let fractions: Vec<f32> = readings.iter().filter_map(|r| r.fraction).collect();
    assert_eq!(
        fractions.len(),
        3,
        "only the package lines carry a count over the whole job"
    );
    assert!((fractions[0] - 1.0 / 3.0).abs() < 1e-6);
    assert!((fractions[1] - 2.0 / 3.0).abs() < 1e-6);
    assert_eq!(fractions[2], 1.0);
    let last_fraction = readings.iter().rposition(|r| r.fraction.is_some()).unwrap();
    let hooks = readings
        .iter()
        .position(|r| r.message.as_deref() == Some("Running post-transaction hooks"))
        .unwrap();
    assert!(
        last_fraction < hooks,
        "hook counts do not move the rail back"
    );
}

#[test]
fn an_apt_transcript_names_each_phase() {
    let text = std::fs::read_to_string(fixture("apt-install.txt")).unwrap();
    let readings = readings("apt-get", &text);
    let messages: Vec<&str> = readings
        .iter()
        .filter_map(|r| r.message.as_deref())
        .collect();
    assert_eq!(
        messages,
        [
            "Reading package lists",
            "Building dependency tree",
            "Reading state information",
            "Downloading steam",
            "Preparing to unpack steam_1.0.0.78-2_amd64.deb",
            "Unpacking steam",
            "Setting up steam",
            "Processing triggers for man-db",
        ]
    );
    assert!(
        readings.iter().all(|r| r.fraction.is_none()),
        "apt states no count on a pipe"
    );
}

/// The built helper, when there is one beside the test binary. Skipped
/// with a note otherwise, because `cargo test -p brokey-core` does not build
/// `brokey-helper` and must not depend on it.
#[test]
fn the_built_helper_checks_plans_and_refuses_to_run_as_a_user() {
    let Some(helper) = runner::locate_helper() else {
        eprintln!(
            "skipped: no brokey-helper build found; run `cargo build -p brokey-helper` first"
        );
        return;
    };
    let version = Process::new(&helper).arg("--version").output().unwrap();
    if !version.stdout.starts_with(b"brokey-helper ") {
        eprintln!("skipped: {} is not this helper", helper.display());
        return;
    }
    let check = |name: &str| {
        let text = std::fs::read_to_string(fixture(name)).unwrap();
        let mut child = Process::new(&helper)
            .arg("check")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        std::io::Write::write_all(child.stdin.as_mut().unwrap(), text.as_bytes()).unwrap();
        child.wait_with_output().unwrap()
    };
    let ok = check("plan-allowed.json");
    assert_eq!(ok.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&ok.stdout), "ok\n");

    let refused = check("plan-refused.json");
    assert_eq!(refused.status.code(), Some(3));
    assert!(
        String::from_utf8_lossy(&refused.stderr)
            .starts_with("The helper refused a step it does not allow: pacman -S --config")
    );
    for (name, reason) in [
        ("plan-refused-flatpak-path.json", allow::FLATPAK_NOT_A_FILE),
        (
            "plan-refused-env-injection.json",
            "The value of DEBIAN_FRONTEND is not a shape the helper passes on.",
        ),
    ] {
        let refused = check(name);
        assert_eq!(refused.status.code(), Some(3), "{name}");
        let stderr = String::from_utf8_lossy(&refused.stderr);
        assert!(
            stderr.starts_with("The helper refused a step it does not allow: "),
            "{stderr}"
        );
        assert!(stderr.contains(reason), "{name}: {stderr}");
    }

    let uid = std::os::unix::fs::MetadataExt::uid(&std::fs::metadata("/proc/self").unwrap());
    if uid == 0 {
        eprintln!("skipped the not-root check: the tests are running as root");
        return;
    }
    let mut child = Process::new(&helper)
        .arg("run")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let text = std::fs::read_to_string(fixture("plan-allowed.json")).unwrap();
    let _ = std::io::Write::write_all(child.stdin.as_mut().unwrap(), text.as_bytes());
    let run = child.wait_with_output().unwrap();
    assert_eq!(run.status.code(), Some(4), "run as a user is refused");
    assert!(String::from_utf8_lossy(&run.stderr).contains("only works as root"));
    assert!(run.stdout.is_empty(), "nothing ran");
}
