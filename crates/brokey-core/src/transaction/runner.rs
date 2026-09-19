//! Executes a plan. Root steps go to `brokey-helper` through `pkexec`, one
//! helper run per maximal stretch of consecutive root steps; session steps
//! run here as the user. Every line either child prints becomes an
//! `Event::Log`, and is read by the progress parser for the step's program
//! so the activity panel can say "Installing foo" with a fraction when the
//! tool stated one.
//!
//! This is the only place in the workspace that spawns `pkexec`. A new
//! privileged operation is a new entry in `allow.rs` with a test, never a
//! second call site.
//!
//! Cancellation is a flag checked between lines. A session child is killed
//! with its whole process group, so a `sh -c` wrapper and what it started go
//! together. A root child under pkexec belongs to root and the user cannot
//! signal it, so the runner writes `cancel` on the helper's stdin and the
//! helper stops after the step that is running; the events say so.

use crate::model::*;
use crate::transaction::CancelToken;
use crate::transaction::allow::{self, Allowed};
use crate::transaction::progress::{self, ProgressParser};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as Process, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Receives events as a plan runs.
pub trait Sink: Send {
    fn event(&mut self, event: Event);
}

impl<F: FnMut(Event) + Send> Sink for F {
    fn event(&mut self, event: Event) {
        self(event)
    }
}

/// How a run ended. The same words as the final `Event::PlanFinished`, for
/// callers that want a value rather than an event stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub ok: bool,
    pub message: String,
}

pub const HELPER_MISSING: &str =
    "The helper that installs packages was not found. Reinstall Brokey.";
pub const AUTH_CANCELLED: &str = "Authentication was cancelled.";
pub const NOT_AUTHORISED: &str = "You are not authorised to install packages on this machine.";
pub const CANCELLED: &str = "Cancelled.";
pub const CANCELLING_ROOT: &str =
    "Cancelling. The step running under the helper cannot be stopped; nothing after it will start.";
/// Shown when a session step is being killed. paru, yay and makepkg start a
/// root pacman through pkexec, which the user's signal does not reach; that
/// pacman finishes on its own and the runner waits for it.
pub const CANCELLING_SESSION: &str = "Cancelling. A package manager this step started under pkexec finishes first; nothing after it will start.";

/// The word written to the helper's stdin to stop it between steps.
pub const CANCEL_LINE: &str = "cancel";

/// Where a packaged helper lives, in the order they are tried.
pub const HELPER_PATHS: [&str; 2] = [
    "/usr/lib/brokey/brokey-helper",
    "/usr/libexec/brokey/brokey-helper",
];

pub struct Runner {
    helper: Option<PathBuf>,
    wrapper: Vec<String>,
    cancel: CancelToken,
    allowed: Allowed,
}

impl Default for Runner {
    fn default() -> Self {
        Self::new()
    }
}

impl Runner {
    /// A runner that finds the helper the way the application does and
    /// starts it through `pkexec`.
    pub fn new() -> Runner {
        Runner {
            helper: locate_helper(),
            wrapper: vec!["pkexec".to_string()],
            cancel: CancelToken::new(),
            allowed: Allowed::for_home(std::env::var_os("HOME").map(PathBuf::from).as_deref()),
        }
    }

    /// Run with defaults and no way to cancel. `Ok` when the plan finished,
    /// `Err` with the same sentence the final `PlanFinished` carried when it
    /// failed, was refused or was cancelled. The application uses
    /// [`Runner::new`] and [`Runner::execute`] so it can hand out the token.
    pub fn run(plan: &Plan, sink: &mut dyn Sink) -> crate::Result<()> {
        let outcome = Runner::new().execute(plan, sink);
        if outcome.ok {
            Ok(())
        } else {
            Err(crate::Error::new(outcome.message))
        }
    }

