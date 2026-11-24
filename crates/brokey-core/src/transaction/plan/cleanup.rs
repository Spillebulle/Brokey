//! The cleanup's part of planning: what a removal on Windows does after its
//! uninstaller (wait for the uninstall key to go, then scan), forced removal,
//! clearing a scan's ticked items, putting a holding entry back, emptying,
//! and the uninstaller that has to elevate itself.
//!
//! Everything the machine is asked here comes through [`Planning`], so the
//! planner is tested without the registry, the holding area or the closed
//! list's view of real files.

use crate::cleanup::guard::{self, Claims};
use crate::cleanup::hints::Hints;
use crate::cleanup::holding::{self, HoldItem, Listing};
use crate::cleanup::internal::{
    AWAIT_GONE, EMPTY, HOLD, HoldPayload, MSI_UNREGISTER, RESTORE, SCAN, SHELL_OPEN, ScanPayload,
    ScanRecord, Scans, is_steam_app_key,
};
use crate::cleanup::scan::{self, Candidate};
use crate::cleanup::view::{SystemView, ViewError};
use crate::model::*;
use crate::{Error, Result};

/// What planning a cleanup asks of the machine.
pub struct Planning<'a> {
    /// The scans this session has made, which a clear takes its items from.
    pub scans: &'a Scans,
    /// What the closed list says of a step that would run as Administrator.
    pub check: &'a dyn Fn(&Step) -> std::result::Result<(), String>,
    /// What a scan after removing this package would search by, read now,
    /// before its uninstaller runs. `None` where there is no scan to make.
    pub hints: &'a dyn Fn(&PackageRef) -> Option<Hints>,
    /// The holding area, both roots.
    pub holding: &'a dyn Fn() -> Listing,
    /// The machine as it is now, to leave out of a clear what has gone since
    /// the scan. `None` keeps everything.
    pub view: Option<&'a dyn SystemView>,
    /// Every available source's installed list, read now: a clear asks
    /// again who uses what, since something may have been installed
    /// while the list of leftovers was open.
    pub installed: &'a dyn Fn() -> Vec<Package>,
}

/// The title of the step that waits for the uninstaller.
pub const AWAITING: &str = "Waiting for the uninstaller to finish";
/// The title of the step that scans.
pub const SCANNING: &str = "Looking for what was left behind";

fn internal(
    source: SourceKind,
    title: String,
    program: &str,
    args: Vec<String>,
    root: bool,
) -> Step {
    Step {
        source,
        title,
        command: Command {
            program: program.to_string(),
            args,
            env: Vec::new(),
            cwd: None,
        },
        needs_root: root,
        weight: 2,
    }
}

fn json<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("the payloads are plain data")
}

/// The two steps after a removal: wait until the uninstall key is gone, then
/// look for what is left.
pub fn wait_and_scan(hints: &Hints) -> Vec<Step> {
    let source = hints.package.source;
    vec![
        internal(
            source,
            AWAITING.to_string(),
            AWAIT_GONE,
            vec![json(hints)],
            false,
        ),
        scan_step(hints, false),
    ]
}

fn scan_step(hints: &Hints, force: bool) -> Step {
    let payload = ScanPayload {
        hints: hints.clone(),
        force,
    };
    let mut step = internal(
        hints.package.source,
        SCANNING.to_string(),
        SCAN,
        vec![json(&payload)],
        false,
    );
    step.weight = 3;
    step
}

/// Forced removal: no uninstaller, one scan with the application's own
/// folder, key and registration not counted as claims.
pub fn force(package: &PackageRef, planning: &Planning) -> Result<Vec<Step>> {
    let hints = (planning.hints)(package).ok_or_else(|| {
        Error::new(format!(
            "Brokey cannot find {} on this machine any more, so there is nothing to remove by \
             force. Look in Installed to see whether it has gone.",
            package.id
        ))
    })?;
    // Steam keeps its own record of a game, which forcing the uninstall key
    // away would leave behind; Steam's library is where it is removed.
    if hints
        .uninstall_key
        .as_ref()
        .is_some_and(|(_, key)| is_steam_app_key(key))
    {
        return Err(Error::new(format!(
            "Brokey does not remove a Steam game by force. Remove {} from your Steam library.",
            hints.name
        )));
    }
    Ok(vec![scan_step(&hints, true)])
}

/// The sentence for a clear with nothing left to do.
pub const ALL_GONE: &str = "Everything ticked has already gone, so nothing was cleared.";
/// The sentence for a clear with nothing ticked.
pub const NOTHING_TICKED: &str =
    "Nothing was ticked, so nothing was cleared. Tick what to move to the holding area.";
