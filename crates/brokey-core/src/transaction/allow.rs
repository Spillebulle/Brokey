//! The closed list: what the helper will run as root, and nothing else.
//!
//! Every step a plan sends to the helper is checked here before the first one
//! starts, so a plan with one bad step is refused whole rather than half run.
//! The check is a pure function of the plan and a list of directories, which
//! is what lets it be tested in `brokey-core` without root and without the
//! helper binary; `brokey-helper` only wires stdin to [`validate_with`].
//!
//! The list is deliberately narrow. A program is named, not a path, and the
//! helper resolves it in `/usr/bin:/bin:/usr/sbin:/sbin`; a verb comes from a
//! short list; an option comes from a shorter one; a name is letters, digits
//! and a few punctuation marks and never starts with a dash, so an option can
//! not be smuggled in as a name; a package file is an absolute path with no
//! `..` under a directory the store may install from. Anything the list does
//! not name is refused with a sentence that says which step and why. Widening
//! the list is a deliberate edit here with a test beside it, never a special
//! case in a source.

#[cfg(windows)]
use crate::model::SourceKind;
use crate::model::{Command, Plan, Step};
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
#[cfg(windows)]
use std::os::windows::io::{AsRawHandle, OwnedHandle};
#[cfg(unix)]
use std::path::Component;
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::ptr;

#[cfg(windows)]
use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_INVALID_HANDLE, ERROR_NO_SUCH_LOGON_SESSION,
    ERROR_PATH_NOT_FOUND, ERROR_SHARING_VIOLATION, ERROR_SUCCESS, GetLastError, HANDLE, HLOCAL,
    LocalFree,
};
#[cfg(windows)]
use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
#[cfg(windows)]
use windows_sys::Win32::Security::{
    AccessCheck, DACL_SECURITY_INFORMATION, DuplicateTokenEx, GENERIC_MAPPING,
    GROUP_SECURITY_INFORMATION, GetTokenInformation, ImpersonateLoggedOnUser, MapGenericMask,
    OWNER_SECURITY_INFORMATION, PRIVILEGE_SET, PSECURITY_DESCRIPTOR, RevertToSelf,
    SecurityImpersonation, TOKEN_DUPLICATE, TOKEN_ELEVATION, TOKEN_IMPERSONATE, TOKEN_LINKED_TOKEN,
    TOKEN_QUERY, TokenElevation, TokenImpersonation, TokenLinkedToken,
};
#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, DELETE, FILE_ADD_FILE, FILE_ADD_SUBDIRECTORY, FILE_ALL_ACCESS, FILE_APPEND_DATA,
    FILE_DELETE_CHILD, FILE_FLAG_BACKUP_SEMANTICS, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ,
    FILE_GENERIC_WRITE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_DATA,
    GetDriveTypeW, OPEN_EXISTING, WRITE_DAC, WRITE_OWNER,
};
#[cfg(windows)]
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

#[cfg(windows)]
use crate::system::windows::owned_handle;
#[cfg(windows)]
use windows_sys::Win32::System::WindowsProgramming::DRIVE_FIXED;

/// The facts the closed list cannot derive for itself, handed to it by the
/// caller: the directories a package file (`pacman -U`, `dpkg -i`, `rpm -U`,
/// a `.deb` given to `apt-get install`) may come from, and on Windows the
/// removal commands the registry records.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Allowed {
    pub package_dirs: Vec<PathBuf>,
    /// The removal commands the registry actually records, for the one kind
    /// of step whose shape cannot be checked. Empty means nothing from
    /// Add/Remove Programs may run, which is the safe direction to fail.
    #[cfg(windows)]
    pub removals: Vec<crate::model::Command>,
}

