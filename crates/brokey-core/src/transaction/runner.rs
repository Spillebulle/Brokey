//! Executes a plan. Root steps go to `brokey-helper`, however `elevate`
//! starts it on this platform; session steps run here as the user. Every line
//! either child prints becomes an `Event::Log`, and is read by the progress
//! parser for the step's program so the activity panel can say "Installing
//! foo" with a fraction when the tool stated one, and by a
//! [`Transcript`] so a failure says why and a finished plan says what the
//! user should know.
//!
//! **One helper per plan, and so one password.** The helper is started at
//! the first root step and kept until the plan ends: each later stretch of
//! root steps is written to the same helper as another sub-plan, however
//! long the session steps between them take. An AUR build is the case this
//! is for: dependencies through the helper, then a build as the user that
//! can run for an hour, then the install through the helper again. Starting
//! a second helper after the build would ask again whenever polkit's
//! `auth_admin_keep` had lapsed in the meantime, which for a long build is
//! always. A plan with no root step never starts the helper and never asks.
//!
//! **One plan at a time.** Two plans that both reach pacman would fight
//! over its lock, and the second would fail at once. A runner given a
//! queue ([`Runner::with_queue`]) waits for the plan before it, saying so;
//! the application gives every runner the same one. And a root stretch that
//! reaches pacman waits while somebody else's pacman (Pamac, a terminal)
//! holds `db.lck`, rather than failing with "unable to lock database".
//!
//! A new privileged operation is a new entry in `allow.rs` with a test,
//! never a second call site.
//!
//! Cancellation is a flag checked between lines. A session child is killed
//! with its whole process group on Linux, so a `sh -c` wrapper and what it
//! started go together; Windows has no process group and reaches only the
//! child itself. A root child under the helper belongs to a different user
//! and cannot be signalled from here, so the runner writes `cancel` on the
//! helper's stdin and the helper stops after the step that is running; the
//! events say so.

use crate::model::*;
use crate::transaction::CancelToken;
use crate::transaction::allow::{self, Allowed};
use crate::transaction::diagnose::{self, Transcript};
use crate::transaction::elevate;
use crate::transaction::progress::{self, ProgressParser};
use std::io::{BufRead, BufReader, Read, Write};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as Process, Stdio};
use std::sync::{Arc, Mutex, TryLockError, mpsc};
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
/// pkexec answers 127 for this as well, and it is not the sentence above:
/// nobody was asked, so nothing was refused. Seen on Pop!_OS, where pkexec
/// falls back to its own textual agent and then cannot open `/dev/tty`
/// because a window has none.
///
/// On a window manager with no agent installed at all (a bare Sway, i3 or
/// Hyprland), logging out changes nothing, so the sentence names both.
pub const NO_AUTH_AGENT: &str = "Nothing on this desktop offered to ask for your password, so the helper was never started. \
     If your desktop normally asks, log out and back in to start its authentication agent again. \
     On a window manager such as Sway, i3 or Hyprland, install a polkit authentication agent \
     (polkit-gnome or hyprpolkitagent, say), start it with your session, and try again.";
/// And 127 for this, which is further from the sentence above still: the
/// password was taken and polkit could not say whose session it belonged to.
pub const NO_POLKIT_SESSION: &str = "This desktop session could not be authenticated, so the helper was never started. The polkit \
     service has no record of the session, which is what happens when it is restarted while you \
     are logged in. Log out and back in, or restart the machine.";
pub const CANCELLED: &str = "Cancelled.";
pub const CANCELLING_ROOT: &str =
    "Cancelling. The step running under the helper cannot be stopped; nothing after it will start.";
/// Shown when a session step is being killed. paru, yay and makepkg start a
/// root pacman through pkexec, which the user's signal does not reach; that
/// pacman finishes on its own and the runner waits for it.
#[cfg(unix)]
pub const CANCELLING_SESSION: &str = "Cancelling. A package manager this step started under pkexec finishes first; nothing after it will start.";
/// Shown when a session step is being killed. Windows has no process group to
/// signal, so this reaches only the child; an installer it started is left to
/// finish on its own, which is the truthful thing to promise.
#[cfg(windows)]
pub const CANCELLING_SESSION: &str =
    "Cancelling. An installer this step started finishes first; nothing after it will start.";

