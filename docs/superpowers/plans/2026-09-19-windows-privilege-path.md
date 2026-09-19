# Windows Privilege Path Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A Windows machine can actually run a plan, so installing, updating and removing software through winget and removing through Add/Remove Programs do something.

**Architecture:** `transaction::runner` and `transaction::allow` are `#[cfg(unix)]` today, so Windows has no execution engine at all. This plan cuts one seam, `transaction::elevate`, which hands the runner a writer, a line receiver and an exit code however the platform starts the helper, then fills it in twice: on Linux by spawning `pkexec` with piped standard streams, exactly as today, and on Windows by creating a named pipe whose DACL admits only the current user and Administrators, elevating `brokey-helper.exe` with `ShellExecuteEx`'s `runas` verb, and letting it connect back. The runner's event loop, the closed list and `CANCEL_LINE` are unchanged in shape.

**Tech Stack:** Rust 2024, `windows-sys 0.61` for the Win32 calls, React 19 for the three sentences that become reachable.

**Spec:** `docs/superpowers/specs/2026-09-19-windows-support-design.md`, the "Privilege" section.

## Global Constraints

- **The window never elevates.** A privileged operation is an entry in `brokey-helper`'s closed list, with a test that the helper refuses anything else. Never an elevation call site outside `transaction/elevate/`.
- **A source never runs anything.** `Source::plan` returns steps; the Runner runs them.
- **Copy rules, in Rust and TypeScript alike:** British spelling, sentence case, full stops in sentences, no em dashes, no emoji, says what happens. Errors name what went wrong and what to do.
- **A module compiles on both platforms.** Only OS-touching functions carry `#[cfg]`, so the pure halves are tested on both. This plan's whole point is that `runner`, `allow` and the helper stop being Linux-only files.
- **Linux behaviour does not change.** Every existing test in `transaction/` and in `crates/brokey-core/tests/transaction.rs` passes untouched. Where a test must move, it moves verbatim.
- **`rust-version` is 1.88.** Let-chains are house style.
- **The page's types are the contract**, checked by `crates/brokey/tests/contract.rs`.

### The verification problem, which shapes this whole plan

**Nothing on this branch can be compiled for Linux on the development machine.** The machine is Windows and there is no cross toolchain. The only Linux verification available is CI, which runs on any pull request: `.github/workflows/ci.yml` has a bare `pull_request:` trigger and a `rust` job whose matrix includes `ubuntu-latest`.

Two consequences, both binding:

1. Tasks 1 to 3 change code that only Linux compiles, and the implementer **cannot run a single one of their tests**. They compile for Windows, reason about the Linux half as the compiler would, and say so plainly. Never write "tests pass" about something that did not run.
2. **Task 4 is a gate, not a formality.** The branch is pushed and a pull request opened, and Linux CI must be green before Task 5 starts. Building the Windows half on top of a Linux refactor that does not compile would mean untangling two unrelated failures at once.

Every task report carries a line headed **Verified by running:** and a line headed **Reasoned, not run:**. A report with only the first is wrong on this branch.

## Verified before this plan was written

These were checked against the real code and the real toolchain. Do not re-derive them, and do not assume anything beside them.

**`transaction/mod.rs`** gates `pub mod allow;`, `pub mod runner;`, `pub use allow::{Allowed, validate, validate_with};` and `pub use runner::{Outcome, Runner, Sink};` with `#[cfg(unix)]`. `CancelToken` is already ungated and stays so.

**`runner.rs`** is 917 lines. Its Unix-only surface, by line:

- 23: `use std::os::unix::process::CommandExt;`
- 58: `CANCELLING_SESSION`, whose sentence names pkexec
- 64: `HELPER_PATHS`, two absolute Linux paths
- 88: the default wrapper, `vec!["pkexec".to_string()]`
- 229: `.process_group(0)` at the end of `run_session`'s builder chain
- 549 to 571: `locate_helper`
- 713 onwards: `kill_group`, and the `unsafe extern "C" { fn kill(...) }` block with `SIGTERM`/`SIGKILL`

**`runner.rs` internals the seam depends on:**

- `struct Line` at 656, private, fields `text` and `stderr`
- `fn stream_lines(child: &mut Child) -> mpsc::Receiver<Line>` at 663, private
- `fn pump(reader: impl Read, stderr: bool, tx: mpsc::Sender<Line>)` at 675, private. **It already takes `impl Read`, so it works over a named pipe with no change at all.** That is why this seam is cheap.
- `execute_inner` at 156 calls `allow::validate_with(&sub_plan(plan, run), &self.allowed)` for every root run, before any prompt.
- `Runner` has `new`, `with_helper`, `with_wrapper`, `with_allowed`, `helper`, `cancel_token`, `execute`.

**`runner.rs`'s inline `mod tests` has exactly one helper, `fn step(root: bool, weight: u32) -> Step`,** hardcoding `SourceKind::Pacman` and program `true`. It has no sink, no `Collected`, no `session_step`, and **not one test that executes a plan.** Do not write a test there that calls `execute`.

**Executing a plan is tested in `crates/brokey-core/tests/transaction.rs`**, which is **`#![cfg(unix)]` at file level** (line 14) and so does not build on Windows at all. Its helpers are `fixture`, `command(program, args)`, `step(title, program, args, root, weight)`, `session(title, program, args)`, `root(title, program, args)`, `plan(steps)`, `run_collect(runner, plan) -> (Outcome, Vec<Event>)` and `shape(event) -> String`. A Linux-only runner test belongs in this file and uses these names.

**`allow.rs`'s inline `mod tests` helpers are `step(program, args)` (hardcodes `SourceKind::Pacman` and `needs_root: true`), `plan_of(steps)`, `allowed()`, `ok(program, args)` and `refused(program, args) -> String`.** Note the last one: **a test helper is already called `refused`**, so a production function of that name in the same file is a collision. The Linux refusal sentence begins `"The helper refused a step it does not allow: {program}"` and ends with a full stop; `refused` asserts both, and that no em dash appears.

**`allow.rs` public surface:** `Allowed` (with `SYSTEM_PACKAGE_DIRS`, `system()`, `for_home(Option<&Path>)`), `validate(plan)`, `validate_with(plan, allowed)`, `check_step(step, allowed)`. **`Allowed` must stay ungated:** `Runner` holds one in a field and `Runner` is about to exist on Windows.

**`SourceKind`** has 15 variants; the two that matter here are `Winget` and `Arp`, spelt exactly so.

**`brokey-helper/src/main.rs`** is 406 lines and carries `#[cfg(unix)]` on **almost every individual item** — every `use`, every `const`, `main`, `check`, `run` and all their helpers. The Windows `main` at line 28 prints "This build of brokey-helper does not run on Windows yet." and exits 1. Exit codes: 0 finished, 1 a step failed or was cancelled, 2 the plan could not be read, 3 the plan was refused, 4 not root. `run()` reads one JSON plan line from stdin, validates with `allow::validate_with(&plan, &allowed())`, runs the steps, writes one JSON `Event` per line to stdout, and watches stdin for `cancel`.

**`windows-sys 0.61.2`** resolves. The features needed are exactly `Win32_Foundation`, `Win32_Security`, `Win32_Security_Authorization`, `Win32_Storage_FileSystem`, `Win32_System_IO`, `Win32_System_Pipes`, `Win32_System_Registry`, `Win32_System_Threading`, `Win32_UI_Shell`, `Win32_UI_WindowsAndMessaging`. Two of those are not guessable:

