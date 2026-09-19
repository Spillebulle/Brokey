//! What a tool's output says about how far it is. The helper streams every
//! line as a log; the runner feeds each one through the parser for the step's
//! program and turns what it learns into `Event::Progress`.
//!
//! A parser gives a fraction only when the line states a count it can trust
//! (`(3/7) installing foo`). Everything else is at most a sentence for the
//! rail, and a line that says nothing is `None`. Never a guess: a rail that
//! creeps forward on a timer would be the fake percentage the invariants
//! forbid.

/// One line's worth of progress. `fraction` is how far the current step is,
/// 0 to 1, when the line said so; `message` is what the rail should say.
#[derive(Clone, Debug, PartialEq)]
pub struct Reading {
    pub fraction: Option<f32>,
    pub message: Option<String>,
}

impl Reading {
    fn message(text: impl Into<String>) -> Reading {
        Reading {
            fraction: None,
            message: Some(text.into()),
        }
    }
    fn at(done: u32, total: u32, text: impl Into<String>) -> Reading {
        Reading {
            fraction: Some(done as f32 / total as f32),
            message: Some(text.into()),
        }
    }
}

/// Reads a tool's output one line at a time. Stateful so a parser may
/// remember a total announced on an earlier line.
pub trait ProgressParser: Send {
    fn line(&mut self, line: &str) -> Option<Reading>;
}

/// The parser for a program's output. Programs without one get the generic
/// parser, which only turns "doing something..." lines into sentences.
pub fn parser_for(program: &str) -> Box<dyn ProgressParser> {
    match program {
        "pacman" => Box::new(Pacman),
        "apt-get" | "apt" | "dpkg" => Box::new(Apt),
        "dnf" => Box::new(Dnf),
        // The Flatpak source owns its output format: a percentage line when
        // flatpak draws progress, or one "Installing <ref>" line per
        // operation under --noninteractive, which gives a sentence and no
        // fraction. Linux only, along with the source itself.
        #[cfg(unix)]
        "flatpak" => Box::new(FnParser(flatpak_line)),
        _ => Box::new(Generic),
    }
}

#[cfg(unix)]
fn flatpak_line(line: &str) -> Option<Reading> {
    if let Some((fraction, message)) = crate::sources::linux::flatpak::parse_progress(line) {
        return Some(Reading {
            fraction: Some(fraction),
            message: Some(message),
        });
    }
    crate::sources::linux::flatpak::parse_operation(line).map(Reading::message)
}

/// Adapts a plain `fn(&str) -> Option<Reading>` so a source can ship its
/// parser as a function and the runner can still hold it as a trait object.
pub struct FnParser(pub fn(&str) -> Option<Reading>);

impl ProgressParser for FnParser {
    fn line(&mut self, line: &str) -> Option<Reading> {
        (self.0)(line)
    }
}

/// pacman prints `(n/m) verbing name` for each package it touches, and a
/// handful of phase lines ending in `...`. With stdout on a pipe it draws no
/// bars, so what arrives is exactly these lines.
pub struct Pacman;

impl ProgressParser for Pacman {
    fn line(&mut self, line: &str) -> Option<Reading> {
        let line = line.trim();
        if let Some((done, total, rest)) = counted(line) {
            let mut words = rest.splitn(2, ' ');
            let verb = words.next().unwrap_or("");
            let name = words.next().unwrap_or("").trim();
            let verb_word = match verb {
                "installing" => Some("Installing"),
                "upgrading" => Some("Upgrading"),
                "reinstalling" => Some("Reinstalling"),
                "downgrading" => Some("Downgrading"),
                "removing" => Some("Removing"),
                _ => None,
            };
            return Some(match verb_word {
                // The count over the whole transaction is only trustworthy
                // for the package lines; the pre-flight checks and the hooks
                // use their own counts, which would make the rail jump back.
                Some(verb_word) if !name.is_empty() => {
                    Reading::at(done, total, format!("{verb_word} {name}"))
                }
                _ => Reading::message(sentence(rest)),
            });
        }
        if let Some(name) = line.strip_suffix(" downloading...") {
            return Some(Reading::message(format!(
                "Downloading {}",
                strip_version(name.trim())
            )));
        }
        let phrase = match line {
            ":: Synchronizing package databases..." => Some("Synchronising package databases"),
            ":: Starting full system upgrade..." => Some("Starting full system upgrade"),
            "resolving dependencies..." => Some("Resolving dependencies"),
            "looking for conflicting packages..." => Some("Checking for conflicts"),
            ":: Retrieving packages..." => Some("Downloading packages"),
            "checking keyring..." => Some("Checking keyring"),
            "checking package integrity..." => Some("Checking package integrity"),
            "loading package files..." => Some("Loading package files"),
            "checking for file conflicts..." => Some("Checking for file conflicts"),
            "checking available disk space..." => Some("Checking available disk space"),
            ":: Processing package changes..." => Some("Applying changes"),
            ":: Running pre-transaction hooks..." => Some("Running pre-transaction hooks"),
            ":: Running post-transaction hooks..." => Some("Running post-transaction hooks"),
            _ => None,
        };
        if let Some(phrase) = phrase {
            return Some(Reading::message(phrase));
        }
        Generic.line(line)
    }
}

