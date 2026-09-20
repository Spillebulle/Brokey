//! Text mode: `brokey search steam`, `brokey sources`, `brokey
//! updates`. The same core with a table printer, and how the sources are
//! exercised on a machine with no display.
//!
//! Arguments are parsed by hand. They are a word and a few flags, and a
//! parser crate would be the only dependency the window does not need.
//!
//! Exit codes: 0 when the command did what it says, 1 when it ran but
//! something failed (a source could not answer, a plan could not be built),
//! 2 when the arguments were wrong.

use crate::commands::{logic, selfupdate_adapter, sentence};
use brokey_core::{
    App, Op, PackageKind, PackageRef, Plan, Platform, Query, SourceKind, SourceStatus, Store,
};
use std::io::IsTerminal;

pub const USAGE: &str = "\
Usage: brokey <command> [options]

Commands:
  search <term> [--sources pacman,aur] [--packages]
      Search the sources for a term. Only applications are listed unless
      --packages is given.
  sources
      Which sources this machine has, why the others are not usable, and
      which of those Brokey can set up.
  updates
      Everything that can be brought up to date, with download sizes.
  installed
      Everything the sources have put on this machine.
  plan install|remove|update <source>:<id> ...
  plan update-all|refresh|setup <source>
      The steps a transaction would run. Nothing is executed.
  drivers
      Devices, the driver profiles offered for them, and firmware.
  open <source>:<id> [--dry-run]
      Open an installed application the way the window's Open button does.
      --dry-run says what would be opened and opens nothing.
  self-update
      Whether a newer Brokey exists and how this copy would get it.
  help
      This text.

Without a command, brokey opens the window.
Exit codes: 0 done, 1 something failed, 2 the arguments were wrong.";

/// Results a source is asked for in text mode. Fewer than the window's
/// 200: a terminal is read top to bottom and nobody reads row 150.
const SEARCH_LIMIT: usize = 100;

/// Dispatch one command line, returning the exit code.
pub fn run(args: &[String]) -> i32 {
    // Warnings from the sources go to stderr, where a diagnostic tool's
    // warnings belong; RUST_LOG turns the rest on.
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn"))
        .try_init();
    let Some(command) = args.first() else {
        eprintln!("{USAGE}");
        return 2;
    };
    let rest = &args[1..];
    match command.as_str() {
        "help" | "--help" | "-h" => {
            println!("{USAGE}");
            0
        }
        "--version" | "-V" => {
            println!("brokey {}", env!("CARGO_PKG_VERSION"));
            0
        }
        "--install" => install(rest),
        "search" => search(rest),
        "sources" => sources(rest),
        "updates" => updates(rest),
        "installed" => installed(rest),
        "plan" => plan(rest),
        "drivers" => drivers(rest),
        "self-update" => self_update(rest),
        "open" => open(rest),
        other => usage_error(&format!("{other} is not a command.")),
    }
}

/// `--install`: the setup executable installs the Brokey it carries.
///
/// Deliberately not in [`USAGE`]. It belongs to `brokey-setup-<version>-<architecture>.exe`
/// and does nothing for the `brokey.exe` a user has on their path, so
/// listing it would offer everybody a command that answers "this copy
/// carries no installer". It is written down in `setup/payload.rs`, which is
/// where somebody looking for it would be.
fn install(args: &[String]) -> i32 {
    if let Some(code) = no_arguments("--install", args) {
        return code;
    }
    #[cfg(windows)]
    {
        crate::setup::install()
    }
    #[cfg(not(windows))]
    {
        eprintln!(
            "--install belongs to the Windows setup executable, and this is the Linux build. \
             Install Brokey here from the AppImage or from the package your distribution has."
        );
        2
    }
}

/// `open <source>:<id>`: what the window's Open button does, and what it
/// would open, so a launcher problem can be looked at from a terminal.
fn open(args: &[String]) -> i32 {
    #[cfg(unix)]
    {
        let dry_run = args.iter().any(|a| a == "--dry-run");
        let rest: Vec<&String> = args.iter().filter(|a| *a != "--dry-run").collect();
        let [reference] = rest.as_slice() else {
            return usage_error(
                "open takes one package reference, for example open flatpak:flathub/app/com.notepadqq.Notepadqq/x86_64/stable.",
            );
        };
        let package = match parse_ref(reference) {
            Ok(p) => p,
            Err(e) => return usage_error(&e),
        };
        let store = brokey_core::Store::detect();
        let Some(launch) = store.launcher(&package) else {
            eprintln!(
                "{} is not something Brokey can open. It may not be installed, or it has no application to start.",
                package.id
            );
            return 1;
        };
        if dry_run {
            println!("Would open {}, with: {}.", launch.describe(), {
                let c = launch.command();
                std::iter::once(c.program)
                    .chain(c.args)
                    .collect::<Vec<_>>()
                    .join(" ")
            });
        } else {
            println!("Opening {}.", launch.describe());
        }
        for (_, notice) in store.launcher_notices() {
            println!("{notice}");
        }
        if dry_run {
            return 0;
        }
        match logic::start(&launch) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("{e}");
                1
            }
        }
    }
    #[cfg(windows)]
    {
        // No source has a launcher on Windows yet; see `logic::open_app`.
        let _ = args;
        eprintln!("Opening an installed application is not available on Windows yet.");
        1
    }
}

