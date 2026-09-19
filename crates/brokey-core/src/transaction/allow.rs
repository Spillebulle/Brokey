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

use crate::model::Plan;
#[cfg(unix)]
use crate::model::{Command, Step};
#[cfg(unix)]
use std::path::Component;
use std::path::{Path, PathBuf};

/// The directories a package file (`pacman -U`, `dpkg -i`, `rpm -U`, a `.deb`
/// given to `apt-get install`) may come from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Allowed {
    pub package_dirs: Vec<PathBuf>,
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

/// Placeholder until the Windows closed list lands in the next task.
#[cfg(windows)]
pub fn validate_with(plan: &Plan, allowed: &Allowed) -> Result<(), String> {
    let _ = (plan, allowed);
    Err("Brokey cannot run anything as Administrator yet.".to_string())
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

#[cfg(unix)]
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

/// This module is entirely about the Linux closed list; Task 3 adds a
/// Windows test module beside it.
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