/// `(n/m) rest` at the start of a line.
fn counted(line: &str) -> Option<(u32, u32, &str)> {
    let inner = line.strip_prefix('(')?;
    let (count, rest) = inner.split_once(") ")?;
    let (done, total) = count.split_once('/')?;
    let done: u32 = done.parse().ok()?;
    let total: u32 = total.parse().ok()?;
    if total == 0 || done > total {
        return None;
    }
    Some((done, total, rest))
}

/// `name-1.2.3-1-x86_64` to `name`. A pacman file name is name, version,
/// release and architecture joined by dashes, and only the name may itself
/// contain dashes, so the last three dash-separated parts are dropped when
/// they look like a version and an architecture.
fn strip_version(token: &str) -> &str {
    let parts: Vec<usize> = token.match_indices('-').map(|(i, _)| i).collect();
    if parts.len() < 3 {
        return token;
    }
    let cut = parts[parts.len() - 3];
    let version = &token[cut + 1..];
    if version.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        &token[..cut]
    } else {
        token
    }
}

/// apt-get and dpkg. Without a terminal apt prints no percentage, so most of
/// this is sentences; `Progress: [ 20%]` is honoured when a configuration
/// does print it.
pub struct Apt;

impl ProgressParser for Apt {
    fn line(&mut self, line: &str) -> Option<Reading> {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Progress: [") {
            let digits: String = rest
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == ' ')
                .collect();
            if let Ok(percent) = digits.trim().parse::<u32>() {
                return Some(Reading {
                    fraction: Some((percent.min(100)) as f32 / 100.0),
                    message: None,
                });
            }
        }
        if line.starts_with("Get:") {
            // `Get:3 http://deb.debian.org/debian bookworm/main amd64 steam amd64 1.0 [1234 kB]`
            let mut fields = line.split_whitespace();
            let name = fields.nth(4).unwrap_or("");
            if !name.is_empty() {
                return Some(Reading::message(format!("Downloading {name}")));
            }
        }
        for (prefix, verb) in [
            ("Unpacking ", "Unpacking"),
            ("Setting up ", "Setting up"),
            ("Removing ", "Removing"),
            (
                "Purging configuration files for ",
                "Purging configuration files for",
            ),
            ("Processing triggers for ", "Processing triggers for"),
            ("Preparing to unpack ", "Preparing to unpack"),
        ] {
            if let Some(rest) = line.strip_prefix(prefix) {
                let name = rest
                    .split([' ', '('])
                    .next()
                    .unwrap_or("")
                    .trim_end_matches('.');
                if !name.is_empty() {
                    let name = name.rsplit('/').next().unwrap_or(name);
                    return Some(Reading::message(format!("{verb} {name}")));
                }
            }
        }
        Generic.line(line)
    }
}

/// dnf prints a table with `Installing  : name   3/7` rows during the
/// transaction, which is the one place its count is over the whole job.
pub struct Dnf;