- `Win32_System_Registry` is what makes `SHELLEXECUTEINFOW` exist, because it carries an `HKEY`. Without it the import fails and the error does not say why.
- `PIPE_ACCESS_DUPLEX` lives in `Win32::Storage::FileSystem`, **not** in `Win32::System::Pipes` where `CreateNamedPipeW` is.

Verified import paths:

```rust
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, LocalFree};
use windows_sys::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, ConvertSidToStringSidW};
use windows_sys::Win32::Security::{GetTokenInformation, SECURITY_ATTRIBUTES, TOKEN_QUERY, TokenUser};
use windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
use windows_sys::Win32::System::Pipes::{ConnectNamedPipe, CreateNamedPipeW};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::Win32::UI::Shell::{SHELLEXECUTEINFOW, ShellExecuteExW};
```

`size_of::<SHELLEXECUTEINFOW>()` is 112 on x86-64. `CreateNamedPipeW`'s signature is `(lpname: PCWSTR, dwopenmode: FILE_FLAGS_AND_ATTRIBUTES, dwpipemode: NAMED_PIPE_MODE, nmaxinstances: u32, noutbuffersize: u32, ninbuffersize: u32, ndefaulttimeout: u32, lpsecurityattributes: *const SECURITY_ATTRIBUTES) -> HANDLE`.

**Frontend.** `frontend/src/activity/FlowDialog.tsx` already solves this problem for the confirm dialog: `isWindows()` at line 16, `elevatedCopy(platform)` at 26 returning `{ badge: "needs Administrator", title: "This step runs as Administrator. Windows will ask you to allow it." }`, and the platform read at 123 as `const platform = useShell((s) => s.system?.platform);`. `frontend/src/types.ts:257` has `platform: "linux" | "windows"`. The three sentences that become wrong are `ActivityPanel.tsx:100`, `ActivityPanel.tsx:119` and `shell/Shell.tsx:98` — note `shell/`, not `components/`. **`ActivityPanel.tsx` does not read the platform at all today.**

## File structure

```
crates/brokey-core/src/transaction/
  mod.rs          stops gating allow and runner
  elevate/
    mod.rs        the seam: `Elevated`, `start`, what the runner needs
    unix.rs       pkexec with piped standard streams, moved out of runner.rs
    windows.rs    named pipe with a DACL, then ShellExecuteEx runas
  runner.rs       platform-neutral; its Unix-only lines move or gain a cfg
  allow.rs        platform-neutral; a second closed list behind cfg
crates/brokey-core/tests/transaction.rs   the Linux seam test
crates/brokey-helper/src/main.rs   de-gated, then given a Windows body
crates/brokey/src/commands.rs      stops refusing a plan on Windows
frontend/src/activity/ActivityPanel.tsx   platform-aware running-state copy
frontend/src/shell/Shell.tsx              the same, one sentence
```

---

### Task 1: Cut the elevate seam, on Linux, changing nothing

The point of this task is that Linux behaviour is provably identical afterwards. No Windows code appears yet, and **nothing in this task can be run on the development machine** — `elevate/unix.rs` is not even compiled there. Task 4 is where it gets verified.

**Files:**
- Create: `crates/brokey-core/src/transaction/elevate/mod.rs`
- Create: `crates/brokey-core/src/transaction/elevate/unix.rs`
- Modify: `crates/brokey-core/src/transaction/mod.rs`
- Modify: `crates/brokey-core/src/transaction/runner.rs`
- Modify: `crates/brokey-core/tests/transaction.rs`

**Interfaces:**
- Produces:
  - `pub struct Elevated { pub input: Option<Box<dyn std::io::Write + Send>>, pub lines: std::sync::mpsc::Receiver<crate::transaction::runner::Line> }`
  - `pub fn start(helper: &std::path::Path, wrapper: &[String]) -> std::io::Result<Elevated>`
  - `impl Elevated { pub fn wait(&mut self) -> std::io::Result<Option<i32>>; }`
- Consumes: `runner::Line` and `runner::stream_lines`, both currently private and both made reachable by this task.

**There is deliberately no `kill`.** `run_helper` has never killed the elevated child and must not start: it cancels by writing `CANCEL_LINE` down the same stream the plan went down, and waits. `kill_group` at `runner.rs:713` is called from **`run_session`** (line 275) and from nowhere else. The helper's own doc says why: a root child cannot be killed by the user, and killing a package manager mid-transaction is the one thing worse than waiting. Do not give the seam a `kill`; Windows cancels the same way, down the pipe.

- [ ] **Step 1: Read `run_helper` before changing it**

Read `crates/brokey-core/src/transaction/runner.rs` from the start of `run_helper` (line 320) to the end of the function, plus `stream_lines` (663), `pump` (675) and `kill_group` (713). You are moving the spawning, not the event loop. List in your report the exact line range you moved.

- [ ] **Step 2: Write the failing test**

This goes in `crates/brokey-core/tests/transaction.rs`, which is already `#![cfg(unix)]`. It uses that file's existing imports; add `use brokey_core::transaction::elevate;`.

```rust
/// The seam gives back the same three things the runner used to take off
/// the child directly: somewhere to write the plan, somewhere to read
/// lines, and an exit code. If this breaks, the helper cannot be spoken
/// to at all.
#[test]
fn the_elevate_seam_carries_a_plan_and_brings_back_lines() {
    // `cat` stands in for the helper: whatever is written to it comes
    // straight back on its output, which is exactly the shape the seam has
    // to carry. `env` stands in for pkexec, as elsewhere in this file.
    let mut e = elevate::start(Path::new("/bin/cat"), &["env".to_string()])
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
```

`/bin/cat` is given `run` as an argument by `start` and ignores it, which is what makes it usable as a stand-in.

- [ ] **Step 3: Run it and watch it fail**

Run: `cargo test -p brokey-core --test transaction`

Expected on Linux: FAIL, `could not find elevate in transaction`.

Expected on this Windows machine: the whole file is `#![cfg(unix)]`, so **zero tests run and the command reports success**. That is not a pass. Say so in your report.

- [ ] **Step 4: Create the seam**

`crates/brokey-core/src/transaction/elevate/mod.rs`:

```rust
//! How an elevated `brokey-helper` is started and spoken to.
//!
//! The runner does not care how the privilege was obtained. It needs three
//! things: somewhere to write one JSON Plan and possibly a cancel line,
//! somewhere to read one JSON Event per line, and an exit code at the end.
//! Linux gets them from `pkexec`'s standard streams. Windows cannot:
//! `ShellExecuteEx` is the only call that elevates and it cannot redirect
//! standard streams, while `CreateProcess` can redirect them and cannot
//! elevate. So on Windows the two sides meet on a named pipe instead.
//!
//! This module is the only place in the workspace that obtains privilege.

use crate::transaction::runner::Line;
use std::io::Write;
use std::path::Path;
use std::sync::mpsc::Receiver;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix::Inner;

/// One elevated helper run, however the platform started it.
pub struct Elevated {
    /// The plan goes down this. Taken by the writer thread, which owns it
    /// so that the far end sees a close when that thread ends.
    pub input: Option<Box<dyn Write + Send>>,
    /// Whatever the helper wrote, one line at a time.
    pub lines: Receiver<Line>,
    inner: Inner,
}

impl Elevated {
    /// Wait for the helper to finish. `None` when the platform reported no
    /// code, which is not by itself a failure.
    ///
    /// There is no `kill` beside this on purpose. A running helper is
    /// stopped by writing `CANCEL_LINE` to `input`, which lets it finish the
    /// step it is on; killing a package manager part-way through is worse
    /// than waiting for it, and on Linux the child belongs to root and could
    /// not be killed from here anyway.
    pub fn wait(&mut self) -> std::io::Result<Option<i32>> {
        self.inner.wait()
    }
}

/// Start `helper` with privilege. `wrapper` is the program and any leading
/// arguments that obtain it, `pkexec` on Linux; it is ignored on a platform
/// that has its own way.
pub fn start(helper: &Path, wrapper: &[String]) -> std::io::Result<Elevated> {
    #[cfg(unix)]
    {
        unix::start(helper, wrapper)
    }
    #[cfg(not(unix))]
    {
        let _ = (helper, wrapper);
        Err(std::io::Error::other(
            "Brokey cannot obtain Administrator on this system.",
        ))
    }
}
```

`crates/brokey-core/src/transaction/elevate/unix.rs`:

```rust
//! Privilege through `pkexec`, with the helper's standard streams piped.
//!
//! This is the only place in the workspace that spawns `pkexec`.

use super::Elevated;
use crate::transaction::runner::stream_lines;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};

pub struct Inner {
    child: Child,
}

impl Inner {
    pub fn wait(&mut self) -> std::io::Result<Option<i32>> {
        self.child.wait().map(|status| status.code())
    }
}

pub fn start(helper: &Path, wrapper: &[String]) -> std::io::Result<Elevated> {
    let Some((program, leading)) = wrapper.split_first() else {
        return Err(std::io::Error::other(
            "No privilege wrapper is configured, so nothing can be run as root.",
        ));
    };
    let mut child = Command::new(program)
        .args(leading)
        .arg(helper)
        .arg("run")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()?;
    let input: Option<Box<dyn Write + Send>> = child
        .stdin
        .take()
        .map(|stdin| Box::new(stdin) as Box<dyn Write + Send>);
    let lines = stream_lines(&mut child);
    Ok(Elevated {
        input,
        lines,
        inner: Inner { child },
    })
}
```

The `.process_group(0)` here is the one `run_helper` already had on its own spawn. `run_session`'s separate `.process_group(0)` at line 229 is a different call and is **not** touched by this task.

- [ ] **Step 5: Make `Line` and `stream_lines` reachable**

In `runner.rs`: `struct Line` becomes `pub struct Line` with both fields `pub`; `fn stream_lines` becomes `pub(crate) fn stream_lines`. **`kill_group` does not change** — it belongs to `run_session` and the seam has no use for it. Add `pub mod elevate;` to `transaction/mod.rs`, ungated, re-exporting nothing from it.

`Line` is `pub` rather than `pub(crate)` because `Elevated::lines` is a public field of a public type, so the type it yields must be nameable from outside the crate. Give `Line` a doc comment saying what it is: one line of the helper's output, and which stream it came from.

- [ ] **Step 6: Use the seam in `run_helper`**

Replace, in `run_helper` only: the `Command::new(...)...spawn()` at lines 335 to 344, the `child.stdin.take()` at 351, the `stream_lines(&mut child)` at 371, and the `child.wait()` at the end. Everything else stays exactly as it is — the writer thread and its comment, the `cancel_tx`/`cancel_rx` channel, the event loop, the exit-code interpretation at line 486, and every sentence.

The writer thread now takes `e.input.take()`; the loop reads `e.lines`; the exit code comes from `e.wait()`. **Cancellation is untouched**: it already goes through `cancel_tx` to the writer thread, which writes `CANCEL_LINE`, and that is the only way the helper is ever stopped.

`start` returns `io::Result` and `run_helper` returns `Result<(), String>`, so map the error into a sentence that names the wrapper, in the register of the sentences already in that function — the one it replaces reads `"Could not start {program}: {e}."`.

- [ ] **Step 7: Build for Windows and reason about Linux**

Run: `cargo build -p brokey-core && cargo test -p brokey-core --lib`

Expected: PASS. This proves only that the ungated half compiles.

Then read every line you wrote under `#[cfg(unix)]` as the Linux compiler would, and write in your report: each item's gate, whether every `use` is used on its own side, and whether `run_helper` still names every variable it uses. State plainly that the Linux half is reasoned, not run.

- [ ] **Step 8: Commit**

```bash
git add crates/brokey-core/src/transaction/ crates/brokey-core/tests/transaction.rs
git commit -m "Runner: put a seam where privilege is obtained"
```

---

### Task 2: Make `runner` and `allow` compile on Windows

After this task both modules exist on both platforms, and Windows refuses a root run with a sentence rather than failing to build.

**Files:**
- Modify: `crates/brokey-core/src/transaction/mod.rs`
- Modify: `crates/brokey-core/src/transaction/runner.rs`
- Modify: `crates/brokey-core/src/transaction/allow.rs`

**Interfaces:**
- Consumes: `elevate::start` from Task 1.
- Produces: `transaction::runner` and `transaction::allow` built on both platforms; `allow::validate_with`, `allow::validate`, `allow::check_step` and `Allowed` all callable on Windows; `runner::locate_helper` with a Windows arm.

- [ ] **Step 1: Write the failing test**

Add to `runner.rs`'s inline `mod tests`, ungated, using that module's existing `step(root, weight)` helper and nothing else. These tests deliberately do **not** execute a plan: that module has never executed one and has no sink.

```rust
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
```

Read `runs_join_consecutive_root_steps_only` first and match how it inspects a `Run`; if `Run`'s fields are spelt differently from `root`, use the real names.

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test -p brokey-core --lib transaction::runner`

Expected on Windows: FAIL to compile, because the module is not built there.

- [ ] **Step 3: Ungate the modules**

In `transaction/mod.rs`, remove `#[cfg(unix)]` from `pub mod allow;`, `pub mod runner;`, the `pub use allow::{...}` line and the `pub use runner::{...}` line.

Then fix the module doc, which is now wrong in three places. Lines 6 to 8 say `allow` is "Linux's shape of it (Windows will get its own in a later plan)"; lines 10 to 11 call `runner` "The only `pkexec` call site"; line 12 says `runner` is "Linux only until a later plan gives Windows its own privilege path". The first and third described this plan. The second moved to `elevate/unix.rs` in Task 1. Replace all three with what is now true.

- [ ] **Step 4: Gate only what actually touches Unix, in `runner.rs`**

- `#[cfg(unix)]` on `use std::os::unix::process::CommandExt;` (line 23).
- `run_session`'s builder chain at line 229 ends in `.process_group(0)`. Unpick it so the cfg attaches to a statement rather than to a link in a chain: bind the builder to a `let mut process = ...;` without that call, then

```rust
// A session step gets its own process group so cancelling reaches the
// whole tree. Windows has no equivalent and does not need one here.
#[cfg(unix)]
process.process_group(0);
```

  before `spawn()`.

- `#[cfg(unix)]` on the `unsafe extern "C" { fn kill(...) }` block, on `SIGTERM`, on `SIGKILL` and on `kill_group`. **A Windows `kill_group` is genuinely needed**, because `run_session` calls it at line 275 and `run_session` is not gated — a cancelled session step on Windows reaches that line:

```rust
/// Stop the child. Windows has no process group to signal, so this reaches
/// the child itself and not its descendants: an installer the step started
/// is left to finish, which is the truthful thing to promise and is what
/// `CANCELLING_SESSION` says.
#[cfg(windows)]
fn kill_group(child: &mut Child) {
    let _ = child.kill();
}
```

  Keep it private, exactly as the Linux one is. Nothing outside `runner.rs` calls it.

- **`CANCELLING_SESSION` is reachable on Windows and its sentence is wrong there.** It is emitted at line 273, inside `run_session`, which now runs on both platforms; it currently reads "Cancelling. A package manager this step started under pkexec finishes first; nothing after it will start." Nothing on Windows goes through pkexec. Split it, the way the rest of this plan splits copy — whole sentences, never one sentence with a word swapped:

```rust
#[cfg(unix)]
pub const CANCELLING_SESSION: &str = "Cancelling. A package manager this step started under pkexec finishes first; nothing after it will start.";
#[cfg(windows)]
pub const CANCELLING_SESSION: &str = "Cancelling. An installer this step started finishes first; nothing after it will start.";
```

  `crates/brokey-core/tests/transaction.rs:584` compares an event message against `runner::CANCELLING_SESSION` by name, so it keeps working unchanged on Linux. Check line 910 of `runner.rs`, which also names the constant, and make sure whatever it asserts still holds on both.

- `#[cfg(unix)]` on `HELPER_PATHS` and on the existing `locate_helper`. Read the existing one first, including how it handles `BROKEY_HELPER`, and mirror that behaviour exactly in:

```rust
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
```

  If the Linux one does not in fact consult `BROKEY_HELPER`, drop that block rather than inventing a difference, and say so in your report.

- The default wrapper at line 88 is `vec!["pkexec".to_string()]`. Windows has no wrapper program, because `ShellExecuteEx` is a call and not a command:

```rust
#[cfg(unix)]
let wrapper = vec!["pkexec".to_string()];
#[cfg(windows)]
let wrapper = Vec::new();
```

- [ ] **Step 5: Split `allow.rs` by platform**

`Allowed` stays **ungated**: `Runner` holds one in a field, `Runner::with_allowed` takes one, and `execute_inner` passes it to `validate_with`, all of which now exist on Windows. `Allowed::SYSTEM_PACKAGE_DIRS` is two Linux paths, which is harmless as data on Windows; leave it, and leave `system()` and `for_home` as they are.

Gate with `#[cfg(unix)]`: the Linux `validate_with`, `check_step`, and every helper only they use. `validate(plan)` is a thin wrapper over `validate_with(plan, &Allowed::system())`; keep it ungated so both platforms have it.

Add, for now, a Windows `validate_with` that refuses everything. Task 3 replaces it; it exists so this task's commit builds and so the refusal is a sentence rather than a compile error.

```rust
/// Placeholder until the Windows closed list lands in the next task.
#[cfg(windows)]
pub fn validate_with(plan: &Plan, allowed: &Allowed) -> Result<(), String> {
    let _ = (plan, allowed);
    Err("Brokey cannot run anything as Administrator yet.".to_string())
}
```

`allow.rs`'s inline `mod tests` is entirely about the Linux list. Gate the whole `mod tests` with `#[cfg(unix)]` rather than picking through it; Task 3 adds a Windows test module beside it.

- [ ] **Step 6: Build and test on this machine**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets`

Expected: PASS, clean.

This is the first time `transaction::runner`'s inline tests have ever been built on Windows. Expect to fix compile errors in the test module itself, not only in the code. Any existing test that is genuinely Linux-only — one asserting a `pkexec` argument, one using a `/` path — gets `#[cfg(unix)]` and a one-line comment saying why. **Do not delete a test to make it compile**, and do not weaken an assertion.

- [ ] **Step 7: Reason about Linux**

Write in your report a table: every item you gated, which side it is on, and whether its `use` statements are used on that side. Linux is reasoned, not run.

- [ ] **Step 8: Commit**

```bash
git add crates/brokey-core/src/transaction/
git commit -m "Runner and allow: compile on Windows, refuse to elevate there"
```

---

### Task 3: The Windows closed list

**Files:**
- Modify: `crates/brokey-core/src/transaction/allow.rs`

**Interfaces:**
- Consumes: the Task 2 placeholder, which it replaces.
- Produces: `#[cfg(windows)] validate_with` and `#[cfg(windows)] check_step` with the same signatures the Linux pair has, so `execute_inner` and the helper call one name on both platforms.

- [ ] **Step 1: Write the failing tests**

A new test module, beside the `#[cfg(unix)] mod tests` Task 2 gated. The existing test helpers are Linux-shaped — `step` hardcodes `SourceKind::Pacman` and `refused` asserts the Linux sentence — so this module gets its own, named differently so that nobody reading both side by side confuses them.