fn usage_error(message: &str) -> i32 {
    eprintln!("brokey: {message} Run brokey help to see the commands.");
    2
}

/// A command that takes no arguments was given some.
fn no_arguments(command: &str, args: &[String]) -> Option<i32> {
    if args.is_empty() {
        None
    } else {
        Some(usage_error(&format!(
            "{command} takes no arguments, but got {}.",
            args.join(" ")
        )))
    }
}

// ---------------------------------------------------------------- search

#[derive(Debug, PartialEq, Eq)]
pub struct SearchArgs {
    pub term: String,
    pub sources: Option<Vec<SourceKind>>,
    pub packages: bool,
}

/// `search <term...> [--sources a,b] [--packages]`. Several words are one
/// term, so `brokey search visual studio code` asks what a user typing
/// into the box would ask.
pub fn parse_search(args: &[String]) -> Result<SearchArgs, String> {
    let mut words: Vec<&str> = Vec::new();
    let mut sources = None;
    let mut packages = false;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "--packages" {
            packages = true;
        } else if let Some(list) = arg.strip_prefix("--sources=") {
            sources = Some(parse_sources(list)?);
        } else if arg == "--sources" {
            i += 1;
            let Some(list) = args.get(i) else {
                return Err(
                    "--sources needs a comma list of sources, for example --sources pacman,aur."
                        .to_string(),
                );
            };
            sources = Some(parse_sources(list)?);
        } else if arg.starts_with('-') && arg.len() > 1 {
            return Err(format!(
                "{arg} is not an option of search. The options are --sources and --packages."
            ));
        } else {
            words.push(arg);
        }
        i += 1;
    }
    let term = words.join(" ").trim().to_string();
    if term.is_empty() {
        return Err("search needs a term, for example brokey search steam.".to_string());
    }
    Ok(SearchArgs {
        term,
        sources,
        packages,
    })
}

/// A comma list of source ids, every one of which must be known: a typo in
/// a source name is a question that deserves an answer, not a silent
/// search of nothing.
pub fn parse_sources(list: &str) -> Result<Vec<SourceKind>, String> {
    let mut kinds = Vec::new();
    for word in list.split(',').map(str::trim).filter(|w| !w.is_empty()) {
        match SourceKind::parse(&word.to_ascii_lowercase()) {
            Some(k) if !kinds.contains(&k) => kinds.push(k),
            Some(_) => {}
            None => {
                let known: Vec<&str> = SourceKind::ALL.iter().map(|k| k.id()).collect();
                return Err(format!(
                    "{word} is not a source. The sources are {}.",
                    known.join(", ")
                ));
            }
        }
    }
    if kinds.is_empty() {
        return Err(
            "--sources needs at least one source, for example --sources pacman,aur.".to_string(),
        );
    }
    Ok(kinds)
}

fn search(args: &[String]) -> i32 {
    let parsed = match parse_search(args) {
        Ok(p) => p,
        Err(e) => return usage_error(&e),
    };
    let store = Store::detect();
    let query = Query {
        text: parsed.term.clone(),
        sources: parsed.sources,
        limit: SEARCH_LIMIT,
        split: Vec::new(),
    };
    let result = store.search(&query);
    let rows: Vec<&App> = result
        .apps
        .iter()
        .filter(|a| parsed.packages || a.kind == PackageKind::App)
        .collect();

    if result.searched.is_empty() {
        println!("No source is available to search. Run brokey sources to see why.");
    } else if rows.is_empty() {
        let hint = if !parsed.packages && !result.apps.is_empty() {
            format!(
                " {} matching packages are hidden; add --packages to list them.",
                result.apps.len()
            )
        } else {
            String::new()
        };
        println!(
            "Nothing matched {} in {}.{hint}",
            parsed.term,
            names(&result.searched)
        );
    } else {
        let mut table = Table::new(&["Name", "Sources", "Installed", "Summary"]);
        for app in &rows {
            table.row(vec![
                app.name.clone(),
                editions(app, false),
                if app.installed {
                    "yes".to_string()
                } else {
                    String::new()
                },
                app.summary.clone().unwrap_or_default(),
            ]);
        }
        print!("{}", table.render(terminal_width()));
        println!(
            "{} {} from {}.",
            rows.len(),
            plural(rows.len(), "result", "results"),
            names(&result.searched)
        );
    }
    report_failures(&result.failed)
}