impl Allowed {
    /// The distribution's own download caches. Always allowed: a file there
    /// was put there by the package manager.
    pub const SYSTEM_PACKAGE_DIRS: [&'static str; 2] =
        ["/var/cache/pacman/pkg", "/var/cache/apt/archives"];

    /// The system caches only. This is what [`validate`] uses.
    pub fn system() -> Allowed {
        Allowed {
            package_dirs: Self::SYSTEM_PACKAGE_DIRS
                .iter()
                .map(PathBuf::from)
                .collect(),
            // Nothing from Add/Remove Programs until a caller reads the
            // registry and says what is there. See `with_registered_removals`.
            #[cfg(windows)]
            removals: Vec::new(),
        }
    }

    /// The system caches plus the store's own download cache under `home`,
    /// which is where the self-updater and the GitHub source put a package
    /// they downloaded. The helper derives `home` from the invoking user's
    /// passwd entry rather than from `HOME`, because under pkexec `HOME` is
    /// root's and `XDG_CACHE_HOME` is scrubbed; that is why the store's cache
    /// is fixed at `~/.cache/brokey` for this purpose.
    pub fn for_home(home: Option<&Path>) -> Allowed {
        let mut allowed = Allowed::system();
        if let Some(home) = home {
            allowed
                .package_dirs
                .push(home.join(".cache").join("brokey"));
        }
        allowed
    }

    /// The system list plus the removal commands `HKLM` records. Only those
    /// ever reach the helper: `removal_step` sets `needs_root` from
    /// `Hive::needs_elevation`, which is false for `HKCU`, so a per-user
    /// removal is a session step and is never validated here at all. That is
    /// also why an elevated helper reading `HKLM` sees the right set despite
    /// `HKCU` being the administrator's under elevation.
    ///
    /// Both sides of the seam call this. The runner checks every root run
    /// against its own `Allowed` before the first prompt, so a runner left
    /// with an empty list would refuse every removal before the user was
    /// ever asked.
    #[cfg(windows)]
    pub fn with_registered_removals() -> Allowed {
        // The same path `Arp::plan` gives `removal_step`, because the two
        // lists are compared against each other. Resolved once here rather
        // than per entry: it is a syscall, and the answer does not change.
        let msiexec = crate::sources::windows::arp::msiexec_program();
        let removals = crate::sources::windows::arp::read()
            .iter()
            .filter(|e| e.hive.needs_elevation())
            .filter_map(|e| crate::sources::windows::arp::removal_step(e, &msiexec))
            .map(|step| step.command)
            .collect();
        Allowed {
            removals,
            ..Allowed::system()
        }
    }
}

/// Every environment variable a step may hand to a root child. Everything
/// else is dropped by the helper before the child starts, and refused here so
/// a source learns about it in a test rather than in a confusing log. The
/// value is checked too, by [`env_value_ok`]: debconf evaluates
/// `DEBIAN_FRONTEND` as Perl code as root, so only a frontend name passes.
#[cfg(unix)]
pub const ALLOWED_ENV: [&str; 3] = ["DEBIAN_FRONTEND", "LC_ALL", "LANG"];

/// Whether `value` is a shape the variable `key` may carry to a root child.
/// `DEBIAN_FRONTEND` is one lowercase word (`noninteractive`, `text`,
/// `dialog`); `LC_ALL` and `LANG` are a locale name (`C.UTF-8`,
/// `en_GB.UTF-8`, `de_DE@euro`). Nothing else is a variable the list names.
#[cfg(unix)]
pub fn env_value_ok(key: &str, value: &str) -> bool {
    let shape: fn(char) -> bool = match key {
        "DEBIAN_FRONTEND" => |c| c.is_ascii_lowercase(),
        "LC_ALL" | "LANG" => |c| c.is_ascii_alphanumeric() || "_.@-".contains(c),
        _ => return false,
    };
    !value.is_empty() && value.chars().all(shape)
}

/// Every program the helper will start. A step naming anything else is
/// refused, whatever its arguments. `systemctl` and `ln` are here for one
/// command each, the two that setting snapd up needs; see `systemctl` and
/// `ln` below.
#[cfg(unix)]
pub const ALLOWED_PROGRAMS: [&str; 10] = [
    "pacman",
    "apt-get",
    "dnf",
    "snap",
    "flatpak",
    "chwd",
    "rpm",
    "dpkg",
    "systemctl",
    "ln",
];

/// The one remote the helper will add, by name and by the address of its
/// `.flatpakrepo` file: Flathub, from either of its hosts. Setting Flatpak
/// up adds it; nothing else is ever added as root.
#[cfg(unix)]
pub const FLATHUB_REMOTES: [(&str, &str); 2] = [
    ("flathub", "https://dl.flathub.org/repo/flathub.flatpakrepo"),
    ("flathub", "https://flathub.org/repo/flathub.flatpakrepo"),
];

/// The one unit the helper will enable: snapd's socket, which starts snapd
/// on the first request.
#[cfg(unix)]
pub const SNAPD_SOCKET_UNIT: &str = "snapd.socket";

/// Where snapd mounts snaps on a distribution that does not use `/snap`
/// (Arch, Fedora), and the link classic snaps need. Classic confinement is
/// built with `/snap` as the mount point, so without the link a classic
/// snap refuses to install; `snap` itself says to create it.
#[cfg(unix)]
pub const SNAP_MOUNT_DIR: &str = "/var/lib/snapd/snap";
#[cfg(unix)]
pub const SNAP_LINK: &str = "/snap";

/// Check a plan against the closed list with the system package caches only.
/// `Ok(())` means every step may run; `Err` carries the sentence the helper
/// reports, naming the step and the reason.
pub fn validate(plan: &Plan) -> Result<(), String> {
    validate_with(plan, &Allowed::system())
}

/// [`validate`] with a caller-chosen list of package directories.
#[cfg(unix)]
pub fn validate_with(plan: &Plan, allowed: &Allowed) -> Result<(), String> {
    for step in &plan.steps {
        check_step(step, allowed).map_err(|reason| refusal(&step.command, &reason))?;
    }
    Ok(())
}

/// What may run as Administrator on Windows.
///
/// Three things, and the question asked of each is the same one:
/// provenance. A winget or Chocolatey command is admitted only if its
/// arguments are ones a source in this crate would have built for the
/// program it names; a removal from Add/Remove Programs is admitted only
/// if its whole command is one the registry itself records.
///
/// There is a second question, put to all three and described on
/// [`check_step`]: whether this process could put different bytes at the
/// program's own path. Provenance is about the command and says nothing
/// about the file, and a file an unprivileged process can replace is one
/// elevating grants nothing by.
///
/// `winget.exe` is a fixed program with a fixed set of commands, and
/// [`operation_step`](crate::sources::windows::winget::operation_step) and
/// [`update_all_step`](crate::sources::windows::winget::update_all_step)
/// are the only things that build them. Both are pure functions of the
/// program path and the package id, so the arm below rebuilds the command
/// the source would have made and compares the whole `Command` against it:
/// program, arguments, environment and working directory. Naming the
/// program and checking the verb was not enough, because everything after
/// the verb was then free, and `winget install --manifest` with a yaml of
/// the caller's choosing, or `--override` with a command line, is arbitrary
/// elevated execution through a genuine `winget.exe`. Rebuilding closes the
/// environment too: a winget command carries no `env`, because that is what
/// the source builds.
///
/// This arm's strength is in the arguments, not the program. The expected
/// command is rebuilt using the command's *own* `program`, so the program is
/// compared against itself and always agrees; a step is not admitted because
/// its program is winget's real path, but because its arguments are exactly
/// what a genuine plan builds for whatever program it names. A local
/// absolute path ending in `winget.exe` that is not really winget still
/// passes this arm if its arguments have the right shape.
///
/// The program must still answer [`on_a_local_disk`], which is a narrower
/// question than "is this really winget": it only rules out a UNC path such
/// as `\\somewhere\share\winget.exe`, which answers `true` to
/// `Path::is_absolute` on Windows and would be started over the network.
///
/// What this arm leaves open, and what closes it: this arm asks nothing
/// about the file, so arguments of the right shape around a program the
/// invoking user can replace would be elevated on its word alone. The rule
/// that closes it is not in this arm but after it, in [`check_step`]: a
/// program this process can replace is refused whatever source asked for
/// it.
///
/// For winget that rule meant a flat refusal on every machine, because
/// `winget_program` answered the App Execution Alias in
/// `%LOCALAPPDATA%\Microsoft\WindowsApps`, a zero-length reparse point in a
/// folder the invoking user has full control of. It no longer answers that.
/// The alias is resolved to the executable `CreateProcess` would really
/// start, under `C:\Program Files\WindowsApps`, which no unprivileged
/// process can replace. The resolution is
/// [`resolve_app_execution_alias`](crate::system::windows::resolve_app_execution_alias),
/// and the source does it once, so the source and this list name the same
/// file by construction and there is no second resolution here.
///
/// Resolving the alias was not by itself enough to admit a winget step,
/// and one more thing had to change before it was.
/// `GetNamedSecurityInfoW` on `C:\Program Files\WindowsApps` fails with
/// `ERROR_ACCESS_DENIED`, because reading a security descriptor needs
/// `READ_CONTROL` and a standard user is granted none on that directory,
/// so the question could not be put and the step was refused by
/// [`cannot_be_checked`]. A descriptor this account may not read is not an
/// answer, so the gate now asks the object instead: it opens that element
/// for each right that would let this account replace it, which
/// [`any_right_granted_on`] describes. On the development machine that
/// directory is the only element of the resolved winget path whose
/// descriptor is unreadable, and it refuses every one of those opens, so
/// the step is admitted, and it is admitted on a measurement rather than on
/// an inference from what could not be read.
///
/// `choco.exe` is admitted by the same rule, through a function of the
/// same shape.
/// [`operation_step`](crate::sources::windows::choco::operation_step) is
/// the only thing that builds a Chocolatey command, and it too is a pure
/// function of the program path and the package id, so the arm rebuilds
/// the command the source would have made and compares the whole
/// `Command`. The difference worth naming is where the id sits: a
/// Chocolatey command is the verb, the id and `-y`, with
/// `--remove-dependencies` after it for a removal, so the id is the second
/// argument and not winget's fourth. The check that the id is not an
/// option is there for the same reason as winget's: an id of `-y` would
/// otherwise rebuild into the very command it was taken from. It refuses
/// one character more than winget's does, because Chocolatey accepts `/y`
/// as well as `-y` and `--yes`, so an option can be written with a leading
/// slash there and a dash alone would only be half the rule.
///
/// Every Chocolatey step carries `needs_root`, because the default install
/// root is under `C:\ProgramData`, so this arm is the only way an install,
/// an update or a removal through Chocolatey runs at all. Chocolatey has
/// no update-everything step in this crate, so unlike winget there is no
/// second shape to admit.
///
/// A removal from Add/Remove Programs cannot be rebuilt that way. The
/// command is whatever the installer wrote into the registry years ago, so
/// no requirement about its *shape* would be honest. The only honest check
/// is the same question put differently: is this command one the registry
/// actually records?
///
/// `Allowed::removals` carries the removal commands read back out of the
/// registry, the way `package_dirs` carries the directories a package file
/// may come from, and a step is admitted only if its whole command is one
/// of them. Labelling a step `SourceKind::Arp` buys a forged plan nothing,
/// because `source` now only chooses which question is asked. An empty list
/// refuses every removal, which is the safe direction to fail: a caller
/// that has not read the registry cannot run anything from it.
///
/// Both sides fill the list, through `Allowed::with_registered_removals`.
/// The helper does because it is what enforces the list; the runner does
/// because it validates every root run before the first prompt, and a
/// runner with an empty list would refuse every removal before the user was
/// ever asked.
///
/// Only `HKLM` removals ever arrive here. `removal_step` sets `needs_root`
/// from `Hive::needs_elevation`, which is false for `HKCU`, so a per-user
/// removal is a session step and is never validated here at all. That is
/// what makes reading the registry in an elevated process sound: `HKLM` is
/// the same whoever reads it, while the elevated process's `HKCU` may be an
/// administrator's rather than the user's.
///
/// Widening this is a deliberate edit here with a test beside it.
///
/// Only the steps that need Administrator are checked, because a step that
/// does not is one the runner runs in the user's own session and never
/// sends to the helper. A per-user removal from `HKCU` is exactly that, and
/// checking it here would refuse every one of them before the user was ever
/// asked. What makes that filter safe is at the other end: the helper
/// refuses a whole plan that carries a step which does not need root, so
/// nothing reaches an elevated process without passing through here.
#[cfg(windows)]
pub fn validate_with(plan: &Plan, allowed: &Allowed) -> Result<(), String> {
    for step in plan.steps.iter().filter(|s| s.needs_root) {
        check_step(step, allowed)?;
    }
    Ok(())
}

/// Whether one step may run as Administrator, and why not when it may not.
///
/// Two questions, both of which a step has to answer. The first is the
/// source's own and is about the command: the arms below rebuild what the
/// source would have produced and compare it whole, or, for a removal,
/// compare the command against the ones the registry records.
///
/// The second is [`program_this_process_cannot_replace`] and is about the
/// file, and it is asked of every program this list admits, whatever
/// source asked for it, so that a source added later inherits it rather
/// than having to remember it. It refuses a program this process could put
/// different bytes at, because elevating such a program hands
/// Administrator to whoever put them there, which is no privilege boundary
/// at all. The file itself is asked, and so is every directory above it up
/// to the drive, and a path nothing can be read from is refused rather than
/// admitted.
///
/// What the second question does not settle, and nothing here claims it
/// does:
///
/// - **That the program is what it says it is.** It asks who may write the
///   file, never what is in it. A `choco.exe` under `C:\ProgramData` that
///   only an administrator can write passes whether or not Chocolatey put
///   it there, and no signature, hash or publisher is looked at.
/// - **That the file will still be that file when it starts.** The answer
///   is true of the moment it is given. Between this check and
///   `ShellExecuteEx` a permission can change, and on a path where nothing
///   unprivileged can write, only an administrator could make that happen.
/// - **A junction or a symbolic link in the middle of the path.** Each
///   element is asked about by name, and Windows answers for what the name
///   leads to, so a link this account could repoint is measured by its
///   target's permissions and not by its own.
/// - **What an elevated program then does.** A directory on the DLL search
///   path that this account can write is not this check's question; a
///   working directory is, and it is the whole-command comparison above
///   that refuses one.
/// - **Anyone but this account.** `AccessCheck` is put one token: this
///   process's, or the unelevated token linked to it when the process is
///   the elevated helper. Another user, a service or an administrator who
///   can write the file is not what is being asked about, because none of
///   them gains anything from Brokey elevating it.
/// - **That the program is installed at all.** A path that is not on the
///   disk is admitted when nothing unprivileged could create it, because
///   nothing can appear there without Administrator. Such a step fails
///   when it is run, with the error starting a program that is not there
///   produces, and not as a refusal here.
///
/// A question that cannot be put is a refusal: a path Windows will not
/// name, an error that is neither a denial nor a missing name, and a token
/// road that fails for any reason but one all end in [`cannot_be_checked`]
/// rather than in `Ok(())`. The one exception is an elevated process whose
/// token has no filtered sibling: there is then no unprivileged account for
/// the question to be about, and the answer is `Ok(())`, which
/// [`an_unelevated_impersonation_token`] argues. A descriptor this account
/// may not read is not one of those. It is a question put a different way
/// rather than one that cannot be put: the object is opened for each right
/// instead, which is what [`any_right_granted_on`] explains and what admits
/// the winget under `C:\Program Files\WindowsApps`.
#[cfg(windows)]
pub fn check_step(step: &Step, allowed: &Allowed) -> Result<(), String> {
    match step.source {
        SourceKind::Winget => check_winget(&step.command)?,
        SourceKind::Choco => check_choco(&step.command)?,
        SourceKind::Arp => {
            if !allowed.removals.contains(&step.command) {
                return Err(not_allowed(&step.command.program));
            }
        }
        other => return Err(not_allowed(&format!("{other:?}"))),
    }
    // Asked again here, of every program the list admits and not only of
    // the two the arms above read. A removal the registry records reaches
    // the gate the same way, and the gate's own instruments are answered by
    // the volume holding the program, so a volume that is not this
    // machine's answers nothing worth having. [`on_a_local_disk`] says why.
    if !on_a_local_disk(Path::new(&step.command.program)) {
        return Err(not_allowed(&step.command.program));
    }
    program_this_process_cannot_replace(&step.command.program)
}

/// Whether `command` is one the winget source would have built, rebuilt
/// from the command's own program and package id and compared whole.
#[cfg(windows)]
fn check_winget(command: &Command) -> Result<(), String> {
    use crate::sources::windows::winget::{OpKind, operation_step, update_all_step};

    let path = Path::new(&command.program);
    if !on_a_local_disk(path) {
        return Err(not_allowed(&command.program));
    }
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if file_name != "winget.exe" {
        return Err(not_allowed(&command.program));
    }
    let verb = command.args.first().map(String::as_str).unwrap_or("");
    let expected = if verb == "upgrade" && command.args.get(1).map(String::as_str) == Some("--all")
    {
        update_all_step(&command.program).command
    } else {
        let kind = match verb {
            "install" => OpKind::Install,
            "upgrade" => OpKind::Update,
            "uninstall" => OpKind::Remove,
            // `describe(command)`, not `verb` alone: `not_a_winget_command`
            // is given the whole command everywhere else it is called, and
            // a bare empty-args command has an empty verb, which would
            // otherwise refuse a step by a name with nothing in it.
            _ => return Err(not_a_winget_command(&describe(command))),
        };
        // Where `operation_step` puts the id, and the only one of its eight
        // arguments that is not a fixed word. An id is a package id and
        // never an option: without this, an argument beginning with a dash
        // would be rebuilt into the very command it was taken from and the
        // comparison below would agree with itself.
        let id = command.args.get(3).map(String::as_str).unwrap_or("");
        if id.is_empty() || id.starts_with('-') {
            return Err(not_a_winget_command(&describe(command)));
        }
        operation_step(kind, id, &command.program).command
    };
    if *command == expected {
        Ok(())
    } else {
        Err(not_a_winget_command(&describe(command)))
    }
}

/// Whether `command` is one the Chocolatey source would have built,
/// rebuilt from the command's own program and package id and compared
/// whole.
#[cfg(windows)]
fn check_choco(command: &Command) -> Result<(), String> {
    use crate::sources::windows::choco::{OpKind, operation_step};

    let path = Path::new(&command.program);
    if !on_a_local_disk(path) {
        return Err(not_allowed(&command.program));
    }
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if file_name != "choco.exe" {
        return Err(not_allowed(&command.program));
    }
    let verb = command.args.first().map(String::as_str).unwrap_or("");
    let kind = match verb {
        "install" => OpKind::Install,
        "upgrade" => OpKind::Update,
        "uninstall" => OpKind::Remove,
        // `describe(command)` rather than `verb` alone, because a command
        // with no arguments has an empty verb and the sentence would then
        // name nothing.
        _ => return Err(not_a_choco_command(&describe(command))),
    };
    // Where `operation_step` puts the id: second, not fourth as winget's
    // is. A Chocolatey command is the verb, the id and `-y`, with
    // `--remove-dependencies` after it for a removal. An id is a package id
    // and never an option: without this, an argument beginning with a dash
    // would be rebuilt into the very command it was taken from and the
    // comparison below would agree with itself.
    //
    // A dash is not the only way Chocolatey writes an option. Its own
    // documentation gives `-y`, `--yes` and `/y` as the same switch, and
    // its argument parser accepts the slash form of every one of them, so
    // `/force` smuggled in where a package id belongs is an option too and
    // is refused by the same rule for the same reason. Only the first
    // character is looked at, which is all either spelling needs: a package
    // id never begins with one.
    let id = command.args.get(1).map(String::as_str).unwrap_or("");
    if id.is_empty() || id.starts_with(['-', '/']) {
        return Err(not_a_choco_command(&describe(command)));
    }
    if *command == operation_step(kind, id, &command.program).command {
        Ok(())
    } else {
        Err(not_a_choco_command(&describe(command)))
    }
}

/// Whether `path` is absolute under a drive letter, `X:\...` or the
/// `\\?\X:\...` form, whose volume is a local fixed disk.
///
/// Both halves are load-bearing. A UNC path such as
/// `\\somewhere\share\winget.exe` is absolute too, and `Command::new`
/// would start it over the network, so `Path::is_absolute` alone is not
/// enough to ask. And a drive letter is not a disk of this machine either:
/// `Prefix::Disk` matches a mapped network drive and a `subst` drive as
/// happily as it matches `C:`, so the volume is asked what it is with
/// `GetDriveTypeW` and only `DRIVE_FIXED` is admitted.
///
/// Why the volume has to be local, which is the part this used to admit as
/// though it were a decision. Everything the gate measures about a program
/// is answered by the filesystem holding it. Over SMB that is a server, and
/// a server the attacker owns answers `ERROR_ACCESS_DENIED` to every
/// question the walk puts while serving whatever bytes it likes for the
/// file itself, so the gate would be asking the attacker whether the
/// attacker may write, and being told no. A removable or remote volume is
/// therefore refused before any of that is asked, by the one rule that a
/// program which is to run as Administrator sits on a local fixed disk.
///
/// The two spellings of a remote volume are refused by two different lines
/// here, which is worth saying because the drive type does not do both. A
/// UNC path names no drive letter, so it never reaches `GetDriveTypeW` at
/// all: the prefix check above is what refuses it. A mapped drive does
/// reach it and answers `DRIVE_REMOTE`. Measured on the development
/// machine, which has four fixed volumes and three mapped ones: `C:\` and
/// `\\?\C:\` answer 3, `Z:\` answers 4, the UNC root that `X:` is mapped to
/// answers 4 when it is asked directly, and a drive letter nothing is
/// mounted on answers 1.
///
/// Every real resolution of winget answers `true` to this: the App
/// Execution Alias lives under a drive letter in the user's own profile,
/// and so does the executable it is resolved to, under
/// `C:\Program Files\WindowsApps`.
#[cfg(windows)]
fn on_a_local_disk(path: &Path) -> bool {
    use std::path::{Component, Prefix};
    if !path.is_absolute() {
        return false;
    }
    let Some(Component::Prefix(prefix)) = path.components().next() else {
        return false;
    };
    let (Prefix::Disk(letter) | Prefix::VerbatimDisk(letter)) = prefix.kind() else {
        return false;
    };
    a_fixed_volume(letter)
}

/// Whether the volume a drive letter names is a local fixed disk.
///
/// `GetDriveTypeW` wants a root directory with a trailing backslash, so the
/// letter is spelt back out as `X:\` rather than the path being handed on:
/// the verbatim form carries its own prefix and the rest of the path is not
/// the volume. A letter nothing is mounted on answers `DRIVE_NO_ROOT_DIR`,
/// a mapped network drive answers `DRIVE_REMOTE`, and a `subst` drive
/// answers whatever its backing volume is, which is the right answer: its
/// elements are measured on that volume by every other question the gate
/// puts. Nothing without a drive letter is ever asked, so no UNC root
/// reaches this.
#[cfg(windows)]
fn a_fixed_volume(letter: u8) -> bool {
    let root: [u16; 4] = [u16::from(letter), u16::from(b':'), u16::from(b'\\'), 0];
    // SAFETY: `root` is a null-terminated wide string of four units that
    // outlives the call, and the call reads it and takes nothing else.
    let kind = unsafe { GetDriveTypeW(root.as_ptr()) };
    kind == DRIVE_FIXED
}

/// The rights that would let this process put its own bytes at the
/// program's own path: overwrite it, append to it, delete it so that a new
/// file can take the name, or rewrite its security descriptor so that it
/// could do any of those. Any one of them is enough, which is why they are
/// asked one at a time and the first yes ends the question.
///
/// `WRITE_DAC` and `WRITE_OWNER` are here because a descriptor grants them
/// to the object's owner whether or not an entry says so, so a file this
/// account created is one this account can always re-permit.
#[cfg(windows)]
const REPLACE_A_FILE: [u32; 5] = [
    FILE_WRITE_DATA,
    FILE_APPEND_DATA,
    DELETE,
    WRITE_DAC,
    WRITE_OWNER,
];

/// The rights that would let this process swap out a directory on the way
/// to the program: delete or rename the directory itself, delete what is
/// inside it, or rewrite its security descriptor so that it could.
///
/// `FILE_DELETE_CHILD` is asked of the directory rather than left to the
/// child, because that right removes a child whose own descriptor refuses
/// `DELETE`, and nothing in the child's descriptor says so.
///
/// `FILE_ADD_FILE` and `FILE_ADD_SUBDIRECTORY` are deliberately not here.
/// Being allowed to add a name to a directory is not being allowed to take
/// a name that is already taken, and a stock Windows grants exactly that on
/// `C:\ProgramData` to `BUILTIN\Users`, so asking for it would refuse every
/// program under `C:\ProgramData`, including a properly installed
/// Chocolatey. They are asked of the directory above a name that is *not*
/// taken instead, by [`CREATE_A_NAME`].
#[cfg(windows)]
const REPLACE_A_DIRECTORY: [u32; 4] = [DELETE, FILE_DELETE_CHILD, WRITE_DAC, WRITE_OWNER];

/// The rights that would let this process bring the missing part of a path
/// into being. This is the planted case itself: `C:\ProgramData\chocolatey`
/// does not exist on a machine without Chocolatey, `C:\ProgramData` lets
/// any user create a directory, so an unprivileged process makes the folder
/// and puts its own `choco.exe` at the end of it.
///
/// The aliasing in these three arrays is known rather than overlooked.
/// `FILE_ADD_FILE` and `FILE_WRITE_DATA` are the same bit, 0x0002, and
/// `FILE_ADD_SUBDIRECTORY` and `FILE_APPEND_DATA` are the same bit, 0x0004:
/// Windows reads the pair by whether the object is a directory or a file,
/// and there is no way to ask for one meaning and not the other. So
/// `CREATE_A_NAME` put to something that turns out to be a file measures
/// write and append on that file instead. What follows from it is that
/// `C:\Windows\System32\cmd.exe\winget.exe` is admitted by the walk,
/// because the missing leaf's directory above is `cmd.exe` and this account
/// can write neither. Nothing follows from that in turn, because such a
/// path cannot be executed, and it is written down here so that the next
/// reader does not have to rediscover it.
#[cfg(windows)]
const CREATE_A_NAME: [u32; 2] = [FILE_ADD_FILE, FILE_ADD_SUBDIRECTORY];

/// How `AccessCheck` is to read a right that a stored descriptor still
/// spells generically. These are the file system's own four mappings.
///
/// A `static` and not a `const`: a `const` is a value, so every
/// `&FILE_MAPPING` would materialise a fresh temporary at a fresh address.
/// Both parameters it is passed to are read-only today, so that is harmless
/// today, and it stops being harmless the moment either is spelt `*mut`.
#[cfg(windows)]
static FILE_MAPPING: GENERIC_MAPPING = GENERIC_MAPPING {
    GenericRead: FILE_GENERIC_READ,
    GenericWrite: FILE_GENERIC_WRITE,
    GenericExecute: FILE_GENERIC_EXECUTE,
    GenericAll: FILE_ALL_ACCESS,
};

/// A security descriptor `GetNamedSecurityInfoW` allocated, freed once when
/// the value goes out of scope, however the scope is left.
///
/// It lives no longer than the one call to [`any_right_granted_on`] that
/// read it, and every path out of that function drops it; there is no other
/// constructor, the type is neither `Copy` nor `Clone`, and the pointer is
/// never handed anywhere that would free it a second time.
#[cfg(windows)]
struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

#[cfg(windows)]
impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: `self.0` is the pointer a successful
        // `GetNamedSecurityInfoW` handed back, which is documented to be
        // released with `LocalFree`, and this is the only place that
        // releases it.
        unsafe { LocalFree(self.0 as HLOCAL) };
    }
}

