//! What a failed step's output says went wrong, and what a finished plan's
//! output says the user should know.
//!
//! Every step's exit used to reach the page as "failed with exit code 1.
//! The log has the details." That is true and useless: a held database
//! lock, a dead mirror, a conflict, an out-of-date keyring and a package
//! that no longer exists all exit 1, and each has a different remedy. The
//! runner already sees every line a step prints, so it feeds them through a
//! [`Transcript`] here, and a failure becomes the sentence for the first
//! cause it recognises, or failing that the tool's own last error line.
//!
//! The same reading of a successful plan says when a kernel or a graphics
//! driver changed (the running one is still the old one until a restart)
//! and when pacman left a `.pacnew` beside a configuration file somebody
//! edited.
//!
//! Pure: lines in, sentences out, so every case is a table in the tests.
//! pacman, makepkg and the AUR steps run with `LC_ALL=C.UTF-8`, which is
//! what makes their English output something to match on.

use regex::Regex;
use std::collections::VecDeque;
use std::sync::OnceLock;

/// How many of a step's most recent lines are kept for diagnosis. A failing
/// tool says why at the end; a build log can run to tens of thousands of
/// lines before it, and none of those are needed.
const KEPT: usize = 400;

pub const LOCKED: &str = "Another package manager is using pacman's database, so nothing was changed. \
     Close Pamac, Octopi or any terminal running pacman and try again. If none is running, \
     a crashed one left its lock behind: remove /var/lib/pacman/db.lck and try again.";

pub const MIRRORS: &str = "pacman could not download from the mirrors. Check the internet \
     connection and try again. If it keeps failing, the mirror list is out of date: refresh it \
     (cachyos-rate-mirrors on CachyOS, reflector on Arch) and try again.";

pub const KEYRING: &str = "A package's signature could not be checked against the keys on this \
     machine, which means the keyring is out of date. In a terminal, run sudo pacman -Sy \
     archlinux-keyring (adding cachyos-keyring on CachyOS) and then sudo pacman -Su, and try again.";

pub const DISK_FULL: &str =
    "There is not enough free disk space for this. Free some space and try again.";

pub const CHECKSUM: &str = "A source file did not match the checksum the package lists, so the \
     build was stopped. The AUR package is probably out of date; check its comments on the AUR, \
     and try again once its maintainer has updated it.";

pub const BUILD_FAILED: &str = "The package failed to build. That is a problem in the package or \
     its sources rather than in Brokey: the log has the build's own output, and the package's \
     comments on the AUR often already say what is wrong.";

pub const RESTART_DRIVER: &str =
    "The graphics driver changed. Restart the computer to start using it.";

/// Reads a plan's output as it streams past.
#[derive(Debug, Default)]
pub struct Transcript {
    /// The current step's last [`KEPT`] lines.
    step: VecDeque<String>,
    /// Packages a pacman transaction installed or upgraded, by name, in
    /// the order they appeared, over the whole plan.
    changed: Vec<String>,
    /// The `.pacnew` files pacman reported, over the whole plan.
    pacnew: Vec<String>,
    /// Whether a chwd step finished, which means a driver profile changed.
    driver_changed: bool,
}

impl Transcript {
    pub fn new() -> Transcript {
        Transcript::default()
    }

    /// A new step begins; what the last one printed is no longer evidence.
    pub fn begin_step(&mut self) {
        self.step.clear();
    }

    /// One line of the current step's output.
    pub fn line(&mut self, text: &str) {
        let text = text.trim_end();
        if self.step.len() == KEPT {
            self.step.pop_front();
        }
        self.step.push_back(text.to_string());
        if let Some(name) = changed_package(text) {
            self.changed.push(name.to_string());
        }
        if let Some(file) = pacnew(text) {
            self.pacnew.push(file.to_string());
        }
    }

    /// A step running `program` finished without an error.
    pub fn step_succeeded(&mut self, program: &str) {
        if program == "chwd" {
            self.driver_changed = true;
        }
    }

    /// The sentence for the current step's failure, named by `title`, if
    /// its output says why. `None` when it says nothing recognisable, and
    /// the caller keeps its own sentence.
    pub fn failure(&self, title: &str) -> Option<String> {
        let lines: Vec<&str> = self.step.iter().map(String::as_str).collect();
        diagnose(&lines).map(|why| format!("{title} failed. {why}"))
    }