/// The sentence for a clear asked for beside anything else.
pub const CLEAR_ALONE: &str = "Clearing leftovers is a change of its own. Clear them first, then \
                               make the other changes.";

/// The sentence the confirm step shows before an uninstaller starts itself.
pub fn shell_open_notice(name: &str) -> String {
    format!(
        "Windows will ask you to allow {name}'s uninstaller, because Brokey does not start it \
         as Administrator itself."
    )
}

/// The sentence the confirm step shows before an incomplete holding entry
/// is deleted for good.
pub fn incomplete_notice(name: &str) -> String {
    format!(
        "What was cleared from {name} is incomplete in the holding area: its record cannot be          read, or part of it is still where it was and this is the only whole copy. Deleting it          cannot be undone, and Brokey never empties it by itself."
    )
}

/// An Add/Remove removal the closed list refuses only because this account
/// could replace the uninstaller, rewritten to start it as the user so that
/// it elevates itself. Every other step, and every other refusal, is left as
/// it is: the closed list's refusal then stops the plan before any prompt.
pub fn self_elevating(step: &Step, name: &str, planning: &Planning) -> Option<Step> {
    if step.source != SourceKind::Arp || !step.needs_root {
        return None;
    }
    match (planning.check)(step) {
        Err(why) if crate::transaction::allow::is_can_be_replaced(&why) => {}
        _ => return None,
    }
    let mut args = vec![step.command.program.clone()];
    args.extend(step.command.args.iter().cloned());
    let mut open = internal(
        SourceKind::Arp,
        format!("Starting {name}'s uninstaller. Windows will ask you to allow it."),
        SHELL_OPEN,
        args,
        false,
    );
    open.weight = step.weight;
    Some(open)
}

/// A holding id no other entry has: the time and the application's name.
fn holding_id(name: &str) -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let mut slug = String::new();
    for c in name.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug: String = slug.trim_matches('-').chars().take(40).collect();
    let id = format!("{millis}-{}", slug.trim_end_matches('-'));
    let id = id.trim_end_matches('-').to_string();
    if holding::valid_id(&id) {
        id
    } else {
        millis.to_string()
    }
}

/// Whether a leftover is still where the scan found it. What has gone went
/// with something above it or with the uninstaller, and a clear that held it
/// would be refused whole before anything moved. What cannot be read is kept
/// in: the closed list then says why.
fn still_there(item: &LeftoverItem, view: &dyn SystemView) -> bool {
    let at = item.location.as_str();
    match item.kind {
        ItemKind::File | ItemKind::Folder | ItemKind::Shortcut => {
            view.exists(std::path::Path::new(at))
        }
        ItemKind::RegistryKey => match holding::parse_key(at) {
            Some((hive, path)) => !matches!(view.subkeys(hive, &path), Err(ViewError::Missing(_))),
            None => true,
        },
        ItemKind::RegistryValue => {
            let Some((key, name)) = at.split_once(" | ") else {
                return true;
            };
            match holding::parse_key(key).map(|(hive, path)| view.values(hive, &path)) {
                Some(Ok(values)) => values.iter().any(|(n, _)| n.eq_ignore_ascii_case(name)),
                Some(Err(ViewError::Missing(_))) => false,
                _ => true,
            }
        }
        ItemKind::Service => {
            let listed = view.services();
            let name = at.strip_prefix("service:").unwrap_or(at);
            listed.unreadable > 0
                || listed
                    .items
                    .iter()
                    .any(|s| s.name.eq_ignore_ascii_case(name))
        }
        ItemKind::Task => {
            let listed = view.tasks();
            let path = at.strip_prefix("task:").unwrap_or(at);
            listed.unreadable > 0
                || listed
                    .items
                    .iter()
                    .any(|t| t.path.eq_ignore_ascii_case(path))
        }
        ItemKind::FirewallRule => {
            let Some((_, id)) = holding::firewall_parts(at) else {
                return true;
            };
            let listed = view.firewall_rules();
            listed.unreadable > 0 || listed.items.iter().any(|r| r.id.eq_ignore_ascii_case(id))
        }
    }
}

fn items_word(n: usize) -> String {
    if n == 1 {
        "1 leftover".to_string()
    } else {
        format!("{n} leftovers")
    }
}