/// The security descriptor of one file or directory, or the Win32 error
/// that says why there is none. A path that is not there answers
/// `ERROR_FILE_NOT_FOUND` or `ERROR_PATH_NOT_FOUND`, which the caller turns
/// into a question about the directory above rather than a failure.
///
/// A descriptor is metadata this account has to be granted `READ_CONTROL`
/// to read, and there are directories that grant it none:
/// `C:\Program Files\WindowsApps`, where winget really lives, answers
/// `ERROR_ACCESS_DENIED` on this machine, measured with `Get-Acl`
/// unelevated. That error is not a failure of the walk either.
/// [`any_right_granted_on`] puts the question to the object instead.
#[cfg(windows)]
fn descriptor_of(path: &Path) -> Result<SecurityDescriptor, u32> {
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: `wide` is a null-terminated wide string that outlives the
    // call; the four SID and ACL out-parameters are null, which is the
    // documented way of asking for none of them; and `descriptor` is a
    // writable out-parameter which the call sets, on success only, to
    // memory it has allocated and this process owns.
    let status = unsafe {
        GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != ERROR_SUCCESS {
        // Nothing is allocated on a failure, so there is nothing to free.
        return Err(status);
    }
    // SAFETY: the call above succeeded, so `descriptor` is the one pointer
    // it allocated and nothing else owns it.
    Ok(SecurityDescriptor(descriptor))
}

/// Whether `token` is granted any one of `rights` on the object
/// `descriptor` describes, or the Win32 error that says the question could
/// not be put.
///
/// One `AccessCheck` per right, because `AccessCheck` answers "all of
/// these" and the question here is "any of these": a program that can only
/// be deleted is as replaceable as one that can be overwritten.
#[cfg(windows)]
fn any_right_granted(
    descriptor: &SecurityDescriptor,
    token: &OwnedHandle,
    rights: &[u32],
) -> Result<bool, u32> {
    for right in rights {
        let mut desired = *right;
        // SAFETY: `desired` is a writable `u32` local and `FILE_MAPPING` is
        // a fully initialised `GENERIC_MAPPING` that outlives the call. It
        // only rewrites generic bits, of which these rights have none, and
        // is called because `AccessCheck` is documented to be given a mask
        // with no generic bits left in it.
        unsafe { MapGenericMask(&mut desired, &FILE_MAPPING) };

        // `AccessCheck` writes the privileges it used here. A file check
        // uses none, but the call insists on somewhere to put them. Sixty
        // four `u32`s is far more than a `PRIVILEGE_SET` of a few entries
        // needs, and an array of `u32` is aligned for one, whose fields are
        // all four bytes wide.
        let mut privileges = [0u32; 64];
        let mut privileges_len = std::mem::size_of_val(&privileges) as u32;
        let mut granted: u32 = 0;
        let mut allowed: windows_sys::core::BOOL = 0;
        // SAFETY: `descriptor.0` is a descriptor this process owns and
        // `token` an open impersonation token, both alive for the whole
        // call; `FILE_MAPPING` is fully initialised; `privileges` is a
        // writable buffer whose true byte length is in `privileges_len`;
        // and the remaining out-parameters are writable locals of the types
        // the signature names.
        let asked = unsafe {
            AccessCheck(
                descriptor.0,
                token.as_raw_handle() as HANDLE,
                desired,
                &FILE_MAPPING,
                privileges.as_mut_ptr().cast::<PRIVILEGE_SET>(),
                &mut privileges_len,
                &mut granted,
                &mut allowed,
            )
        };
        if asked == 0 {
            // SAFETY: `GetLastError` takes no arguments and reads this
            // thread's own last error code.
            return Err(unsafe { GetLastError() });
        }
        if allowed != 0 {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether this account is granted any one of `rights` on `path`, or the
/// Win32 error that says the question could not be put. This is the one
/// question the walk asks of a path, and it has two instruments behind it.
///
/// The primary one is `AccessCheck` on the path's own security descriptor.
/// It reads metadata, opens no handle, and so cannot trip over a file
/// somebody else has open or over a share mode.
///
/// The fallback is for one answer only: a descriptor this account may not
/// read. Reading one needs `READ_CONTROL`, and
/// `C:\Program Files\WindowsApps` grants this account none, so
/// `GetNamedSecurityInfoW` there answers `ERROR_ACCESS_DENIED` and the
/// primary instrument has nothing to check against. A descriptor that
/// cannot be read is not an answer, so the object itself is asked instead,
/// by [`any_right_opened_on`]. That is a direct measurement and not the
/// cheaper inference that a descriptor this account cannot read describes
/// an object this account cannot write: an entry can grant `WRITE_DAC`
/// without granting `READ_CONTROL`, and this gate is the one place in
/// Brokey where that case is worth the code to rule out.
///
/// `ERROR_FILE_NOT_FOUND` and `ERROR_PATH_NOT_FOUND` are passed on
/// unchanged, because [`program_this_process_cannot_replace`] matches on
/// them to find the first element that is not on the disk and put
/// [`CREATE_A_NAME`] to the directory above it.
#[cfg(windows)]
fn any_right_granted_on(path: &Path, token: &OwnedHandle, rights: &[u32]) -> Result<bool, u32> {
    match descriptor_of(path) {
        Ok(descriptor) => any_right_granted(&descriptor, token, rights),
        Err(ERROR_ACCESS_DENIED) => any_right_opened_on(path, token, rights),
        Err(other) => Err(other),
    }
}

/// Whether `token` can open `path` for any one of `rights`, asked of the
/// object rather than of its descriptor. An open that succeeds proves the
/// right is granted; an open refused with `ERROR_SHARING_VIOLATION` proves
/// it is granted as well, for the reason two paragraphs down; an open
/// refused with `ERROR_ACCESS_DENIED` proves it is not; anything else is a
/// question that could not be put and comes back as `Err`.
///
/// Nothing is created, written or deleted. `OPEN_EXISTING` brings no file
/// into being, and a handle opened for `DELETE` access deletes nothing: the
/// deletion is a separate call that is never made here, and the handle is
/// closed on the line after it is opened. `FILE_FLAG_BACKUP_SEMANTICS` is
/// what lets a directory be opened at all, and every element above the
/// program is one.
///
/// The share mode passed here is the widest there is, but that declares
/// only what this open permits others. It does not exempt this open from
/// the share mode an existing opener already declared, which the comment
/// here used to imply it did. Windows maps an executable image with
/// `FILE_SHARE_READ | FILE_SHARE_DELETE` and no `FILE_SHARE_WRITE`, so
/// opening a running `winget.exe` for `FILE_WRITE_DATA` is refused with
/// `ERROR_SHARING_VIOLATION` however wide a share mode this asks for.
///
/// That refusal proves the opposite of a denial, which is why it answers
/// `Ok(true)` and the program is refused with [`can_be_replaced`]. Windows
/// evaluates the DACL before it evaluates share modes, so a sharing
/// violation is returned only after access was granted, and the right is
/// therefore held. Measured unelevated on the development machine:
/// `C:\Windows\System32\kernel32.dll`, which every process on the machine
/// holds mapped without `FILE_SHARE_WRITE` and which this account may not
/// write, is refused `FILE_WRITE_DATA` with error 5 and not 32, while a
/// file this account owns and holds open with `FILE_SHARE_READ` alone is
/// refused with 32.
///
/// The opens are made under [`ImpersonateLoggedOnUser`], because a
/// `CreateFileW` from the elevated helper's own thread would be answered
/// for Administrator and would refuse every program on the machine. The
/// guard reverts the thread on every path out of here, a panic included.
#[cfg(windows)]
fn any_right_opened_on(path: &Path, token: &OwnedHandle, rights: &[u32]) -> Result<bool, u32> {
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let _impersonating = Impersonating::begin(token)?;
    for right in rights {
        // SAFETY: `wide` is a null-terminated wide string that outlives the
        // call; a null security-attributes pointer is the documented way to
        // ask for the default; and `OPEN_EXISTING` requires the template
        // handle to be null, which it is.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                *right,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                ptr::null_mut(),
            )
        };
        // `owned_handle` rather than a bare check against
        // `INVALID_HANDLE_VALUE`, because null is the other value an
        // `OwnedHandle` may not hold. It closes the handle exactly once,
        // here and now. A null would fall to the error road below and be a
        // refusal, which is the safe direction for a value `CreateFileW`
        // does not return.
        if let Some(opened) = owned_handle(handle) {
            drop(opened);
            return Ok(true);
        }
        // SAFETY: `GetLastError` takes no arguments and reads this thread's
        // own last error code, which the failed call above has just set.
        let error = unsafe { GetLastError() };
        // An access this account does not hold is refused before the share
        // mode is ever consulted, so a sharing violation says the access
        // was granted and somebody else's handle is in the way.
        if error == ERROR_SHARING_VIOLATION {
            return Ok(true);
        }
        if error != ERROR_ACCESS_DENIED {
            return Err(error);
        }
    }
    Ok(false)
}

/// The thread impersonating a token, reverted when the value goes out of
/// scope, however the scope is left.
///
/// A thread left impersonating is not a leak that shows up as one: every
/// later call on it would be answered for the wrong account. So the revert
/// is a `Drop` rather than a line at the end of the function, which a `?`
/// or a panic would step over.
#[cfg(windows)]
struct Impersonating;

#[cfg(windows)]
impl Impersonating {
    /// The thread impersonates `token` until the value returned is dropped.
    /// `token` has to carry `TOKEN_IMPERSONATE`, which is why
    /// [`impersonation_of`] asks for it by name.
    fn begin(token: &OwnedHandle) -> Result<Impersonating, u32> {
        // SAFETY: `token` is an open impersonation token that is alive for
        // the call, and the call takes nothing else.
        let began = unsafe { ImpersonateLoggedOnUser(token.as_raw_handle() as HANDLE) };
        if began == 0 {
            // SAFETY: `GetLastError` takes no arguments and reads this
            // thread's own last error code.
            return Err(unsafe { GetLastError() });
        }
        Ok(Impersonating)
    }
}

#[cfg(windows)]
impl Drop for Impersonating {
    fn drop(&mut self) {
        // SAFETY: `RevertToSelf` takes no arguments and acts on the calling
        // thread, which `begin` put into impersonation and which nothing
        // between then and now has reverted.
        let reverted = unsafe { RevertToSelf() };
        if reverted == 0 {
            // SAFETY: `GetLastError` takes no arguments and reads this
            // thread's own last error code, which the failed call above has
            // just set.
            let error = unsafe { GetLastError() };
            // The decision, made here rather than left to whatever happens
            // next. This thread is still answering as somebody else, and
            // every question this gate asks is a question about which
            // account; the call that undoes it is the one that has just
            // failed, so there is nothing to retry. Carrying on would mean
            // a process quietly measuring the wrong account for the rest of
            // its life, which is worse than stopping. It is an abort and
            // not a panic because a panic would unwind, running arbitrary
            // `Drop` code on a thread that is still impersonating, and a
            // panic in a `Drop` during an unwind aborts anyway.
            log::error!("the thread could not stop impersonating this account: {error}");
            eprintln!("Brokey stopped: the thread could not stop impersonating this account.");
            std::process::abort();
        }
    }
}

/// `token` again as an impersonation token, which is the only kind
/// `AccessCheck` takes, and with `TOKEN_IMPERSONATE` on the handle, which
/// is the only kind `ImpersonateLoggedOnUser` takes.
///
/// `DuplicateTokenEx` rather than `DuplicateToken` for that second reason
/// alone. `DuplicateToken` asks for the same access the source handle has,
/// and the process token is opened with `TOKEN_QUERY | TOKEN_DUPLICATE`, so
/// the probe would fail to impersonate and the step would be refused for a
/// reason that has nothing to do with its permissions. The rights asked for
/// here are the three the two instruments between them need and no more.
#[cfg(windows)]
fn impersonation_of(token: &OwnedHandle) -> Result<OwnedHandle, u32> {
    let mut raw: HANDLE = ptr::null_mut();
    // SAFETY: `token` is open with `TOKEN_DUPLICATE` and alive for the
    // call; a null security-attributes pointer is the documented way to ask
    // for the default; and `raw` is a writable out-parameter which the call
    // sets, on success only, to a handle this process owns alone.
    let duplicated = unsafe {
        DuplicateTokenEx(
            token.as_raw_handle() as HANDLE,
            TOKEN_QUERY | TOKEN_DUPLICATE | TOKEN_IMPERSONATE,
            ptr::null(),
            SecurityImpersonation,
            TokenImpersonation,
            &mut raw,
        )
    };
    if duplicated == 0 {
        // SAFETY: `GetLastError` takes no arguments and reads this thread's
        // own last error code.
        return Err(unsafe { GetLastError() });
    }
    // A success that handed back no handle is not one this can carry on
    // from, and it is a refusal rather than a value `OwnedHandle` may not
    // hold. `DuplicateTokenEx` does not do it; the check is what says so.
    owned_handle(raw).ok_or(ERROR_INVALID_HANDLE)
}

/// What the gate can put its questions to: a token that stands for this
/// process without Administrator, or the finding that there is no
/// unprivileged account on this machine for the questions to be about.
#[cfg(windows)]
enum Unprivileged {
    /// An impersonation token for this account without Administrator.
    Token(OwnedHandle),
    /// This process is elevated and Windows answers that no filtered token
    /// is linked to its own, so this account has no unprivileged sibling.
    /// [`an_unelevated_impersonation_token`] says what rests on that.
    NoSuchAccount,
}

/// An impersonation token that stands for this process without
/// Administrator: its own token when the process is not elevated, and the
/// filtered token linked to it when it is.
///
/// The elevated case is not a nicety. The helper runs this same check while
/// it is Administrator, and Administrators are granted write on most of the
/// machine, so asking the elevated token would refuse every program there
/// is and nothing would ever install. `TokenLinkedToken` is the standard
/// user token UAC filtered out of the elevated one, which is the token the
/// window itself holds, so both ends of the seam ask about the same
/// account.
///
/// They do not always reach the answer by the same road, and this comment
/// used to claim that they did. [`descriptor_of`] is not one of the calls
/// made under impersonation, so the elevated helper reads a descriptor with
/// its own elevated token: on `C:\Program Files\WindowsApps` it reads it
/// and never enters the probe, while the unelevated window is refused it
/// and the probe is where the window's answer comes from. CI run
/// 35532208191 measured exactly that, `descriptor_of` answering no error
/// on the runner against `ERROR_ACCESS_DENIED` on an ordinary user's
/// machine. The two instruments are built to agree and on every path
/// measured so far they do, but they are not the same question, and the
/// seam's value is that the second check is a re-check of the first. Any
/// divergence shows up as a plan admitted before the prompt and refused
/// after it.
///
/// What that does not give is the *invoking* user's token when somebody
/// else's administrator credentials answered the prompt: the linked token
/// is then that administrator's, not the user's. The unelevated end asked
/// first, before anyone was prompted, and that is the end that matters.
///
/// One failure of `TokenLinkedToken` is not a failure of the question.
/// `ERROR_NO_SUCH_LOGON_SESSION` there says this elevated token has no
/// filtered token behind it, which is what a built-in Administrator
/// session, a machine with UAC turned off and a domain administrator's
/// console all answer. That is [`Unprivileged::NoSuchAccount`], and
/// [`program_this_process_cannot_replace`] admits the program rather than
/// refusing it.
///
/// What the carve-out rests on, written out so that the next reader can
/// attack the reasoning rather than the code. The gate's premise is that
/// some process which is not already Administrator could put different
/// bytes at the program's path and then have Brokey elevate them. That
/// premise needs an unprivileged account to be the attacker. Where the
/// token has no filtered sibling, every process this user starts is
/// Administrator already, so replacing the file wins nobody anything
/// Brokey is handing out: there is no privilege boundary for the replaced
/// program to cross, and the question is vacuous rather than unanswered.
/// Refusing instead was measured on CI run 35532208191, where the runner
/// is the built-in Administrator and the gate refused every program on the
/// machine, `C:\Windows\System32\cmd.exe` included, after telling the
/// user to check a path that was perfectly readable.
///
/// It is keyed on that one error and on `TokenIsElevated` being set. Never
/// on "elevated" alone and never on any other error from this road: every
/// other failure here is still a question that could not be put, and is
/// still a refusal. The unelevated branch keeps refusing on error too,
/// because there the boundary is real.
#[cfg(windows)]
fn an_unelevated_impersonation_token() -> Result<Unprivileged, u32> {
    // SAFETY: `GetCurrentProcess` takes no arguments and returns a pseudo
    // handle that is always valid and never needs closing.
    let process = unsafe { GetCurrentProcess() };

    let mut raw: HANDLE = ptr::null_mut();
    // SAFETY: `process` is that pseudo handle and `raw` is a writable
    // out-parameter which the call sets, on success only, to a handle this
    // process owns alone.
    let opened = unsafe { OpenProcessToken(process, TOKEN_QUERY | TOKEN_DUPLICATE, &mut raw) };
    if opened == 0 {
        // SAFETY: `GetLastError` takes no arguments and reads this thread's
        // own last error code.
        return Err(unsafe { GetLastError() });
    }
    // A success that handed back no handle is a refusal here too. See
    // [`owned_handle`] for why it is not merely wrapped.
    let process_token = owned_handle(raw).ok_or(ERROR_INVALID_HANDLE)?;

    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut written: u32 = 0;
    // SAFETY: `process_token` is open with `TOKEN_QUERY` and alive for the
    // call; the buffer is one fully initialised `TOKEN_ELEVATION` and the
    // length given is its own `size_of`; `written` is a writable local.
    let read = unsafe {
        GetTokenInformation(
            process_token.as_raw_handle() as HANDLE,
            TokenElevation,
            ptr::from_mut(&mut elevation).cast(),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut written,
        )
    };
    if read == 0 {
        // SAFETY: `GetLastError` takes no arguments and reads this thread's
        // own last error code.
        return Err(unsafe { GetLastError() });
    }
    if elevation.TokenIsElevated == 0 {
        return impersonation_of(&process_token).map(Unprivileged::Token);
    }

    let mut link = TOKEN_LINKED_TOKEN {
        LinkedToken: ptr::null_mut(),
    };
    // SAFETY: the same open token, a buffer of one fully initialised
    // `TOKEN_LINKED_TOKEN` whose own `size_of` is the length given, and a
    // writable local for the length written. On success the call sets
    // `LinkedToken` to a handle this process owns alone.
    let read = unsafe {
        GetTokenInformation(
            process_token.as_raw_handle() as HANDLE,
            TokenLinkedToken,
            ptr::from_mut(&mut link).cast(),
            std::mem::size_of::<TOKEN_LINKED_TOKEN>() as u32,
            &mut written,
        )
    };
    if read == 0 {
        // SAFETY: `GetLastError` takes no arguments and reads this thread's
        // own last error code.
        let error = unsafe { GetLastError() };
        // The one error on this road that answers the question rather than
        // failing to put it. The doc comment above argues why.
        if error == ERROR_NO_SUCH_LOGON_SESSION {
            return Ok(Unprivileged::NoSuchAccount);
        }
        return Err(error);
    }
    // The call above succeeded, so `link.LinkedToken` is a handle this
    // process owns alone and nothing else will close. It is still put
    // through [`owned_handle`], because the failure road above leaves that
    // field null and a future edit that reordered these lines would wrap
    // it.
    let linked = owned_handle(link.LinkedToken).ok_or(ERROR_INVALID_HANDLE)?;
    // A linked token is already an impersonation token, so when it cannot
    // be duplicated it is used as it stands rather than the step refused.
    // The duplicate is what carries `TOKEN_IMPERSONATE`, and `AccessCheck`
    // does not need it, so the fallback still answers every path whose
    // descriptor can be read and only loses the probe. That is the elevated
    // helper's second check of a plan the unelevated end has already
    // admitted, so what it costs is a refusal, never an admission.
    Ok(Unprivileged::Token(
        impersonation_of(&linked).unwrap_or(linked),
    ))
}

/// `Ok(())` when nothing this process may do would put a different program
/// at `program`, and the sentence to show otherwise.
///
/// The path is walked from the drive down to the file. Every directory on
/// the way is asked whether this process may delete it, delete what is
/// inside it, or rewrite its security; the file at the end is asked whether
/// this process may write it, delete it, or rewrite its security. The first
/// element that is not on the disk ends the walk with one more question,
/// put to the directory above it: may this process create that name? If it
/// may, the whole chain below is the attacker's to build, which is the
/// planted case; if it may not, nothing can appear there without
/// Administrator and the program is admitted although it is not there.
///
/// Each of those questions goes to [`any_right_granted_on`], which reads
/// the element's descriptor where it can and opens the element itself where
/// it cannot.
///
/// There is one machine where no question is asked at all: one whose token
/// has no unprivileged sibling, which is [`Unprivileged::NoSuchAccount`]
/// and is answered `Ok(())`. That carve-out, and what it rests on, are set
/// out at [`an_unelevated_impersonation_token`].
#[cfg(windows)]
fn program_this_process_cannot_replace(program: &str) -> Result<(), String> {
    let token = match an_unelevated_impersonation_token() {
        Ok(Unprivileged::Token(token)) => token,
        Ok(Unprivileged::NoSuchAccount) => return Ok(()),
        Err(_) => return Err(cannot_be_checked(program)),
    };

    let mut chain: Vec<&Path> = Path::new(program).ancestors().collect();
    chain.reverse();

    // The element before this one, which is the directory that would have
    // to grant the name when the walk runs off the end of the disk. A
    // `&Path` rather than the descriptor it used to be, because the
    // descriptor is now one instrument of two and belongs inside
    // [`any_right_granted_on`] rather than in the loop state.
    let mut above: Option<&Path> = None;
    for (index, element) in chain.iter().copied().enumerate() {
        let leaf = index + 1 == chain.len();
        let rights: &[u32] = if leaf {
            &REPLACE_A_FILE
        } else {
            &REPLACE_A_DIRECTORY
        };
        match any_right_granted_on(element, &token, rights) {
            Ok(true) => return Err(can_be_replaced(program)),
            Ok(false) => above = Some(element),
            Err(ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND) => {
                let Some(above) = above else {
                    // Not even the drive answered, so there is nothing left
                    // to put the question to.
                    return Err(cannot_be_checked(program));
                };
                return match any_right_granted_on(above, &token, &CREATE_A_NAME) {
                    Ok(true) => Err(can_be_replaced(program)),
                    Ok(false) => Ok(()),
                    Err(_) => Err(cannot_be_checked(program)),
                };
            }
            Err(_) => return Err(cannot_be_checked(program)),
        }
    }
    Ok(())
}

/// The sentence a program this account could replace produces. It names
/// the program and what to do, and nothing of the permissions themselves:
/// an access control list in an error message is nothing a reader can act
/// on, and it tells anyone reading over their shoulder where to aim.
#[cfg(windows)]
fn can_be_replaced(program: &str) -> String {
    format!(
        "The helper refused a program this account can replace: {program}. Starting it as \
         Administrator would hand Administrator to whoever replaced it, so Brokey will not. \
         Install it somewhere only an administrator can write, under Program Files or \
         ProgramData, and try again."
    )
}

/// The sentence a program whose permissions could not be read produces.
/// Not being able to ask is never a reason to admit something.
#[cfg(windows)]
fn cannot_be_checked(program: &str) -> String {
    format!(
        "The helper could not read the permissions of {program}, so it refused to start it as \
         Administrator. Check that the path is one this account may read, then try again."
    )
}

/// The sentence a winget step Brokey did not build produces. It names the
/// whole command rather than the program, because the difference is usually
/// in the arguments and a reader needs to see which ones.
#[cfg(windows)]
fn not_a_winget_command(what: &str) -> String {
    format!(
        "The helper refused a winget step Brokey did not build: {what}. Only the exact install, \
         upgrade and uninstall commands the winget source produces may run as Administrator."
    )
}

/// The sentence a Chocolatey step Brokey did not build produces. It names
/// the whole command rather than the program, for the reason the winget
/// one does: the difference is usually in the arguments and a reader needs
/// to see which ones.
#[cfg(windows)]
fn not_a_choco_command(what: &str) -> String {
    format!(
        "The helper refused a Chocolatey step Brokey did not build: {what}. Only the exact \
         install, upgrade and uninstall commands the Chocolatey source produces may run as \
         Administrator."
    )
}

/// The one sentence a refusal produces, in the register the Linux list
/// uses: what was refused, and what the rule is.
#[cfg(windows)]
fn not_allowed(what: &str) -> String {
    format!(
        "The helper refused a step it does not allow: {what}. Only winget, Chocolatey and a \
         removal Windows itself recorded may run as Administrator."
    )
}

/// The sentence for a refused step. One shape everywhere, so the page and the
/// tests can recognise it.
#[cfg(unix)]
pub fn refusal(command: &Command, reason: &str) -> String {
    format!(
        "The helper refused a step it does not allow: {}. {reason}",
        describe(command)
    )
}

/// A command as one line, for a refusal to name. Both platforms use it:
/// Linux through [`refusal`], Windows through the winget arm, where the
/// difference between a command Brokey built and one it did not is often in
/// the arguments rather than the program.
fn describe(command: &Command) -> String {
    let mut text = command.program.clone();
    for arg in &command.args {
        text.push(' ');
        text.push_str(arg);
    }
    text
}

/// Why one step may not run, or `Ok(())`. The reason is a sentence without
/// the step in it; [`refusal`] adds that.
#[cfg(unix)]
pub fn check_step(step: &Step, allowed: &Allowed) -> Result<(), String> {
    if !step.needs_root {
        return Err("The helper only runs steps marked as needing root.".to_string());
    }
    if step.command.cwd.is_some() {
        return Err("Steps run by the helper must not set a working directory.".to_string());
    }
    for (key, value) in &step.command.env {
        if !ALLOWED_ENV.contains(&key.as_str()) {
            return Err(format!(
                "The environment variable {key} is not passed to the helper. Only {} are.",
                ALLOWED_ENV.join(", ")
            ));
        }
        if value.contains(['\0', '\n']) {
            return Err(format!("The value of {key} contains a control character."));
        }
        if !env_value_ok(key, value) {
            return Err(format!(
                "The value of {key} is not a shape the helper passes on. {}",
                match key.as_str() {
                    "DEBIAN_FRONTEND" => "A debconf frontend is one lowercase word.",
                    _ => "A locale has letters, digits and _ . @ - only.",
                }
            ));
        }
    }
    check_command(&step.command, allowed)
}

#[cfg(unix)]
fn check_command(command: &Command, allowed: &Allowed) -> Result<(), String> {
    let args: Vec<&str> = command.args.iter().map(String::as_str).collect();
    if args.iter().any(|a| a.contains(['\0', '\n'])) {
        return Err("An argument contains a control character.".to_string());
    }
    match command.program.as_str() {
        "pacman" => pacman(&args, allowed),
        "apt-get" => apt_get(&args, allowed),
        "dnf" => dnf(&args, allowed),
        "snap" => snap(&args),
        "flatpak" => flatpak(&args),
        "chwd" => chwd(&args),
        "rpm" => one_file("rpm", "-U", ".rpm", &args, allowed),
        "dpkg" => one_file("dpkg", "-i", ".deb", &args, allowed),
        "systemctl" => systemctl(&args),
        "ln" => ln(&args),
        _ => Err(format!(
            "Only {} may be run by the helper, by name and not by path.",
            list(&ALLOWED_PROGRAMS)
        )),
    }
}

/// `pacman` takes its operation as the first argument and nothing else looks
/// like one, so the check is positional rather than the verb-anywhere shape
/// the other tools get.
///
/// `-Sy` on its own is not here: refreshing the databases and then installing
/// against them is the partial upgrade Arch does not support, so a pacman
/// install or update is `-Syu` with names, and the store refreshes its own
/// copy of the databases without root.
#[cfg(unix)]
fn pacman(args: &[&str], allowed: &Allowed) -> Result<(), String> {
    const VERBS: [&str; 4] = ["-S", "-Syu", "-Rs", "-U"];
    // `--asdeps` is here for the AUR path, which installs a build's
    // repository dependencies through the helper before makepkg runs; without
    // it they would be left looking explicitly installed.
    const OPTIONS: [&str; 3] = ["--noconfirm", "--needed", "--asdeps"];
    let Some(verb) = args.first().filter(|v| VERBS.contains(v)) else {
        return Err(format!(
            "pacman must be given one of {} as its first argument.",
            list(&VERBS)
        ));
    };
    let (options, positionals) = split(&args[1..]);
    for option in options {
        if !OPTIONS.contains(&option) {
            return Err(format!(
                "The option {option} is not allowed for pacman {verb}."
            ));
        }
    }
    if *verb == "-U" {
        if positionals.is_empty() {
            return Err("pacman -U needs at least one package file.".to_string());
        }
        return positionals
            .iter()
            .try_for_each(|p| package_file(p, ".pkg.tar", allowed));
    }
    if positionals.is_empty() && matches!(*verb, "-S" | "-Rs") {
        return Err(format!("pacman {verb} needs at least one package name."));
    }
    positionals.iter().try_for_each(|n| name(n, NAME_MARKS))
}

#[cfg(unix)]
fn apt_get(args: &[&str], allowed: &Allowed) -> Result<(), String> {
    const VERBS: [&str; 4] = ["install", "remove", "update", "upgrade"];
    const OPTIONS: [&str; 3] = ["-y", "--only-upgrade", "--with-new-pkgs"];
    let (verb, _, positionals) = verb_options_positionals("apt-get", args, &VERBS, &OPTIONS)?;
    match verb {
        "install" => {
            if positionals.is_empty() {
                return Err(
                    "apt-get install needs at least one package name or .deb file.".to_string(),
                );
            }
            positionals.iter().try_for_each(|p| {
                if p.starts_with('/') {
                    package_file(p, ".deb", allowed)
                } else {
                    name(p, APT_NAME_MARKS)
                }
            })
        }
        "remove" => {
            if positionals.is_empty() {
                return Err("apt-get remove needs at least one package name.".to_string());
            }
            positionals.iter().try_for_each(|n| name(n, APT_NAME_MARKS))
        }
        _ => no_positionals("apt-get", verb, &positionals),
    }
}

/// `dnf install` takes an `.rpm` file as well as a name, the way `apt-get
/// install` takes a `.deb`; that is how the self-updater installs a
/// downloaded release on Fedora.
#[cfg(unix)]
fn dnf(args: &[&str], allowed: &Allowed) -> Result<(), String> {
    const VERBS: [&str; 4] = ["install", "remove", "upgrade", "makecache"];
    const OPTIONS: [&str; 1] = ["-y"];
    let (verb, _, positionals) = verb_options_positionals("dnf", args, &VERBS, &OPTIONS)?;
    match verb {
        "install" if positionals.is_empty() => {
            Err("dnf install needs at least one package name or .rpm file.".to_string())
        }
        "install" => positionals.iter().try_for_each(|p| {
            if p.starts_with('/') {
                package_file(p, ".rpm", allowed)
            } else {
                name(p, DNF_NAME_MARKS)
            }
        }),
        "remove" if positionals.is_empty() => {
            Err("dnf remove needs at least one package name.".to_string())
        }
        "makecache" => no_positionals("dnf", verb, &positionals),
        _ => positionals.iter().try_for_each(|n| name(n, DNF_NAME_MARKS)),
    }
}

/// `snap wait system seed.loaded`, exactly: a freshly started snapd seeds
/// itself before it accepts an install ("too early for operation, device
/// not yet seeded"), so setting snapd up waits for that in the same plan.
#[cfg(unix)]
pub const SNAP_WAIT_SEEDED: [&str; 3] = ["wait", "system", "seed.loaded"];

#[cfg(unix)]
fn snap(args: &[&str]) -> Result<(), String> {
    const VERBS: [&str; 3] = ["install", "remove", "refresh"];
    const OPTIONS: [&str; 1] = ["--classic"];
    if args.first() == Some(&"wait") {
        return if args == SNAP_WAIT_SEEDED {
            Ok(())
        } else {
            Err("snap may only wait for system seed.loaded.".to_string())
        };
    }
    let (verb, _, positionals) = verb_options_positionals("snap", args, &VERBS, &OPTIONS)?;
    if positionals.is_empty() && verb != "refresh" {
        return Err(format!("snap {verb} needs at least one snap name."));
    }
    positionals.iter().try_for_each(|n| name(n, NAME_MARKS))
}

/// `flatpak --system` is allowed even though a system installation normally
/// authorises itself through polkit: on a machine without an agent it needs
/// root, and the helper is the one root path the store has.
#[cfg(unix)]
fn flatpak(args: &[&str]) -> Result<(), String> {
    const VERBS: [&str; 3] = ["install", "uninstall", "update"];
    const OPTIONS: [&str; 3] = ["-y", "--noninteractive", "--system"];
    if args.contains(&"remote-add") {
        return flatpak_remote_add(args);
    }
    let (verb, _, positionals) = verb_options_positionals("flatpak", args, &VERBS, &OPTIONS)?;
    if positionals.is_empty() && verb != "update" {
        return Err(format!("flatpak {verb} needs at least one ref."));
    }
    positionals.iter().try_for_each(|r| {
        // flatpak reads a sole positional ending in `.flatpak` as a local
        // bundle and one ending in `.flatpakref` as a file that enrols a
        // remote, so a path here would install anything as root. A ref, an
        // id and a remote name never start with `/` or `.`.
        if flatpak_positional_is_a_path(r) {
            return Err(FLATPAK_NOT_A_FILE.to_string());
        }
        name(r, FLATPAK_REF_MARKS)
    })
}

/// The sentence for a flatpak positional that names a file rather than a
/// remote or a ref.
#[cfg(unix)]
pub const FLATPAK_NOT_A_FILE: &str = "A Flatpak positional must be a remote name or a ref, not a file path. The helper installs from remotes only.";

#[cfg(unix)]
fn flatpak_positional_is_a_path(value: &str) -> bool {
    value.starts_with(['/', '.'])
        || value
            .split('/')
            .any(|part| part.ends_with(".flatpak") || part.ends_with(".flatpakref"))
}

/// The sentence for a `flatpak remote-add` that is not the one form allowed.
#[cfg(unix)]
pub const FLATPAK_REMOTE_ADD_ONLY_FLATHUB: &str = "The helper adds one Flatpak remote only: Flathub, as flatpak remote-add --if-not-exists --system flathub with Flathub's own .flatpakrepo address.";

/// `flatpak remote-add --if-not-exists --system flathub <url>` with `url`
/// one of [`FLATHUB_REMOTES`], exactly in that order, and nothing else: not
/// another name, not another address, not without `--if-not-exists` (which
/// would fail on a machine that has it), not with any further option. A
/// `.flatpakrepo` file can point anywhere and carry a GPG key, so the
/// address is matched whole rather than by host.
#[cfg(unix)]
fn flatpak_remote_add(args: &[&str]) -> Result<(), String> {
    let form_ok = matches!(
        args,
        ["remote-add", "--if-not-exists", "--system", name, url]
            if FLATHUB_REMOTES.contains(&(*name, *url))
    );
    if form_ok {
        Ok(())
    } else {
        Err(FLATPAK_REMOTE_ADD_ONLY_FLATHUB.to_string())
    }
}

/// `systemctl enable --now snapd.socket`, exactly: setting snapd up is the
/// only reason the helper touches a unit.
#[cfg(unix)]
fn systemctl(args: &[&str]) -> Result<(), String> {
    if args == ["enable", "--now", SNAPD_SOCKET_UNIT] {
        Ok(())
    } else {
        Err(format!(
            "systemctl may only run enable --now {SNAPD_SOCKET_UNIT}."
        ))
    }
}

/// `ln -sfn /var/lib/snapd/snap /snap`, exactly: the link classic snaps
/// need on a distribution that mounts snaps elsewhere.
#[cfg(unix)]
fn ln(args: &[&str]) -> Result<(), String> {
    if args == ["-sfn", SNAP_MOUNT_DIR, SNAP_LINK] {
        Ok(())
    } else {
        Err(format!(
            "ln may only link {SNAP_MOUNT_DIR} to {SNAP_LINK} with -sfn."
        ))
    }
}

#[cfg(unix)]
fn chwd(args: &[&str]) -> Result<(), String> {
    match args {
        [verb, profile] if *verb == "-i" || *verb == "-r" => name(profile, NAME_MARKS),
        _ => Err("chwd takes exactly -i or -r and one profile name.".to_string()),
    }
}

#[cfg(unix)]
fn one_file(
    program: &str,
    flag: &str,
    extension: &str,
    args: &[&str],
    allowed: &Allowed,
) -> Result<(), String> {
    match args {
        [f, path] if *f == flag => package_file(path, extension, allowed),
        _ => Err(format!(
            "{program} takes exactly {flag} and one {extension} file."
        )),
    }
}

/// The verb is the first argument that is not an option, options may sit on
/// either side of it, and everything else is positional. That is how getopt
/// reads them, so a source may write `apt-get -y install foo` or
/// `apt-get install -y foo` and both mean the same to the helper.
#[cfg(unix)]
fn verb_options_positionals<'a>(
    program: &str,
    args: &[&'a str],
    verbs: &[&str],
    options: &[&str],
) -> Result<(&'a str, Vec<&'a str>, Vec<&'a str>), String> {
    let (opts, positionals) = split(args);
    let Some((verb, rest)) = positionals.split_first().filter(|(v, _)| verbs.contains(v)) else {
        return Err(format!("{program} must be given one of {}.", list(verbs)));
    };
    for option in &opts {
        if !options.contains(option) {
            return Err(format!(
                "The option {option} is not allowed for {program} {verb}."
            ));
        }
    }
    Ok((verb, opts, rest.to_vec()))
}

#[cfg(unix)]
fn split<'a>(args: &[&'a str]) -> (Vec<&'a str>, Vec<&'a str>) {
    args.iter().partition(|a| a.starts_with('-'))
}