```rust
#[cfg(windows)]
#[cfg(test)]
mod windows_tests {
    use super::*;
    use crate::model::{Command, SourceKind};

    fn step_from(source: SourceKind, program: &str, args: &[&str]) -> Step {
        Step {
            source,
            title: "Test".to_string(),
            command: Command {
                program: program.to_string(),
                args: args.iter().map(|a| a.to_string()).collect(),
                env: Vec::new(),
                cwd: None,
            },
            needs_root: true,
            weight: 1,
        }
    }

    fn plan_of(steps: Vec<Step>) -> Plan {
        Plan {
            id: "test".to_string(),
            ops: Vec::new(),
            steps,
        }
    }

    /// The three things winget is asked to do, and nothing else.
    #[test]
    fn the_winget_verbs_are_allowed() {
        for verb in ["install", "upgrade", "uninstall"] {
            let plan = plan_of(vec![step_from(
                SourceKind::Winget,
                "winget.exe",
                &[verb, "--id", "Valve.Steam"],
            )]);
            assert_eq!(validate(&plan), Ok(()), "{verb} should be allowed");
        }
    }

    /// A verb that is not one of the three is refused even from winget.
    #[test]
    fn another_winget_verb_is_refused() {
        let plan = plan_of(vec![step_from(
            SourceKind::Winget,
            "winget.exe",
            &["export", "--output", "C:/everything.json"],
        )]);
        let err = validate(&plan).expect_err("export is not on the list");
        assert!(err.contains("export"), "the refusal names the verb: {err}");
        assert!(err.ends_with('.'), "the reason is a sentence: {err}");
        assert!(!err.contains('\u{2014}'), "no em dashes: {err}");
    }

    /// The list is about the program, not the source that claims it. A step
    /// that says it is winget and runs something else is refused.
    #[test]
    fn a_step_cannot_claim_to_be_winget_and_run_something_else() {
        let plan = plan_of(vec![step_from(
            SourceKind::Winget,
            "powershell.exe",
            &["-Command", "Remove-Item C:/Windows -Recurse"],
        )]);
        let err = validate(&plan).expect_err("the program decides, not the source");
        assert!(err.contains("powershell.exe"), "{err}");
    }

    /// A full path to winget is the same program as a bare name. Both the
    /// registry and the catalogue produce absolute paths.
    #[test]
    fn winget_is_recognised_by_a_full_path_too() {
        let plan = plan_of(vec![step_from(
            SourceKind::Winget,
            r"C:\Program Files\WindowsApps\winget.exe",
            &["install", "--id", "Valve.Steam"],
        )]);
        assert_eq!(validate(&plan), Ok(()));
    }

    /// An uninstall string out of the registry is whatever the installer
    /// wrote, so nothing about its shape can be required.
    #[test]
    fn an_add_remove_programs_removal_is_allowed() {
        let plan = plan_of(vec![step_from(
            SourceKind::Arp,
            r"C:\Program Files\Thing\unins000.exe",
            &["/SILENT"],
        )]);
        assert_eq!(validate(&plan), Ok(()));
    }

    /// A source with no Windows business is refused whatever it runs.
    #[test]
    fn a_step_from_another_platforms_source_is_refused() {
        let plan = plan_of(vec![step_from(
            SourceKind::Pacman,
            "pacman",
            &["-S", "steam"],
        )]);
        let err = validate(&plan).expect_err("pacman does not run on Windows");
        assert!(err.contains("Pacman"), "the refusal names the source: {err}");
    }

    /// A session step is not the helper's business at all. The Linux list
    /// makes the same check; this is the Windows half of it.
    #[test]
    fn a_session_step_never_reaches_the_helper() {
        let mut step = step_from(SourceKind::Winget, "winget.exe", &["install"]);
        step.needs_root = false;
        assert_eq!(validate(&plan_of(vec![step])), Ok(()));
    }

    /// One bad step refuses the whole plan, so a plan is never half run.
    #[test]
    fn one_bad_step_refuses_the_whole_plan() {
        let plan = plan_of(vec![
            step_from(SourceKind::Winget, "winget.exe", &["install", "--id", "A"]),
            step_from(SourceKind::Winget, "cmd.exe", &["/c", "whoami"]),
        ]);
        assert!(validate(&plan).is_err());
    }
}
```

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test -p brokey-core --lib transaction::allow`

Expected: FAIL, every one of them, refused by the Task 2 placeholder.

- [ ] **Step 3: Write the list**

Replace the placeholder. **Do not name this helper `refused`:** `allow.rs` already has a test helper of that name, and two things called `refused` in one file is how a reader loses an afternoon.

```rust
/// What may run as Administrator on Windows.
///
/// Two things, and the reasoning differs for each.
///
/// `winget.exe` is a fixed program with a fixed set of verbs, so it is
/// checked the way the Linux list checks a package manager: the program is
/// named and the verb comes from a short list.
///
/// A removal from Add/Remove Programs is not checkable that way. The
/// command is whatever the installer wrote into the registry years ago, so
/// no requirement about its shape would be honest. It is admitted because
/// the step came from the Add/Remove Programs source, which read it out of
/// the registry rather than inventing it, and because the confirm dialog
/// shows the user that exact command line before anything runs. Widening
/// this is a deliberate edit here with a test beside it.
#[cfg(windows)]
const WINGET_VERBS: [&str; 3] = ["install", "upgrade", "uninstall"];

#[cfg(windows)]
pub fn validate_with(plan: &Plan, allowed: &Allowed) -> Result<(), String> {
    for step in plan.steps.iter().filter(|s| s.needs_root) {
        check_step(step, allowed)?;
    }
    Ok(())
}