/// The word written to the helper's stdin to stop it between steps.
pub const CANCEL_LINE: &str = "cancel";

/// Shown while a plan waits for the one before it.
pub const WAITING_FOR_PLAN: &str = "Waiting for the change before this one to finish.";

/// Shown while somebody else's pacman holds its database lock.
pub const WAITING_FOR_PACMAN: &str =
    "Waiting for another package manager to finish. pacman's database is in use.";

/// How long a plan waits for somebody else's pacman before giving up and
/// saying what to do. Long enough for a system upgrade in a terminal; a
/// lock left by a crash never goes away by itself, so there has to be an
/// end.
const PACMAN_LOCK_PATIENCE: Duration = Duration::from_secs(10 * 60);

/// Where a packaged helper lives, in the order they are tried.
#[cfg(unix)]
pub const HELPER_PATHS: [&str; 2] = [
    "/usr/lib/brokey/brokey-helper",
    "/usr/libexec/brokey/brokey-helper",
];

pub struct Runner {
    helper: Option<PathBuf>,
    wrapper: Vec<String>,
    cancel: CancelToken,
    allowed: Allowed,
    queue: Option<Arc<Mutex<()>>>,
    pacman_lock: Option<PathBuf>,
}

impl Default for Runner {
    fn default() -> Self {
        Self::new()
    }
}

impl Runner {
    /// A runner that finds the helper the way the application does and
    /// starts it however this platform elevates.
    pub fn new() -> Runner {
        #[cfg(unix)]
        let wrapper = vec!["pkexec".to_string()];
        #[cfg(windows)]
        let wrapper = Vec::new();
        #[cfg(unix)]
        let allowed = Allowed::for_home(std::env::var_os("HOME").map(PathBuf::from).as_deref());
        // The same list the helper builds. `execute_inner` validates every
        // root run against this before the first prompt, so a runner left
        // with no removals would refuse every removal before the user was
        // ever asked, and the helper's own check would never be reached.
        #[cfg(windows)]
        let allowed = Allowed::with_registered_removals();
        Runner {
            helper: locate_helper(),
            wrapper,
            cancel: CancelToken::new(),
            allowed,
            queue: None,
            pacman_lock: pacman_lock_file(),
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
    /// `pkexec` by default on Linux, empty on Windows; tests use `env`, and
    /// the helper itself refuses to run without root, so that path cannot
    /// install anything.
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

    /// Run one plan at a time with every other runner holding the same
    /// queue: this one waits, saying so, until the plan before it ends.
    pub fn with_queue(mut self, queue: Arc<Mutex<()>>) -> Runner {
        self.queue = Some(queue);
        self
    }

    /// The file whose presence means pacman is busy, or `None` to never
    /// wait for it. `Runner::new` reads it from `pacman.conf`'s `DBPath` on
    /// a machine that has pacman.
    pub fn with_pacman_lock(mut self, lock: Option<PathBuf>) -> Runner {
        self.pacman_lock = lock;
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
        let queue = self.queue.clone();
        let outcome = match queue.as_deref() {
            Some(queue) => match self.wait_for_turn(queue, plan, sink) {
                Ok(_turn) => self.execute_inner(plan, sink),
                Err(outcome) => outcome,
            },
            None => self.execute_inner(plan, sink),
        };
        sink.event(Event::PlanFinished {
            plan: plan.id.clone(),
            ok: outcome.ok,
            message: outcome.message.clone(),
        });
        outcome
    }

    /// Take the queue, waiting while another plan holds it. The guard is
    /// the turn; dropping it lets the next plan go.
    fn wait_for_turn<'q>(
        &self,
        queue: &'q Mutex<()>,
        plan: &Plan,
        sink: &mut dyn Sink,
    ) -> Result<std::sync::MutexGuard<'q, ()>, Outcome> {
        let mut said = false;
        loop {
            match queue.try_lock() {
                Ok(guard) => return Ok(guard),
                // A plan that panicked while holding the queue has ended all
                // the same; the next one may go.
                Err(TryLockError::Poisoned(poisoned)) => return Ok(poisoned.into_inner()),
                Err(TryLockError::WouldBlock) => {}
            }
            if self.cancel.is_cancelled() {
                return Err(Outcome {
                    ok: false,
                    message: CANCELLED.to_string(),
                });
            }
            if !said {
                said = true;
                sink.event(Event::Progress {
                    plan: plan.id.clone(),
                    step: 0,
                    fraction: None,
                    message: Some(WAITING_FOR_PLAN.to_string()),
                });
            }
            std::thread::sleep(POLL);
        }
    }