    /// Use this helper instead of the located one. `None` means no helper,
    /// which is how the missing-helper path is tested.
    pub fn with_helper(mut self, helper: Option<PathBuf>) -> Runner {
        self.helper = helper;
        self
    }

    /// The program (and leading arguments) the helper is started through.
    /// `pkexec` by default; tests use `env`, and the helper itself refuses
    /// to run without root, so that path cannot install anything.
    pub fn with_wrapper(mut self, wrapper: Vec<String>) -> Runner {
        self.wrapper = wrapper;
        self
    }

    /// Where package files may be installed from, for the dry run that
    /// checks a plan before asking for a password.
    pub fn with_allowed(mut self, allowed: Allowed) -> Runner {
        self.allowed = allowed;
        self
    }

    pub fn helper(&self) -> Option<&Path> {
        self.helper.as_deref()
    }

    /// The token that stops this runner. Clone it and keep it; `cancel` from
    /// any thread.
    pub fn cancel_token(&self) -> CancelToken {
        self.cancel.clone()
    }

    /// Run the plan to its end, emitting every event to `sink`. The outcome
    /// is what the final `PlanFinished` event said, for callers that want a
    /// value as well as the stream.
    pub fn execute(&self, plan: &Plan, sink: &mut dyn Sink) -> Outcome {
        sink.event(Event::PlanStarted {
            plan: plan.id.clone(),
            steps: plan.steps.len(),
        });
        let outcome = self.execute_inner(plan, sink);
        sink.event(Event::PlanFinished {
            plan: plan.id.clone(),
            ok: outcome.ok,
            message: outcome.message.clone(),
        });
        outcome
    }

    fn execute_inner(&self, plan: &Plan, sink: &mut dyn Sink) -> Outcome {
        let fail = |message: String| Outcome { ok: false, message };
        if plan.steps.is_empty() {
            return Outcome {
                ok: true,
                message: "Nothing to do.".to_string(),
            };
        }
        let runs = split_runs(&plan.steps);
        let helper = if runs.iter().any(|r| r.root) {
            match &self.helper {
                Some(h) if h.is_file() => Some(h.clone()),
                _ => return fail(HELPER_MISSING.to_string()),
            }
        } else {
            None
        };
        // Check every root step before the first password prompt, so a plan
        // the helper would refuse never costs the user an authentication.
        for run in runs.iter().filter(|r| r.root) {
            if let Err(message) = allow::validate_with(&sub_plan(plan, run), &self.allowed) {
                return fail(message);
            }
        }
        let mut state = ProgressState::new(&plan.steps);
        for run in &runs {
            if self.cancel.is_cancelled() {
                return fail(CANCELLED.to_string());
            }
            let result = match &helper {
                Some(helper) if run.root => self.run_helper(plan, run, helper, &mut state, sink),
                _ => self.run_session(plan, run.first, &mut state, sink),
            };
            if let Err(message) = result {
                return fail(message);
            }
        }
        Outcome {
            ok: true,
            message: summary(&plan.ops),
        }
    }