#[cfg(windows)]
pub fn check_step(step: &Step, allowed: &Allowed) -> Result<(), String> {
    // Windows has no equivalent of the package-file directories the Linux
    // list guards: nothing here is handed a file to install.
    let _ = allowed;
    let program = Path::new(&step.command.program)
        .file_name()
        .map(|name| name.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    match step.source {
        SourceKind::Winget => {
            if program != "winget.exe" {
                return Err(not_allowed(&step.command.program));
            }
            let verb = step.command.args.first().map(String::as_str).unwrap_or("");
            if !WINGET_VERBS.contains(&verb) {
                return Err(not_allowed(verb));
            }
            Ok(())
        }
        SourceKind::Arp => Ok(()),
        other => Err(not_allowed(&format!("{other:?}"))),
    }
}

/// The one sentence a refusal produces, in the register the Linux list
/// uses: what was refused, and what the rule is.
#[cfg(windows)]
fn not_allowed(what: &str) -> String {
    format!(
        "The helper refused a step it does not allow: {what}. Only winget and a \
         removal Windows itself recorded may run as Administrator."
    )
}
```

`SourceKind` must be in scope in the non-test half of the file; `allow.rs` currently imports only `Command, Plan, Step` from `crate::model`, so add `SourceKind` to that import under the cfg that needs it, or use the full path.

- [ ] **Step 4: Run and watch them pass**

Run: `cargo test -p brokey-core --lib transaction::allow`

Expected: PASS, all eight.

- [ ] **Step 5: Commit**

```bash
git add crates/brokey-core/src/transaction/allow.rs
git commit -m "Allow: the Windows closed list"
```

---

### Task 4: Gate — have Linux compile this before building on it

**This task writes no feature code.** Tasks 1 to 3 changed code only Linux compiles, and none of it has been compiled. CI is the only Linux toolchain available. Everything after this task builds on top of that refactor, so it gets verified here, where a failure is unambiguous.

**Files:** none, unless CI finds something.

- [ ] **Step 1: Run everything this machine can run**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets && cargo fmt --all --check`

Expected: PASS, clean. Fix anything that is not before pushing.

- [ ] **Step 2: Push the branch and open a pull request**

Push the branch and open a pull request against `main`, titled "Windows privilege path", with a body saying what Tasks 1 to 3 did and that the point of the pull request is Linux verification. End the body with the attribution lines this session requires.

- [ ] **Step 3: Wait for CI and read the result**

Watch the run. The jobs that matter are `rust ubuntu-latest` and `rust windows-latest`; `packaging`, `deb`, `page` and `arch` should also stay green.

- [ ] **Step 4: Fix whatever Linux found, and say what it was**

Any failure here is a defect in Tasks 1 to 3 that could not have been caught locally. Fix it, commit, push, and wait again. Repeat until `rust ubuntu-latest` is green.

In your report, list every Linux failure CI found and what it was, even where the fix was one character. That list is the measure of how good the reasoning in Tasks 1 to 3 actually was, and it is the most useful thing this task produces.

- [ ] **Step 5: Record the green run**

Note the run id and that `rust ubuntu-latest` passed. Do not proceed while it is red.

---

### Task 5: A named pipe only this user and Administrators can open

**Files:**
- Create: `crates/brokey-core/src/transaction/elevate/windows.rs`
- Modify: `crates/brokey-core/src/transaction/elevate/mod.rs`
- Modify: `crates/brokey-core/Cargo.toml`
- Modify: `Cargo.toml`

**Interfaces:**
- Produces: `pipe_name() -> String`, `sddl_for_current_user() -> io::Result<String>`, `listen(name: &str) -> io::Result<PipeServer>`, and `PipeServer::accept(&mut self) -> io::Result<std::fs::File>`.

- [ ] **Step 1: Add the dependency**

Workspace `Cargo.toml`, under `[workspace.dependencies]`:

```toml
# The Windows privilege path: a named pipe with its own DACL, and
# ShellExecuteEx, which is the only call that elevates.
windows-sys = "0.61"
```

`crates/brokey-core/Cargo.toml`, under `[target."cfg(windows)".dependencies]` beside `windows-registry`:

```toml
windows-sys = { workspace = true, features = [
  "Win32_Foundation",
  "Win32_Security",
  "Win32_Security_Authorization",
  "Win32_Storage_FileSystem",
  "Win32_System_IO",
  "Win32_System_Pipes",
  "Win32_System_Registry",
  "Win32_System_Threading",
  "Win32_UI_Shell",
  "Win32_UI_WindowsAndMessaging",
] }
```

Every feature there is needed and was verified. In particular `Win32_System_Registry` is what makes `SHELLEXECUTEINFOW` exist, and `Win32_Storage_FileSystem` is what makes `CreateNamedPipeW` exist; dropping either gives an unresolved import whose message does not explain itself.

- [ ] **Step 2: Write the failing tests**

In the new `windows.rs`:

```rust
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

    /// The descriptor names this user and the Administrators group and
    /// nobody else. `BA` is the Administrators alias; the user arrives as a
    /// SID string, so this checks the shape rather than a fixed value.
    #[test]
    fn the_descriptor_admits_this_user_and_administrators() {
        let sddl = sddl_for_current_user().expect("this user has a SID");
        assert!(sddl.starts_with("D:"), "{sddl}");
        assert!(sddl.contains("(A;;GA;;;BA)"), "{sddl}");
        assert!(sddl.contains("(A;;GA;;;S-1-5-"), "{sddl}");
        assert!(!sddl.contains(";;;WD)"), "everyone can open it: {sddl}");
        assert!(!sddl.contains(";;;AN)"), "anonymous can open it: {sddl}");
    }

    /// The descriptor is not merely a plausible string: Windows parses it
    /// and creates the pipe, or this fails.
    #[test]
    fn windows_accepts_the_descriptor() {
        let name = pipe_name();
        let server = listen(&name).expect("the pipe is created");
        drop(server);
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
```

- [ ] **Step 3: Run and watch them fail**

Run: `cargo test -p brokey-core --lib transaction::elevate`

Expected: FAIL, the module does not exist.

- [ ] **Step 4: Implement**

```rust
//! The Windows end of the privilege seam.
//!
//! `ShellExecuteEx` is the only call that elevates and it cannot redirect
//! standard streams; `CreateProcess` can redirect them and cannot elevate.
//! So the unelevated side listens on a named pipe first, hands the name to
//! the elevated helper on its command line, and the helper connects back.
//! The pipe's DACL admits this user and the Administrators group and
//! nobody else, so nothing else on the machine can answer in the helper's
//! place or listen to what passes.

use std::io;

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
```

Then write `current_user_sid`, which opens the process token with `OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, ...)`, calls `GetTokenInformation(TokenUser, ...)` twice — once for the size, once for the data — and converts with `ConvertSidToStringSidW`, freeing the result with `LocalFree`.

Then `PipeServer` and `listen`, which build a `SECURITY_ATTRIBUTES` whose `lpSecurityDescriptor` comes from `ConvertStringSecurityDescriptorToSecurityDescriptorW` over the SDDL (revision 1), call `CreateNamedPipeW` with `PIPE_ACCESS_DUPLEX`, `PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT`, one instance and sensible buffer sizes, `LocalFree` the descriptor once the pipe holds its own copy, and return the handle in an `OwnedHandle` so it is closed exactly once by the type rather than by hand.

`PipeServer::accept` calls `ConnectNamedPipe` and turns the handle into a `std::fs::File` with `File::from_raw_handle`, so everything above it reads and writes an ordinary stream. Treat `ERROR_PIPE_CONNECTED` (535) from `ConnectNamedPipe` as success: a client that opened the pipe before the call gets that instead of a zero return, and the test above can produce exactly that race.

Rules for the unsafe code, which the reviewer will check:

- one call per `unsafe` block, each with a `// SAFETY:` comment saying what makes it sound
- every failure returns `io::Error::last_os_error()`, never a bare `unwrap`
- `INVALID_HANDLE_VALUE` is checked for, not assumed away
- no raw handle outlives the `OwnedHandle` that owns it

- [ ] **Step 5: Run and watch them pass**

Run: `cargo test -p brokey-core --lib transaction::elevate`

Expected: PASS, all four.

- [ ] **Step 6: Commit**

```bash
git add crates/brokey-core/src/transaction/elevate/ crates/brokey-core/Cargo.toml Cargo.toml Cargo.lock
git commit -m "Elevate: a named pipe this user and Administrators can open"
```

---

### Task 6: Elevate the helper and meet it on the pipe

**Files:**
- Modify: `crates/brokey-core/src/transaction/elevate/windows.rs`
- Modify: `crates/brokey-core/src/transaction/elevate/mod.rs`
- Modify: `crates/brokey-core/src/transaction/runner.rs`

**Interfaces:**
- Consumes: `pipe_name`, `listen`, `PipeServer` from Task 5.
- Produces: `windows::start(helper: &Path, wrapper: &[String]) -> io::Result<Elevated>` and a `windows::Inner` with `wait` and `kill`, matching the Unix shape Task 1 defined; `helper_arguments(pipe: &str) -> Vec<String>`, whose `--pipe` spelling Task 8 parses; `runner::stream_lines_from`.

- [ ] **Step 1: Write the failing tests**

These go **inside the `#[cfg(test)] mod tests` that Task 5 created in `windows.rs`**, beside its four, not in a second module.

```rust
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
```

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test -p brokey-core --lib transaction::elevate`

Expected: FAIL, `cannot find function helper_arguments`.

- [ ] **Step 3: Implement the pure parts**

```rust
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

/// ShellExecuteEx takes one parameter string, so the arguments are joined
/// and anything with a space in it is quoted. A pipe name never contains a
/// space; a helper path very often does.
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
```

- [ ] **Step 4: Implement `start`**

In order, and the order matters:

1. `let name = pipe_name();` then `let mut server = listen(&name)?;` — **listen before elevating**, or the helper connects to nothing.
2. Build `SHELLEXECUTEINFOW` with `cbSize: shell_execute_info_size()`, `lpVerb` the wide string `runas`, `lpFile` the helper path wide, `lpParameters` `join_arguments(&helper_arguments(&name))` wide, `nShow: SW_HIDE`, `fMask: SEE_MASK_NOCLOSEPROCESS` so `hProcess` comes back and can be waited on. Keep every wide string alive in a binding until after the call; a temporary's pointer is a dangling one.
3. `ShellExecuteExW`. A zero return means it failed. When `GetLastError()` is `ERROR_CANCELLED` (1223) the user declined the prompt, and that gets its own sentence rather than an OS error: `"You did not allow the change, so nothing was done."` Anything else reports the OS error.
4. `let stream = server.accept()?;` then `let reader = stream.try_clone()?;` so one handle writes and one reads.
5. `let lines = crate::transaction::runner::stream_lines_from(reader);`
6. Return `Elevated { input: Some(Box::new(stream)), lines, inner: Inner { process } }`.

`Inner` has **only** `wait`, matching the Unix `Inner` from Task 1: it waits with `WaitForSingleObject(handle, INFINITE)` then reads `GetExitCodeProcess`. There is no `kill` and no `TerminateProcess` — cancelling writes `CANCEL_LINE` down the pipe, exactly as Linux writes it down stdin, and the helper stops after the step it is on. Same unsafe rules as Task 5. Hold the process handle in an `OwnedHandle`.

- [ ] **Step 5: Add `stream_lines_from` to `runner.rs`**

```rust
/// One reader's lines, for a transport that has a single stream. A named
/// pipe has no separate error stream, so everything that arrives is
/// output. `stream_lines` is the two-stream version, for piped stdio.
#[cfg(windows)]
pub(crate) fn stream_lines_from(
    reader: impl Read + Send + 'static,
) -> mpsc::Receiver<Line> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || pump(reader, false, tx));
    rx
}
```

**The `#[cfg(windows)]` is load-bearing.** Only `elevate/windows.rs` calls this, so without the gate it is dead code on Linux, and CI sets `RUSTFLAGS: -D warnings`, which turns that into a failed Linux build rather than a warning nobody reads.