/// "pacman 1.0.0.81, AUR 1.0.0.81-1": every edition with its version, in
/// source order. With `installed`, the installed version instead.
fn editions(app: &App, installed: bool) -> String {
    app.editions
        .iter()
        .map(|e| {
            let p = &e.package;
            let version = if installed {
                &p.installed_version
            } else {
                &p.version
            };
            match version {
                Some(v) => format!("{} {v}", p.source.label()),
                None => p.source.label().to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Failures to stderr, one sentence each; exit 1 if there were any, since
/// a partial answer is a failure for anything that scripts this. A sentence
/// that already opens with the source's label (the stale-list line from
/// `updates::collect_with`) is printed as it is.
fn report_failures(failed: &[(SourceKind, String)]) -> i32 {
    for (kind, message) in failed {
        let label = kind.label();
        let text = sentence(message);
        if text.starts_with(&format!("{label}: ")) {
            eprintln!("{text}");
        } else {
            eprintln!("{label}: {text}");
        }
    }
    if failed.is_empty() { 0 } else { 1 }
}

// --------------------------------------------------------------- sources

fn sources(args: &[String]) -> i32 {
    if let Some(code) = no_arguments("sources", args) {
        return code;
    }
    let store = Store::detect();
    print!(
        "{}",
        sources_table(&store.statuses()).render(terminal_width())
    );
    println!("{} on {}.", store.system.pretty_name, store.system.arch);
    0
}

/// One row per source: whether it is usable, what setting it up is called
/// when the store can do that ("Install Flatpak"), and the detail or the
/// reason it is not usable. A source that can be searched without its tool
/// says so in the Available column.
pub fn sources_table(statuses: &[SourceStatus]) -> Table {
    let mut table = Table::new(&["Source", "Available", "Setup", "Detail"]);
    for status in statuses {
        let detail = if status.available {
            status.detail.clone().unwrap_or_default()
        } else {
            status
                .reason
                .clone()
                .map(|r| sentence(&r))
                .unwrap_or_default()
        };
        let available = if status.available {
            "yes"
        } else if status.searchable {
            "search only"
        } else {
            "no"
        };
        table.row(vec![
            status.kind.label().to_string(),
            available.to_string(),
            status
                .setup
                .as_ref()
                .map(|s| s.label.clone())
                .unwrap_or_default(),
            detail,
        ]);
    }
    table
}

// --------------------------------------------------------------- updates

fn updates(args: &[String]) -> i32 {
    if let Some(code) = no_arguments("updates", args) {
        return code;
    }
    let store = Store::detect();
    let list = store.updates();
    if list.updates.is_empty() {
        println!("Everything is up to date.");
    } else {
        let mut table = Table::new(&["Name", "Source", "From", "To", "Size"]);
        let mut total = 0u64;
        for u in &list.updates {
            total += u.download_size.unwrap_or(0);
            table.row(vec![
                if u.is_self {
                    format!("{} (this application)", u.name)
                } else {
                    u.name.clone()
                },
                u.package.source.label().to_string(),
                u.from.clone().unwrap_or_default(),
                u.to.clone(),
                u.download_size.map(size).unwrap_or_default(),
            ]);
        }
        print!("{}", table.render(terminal_width()));
        println!(
            "{} {}, {} to download.",
            list.updates.len(),
            plural(list.updates.len(), "update", "updates"),
            size(total)
        );
    }
    report_failures(&list.failed)
}

// ------------------------------------------------------------- installed

fn installed(args: &[String]) -> i32 {
    if let Some(code) = no_arguments("installed", args) {
        return code;
    }
    let store = Store::detect();
    let result = store.installed();
    if result.apps.is_empty() {
        println!("Nothing is installed from {}.", names(&result.searched));
    } else {
        let mut table = Table::new(&["Name", "Sources", "Summary"]);
        for app in &result.apps {
            table.row(vec![
                app.name.clone(),
                editions(app, true),
                app.summary.clone().unwrap_or_default(),
            ]);
        }
        print!("{}", table.render(terminal_width()));
        println!(
            "{} installed from {}.",
            result.apps.len(),
            names(&result.searched)
        );
    }
    report_failures(&result.failed)
}

// ------------------------------------------------------------------ plan

/// `source:id` as the page and the settings write it.
pub fn parse_ref(text: &str) -> Result<PackageRef, String> {
    let wrong = || {
        format!(
            "{text} is not a package reference. Write it as source:id, for example pacman:steam or flatpak:flathub/app/org.gimp.GIMP/x86_64/stable."
        )
    };
    let (source, id) = text.split_once(':').ok_or_else(wrong)?;
    let source = SourceKind::parse(&source.to_ascii_lowercase()).ok_or_else(wrong)?;
    if id.is_empty() {
        return Err(wrong());
    }
    Ok(PackageRef {
        source,
        id: id.to_string(),
    })
}

/// `install|remove|update <ref>...` or `update-all|refresh|setup <source>`.
pub fn parse_plan(args: &[String]) -> Result<Vec<Op>, String> {
    let Some(verb) = args.first() else {
        return Err(
            "plan needs an operation: install, remove, update, update-all, refresh or setup."
                .to_string(),
        );
    };
    let rest = &args[1..];
    match verb.as_str() {
        "install" | "remove" | "update" => {
            if rest.is_empty() {
                return Err(format!(
                    "plan {verb} needs at least one source:id, for example plan {verb} pacman:steam."
                ));
            }
            rest.iter()
                .map(|r| {
                    let package = parse_ref(r)?;
                    Ok(match verb.as_str() {
                        "install" => Op::Install { package },
                        "remove" => Op::Remove { package },
                        _ => Op::Update { package },
                    })
                })
                .collect()
        }
        "update-all" | "refresh" | "setup" => {
            if rest.is_empty() {
                return Err(format!(
                    "plan {verb} needs a source, for example plan {verb} pacman."
                ));
            }
            rest.iter()
                .map(|s| {
                    let source = parse_sources(s)?;
                    let source = source[0];
                    Ok(match verb.as_str() {
                        "update-all" => Op::UpdateAll { source },
                        "refresh" => Op::Refresh { source },
                        _ => Op::Setup { source },
                    })
                })
                .collect()
        }
        other => Err(format!(
            "{other} is not a plan operation. Use install, remove, update, update-all, refresh or setup."
        )),
    }
}

fn plan(args: &[String]) -> i32 {
    let ops = match parse_plan(args) {
        Ok(ops) => ops,
        Err(e) => return usage_error(&e),
    };
    let store = Store::detect();
    let plan = match store.plan(&ops) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("The plan could not be built. {}", sentence(&e.message));
            return 1;
        }
    };
    print!(
        "{}",
        plan_text(
            &plan,
            &logic::notices(&store, &ops),
            store.system.platform,
            terminal_width()
        )
    );
    0
}

/// Who a privileged step runs as, for the summary sentence: there is no root
/// on Windows, only Administrator, and UAC asks the user to allow the step
/// rather than asking for a password.
fn elevated_word(platform: Platform) -> &'static str {
    match platform {
        Platform::Linux => "root",
        Platform::Windows => "Administrator",
    }
}

/// The same word, for the table header, which is title case throughout.
/// "Administrator" is already the right case; "root" is not.
fn elevated_header(platform: Platform) -> &'static str {
    match platform {
        Platform::Linux => "Root",
        Platform::Windows => "Administrator",
    }
}