#[cfg(unix)]
fn no_positionals(program: &str, verb: &str, positionals: &[&str]) -> Result<(), String> {
    if positionals.is_empty() {
        Ok(())
    } else {
        Err(format!("{program} {verb} takes no package names."))
    }
}

/// Punctuation a pacman, snap, chwd name may contain beside letters and digits.
#[cfg(unix)]
const NAME_MARKS: &str = "@._+-";
/// apt also takes `pkg:amd64`, `pkg=1.2-3` and `~` in a version.
#[cfg(unix)]
const APT_NAME_MARKS: &str = "@._+-:=~";
/// dnf takes `name:stream` for modules.
#[cfg(unix)]
const DNF_NAME_MARKS: &str = "@._+-:";
/// A ref (`app/org.gimp.GIMP/x86_64/stable`), an id or a remote name.
#[cfg(unix)]
const FLATPAK_REF_MARKS: &str = "./_-";

#[cfg(unix)]
fn name(value: &str, marks: &str) -> Result<(), String> {
    let shape_ok = !value.is_empty()
        && !value.starts_with('-')
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || marks.contains(c));
    if shape_ok {
        Ok(())
    } else {
        Err(format!(
            "{value} is not a valid name. A name has letters, digits and {} and does not start with a dash.",
            marks
                .chars()
                .map(|c| c.to_string())
                .collect::<Vec<_>>()
                .join(" ")
        ))
    }
}