Read `stream_lines` first and match how it spawns and names its threads. `pump` is reused unchanged, which is the whole reason this is three lines.

- [ ] **Step 6: Wire it into the seam**

In `elevate/mod.rs`: add `#[cfg(windows)] mod windows;` and `#[cfg(windows)] use windows::Inner;`, and give `start` a `#[cfg(windows)]` arm calling `windows::start(helper, wrapper)`. The old error arm narrows to `#[cfg(not(any(unix, windows)))]`.

- [ ] **Step 7: Run the suite**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets`

Expected: PASS, clean. No test elevates, and none may.

- [ ] **Step 8: Commit**

```bash
git add crates/brokey-core/src/transaction/
git commit -m "Elevate: start the helper with runas and meet it on the pipe"
```

---

### Task 7: De-gate the helper and give it a reader and a writer

The helper is 406 lines with `#[cfg(unix)]` on nearly every individual item. The Windows body differs in only two things: where the plan comes from and where events go, and how "am I privileged" is answered. This task makes that structure true. It adds no Windows behaviour, so Linux behaviour must be unchanged.

**Files:**
- Modify: `crates/brokey-helper/src/main.rs`

**Interfaces:**
- Produces: the shared body of `run()` as a function taking a reader and a writer. Choose the exact signature to fit what `run()` actually needs — it also watches for a cancel line — and state it in your report, because Task 8 calls it.

- [ ] **Step 1: Read the whole file**

Read all 406 lines. Write in your report a list: for each `#[cfg(unix)]` item, whether it is genuinely Linux-only (`geteuid`, `CHILD_PATH`, the environment scrub, anything under `std::os::unix`) or merely gated because the whole file was. Most are the latter.

- [ ] **Step 2: Write the failing test**

```rust
/// The helper refuses a plan the closed list does not admit, on every
/// platform. This is the invariant the whole binary exists to hold.
#[test]
fn a_plan_off_the_closed_list_is_refused() {
    let plan = refusable_plan();
    assert!(brokey_core::transaction::allow::validate(&plan).is_err());
}
```

Write `refusable_plan()` to produce a step that is refused on the platform being compiled for: `cmd.exe` from `SourceKind::Winget` on Windows, and whatever the Linux list refuses on Linux. Gate the helper function, not the test. If `main.rs` has no `mod tests` today, add one.

- [ ] **Step 3: Run and watch it fail**

Run: `cargo test -p brokey-helper`

Expected: FAIL to compile on Windows, because the helper's Windows half imports nothing.

- [ ] **Step 4: De-gate**

Remove `#[cfg(unix)]` from every item that is not genuinely Linux-only: the exit-code constants, `USAGE`, `check`, the argument dispatch, the event writing, the step-running loop, the cancel watching. Keep `#[cfg(unix)]` on `geteuid`, `CHILD_PATH`, the environment scrub and any `std::os::unix` import, and give each of those a Windows counterpart or a cfg'd call site.

Extract the body of `run()` into a function taking a reader and a writer, so `run()` on Linux passes `std::io::stdin().lock()` and `std::io::stdout()`, and Task 8's Windows body passes the two ends of the pipe. **The Linux path must be behaviour-identical**: same exit codes, same events, same sentences, same cancel handling.

Update the module doc, whose last paragraph says the helper is "Linux only for now" and that "a later plan gives Windows its own privilege path and its own helper body". That plan is this one.

- [ ] **Step 5: Build and reason**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets`

Expected: PASS, clean.

The Linux half is reasoned, not run. In your report, walk the Linux `run()` path statement by statement against what it was before, and state that it is unchanged — or say exactly what changed and why it had to.

- [ ] **Step 6: Commit**

```bash
git add crates/brokey-helper/src/main.rs
git commit -m "Helper: one body, with the transport passed in"
```

---

### Task 8: The helper's Windows body

**Files:**
- Modify: `crates/brokey-helper/src/main.rs`
- Modify: `crates/brokey-helper/Cargo.toml`

**Interfaces:**
- Consumes: `--pipe <name>` as Task 6's `helper_arguments` spells it, and the shared body from Task 7.

- [ ] **Step 1: Write the failing tests**

```rust
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
```

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test -p brokey-helper`

Expected: FAIL, `cannot find function pipe_argument`.

- [ ] **Step 3: Implement**

```rust
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
    Err("The helper was started without --pipe, so it has no way to answer. \
         Start Brokey rather than running the helper yourself."
        .to_string())
}
```

The Windows `run()` then: takes the arguments, gets the pipe name, opens it with `OpenOptions::new().read(true).write(true).open(name)`, `try_clone()`s it so one handle reads and one writes, and hands both to Task 7's shared body. Everything after that — validation, the step loop, the events, the exit codes — is the code Linux runs.

Replace the `geteuid() != 0` check with the Windows equivalent: `GetTokenInformation` with `TokenElevation` on the current process token, refusing with `EXIT_NOT_ROOT` and a sentence in the same register as the Linux one, naming Administrator instead of root. The helper only ever runs because `ShellExecuteEx` elevated it, so this failing means something is wrong rather than something is unsupported.

Add `windows-sys` to `crates/brokey-helper/Cargo.toml` under `[target."cfg(windows)".dependencies]` with only the features the helper actually calls: `Win32_Foundation`, `Win32_Security`, `Win32_System_Threading`. It creates no pipe and elevates nothing, so it needs none of the rest.

Delete the old Windows `main` that printed "This build of brokey-helper does not run on Windows yet."

- [ ] **Step 4: Run and watch them pass**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets`

Expected: PASS, clean.

- [ ] **Step 5: Commit**

```bash
git add crates/brokey-helper/
git commit -m "Helper: a Windows body that answers on the pipe"
```

---

### Task 9: Let the application run a plan on Windows, and say the right thing while it does

**Files:**
- Modify: `crates/brokey/src/commands.rs`
- Modify: `frontend/src/activity/ActivityPanel.tsx`
- Modify: `frontend/src/shell/Shell.tsx`

**Interfaces:**
- Consumes: everything above; `useShell((s) => s.system?.platform)` as `FlowDialog.tsx:123` already uses it.

- [ ] **Step 1: Find every refusal in `commands.rs`**

`commands.rs` has `#[cfg(unix)]` or `#[cfg(windows)]` arms at lines 147, 151, 261, 264, 266, 272, 397, 404, 419, 427, 446 and 536. Read every one. Some are correct and stay: anything about `launcher` or `start`, which is Linux-only by design and out of scope here. What changes is only an arm that refuses to build or run a plan because the platform is Windows.

List all twelve in your report with a verdict each: changed, or left alone and why.

- [ ] **Step 2: Write the failing test**