impl ProgressParser for Dnf {
    fn line(&mut self, line: &str) -> Option<Reading> {
        let line = line.trim();
        for verb in [
            "Installing",
            "Upgrading",
            "Removing",
            "Reinstalling",
            "Downgrading",
        ] {
            let Some(rest) = line.strip_prefix(verb) else {
                continue;
            };
            let Some(rest) = rest.trim_start().strip_prefix(':') else {
                continue;
            };
            let mut fields = rest.split_whitespace();
            let Some(name) = fields.next() else { continue };
            let Some((done, total)) = fields.last().and_then(|f| f.split_once('/')) else {
                return Some(Reading::message(format!("{verb} {name}")));
            };
            if let (Ok(done), Ok(total)) = (done.parse::<u32>(), total.parse::<u32>())
                && total > 0
                && done <= total
            {
                return Some(Reading::at(done, total, format!("{verb} {name}")));
            }
            return Some(Reading::message(format!("{verb} {name}")));
        }
        match line {
            "Downloading Packages:" => Some(Reading::message("Downloading packages")),
            "Running transaction check" | "Running transaction test" => {
                Some(Reading::message("Checking the transaction"))
            }
            "Running transaction" => Some(Reading::message("Applying changes")),
            _ => Generic.line(line),
        }
    }
}

/// A line that ends in `...` is a phase announcement in every tool this
/// store runs, so it becomes a sentence; anything else is a plain log line.
pub struct Generic;

impl ProgressParser for Generic {
    fn line(&mut self, line: &str) -> Option<Reading> {
        let line = line.trim().trim_start_matches(":: ");
        let text = line.strip_suffix("...")?;
        if text.is_empty() || text.len() > 80 {
            return None;
        }
        Some(Reading::message(sentence(text)))
    }
}