    /// What the user should know once the plan has finished, in order.
    pub fn afterword(&self) -> Vec<String> {
        let mut out = Vec::new();
        let kernels: Vec<&str> = unique(
            self.changed
                .iter()
                .map(String::as_str)
                .filter(|n| is_kernel(n)),
        );
        let drivers: Vec<&str> = unique(
            self.changed
                .iter()
                .map(String::as_str)
                .filter(|n| is_driver(n)),
        );
        if !kernels.is_empty() {
            out.push(format!(
                "{} changed. The running kernel is still the old one, so restart the computer when it suits you.",
                list(&kernels)
            ));
        } else if !drivers.is_empty() {
            out.push(format!(
                "{} changed. Restart the computer to start using the new driver.",
                list(&drivers)
            ));
        } else if self.driver_changed {
            out.push(RESTART_DRIVER.to_string());
        }
        let pacnew: Vec<&str> = unique(self.pacnew.iter().map(String::as_str));
        match pacnew.as_slice() {
            [] => {}
            [one] => out.push(format!(
                "An update brought a new version of a configuration file you had changed, and saved it as {one} beside yours. Compare the two when convenient; pacdiff, from pacman-contrib, does this."
            )),
            [first, rest @ ..] => out.push(format!(
                "Updates brought new versions of {} configuration files you had changed, and saved them beside yours with .pacnew on the end, such as {first}. Compare them when convenient; pacdiff, from pacman-contrib, does this.",
                rest.len() + 1
            )),
        }
        out
    }
}

/// The cause the lines show, as a sentence, checked most specific first.
pub fn diagnose(lines: &[&str]) -> Option<String> {
    let any = |needles: &[&str]| lines.iter().any(|l| needles.iter().any(|n| l.contains(n)));

    if any(&["unable to lock database", "could not lock database"]) {
        return Some(LOCKED.to_string());
    }
    if let Some(sentence) = conflict(lines) {
        return Some(sentence);
    }
    if let Some(path) = lines.iter().find_map(|l| existing_file(l)) {
        return Some(format!(
            "{path} is already on disk and belongs to another package, or to none, and pacman does \
             not overwrite it. Move or remove that file, or the package it belongs to, and try again."
        ));
    }
    if any(&[
        "(PGP signature)",
        "is unknown trust",
        "is marginal trust",
        "required key missing from keyring",
        "invalid or corrupted package (checksum)",
    ]) || lines
        .iter()
        .any(|l| l.contains("signature from") && l.contains("is invalid"))
    {
        return Some(KEYRING.to_string());
    }
    if let Some(key) = lines.iter().find_map(|l| unknown_public_key(l)) {
        return Some(format!(
            "The source is signed with key {key}, which is not in your keyring, so its signature could \
             not be checked. Import it with gpg --recv-keys {key} and try again."
        ));
    }
    if any(&["not enough free disk space", "too full:"]) {
        return Some(DISK_FULL.to_string());
    }
    let missing: Vec<&str> = lines.iter().filter_map(|l| target_not_found(l)).collect();
    if !missing.is_empty() {
        let names = list(&unique(missing.into_iter()));
        return Some(format!(
            "{names} is not in any repository this machine uses. It may have been renamed or removed \
             since the last search; search again and try once more."
        ));
    }
    if any(&[
        "failed to synchronize all databases",
        "failed retrieving file",
        "failed to retrieve some files",
        "Failure while downloading",
        "Could not resolve host",
        "fatal: unable to access",
    ]) {
        return Some(MIRRORS.to_string());
    }
    if let Some(sentence) = missing_dependencies(lines) {
        return Some(sentence);
    }
    if let Some(dep) = lines.iter().find_map(|l| unsatisfied(l)) {
        return Some(format!(
            "pacman could not install a dependency: {dep}. This usually clears once the repositories \
             have caught up with each other, so try again later."
        ));
    }
    if any(&[
        "did not pass the validity check",
        "Integrity checks are missing",
    ]) {
        return Some(CHECKSUM.to_string());
    }
    if any(&[
        "A failure occurred in build()",
        "A failure occurred in package",
    ]) || any(&[
        "A failure occurred in prepare()",
        "A failure occurred in check()",
    ]) {
        return Some(BUILD_FAILED.to_string());
    }
    last_error(lines).map(|e| format!("The last error was: {}", with_full_stop(e)))
}