/// Move the ticked items of a scan to the holding area: the user's in one
/// step as the user, the machine's in one step as Administrator, under one
/// holding id; and, after a forced removal of a Windows Installer product,
/// that product's registration into the same entry.
///
/// Always a plan of its own, never added to a removal's: the closed list
/// checks every step before the first runs, and a removal's leftovers are
/// not known until its scan has run.
pub fn clear(scan: &str, ticked: &[String], planning: &Planning) -> Result<Vec<Step>> {
    let record = record_of(scan, planning)?;
    let Chosen {
        user,
        machine,
        left,
    } = choose(&record, ticked, planning)?;
    let name = record.findings.name.clone();
    let source = record.findings.package.source;
    let holding = holding_id(&name);
    let mut steps = Vec::new();
    for (items, root) in [(user, false), (machine, true)] {
        if items.is_empty() {
            continue;
        }
        let title = if root {
            format!(
                "Moving {} from {name} that need Administrator to the holding area",
                items_word(items.len())
            )
        } else {
            format!(
                "Moving {} from {name} to the holding area",
                items_word(items.len())
            )
        };
        let payload = HoldPayload {
            holding: holding.clone(),
            name: name.clone(),
            items,
        };
        steps.push(internal(source, title, HOLD, vec![json(&payload)], root));
    }
    if record.forced
        && let Some(code) = &record.hints.product_code
    {
        steps.push(internal(
            source,
            format!("Clearing what the Windows Installer still records for {name}"),
            MSI_UNREGISTER,
            vec![code.clone(), holding],
            true,
        ));
    }
    // An empty plan would finish and say "Nothing to do." as if it had
    // worked; the user should hear why nothing moved.
    if steps.is_empty() {
        if left.is_empty() {
            return Err(Error::new(ALL_GONE));
        }
        return Err(Error::new(format!(
            "{} Nothing was cleared.",
            left.join(" ")
        )));
    }
    Ok(steps)
}

/// What a clear leaves where it is although it was ticked, one sentence
/// each: [`clear`] says it in the confirm step through the plan's notices.
pub fn left_since_scan(scan: &str, ticked: &[String], planning: &Planning) -> Vec<String> {
    record_of(scan, planning)
        .and_then(|record| choose(&record, ticked, planning))
        .map(|chosen| chosen.left)
        .unwrap_or_default()
}

fn record_of(scan: &str, planning: &Planning) -> Result<ScanRecord> {
    planning
        .scans
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(scan)
        .cloned()
        .ok_or_else(|| {
            Error::new(
                "Brokey no longer has that list of leftovers, because it has been closed since \
                 the scan. Nothing was cleared.",
            )
        })
}

/// The ticked items a clear moves, by side, and a sentence for each it
/// leaves because something uses it now.
struct Chosen {
    user: Vec<HoldItem>,
    machine: Vec<HoldItem>,
    left: Vec<String>,
}

/// The machine as it is at clear time: who uses what, read again from a
/// fresh installed list the way the scan reads it, and what a scan would
/// find now, for what each item points at.
struct Now<'a> {
    claims: Claims,
    found: Vec<Candidate>,
    view: &'a dyn SystemView,
}

impl Now<'_> {
    /// Why an item still there may no longer be moved, if it may not.
    fn why_left(&self, item: &LeftoverItem, name: &str) -> Option<String> {
        let found = self
            .found
            .iter()
            .find(|c| c.location.eq_ignore_ascii_case(&item.location));
        // A rule is offered only while its program is in the application's
        // folder; a scan now that does not offer it says it no longer is.
        if item.kind == ItemKind::FirewallRule && found.is_none() {
            let listed = self.view.firewall_rules();
            let id = holding::firewall_parts(&item.location).map(|(_, id)| id);
            if listed.unreadable == 0
                && listed
                    .items
                    .iter()
                    .any(|r| id.is_some_and(|id| r.id.eq_ignore_ascii_case(id)))
            {
                return Some(format!(
                    "{} was left where it is, because the program it lets through is no longer \
                     in {name}'s folder.",
                    item.location
                ));
            }
        }
        let candidate = Candidate {
            kind: item.kind,
            location: item.location.clone(),
            size: None,
            confidence: item.confidence,
            scope: item.scope,
            reason: item.reason.clone(),
            points_to: found.and_then(|c| c.points_to.clone()),
            holds_only: None,
        };
        self.claims.user_of(&candidate).map(|user| {
            format!(
                "{} was left where it is, because {user} uses it now.",
                item.location
            )
        })
    }
}

