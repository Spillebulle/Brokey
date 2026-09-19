//! Operations to steps. Each source says how its own operations are carried
//! out; this module expands setups, puts the steps in order and joins
//! adjacent package-manager calls into one (`pacman -S a`, `pacman -S b`
//! becomes `pacman -S a b`), which is both one password and one dependency
//! resolution instead of two.
//!
//! The order is refresh steps, then every other step in the order of the
//! operations that produced it, and within one operation the order its
//! source gave. Nothing is hoisted across operations: a setup chain depends
//! on its order (on Arch snapd is built from the AUR as the user before the
//! root step that starts its socket, and `flatpak install` runs after the
//! root steps that install flatpak and add Flathub), and so does a source
//! whose steps alternate (download as the user, then `pacman -U` as root).
//! The runner starts one helper run per stretch of consecutive root steps,
//! and polkit's `auth_admin_keep` keeps that to one password in practice.

use crate::model::*;
use crate::{Result, Store};

/// The sentence the Updates page shows beside a pacman update that is not
/// "Update all". A pacman install or update is `pacman -Syu` with names, so
/// ticking one package brings every package up to date; the sentence says
/// so rather than pretending the subset is what runs.
pub const PARTIAL_UPGRADE_NOTICE: &str = "Arch does not support partial upgrades, so updating any pacman package updates every pacman package. Update all is what runs.";

/// Build the plan for `ops`: expand each setup, ask each source for its
/// steps, put refresh steps first, batch what is adjacent and can be
/// batched.
pub fn build(store: &Store, ops: &[Op]) -> Result<Plan> {
    let mut gatherer = Gatherer {
        store,
        expanded: Vec::new(),
        gathered: Vec::new(),
        set_up: Vec::new(),
    };
    for op in ops {
        gatherer.gather(op)?;
    }
    let Gatherer {
        expanded, gathered, ..
    } = gatherer;
    let steps = batch(order(gathered), &expanded);
    Ok(Plan {
        id: new_id(),
        ops: ops.to_vec(),
        steps,
    })
}

/// The sentence for a setup the source cannot give on this machine.
pub fn cannot_set_up(kind: SourceKind) -> String {
    format!(
        "{} cannot be set up on this system by Brokey.",
        kind.label()
    )
}

/// Walks the operations, expanding each `Setup` into the operations and
/// steps its source gives. `expanded` is every operation a step came from,
/// setup operations included, so a batched title can say what the batch
/// does.
struct Gatherer<'a> {
    store: &'a Store,
    expanded: Vec<Op>,
    gathered: Vec<Gathered>,
    /// Sources already set up in this plan: a second `Setup` for one, or
    /// a setup chain that comes back to it, adds nothing.
    set_up: Vec<SourceKind>,
}

impl Gatherer<'_> {
    fn gather(&mut self, op: &Op) -> Result<()> {
        let kind = op.source();
        let Some(source) = self.store.source(kind) else {
            return Err(crate::Error::new(format!(
                "{} is not a source on this machine.",
                kind.label()
            )));
        };
        let index = self.expanded.len();
        self.expanded.push(op.clone());
        if let Op::Setup { .. } = op {
            if self.set_up.contains(&kind) {
                return Ok(());
            }
            self.set_up.push(kind);
            let Some(setup) = source.setup() else {
                return Err(crate::Error::from_source(kind, cannot_set_up(kind)));
            };
            for inner in &setup.ops {
                self.gather(inner)?;
            }
            for step in setup.steps {
                self.gathered.push(Gathered {
                    step,
                    op: index,
                    refresh: false,
                });
            }
            return Ok(());
        }
        for step in source.plan(op)? {
            self.gathered.push(Gathered {
                step,
                op: index,
                refresh: matches!(op, Op::Refresh { .. }),
            });
        }
        Ok(())
    }
}

/// What the page should say beside a plan before it runs.
///
/// A setup says what setting its source up does ("Flatpak is not
/// installed. It is installed and Flathub is added."), and when the plan
/// then installs something from that source, the two are one sentence
/// ("..., then GNU Image Manipulation Program is installed from it.").
///
/// A pacman update that leaves other pending pacman updates out is a
/// partial upgrade, which Arch does not support, so what runs is an update
/// of everything. That sentence is shown whenever the plan updates some
/// pacman packages but not all of them, when the pending list cannot be
/// read (because then nobody can say the update is complete), and whenever
/// a pacman refresh is planned beside a pacman install or update without
/// an "Update all", because a refresh followed by an install is that same
/// partial upgrade in two steps.
///
/// A setup that installs Flatpak or snapd also says that the launcher lists
/// the new applications only after the user logs out and back in, unless the
/// running session already reads that format's export directory (see
/// [`crate::launch`]).
pub fn notices(store: &Store, ops: &[Op]) -> Vec<String> {
    #[cfg(unix)]
    {
        notices_in(store, ops, &crate::launch::this_session())
    }
    #[cfg(windows)]
    {
        // No format here yet lists its applications by desktop entry, so
        // there is no session to read. `log_out_notice` answers `None`
        // itself; the empty session is never inspected.
        notices_in(store, ops, &[])
    }
}

/// [`notices`], for a session reading desktop entries from `session`.
pub fn notices_in(store: &Store, ops: &[Op], session: &[std::path::PathBuf]) -> Vec<String> {
    let mut notices = setup_notices(store, ops, session);
    if let Some(partial) = partial_upgrade_notice(store, ops) {
        notices.push(partial);
    }
    notices
}

/// The sentence after a setup that installs a format's tool, when the
/// session cannot yet list that format's applications.
pub fn log_out_notice(kind: SourceKind, session: &[std::path::PathBuf]) -> Option<String> {
    #[cfg(unix)]
    {
        let (label, dirs) = match kind {
            SourceKind::Flatpak => ("Flatpak", crate::sources::linux::flatpak::export_dirs()),
            SourceKind::Snap => (
                "Snap",
                vec![std::path::PathBuf::from(
                    crate::sources::linux::snap::SNAP_DESKTOP_DIR,
                )],
            ),
            _ => return None,
        };
        if dirs.iter().any(|d| crate::launch::sees(session, d)) {
            return None;
        }
        Some(format!(
            "Your launcher lists {label} applications only after you log out and back in once. Until then, open them from Brokey."
        ))
    }
    #[cfg(windows)]
    {
        // Neither format exists on Windows yet, so there is nothing to
        // catch up on after a log out.
        let _ = (kind, session);
        None
    }
}