/// ":: a and b are in conflict. Remove b? [y/N]", which `--noconfirm`
/// answers no to, and rightly: removing an installed package the user did
/// not name is not something to do without asking. So the sentence says
/// which one to remove.
fn conflict(lines: &[&str]) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"::\s+(\S+) and (\S+) are in conflict(?: \([^)]*\))?\. Remove (\S+)\?")
            .expect("a valid pattern")
    });
    if let Some(c) = lines.iter().find_map(|l| re.captures(l)) {
        let (a, b, old) = (&c[1], &c[2], &c[3]);
        let new = if old == b { a } else { b };
        return Some(format!(
            "{new} cannot be installed while {old} is, and Brokey does not remove a package you did \
             not ask it to. Remove {old} first, then try again."
        ));
    }
    lines
        .iter()
        .any(|l| l.contains("unresolvable package conflicts detected"))
        .then(|| {
            "Two packages in this change conflict with each other, and Brokey does not remove a \
             package you did not ask it to. The log names them; remove the one you no longer want, \
             then try again."
                .to_string()
        })
}

/// "foo: /usr/bin/foo exists in filesystem" or "/usr/bin/foo exists in
/// filesystem (owned by bar)".
fn existing_file(line: &str) -> Option<&str> {
    let head = line.split(" exists in filesystem").next()?;
    if head.len() == line.len() {
        return None;
    }
    head.rsplit(' ').next().filter(|p| p.starts_with('/'))
}

/// makepkg's "unknown public key ABCD1234" (from gpg's "Can't check
/// signature: No public key" path).
fn unknown_public_key(line: &str) -> Option<&str> {
    let rest = line.split("unknown public key ").nth(1)?;
    let key = rest.split(|c: char| !c.is_ascii_hexdigit()).next()?;
    (key.len() >= 8).then_some(key)
}

fn target_not_found(line: &str) -> Option<&str> {
    line.split("target not found: ")
        .nth(1)
        .map(str::trim)
        .filter(|n| !n.is_empty())
}

/// "unable to satisfy dependency 'foo>=2' required by bar".
fn unsatisfied(line: &str) -> Option<&str> {
    line.split("unable to satisfy dependency ")
        .nth(1)
        .map(str::trim)
        .filter(|n| !n.is_empty())
}

/// makepkg's "==> Missing dependencies:" followed by "  -> name" lines.
fn missing_dependencies(lines: &[&str]) -> Option<String> {
    let start = lines
        .iter()
        .position(|l| l.contains("Missing dependencies:"))?;
    let names: Vec<&str> = lines[start + 1..]
        .iter()
        .map_while(|l| l.trim_start().strip_prefix("-> "))
        .map(str::trim)
        .collect();
    if names.is_empty() {
        return None;
    }
    Some(format!(
        "The build needs {}, which is not installed and which Brokey could not find in a repository \
         or on the AUR. Install it another way and try again.",
        list(&names)
    ))
}

/// pacman's "error: …", makepkg's "==> ERROR: …", and flatpak's and git's
/// "error: …" and "fatal: …": the last one printed, which is the cause more
/// often than the first.
fn last_error<'a>(lines: &[&'a str]) -> Option<&'a str> {
    lines.iter().rev().find_map(|l| {
        let l = l.trim();
        ["error: ", "==> ERROR: ", "fatal: ", "Error: "]
            .iter()
            .find_map(|p| l.strip_prefix(p))
            .map(str::trim)
            .filter(|rest| !rest.is_empty())
    })
}

/// "( 3/12) upgrading linux-cachyos" or "(1/1) installing foo".
fn changed_package(line: &str) -> Option<&str> {
    let rest = line.trim().strip_prefix('(')?;
    let (_, rest) = rest.split_once(") ")?;
    let mut words = rest.split_whitespace();
    let verb = words.next()?;
    let name = words.next()?;
    matches!(
        verb,
        "installing" | "upgrading" | "reinstalling" | "downgrading"
    )
    .then_some(name)
}

/// "warning: /etc/pacman.conf installed as /etc/pacman.conf.pacnew".
fn pacnew(line: &str) -> Option<&str> {
    let rest = line.split(" installed as ").nth(1)?;
    let file = rest.split_whitespace().next()?;
    file.ends_with(".pacnew").then_some(file)
}