    /// One session step, as the user, with the user's environment plus the
    /// step's own.
    fn run_session(
        &self,
        plan: &Plan,
        index: usize,
        state: &mut ProgressState,
        sink: &mut dyn Sink,
    ) -> Result<(), String> {
        let step = &plan.steps[index];
        let id = plan.id.clone();
        state.start(index);
        sink.event(Event::StepStarted {
            plan: id.clone(),
            step: index,
            title: step.title.clone(),
        });
        emit_progress(sink, &id, index, state, None);
        let mut process = Process::new(&step.command.program);
        process
            .args(&step.command.args)
            .envs(
                step.command
                    .env
                    .iter()
                    .map(|(k, v)| (k.as_str(), v.as_str())),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        if let Some(cwd) = &step.command.cwd {
            process.current_dir(cwd);
        }
        let mut child = match process.spawn() {
            Ok(child) => child,
            Err(e) => {
                let message = format!("Could not start {}: {e}.", step.command.program);
                sink.event(Event::StepFinished {
                    plan: id,
                    step: index,
                    ok: false,
                    message: Some(message.clone()),
                });
                return Err(format!("{} failed. {message}", step.title));
            }
        };
        let lines = stream_lines(&mut child);
        let mut parser = progress::parser_for(&step.command.program);
        let mut cancelled = false;
        loop {
            match lines.recv_timeout(POLL) {
                Ok(line) => {
                    sink.event(Event::Log {
                        plan: id.clone(),
                        step: index,
                        line: line.text.clone(),
                        stderr: line.stderr,
                    });
                    if let Some(reading) = parser.line(&line.text) {
                        state.read(reading.fraction);
                        emit_progress(sink, &id, index, state, reading.message);
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            if !cancelled && self.cancel.is_cancelled() {
                cancelled = true;
                emit_progress(
                    sink,
                    &id,
                    index,
                    state,
                    Some(CANCELLING_SESSION.to_string()),
                );
                kill_group(&mut child);
            }
        }
        let status = child
            .wait()
            .map_err(|e| format!("Could not wait for {}: {e}.", step.command.program))?;
        if cancelled {
            sink.event(Event::StepFinished {
                plan: id,
                step: index,
                ok: false,
                message: Some(CANCELLED.to_string()),
            });
            return Err(CANCELLED.to_string());
        }
        if status.success() {
            state.finish(index);
            sink.event(Event::StepFinished {
                plan: id.clone(),
                step: index,
                ok: true,
                message: None,
            });
            emit_progress(sink, &id, index, state, None);
            return Ok(());
        }
        let message = match status.code() {
            Some(code) => format!(
                "{} failed with exit code {code}. The log has the details.",
                step.title
            ),
            None => format!("{} was stopped by a signal.", step.title),
        };
        sink.event(Event::StepFinished {
            plan: id,
            step: index,
            ok: false,
            message: Some(message.clone()),
        });
        Err(message)
    }

    /// One helper invocation for a stretch of root steps. The helper's own
    /// events come back on its stdout with step numbers local to the
    /// sub-plan; they are renumbered to the plan's and forwarded.
    fn run_helper(
        &self,
        plan: &Plan,
        run: &Run,
        helper: &Path,
        state: &mut ProgressState,
        sink: &mut dyn Sink,
    ) -> Result<(), String> {
        let id = plan.id.clone();
        let sub = sub_plan(plan, run);
        let json = serde_json::to_string(&sub)
            .map_err(|e| format!("The plan could not be encoded: {e}."))?;
        sink.event(Event::AuthRequired { plan: id.clone() });
        let Some((program, leading)) = self.wrapper.split_first() else {
            return Err("No privilege wrapper is configured.".to_string());
        };
        let mut child = Process::new(program)
            .args(leading)
            .arg(helper)
            .arg("run")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("Could not start {program}: {e}."))?;
        // The plan is written on its own thread. pkexec reads nothing until
        // the dialog has been answered, so a plan larger than the pipe buffer
        // would block this loop before the first event arrived if it were
        // written here. The same thread owns the cancel line, so stdin has
        // one owner and closes when the thread ends.
        let (cancel_tx, cancel_rx) = mpsc::channel::<()>();
        if let Some(mut stdin) = child.stdin.take() {
            std::thread::spawn(move || {
                let written = stdin
                    .write_all(json.as_bytes())
                    .and_then(|()| stdin.write_all(b"\n"))
                    .and_then(|()| stdin.flush());
                if let Err(e) = written {
                    // The helper exits before reading when it is not root or
                    // pkexec refused; the exit code says which, so this is
                    // only logged.
                    log::debug!("could not write the plan to the helper: {e}");
                    return;
                }
                if cancel_rx.recv().is_ok() {
                    let _ = stdin
                        .write_all(CANCEL_LINE.as_bytes())
                        .and_then(|()| stdin.write_all(b"\n"))
                        .and_then(|()| stdin.flush());
                }
            });
        }
        let lines = stream_lines(&mut child);
        let mut current = run.first;
        let mut parser: Box<dyn ProgressParser> =
            progress::parser_for(&plan.steps[run.first].command.program);
        let mut helper_said: Option<(bool, String)> = None;
        let mut stderr_text = String::new();
        let mut cancel_sent = false;
        loop {
            match lines.recv_timeout(POLL) {
                Ok(line) if line.stderr => {
                    if stderr_text.len() < 4096 {
                        stderr_text.push_str(&line.text);
                        stderr_text.push('\n');
                    }
                    sink.event(Event::Log {
                        plan: id.clone(),
                        step: current,
                        line: line.text,
                        stderr: true,
                    });
                }
                Ok(line) => match serde_json::from_str::<Event>(&line.text) {
                    Ok(Event::StepStarted { step, title, .. }) => {
                        current = run.first + step;
                        state.start(current);
                        parser = progress::parser_for(
                            plan.steps
                                .get(current)
                                .map(|s| s.command.program.as_str())
                                .unwrap_or(""),
                        );
                        sink.event(Event::StepStarted {
                            plan: id.clone(),
                            step: current,
                            title,
                        });
                        emit_progress(sink, &id, current, state, None);
                    }
                    Ok(Event::Log {
                        step, line, stderr, ..
                    }) => {
                        let step = run.first + step;
                        sink.event(Event::Log {
                            plan: id.clone(),
                            step,
                            line: line.clone(),
                            stderr,
                        });
                        if let Some(reading) = parser.line(&line) {
                            state.read(reading.fraction);
                            emit_progress(sink, &id, step, state, reading.message);
                        }
                    }
                    Ok(Event::StepFinished {
                        step, ok, message, ..
                    }) => {
                        let step = run.first + step;
                        if ok {
                            state.finish(step);
                        }
                        sink.event(Event::StepFinished {
                            plan: id.clone(),
                            step,
                            ok,
                            message,
                        });
                        if ok {
                            emit_progress(sink, &id, step, state, None);
                        }
                    }
                    Ok(Event::Progress {
                        step,
                        fraction,
                        message,
                        ..
                    }) => {
                        let step = run.first + step;
                        state.read(fraction);
                        emit_progress(sink, &id, step, state, message);
                    }
                    Ok(Event::PlanFinished { ok, message, .. }) => {
                        helper_said = Some((ok, message))
                    }
                    Ok(Event::PlanStarted { .. } | Event::AuthRequired { .. }) => {}
                    Err(_) => sink.event(Event::Log {
                        plan: id.clone(),
                        step: current,
                        line: line.text,
                        stderr: false,
                    }),
                },
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            if !cancel_sent && self.cancel.is_cancelled() {
                cancel_sent = true;
                let _ = cancel_tx.send(());
                emit_progress(sink, &id, current, state, Some(CANCELLING_ROOT.to_string()));
            }
        }
        drop(cancel_tx);
        let status = child
            .wait()
            .map_err(|e| format!("Could not wait for the helper: {e}."))?;
        match (status.code(), helper_said) {
            (Some(0), Some((true, _))) | (Some(0), None) => {
                // A helper that exited cleanly finished every step, whether
                // or not each `StepFinished` arrived intact.
                for index in run.first..run.first + run.len {
                    state.finish(index);
                }
                Ok(())
            }
            (Some(126), _) => Err(AUTH_CANCELLED.to_string()),
            // 127 is also pkexec's answer when no authentication agent is
            // registered or the helper could not be executed; its own line
            // says which.
            (Some(127), _) => Err(format!("{NOT_AUTHORISED} {}", first_line(&stderr_text))
                .trim_end()
                .to_string()),
            (_, Some((false, message))) => Err(message),
            (Some(4), _) => Err(format!(
                "The helper refused to run because it was not started as root. {}",
                first_line(&stderr_text)
            )
            .trim_end()
            .to_string()),
            (Some(code), _) => Err(format!(
                "The helper stopped with exit code {code}. {}",
                first_line(&stderr_text)
            )
            .trim_end()
            .to_string()),
            (None, _) => Err("The helper was stopped by a signal.".to_string()),
        }
    }
}

const POLL: Duration = Duration::from_millis(50);

/// A maximal stretch of steps with the same privilege: `first..first+len`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run {
    pub first: usize,
    pub len: usize,
    pub root: bool,
}

/// Consecutive root steps become one helper invocation; every session step
/// is a run of its own.
pub fn split_runs(steps: &[Step]) -> Vec<Run> {
    let mut runs: Vec<Run> = Vec::new();
    for (index, step) in steps.iter().enumerate() {
        match runs.last_mut() {
            Some(run) if run.root && step.needs_root => run.len += 1,
            _ => runs.push(Run {
                first: index,
                len: 1,
                root: step.needs_root,
            }),
        }
    }
    runs
}

fn sub_plan(plan: &Plan, run: &Run) -> Plan {
    Plan {
        id: plan.id.clone(),
        ops: plan.ops.clone(),
        steps: plan.steps[run.first..run.first + run.len].to_vec(),
    }
}

/// Where the helper is on this machine: `BROKEY_HELPER` for development, then
/// beside the running executable (a `cargo run` build, or a test binary in
/// `target/debug/deps` with the helper one level up), then the packaged
/// locations.
pub fn locate_helper() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("BROKEY_HELPER").map(PathBuf::from)
        && path.is_file()
    {
        return Some(path);
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let beside = dir.join("brokey-helper");
        if beside.is_file() {
            return Some(beside);
        }
        if dir.file_name().is_some_and(|n| n == "deps")
            && let Some(parent) = dir.parent()
        {
            let above = parent.join("brokey-helper");
            if above.is_file() {
                return Some(above);
            }
        }
    }
    HELPER_PATHS.iter().map(PathBuf::from).find(|p| p.is_file())
}

/// The plan-level fraction. Weights are the steps' own; a step of unknown
/// internal progress counts as not started until it finishes. Finishing is
/// recorded per step rather than as a running sum, so a step reported done
/// twice (once by the helper's event, once when the helper exits) counts once.
struct ProgressState {
    weights: Vec<u32>,
    done: Vec<bool>,
    current: Option<usize>,
    local: Option<f32>,
}

impl ProgressState {
    fn new(steps: &[Step]) -> ProgressState {
        ProgressState {
            weights: steps.iter().map(|s| s.weight.max(1)).collect(),
            done: vec![false; steps.len()],
            current: None,
            local: None,
        }
    }
    fn start(&mut self, index: usize) {
        self.current = Some(index);
        self.local = None;
    }
    fn read(&mut self, fraction: Option<f32>) {
        if let Some(f) = fraction {
            self.local = Some(f.clamp(0.0, 1.0));
        }
    }
    fn finish(&mut self, index: usize) {
        if let Some(done) = self.done.get_mut(index) {
            *done = true;
        }
        if self.current == Some(index) {
            self.current = None;
            self.local = None;
        }
    }
    fn completed(&self) -> u32 {
        self.weights
            .iter()
            .zip(&self.done)
            .filter(|(_, done)| **done)
            .map(|(w, _)| *w)
            .sum()
    }
    fn total(&self) -> u32 {
        self.weights.iter().sum::<u32>().max(1)
    }
    /// `None` until either a step has finished or the running tool said how
    /// far it is; an empty rail is the honest picture before that.
    fn fraction(&self) -> Option<f32> {
        let weight = self
            .current
            .and_then(|i| self.weights.get(i))
            .copied()
            .unwrap_or(0) as f32;
        let completed = self.completed() as f32;
        let total = self.total() as f32;
        match self.local {
            Some(local) => Some((completed + local * weight) / total),
            None if completed > 0.0 => Some(completed / total),
            None => None,
        }
    }
}

fn emit_progress(
    sink: &mut dyn Sink,
    plan: &str,
    step: usize,
    state: &ProgressState,
    message: Option<String>,
) {
    sink.event(Event::Progress {
        plan: plan.to_string(),
        step,
        fraction: state.fraction(),
        message,
    });
}

struct Line {
    text: String,
    stderr: bool,
}

/// Read the child's stdout and stderr on two threads into one channel, so a
/// child that fills one pipe while the other is being read cannot block.
fn stream_lines(child: &mut Child) -> mpsc::Receiver<Line> {
    let (tx, rx) = mpsc::channel();
    if let Some(out) = child.stdout.take() {
        let tx = tx.clone();
        std::thread::spawn(move || pump(out, false, tx));
    }
    if let Some(err) = child.stderr.take() {
        std::thread::spawn(move || pump(err, true, tx));
    }
    rx
}

fn pump(reader: impl Read, stderr: bool, tx: mpsc::Sender<Line>) {
    let mut reader = BufReader::new(reader);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                // Progress bars redraw with a carriage return on one line;
                // the last segment is what the tool meant to leave standing.
                let text = String::from_utf8_lossy(&buf);
                let text = text.trim_end_matches(['\n', '\r']);
                let text = text.rsplit('\r').next().unwrap_or(text);
                if tx
                    .send(Line {
                        text: text.to_string(),
                        stderr,
                    })
                    .is_err()
                {
                    break;
                }
            }
        }
    }
}