fn setup_notices(store: &Store, ops: &[Op], session: &[std::path::PathBuf]) -> Vec<String> {
    let mut notices: Vec<String> = Vec::new();
    let mut said: Vec<SourceKind> = Vec::new();
    for (i, op) in ops.iter().enumerate() {
        let Op::Setup { source: kind } = op else {
            continue;
        };
        if said.contains(kind) {
            continue;
        }
        said.push(*kind);
        let Some(setup) = store.source(*kind).and_then(|s| s.setup()) else {
            continue;
        };
        let then = ops[i + 1..].iter().find_map(|later| match later {
            Op::Install { package } if package.source == *kind => Some(package),
            _ => None,
        });
        let sentence = match then {
            Some(package) => combined_notice(&setup.notice, &display_name(store, package)),
            None => setup.notice.clone(),
        };
        notices.push(sentence);
        // Only a setup that installs the tool changes what the session can
        // see; adding a remote or starting a service does not.
        if !setup.ops.is_empty()
            && let Some(log_out) = log_out_notice(*kind, session)
        {
            notices.push(log_out);
        }
    }
    notices
}

/// "Flatpak is not installed. It is installed and Flathub is added." and a
/// name become "Flatpak is not installed. It is installed and Flathub is
/// added first, then GIMP is installed from it."
pub fn combined_notice(setup_notice: &str, name: &str) -> String {
    format!(
        "{} first, then {name} is installed from it.",
        setup_notice.trim_end().trim_end_matches('.')
    )
}

/// The name the notice gives a package: its source's record where the
/// source can give one, else the id's last segment.
fn display_name(store: &Store, package: &PackageRef) -> String {
    store
        .source(package.source)
        .and_then(|s| s.details(&package.id).ok())
        .map(|p| p.name)
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| {
            let id = package.id.trim_end_matches('/');
            let parts: Vec<&str> = id.split('/').collect();
            if package.source == SourceKind::Flatpak && parts.len() >= 4 {
                // A Flatpak ref ends in the application id, arch and
                // branch; the id's last part is the name Flatpak's own
                // messages use ("GIMP" for org.gimp.GIMP).
                let app_id = parts[parts.len() - 3];
                app_id.rsplit('.').next().unwrap_or(app_id).to_string()
            } else {
                parts.last().copied().unwrap_or(id).to_string()
            }
        })
}

fn partial_upgrade_notice(store: &Store, ops: &[Op]) -> Option<String> {
    let is_pacman = |package: &PackageRef| package.source == SourceKind::Pacman;
    let updates_all = ops.iter().any(|op| {
        matches!(
            op,
            Op::UpdateAll {
                source: SourceKind::Pacman
            }
        )
    });
    let refreshes = ops.iter().any(|op| {
        matches!(
            op,
            Op::Refresh {
                source: SourceKind::Pacman
            }
        )
    });
    let installs = ops
        .iter()
        .any(|op| matches!(op, Op::Install { package } if is_pacman(package)));
    let chosen: Vec<&str> = ops
        .iter()
        .filter_map(|op| match op {
            Op::Update { package } if is_pacman(package) => Some(package.id.as_str()),
            _ => None,
        })
        .collect();
    if updates_all {
        return None;
    }
    if refreshes && (installs || !chosen.is_empty()) {
        return Some(PARTIAL_UPGRADE_NOTICE.to_string());
    }
    if !chosen.is_empty() {
        let pending = store
            .source(SourceKind::Pacman)
            .and_then(|s| s.updates().ok());
        let complete = pending.as_ref().is_some_and(|p| {
            !p.is_empty() && p.iter().all(|u| chosen.contains(&u.package.id.as_str()))
        });
        if !complete {
            return Some(PARTIAL_UPGRADE_NOTICE.to_string());
        }
    }
    None
}

pub fn new_id() -> String {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("plan-{t}")
}

struct Gathered {
    step: Step,
    /// Index into the expanded ops, so a batched title can say what the
    /// batch does.
    op: usize,
    refresh: bool,
}

/// Refresh steps first, then the rest, each part in the order it was
/// gathered. That order is the order of the operations and, within one,
/// the order its source gave; nothing else is moved.
fn order(gathered: Vec<Gathered>) -> Vec<Gathered> {
    let (mut refresh, rest): (Vec<Gathered>, Vec<Gathered>) =
        gathered.into_iter().partition(|g| g.refresh);
    refresh.extend(rest);
    refresh
}

/// One batchable shape: `program verb` where the arguments after the verb are
/// options, then `head_positionals` fixed arguments, then package names.
/// Two consecutive steps with the same program, verb, options, fixed
/// arguments, environment, working directory and privilege become one step
/// with the names joined.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BatchRule {
    pub program: &'static str,
    pub verb: &'static str,
    /// Positional arguments that are part of the head rather than names:
    /// the remote in `flatpak install flathub a b`.
    pub head_positionals: usize,
}

/// Every command shape the planner may batch. A program not in this table is
/// never joined with another step, whatever its arguments. A pacman verb is
/// listed here only if `allow.rs` accepts it, so a batch the planner builds
/// is never one the helper refuses after the password prompt.
pub const BATCHABLE: &[BatchRule] = &[
    BatchRule {
        program: "pacman",
        verb: "-S",
        head_positionals: 0,
    },
    BatchRule {
        program: "pacman",
        verb: "-Syu",
        head_positionals: 0,
    },
    BatchRule {
        program: "pacman",
        verb: "-Rs",
        head_positionals: 0,
    },
    BatchRule {
        program: "apt-get",
        verb: "install",
        head_positionals: 0,
    },
    BatchRule {
        program: "apt-get",
        verb: "remove",
        head_positionals: 0,
    },
    BatchRule {
        program: "dnf",
        verb: "install",
        head_positionals: 0,
    },
    BatchRule {
        program: "dnf",
        verb: "remove",
        head_positionals: 0,
    },
    BatchRule {
        program: "snap",
        verb: "install",
        head_positionals: 0,
    },
    BatchRule {
        program: "snap",
        verb: "remove",
        head_positionals: 0,
    },
    BatchRule {
        program: "flatpak",
        verb: "install",
        head_positionals: 1,
    },
    BatchRule {
        program: "flatpak",
        verb: "uninstall",
        head_positionals: 0,
    },
    BatchRule {
        program: "flatpak",
        verb: "update",
        head_positionals: 0,
    },
];