/// An absolute, normal path (no `..`, no `.`) under one of the allowed
/// directories, whose file name carries the extension. Existence is not
/// checked here: the tool reports a missing file itself, and a check that
/// touched the disk would make this impure and untestable.
#[cfg(unix)]
fn package_file(value: &str, extension: &str, allowed: &Allowed) -> Result<(), String> {
    let path = Path::new(value);
    let normal = path.is_absolute()
        && path
            .components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_)));
    let file_ok = path
        .file_name()
        .and_then(|f| f.to_str())
        .is_some_and(|f| f.contains(extension) && !f.starts_with('-'));
    let under_allowed = allowed.package_dirs.iter().any(|dir| path.starts_with(dir));
    if normal && file_ok && under_allowed {
        Ok(())
    } else {
        Err(format!(
            "{value} is not a {extension} file under a directory the store may install from ({}).",
            allowed
                .package_dirs
                .iter()
                .map(|d| d.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }
}

#[cfg(unix)]
fn list(words: &[&str]) -> String {
    match words {
        [] => String::new(),
        [one] => one.to_string(),
        [head @ .., last] => format!("{} or {last}", head.join(", ")),
    }
}

/// This module is entirely about the Linux closed list; the Windows test
/// module sits beside it below.
#[cfg(unix)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SourceKind;

    fn step(program: &str, args: &[&str]) -> Step {
        Step {
            source: SourceKind::Pacman,
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

    fn allowed() -> Allowed {
        Allowed::for_home(Some(Path::new("/home/me")))
    }

    fn ok(program: &str, args: &[&str]) {
        let step = step(program, args);
        assert_eq!(
            check_step(&step, &allowed()),
            Ok(()),
            "{program} {args:?} should be allowed"
        );
    }

    fn refused(program: &str, args: &[&str]) -> String {
        let step = step(program, args);
        let plan = plan_of(vec![step]);
        let err = validate_with(&plan, &allowed())
            .expect_err(&format!("{program} {args:?} should be refused"));
        assert!(
            err.starts_with(&format!(
                "The helper refused a step it does not allow: {program}"
            )),
            "{err}"
        );
        assert!(err.ends_with('.'), "the reason is a sentence: {err}");
        assert!(!err.contains('\u{2014}'), "no em dashes: {err}");
        err
    }

    #[test]
    fn the_closed_list_accepts_what_the_sources_produce() {
        ok(
            "pacman",
            &["-S", "--noconfirm", "--needed", "steam", "lib32-mesa"],
        );
        ok("pacman", &["-Syu", "--noconfirm"]);
        ok(
            "pacman",
            &["-Syu", "--noconfirm", "--needed", "steam", "lib32-mesa"],
        );
        ok(
            "pacman",
            &["-S", "--needed", "--asdeps", "--noconfirm", "cmake"],
        );
        ok("pacman", &["-Rs", "--noconfirm", "steam"]);
        ok(
            "pacman",
            &[
                "-U",
                "--noconfirm",
                "/var/cache/pacman/pkg/steam-1.0-1-x86_64.pkg.tar.zst",
            ],
        );
        ok(
            "pacman",
            &[
                "-U",
                "/home/me/.cache/brokey/brokey-0.2.0-1-x86_64.pkg.tar.zst",
            ],
        );
        ok(
            "apt-get",
            &["install", "-y", "steam", "libgl1:i386", "foo=1.2-3~bpo1"],
        );
        ok("apt-get", &["-y", "install", "--only-upgrade", "firefox"]);
        ok(
            "apt-get",
            &[
                "install",
                "-y",
                "/home/me/.cache/brokey/brokey_0.2.0_amd64.deb",
            ],
        );
        ok("apt-get", &["remove", "-y", "steam"]);
        ok("apt-get", &["update"]);
        ok("apt-get", &["upgrade", "-y"]);
        ok("apt-get", &["upgrade", "--with-new-pkgs", "-y"]);
        ok("dnf", &["install", "-y", "steam", "nodejs:20"]);
        ok(
            "dnf",
            &[
                "install",
                "-y",
                "/home/me/.cache/brokey/http/downloads/brokey-0.2.0-1.x86_64.rpm",
            ],
        );
        ok("dnf", &["remove", "-y", "steam"]);
        ok("dnf", &["upgrade", "-y"]);
        ok("dnf", &["upgrade", "-y", "firefox"]);
        ok("dnf", &["makecache"]);
        ok("snap", &["install", "--classic", "code"]);
        ok("snap", &["remove", "code"]);
        ok("snap", &["refresh"]);
        ok("snap", &["refresh", "code"]);
        ok(
            "flatpak",
            &[
                "install",
                "--system",
                "-y",
                "--noninteractive",
                "flathub",
                "org.gimp.GIMP",
            ],
        );
        ok(
            "flatpak",
            &[
                "--system",
                "install",
                "-y",
                "flathub",
                "app/org.gimp.GIMP/x86_64/stable",
            ],
        );
        ok("flatpak", &["uninstall", "--system", "-y", "org.gimp.GIMP"]);
        ok("flatpak", &["update", "--system", "-y"]);
        ok("chwd", &["-i", "nvidia-open-dkms.prime"]);
        ok("chwd", &["-r", "nvidia-open-dkms.prime"]);
        ok(
            "rpm",
            &["-U", "/home/me/.cache/brokey/brokey-0.2.0-1.x86_64.rpm"],
        );
        ok(
            "dpkg",
            &["-i", "/home/me/.cache/brokey/brokey_0.2.0_amd64.deb"],
        );
        ok(
            "flatpak",
            &[
                "remote-add",
                "--if-not-exists",
                "--system",
                "flathub",
                "https://dl.flathub.org/repo/flathub.flatpakrepo",
            ],
        );
        ok("systemctl", &["enable", "--now", "snapd.socket"]);
        ok("ln", &["-sfn", "/var/lib/snapd/snap", "/snap"]);
        ok("snap", &["wait", "system", "seed.loaded"]);
    }

    /// Setting a source up is exactly four commands beyond the package
    /// installs: Flathub added, snapd's socket enabled, the `/snap` link,
    /// and the wait for snapd to seed.
    /// Every other spelling of each is refused.
    #[test]
    fn setting_a_source_up_is_exactly_four_commands() {
        for (name, url) in FLATHUB_REMOTES {
            ok(
                "flatpak",
                &["remote-add", "--if-not-exists", "--system", name, url],
            );
        }
        let flathub = "https://dl.flathub.org/repo/flathub.flatpakrepo";
        for args in [
            vec!["remote-add", "--if-not-exists", "--system", "flathub"],
            vec![
                "remote-add",
                "--if-not-exists",
                "--system",
                "flathub",
                flathub,
                "extra",
            ],
            vec!["remote-add", "--system", "flathub", flathub],
            vec!["remote-add", "--if-not-exists", "flathub", flathub],
            vec![
                "remote-add",
                "--if-not-exists",
                "--user",
                "flathub",
                flathub,
            ],
            vec![
                "remote-add",
                "--if-not-exists",
                "--system",
                "--no-gpg-verify",
                "flathub",
                flathub,
            ],
            vec!["remote-add", "--if-not-exists", "--system", "hub", flathub],
            vec![
                "remote-add",
                "--if-not-exists",
                "--system",
                "flathub",
                "https://dl.flathub.org/beta-repo/flathub-beta.flatpakrepo",
            ],
            vec![
                "remote-add",
                "--if-not-exists",
                "--system",
                "flathub",
                "https://evil.example/flathub.flatpakrepo",
            ],
            vec![
                "remote-add",
                "--if-not-exists",
                "--system",
                "flathub",
                "http://dl.flathub.org/repo/flathub.flatpakrepo",
            ],
            vec![
                "remote-add",
                "--if-not-exists",
                "--system",
                "flathub",
                "/tmp/flathub.flatpakrepo",
            ],
            vec![
                "--system",
                "remote-add",
                "--if-not-exists",
                "flathub",
                flathub,
            ],
            vec!["remote-delete", "--system", "flathub"],
            vec!["remote-modify", "--system", "flathub", "--url", flathub],
        ] {
            let err = refused("flatpak", &args);
            if args.contains(&"remote-add") {
                assert!(err.ends_with(FLATPAK_REMOTE_ADD_ONLY_FLATHUB), "{err}");
            }
        }

        for args in [
            vec!["enable", "snapd.socket"],
            vec!["enable", "--now", "snapd.service"],
            vec!["enable", "--now", "sshd.socket"],
            vec!["enable", "--now", "snapd.socket", "sshd.service"],
            vec!["start", "snapd.socket"],
            vec!["disable", "--now", "snapd.socket"],
            vec!["enable", "--now", "--force", "snapd.socket"],
            vec!["--now", "enable", "snapd.socket"],
            vec!["enable", "--now"],
            vec![],
        ] {
            let err = refused("systemctl", &args);
            assert!(
                err.ends_with("systemctl may only run enable --now snapd.socket."),
                "{err}"
            );
        }

        for args in [
            vec!["-sfn", "/var/lib/snapd/snap", "/usr/bin/snap"],
            vec!["-sfn", "/tmp/evil", "/snap"],
            vec!["-s", "/var/lib/snapd/snap", "/snap"],
            vec!["-sfn", "/snap", "/var/lib/snapd/snap"],
            vec!["-sfn", "/var/lib/snapd/snap", "/snap", "--force"],
            vec!["-sfn", "/var/lib/snapd/snap"],
            vec![],
        ] {
            let err = refused("ln", &args);
            assert!(
                err.ends_with("ln may only link /var/lib/snapd/snap to /snap with -sfn."),
                "{err}"
            );
        }
        for args in [
            vec!["wait", "system"],
            vec!["wait", "system", "seed.loaded", "extra"],
            vec!["wait", "core", "seed.loaded"],
            vec!["wait", "system", "refresh.hold"],
        ] {
            let err = refused("snap", &args);
            assert!(
                err.ends_with("snap may only wait for system seed.loaded."),
                "{err}"
            );
        }
        assert!(FLATPAK_REMOTE_ADD_ONLY_FLATHUB.ends_with('.'));
        assert!(!FLATPAK_REMOTE_ADD_ONLY_FLATHUB.contains('\u{2014}'));
    }

    #[test]
    fn anything_else_is_refused_whole() {
        refused("rm", &["-rf", "/"]);
        refused("sh", &["-c", "pacman -S foo"]);
        refused("/usr/bin/pacman", &["-S", "foo"]);
        refused("pacman", &["-Ss", "foo"]);
        refused("pacman", &["-Sy"]);
        refused("pacman", &["-Sy", "--noconfirm", "foo"]);
        refused("pacman", &["-S"]);
        refused("pacman", &["-Rs"]);
        refused("pacman", &["-S", "--config", "/tmp/evil.conf", "foo"]);
        refused("pacman", &["-S", "--noconfirm", "-foo"]);
        refused("pacman", &["-S", "foo bar"]);
        refused("pacman", &["-S", "foo;rm"]);
        refused("pacman", &["-U", "/tmp/evil.pkg.tar.zst"]);
        refused(
            "pacman",
            &["-U", "/var/cache/pacman/pkg/../../../tmp/evil.pkg.tar.zst"],
        );
        refused("pacman", &["-U", "/var/cache/pacman/pkg/notes.txt"]);
        refused("pacman", &["-U", "var/cache/pacman/pkg/x.pkg.tar.zst"]);
        refused("pacman", &["-U"]);
        refused(
            "apt-get",
            &["install", "-y", "--allow-unauthenticated", "foo"],
        );
        refused("apt-get", &["update", "foo"]);
        refused("apt-get", &["install", "-y", "/tmp/evil.deb"]);
        refused("apt-get", &["dist-upgrade", "-y"]);
        refused("dnf", &["install", "-y", "--nogpgcheck", "foo"]);
        refused("dnf", &["makecache", "foo"]);
        refused("dnf", &["install", "-y"]);
        refused("dnf", &["install", "-y", "/tmp/x.rpm"]);
        refused("dnf", &["install", "-y", "/home/me/.cache/brokey/x.deb"]);
        refused("dnf", &["remove", "-y", "/home/me/.cache/brokey/x.rpm"]);
        refused("dnf", &["upgrade", "-y", "/home/me/.cache/brokey/x.rpm"]);
        refused("snap", &["install", "--dangerous", "foo"]);
        refused("snap", &["install"]);
        refused(
            "flatpak",
            &["install", "-y", "flathub", "org.gimp.GIMP", "--user"],
        );
        refused("flatpak", &["run", "org.gimp.GIMP"]);
        refused("flatpak", &["install", "-y"]);
        refused("flatpak", &["install", "-y", "flathub", "org.gimp.GIMP;x"]);
        refused("chwd", &["-i"]);
        refused("chwd", &["-a"]);
        refused("chwd", &["-i", "profile", "--extra"]);
        refused("rpm", &["-e", "foo"]);
        refused("rpm", &["-U", "/tmp/x.rpm"]);
        refused("rpm", &["-U", "/home/me/.cache/brokey/x.deb"]);
        refused(
            "dpkg",
            &["-i", "/home/me/.cache/brokey/x.deb", "--force-all"],
        );
        refused("dpkg", &["--configure", "-a"]);
    }

    #[test]
    fn a_flatpak_positional_is_never_a_file() {
        for path in [
            "/tmp/evil.flatpak",
            "/home/me/evil.flatpakref",
            "/tmp/evil.flatpak/",
            "./evil.flatpak",
            "../evil.flatpak",
            "evil.flatpak",
            "evil.flatpakref",
            "dir/evil.flatpak",
            "org.gimp.GIMP.flatpakref/x",
        ] {
            for args in [
                vec!["install", "--system", "-y", path],
                vec!["install", "--system", "-y", "flathub", path],
                vec!["install", "--system", "-y", path, "org.gimp.GIMP"],
                vec!["update", "--system", "-y", path],
                vec!["uninstall", "--system", "-y", path],
            ] {
                let err = refused("flatpak", &args);
                assert!(err.ends_with(FLATPAK_NOT_A_FILE), "{err}");
            }
        }
        ok(
            "flatpak",
            &["install", "--system", "-y", "flathub", "org.gimp.GIMP"],
        );
        ok(
            "flatpak",
            &[
                "install",
                "--system",
                "-y",
                "flathub",
                "app/org.gimp.GIMP/x86_64/stable",
            ],
        );
        ok("flatpak", &["update", "--system", "-y", "org.gimp.GIMP"]);
        assert!(FLATPAK_NOT_A_FILE.ends_with('.'));
        assert!(!FLATPAK_NOT_A_FILE.contains('\u{2014}'));
    }

    #[test]
    fn a_refusal_names_the_step_and_the_reason() {
        let err = refused("pacman", &["-S", "--config", "/tmp/evil.conf", "foo"]);
        assert_eq!(
            err,
            "The helper refused a step it does not allow: pacman -S --config /tmp/evil.conf foo. \
             The option --config is not allowed for pacman -S."
        );
    }

    #[test]
    fn one_bad_step_refuses_the_whole_plan() {
        let plan = plan_of(vec![
            step("pacman", &["-S", "--noconfirm", "foo"]),
            step("pacman", &["-S", "--noconfirm", "bar"]),
            step("curl", &["https://example.org"]),
        ]);
        let err = validate(&plan).unwrap_err();
        assert!(err.contains("curl https://example.org"), "{err}");
    }

    #[test]
    fn session_steps_and_working_directories_do_not_belong_in_the_helper() {
        let mut session = step("pacman", &["-S", "--noconfirm", "foo"]);
        session.needs_root = false;
        assert!(
            check_step(&session, &allowed())
                .unwrap_err()
                .contains("needing root")
        );

        let mut with_cwd = step("pacman", &["-S", "--noconfirm", "foo"]);
        with_cwd.command.cwd = Some(PathBuf::from("/tmp"));
        assert!(
            check_step(&with_cwd, &allowed())
                .unwrap_err()
                .contains("working directory")
        );
    }

    #[test]
    fn only_three_environment_variables_pass() {
        let mut fine = step("apt-get", &["install", "-y", "foo"]);
        fine.command.env = vec![
            ("DEBIAN_FRONTEND".to_string(), "noninteractive".to_string()),
            ("LC_ALL".to_string(), "C.UTF-8".to_string()),
            ("LANG".to_string(), "C.UTF-8".to_string()),
        ];
        assert_eq!(check_step(&fine, &allowed()), Ok(()));

        let mut sneaky = step("apt-get", &["install", "-y", "foo"]);
        sneaky.command.env = vec![("LD_PRELOAD".to_string(), "/tmp/evil.so".to_string())];
        assert!(
            check_step(&sneaky, &allowed())
                .unwrap_err()
                .contains("LD_PRELOAD")
        );

        let mut newline = step("apt-get", &["install", "-y", "foo"]);
        newline.command.env = vec![("LANG".to_string(), "C\nPATH=/tmp".to_string())];
        assert!(
            check_step(&newline, &allowed())
                .unwrap_err()
                .contains("control character")
        );
    }

    #[test]
    fn an_environment_value_must_look_like_what_the_variable_holds() {
        // debconf's make_frontend evaluates the frontend name as Perl, as root.
        let injection = "Noninteractive; system(\"touch /tmp/pwned\"); 1; #";
        let mut sneaky = step("apt-get", &["install", "-y", "foo"]);
        sneaky.command.env = vec![("DEBIAN_FRONTEND".to_string(), injection.to_string())];
        let err = check_step(&sneaky, &allowed()).unwrap_err();
        assert!(err.contains("DEBIAN_FRONTEND"), "{err}");
        assert!(
            err.ends_with("A debconf frontend is one lowercase word."),
            "{err}"
        );

        for (key, value) in [
            ("DEBIAN_FRONTEND", "Noninteractive"),
            ("DEBIAN_FRONTEND", "non interactive"),
            ("DEBIAN_FRONTEND", ""),
            ("LC_ALL", "C.UTF-8; rm -rf /"),
            ("LANG", "en_GB.UTF-8 x"),
            ("LANG", "$(id)"),
            ("LANG", ""),
        ] {
            let mut bad = step("apt-get", &["install", "-y", "foo"]);
            bad.command.env = vec![(key.to_string(), value.to_string())];
            let err = check_step(&bad, &allowed())
                .expect_err(&format!("{key}={value:?} should be refused"));
            assert!(err.contains(key), "{err}");
        }
        for (key, value) in [
            ("DEBIAN_FRONTEND", "noninteractive"),
            ("DEBIAN_FRONTEND", "text"),
            ("LC_ALL", "C.UTF-8"),
            ("LANG", "en_GB.UTF-8"),
            ("LANG", "de_DE@euro"),
            ("LANG", "C"),
        ] {
            let mut fine = step("apt-get", &["install", "-y", "foo"]);
            fine.command.env = vec![(key.to_string(), value.to_string())];
            assert_eq!(check_step(&fine, &allowed()), Ok(()), "{key}={value}");
        }
        assert!(
            !env_value_ok("PATH", "/usr/bin"),
            "a key outside the list has no shape"
        );
    }

    #[test]
    fn the_system_caches_are_allowed_without_a_home() {
        let plan = plan_of(vec![step(
            "pacman",
            &["-U", "/var/cache/pacman/pkg/foo-1-1-any.pkg.tar.zst"],
        )]);
        assert_eq!(validate(&plan), Ok(()));
        let plan = plan_of(vec![step(
            "pacman",
            &["-U", "/home/me/.cache/brokey/foo-1-1-any.pkg.tar.zst"],
        )]);
        assert!(
            validate(&plan).is_err(),
            "the home cache needs to be named by the caller"
        );
    }

    #[test]
    fn an_empty_plan_is_fine() {
        assert_eq!(validate(&plan_of(Vec::new())), Ok(()));
    }
}

#[cfg(windows)]
#[cfg(test)]
mod windows_tests {
    use super::*;
    use crate::model::{Command, SourceKind};
    use crate::sources::windows::choco;
    use crate::sources::windows::winget::{OpKind, operation_step, update_all_step};

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

    /// The token the gate puts its questions to, or `None` where this
    /// machine has no unprivileged account for them to be about.
    ///
    /// A process that is elevated and has no filtered token linked to it is
    /// the built-in Administrator, a machine with UAC turned off, or a
    /// domain administrator's console; CI's own Windows runner is the first
    /// of those. There the gate admits every program, deliberately, so every
    /// test about a refusal is a test of a property the machine does not
    /// have. Such a test says which environment it is in and what was
    /// therefore not tested, and stops. It is not `#[ignore]`, which would
    /// hide it from the run, and the assertion it would have made is not
    /// weakened.
    fn an_account_that_is_not_administrator(untested: &str) -> Option<OwnedHandle> {
        match an_unelevated_impersonation_token() {
            Ok(Unprivileged::Token(token)) => Some(token),
            Ok(Unprivileged::NoSuchAccount) => {
                eprintln!(
                    "skipped: this process is elevated and no filtered token is linked to its \
                     own, so this machine has no unprivileged account and {untested} was not \
                     tested."
                );
                None
            }
            Err(error) => {
                panic!("this process has a token that stands for it without Administrator: {error}")
            }
        }
    }

    /// A full path ending in `winget.exe`, which is all this arm's tests
    /// need: they are about the arguments, and the file itself is asked
    /// about by the gate after the arm rather than inside it.
    ///
    /// It is no longer the path `winget_program` answers on a real machine.
    /// That one is the executable the App Execution Alias names, under
    /// `C:\Program Files\WindowsApps`, and
    /// `the_resolved_winget_is_not_a_program_this_account_can_replace` is
    /// the test that puts the real one to the real gate.
    const WINGET: &str = r"C:\Users\me\AppData\Local\Microsoft\WindowsApps\winget.exe";

    /// The three things winget is asked to do, exactly as the source
    /// builds them, and nothing else.
    #[test]
    fn the_winget_operations_are_allowed() {
        for kind in [OpKind::Install, OpKind::Update, OpKind::Remove] {
            let plan = plan_of(vec![operation_step(kind, "Valve.Steam", WINGET)]);
            assert_eq!(validate(&plan), Ok(()), "{kind:?} should be allowed");
        }
    }

    /// Updating everything winget can is a different command from any one
    /// package's, and it is one the source builds, so the list knows it.
    /// Without this the Updates page's "Update all" would be refused after
    /// the prompt rather than before anyone asked for it.
    #[test]
    fn updating_everything_winget_can_is_allowed() {
        assert_eq!(validate(&plan_of(vec![update_all_step(WINGET)])), Ok(()));
    }

    /// A verb that is not one of the three is refused even from a real
    /// winget.
    #[test]
    fn another_winget_verb_is_refused() {
        let step = step_from(
            SourceKind::Winget,
            WINGET,
            &["export", "--output", r"C:\everything.json"],
        );
        let expected = not_a_winget_command(&describe(&step.command));
        let err = validate(&plan_of(vec![step])).expect_err("export is not on the list");
        assert_eq!(err, expected, "the sentence this test means");
        assert!(err.contains("export"), "the refusal names the verb: {err}");
        assert!(err.ends_with('.'), "the reason is a sentence: {err}");
        assert!(!err.contains('\u{2014}'), "no em dashes: {err}");
    }

    /// A winget command with no arguments at all has no verb to name, so
    /// the refusal must fall back to the whole command rather than naming
    /// nothing. Before this the sentence read "...Brokey did not build: .
    /// Only..." with an empty name where the offending command should be.
    #[test]
    fn a_winget_command_with_no_arguments_names_the_program_not_nothing() {
        let step = step_from(SourceKind::Winget, WINGET, &[]);
        let expected = not_a_winget_command(&describe(&step.command));
        let err = validate(&plan_of(vec![step])).expect_err("no verb is on the list");
        assert_eq!(err, expected, "the sentence this test means");
        assert!(
            err.contains(WINGET),
            "the refusal names the command it refused: {err}"
        );
        assert!(
            !err.contains(": .\u{0020}"),
            "the name must not be empty: {err}"
        );
        assert!(err.ends_with('.'), "the reason is a sentence: {err}");
    }

    /// The list is about the command, not the source that claims it. A step
    /// that says it is winget and runs something else is refused.
    #[test]
    fn a_step_cannot_claim_to_be_winget_and_run_something_else() {
        const POWERSHELL: &str = r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe";
        let plan = plan_of(vec![step_from(
            SourceKind::Winget,
            POWERSHELL,
            &["-Command", "Remove-Item C:/Windows -Recurse"],
        )]);
        assert_eq!(validate(&plan), Err(not_allowed(POWERSHELL)));
    }

    /// A bare name is refused. The helper searches for nothing, so the
    /// unelevated side resolves the path, and a step that has not had that
    /// done to it is not one Brokey built.
    #[test]
    fn a_bare_winget_name_is_refused() {
        let mut step = operation_step(OpKind::Install, "Valve.Steam", WINGET);
        step.command.program = "winget.exe".to_string();
        assert_eq!(
            validate(&plan_of(vec![step])),
            Err(not_allowed("winget.exe")),
            "a bare name is not a full path"
        );
    }

    /// `winget.exe` on a network share is not this machine's winget. A UNC
    /// path is absolute, so absolute is not the question; a drive letter
    /// is.
    #[test]
    fn a_winget_on_a_share_is_refused() {
        const SHARE: &str = r"\\attacker\share\winget.exe";
        let step = operation_step(OpKind::Install, "Valve.Steam", SHARE);
        assert_eq!(
            validate(&plan_of(vec![step])),
            Err(not_allowed(SHARE)),
            "a share is not a disk of this machine"
        );
    }

    /// `--manifest` runs an installer of the caller's choosing through a
    /// genuine winget, which is arbitrary elevated execution. The source
    /// never builds it, so it is refused.
    #[test]
    fn a_manifest_is_refused() {
        let step = step_from(
            SourceKind::Winget,
            WINGET,
            &["install", "--manifest", r"C:\Users\me\Downloads\evil.yaml"],
        );
        let expected = not_a_winget_command(&describe(&step.command));
        let err =
            validate(&plan_of(vec![step])).expect_err("--manifest is not a command Brokey builds");
        assert_eq!(err, expected, "the sentence this test means");
        assert!(err.contains("--manifest"), "the refusal names it: {err}");
        assert!(err.ends_with('.'), "the reason is a sentence: {err}");
    }

    /// `--override` hands a command line straight to the installer. Same
    /// answer, and it is why the whole argument vector is compared rather
    /// than the verb alone.
    #[test]
    fn an_override_is_refused() {
        let mut step = operation_step(OpKind::Install, "Valve.Steam", WINGET);
        step.command.args.push("--override".to_string());
        step.command
            .args
            .push("/C powershell -Command whoami".to_string());
        let expected = not_a_winget_command(&describe(&step.command));
        let err =
            validate(&plan_of(vec![step])).expect_err("--override is not a command Brokey builds");
        assert_eq!(err, expected, "the sentence this test means");
        assert!(err.contains("--override"), "{err}");
    }

    /// A winget step carries no environment, because `operation_step`
    /// builds none. Comparing the whole command is what says so, rather
    /// than a Windows list of permitted variables that does not exist.
    #[test]
    fn a_winget_step_with_an_environment_is_refused() {
        let mut step = operation_step(OpKind::Install, "Valve.Steam", WINGET);
        step.command
            .env
            .push(("PATH".to_string(), r"C:\somewhere\else".to_string()));
        let expected = not_a_winget_command(&describe(&step.command));
        assert_eq!(validate(&plan_of(vec![step])), Err(expected));
    }

    /// A working directory is not something the source sets either, and it
    /// is part of the same comparison.
    #[test]
    fn a_winget_step_with_a_working_directory_is_refused() {
        let mut step = operation_step(OpKind::Install, "Valve.Steam", WINGET);
        step.command.cwd = Some(std::path::PathBuf::from(r"C:\somewhere\else"));
        let expected = not_a_winget_command(&describe(&step.command));
        assert_eq!(validate(&plan_of(vec![step])), Err(expected));
    }

    /// An id beginning with a dash is an option to winget, and rebuilding
    /// the command around it would agree with itself. The id is checked
    /// before the rebuild for exactly that reason, and an empty one with
    /// it.
    #[test]
    fn an_id_that_is_not_an_id_is_refused() {
        let smuggled = operation_step(OpKind::Install, "--override", WINGET);
        let expected = not_a_winget_command(&describe(&smuggled.command));
        let err = validate(&plan_of(vec![smuggled])).expect_err("an id is never an option");
        assert_eq!(err, expected, "the sentence this test means");
        assert!(err.contains("--override"), "{err}");
        let empty = operation_step(OpKind::Install, "", WINGET);
        let expected = not_a_winget_command(&describe(&empty.command));
        assert_eq!(
            validate(&plan_of(vec![empty])),
            Err(expected),
            "an id is never empty"
        );
    }

    /// The path a Chocolatey step really carries: `choco.exe` in the `bin`
    /// directory under `%ChocolateyInstall%`, which is where
    /// `choco_program` finds it on a machine that has Chocolatey.
    const CHOCO: &str = r"C:\ProgramData\chocolatey\bin\choco.exe";

    /// One Chocolatey step for this machine's `choco.exe`, built the way
    /// the source builds it.
    fn choco_step(kind: choco::OpKind, id: &str) -> Step {
        choco::operation_step(kind, id, CHOCO)
    }

    /// The predicate itself, put to the volumes this machine really has.
    ///
    /// A path under a drive letter that is not a fixed disk cannot be
    /// manufactured in a unit test: it needs a mapped network drive or
    /// removable media, and a test may make neither. So the machine's own
    /// drive letters are enumerated, each is asked what kind of volume it
    /// is, and the predicate is required to agree for every one of them. A
    /// machine with only fixed volumes says which half it could not test
    /// rather than pretending it did.
    #[test]
    fn only_a_local_fixed_volume_is_a_local_disk() {
        use windows_sys::Win32::Storage::FileSystem::GetLogicalDrives;

        // SAFETY: `GetLogicalDrives` takes no arguments and returns a
        // bitmask of the drive letters this machine has.
        let mask = unsafe { GetLogicalDrives() };
        let mut fixed = 0;
        let mut not_fixed = 0;
        for index in 0..26u32 {
            if mask & (1u32 << index) == 0 {
                continue;
            }
            let letter = b'A' + index as u8;
            let root: [u16; 4] = [u16::from(letter), u16::from(b':'), u16::from(b'\\'), 0];
            // SAFETY: `root` is a null-terminated wide string of four units
            // that outlives the call, and the call takes nothing else. This
            // is the test's own instrument, so that the predicate is not
            // checked against itself.
            let kind = unsafe { GetDriveTypeW(root.as_ptr()) };
            let program = format!(r"{}:\a\program.exe", letter as char);
            assert_eq!(
                on_a_local_disk(Path::new(&program)),
                kind == DRIVE_FIXED,
                "{program} is on a volume GetDriveTypeW calls {kind}"
            );
            let verbatim = format!(r"\\?\{}:\a\program.exe", letter as char);
            assert_eq!(
                on_a_local_disk(Path::new(&verbatim)),
                kind == DRIVE_FIXED,
                "the verbatim spelling names the same volume: {verbatim}"
            );
            if kind == DRIVE_FIXED {
                fixed += 1;
            } else {
                not_fixed += 1;
            }
        }
        assert!(fixed > 0, "every Windows has a fixed volume to install on");
        if not_fixed == 0 {
            eprintln!(
                "skipped: every drive letter on this machine is a fixed disk, so a letter that \
                 must be refused was not tested."
            );
        }
        assert!(
            !on_a_local_disk(Path::new(r"\\somewhere\share\program.exe")),
            "a UNC path names no drive letter at all"
        );
    }

    /// A `choco.exe` where no unprivileged process can put one, which on
    /// any Windows is `%SystemRoot%\System32`. No such file is there and
    /// none needs to be: nothing unprivileged can create the name either,
    /// which is the question the gate puts to a path that is not on the
    /// disk.
    ///
    /// `CHOCO` cannot serve for a test that expects `Ok(())` any more.
    /// Whether the real default path is admitted depends on whether
    /// Chocolatey is installed on the machine running the test, because
    /// `C:\ProgramData` lets any user create `chocolatey` when it is not,
    /// and that difference is the whole point of the gate.
    fn choco_somewhere_unwritable() -> String {
        Path::new(&std::env::var("SystemRoot").expect("Windows sets SystemRoot"))
            .join("System32")
            .join("choco.exe")
            .to_string_lossy()
            .into_owned()
    }

    /// The three things Chocolatey is asked to do, exactly as the source
    /// builds them. Every Chocolatey step needs Administrator, because the
    /// default install root is under `C:\ProgramData`, so until this arm
    /// existed all three were refused before the user was ever asked.
    #[test]
    fn a_choco_install_the_source_would_build_is_allowed() {
        let program = choco_somewhere_unwritable();
        for kind in [
            choco::OpKind::Install,
            choco::OpKind::Update,
            choco::OpKind::Remove,
        ] {
            let plan = plan_of(vec![choco::operation_step(kind, "7zip", &program)]);
            assert_eq!(validate(&plan), Ok(()), "{kind:?} should be allowed");
        }
    }

    /// `--install-arguments` hands a command line straight to the package's
    /// own installer, which is arbitrary elevated execution through a
    /// genuine `choco.exe`. The source never builds it, and comparing the
    /// whole command rather than the verb alone is what refuses it.
    ///
    /// It arrives twice: appended, which changes the number of arguments,
    /// and written over `-y`, which does not. The second is the one that
    /// says the *content* of every argument is compared. A comparison that
    /// counted the arguments and read all but the last would pass the first
    /// and admit the second.
    #[test]
    fn a_choco_command_with_an_extra_argument_is_refused() {
        const SMUGGLED: &str = r"--install-arguments=/D=C:\Windows";

        let mut appended = choco_step(choco::OpKind::Install, "7zip");
        appended.command.args.push(SMUGGLED.to_string());

        let mut in_place = choco_step(choco::OpKind::Install, "7zip");
        let last = in_place.command.args.len() - 1;
        assert_eq!(in_place.command.args[last], "-y", "the last argument");
        in_place.command.args[last] = SMUGGLED.to_string();

        for step in [appended, in_place] {
            let args = step.command.args.clone();
            let expected = not_a_choco_command(&describe(&step.command));
            let err =
                validate(&plan_of(vec![step])).expect_err("the source builds no such argument");
            assert_eq!(err, expected, "the sentence this test means, for {args:?}");
            assert!(
                err.contains(SMUGGLED),
                "the refusal names it: {err} for {args:?}"
            );
            assert!(
                err.contains("did not build"),
                "the arguments are what was refused: {err}"
            );
            assert!(err.ends_with('.'), "the reason is a sentence: {err}");
            assert!(!err.contains('\u{2014}'), "no em dashes: {err}");
        }
    }

    /// A working directory is not something the source sets either, and it
    /// is part of the same comparison. It is not cosmetic: the current
    /// directory sits in the DLL search order, so a working directory
    /// somebody else can write to is a way into an elevated `choco.exe`.
    #[test]
    fn a_choco_step_with_a_working_directory_is_refused() {
        let mut step = choco_step(choco::OpKind::Install, "7zip");
        step.command.cwd = Some(std::path::PathBuf::from(r"C:\Users\me\Downloads"));
        let expected = not_a_choco_command(&describe(&step.command));
        let err = validate(&plan_of(vec![step]))
            .expect_err("the source sets no working directory, so an equal comparison refuses one");
        assert_eq!(err, expected, "the sentence this test means");
        assert!(
            err.contains("did not build"),
            "the command is what was refused: {err}"
        );
        assert!(err.ends_with('.'), "the reason is a sentence: {err}");
    }

    /// A command one argument short is not the command the source builds
    /// either, whichever of the three it started as.
    #[test]
    fn a_choco_command_missing_an_argument_is_refused() {
        for kind in [
            choco::OpKind::Install,
            choco::OpKind::Update,
            choco::OpKind::Remove,
        ] {
            let mut step = choco_step(kind, "7zip");
            step.command.args.retain(|arg| arg != "-y");
            let expected = not_a_choco_command(&describe(&step.command));
            let err = validate(&plan_of(vec![step]))
                .expect_err("-y is one of the arguments the source builds");
            assert_eq!(err, expected, "the sentence this test means");
            assert!(err.contains("7zip"), "{err}");
            assert!(err.ends_with('.'), "the reason is a sentence: {err}");
        }
    }

    /// An id beginning with a dash is an option to `choco.exe`, and
    /// rebuilding the command around it would agree with itself: an id of
    /// `-y` rebuilds into the very command it was taken from. The id is
    /// checked before the rebuild for exactly that reason, and an empty one
    /// with it.
    ///
    /// A slash is the other spelling. Chocolatey's own documentation gives
    /// `-y`, `--yes` and `/y` as one switch and its parser takes the slash
    /// form of every option, so `/y` and `/force` are options written the
    /// other way and are refused by the same rule. The dash cases and the
    /// slash cases are driven through together here, because a guard that
    /// caught only one of them would pass this test with the other half
    /// removed.
    #[test]
    fn an_id_that_is_really_an_option_is_refused() {
        for id in ["--force", "-y", "/force", "/y", "/?", ""] {
            let step = choco_step(choco::OpKind::Install, id);
            let expected = not_a_choco_command(&describe(&step.command));
            let err = validate(&plan_of(vec![step]))
                .expect_err("an id is never an option and never empty");
            assert_eq!(err, expected, "the sentence this test means, for {id:?}");
            assert!(err.ends_with('.'), "the reason is a sentence: {err}");
        }
        for id in ["--force", "/force"] {
            let forced = choco_step(choco::OpKind::Install, id);
            let err = validate(&plan_of(vec![forced])).expect_err("an id is never an option");
            assert!(err.contains(id), "the refusal names it: {err}");
        }
    }

    /// The program's file name is compared whole, so neither a name that
    /// begins with `choco.exe` nor one that ends with it is it.
    /// `choco.exe.exe` is the first and `notchoco.exe` the second, and each
    /// catches a different way of writing the check too loosely.
    #[test]
    fn a_program_that_is_not_choco_exe_is_refused() {
        for program in [
            r"C:\Windows\System32\cmd.exe",
            r"C:\ProgramData\chocolatey\bin\choco.exe.exe",
            r"C:\Users\me\Downloads\notchoco.exe",
        ] {
            let step = choco::operation_step(choco::OpKind::Install, "7zip", program);
            let err = validate(&plan_of(vec![step])).expect_err("only choco.exe runs here");
            assert_eq!(err, not_allowed(program), "the sentence this test means");
            assert!(err.ends_with('.'), "the reason is a sentence: {err}");
        }
    }

    /// `choco.exe` on a network share is not this machine's Chocolatey. The
    /// path is a real UNC path, with both leading backslashes, because that
    /// is the one this arm has to refuse for itself: a UNC path answers
    /// `true` to `Path::is_absolute`, so absolute is not the question;
    /// a drive letter is, and `on_a_local_disk` is the only thing that asks
    /// it. With one backslash the path is rooted but not absolute, and the
    /// `Prefix::Disk` half of `on_a_local_disk` would never run.
    #[test]
    fn a_choco_exe_on_a_network_path_is_refused() {
        const SHARE: &str = r"\\somewhere\share\choco.exe";
        let step = choco::operation_step(choco::OpKind::Install, "7zip", SHARE);
        let err =
            validate(&plan_of(vec![step])).expect_err("a share is not a disk of this machine");
        assert_eq!(err, not_allowed(SHARE), "the sentence this test means");
        assert!(
            err.contains("does not allow") && !err.contains("did not build"),
            "the program is what was refused, before any argument was read: {err}"
        );
        assert!(err.ends_with('.'), "the reason is a sentence: {err}");
    }

    /// A Chocolatey step carries no environment, because `operation_step`
    /// builds none. Comparing the whole command is what says so, rather
    /// than a Windows list of permitted variables that does not exist.
    /// `ChocolateyInstall` is the one that would matter: it moves the
    /// install root, and this is the elevated process.
    #[test]
    fn a_choco_command_carrying_an_environment_is_refused() {
        let mut step = choco_step(choco::OpKind::Install, "7zip");
        step.command.env.push((
            "ChocolateyInstall".to_string(),
            r"C:\Users\me\somewhere-else".to_string(),
        ));
        let expected = not_a_choco_command(&describe(&step.command));
        let err = validate(&plan_of(vec![step]))
            .expect_err("the source sets no environment, so an equal comparison refuses one");
        assert_eq!(err, expected, "the sentence this test means");
        assert!(
            err.contains("did not build"),
            "the command is what was refused, not the program: {err}"
        );
        assert!(err.contains(CHOCO), "the refusal names the command: {err}");
        assert!(err.ends_with('.'), "the reason is a sentence: {err}");
    }

    /// A verb the source does not build is refused even from a real
    /// `choco.exe`. `push` uploads a package with an API key and `list`
    /// changes nothing at all, and both get the same answer, because the
    /// question is provenance rather than danger. A command with no
    /// arguments has no verb to name, so the refusal falls back to naming
    /// the whole command.
    #[test]
    fn a_verb_choco_does_not_have_is_refused() {
        for args in [vec!["list"], vec!["push", "evil.nupkg"], vec![]] {
            let step = step_from(SourceKind::Choco, CHOCO, &args);
            let expected = not_a_choco_command(&describe(&step.command));
            let err = validate(&plan_of(vec![step]))
                .expect_err("only install, upgrade and uninstall are built");
            assert_eq!(err, expected, "the sentence this test means, for {args:?}");
            assert!(err.contains(CHOCO), "the refusal names the command: {err}");
            assert!(!err.contains(": . "), "the name must not be empty: {err}");
            assert!(err.ends_with('.'), "the reason is a sentence: {err}");
        }
    }

    /// The label chooses which question is asked and nothing more. A
    /// Chocolatey command that says it is winget is refused by the winget
    /// arm, and a winget command that says it is Chocolatey by this one.
    #[test]
    fn a_choco_command_cannot_borrow_another_sources_label() {
        let mut as_winget = choco_step(choco::OpKind::Install, "7zip");
        as_winget.source = SourceKind::Winget;
        assert_eq!(
            validate(&plan_of(vec![as_winget])),
            Err(not_allowed(CHOCO)),
            "choco.exe is not winget.exe, whatever the step says"
        );

        let mut as_choco = operation_step(OpKind::Install, "Valve.Steam", WINGET);
        as_choco.source = SourceKind::Choco;
        assert_eq!(
            validate(&plan_of(vec![as_choco])),
            Err(not_allowed(WINGET)),
            "winget.exe is not choco.exe, whatever the step says"
        );
    }

    /// The hole this gate exists to close. `C:\ProgramData` lets any user
    /// create a directory in it, so on a machine without Chocolatey an
    /// unprivileged process can make `chocolatey\bin\choco.exe` itself,
    /// and `choco_exe` then resolves that file, the arm admits it and
    /// `ShellExecuteEx` starts it as Administrator. A directory this test
    /// has just made stands in for that folder: it is one this process can
    /// write by construction, which is the whole question.
    #[test]
    fn a_planted_chocolatey_is_refused() {
        if an_account_that_is_not_administrator("a planted Chocolatey").is_none() {
            return;
        }
        let dir = tempfile::tempdir().expect("a temporary directory under this user's profile");
        let planted = dir.path().join("choco.exe");
        std::fs::write(&planted, b"not really Chocolatey").expect("this process can write here");
        let program = planted.to_string_lossy().into_owned();
        let step = choco::operation_step(choco::OpKind::Install, "7zip", &program);
        let err = validate(&plan_of(vec![step]))
            .expect_err("a choco.exe this process can overwrite must not run as Administrator");
        assert_eq!(
            err,
            can_be_replaced(&program),
            "refused because it can be replaced, which is the only refusal this test means"
        );
        assert!(err.ends_with('.'), "the reason is a sentence: {err}");
        assert!(!err.contains('\u{2014}'), "no em dashes: {err}");
    }

    /// A removal the registry records is allowed.
    #[test]
    fn a_registered_removal_is_allowed() {
        let step = step_from(
            SourceKind::Arp,
            r"C:\Program Files\Thing\unins000.exe",
            &["/SILENT"],
        );
        let allowed = Allowed {
            removals: vec![step.command.clone()],
            ..Allowed::system()
        };
        assert_eq!(validate_with(&plan_of(vec![step]), &allowed), Ok(()));
    }

    /// A command the registry does not record is refused, however the step
    /// labels itself. This is the forged-plan case, and before this check
    /// existed it ran as Administrator.
    #[test]
    fn an_unregistered_removal_is_refused_even_when_labelled_arp() {
        let registered = step_from(
            SourceKind::Arp,
            r"C:\Program Files\Thing\unins000.exe",
            &["/SILENT"],
        );
        let forged = step_from(SourceKind::Arp, "powershell.exe", &["-Command", "whoami"]);
        let allowed = Allowed {
            removals: vec![registered.command],
            ..Allowed::system()
        };
        let err = validate_with(&plan_of(vec![forged]), &allowed)
            .expect_err("the registry does not record this command");
        assert_eq!(
            err,
            not_allowed("powershell.exe"),
            "the sentence this test means"
        );
        assert!(err.ends_with('.'), "the reason is a sentence: {err}");
    }

    /// Same program, different arguments, is a different command. An
    /// uninstaller that takes a path to delete must not be reachable with a
    /// path of someone else's choosing.
    #[test]
    fn a_registered_program_with_other_arguments_is_refused() {
        let registered = step_from(
            SourceKind::Arp,
            r"C:\Program Files\Thing\unins000.exe",
            &["/SILENT"],
        );
        let twisted = step_from(
            SourceKind::Arp,
            r"C:\Program Files\Thing\unins000.exe",
            &["/SILENT", r"C:\Windows"],
        );
        let allowed = Allowed {
            removals: vec![registered.command],
            ..Allowed::system()
        };
        let expected = not_allowed(&twisted.command.program);
        assert_eq!(
            validate_with(&plan_of(vec![twisted]), &allowed),
            Err(expected)
        );
    }

    /// An empty list refuses every removal. That is the safe direction to
    /// fail: a caller that has not read the registry runs nothing from it.
    #[test]
    fn no_registered_removals_means_no_removal_runs() {
        const UNINSTALLER: &str = r"C:\Program Files\Thing\unins000.exe";
        let plan = plan_of(vec![step_from(SourceKind::Arp, UNINSTALLER, &["/SILENT"])]);
        assert_eq!(
            validate(&plan),
            Err(not_allowed(UNINSTALLER)),
            "validate uses the system list, which records nothing"
        );
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
        assert_eq!(
            err,
            not_allowed("Pacman"),
            "the source is what was refused, before any command was read"
        );
    }

    /// A session step is not the helper's business at all: the runner runs
    /// it itself, and a per-user removal from `HKCU` is one, so checking it
    /// here would refuse every one of them. The helper refuses a plan that
    /// carries such a step outright, which is what keeps this filter from
    /// being a way past the list.
    #[test]
    fn a_session_step_is_not_checked_by_this_list() {
        let mut step = step_from(SourceKind::Winget, "winget.exe", &["install"]);
        step.needs_root = false;
        assert_eq!(validate(&plan_of(vec![step])), Ok(()));
    }

    /// One bad step refuses the whole plan, so a plan is never half run.
    #[test]
    fn one_bad_step_refuses_the_whole_plan() {
        const CMD: &str = r"C:\Windows\System32\cmd.exe";
        let plan = plan_of(vec![
            operation_step(OpKind::Install, "Valve.Steam", WINGET),
            step_from(SourceKind::Winget, CMD, &["/c", "whoami"]),
        ]);
        assert_eq!(validate(&plan), Err(not_allowed(CMD)));
    }

    /// The two directions of the gate, measured rather than assumed, on
    /// two paths every Windows machine has.
    ///
    /// A directory this test has just made is one this process can write by
    /// construction, so the file in it must be refused; `cmd.exe` under
    /// `%SystemRoot%\System32` is the case a standard user cannot write, so
    /// it must be admitted. Neither needs Administrator to set up and
    /// neither writes anywhere protected, which is what lets the whole rule
    /// be checked without elevating anything.
    #[test]
    fn a_file_this_process_can_write_is_refused_and_one_it_cannot_is_admitted() {
        if an_account_that_is_not_administrator("a file this process can write").is_none() {
            return;
        }
        let dir = tempfile::tempdir().expect("a temporary directory under this user's profile");
        let mine = dir.path().join("anything.exe");
        std::fs::write(&mine, b"mine").expect("this process can write here");
        let program = mine.to_string_lossy().into_owned();
        let refusal = program_this_process_cannot_replace(&program)
            .expect_err("a file this process just wrote is one it can replace");
        assert_eq!(
            refusal,
            can_be_replaced(&program),
            "refused because it can be replaced, not because it could not be checked"
        );

        let theirs = Path::new(&std::env::var("SystemRoot").expect("Windows sets SystemRoot"))
            .join("System32")
            .join("cmd.exe");
        assert!(theirs.is_file(), "every Windows has {theirs:?}");
        assert_eq!(
            program_this_process_cannot_replace(&theirs.to_string_lossy()),
            Ok(()),
            "a standard user cannot write System32"
        );
    }

    /// A directory above the program counts too. The file here is written
    /// once and then left alone, and it is the folder around it that this
    /// process owns, which is enough: a directory this account can rename
    /// or empty is a program this account can swap.
    #[test]
    fn a_directory_above_the_program_is_asked_about_as_well() {
        if an_account_that_is_not_administrator("a directory above the program").is_none() {
            return;
        }
        let dir = tempfile::tempdir().expect("a temporary directory under this user's profile");
        let below = dir.path().join("bin");
        std::fs::create_dir(&below).expect("this process can create directories here");
        let program = below.join("thing.exe").to_string_lossy().into_owned();
        std::fs::write(&program, b"mine").expect("this process can write here");
        assert_eq!(
            program_this_process_cannot_replace(&program),
            Err(can_be_replaced(&program)),
            "the folders above the file are this account's own"
        );
    }

    /// A path that is not on the disk is answered by the directory above
    /// it, and the answer is what the hole was made of. `C:\ProgramData`
    /// grants `BUILTIN\Users` the right to create a directory on a stock
    /// Windows, so a `choco.exe` under a folder of `C:\ProgramData` that
    /// does not exist yet is one an unprivileged process can put there
    /// before the prompt, and it is refused although there is nothing at
    /// the path to read.
    ///
    /// The name below is not a real product and never will be, so the test
    /// creates nothing and cleans nothing up.
    #[test]
    fn a_name_this_process_could_still_create_is_refused() {
        if an_account_that_is_not_administrator("a name this process could still create").is_none()
        {
            return;
        }
        let program = Path::new(&std::env::var("ProgramData").expect("Windows sets ProgramData"))
            .join("brokey-no-such-package-manager")
            .join("bin")
            .join("choco.exe");
        assert!(!program.exists(), "the test invents a name nothing uses");
        let program = program.to_string_lossy().into_owned();
        let refusal = program_this_process_cannot_replace(&program)
            .expect_err("any user may create a directory in C:\\ProgramData");
        assert_eq!(
            refusal,
            can_be_replaced(&program),
            "refused because the name is still this account's to take"
        );
    }

    /// The other half of the same question. A path that is not on the disk
    /// under a directory nothing unprivileged can add to is admitted: no
    /// file can appear there without Administrator, so elevating the name
    /// hands nobody anything. This is what keeps a step whose program is
    /// simply not installed from being reported as a security refusal, and
    /// it is what lets the plan tests in the sources use paths under
    /// `C:\Program Files` that no machine really has.
    #[test]
    fn a_name_nothing_unprivileged_can_create_is_admitted() {
        let program = Path::new(&std::env::var("ProgramFiles").expect("Windows sets ProgramFiles"))
            .join("Brokey No Such Application")
            .join("unins000.exe");
        assert!(!program.exists(), "the test invents a name nothing uses");
        assert_eq!(
            program_this_process_cannot_replace(&program.to_string_lossy()),
            Ok(()),
            "a standard user cannot create a directory in Program Files"
        );
    }

    /// A path the gate cannot put its question to at all is refused, not
    /// admitted. A bare name has no directory to ask about and an empty one
    /// has nothing at all, and both come back as a refusal.
    #[test]
    fn a_path_that_cannot_be_interrogated_is_refused() {
        if an_account_that_is_not_administrator("a path the gate cannot interrogate").is_none() {
            return;
        }
        for program in ["", "choco.exe"] {
            let refusal = program_this_process_cannot_replace(program)
                .expect_err("failure is a refusal here");
            assert_eq!(
                refusal,
                cannot_be_checked(program),
                "refused because the question could not be put, which is this one's meaning"
            );
            assert!(
                refusal.ends_with('.'),
                "the reason is a sentence: {refusal}"
            );
        }
    }

    /// The gate is in `check_step` rather than in one arm, so a winget step
    /// answers to it as well: the same file under a directory this test has
    /// just made is refused although its arguments are exactly the ones
    /// `operation_step` builds. A source added to the match later inherits
    /// the gate the same way.
    ///
    /// This was not a hypothetical case for winget. `winget_program` used
    /// to answer the App Execution Alias in
    /// `%LOCALAPPDATA%\Microsoft\WindowsApps`, which the invoking user owns
    /// and can replace, so the gate refused the real winget on a real
    /// machine. It now answers the executable that alias names, which that
    /// user cannot replace, and the planted file below is what keeps the
    /// rule itself under test rather than the accident that winget used to
    /// trip it.
    #[test]
    fn the_gate_covers_every_source_the_list_admits_not_only_chocolatey() {
        if an_account_that_is_not_administrator("a planted winget").is_none() {
            return;
        }
        let dir = tempfile::tempdir().expect("a temporary directory under this user's profile");
        let planted = dir.path().join("winget.exe");
        std::fs::write(&planted, b"not really winget").expect("this process can write here");
        let program = planted.to_string_lossy().into_owned();
        let step = operation_step(OpKind::Install, "Valve.Steam", &program);
        let err = validate(&plan_of(vec![step]))
            .expect_err("a winget.exe this process can overwrite is not one to elevate");
        assert_eq!(
            err,
            can_be_replaced(&program),
            "refused because it can be replaced, which is what this test means"
        );
    }

    /// A removal Windows recorded is checked the same way, so the closed
    /// list cannot be walked around by labelling a planted program as one
    /// the registry names. An uninstaller this process can overwrite is
    /// refused even when its whole command is on the list.
    #[test]
    fn a_registered_removal_this_process_can_replace_is_refused() {
        if an_account_that_is_not_administrator("a registered removal this process can replace")
            .is_none()
        {
            return;
        }
        let dir = tempfile::tempdir().expect("a temporary directory under this user's profile");
        let uninstaller = dir.path().join("unins000.exe");
        std::fs::write(&uninstaller, b"mine").expect("this process can write here");
        let program = uninstaller.to_string_lossy().into_owned();
        let step = step_from(SourceKind::Arp, &program, &["/SILENT"]);
        let allowed = Allowed {
            removals: vec![step.command.clone()],
            ..Allowed::system()
        };
        let err = validate_with(&plan_of(vec![step]), &allowed)
            .expect_err("the registry recording it does not make it safe to elevate");
        assert_eq!(
            err,
            can_be_replaced(&program),
            "refused because it can be replaced, not because the list does not name it"
        );
    }

    /// Both of the gate's sentences follow the copy rules and neither
    /// carries any part of an access control list into the interface: a
    /// reader is told which program and what to do, and nothing that would
    /// tell somebody else where the machine is soft.
    #[test]
    fn the_gates_refusals_say_what_to_do_and_leak_no_permissions() {
        if an_account_that_is_not_administrator("the gate's two refusal sentences").is_none() {
            return;
        }
        let dir = tempfile::tempdir().expect("a temporary directory under this user's profile");
        let mine = dir.path().join("thing.exe").to_string_lossy().into_owned();
        std::fs::write(&mine, b"mine").expect("this process can write here");
        let replaceable =
            program_this_process_cannot_replace(&mine).expect_err("a file this process wrote");
        assert_eq!(replaceable, can_be_replaced(&mine), "the first sentence");
        let unreadable =
            program_this_process_cannot_replace("").expect_err("a path with nothing in it");
        assert_eq!(unreadable, cannot_be_checked(""), "the second sentence");
        for sentence in [replaceable, unreadable] {
            assert!(sentence.ends_with('.'), "a sentence: {sentence}");
            assert!(!sentence.contains('\u{2014}'), "no em dashes: {sentence}");
            assert!(
                sentence.contains("try again"),
                "it says what to do: {sentence}"
            );
            for leaked in ["BUILTIN", "S-1-5", "FILE_", "ACL", "DACL", "Allow "] {
                assert!(
                    !sentence.contains(leaked),
                    "no permissions in the interface: {sentence}"
                );
            }
        }
    }

    /// The case the fallback exists for, put to the two functions directly
    /// rather than through a step, so that a later change which quietly
    /// stopped reaching the probe would be caught here.
    ///
    /// `C:\Program Files\WindowsApps` grants this account no `READ_CONTROL`,
    /// so its descriptor cannot be read and `descriptor_of` says so with
    /// `ERROR_ACCESS_DENIED`; asked the same question about the same
    /// directory, [`any_right_granted_on`] opens it for each right instead
    /// and answers that none of them is granted. Skipped where that
    /// directory is not on the machine. It assumes the suite is run
    /// unelevated, which is a rule of this arm anyway: an administrator can
    /// read that descriptor, and then there is nothing to fall back from.
    #[test]
    fn a_descriptor_that_cannot_be_read_is_asked_of_the_object_instead() {
        let windows_apps =
            Path::new(&std::env::var("ProgramFiles").expect("Windows sets ProgramFiles"))
                .join("WindowsApps");
        if !windows_apps.exists() {
            eprintln!("skipped: this machine has no {}.", windows_apps.display());
            return;
        }
        if descriptor_of(&windows_apps).is_ok() {
            // Measured on CI run 35532208191, where this answered `None`
            // against the `Some(5)` this machine gives: an administrator may
            // read that descriptor, and then there is nothing to fall back
            // from. The test is worth keeping for every ordinary user's
            // machine, where the premise holds.
            eprintln!(
                "skipped: this account may read the security of {}, so the fallback to the \
                 object was not tested.",
                windows_apps.display()
            );
            return;
        }
        assert_eq!(
            descriptor_of(&windows_apps).err(),
            Some(ERROR_ACCESS_DENIED),
            "a standard user may traverse {} and may not read its security",
            windows_apps.display()
        );
        let Some(token) = an_account_that_is_not_administrator("the fallback to the object itself")
        else {
            return;
        };
        assert_eq!(
            any_right_granted_on(&windows_apps, &token, &REPLACE_A_DIRECTORY),
            Ok(false),
            "the probe answers the directory the descriptor could not"
        );
    }

    /// The probe discriminates rather than always saying no, which is the
    /// failure that would look exactly like the gate working. A directory
    /// this process has just made is one it can delete, and the probe is
    /// called directly so that the answer cannot come from the descriptor
    /// road by accident: this one's descriptor reads perfectly well, so
    /// through [`any_right_granted_on`] it would never reach the probe at
    /// all.
    ///
    /// It is also the test that proves the token carries
    /// `TOKEN_IMPERSONATE`. Without it `ImpersonateLoggedOnUser` fails, the
    /// probe answers `Err`, and every path whose descriptor cannot be read
    /// is refused again, which is the outcome this change exists to stop.
    #[test]
    fn the_probe_says_yes_to_a_directory_this_account_can_replace() {
        let Some(token) = an_account_that_is_not_administrator("the probe's two answers") else {
            return;
        };
        let dir = tempfile::tempdir().expect("a temporary directory under this user's profile");
        assert_eq!(
            any_right_opened_on(dir.path(), &token, &REPLACE_A_DIRECTORY),
            Ok(true),
            "a directory this process made is one it can delete"
        );
        assert!(
            dir.path().is_dir(),
            "the probe opens and closes handles and changes nothing"
        );
    }

    /// A file somebody else holds open is answered, not refused for a
    /// reason that has nothing to do with permissions.
    ///
    /// The share mode this probe passes declares what it permits others; it
    /// does not exempt it from the share mode an existing opener declared,
    /// so a file held open without `FILE_SHARE_WRITE` answers
    /// `ERROR_SHARING_VIOLATION` to an open for `FILE_WRITE_DATA`. Windows
    /// evaluates the DACL first, so that error is returned only after the
    /// access was granted, and the right is held. The file below is one
    /// this process wrote, so `Ok(true)` is also the right answer on the
    /// merits, which is what makes the two roads to it comparable: the same
    /// file with nobody holding it answers `Ok(true)` through a plain
    /// successful open.
    #[test]
    fn a_file_another_handle_holds_open_is_still_answered() {
        use std::os::windows::fs::OpenOptionsExt;

        let Some(token) = an_account_that_is_not_administrator("a file held open elsewhere") else {
            return;
        };
        let dir = tempfile::tempdir().expect("a temporary directory under this user's profile");
        let program = dir.path().join("held.exe");
        std::fs::write(&program, b"mine").expect("this process can write here");
        assert_eq!(
            any_right_opened_on(&program, &token, &REPLACE_A_FILE),
            Ok(true),
            "a file this process wrote, with nobody holding it"
        );

        let held = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(FILE_SHARE_READ)
            .open(&program)
            .expect("this process can open the file it just wrote");
        assert_eq!(
            any_right_opened_on(&program, &token, &REPLACE_A_FILE),
            Ok(true),
            "a sharing violation is not a denial"
        );
        drop(held);
    }

    /// The end-to-end question this whole arm was built to answer: a winget
    /// step naming the winget this machine really runs is admitted. A
    /// live-machine test, skipped with a reason where winget is not
    /// installed rather than failed.
    ///
    /// It took three changes to get here, and the two refusals it passed
    /// through are why the assertion is `Ok(())` and not something softer.
    /// `winget_program` used to answer the alias in
    /// `%LOCALAPPDATA%\Microsoft\WindowsApps`, a zero-length reparse point
    /// in a folder the invoking user has full control of, and the gate
    /// refused it with [`can_be_replaced`]. Resolving the alias moved the
    /// refusal rather than ending it: `GetNamedSecurityInfoW` on
    /// `C:\Program Files\WindowsApps` fails with `ERROR_ACCESS_DENIED`,
    /// because reading a descriptor needs `READ_CONTROL` and that directory
    /// grants this account none, so the step was refused by
    /// [`cannot_be_checked`] instead. The gate now asks the object when it
    /// cannot read the descriptor, and every element of the resolved path
    /// answers that this account cannot replace it. Measured unelevated on
    /// the development machine: that one directory is the only element
    /// whose descriptor `Get-Acl` cannot read, the four others answer
    /// through `AccessCheck` as before, and `CreateFileW` for
    /// `FILE_ADD_FILE`, `FILE_DELETE_CHILD`, `DELETE`, `WRITE_DAC` and
    /// `WRITE_OWNER` is refused with error 5 on `C:\`, on
    /// `C:\Program Files` and on `C:\Program Files\WindowsApps`, while the
    /// alias folder in `%LOCALAPPDATA%` grants all five, which is what says
    /// the instrument discriminates rather than always saying no.
    ///
    /// A failure here on a machine where winget is installed is not a flaky
    /// test. It means either that the resolution stopped working or that
    /// one element of that path is one this account can write, and the
    /// second would be worth knowing about.
    #[test]
    fn the_resolved_winget_is_not_a_program_this_account_can_replace() {
        use crate::sources::windows::winget::{winget_exe, winget_program};

        let Some(found) = winget_exe() else {
            eprintln!("skipped: winget is not on this machine, so there is nothing to resolve");
            return;
        };
        let program = winget_program();
        assert_ne!(
            Path::new(&program),
            found,
            "the alias at {} is not the program that runs",
            found.display()
        );
        assert!(
            Path::new(&program).is_file(),
            "{program} is what was resolved, so it is on the disk"
        );

        let step = operation_step(OpKind::Install, "Valve.Steam", &program);
        assert_eq!(
            check_step(&step, &Allowed::system()),
            Ok(()),
            "the winget this machine really runs is one only an administrator can replace"
        );
        eprintln!("admitted: {program}");
    }
}