// libc's kill(2), declared here rather than through the libc crate, which
// the workspace does not carry. Spawning `/usr/bin/kill` was the other way
// and would have made cancellation depend on a binary being on PATH.
unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}
const SIGTERM: i32 = 15;
const SIGKILL: i32 = 9;

/// Stop the child and everything in its process group: SIGTERM, a moment
/// for it to finish, then SIGKILL.
fn kill_group(child: &mut Child) {
    let pgid = -(child.id() as i32);
    // SAFETY: kill(2) takes two integers and touches no memory; a wrong pid
    // is an error return, not undefined behaviour. The group id is the
    // child's own pid because it was spawned with process_group(0).
    unsafe { kill(pgid, SIGTERM) };
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    // SAFETY: as above.
    unsafe { kill(pgid, SIGKILL) };
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("").trim()
}

/// The final sentence for a plan that ran through: what was asked for, in
/// the past tense.
pub fn summary(ops: &[Op]) -> String {
    if ops.is_empty() {
        return "Finished.".to_string();
    }
    let n = ops.len();
    let all = |pred: fn(&Op) -> bool| ops.iter().all(pred);
    let (verb, counted) = if all(|op| matches!(op, Op::Install { .. })) {
        ("Installed", true)
    } else if all(|op| matches!(op, Op::Remove { .. })) {
        ("Removed", true)
    } else if all(|op| matches!(op, Op::Update { .. } | Op::UpdateAll { .. })) {
        (
            "Updated",
            !ops.iter().any(|op| matches!(op, Op::UpdateAll { .. })),
        )
    } else if all(|op| matches!(op, Op::Refresh { .. })) {
        ("Refreshed", false)
    } else {
        ("Finished", false)
    };
    if counted && n > 1 {
        format!("{verb} {n} packages.")
    } else {
        format!("{verb}.")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(root: bool, weight: u32) -> Step {
        Step {
            source: SourceKind::Pacman,
            title: "t".to_string(),
            command: Command {
                program: "true".to_string(),
                args: Vec::new(),
                env: Vec::new(),
                cwd: None,
            },
            needs_root: root,
            weight,
        }
    }

    #[test]
    fn runs_join_consecutive_root_steps_only() {
        let steps = [
            step(true, 1),
            step(true, 1),
            step(false, 1),
            step(true, 1),
            step(false, 1),
            step(false, 1),
        ];
        let runs = split_runs(&steps);
        let shape: Vec<(usize, usize, bool)> =
            runs.iter().map(|r| (r.first, r.len, r.root)).collect();
        assert_eq!(
            shape,
            [
                (0, 2, true),
                (2, 1, false),
                (3, 1, true),
                (4, 1, false),
                (5, 1, false)
            ]
        );
        assert!(split_runs(&[]).is_empty());
    }

    #[test]
    fn the_fraction_is_none_until_something_is_known() {
        let steps = [step(true, 1), step(true, 3)];
        let mut s = ProgressState::new(&steps);
        assert_eq!(s.fraction(), None);
        s.start(0);
        assert_eq!(
            s.fraction(),
            None,
            "a step that has just started says nothing yet"
        );
        s.read(Some(0.5));
        assert_eq!(s.fraction(), Some(0.125));
        s.read(None);
        assert_eq!(
            s.fraction(),
            Some(0.125),
            "a message-only line keeps the last fraction"
        );
        s.finish(0);
        assert_eq!(s.fraction(), Some(0.25));
        s.start(1);
        assert_eq!(s.fraction(), Some(0.25));
        s.read(Some(2.0));
        assert_eq!(s.fraction(), Some(1.0), "clamped");
        s.finish(1);
        assert_eq!(s.fraction(), Some(1.0));
    }

    #[test]
    fn a_zero_weight_still_counts_as_one() {
        let steps = [step(true, 0), step(true, 0)];
        let mut s = ProgressState::new(&steps);
        s.start(0);
        s.finish(0);
        assert_eq!(s.fraction(), Some(0.5));
    }

    #[test]
    fn finishing_a_step_twice_counts_once() {
        let steps = [step(true, 1), step(true, 1)];
        let mut s = ProgressState::new(&steps);
        s.start(0);
        s.finish(0);
        s.finish(0);
        assert_eq!(s.fraction(), Some(0.5));
        s.finish(7);
        assert_eq!(s.fraction(), Some(0.5), "an index past the end is ignored");
    }

    #[test]
    fn summaries_say_what_happened() {
        let p = |id: &str| PackageRef {
            source: SourceKind::Pacman,
            id: id.to_string(),
        };
        assert_eq!(summary(&[]), "Finished.");
        assert_eq!(summary(&[Op::Install { package: p("a") }]), "Installed.");
        assert_eq!(
            summary(&[
                Op::Install { package: p("a") },
                Op::Install { package: p("b") }
            ]),
            "Installed 2 packages."
        );
        assert_eq!(
            summary(&[
                Op::Remove { package: p("a") },
                Op::Remove { package: p("b") }
            ]),
            "Removed 2 packages."
        );
        assert_eq!(summary(&[Op::Update { package: p("a") }]), "Updated.");
        assert_eq!(
            summary(&[Op::UpdateAll {
                source: SourceKind::Pacman
            }]),
            "Updated."
        );
        assert_eq!(
            summary(&[Op::Refresh {
                source: SourceKind::Pacman
            }]),
            "Refreshed."
        );
        assert_eq!(
            summary(&[
                Op::Install { package: p("a") },
                Op::Remove { package: p("b") }
            ]),
            "Finished."
        );
    }

    #[test]
    fn the_sentences_follow_the_copy_rules() {
        for s in [
            HELPER_MISSING,
            AUTH_CANCELLED,
            NOT_AUTHORISED,
            CANCELLED,
            CANCELLING_ROOT,
            CANCELLING_SESSION,
        ] {
            assert!(s.ends_with('.'), "{s}");
            assert!(!s.contains('\u{2014}'), "{s}");
        }
        assert!(NOT_AUTHORISED.contains("authorised"), "British spelling");
    }
}