/// A command split into the part that must match for batching and the names
/// that are joined.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchKey {
    pub program: String,
    pub head: Vec<String>,
    pub names: Vec<String>,
}

/// How `command` would batch, or `None` when it is not a batchable shape.
/// The verb is the first argument that is not an option; options before or
/// after it are part of the head; once a name has been seen, every later
/// argument must be a name too, otherwise the command is left alone rather
/// than reordered.
pub fn batch_key(command: &Command) -> Option<BatchKey> {
    let mut head = Vec::new();
    let mut names = Vec::new();
    let mut rule: Option<&BatchRule> = None;
    let mut fixed_left = 0;
    for arg in &command.args {
        match rule {
            None => {
                if arg.starts_with('-') && command.program != "pacman" {
                    head.push(arg.clone());
                    continue;
                }
                let found = BATCHABLE
                    .iter()
                    .find(|r| r.program == command.program && r.verb == arg)?;
                rule = Some(found);
                fixed_left = found.head_positionals;
                head.push(arg.clone());
            }
            Some(_) => {
                if arg.starts_with('-') {
                    if !names.is_empty() {
                        return None;
                    }
                    head.push(arg.clone());
                } else if fixed_left > 0 {
                    fixed_left -= 1;
                    head.push(arg.clone());
                } else {
                    names.push(arg.clone());
                }
            }
        }
    }
    rule?;
    if fixed_left > 0 {
        return None;
    }
    Some(BatchKey {
        program: command.program.clone(),
        head,
        names,
    })
}

fn batch(ordered: Vec<Gathered>, ops: &[Op]) -> Vec<Step> {
    struct Pending {
        step: Step,
        key: Option<BatchKey>,
        ops: Vec<usize>,
    }
    let mut out: Vec<Pending> = Vec::new();
    for g in ordered {
        let key = batch_key(&g.step.command);
        let joins = match (out.last(), key.as_ref()) {
            (Some(last), Some(key)) => last
                .key
                .as_ref()
                .is_some_and(|last_key| joinable(&last.step, &g.step, last_key, key)),
            _ => false,
        };
        if joins {
            let last = out.last_mut().expect("checked above");
            let key = key.expect("checked above");
            let joined = last.key.as_mut().expect("checked above");
            for name in key.names {
                if !joined.names.contains(&name) {
                    joined.names.push(name);
                }
            }
            last.step.weight = last.step.weight.saturating_add(g.step.weight);
            last.ops.push(g.op);
            last.step.command.args = joined
                .head
                .iter()
                .chain(joined.names.iter())
                .cloned()
                .collect();
            if joined.names.len() > 1 {
                last.step.title = batched_title(&last.ops, ops, joined.names.len());
            }
            continue;
        }
        out.push(Pending {
            step: g.step,
            key,
            ops: vec![g.op],
        });
    }
    out.into_iter().map(|p| p.step).collect()
}

/// A command without names means "everything" (`flatpak update`,
/// `pacman -Syu`); joining it with a named one would narrow it to the names,
/// so such a step is never joined in either direction.
fn joinable(a: &Step, b: &Step, ka: &BatchKey, kb: &BatchKey) -> bool {
    a.source == b.source
        && a.needs_root == b.needs_root
        && a.command.env == b.command.env
        && a.command.cwd == b.command.cwd
        && ka.program == kb.program
        && ka.head == kb.head
        && !ka.names.is_empty()
        && !kb.names.is_empty()
}