/// `checking keyring` to `Checking keyring`, dropping a trailing `...`.
fn sentence(text: &str) -> String {
    let text = text.trim().trim_end_matches('.');
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(parser: &mut dyn ProgressParser, line: &str) -> Option<Reading> {
        parser.line(line)
    }

    #[test]
    fn pacman_counts_packages_and_names_phases() {
        let mut p = Pacman;
        let table: &[(&str, Option<f32>, Option<&str>)] = &[
            (
                "(3/7) installing foo",
                Some(3.0 / 7.0),
                Some("Installing foo"),
            ),
            (
                "(1/2) upgrading linux-cachyos",
                Some(0.5),
                Some("Upgrading linux-cachyos"),
            ),
            ("(2/2) removing steam", Some(1.0), Some("Removing steam")),
            (
                "(1/1) reinstalling bash",
                Some(1.0),
                Some("Reinstalling bash"),
            ),
            (
                "(1/3) checking keys in keyring",
                None,
                Some("Checking keys in keyring"),
            ),
            (
                "(2/5) checking package integrity",
                None,
                Some("Checking package integrity"),
            ),
            (
                "(1/4) Arming ConditionNeedsUpdate...",
                None,
                Some("Arming ConditionNeedsUpdate"),
            ),
            (
                " foo-1.2.3-1-x86_64 downloading...",
                None,
                Some("Downloading foo"),
            ),
            (
                "lib32-mesa-24.0.1-2-x86_64 downloading...",
                None,
                Some("Downloading lib32-mesa"),
            ),
            ("steam downloading...", None, Some("Downloading steam")),
            (
                ":: Synchronizing package databases...",
                None,
                Some("Synchronising package databases"),
            ),
            (
                "resolving dependencies...",
                None,
                Some("Resolving dependencies"),
            ),
            (
                ":: Retrieving packages...",
                None,
                Some("Downloading packages"),
            ),
            (
                ":: Running post-transaction hooks...",
                None,
                Some("Running post-transaction hooks"),
            ),
            ("warning: foo is up to date", None, None),
            ("Total Installed Size:  12.34 MiB", None, None),
            ("", None, None),
        ];
        for (line, fraction, message) in table {
            let reading = read(&mut p, line);
            match (fraction, message) {
                (None, None) => assert_eq!(reading, None, "{line:?}"),
                _ => {
                    let reading = reading.unwrap_or_else(|| panic!("{line:?} should be read"));
                    match (fraction, reading.fraction) {
                        (Some(want), Some(got)) => {
                            assert!((want - got).abs() < 1e-6, "{line:?}: {got} is not {want}")
                        }
                        (None, None) => {}
                        _ => panic!(
                            "{line:?}: fraction {:?}, wanted {:?}",
                            reading.fraction, fraction
                        ),
                    }
                    assert_eq!(reading.message.as_deref(), *message, "{line:?}");
                }
            }
        }
    }

    #[test]
    fn a_count_past_the_total_is_not_a_fraction() {
        assert_eq!(counted("(8/7) installing foo"), None);
        assert_eq!(counted("(1/0) installing foo"), None);
        assert_eq!(counted("(a/b) installing foo"), None);
    }

    #[test]
    fn apt_names_what_it_is_doing() {
        let mut p = Apt;
        let table: &[(&str, Option<f32>, Option<&str>)] = &[
            (
                "Reading package lists...",
                None,
                Some("Reading package lists"),
            ),
            (
                "Building dependency tree...",
                None,
                Some("Building dependency tree"),
            ),
            (
                "Get:1 http://deb.debian.org/debian bookworm/main amd64 steam amd64 1.0.0.78-2 [1234 kB]",
                None,
                Some("Downloading steam"),
            ),
            ("Selecting previously unselected package steam.", None, None),
            (
                "Preparing to unpack .../steam_1.0.0.78-2_amd64.deb ...",
                None,
                Some("Preparing to unpack steam_1.0.0.78-2_amd64.deb"),
            ),
            (
                "Unpacking steam (1.0.0.78-2) ...",
                None,
                Some("Unpacking steam"),
            ),
            (
                "Setting up steam (1.0.0.78-2) ...",
                None,
                Some("Setting up steam"),
            ),
            (
                "Removing steam (1.0.0.78-2) ...",
                None,
                Some("Removing steam"),
            ),
            (
                "Processing triggers for man-db (2.11.2-2) ...",
                None,
                Some("Processing triggers for man-db"),
            ),
            ("Progress: [ 20%]", Some(0.2), None),
            ("Progress: [100%]", Some(1.0), None),
            ("Fetched 1234 kB in 1s (1000 kB/s)", None, None),
        ];
        for (line, fraction, message) in table {
            let reading = read(&mut p, line);
            match (fraction, message) {
                (None, None) => assert_eq!(reading, None, "{line:?}"),
                _ => {
                    let reading = reading.unwrap_or_else(|| panic!("{line:?} should be read"));
                    assert_eq!(reading.fraction, *fraction, "{line:?}");
                    assert_eq!(reading.message.as_deref(), *message, "{line:?}");
                }
            }
        }
    }

    #[test]
    fn dnf_reads_its_transaction_table() {
        let mut p = Dnf;
        let r = read(
            &mut p,
            "  Installing       : steam-1.0.0.78-2.fc40.x86_64            3/7",
        )
        .unwrap();
        assert!((r.fraction.unwrap() - 3.0 / 7.0).abs() < 1e-6);
        assert_eq!(
            r.message.as_deref(),
            Some("Installing steam-1.0.0.78-2.fc40.x86_64")
        );
        let r = read(
            &mut p,
            "  Upgrading        : firefox-128.0-1.fc40.x86_64             1/2",
        )
        .unwrap();
        assert_eq!(r.fraction, Some(0.5));
        assert_eq!(
            read(&mut p, "Downloading Packages:")
                .unwrap()
                .message
                .as_deref(),
            Some("Downloading packages")
        );
        assert_eq!(read(&mut p, "Complete!"), None);
    }

    #[test]
    fn the_generic_parser_only_reads_phase_lines() {
        let mut p = Generic;
        assert_eq!(
            read(&mut p, "resolving things..."),
            Some(Reading::message("Resolving things"))
        );
        assert_eq!(
            read(&mut p, ":: doing more..."),
            Some(Reading::message("Doing more"))
        );
        assert_eq!(read(&mut p, "..."), None);
        assert_eq!(read(&mut p, "a plain line"), None);
        let long = format!("{}...", "x".repeat(100));
        assert_eq!(read(&mut p, &long), None);
    }

    #[test]
    fn parser_for_falls_back_to_generic() {
        let mut p = parser_for("makepkg");
        assert_eq!(p.line("==> Making package: foo 1.0-1 (Wed)"), None);
        let mut p = parser_for("pacman");
        assert!(p.line("(1/1) installing foo").unwrap().fraction.is_some());
        let mut f = FnParser(|line| line.strip_prefix("hook:").map(Reading::message));
        assert_eq!(f.line("hook:Pulling"), Some(Reading::message("Pulling")));
    }
}