/// A kernel package: `linux`, `linux-lts`, `linux-zen`, `linux-cachyos`,
/// `linux-cachyos-bore` and so on, but not their headers, documentation or
/// firmware, none of which needs a restart.
pub fn is_kernel(name: &str) -> bool {
    if name == "linux" {
        return true;
    }
    let Some(flavour) = name.strip_prefix("linux-") else {
        return false;
    };
    !flavour.is_empty()
        && !flavour.starts_with("firmware")
        && !flavour.starts_with("api-headers")
        && !flavour.starts_with("tools")
        && !flavour.contains("-nvidia")
        && !flavour.ends_with("-zfs")
        && !["-headers", "-docs", "-dbg"]
            .iter()
            .any(|s| flavour.ends_with(s))
}

/// A kernel module for a graphics driver, which the running kernel keeps
/// using in its old version until a restart.
pub fn is_driver(name: &str) -> bool {
    name == "nvidia"
        || name.starts_with("nvidia-") && !name.ends_with("-settings")
        || name.starts_with("linux-cachyos") && name.contains("-nvidia")
}

fn unique<'a>(items: impl Iterator<Item = &'a str>) -> Vec<&'a str> {
    let mut out: Vec<&str> = Vec::new();
    for item in items {
        if !out.contains(&item) {
            out.push(item);
        }
    }
    out
}