/// "Installing 3 packages", from what the ops that fed the batch asked for.
/// Mixed batches (an install and an update through the same `pacman -S`)
/// say "Installing", which is what pacman does in both cases.
fn batched_title(op_indexes: &[usize], ops: &[Op], count: usize) -> String {
    let mut removes = 0;
    let mut updates = 0;
    let mut refreshes = 0;
    for i in op_indexes {
        match ops.get(*i) {
            Some(Op::Remove { .. }) => removes += 1,
            Some(Op::Update { .. } | Op::UpdateAll { .. }) => updates += 1,
            Some(Op::Refresh { .. }) => refreshes += 1,
            _ => {}
        }
    }
    let n = op_indexes.len();
    let verb = if removes == n {
        "Removing"
    } else if updates == n {
        "Updating"
    } else if refreshes == n {
        "Refreshing"
    } else {
        "Installing"
    };
    format!("{verb} {count} packages")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The notices for a session that already lists every format's
    /// applications, so the tests below do not depend on how the machine
    /// running them was logged in.
    #[cfg(unix)]
    fn notices(store: &Store, ops: &[Op]) -> Vec<String> {
        let mut seen: Vec<std::path::PathBuf> = crate::sources::linux::flatpak::export_dirs();
        seen.push(std::path::PathBuf::from(
            crate::sources::linux::snap::SNAP_DESKTOP_DIR,
        ));
        notices_in(store, ops, &seen)
    }

    /// Windows sources (`arp`, `winget`) have no desktop-session notion the
    /// way Flatpak and Snap do, so there is nothing for a session to have
    /// missed.
    #[cfg(windows)]
    fn notices(store: &Store, ops: &[Op]) -> Vec<String> {
        notices_in(store, ops, &[])
    }

    #[cfg(unix)]
    #[test]
    fn installing_flatpak_says_the_launcher_needs_a_new_session() {
        let blind = crate::launch::session_dirs(Some("/usr/share"), None);
        let sentence = log_out_notice(SourceKind::Flatpak, &blind)
            .expect("a session without Flatpak's exports");
        assert!(sentence.contains("log out and back in once"), "{sentence}");
        assert!(log_out_notice(SourceKind::Snap, &blind).is_some());
        assert_eq!(log_out_notice(SourceKind::Pacman, &blind), None);
        let seeing =
            crate::launch::session_dirs(Some("/var/lib/flatpak/exports/share:/usr/share"), None);
        assert_eq!(log_out_notice(SourceKind::Flatpak, &seeing), None);
    }
    use crate::{Query, Setup, Source};

    /// A source that answers `plan` from a table, so the planner is tested
    /// without pacman or the network.
    struct Fake {
        kind: SourceKind,
        steps: fn(&Op) -> Vec<Step>,
        pending: Vec<&'static str>,
        setup: fn() -> Option<Setup>,
    }

    impl Source for Fake {
        fn kind(&self) -> SourceKind {
            self.kind
        }
        fn status(&self) -> SourceStatus {
            SourceStatus {
                kind: self.kind,
                available: true,
                reason: None,
                detail: None,
                searchable: false,
                setup: None,
            }
        }
        fn search(&self, _: &Query) -> Result<Vec<Package>> {
            Ok(Vec::new())
        }
        fn installed(&self) -> Result<Vec<Package>> {
            Ok(Vec::new())
        }
        fn updates(&self) -> Result<Vec<Update>> {
            Ok(self
                .pending
                .iter()
                .map(|id| Update {
                    package: PackageRef {
                        source: self.kind,
                        id: id.to_string(),
                    },
                    name: id.to_string(),
                    kind: PackageKind::Package,
                    summary: None,
                    icon: None,
                    from: None,
                    to: "2".to_string(),
                    download_size: None,
                    published: None,
                    is_self: false,
                })
                .collect())
        }
        fn details(&self, id: &str) -> Result<Package> {
            Err(crate::Error::new(format!("{id} is not known.")))
        }
        fn plan(&self, op: &Op) -> Result<Vec<Step>> {
            Ok((self.steps)(op))
        }
        fn setup(&self) -> Option<Setup> {
            (self.setup)()
        }
    }

    fn step(source: SourceKind, title: &str, program: &str, args: &[&str], root: bool) -> Step {
        Step {
            source,
            title: title.to_string(),
            command: Command {
                program: program.to_string(),
                args: args.iter().map(|a| a.to_string()).collect(),
                env: Vec::new(),
                cwd: None,
            },
            needs_root: root,
            weight: 2,
        }
    }

    fn id_of(op: &Op) -> String {
        match op {
            Op::Install { package } | Op::Remove { package } | Op::Update { package } => {
                package.id.clone()
            }
            Op::UpdateAll { .. } => "all".to_string(),
            Op::Refresh { .. } => "refresh".to_string(),
            Op::Setup { .. } => "setup".to_string(),
        }
    }

    fn pacman_steps(op: &Op) -> Vec<Step> {
        let id = id_of(op);
        match op {
            Op::Install { .. } | Op::Update { .. } => vec![step(
                SourceKind::Pacman,
                &format!("Installing {id}"),
                "pacman",
                &["-Syu", "--noconfirm", "--needed", &id],
                true,
            )],
            Op::Remove { .. } => vec![step(
                SourceKind::Pacman,
                &format!("Removing {id}"),
                "pacman",
                &["-Rs", "--noconfirm", &id],
                true,
            )],
            Op::UpdateAll { .. } => vec![step(
                SourceKind::Pacman,
                "Updating everything",
                "pacman",
                &["-Syu", "--noconfirm"],
                true,
            )],
            // The real pacman source plans no refresh step any more (it
            // refreshes its own copy of the databases without root); the
            // fake keeps one so the refresh lane is exercised.
            Op::Refresh { .. } => vec![step(
                SourceKind::Pacman,
                "Refreshing",
                "pacman",
                &["-Sy"],
                true,
            )],
            Op::Setup { .. } => panic!("the planner never asks a source to plan a setup"),
        }
    }

    /// The shapes `sources/flatpak.rs` builds: an update of everything has
    /// no ref, an update of one has its ref.
    fn flatpak_update_steps(op: &Op) -> Vec<Step> {
        let id = id_of(op);
        let mut args = vec!["update", "-y", "--noninteractive"];
        if !matches!(op, Op::UpdateAll { .. }) {
            args.push(&id);
        }
        vec![step(
            SourceKind::Flatpak,
            &format!("Updating {id}"),
            "flatpak",
            &args,
            false,
        )]
    }

    fn flatpak_steps(op: &Op) -> Vec<Step> {
        let id = id_of(op);
        let remote = if id.ends_with("beta") {
            "flathub-beta"
        } else {
            "flathub"
        };
        vec![step(
            SourceKind::Flatpak,
            &format!("Installing {id}"),
            "flatpak",
            &["install", "--system", "-y", remote, &id],
            true,
        )]
    }

    fn aur_steps(op: &Op) -> Vec<Step> {
        let id = id_of(op);
        vec![
            step(
                SourceKind::Aur,
                "Installing build dependencies",
                "pacman",
                &["-S", "--needed", "--asdeps", "cmake"],
                true,
            ),
            step(
                SourceKind::Aur,
                &format!("Building {id}"),
                "makepkg",
                &["-si"],
                false,
            ),
        ]
    }

    fn github_steps(op: &Op) -> Vec<Step> {
        let id = id_of(op);
        vec![
            step(
                SourceKind::Github,
                &format!("Downloading {id}"),
                "curl",
                &["-o", "x.pkg.tar.zst"],
                false,
            ),
            step(
                SourceKind::Github,
                &format!("Installing {id}"),
                "pacman",
                &["-U", "/tmp/x.pkg.tar.zst"],
                true,
            ),
        ]
    }

    fn store(sources: Vec<Box<dyn Source>>) -> Store {
        Store {
            system: crate::system::from_os_release("ID=arch\n"),
            sources,
        }
    }

    fn fake(kind: SourceKind, steps: fn(&Op) -> Vec<Step>) -> Box<dyn Source> {
        Box::new(Fake {
            kind,
            steps,
            pending: Vec::new(),
            setup: || None,
        })
    }

    fn install(kind: SourceKind, id: &str) -> Op {
        Op::Install {
            package: PackageRef {
                source: kind,
                id: id.to_string(),
            },
        }
    }

    fn update(kind: SourceKind, id: &str) -> Op {
        Op::Update {
            package: PackageRef {
                source: kind,
                id: id.to_string(),
            },
        }
    }

    fn remove(kind: SourceKind, id: &str) -> Op {
        Op::Remove {
            package: PackageRef {
                source: kind,
                id: id.to_string(),
            },
        }
    }

    fn args(step: &Step) -> Vec<&str> {
        step.command.args.iter().map(String::as_str).collect()
    }

    #[test]
    fn three_pacman_installs_become_one_step() {
        let store = store(vec![fake(SourceKind::Pacman, pacman_steps)]);
        let ops = [
            install(SourceKind::Pacman, "steam"),
            install(SourceKind::Pacman, "lutris"),
            install(SourceKind::Pacman, "wine"),
        ];
        let plan = build(&store, &ops).unwrap();
        assert_eq!(plan.steps.len(), 1);
        let s = &plan.steps[0];
        assert_eq!(
            args(s),
            ["-Syu", "--noconfirm", "--needed", "steam", "lutris", "wine"]
        );
        assert_eq!(s.title, "Installing 3 packages");
        assert!(s.needs_root);
        assert_eq!(s.weight, 6, "weights add up");
        assert_eq!(plan.ops.len(), 3);
        assert!(plan.id.starts_with("plan-"));
    }

    #[test]
    fn a_single_step_keeps_its_own_title() {
        let store = store(vec![fake(SourceKind::Pacman, pacman_steps)]);
        let plan = build(&store, &[install(SourceKind::Pacman, "steam")]).unwrap();
        assert_eq!(plan.steps[0].title, "Installing steam");
    }

    #[test]
    fn installs_and_removes_do_not_mix_and_removes_say_so() {
        let store = store(vec![fake(SourceKind::Pacman, pacman_steps)]);
        let ops = [
            install(SourceKind::Pacman, "a"),
            remove(SourceKind::Pacman, "b"),
            remove(SourceKind::Pacman, "c"),
            install(SourceKind::Pacman, "d"),
        ];
        let plan = build(&store, &ops).unwrap();
        let titles: Vec<&str> = plan.steps.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(
            titles,
            ["Installing a", "Removing 2 packages", "Installing d"]
        );
        assert_eq!(args(&plan.steps[1]), ["-Rs", "--noconfirm", "b", "c"]);
    }

    #[test]
    fn updates_through_pacman_s_are_titled_updating() {
        let store = store(vec![fake(SourceKind::Pacman, pacman_steps)]);
        let ops = [
            update(SourceKind::Pacman, "a"),
            update(SourceKind::Pacman, "b"),
        ];
        let plan = build(&store, &ops).unwrap();
        assert_eq!(plan.steps[0].title, "Updating 2 packages");
    }

    #[test]
    fn a_repeated_name_is_not_listed_twice() {
        let store = store(vec![fake(SourceKind::Pacman, pacman_steps)]);
        let ops = [
            install(SourceKind::Pacman, "a"),
            install(SourceKind::Pacman, "a"),
        ];
        let plan = build(&store, &ops).unwrap();
        assert_eq!(
            args(&plan.steps[0]),
            ["-Syu", "--noconfirm", "--needed", "a"]
        );
        assert_eq!(plan.steps[0].title, "Installing a");
    }

    #[test]
    fn an_update_all_is_never_joined_with_a_named_update() {
        let store = store(vec![fake(SourceKind::Flatpak, flatpak_update_steps)]);
        for ops in [
            vec![
                Op::UpdateAll {
                    source: SourceKind::Flatpak,
                },
                update(SourceKind::Flatpak, "app/org.x/x86_64/stable"),
            ],
            vec![
                update(SourceKind::Flatpak, "app/org.x/x86_64/stable"),
                Op::UpdateAll {
                    source: SourceKind::Flatpak,
                },
            ],
        ] {
            let plan = build(&store, &ops).unwrap();
            assert_eq!(plan.steps.len(), 2, "{:?}", plan.steps);
            let everything = plan
                .steps
                .iter()
                .find(|s| s.command.args == ["update", "-y", "--noninteractive"])
                .expect("the update of everything keeps its empty tail");
            assert_eq!(everything.title, "Updating all");
        }
        let plan = build(
            &store,
            &[
                update(SourceKind::Flatpak, "app/org.x/x86_64/stable"),
                update(SourceKind::Flatpak, "app/org.y/x86_64/stable"),
            ],
        )
        .unwrap();
        assert_eq!(plan.steps.len(), 1, "named updates still join");
        assert_eq!(plan.steps[0].title, "Updating 2 packages");
    }

    #[cfg(unix)]
    #[test]
    fn the_batch_table_names_only_pacman_verbs_the_helper_allows() {
        use crate::transaction::allow::{Allowed, check_step};
        for rule in BATCHABLE.iter().filter(|r| r.program == "pacman") {
            let s = step(
                SourceKind::Pacman,
                "t",
                "pacman",
                &[rule.verb, "--noconfirm", "foo"],
                true,
            );
            assert_eq!(
                check_step(&s, &Allowed::system()),
                Ok(()),
                "pacman {} is batchable but the helper refuses it",
                rule.verb
            );
        }
    }

    #[test]
    fn flatpak_batches_within_a_remote_only() {
        let store = store(vec![fake(SourceKind::Flatpak, flatpak_steps)]);
        let ops = [
            install(SourceKind::Flatpak, "org.gimp.GIMP"),
            install(SourceKind::Flatpak, "org.inkscape.Inkscape"),
            install(SourceKind::Flatpak, "org.gimp.GIMP.beta"),
        ];
        let plan = build(&store, &ops).unwrap();
        assert_eq!(plan.steps.len(), 2);
        assert_eq!(
            args(&plan.steps[0]),
            [
                "install",
                "--system",
                "-y",
                "flathub",
                "org.gimp.GIMP",
                "org.inkscape.Inkscape"
            ]
        );
        assert_eq!(plan.steps[0].title, "Installing 2 packages");
        assert_eq!(
            args(&plan.steps[1]),
            [
                "install",
                "--system",
                "-y",
                "flathub-beta",
                "org.gimp.GIMP.beta"
            ]
        );
    }

    #[test]
    fn steps_keep_the_order_of_their_ops_and_only_adjacent_steps_batch() {
        let store = store(vec![
            fake(SourceKind::Pacman, pacman_steps),
            fake(SourceKind::Flatpak, flatpak_steps),
        ]);
        let ops = [
            install(SourceKind::Flatpak, "org.gimp.GIMP"),
            install(SourceKind::Pacman, "a"),
            install(SourceKind::Pacman, "b"),
            install(SourceKind::Flatpak, "org.inkscape.Inkscape"),
            install(SourceKind::Pacman, "c"),
        ];
        let plan = build(&store, &ops).unwrap();
        let shape: Vec<(SourceKind, Vec<&str>)> =
            plan.steps.iter().map(|s| (s.source, args(s))).collect();
        assert_eq!(
            shape,
            [
                (
                    SourceKind::Flatpak,
                    vec!["install", "--system", "-y", "flathub", "org.gimp.GIMP"]
                ),
                (
                    SourceKind::Pacman,
                    vec!["-Syu", "--noconfirm", "--needed", "a", "b"]
                ),
                (
                    SourceKind::Flatpak,
                    vec![
                        "install",
                        "--system",
                        "-y",
                        "flathub",
                        "org.inkscape.Inkscape"
                    ]
                ),
                (
                    SourceKind::Pacman,
                    vec!["-Syu", "--noconfirm", "--needed", "c"]
                ),
            ]
        );
        assert_eq!(plan.steps[1].title, "Installing 2 packages");
    }

    #[test]
    fn refresh_first_then_the_order_of_the_ops() {
        let store = store(vec![
            fake(SourceKind::Pacman, pacman_steps),
            fake(SourceKind::Aur, aur_steps),
        ]);
        let ops = [
            install(SourceKind::Aur, "paru"),
            install(SourceKind::Pacman, "a"),
            Op::Refresh {
                source: SourceKind::Pacman,
            },
        ];
        let plan = build(&store, &ops).unwrap();
        let shape: Vec<(SourceKind, &str, bool)> = plan
            .steps
            .iter()
            .map(|s| (s.source, s.command.program.as_str(), s.needs_root))
            .collect();
        assert_eq!(
            shape,
            [
                (SourceKind::Pacman, "pacman", true),
                (SourceKind::Aur, "pacman", true),
                (SourceKind::Aur, "makepkg", false),
                (SourceKind::Pacman, "pacman", true),
            ],
            "a session step is never jumped by a later op's root step"
        );
        assert_eq!(args(&plan.steps[0]), ["-Sy"]);
        assert_eq!(plan.steps[1].title, "Installing build dependencies");
    }

    #[test]
    fn a_download_then_install_source_keeps_its_order() {
        let store = store(vec![
            fake(SourceKind::Pacman, pacman_steps),
            fake(SourceKind::Github, github_steps),
        ]);
        let ops = [
            install(SourceKind::Github, "brokey"),
            install(SourceKind::Pacman, "a"),
        ];
        let plan = build(&store, &ops).unwrap();
        let shape: Vec<(&str, bool)> = plan
            .steps
            .iter()
            .map(|s| (s.command.program.as_str(), s.needs_root))
            .collect();
        assert_eq!(shape, [("curl", false), ("pacman", true), ("pacman", true)]);
        assert_eq!(
            args(&plan.steps[1]),
            ["-U", "/tmp/x.pkg.tar.zst"],
            "the -U is never batched"
        );
    }

    fn flatpak_setup() -> Option<Setup> {
        Some(Setup {
            ops: vec![install(SourceKind::Pacman, "flatpak")],
            steps: vec![step(
                SourceKind::Flatpak,
                "Adding Flathub",
                "flatpak",
                &[
                    "remote-add",
                    "--if-not-exists",
                    "--system",
                    "flathub",
                    "https://dl.flathub.org/repo/flathub.flatpakrepo",
                ],
                true,
            )],
            notice: "Flatpak is not installed. It is installed and Flathub is added.".to_string(),
        })
    }

    fn snap_setup() -> Option<Setup> {
        Some(Setup {
            ops: vec![install(SourceKind::Aur, "snapd")],
            steps: vec![
                step(
                    SourceKind::Snap,
                    "Starting snapd",
                    "systemctl",
                    &["enable", "--now", "snapd.socket"],
                    true,
                ),
                step(
                    SourceKind::Snap,
                    "Linking /snap",
                    "ln",
                    &["-sfn", "/var/lib/snapd/snap", "/snap"],
                    true,
                ),
            ],
            notice: "snapd is not installed. It is built from the AUR and started.".to_string(),
        })
    }

    fn snap_steps(op: &Op) -> Vec<Step> {
        vec![step(
            SourceKind::Snap,
            &format!("Installing {}", id_of(op)),
            "snap",
            &["install", &id_of(op)],
            true,
        )]
    }

    fn with_setup(
        kind: SourceKind,
        steps: fn(&Op) -> Vec<Step>,
        setup: fn() -> Option<Setup>,
    ) -> Box<dyn Source> {
        Box::new(Fake {
            kind,
            steps,
            pending: Vec::new(),
            setup,
        })
    }

    fn shape(plan: &Plan) -> Vec<(SourceKind, String, bool)> {
        plan.steps
            .iter()
            .map(|s| {
                (
                    s.source,
                    format!("{} {}", s.command.program, s.command.args.join(" ")),
                    s.needs_root,
                )
            })
            .collect()
    }

    fn setup(kind: SourceKind) -> Op {
        Op::Setup { source: kind }
    }

    #[test]
    fn a_flatpak_setup_installs_flatpak_then_adds_flathub_then_installs_from_it() {
        let store = store(vec![
            fake(SourceKind::Pacman, pacman_steps),
            with_setup(SourceKind::Flatpak, flatpak_steps, flatpak_setup),
        ]);
        let ops = [
            setup(SourceKind::Flatpak),
            install(SourceKind::Flatpak, "org.gimp.GIMP"),
        ];
        let plan = build(&store, &ops).unwrap();
        assert_eq!(
            shape(&plan),
            [
                (
                    SourceKind::Pacman,
                    "pacman -Syu --noconfirm --needed flatpak".to_string(),
                    true
                ),
                (
                    SourceKind::Flatpak,
                    "flatpak remote-add --if-not-exists --system flathub https://dl.flathub.org/repo/flathub.flatpakrepo".to_string(),
                    true
                ),
                (
                    SourceKind::Flatpak,
                    "flatpak install --system -y flathub org.gimp.GIMP".to_string(),
                    true
                ),
            ]
        );
        assert_eq!(plan.ops, ops, "the plan keeps the ops as asked");
        assert_eq!(plan.steps[0].title, "Installing flatpak");
    }

    #[test]
    fn a_setup_joins_an_adjacent_pacman_install() {
        let store = store(vec![
            fake(SourceKind::Pacman, pacman_steps),
            with_setup(SourceKind::Flatpak, flatpak_steps, flatpak_setup),
        ]);
        let plan = build(
            &store,
            &[
                install(SourceKind::Pacman, "gimp"),
                setup(SourceKind::Flatpak),
            ],
        )
        .unwrap();
        assert_eq!(plan.steps.len(), 2);
        assert_eq!(
            args(&plan.steps[0]),
            ["-Syu", "--noconfirm", "--needed", "gimp", "flatpak"]
        );
        assert_eq!(plan.steps[0].title, "Installing 2 packages");
    }

    #[test]
    fn a_snap_setup_on_arch_builds_snapd_before_starting_it() {
        let store = store(vec![
            fake(SourceKind::Pacman, pacman_steps),
            fake(SourceKind::Aur, aur_steps),
            with_setup(SourceKind::Snap, snap_steps, snap_setup),
        ]);
        let ops = [setup(SourceKind::Snap), install(SourceKind::Snap, "code")];
        let plan = build(&store, &ops).unwrap();
        let programs: Vec<(String, bool)> = shape(&plan)
            .into_iter()
            .map(|(_, command, root)| (command, root))
            .collect();
        assert_eq!(
            programs,
            [
                ("pacman -S --needed --asdeps cmake".to_string(), true),
                ("makepkg -si".to_string(), false),
                ("systemctl enable --now snapd.socket".to_string(), true),
                ("ln -sfn /var/lib/snapd/snap /snap".to_string(), true),
                ("snap install code".to_string(), true),
            ]
        );
    }

    #[test]
    fn a_setup_is_expanded_once_and_a_source_without_one_is_an_error() {
        let store = store(vec![
            fake(SourceKind::Pacman, pacman_steps),
            with_setup(SourceKind::Flatpak, flatpak_steps, flatpak_setup),
            fake(SourceKind::Snap, snap_steps),
        ]);
        let plan = build(
            &store,
            &[setup(SourceKind::Flatpak), setup(SourceKind::Flatpak)],
        )
        .unwrap();
        assert_eq!(plan.steps.len(), 2, "{:?}", shape(&plan));

        let err = build(&store, &[setup(SourceKind::Snap)]).unwrap_err();
        assert_eq!(
            err.message,
            "Snap cannot be set up on this system by Brokey."
        );
        assert_eq!(
            cannot_set_up(SourceKind::Flatpak),
            "Flatpak cannot be set up on this system by Brokey."
        );
    }

    #[test]
    fn setup_notices_say_what_happens_and_join_the_install_that_follows() {
        let store = store(vec![
            fake(SourceKind::Pacman, pacman_steps),
            with_setup(SourceKind::Flatpak, flatpak_steps, flatpak_setup),
            with_setup(SourceKind::Snap, snap_steps, snap_setup),
        ]);
        assert_eq!(
            notices(&store, &[setup(SourceKind::Flatpak)]),
            ["Flatpak is not installed. It is installed and Flathub is added."]
        );
        assert_eq!(
            notices(
                &store,
                &[
                    setup(SourceKind::Flatpak),
                    install(
                        SourceKind::Flatpak,
                        "flathub/app/org.gimp.GIMP/x86_64/stable"
                    )
                ]
            ),
            [
                "Flatpak is not installed. It is installed and Flathub is added first, then GIMP is installed from it."
            ],
            "the fake has no details, so the name is the ref's application id tail"
        );
        assert_eq!(
            notices(
                &store,
                &[
                    install(SourceKind::Snap, "code"),
                    setup(SourceKind::Snap),
                    setup(SourceKind::Flatpak),
                    install(SourceKind::Snap, "vlc"),
                ]
            ),
            [
                "snapd is not installed. It is built from the AUR and started first, then vlc is installed from it.",
                "Flatpak is not installed. It is installed and Flathub is added.",
            ],
            "only an install after the setup joins it, and each setup is said once"
        );
        assert_eq!(
            combined_notice(
                "Flatpak is not installed. It is installed and Flathub is added.",
                "GNU Image Manipulation Program"
            ),
            "Flatpak is not installed. It is installed and Flathub is added first, then GNU Image Manipulation Program is installed from it."
        );
    }

    #[test]
    fn different_environments_do_not_batch() {
        fn steps(op: &Op) -> Vec<Step> {
            let id = id_of(op);
            let mut s = step(
                SourceKind::Apt,
                &format!("Installing {id}"),
                "apt-get",
                &["install", "-y", &id],
                true,
            );
            if id == "b" {
                s.command
                    .env
                    .push(("DEBIAN_FRONTEND".to_string(), "noninteractive".to_string()));
            }
            vec![s]
        }
        let store = store(vec![fake(SourceKind::Apt, steps)]);
        let ops = [
            install(SourceKind::Apt, "a"),
            install(SourceKind::Apt, "b"),
            install(SourceKind::Apt, "c"),
        ];
        let plan = build(&store, &ops).unwrap();
        assert_eq!(plan.steps.len(), 3);
    }

    #[test]
    fn an_unknown_source_is_an_error_sentence() {
        let store = store(vec![fake(SourceKind::Pacman, pacman_steps)]);
        let err = build(&store, &[install(SourceKind::Snap, "code")]).unwrap_err();
        assert_eq!(err.message, "Snap is not a source on this machine.");
    }

    #[test]
    fn the_batch_table_reads_commands_as_expected() {
        let cmd = |program: &str, args: &[&str]| Command {
            program: program.to_string(),
            args: args.iter().map(|a| a.to_string()).collect(),
            env: Vec::new(),
            cwd: None,
        };
        let key = batch_key(&cmd("pacman", &["-S", "--noconfirm", "--needed", "a", "b"])).unwrap();
        assert_eq!(key.head, ["-S", "--noconfirm", "--needed"]);
        assert_eq!(key.names, ["a", "b"]);

        let key = batch_key(&cmd(
            "flatpak",
            &["--system", "install", "-y", "flathub", "org.x"],
        ))
        .unwrap();
        assert_eq!(key.head, ["--system", "install", "-y", "flathub"]);
        assert_eq!(key.names, ["org.x"]);

        let key = batch_key(&cmd("apt-get", &["-y", "install", "a"])).unwrap();
        assert_eq!(key.head, ["-y", "install"]);

        assert_eq!(
            batch_key(&cmd("pacman", &["-U", "/x.pkg.tar.zst"])),
            None,
            "-U is not in the table"
        );
        assert_eq!(
            batch_key(&cmd("pacman", &["-S", "a", "--needed"])),
            None,
            "an option after a name is left alone"
        );
        assert_eq!(
            batch_key(&cmd("flatpak", &["install", "-y"])),
            None,
            "no remote, nothing to key on"
        );
        assert_eq!(batch_key(&cmd("makepkg", &["-si"])), None);
        assert_eq!(
            batch_key(&cmd("pacman", &["--noconfirm", "-S", "a"])),
            None,
            "pacman's verb comes first"
        );
        assert!(
            BATCHABLE
                .iter()
                .all(|r| !r.program.is_empty() && !r.verb.is_empty())
        );
    }

    #[test]
    fn a_partial_pacman_update_carries_the_notice() {
        let pacman = Box::new(Fake {
            kind: SourceKind::Pacman,
            steps: pacman_steps,
            pending: vec!["a", "b", "c"],
            setup: || None,
        });
        let store = store(vec![pacman]);
        assert_eq!(
            notices(&store, &[update(SourceKind::Pacman, "a")]),
            [PARTIAL_UPGRADE_NOTICE.to_string()]
        );
        assert_eq!(
            notices(
                &store,
                &[
                    update(SourceKind::Pacman, "a"),
                    update(SourceKind::Pacman, "b"),
                    update(SourceKind::Pacman, "c")
                ]
            ),
            Vec::<String>::new(),
            "every pending update ticked is not partial"
        );
        assert_eq!(
            notices(
                &store,
                &[
                    update(SourceKind::Pacman, "a"),
                    Op::UpdateAll {
                        source: SourceKind::Pacman
                    }
                ]
            ),
            Vec::<String>::new()
        );
        assert_eq!(
            notices(&store, &[install(SourceKind::Pacman, "a")]),
            Vec::<String>::new()
        );
        assert_eq!(
            notices(&store, &[update(SourceKind::Flatpak, "org.x")]),
            Vec::<String>::new()
        );
        assert!(!PARTIAL_UPGRADE_NOTICE.contains('\u{2014}'));
        assert!(PARTIAL_UPGRADE_NOTICE.ends_with('.'));
        assert!(
            PARTIAL_UPGRADE_NOTICE.contains("Update all is what runs."),
            "the notice says what happens, not what to prefer"
        );
    }

    #[test]
    fn a_pacman_refresh_beside_an_install_or_update_carries_the_notice() {
        let pacman = Box::new(Fake {
            kind: SourceKind::Pacman,
            steps: pacman_steps,
            pending: vec!["a"],
            setup: || None,
        });
        let store = store(vec![pacman]);
        let refresh = || Op::Refresh {
            source: SourceKind::Pacman,
        };
        assert_eq!(
            notices(&store, &[refresh(), install(SourceKind::Pacman, "x")]),
            [PARTIAL_UPGRADE_NOTICE.to_string()]
        );
        assert_eq!(
            notices(&store, &[update(SourceKind::Pacman, "a"), refresh()]),
            [PARTIAL_UPGRADE_NOTICE.to_string()],
            "every pending update ticked is still a refresh then an install"
        );
        assert_eq!(
            notices(
                &store,
                &[
                    refresh(),
                    install(SourceKind::Pacman, "x"),
                    update(SourceKind::Pacman, "a")
                ]
            )
            .len(),
            1,
            "one notice, not one per reason"
        );
        assert_eq!(
            notices(&store, &[refresh()]),
            Vec::<String>::new(),
            "a refresh alone installs nothing"
        );
        assert_eq!(
            notices(
                &store,
                &[
                    refresh(),
                    install(SourceKind::Pacman, "x"),
                    Op::UpdateAll {
                        source: SourceKind::Pacman
                    }
                ]
            ),
            Vec::<String>::new()
        );
        assert_eq!(
            notices(
                &store,
                &[
                    Op::Refresh {
                        source: SourceKind::Flatpak
                    },
                    install(SourceKind::Pacman, "x")
                ]
            ),
            Vec::<String>::new(),
            "another source's refresh is not pacman's"
        );
    }

    #[test]
    fn an_unreadable_pending_list_still_warns() {
        let store = store(vec![fake(SourceKind::Pacman, pacman_steps)]);
        assert_eq!(notices(&store, &[update(SourceKind::Pacman, "a")]).len(), 1);
    }
}