Read `commands.rs`'s existing test module first — it already has `Runner::new()` at line 572 and an `execute` at 644, so there is a shape to follow. Write a `#[cfg(windows)]` test that a winget removal reaches the runner rather than being refused by the command layer, using the real names of the preview and plan functions that file exposes.

If building a plan there would need a source that touches the machine, assert on the refusal path instead: that a plan with a winget root step is **not** rejected with a platform sentence. Say in your report which you did and why.

- [ ] **Step 3: Run and watch it fail**

Run: `cargo test -p brokey`

Expected: FAIL with whatever sentence the refusal produces.

- [ ] **Step 4: Remove the refusals**

Change only the arms that refuse a plan. Leave the launcher arms exactly as they are.

- [ ] **Step 5: Change the three sentences**

`frontend/src/activity/FlowDialog.tsx` lines 16 to 32 already solve this for the confirm dialog, with an `isWindows(platform)` helper and an `elevatedCopy(platform)` returning whole sentences rather than one sentence with a word swapped. Read it and follow it. Do not invent a second pattern.

`ActivityPanel.tsx` does not read the platform today; add `const platform = useShell((s) => s.system?.platform);` to the component and thread it into the two functions that return these strings.

- `ActivityPanel.tsx:100`, the short label: `"Waiting for Administrator"` on Windows, `"Waiting for your password"` on Linux.
- `ActivityPanel.tsx:119`, the sentence: `"Allow the change in the Windows dialog."` on Windows, `"Enter your password in the system dialog."` on Linux.
- `Shell.tsx:98`: `"Waiting for Administrator…"` on Windows, `"Waiting for your password…"` on Linux. Keep the ellipsis character that is already in that string.

- [ ] **Step 6: Check both sides**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets`

Run: `cd frontend && npm run lint && npm run check:design`

Expected: PASS, clean.

- [ ] **Step 7: Commit**

```bash
git add crates/brokey/src/commands.rs frontend/src/activity/ActivityPanel.tsx frontend/src/shell/Shell.tsx
git commit -m "Windows can start a plan, and says Administrator while it runs"
```

---

### Task 10: Prove it end to end, and write down what is true

**Files:**
- Modify: `README.md`
- Modify: `CLAUDE.md`

- [ ] **Step 1: Build the helper where the application will look for it**

`locate_helper` on Windows looks beside the running executable, then at `BROKEY_HELPER`. From a `cargo run`, both binaries land in `target/debug/`, so the first should already work. Confirm it does; if it does not, that is a defect in Task 2 and it gets fixed here.

- [ ] **Step 2: Run a real install and a real removal**

Read `crates/brokey/src/main.rs` first for the real text-mode subcommands and their spelling; use those, not a guess. Install something small and uncontroversial through winget and remove it again.

Record in your report, exactly: the commands run, whether a UAC prompt appeared and how many times, every event that came back, and whether the package was actually installed and actually removed. Check with something outside Brokey — the registry, or `winget list` — rather than believing Brokey's own report.

**Never write a transcript you did not see.** If it fails, that is the finding, and it is worth more than a green suite.

- [ ] **Step 3: Confirm the closed list refuses something, through the helper**

A unit test covers the list. Here confirm it end to end, because the helper is the thing that enforces it and the helper has never run on Windows before. Record what happened and what the user would have seen.

- [ ] **Step 4: Update the two documents to what is now true**

`README.md:105`'s Windows bullet ends "Nothing runs yet: the helper is a stub on Windows, so a plan is built and nothing carries it out." That is no longer true. Say what is: installing, updating and removing work, and each stretch of elevated steps costs one prompt to allow. Keep the bullet honest about what still does not work — there is no installer yet, so Windows is built from source — and leave the Chocolatey/Scoop/Store sentence.

`CLAUDE.md` is wrong in four places:

- "What this is", lines 24 to 26: "`brokey-helper` still has no Windows implementation, so nothing is actually installed or removed there yet."
- The same section's parenthetical at line 14 lists `transaction/runner.rs` among the files split by cfg. Still true, but it now also involves `transaction/elevate/`.
- The Decisions table's Privilege row, line 46: "(helper still a stub)".
- The first invariant, line 133: "Never a `pkexec` call site outside `transaction/runner.rs`." The call site moved to `transaction/elevate/unix.rs` in Task 1, and the invariant is now the broader one in this plan's Global Constraints: never an elevation call site outside `transaction/elevate/`.

- [ ] **Step 5: Commit**

```bash
git add README.md CLAUDE.md
git commit -m "Windows runs a plan"
```

---

## Self-review

**Spec coverage.** The Privilege section's named pipe with a DACL is Task 5; `ShellExecuteEx` with `runas` is Task 6; the helper reading one JSON Plan and streaming Events is Tasks 7 and 8; `allow.rs`'s second closed list is Task 3; the three Linux-worded sentences are Task 9. The spec's "one helper run per maximal stretch of consecutive root steps, each stretch costing one UAC prompt" is already exactly what `split_runs` does, and `Event::AuthRequired` is already emitted before each helper run, so neither needs a task — Task 9 only makes what the window says about it correct.

**Gaps, recorded rather than hidden.**

1. The spec says a plan that must elevate "names the package that forced it". No task here does that. It needs the confirm dialog to attribute a root step to an op, which `FlowDialog` has the data for and does not say. A later plan.
2. Per-user-first is not achieved. Every winget operation is `needs_root: true`, so every winget plan costs a prompt. Choosing `--scope user` per package needs the manifest, which is the metadata ladder. Already recorded in the spec's Open section.
3. Task 10 runs a real install on one machine. That is a test of one machine, not of Windows. No ARM64 Windows machine exists in this project, and none of this is exercised on one.
4. `CANCELLING_SESSION` still names pkexec. It is emitted only on the Linux session path, so it is not wrong today, but it is a sentence that will read oddly the first time someone greps for pkexec on Windows. Left alone deliberately.

**Type consistency.** `Elevated`, `start`, `Inner::wait` and `Inner::kill` are defined in Task 1 and implemented twice, Task 1 for Unix and Task 6 for Windows, with identical signatures. `Line`, `stream_lines` and `kill_group` change visibility in Task 1 and are used by both sides. `stream_lines_from` is added in Task 6 beside them. `pipe_name`, `sddl_for_current_user`, `listen` and `PipeServer::accept` are defined in Task 5 and used in Task 6. `validate_with` and `check_step` keep one signature across both platforms, so `execute_inner` and the helper call one name. **`helper_arguments` (Task 6) and `pipe_argument` (Task 8) must agree on the spelling `--pipe`**, and Task 8's third test asserts exactly that, because nothing else in the system would notice if they drifted.

**Name collisions checked.** `allow.rs`'s test module already has a helper called `refused`, so Task 3's production function is `not_allowed`. `allow.rs`'s test `step` hardcodes `SourceKind::Pacman`, so Task 3's Windows tests bring their own `step_from`. `runner.rs`'s test `step(root, weight)` is unrelated to `tests/transaction.rs`'s `step(title, program, args, root, weight)`; Task 1's test goes in the latter file and Task 2's in the former, each using the helper that is actually there.

**What every implementer must do.** Nothing on this branch can be compiled for Linux on this machine. Every report carries a **Verified by running:** line and a **Reasoned, not run:** line. Task 4 is where the second list gets checked by a real Linux compiler, and it is a gate: nothing after it starts while `rust ubuntu-latest` is red.