/// What `plan` prints: the steps as a table and a count, or that there is
/// nothing to do, then every notice.
pub fn plan_text(
    plan: &Plan,
    notices: &[String],
    platform: Platform,
    width: Option<usize>,
) -> String {
    let mut out = String::new();
    let elevated = elevated_word(platform);
    if plan.steps.is_empty() {
        out.push_str("Nothing to do. Everything asked for is already in place.\n");
    } else {
        let mut table = Table::new(&["#", "Source", elevated_header(platform), "Step", "Command"]);
        for (i, step) in plan.steps.iter().enumerate() {
            let mut command = vec![step.command.program.clone()];
            command.extend(step.command.args.iter().cloned());
            table.row(vec![
                (i + 1).to_string(),
                step.source.label().to_string(),
                if step.needs_root { "yes" } else { "" }.to_string(),
                step.title.clone(),
                command.join(" "),
            ]);
        }
        out.push_str(&table.render(width));
        let root = plan.steps.iter().filter(|s| s.needs_root).count();
        out.push_str(&format!(
            "{} {}, {} {} {elevated}. Nothing was run.\n",
            plan.steps.len(),
            plural(plan.steps.len(), "step", "steps"),
            root,
            plural(root, "needs", "need")
        ));
    }
    for notice in notices {
        out.push_str(&format!("Note: {}\n", sentence(notice)));
    }
    out
}