fn choose(record: &ScanRecord, ticked: &[String], planning: &Planning) -> Result<Chosen> {
    if ticked.is_empty() {
        return Err(Error::new(NOTHING_TICKED));
    }
    let name = record.findings.name.as_str();
    let now = planning.view.map(|view| Now {
        claims: guard::claims(
            view,
            &(planning.installed)(),
            record.forced.then_some(&record.hints),
        ),
        found: scan::scan(&record.hints, view).candidates,
        view,
    });
    let mut chosen = Chosen {
        user: Vec::new(),
        machine: Vec::new(),
        left: Vec::new(),
    };
    for id in ticked {
        let item = record
            .findings
            .items
            .iter()
            .find(|i| i.id == *id)
            .ok_or_else(|| {
                Error::new(format!(
                    "{id} is not in the list of leftovers from {name}. Open the list again and \
                     choose what to clear."
                ))
            })?;
        if let Some(now) = &now {
            if !still_there(item, now.view) {
                continue;
            }
            if let Some(why) = now.why_left(item, name) {
                chosen.left.push(why);
                continue;
            }
        }
        let held = HoldItem {
            kind: item.kind,
            location: item.location.clone(),
        };
        match item.scope {
            Scope::User => chosen.user.push(held),
            Scope::Machine => chosen.machine.push(held),
        }
    }
    Ok(chosen)
}

/// Put a holding entry back: as the user for what was the user's, as
/// Administrator only when the entry has machine items.
pub fn restore(id: &str, planning: &Planning) -> Result<Vec<Step>> {
    let listing = (planning.holding)();
    let found: Vec<&HoldingEntry> = listing.entries.iter().filter(|e| e.id == id).collect();
    if found.is_empty() {
        return Err(Error::new(format!(
            "There is nothing in the holding area called {id}. It may have been emptied already."
        )));
    }
    Ok(found
        .iter()
        .map(|e| {
            internal(
                SourceKind::Arp,
                format!("Putting back what was cleared from {}", e.name),
                RESTORE,
                vec![id.to_string()],
                e.scope_machine,
            )
        })
        .collect())
}

/// Delete one entry for good, or every entry. As Administrator only where
/// the machine's holding area has something to delete.
pub fn empty(id: Option<&str>, planning: &Planning) -> Result<Vec<Step>> {
    let listing = (planning.holding)();
    match id {
        Some(id) => {
            let found: Vec<&HoldingEntry> = listing.entries.iter().filter(|e| e.id == id).collect();
            if found.is_empty() {
                return Err(Error::new(format!(
                    "There is nothing in the holding area called {id}. It may have been emptied \
                     already."
                )));
            }
            Ok(found
                .iter()
                .map(|e| {
                    internal(
                        SourceKind::Arp,
                        format!("Deleting what was cleared from {} for good", e.name),
                        EMPTY,
                        vec![id.to_string()],
                        e.scope_machine,
                    )
                })
                .collect())
        }
        None => {
            let mut steps = vec![internal(
                SourceKind::Arp,
                "Emptying the holding area".to_string(),
                EMPTY,
                vec!["all".to_string()],
                false,
            )];
            if listing.entries.iter().any(|e| e.scope_machine) {
                steps.push(internal(
                    SourceKind::Arp,
                    "Emptying the holding area for items that need Administrator".to_string(),
                    EMPTY,
                    vec!["all".to_string()],
                    true,
                ));
            }
            Ok(steps)
        }
    }
}

/// Machine entries past thirty days are emptied by the next plan that asks
/// for Administrator anyway, so that start-up never has to. Nothing is added
/// to a plan that does not elevate, nor for an entry the plan already
/// restores or empties.
pub fn empty_waiting(steps: &[Step], planning: &Planning) -> Vec<Step> {
    if !steps.iter().any(|s| s.needs_root) {
        return Vec::new();
    }
    let named: Vec<&str> = steps
        .iter()
        .filter(|s| matches!(s.command.program.as_str(), RESTORE | EMPTY))
        .filter_map(|s| s.command.args.first().map(String::as_str))
        .collect();
    if named.contains(&"all") {
        return Vec::new();
    }
    (planning.holding)()
        .entries
        .iter()
        .filter(|e| {
            e.scope_machine && e.waiting && !e.incomplete && !named.contains(&e.id.as_str())
        })
        .map(|e| {
            internal(
                SourceKind::Arp,
                format!(
                    "Deleting what was cleared from {}, which has been in the holding area for 30 days",
                    e.name
                ),
                EMPTY,
                vec![e.id.clone()],
                true,
            )
        })
        .collect()
}

#[cfg(test)]
mod tests;