    /// Wait while somebody else's pacman holds its lock, before a root
    /// stretch that runs pacman. `Err` is the plan's failure: cancelled, or
    /// the lock outlasted [`PACMAN_LOCK_PATIENCE`].
    fn wait_for_pacman(
        &self,
        plan: &Plan,
        run: &Run,
        state: &ProgressState,
        sink: &mut dyn Sink,
    ) -> Result<(), String> {
        let Some(lock) = &self.pacman_lock else {
            return Ok(());
        };
        let uses_pacman = plan.steps[run.first..run.first + run.len]
            .iter()
            .any(|s| matches!(s.command.program.as_str(), "pacman" | "chwd"));
        if !uses_pacman {
            return Ok(());
        }
        let started = Instant::now();
        let mut said = false;
        while lock.exists() {
            if self.cancel.is_cancelled() {
                return Err(CANCELLED.to_string());
            }
            if started.elapsed() > PACMAN_LOCK_PATIENCE {
                return Err(format!(
                    "{} did not start. {}",
                    plan.steps[run.first].title,
                    diagnose::LOCKED
                ));
            }
            if !said {
                said = true;
                emit_progress(
                    sink,
                    &plan.id,
                    run.first,
                    state,
                    Some(WAITING_FOR_PACMAN.to_string()),
                );
            }
            std::thread::sleep(POLL * 10);
        }
        Ok(())
    }

    fn execute_inner(&self, plan: &Plan, sink: &mut dyn Sink) -> Outcome {
        let mut session: Option<HelperSession> = None;
        let mut transcript = Transcript::new();
        let outcome = self.execute_steps(plan, sink, &mut session, &mut transcript);
        if let Some(session) = session {
            session.close();
        }
        if !outcome.ok {
            return outcome;
        }
        let mut message = outcome.message;
        for line in transcript.afterword() {
            message.push(' ');
            message.push_str(&line);
        }
        Outcome { ok: true, message }
    }