// --------------------------------------------------------------- drivers

fn drivers(args: &[String]) -> i32 {
    if let Some(code) = no_arguments("drivers", args) {
        return code;
    }
    let store = Store::detect();
    let report = brokey_core::drivers::report(&store);
    match &report.manager {
        Some(m) => println!("Driver manager: {m}."),
        None => println!("Driver manager: none."),
    }
    if let Some(note) = &report.manager_note {
        println!("{}", sentence(note));
    }
    for device in &report.devices {
        let class = device
            .class
            .as_deref()
            .map(|c| format!(" ({c})"))
            .unwrap_or_default();
        let vendor = device
            .vendor
            .as_deref()
            .map(|v| format!("{v} "))
            .unwrap_or_default();
        println!("\n{vendor}{}{class} [{}]", device.name, device.id);
        for profile in &device.profiles {
            let mut marks = Vec::new();
            if profile.installed {
                marks.push("installed");
            }
            if profile.recommended {
                marks.push("recommended");
            }
            let marks = if marks.is_empty() {
                String::new()
            } else {
                format!(" ({})", marks.join(", "))
            };
            println!("  {}{marks}", profile.name);
            if let Some(d) = &profile.description {
                println!("      {}", sentence(d));
            }
            if !profile.packages.is_empty() {
                println!("      Installs {}.", profile.packages.join(", "));
            }
        }
    }
    println!();
    if report.firmware_available {
        println!("Firmware: fwupd.");
    } else {
        println!("Firmware: not available.");
    }
    if let Some(note) = &report.firmware_note {
        println!("{}", sentence(note));
    }
    if !report.firmware.is_empty() {
        let mut table = Table::new(&["Device", "Version", "Update", "Size", "Reboot"]);
        for f in &report.firmware {
            let vendor = f
                .vendor
                .as_deref()
                .map(|v| format!("{v} "))
                .unwrap_or_default();
            table.row(vec![
                format!("{vendor}{}", f.name),
                f.version.clone().unwrap_or_default(),
                f.update_version.clone().unwrap_or_default(),
                f.update_size.map(size).unwrap_or_default(),
                if f.needs_reboot { "yes" } else { "" }.to_string(),
            ]);
        }
        print!("{}", table.render(terminal_width()));
    }
    0
}

// ----------------------------------------------------------- self-update

fn self_update(args: &[String]) -> i32 {
    if let Some(code) = no_arguments("self-update", args) {
        return code;
    }
    // Asked for by name, so past the disk cache: the answer is GitHub's or
    // a sentence saying why it could not be.
    let update = match selfupdate_adapter::check(true) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("The check failed. {}", sentence(&e));
            return 1;
        }
    };
    println!("Brokey {}.", update.current);
    match &update.latest {
        Some(latest) if latest.newer => {
            println!("Newest release: {}, newer than this copy.", latest.version)
        }
        Some(latest) => println!(
            "Newest release: {}. This copy is up to date.",
            latest.version
        ),
        // The core's sentence already names GitHub and says what to do.
        None => match &update.error {
            Some(e) => println!("{}", sentence(e)),
            None => println!("No release has been published yet."),
        },
    }
    println!("Installation: {}.", update.installation_label);
    match &update.remedy {
        Some(remedy) => println!("{}", remedy.sentence()),
        None => println!("Nothing to apply from inside the application."),
    }
    0
}

// ------------------------------------------------------------ formatting

/// A plain table: columns as wide as their widest cell, two spaces between,
/// and the last column cut to the terminal's width with an ellipsis, since
/// it is always the one that runs long (a summary, a command). When the
/// fixed columns already overflow, nothing is cut: a wrapped line beats a
/// column of ellipses.
pub struct Table {
    header: Vec<String>,
    rows: Vec<Vec<String>>,
}

const GAP: usize = 2;
/// Below this the last column says nothing; better to let the line wrap.
const MIN_LAST: usize = 8;

impl Table {
    pub fn new(header: &[&str]) -> Table {
        Table {
            header: header.iter().map(|h| h.to_string()).collect(),
            rows: Vec::new(),
        }
    }

    pub fn row(&mut self, cells: Vec<String>) {
        debug_assert_eq!(cells.len(), self.header.len());
        self.rows.push(cells);
    }