/// "a", "a and b", "a, b and c".
fn list(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [one] => one.to_string(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

/// makepkg ends its errors with "!"; a sentence here ends with a full stop.
fn with_full_stop(text: &str) -> String {
    let text = text.trim_end().trim_end_matches('!');
    if text.ends_with(['.', '?']) {
        text.to_string()
    } else {
        format!("{text}.")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn says(lines: &[&str]) -> String {
        diagnose(lines).expect("a diagnosis")
    }

    #[test]
    fn a_held_lock_says_so() {
        assert_eq!(
            says(&[
                ":: Synchronizing package databases...",
                "error: failed to init transaction (unable to lock database)",
                "error: could not lock database: File exists",
            ]),
            LOCKED
        );
    }

    #[test]
    fn a_conflict_names_the_package_to_remove() {
        let s = says(&[
            ":: pipewire-jack and jack2 are in conflict (jack). Remove jack2? [y/N] ",
            "error: unresolvable package conflicts detected",
            "error: failed to prepare transaction (conflicting dependencies)",
        ]);
        assert!(
            s.starts_with("pipewire-jack cannot be installed while jack2 is"),
            "{s}"
        );
        assert!(s.contains("Remove jack2 first"), "{s}");
        let s = says(&[
            ":: iptables-nft and iptables are in conflict. Remove iptables? [y/N]",
            "error: unresolvable package conflicts detected",
        ]);
        assert!(s.contains("Remove iptables first"), "{s}");
        let s = says(&["error: unresolvable package conflicts detected"]);
        assert!(s.starts_with("Two packages"), "{s}");
    }

    #[test]
    fn a_file_on_disk_is_named() {
        let s = says(&[
            "error: failed to commit transaction (conflicting files)",
            "foo: /usr/bin/foo exists in filesystem (owned by bar)",
            "Errors occurred, no packages were upgraded.",
        ]);
        assert!(s.starts_with("/usr/bin/foo is already on disk"), "{s}");
    }

    #[test]
    fn signature_trouble_is_the_keyring() {
        for line in [
            "error: cachyos-keyring: signature from \"CachyOS <admin@cachyos.org>\" is unknown trust",
            "error: foo: signature from \"Someone\" is marginal trust",
            "error: failed to commit transaction (invalid or corrupted package (PGP signature))",
            "error: foo: signature from \"Someone\" is invalid",
            "error: required key missing from keyring",
        ] {
            assert_eq!(says(&[line]), KEYRING, "{line}");
        }
    }

    #[test]
    fn makepkg_names_the_key_to_import() {
        let s = says(&[
            "    foo-1.0.tar.gz ... FAILED (unknown public key 1234ABCD5678EF90)",
            "==> ERROR: One or more PGP signatures could not be verified!",
        ]);
        assert!(s.contains("gpg --recv-keys 1234ABCD5678EF90"), "{s}");
    }

    #[test]
    fn mirrors_disk_and_missing_targets() {
        assert_eq!(
            says(&[
                "error: failed retrieving file 'foo.pkg.tar.zst' from mirror : The requested URL returned error: 404"
            ]),
            MIRRORS
        );
        assert_eq!(
            says(&["error: failed to synchronize all databases (unexpected error)"]),
            MIRRORS
        );
        assert_eq!(
            says(&["error: Partition / too full: 1234 blocks needed, 12 blocks free"]),
            DISK_FULL
        );
        let s = says(&[
            "error: target not found: foo",
            "error: target not found: bar",
        ]);
        assert!(s.starts_with("foo and bar is not in any repository"), "{s}");
    }

    #[test]
    fn makepkg_failures() {
        let s = says(&[
            "==> Missing dependencies:",
            "  -> libfoo",
            "  -> python-bar>=2",
            "==> ERROR: Could not resolve all dependencies.",
        ]);
        assert!(
            s.starts_with("The build needs libfoo and python-bar>=2"),
            "{s}"
        );
        assert_eq!(
            says(&["==> ERROR: One or more files did not pass the validity check!"]),
            CHECKSUM
        );
        assert_eq!(
            says(&[
                "==> ERROR: A failure occurred in build().",
                "    Aborting..."
            ]),
            BUILD_FAILED
        );
    }

    #[test]
    fn otherwise_the_last_error_line() {
        assert_eq!(
            says(&["error: first thing", "error: something odd happened"]),
            "The last error was: something odd happened."
        );
        assert_eq!(diagnose(&["nothing to see"]), None);
        assert_eq!(diagnose(&[]), None);
    }

    #[test]
    fn the_failure_names_the_step() {
        let mut t = Transcript::new();
        t.line("error: target not found: gone");
        assert_eq!(
            t.failure("Installing gone").as_deref(),
            Some(
                "Installing gone failed. gone is not in any repository this machine uses. It may have been renamed or removed since the last search; search again and try once more."
            )
        );
        t.begin_step();
        assert_eq!(t.failure("Next"), None);
    }

    #[test]
    fn kernels_and_drivers_ask_for_a_restart() {
        for name in [
            "linux",
            "linux-lts",
            "linux-zen",
            "linux-cachyos",
            "linux-cachyos-bore",
        ] {
            assert!(is_kernel(name), "{name}");
        }
        for name in [
            "linux-firmware",
            "linux-firmware-nvidia",
            "linux-api-headers",
            "linux-cachyos-headers",
            "linux-tools",
            "linux-cachyos-nvidia-open",
            "linux-cachyos-zfs",
            "linuxconsole",
        ] {
            assert!(!is_kernel(name), "{name}");
        }
        assert!(is_driver("nvidia-open-dkms"));
        assert!(is_driver("linux-cachyos-nvidia-open"));
        assert!(!is_driver("nvidia-settings"));

        let mut t = Transcript::new();
        t.line("( 1/12) upgrading linux-cachyos");
        t.line("( 2/12) upgrading linux-cachyos-headers");
        t.line("( 3/12) upgrading nvidia-utils");
        let after = t.afterword();
        assert_eq!(after.len(), 1);
        assert!(
            after[0].starts_with("linux-cachyos changed."),
            "{}",
            after[0]
        );

        let mut t = Transcript::new();
        t.line("(1/2) upgrading nvidia-open-dkms");
        assert!(t.afterword()[0].contains("new driver"));

        let mut t = Transcript::new();
        t.step_succeeded("chwd");
        assert_eq!(t.afterword(), vec![RESTART_DRIVER.to_string()]);

        let mut t = Transcript::new();
        t.line("(1/1) installing firefox");
        assert!(t.afterword().is_empty());
    }

    #[test]
    fn pacnew_files_are_counted() {
        let mut t = Transcript::new();
        t.line("warning: /etc/pacman.conf installed as /etc/pacman.conf.pacnew");
        let after = t.afterword();
        assert!(after[0].contains("/etc/pacman.conf.pacnew"), "{}", after[0]);
        t.line("warning: /etc/makepkg.conf installed as /etc/makepkg.conf.pacnew");
        let after = t.afterword();
        assert!(
            after[0].starts_with("Updates brought new versions of 2"),
            "{}",
            after[0]
        );
    }

    #[test]
    fn a_long_log_keeps_its_end() {
        let mut t = Transcript::new();
        t.line("==> ERROR: A failure occurred in build().");
        for i in 0..KEPT {
            t.line(&format!("noise {i}"));
        }
        assert_eq!(t.failure("x"), None);
        t.line("error: real");
        assert!(t.failure("x").unwrap().ends_with("real."));
    }
}