    fn execute_steps(
        &self,
        plan: &Plan,
        sink: &mut dyn Sink,
        session: &mut Option<HelperSession>,
        transcript: &mut Transcript,
    ) -> Outcome {
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
            if run.root
                && let Err(message) = self.wait_for_pacman(plan, run, &state, sink)
            {
                return fail(message);
            }
            let result = match &helper {
                Some(helper) if run.root => {
                    self.run_helper(plan, run, helper, &mut state, sink, session, transcript)
                }
                _ => self.run_session(plan, run.first, &mut state, sink, transcript),
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
        transcript: &mut Transcript,
    ) -> Result<(), String> {
        let step = &plan.steps[index];
        let id = plan.id.clone();
        state.start(index);
        transcript.begin_step();
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
            .stderr(Stdio::piped());
        // A session step gets its own process group so cancelling reaches the
        // whole tree. Windows has no equivalent and does not need one here.
        #[cfg(unix)]
        process.process_group(0);
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
                    transcript.line(&line.text);
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
            transcript.step_succeeded(&step.command.program);
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
            Some(code) => transcript.failure(&step.title).unwrap_or_else(|| {
                format!(
                    "{} failed with exit code {code}. The log has the details.",
                    step.title
                )
            }),
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

    /// One stretch of root steps, written to the plan's helper as a
    /// sub-plan. The helper is started here if this is the plan's first
    /// root stretch, which is the one password prompt the plan costs, and
    /// kept in `session` for the stretches after it. Its events come back
    /// with step numbers local to the sub-plan; they are renumbered to the
    /// plan's and forwarded.
    #[allow(clippy::too_many_arguments)]
    fn run_helper(
        &self,
        plan: &Plan,
        run: &Run,
        helper: &Path,
        state: &mut ProgressState,
        sink: &mut dyn Sink,
        session: &mut Option<HelperSession>,
        transcript: &mut Transcript,
    ) -> Result<(), String> {
        let id = plan.id.clone();
        let mut sub = sub_plan(plan, run);
        resolve_built(&mut sub)?;
        let json = serde_json::to_string(&sub)
            .map_err(|e| format!("The plan could not be encoded: {e}."))?;
        if session.is_none() {
            sink.event(Event::AuthRequired { plan: id.clone() });
            let program = elevation_program(&self.wrapper, helper)?;
            let elevated = elevate::start(helper, &self.wrapper)
                .map_err(|e| format!("Could not start {program}: {e}."))?;
            *session = Some(HelperSession::new(elevated));
        }
        let live = session.as_mut().expect("started above");
        live.send(ToHelper::Plan(json));
        let mut current = run.first;
        let mut parser: Box<dyn ProgressParser> =
            progress::parser_for(&plan.steps[run.first].command.program);
        let mut helper_said: Option<(bool, String)> = None;
        let mut diagnosed: Option<String> = None;
        let mut cancel_sent = false;
        loop {
            match live.elevated.lines.recv_timeout(POLL) {
                Ok(line) if line.stderr => {
                    if live.stderr_text.len() < 4096 {
                        live.stderr_text.push_str(&line.text);
                        live.stderr_text.push('\n');
                    }
                    transcript.line(&line.text);
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
                        transcript.begin_step();
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
                        transcript.line(&line);
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
                        let mut message = message;
                        if ok {
                            state.finish(step);
                            if let Some(s) = plan.steps.get(step) {
                                transcript.step_succeeded(&s.command.program);
                            }
                        } else if let Some(better) = plan
                            .steps
                            .get(step)
                            .and_then(|s| transcript.failure(&s.title))
                        {
                            diagnosed = Some(better.clone());
                            message = Some(better);
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
                        helper_said = Some((ok, message));
                        break;
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
                live.send(ToHelper::Cancel);
                emit_progress(sink, &id, current, state, Some(CANCELLING_ROOT.to_string()));
            }
        }
        match helper_said {
            Some((true, _)) => {
                for index in run.first..run.first + run.len {
                    state.finish(index);
                }
                Ok(())
            }
            Some((false, message)) => Err(diagnosed.unwrap_or(message)),
            // The helper went away without finishing this sub-plan: pkexec
            // refused or was dismissed, or the helper itself stopped. Its
            // exit code says which. The session is over either way.
            None => {
                let mut ended = session.take().expect("started above");
                let exit = ended
                    .wait()
                    .map_err(|e| format!("Could not wait for the helper: {e}."))?;
                let stderr_text = std::mem::take(&mut ended.stderr_text);
                Err(match exit {
                    Some(126) => AUTH_CANCELLED.to_string(),
                    Some(127) => pkexec_refusal(&stderr_text),
                    Some(4) => format!(
                        "The helper refused to run because it was not started as root. {}",
                        first_line(&stderr_text)
                    )
                    .trim_end()
                    .to_string(),
                    Some(code) => format!(
                        "The helper stopped with exit code {code} before it finished. {}",
                        first_line(&stderr_text)
                    )
                    .trim_end()
                    .to_string(),
                    None => "The helper was stopped by a signal.".to_string(),
                })
            }
        }
    }
}

/// What the runner writes to its helper.
enum ToHelper {
    /// One sub-plan, as a JSON line.
    Plan(String),
    /// [`CANCEL_LINE`].
    Cancel,
}

/// A helper started for this plan and still listening. Writing happens on
/// a thread of its own: pkexec reads nothing until its dialog has been
/// answered, so a plan larger than the pipe buffer would block the reading
/// loop before the first event arrived if it were written there. The same
/// thread owns the helper's input, so the input has one owner and closes,
/// which the helper reads as "no more plans", when the thread ends.
struct HelperSession {
    elevated: elevate::Elevated,
    to_helper: Option<mpsc::Sender<ToHelper>>,
    writer: Option<std::thread::JoinHandle<()>>,
    stderr_text: String,
}

impl HelperSession {
    fn new(mut elevated: elevate::Elevated) -> HelperSession {
        let (tx, rx) = mpsc::channel::<ToHelper>();
        let writer = elevated.input.take().map(|mut input| {
            std::thread::spawn(move || {
                for message in rx {
                    let text = match message {
                        ToHelper::Plan(json) => json,
                        ToHelper::Cancel => CANCEL_LINE.to_string(),
                    };
                    let written = input
                        .write_all(text.as_bytes())
                        .and_then(|()| input.write_all(b"\n"))
                        .and_then(|()| input.flush());
                    if let Err(e) = written {
                        // The helper exits before reading when it is not
                        // root or pkexec refused; the exit code says which,
                        // so this is only logged.
                        log::debug!("could not write to the helper: {e}");
                        return;
                    }
                }
            })
        });
        HelperSession {
            elevated,
            to_helper: Some(tx),
            writer,
            stderr_text: String::new(),
        }
    }

    fn send(&self, message: ToHelper) {
        if let Some(tx) = &self.to_helper {
            let _ = tx.send(message);
        }
    }

    /// Close the helper's input and wait for it to exit.
    fn wait(&mut self) -> std::io::Result<Option<i32>> {
        self.to_helper = None;
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
        // Drain what is left so the helper never blocks on a full pipe
        // while it finishes.
        while self.elevated.lines.recv().is_ok() {}
        self.elevated.wait()
    }

    /// The plan is over: tell the helper there is nothing more, and reap it.
    fn close(mut self) {
        if let Err(e) = self.wait() {
            log::debug!("could not wait for the helper: {e}");
        }
    }
}

/// `db.lck` in the `DBPath` this machine's `pacman.conf` names, when this
/// machine has pacman at all.
#[cfg(unix)]
fn pacman_lock_file() -> Option<PathBuf> {
    let conf = std::fs::read_to_string("/etc/pacman.conf").ok()?;
    let db = crate::sources::linux::pacman::options(&conf)
        .db_path
        .unwrap_or_else(|| PathBuf::from("/var/lib/pacman/"));
    Some(db.join("db.lck"))
}

#[cfg(not(unix))]
fn pacman_lock_file() -> Option<PathBuf> {
    None
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

/// Swap every built-package selector ([`allow::BUILT_MARK`]) in a pacman
/// step for the newest file of exactly that package in that directory.
/// Done here, as the user and just before the stretch that installs it,
/// because the file only exists once the build before it has run.
pub fn resolve_built(plan: &mut Plan) -> Result<(), String> {
    for step in &mut plan.steps {
        if step.command.program != "pacman" {
            continue;
        }
        for arg in &mut step.command.args {
            let Some((dir, name)) = allow::parse_built_selector(arg) else {
                continue;
            };
            let newest = std::fs::read_dir(dir)
                .into_iter()
                .flatten()
                .flatten()
                .filter(|e| {
                    e.file_name().to_str().and_then(allow::built_package_name) == Some(name)
                })
                .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok());
            match newest {
                Some(entry) => *arg = entry.path().display().to_string(),
                None => {
                    return Err(format!(
                        "{} could not start, because the build left no package named {name} in {}.                          The package's PKGBUILD may name it differently; the log of the build                          above shows what it made.",
                        step.title,
                        dir.display()
                    ));
                }
            }
        }
    }
    Ok(())
}

fn sub_plan(plan: &Plan, run: &Run) -> Plan {
    Plan {
        id: plan.id.clone(),
        ops: plan.ops.clone(),
        steps: plan.steps[run.first..run.first + run.len].to_vec(),
    }
}

/// What is about to be started, for the sentence shown when it cannot be,
/// and the check that there is anything to start at all.
///
/// The two platforms differ in kind, not in detail. Linux starts the helper
/// through a wrapper, `pkexec`, so a runner whose wrapper is empty cannot
/// elevate at all and that is a misconfiguration worth its own sentence.
/// Windows has no wrapper: `elevate::start` calls `ShellExecuteEx` with the
/// `runas` verb on the helper itself and ignores the wrapper entirely, so
/// the empty one `Runner::new` builds there is correct, and the thing being
/// started is the helper.
///
/// This was one check for both platforms until a real Windows run met it.
/// An empty wrapper is normal on Windows, so every plan was refused with
/// "No privilege wrapper is configured." before it reached the seam that
/// does not need one.
fn elevation_program(wrapper: &[String], helper: &Path) -> Result<String, String> {
    #[cfg(unix)]
    {
        let _ = helper;
        match wrapper.split_first() {
            Some((program, _)) => Ok(program.clone()),
            None => Err("No privilege wrapper is configured.".to_string()),
        }
    }
    #[cfg(windows)]
    {
        let _ = wrapper;
        Ok(helper.display().to_string())
    }
}

/// Where the helper is on this machine: `BROKEY_HELPER` for development, then
/// beside the running executable (a `cargo run` build, or a test binary in
/// `target/debug/deps` with the helper one level up), then the packaged
/// locations.
#[cfg(unix)]
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

/// Where a packaged helper lives on Windows: beside the application, which
/// is where the installer puts it. There is no system-wide libexec here.
#[cfg(windows)]
pub fn locate_helper() -> Option<PathBuf> {
    // `BROKEY_HELPER` wins, as on Linux, so a build tree can point at the
    // helper it just built.
    if let Ok(path) = std::env::var("BROKEY_HELPER") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    let beside = std::env::current_exe()
        .ok()?
        .parent()?
        .join("brokey-helper.exe");
    beside.is_file().then_some(beside)
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

/// One line of a child's output, and which stream it came from.
pub struct Line {
    pub text: String,
    pub stderr: bool,
}

/// Read the child's stdout and stderr on two threads into one channel, so a
/// child that fills one pipe while the other is being read cannot block.
pub(crate) fn stream_lines(child: &mut Child) -> mpsc::Receiver<Line> {
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

/// One reader's lines, for a transport that has a single stream. A named
/// pipe has no separate error stream, so everything that arrives is
/// output. `stream_lines` is the two-stream version, for piped stdio.
#[cfg(windows)]
pub(crate) fn stream_lines_from(reader: impl Read + Send + 'static) -> mpsc::Receiver<Line> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || pump(reader, false, tx));
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
#[cfg(unix)]
unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}
#[cfg(unix)]
const SIGTERM: i32 = 15;
#[cfg(unix)]
const SIGKILL: i32 = 9;

/// Stop the child and everything in its process group: SIGTERM, a moment
/// for it to finish, then SIGKILL.
#[cfg(unix)]
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

/// Stop the child. Windows has no process group to signal, so this reaches
/// the child itself and not its descendants: an installer the step started
/// is left to finish, which is the truthful thing to promise and is what
/// `CANCELLING_SESSION` says.
#[cfg(windows)]
fn kill_group(child: &mut Child) {
    let _ = child.kill();
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("").trim()
}

/// What pkexec's 127 actually was.
///
/// pkexec answers 127 for the caller not being authorised, for an
/// authentication that could never be put, and for a helper it could not
/// execute, and only its own output says which. Leading with
/// [`NOT_AUTHORISED`] for all three is wrong twice over: nobody was asked,
/// so nothing was refused, and a reader sent to look at their permissions is
/// looking in the place where the answer is not.
///
/// The line quoted back is the one that matched rather than the first,
/// because pkexec writes its prompt and its complaint in either order and a
/// reader needs the complaint.
fn pkexec_refusal(stderr: &str) -> String {
    let matching = |needle: &str| {
        stderr
            .lines()
            .map(str::trim)
            .find(|line| line.to_lowercase().contains(needle))
    };
    // "Error creating textual authentication agent" when pkexec falls back to
    // its own agent and finds no terminal, and "No authentication agent
    // found." when it does not get that far.
    let (sentence, line) = if let Some(line) = matching("authentication agent") {
        (NO_AUTH_AGENT, line)
    } else if let Some(line) = matching("no session for cookie") {
        (NO_POLKIT_SESSION, line)
    } else {
        (NOT_AUTHORISED, first_line(stderr))
    };
    format!("{sentence} {line}").trim_end().to_string()
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

    /// The wrapper `Runner::new` actually builds on this platform, which is
    /// the state a real run meets. On Windows it is empty, and that used to
    /// refuse every plan.
    #[test]
    fn the_default_wrapper_can_start_something() {
        let runner = Runner::new();
        let helper = Path::new("brokey-helper");
        assert!(
            elevation_program(&runner.wrapper, helper).is_ok(),
            "a runner built the ordinary way cannot name what it would start"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_needs_no_wrapper_and_names_the_helper() {
        let helper = Path::new(r"C:\Program Files\Brokey\brokey-helper.exe");
        assert_eq!(
            elevation_program(&[], helper),
            Ok(helper.display().to_string()),
            "ShellExecuteEx elevates the helper itself, so an empty wrapper is right"
        );
    }

    #[cfg(unix)]
    #[test]
    fn linux_without_a_wrapper_cannot_elevate() {
        assert_eq!(
            elevation_program(&[], Path::new("brokey-helper")),
            Err("No privilege wrapper is configured.".to_string())
        );
        assert_eq!(
            elevation_program(&["pkexec".to_string()], Path::new("brokey-helper")),
            Ok("pkexec".to_string())
        );
    }

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

    /// The runner's closed list is the registry-backed one, not the bare
    /// system list. Nothing else in the suite would notice the difference:
    /// `execute_inner` validates every root run against this before the
    /// first prompt, so a runner built with `Allowed::system()` would refuse
    /// every Add/Remove Programs removal, and only a live removal after a
    /// UAC prompt would ever say so.
    ///
    /// The assertion is the wiring rather than a count, because the number
    /// of machine-wide removals is a property of the machine. On one whose
    /// registry records none the two lists are equal anyway and this can
    /// only pass; on any machine with software installed for all users it
    /// can fail, which is every machine this will actually run on.
    #[cfg(windows)]
    #[test]
    fn the_runners_closed_list_is_the_registry_backed_one() {
        assert_eq!(Runner::new().allowed, Allowed::with_registered_removals());
    }

    /// The runner is built on both platforms. Before this, `Runner` did not
    /// exist on Windows at all, so a plan could not even be described there.
    #[test]
    fn a_runner_can_be_built_and_asked_about_its_helper() {
        let runner = Runner::new().with_helper(None);
        assert!(runner.helper().is_none());
    }

    /// Splitting a plan into runs is arithmetic, not a process, so it gives
    /// the same answer on both platforms.
    #[test]
    fn splitting_runs_works_on_every_platform() {
        let steps = vec![step(false, 1), step(true, 1), step(true, 1)];
        let runs = split_runs(&steps);
        assert_eq!(runs.len(), 2);
        assert!(!runs[0].root);
        assert!(runs[1].root);
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

    /// pkexec's 127 is three different answers wearing one number, and the
    /// two that are not refusals used to be reported as one. A user on
    /// Pop!_OS read "you are not authorised" while being perfectly
    /// authorised, and went looking at their group membership.
    #[test]
    fn a_127_reads_as_whichever_127_it_was() {
        assert_eq!(
            pkexec_refusal("No authentication agent found."),
            format!("{NO_AUTH_AGENT} No authentication agent found.")
        );
        assert_eq!(
            pkexec_refusal(
                "Error creating textual authentication agent: Error opening current \
                 controlling terminal for the process"
            ),
            format!(
                "{NO_AUTH_AGENT} Error creating textual authentication agent: Error opening \
                 current controlling terminal for the process"
            )
        );
        assert_eq!(
            pkexec_refusal(
                "==== AUTHENTICATION FAILED ====\npolkit-agent-helper-1: No session for cookie\n"
            ),
            format!("{NO_POLKIT_SESSION} polkit-agent-helper-1: No session for cookie"),
            "the whole of stderr is read, not the first line of it"
        );
        assert_eq!(pkexec_refusal(""), NOT_AUTHORISED);
        assert_eq!(
            pkexec_refusal("Not authorized to perform operation"),
            format!("{NOT_AUTHORISED} Not authorized to perform operation"),
            "anything else is still the refusal it always was"
        );
    }

    #[test]
    fn a_built_package_is_named_by_its_file_and_debug_packages_are_not_it() {
        assert_eq!(
            allow::built_package_name("foo-1.0-1-x86_64.pkg.tar.zst"),
            Some("foo")
        );
        assert_eq!(
            allow::built_package_name("foo-git-r12.abc123-1-x86_64.pkg.tar.zst"),
            Some("foo-git")
        );
        assert_eq!(
            allow::built_package_name("foo-debug-1.0-1-x86_64.pkg.tar.zst"),
            Some("foo-debug")
        );
        assert_eq!(
            allow::built_package_name("foo-1:2.0-3-any.pkg.tar.xz"),
            Some("foo")
        );
        assert_eq!(
            allow::built_package_name("foo-1.0-1-x86_64.pkg.tar.zst.sig"),
            None
        );
        assert_eq!(allow::built_package_name("notes.txt"), None);
        assert_eq!(allow::built_package_name("x-1.pkg.tar.zst"), None);
        let selector =
            allow::built_selector(Path::new("/home/me/.cache/brokey/aur/out/foo"), "foo");
        assert_eq!(
            selector,
            "/home/me/.cache/brokey/aur/out/foo/foo-*.pkg.tar*"
        );
        assert_eq!(
            allow::parse_built_selector(&selector),
            Some((Path::new("/home/me/.cache/brokey/aur/out/foo"), "foo"))
        );
        assert_eq!(
            allow::parse_built_selector("/var/cache/pacman/pkg/foo-1-1-any.pkg.tar.zst"),
            None
        );
    }

    #[test]
    fn a_selector_becomes_the_newest_file_of_exactly_that_package() {
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str| std::fs::write(dir.path().join(name), b"x").unwrap();
        write("foo-1.0-1-x86_64.pkg.tar.zst");
        write("foo-1.0-1-x86_64.pkg.tar.zst.sig");
        write("foo-debug-1.0-1-x86_64.pkg.tar.zst");
        write("foobar-1.0-1-x86_64.pkg.tar.zst");
        let mut install = step(true, 1);
        install.command.program = "pacman".to_string();
        install.command.args = vec![
            "-U".to_string(),
            "--noconfirm".to_string(),
            allow::built_selector(dir.path(), "foo"),
        ];
        let mut plan = Plan {
            id: "p".to_string(),
            ops: Vec::new(),
            steps: vec![install.clone()],
        };
        resolve_built(&mut plan).unwrap();
        assert_eq!(
            plan.steps[0].command.args[2],
            dir.path()
                .join("foo-1.0-1-x86_64.pkg.tar.zst")
                .display()
                .to_string()
        );

        install.command.args[2] = allow::built_selector(dir.path(), "missing");
        install.title = "Installing missing".to_string();
        let mut plan = Plan {
            id: "p".to_string(),
            ops: Vec::new(),
            steps: vec![install],
        };
        let e = resolve_built(&mut plan).unwrap_err();
        assert!(
            e.starts_with("Installing missing could not start, because the build left no package named missing"),
            "{e}"
        );
        assert!(e.ends_with('.'));
    }

    #[test]
    fn the_sentences_follow_the_copy_rules() {
        for s in [
            HELPER_MISSING,
            AUTH_CANCELLED,
            NOT_AUTHORISED,
            NO_AUTH_AGENT,
            NO_POLKIT_SESSION,
            CANCELLED,
            CANCELLING_ROOT,
            CANCELLING_SESSION,
            WAITING_FOR_PLAN,
            WAITING_FOR_PACMAN,
        ] {
            assert!(s.ends_with('.'), "{s}");
            assert!(!s.contains('\u{2014}'), "{s}");
        }
        assert!(NOT_AUTHORISED.contains("authorised"), "British spelling");
    }
}