    pub fn render(&self, width: Option<usize>) -> String {
        let columns = self.header.len();
        if columns == 0 {
            return String::new();
        }
        let mut widths: Vec<usize> = self.header.iter().map(|h| h.chars().count()).collect();
        for row in &self.rows {
            for (i, cell) in row.iter().enumerate() {
                widths[i] = widths[i].max(cell.chars().count());
            }
        }
        let last = columns - 1;
        let fixed: usize = widths[..last].iter().sum::<usize>() + GAP * last;
        let cut = width
            .map(|w| w.saturating_sub(fixed))
            .filter(|avail| *avail >= MIN_LAST && *avail < widths[last]);

        let mut out = String::new();
        for cells in std::iter::once(&self.header).chain(self.rows.iter()) {
            let mut line = String::new();
            for (i, cell) in cells.iter().enumerate() {
                if i == last {
                    line.push_str(&match cut {
                        Some(avail) => truncate(cell, avail),
                        None => cell.clone(),
                    });
                } else {
                    line.push_str(cell);
                    let pad = widths[i] - cell.chars().count() + GAP;
                    line.extend(std::iter::repeat_n(' ', pad));
                }
            }
            out.push_str(line.trim_end());
            out.push('\n');
        }
        out
    }
}

/// At most `max` characters, the last one an ellipsis when something was
/// cut.
pub fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// "1.5 GB", "12.3 MB", "820 kB", "512 B". Decimal units, as the page's
/// figures are; the space is a plain one because a terminal cannot be
/// trusted with a thin one.
pub fn size(bytes: u64) -> String {
    const K: f64 = 1000.0;
    let b = bytes as f64;
    if b < K {
        format!("{bytes} B")
    } else if b < K * K {
        format!("{:.0} kB", b / K)
    } else if b < K * K * K {
        format!("{:.1} MB", b / (K * K))
    } else {
        format!("{:.1} GB", b / (K * K * K))
    }
}

/// "pacman", "pacman and AUR", "pacman, AUR and Flatpak".
pub fn names(kinds: &[SourceKind]) -> String {
    let labels: Vec<&str> = kinds.iter().map(|k| k.label()).collect();
    match labels.len() {
        0 => "no source".to_string(),
        1 => labels[0].to_string(),
        n => format!("{} and {}", labels[..n - 1].join(", "), labels[n - 1]),
    }
}

fn plural<'a>(n: usize, one: &'a str, many: &'a str) -> &'a str {
    if n == 1 { one } else { many }
}

/// The terminal's width, or `None` when stdout is not a terminal, so piped
/// output is never cut. `COLUMNS` first, then `stty`, then a guess.
fn terminal_width() -> Option<usize> {
    if !std::io::stdout().is_terminal() {
        return None;
    }
    if let Some(c) = std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.parse::<usize>().ok())
    {
        return Some(c.max(MIN_LAST));
    }
    let from_stty = std::process::Command::new("stty")
        .arg("size")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .split_whitespace()
                .nth(1)
                .and_then(|c| c.parse::<usize>().ok())
        });
    Some(from_stty.unwrap_or(100))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn search_arguments_are_a_term_and_two_options() {
        let parsed = parse_search(&args(&[
            "visual",
            "studio",
            "code",
            "--sources",
            "pacman,aur",
            "--packages",
        ]))
        .unwrap();
        assert_eq!(parsed.term, "visual studio code");
        assert_eq!(
            parsed.sources,
            Some(vec![SourceKind::Pacman, SourceKind::Aur])
        );
        assert!(parsed.packages);

        let parsed = parse_search(&args(&["--sources=flatpak", "gimp"])).unwrap();
        assert_eq!(parsed.term, "gimp");
        assert_eq!(parsed.sources, Some(vec![SourceKind::Flatpak]));
        assert!(!parsed.packages);

        assert!(
            parse_search(&args(&[]))
                .unwrap_err()
                .contains("needs a term")
        );
        assert!(
            parse_search(&args(&["--sources"]))
                .unwrap_err()
                .contains("needs a comma list")
        );
        assert!(
            parse_search(&args(&["x", "--verbose"]))
                .unwrap_err()
                .contains("not an option")
        );
        assert!(
            parse_search(&args(&["x", "--sources", "nix"]))
                .unwrap_err()
                .contains("nix is not a source")
        );
    }

    #[test]
    fn a_reference_is_source_colon_id() {
        let r = parse_ref("flatpak:flathub/app/org.gimp.GIMP/x86_64/stable").unwrap();
        assert_eq!(r.source, SourceKind::Flatpak);
        assert_eq!(r.id, "flathub/app/org.gimp.GIMP/x86_64/stable");
        assert_eq!(parse_ref("AUR:steam-git").unwrap().source, SourceKind::Aur);
        for bad in ["steam", "nix:steam", "pacman:"] {
            assert!(
                parse_ref(bad)
                    .unwrap_err()
                    .contains("not a package reference"),
                "{bad}"
            );
        }
    }

    #[test]
    fn plan_arguments_become_operations() {
        let ops = parse_plan(&args(&["install", "pacman:steam", "aur:steam-git"])).unwrap();
        assert_eq!(ops.len(), 2);
        assert!(matches!(&ops[0], Op::Install { package } if package.id == "steam"));
        assert!(matches!(
            parse_plan(&args(&["remove", "pacman:steam"])).unwrap()[0],
            Op::Remove { .. }
        ));
        assert!(matches!(
            parse_plan(&args(&["update", "pacman:steam"])).unwrap()[0],
            Op::Update { .. }
        ));
        assert!(matches!(
            parse_plan(&args(&["update-all", "pacman"])).unwrap()[0],
            Op::UpdateAll {
                source: SourceKind::Pacman
            }
        ));
        assert!(matches!(
            parse_plan(&args(&["refresh", "flatpak"])).unwrap()[0],
            Op::Refresh {
                source: SourceKind::Flatpak
            }
        ));
        assert_eq!(
            parse_plan(&args(&["setup", "flatpak", "snap"])).unwrap(),
            [
                Op::Setup {
                    source: SourceKind::Flatpak
                },
                Op::Setup {
                    source: SourceKind::Snap
                }
            ]
        );
        assert!(
            parse_plan(&args(&["setup"]))
                .unwrap_err()
                .contains("needs a source")
        );
        assert!(
            parse_plan(&args(&["setup", "nowhere"]))
                .unwrap_err()
                .contains("nowhere")
        );
        assert!(
            parse_plan(&args(&[]))
                .unwrap_err()
                .contains("needs an operation")
        );
        assert!(
            parse_plan(&args(&["install"]))
                .unwrap_err()
                .contains("at least one")
        );
        assert!(
            parse_plan(&args(&["update-all"]))
                .unwrap_err()
                .contains("needs a source")
        );
        assert!(
            parse_plan(&args(&["purge", "x"]))
                .unwrap_err()
                .contains("not a plan operation")
        );
    }

    #[test]
    fn a_table_pads_every_column_but_the_last_and_cuts_only_that_one() {
        let mut t = Table::new(&["Name", "Summary"]);
        t.row(vec![
            "steam".into(),
            "Valve's digital software delivery system".into(),
        ]);
        t.row(vec!["gimp".into(), "Image editor".into()]);
        let full = t.render(None);
        assert_eq!(
            full,
            "Name   Summary\nsteam  Valve's digital software delivery system\ngimp   Image editor\n"
        );
        assert!(
            !full.lines().any(|l| l.ends_with(' ')),
            "no trailing spaces"
        );

        let cut = t.render(Some(30));
        // "steam  " is 7, leaving 23 for the summary.
        assert_eq!(
            cut.lines().nth(1).unwrap(),
            "steam  Valve's digital softwa…"
        );
        assert_eq!(cut.lines().nth(2).unwrap(), "gimp   Image editor");
        assert!(cut.lines().all(|l| l.chars().count() <= 30));

        // Too narrow to cut sensibly: leave the line to wrap.
        assert_eq!(t.render(Some(10)), full);
    }

    #[test]
    fn sizes_and_lists_read_as_sentences() {
        assert_eq!(size(512), "512 B");
        assert_eq!(size(820_000), "820 kB");
        assert_eq!(size(12_300_000), "12.3 MB");
        assert_eq!(size(1_500_000_000), "1.5 GB");
        assert_eq!(names(&[]), "no source");
        assert_eq!(names(&[SourceKind::Pacman]), "pacman");
        assert_eq!(
            names(&[SourceKind::Pacman, SourceKind::Aur]),
            "pacman and AUR"
        );
        assert_eq!(
            names(&[SourceKind::Pacman, SourceKind::Aur, SourceKind::Flatpak]),
            "pacman, AUR and Flatpak"
        );
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("abcd", 4), "abcd");
    }

    #[test]
    fn the_sources_table_has_a_setup_column() {
        let status = |kind, available, searchable, setup: Option<&str>| SourceStatus {
            kind,
            available,
            reason: (!available).then(|| format!("{} is not installed", kind.label())),
            detail: available.then(|| "core, extra".to_string()),
            searchable,
            setup: setup.map(|label| brokey_core::SourceSetup {
                label: label.to_string(),
                sentence: "Sets it up.".to_string(),
            }),
        };
        let table = sources_table(&[
            status(SourceKind::Pacman, true, false, None),
            status(SourceKind::Flatpak, false, true, Some("Install Flatpak")),
            status(SourceKind::Fwupd, false, false, None),
        ]);
        assert_eq!(
            table.render(None),
            "\
Source    Available    Setup            Detail
pacman    yes                           core, extra
Flatpak   search only  Install Flatpak  Flatpak is not installed.
Firmware  no                            Firmware is not installed.
"
        );
    }

    #[test]
    fn a_setup_plan_prints_its_steps_in_order_and_its_notice() {
        let step = |source, title: &str, program: &str, args: &[&str], root| brokey_core::Step {
            source,
            title: title.to_string(),
            command: brokey_core::Command {
                program: program.to_string(),
                args: args.iter().map(|a| a.to_string()).collect(),
                env: Vec::new(),
                cwd: None,
            },
            needs_root: root,
            weight: 1,
        };
        let plan = Plan {
            id: "p".into(),
            ops: vec![Op::Setup {
                source: SourceKind::Flatpak,
            }],
            steps: vec![
                step(
                    SourceKind::Pacman,
                    "Installing flatpak and updating the system",
                    "pacman",
                    &["-Syu", "--noconfirm", "--needed", "flatpak"],
                    true,
                ),
                step(
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
                ),
            ],
        };
        let text = plan_text(
            &plan,
            &["Flatpak is not installed. It is installed and Flathub is added.".to_string()],
            Platform::Linux,
            None,
        );
        assert_eq!(
            text,
            "\
#  Source   Root  Step                                        Command
1  pacman   yes   Installing flatpak and updating the system  pacman -Syu --noconfirm --needed flatpak
2  Flatpak  yes   Adding Flathub                              flatpak remote-add --if-not-exists --system flathub https://dl.flathub.org/repo/flathub.flatpakrepo
2 steps, 2 need root. Nothing was run.
Note: Flatpak is not installed. It is installed and Flathub is added.
"
        );
        assert_eq!(
            plan_text(
                &Plan {
                    id: "p".into(),
                    ops: Vec::new(),
                    steps: Vec::new()
                },
                &[],
                Platform::Linux,
                None
            ),
            "Nothing to do. Everything asked for is already in place.\n"
        );
    }

    /// Windows has no root: the same plan says Administrator instead, in
    /// both the column header and the summary.
    #[test]
    fn a_windows_plan_says_administrator_not_root() {
        let plan = Plan {
            id: "p".into(),
            ops: vec![Op::Remove {
                package: PackageRef {
                    source: SourceKind::Arp,
                    id: r"HKLM\7-Zip".to_string(),
                },
            }],
            steps: vec![brokey_core::Step {
                source: SourceKind::Arp,
                title: "Removing 7-Zip".to_string(),
                command: brokey_core::Command {
                    program: r"C:\Program Files\7-Zip\Uninstall.exe".to_string(),
                    args: vec!["/S".to_string()],
                    env: Vec::new(),
                    cwd: None,
                },
                needs_root: true,
                weight: 1,
            }],
        };
        let text = plan_text(&plan, &[], Platform::Windows, None);
        assert!(
            text.contains("Administrator"),
            "the header and summary say Administrator: {text}"
        );
        assert!(
            !text.contains("root"),
            "root is a Linux word and should not appear on Windows: {text}"
        );
    }

    #[test]
    fn exit_codes_say_what_happened() {
        assert_eq!(run(&args(&["help"])), 0);
        assert_eq!(run(&args(&[])), 2);
        assert_eq!(run(&args(&["frobnicate"])), 2);
        assert_eq!(run(&args(&["search"])), 2);
        assert_eq!(run(&args(&["plan", "install", "nonsense"])), 2);
        assert_eq!(run(&args(&["sources", "extra"])), 2);
        // These detect the store, which on this branch is instant and
        // answers with nothing; they still complete.
        assert_eq!(run(&args(&["sources"])), 0);
        // pacman is always a source on Linux, whether or not it is
        // installed. Windows has its own sources (Add/Remove Programs,
        // winget), but pacman itself is Linux-only by design and never one
        // of them, so planning against it is refused instead.
        #[cfg(unix)]
        assert_eq!(run(&args(&["plan", "install", "pacman:steam"])), 0);
        #[cfg(windows)]
        assert_eq!(run(&args(&["plan", "install", "pacman:steam"])), 1);
        assert_eq!(run(&args(&["self-update"])), 0);
    }

    #[test]
    fn the_usage_text_keeps_the_house_voice() {
        assert!(!USAGE.contains('\u{2014}'), "no em dashes");
        assert!(
            USAGE.contains("Exit codes: 0 done, 1 something failed, 2 the arguments were wrong.")
        );
    }
}
