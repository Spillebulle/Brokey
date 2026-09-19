//! The contract between the Rust side and the page, checked.
//!
//! Every value a command returns or an event carries crosses to the page as
//! JSON, and the page reads it through the types in `frontend/src/types.ts`.
//! Nothing compiles the two against each other, and 0.1.0 shipped with the
//! self-update remedy serialised as `installasset` while the page tested for
//! `install_asset`: the Update button never appeared, and every test on both
//! sides passed, each checking its own spelling.
//!
//! So this test serialises a real value of every type the page receives and
//! walks it against the TypeScript declarations: every key the Rust side
//! writes must be declared, every key the page requires must be written, every
//! tagged union's tag must name a variant the page has, and every enum word
//! must be one the page lists. Nested values are followed through the field
//! types (`Picture | null`, `Screenshot[]`), so a shape two levels down is
//! held to the same rule.

use brokey_core::selfupdate::{Asset, Installation, Release, Version, assemble};
use brokey_core::updates::UpdateList;
use brokey_core::*;
use brokey_lib::commands::PlanPreview;
use brokey_lib::settings::Settings;
use brokey_lib::state::{PlanState, PlanStatus};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;

// ------------------------------------------------------------- the reader

/// What `types.ts` declares, reduced to what a JSON value can be checked
/// against.
enum Decl {
    /// `export interface X { a: T; b?: U }`: field name to type text and
    /// whether the page may go without it.
    Object(Vec<Field>),
    /// `export type X = { tag: "a"; ... } | { tag: "b"; ... }`.
    Tagged {
        tag: String,
        variants: HashMap<String, Vec<Field>>,
    },
    /// `export type X = "a" | "b"`.
    Words(Vec<String>),
}

struct Field {
    name: String,
    ty: String,
    optional: bool,
}

fn types_ts() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../frontend/src/types.ts");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

/// Comments out, so a word in a doc comment is never taken for a field.
fn strip_comments(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("/*") {
        out.push_str(&rest[..start]);
        rest = match rest[start..].find("*/") {
            Some(end) => &rest[start + end + 2..],
            None => "",
        };
    }
    out.push_str(rest);
    out.lines()
        .map(|l| match l.find("//") {
            Some(i) => &l[..i],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The fields of one `{ ... }` body, split at `;` or newline outside any
/// nested brackets.
fn fields(body: &str) -> Vec<Field> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut current = String::new();
    for c in body.chars() {
        match c {
            '{' | '[' | '(' | '<' => depth += 1,
            '}' | ']' | ')' | '>' => depth -= 1,
            _ => {}
        }
        if (c == ';' || c == '\n') && depth == 0 {
            parts.push(std::mem::take(&mut current));
        } else {
            current.push(c);
        }
    }
    parts.push(current);
    parts
        .into_iter()
        .filter_map(|p| {
            let p = p.trim();
            let (name, ty) = p.split_once(':')?;
            let name = name.trim();
            let optional = name.ends_with('?');
            let name = name.trim_end_matches('?').to_string();
            if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                return None;
            }
            Some(Field {
                name,
                ty: ty.trim().to_string(),
                optional,
            })
        })
        .collect()
}

/// The `{ ... }` chunks of a union at the top level.
fn object_chunks(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0;
    let mut start = 0;
    for (i, c) in text.char_indices() {
        match c {
            '{' => {
                if depth == 0 {
                    start = i + 1;
                }
                depth += 1;
            }
            '}' => {
                depth -= 1;
                if depth == 0 {
                    out.push(&text[start..i]);
                }
            }
            _ => {}
        }
    }
    out
}

fn parse(ts: &str) -> HashMap<String, Decl> {
    let ts = strip_comments(ts);
    let mut decls = HashMap::new();
    let mut rest = ts.as_str();
    while let Some(at) = rest.find("export ") {
        rest = &rest[at + "export ".len()..];
        if let Some(after) = rest.strip_prefix("interface ") {
            let name: String = after.chars().take_while(|c| c.is_alphanumeric()).collect();
            let open = after.find('{').expect("an interface has a body");
            let body = object_chunks(&after[open..])[0];
            decls.insert(name, Decl::Object(fields(body)));
        } else if let Some(after) = rest.strip_prefix("type ") {
            let name: String = after.chars().take_while(|c| c.is_alphanumeric()).collect();
            let eq = after.find('=').expect("a type alias has a value");
            let end = after[eq..]
                .find("\nexport ")
                .map(|i| eq + i)
                .unwrap_or(after.len());
            let value = &after[eq + 1..end];
            let chunks = object_chunks(value);
            if chunks.is_empty() {
                let words = value
                    .split('|')
                    .filter_map(|w| {
                        let w = w.trim().trim_end_matches(';').trim();
                        w.strip_prefix('"')
                            .and_then(|w| w.strip_suffix('"'))
                            .map(str::to_string)
                    })
                    .collect();
                decls.insert(name, Decl::Words(words));
            } else {
                let parsed: Vec<Vec<Field>> = chunks.iter().map(|c| fields(c)).collect();
                let tag = parsed[0][0].name.clone();
                let variants = parsed
                    .into_iter()
                    .map(|fs| {
                        let value = fs
                            .iter()
                            .find(|f| f.name == tag)
                            .map(|f| f.ty.trim_matches('"').to_string())
                            .unwrap_or_default();
                        (value, fs)
                    })
                    .collect();
                decls.insert(name, Decl::Tagged { tag, variants });
            }
        }
    }
    decls
}

// ------------------------------------------------------------ the checker

struct Checker {
    decls: HashMap<String, Decl>,
    errors: Vec<String>,
}

impl Checker {
    fn value<T: Serialize>(&mut self, ty: &str, value: &T) {
        let json = serde_json::to_value(value).expect("serialises");
        self.check(&json, ty, ty);
    }

    /// Check `json` against a field's type text at `path`.
    fn check(&mut self, json: &Value, ty: &str, path: &str) {
        let ty = ty.trim();
        // `T | null`: null is allowed only where the page says so.
        let alternatives: Vec<&str> = split_top(ty, '|');
        if alternatives.len() > 1 {
            let nullable = alternatives.iter().any(|a| a.trim() == "null");
            let rest: Vec<&str> = alternatives
                .iter()
                .copied()
                .filter(|a| a.trim() != "null")
                .collect();
            if json.is_null() {
                if !nullable {
                    self.errors.push(format!("{path}: the Rust side sent null, and the page's type {ty} does not allow it"));
                }
                return;
            }
            if rest.len() == 1 {
                return self.check(json, rest[0], path);
            }
            // A union of string literals written inline.
            if let Value::String(s) = json
                && rest.iter().all(|a| a.trim().starts_with('"'))
                && !rest.iter().any(|a| a.trim().trim_matches('"') == s)
            {
                self.errors
                    .push(format!("{path}: \"{s}\" is not one of {ty}"));
            }
            return;
        }
        if let Some(inner) = ty.strip_suffix("[]") {
            if let Value::Array(items) = json {
                for (i, item) in items.iter().enumerate() {
                    self.check(item, inner, &format!("{path}[{i}]"));
                }
            } else {
                self.errors
                    .push(format!("{path}: the page expects an array ({ty})"));
            }
            return;
        }
        if ty.starts_with('[') {
            // A tuple; its elements are checked where they name a declaration.
            if let Value::Array(items) = json {
                let inner = &ty[1..ty.len() - 1];
                for (item, elem_ty) in items.iter().zip(split_top(inner, ',')) {
                    self.check(item, elem_ty, path);
                }
            }
            return;
        }
        match ty {
            "string" | "number" | "boolean" | "unknown" => return,
            _ => {}
        }
        let Some(decl) = self.decls.get(ty) else {
            return;
        };
        match decl {
            Decl::Words(words) => {
                if let Value::String(s) = json
                    && !words.contains(s)
                {
                    self.errors.push(format!(
                        "{path}: the Rust side sent \"{s}\", and {ty} in types.ts only has {}",
                        words
                            .iter()
                            .map(|w| format!("\"{w}\""))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
            }
            Decl::Object(fields) => {
                let fields: Vec<(String, String, bool)> = fields
                    .iter()
                    .map(|f| (f.name.clone(), f.ty.clone(), f.optional))
                    .collect();
                self.object(json, &fields, ty, path);
            }
            Decl::Tagged { tag, variants } => {
                let tag = tag.clone();
                let Some(value) = json.get(&tag).and_then(Value::as_str) else {
                    self.errors.push(format!("{path}: the page tells {ty} apart by \"{tag}\", which the Rust side did not send"));
                    return;
                };
                let Some(fields) = variants.get(value) else {
                    self.errors.push(format!(
                        "{path}: the Rust side sent {tag} \"{value}\", and {ty} in types.ts only has {}",
                        variants.keys().map(|k| format!("\"{k}\"")).collect::<Vec<_>>().join(", ")
                    ));
                    return;
                };
                let fields: Vec<(String, String, bool)> = fields
                    .iter()
                    .filter(|f| f.name != tag)
                    .map(|f| (f.name.clone(), f.ty.clone(), f.optional))
                    .collect();
                let mut json = json.clone();
                if let Value::Object(map) = &mut json {
                    map.remove(&tag);
                }
                self.object(&json, &fields, &format!("{ty} \"{value}\""), path);
            }
        }
    }

    fn object(&mut self, json: &Value, fields: &[(String, String, bool)], ty: &str, path: &str) {
        let Value::Object(map) = json else {
            self.errors
                .push(format!("{path}: the page expects an object ({ty})"));
            return;
        };
        for key in map.keys() {
            if !fields.iter().any(|(name, _, _)| name == key) {
                self.errors.push(format!(
                    "{path}.{key}: the Rust side sends it, and {ty} in types.ts does not declare it"
                ));
            }
        }
        for (name, field_ty, optional) in fields {
            match map.get(name) {
                Some(v) => self.check(v, field_ty, &format!("{path}.{name}")),
                None if *optional => {}
                None => self.errors.push(format!("{path}.{name}: {ty} in types.ts requires it, and the Rust side does not send it")),
            }
        }
    }
}

/// Split at `sep` outside brackets and quotes.
fn split_top(text: &str, sep: char) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0;
    let mut quoted = false;
    let mut start = 0;
    for (i, c) in text.char_indices() {
        match c {
            '"' => quoted = !quoted,
            '{' | '[' | '(' | '<' if !quoted => depth += 1,
            '}' | ']' | ')' | '>' if !quoted => depth -= 1,
            c if c == sep && depth == 0 && !quoted => {
                out.push(&text[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&text[start..]);
    out.into_iter()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect()
}

// ------------------------------------------------------------ the samples

fn package() -> Package {
    let mut p = Package::new(
        SourceKind::Flatpak,
        "flathub/app/org.gimp.GIMP/x86_64/stable",
        "GIMP",
    );
    p.kind = PackageKind::App;
    p.summary = Some("Image editor".into());
    p.description = Some("<p>Paint.</p>".into());
    p.version = Some("3.2.4".into());
    p.installed_version = Some("3.2.2".into());
    p.installed = true;
    p.repo = Some("flathub".into());
    p.licence = Some("GPL-3.0".into());
    p.homepage = Some("https://gimp.org".into());
    p.developer = Some("GIMP".into());
    p.updated = Some(1);
    p.download_size = Some(2);
    p.installed_size = Some(3);
    p.popularity = Some(0.5);
    p.popularity_label = Some("5 installs".into());
    p.icon = Some(Picture::File("/i.png".into()));
    p.screenshots = vec![Screenshot {
        image: Picture::Url("https://x/1.png".into()),
        thumbnail: None,
        caption: Some("One".into()),
        width: Some(1),
        height: None,
    }];
    p.categories = vec!["Graphics".into()];
    p.appstream_id = Some("org.gimp.GIMP".into());
    p.facts = vec![("Runtime".into(), "GNOME".into())];
    p
}

fn reference() -> PackageRef {
    PackageRef {
        source: SourceKind::Pacman,
        id: "steam".into(),
    }
}

fn step() -> Step {
    Step {
        source: SourceKind::Pacman,
        title: "Installing steam".into(),
        command: Command {
            program: "pacman".into(),
            args: vec!["-S".into()],
            env: vec![("LC_ALL".into(), "C.UTF-8".into())],
            cwd: None,
        },
        needs_root: true,
        weight: 3,
    }
}

fn every_op() -> Vec<Op> {
    vec![
        Op::Install {
            package: reference(),
        },
        Op::Remove {
            package: reference(),
        },
        Op::Update {
            package: reference(),
        },
        Op::UpdateAll {
            source: SourceKind::Pacman,
        },
        Op::Refresh {
            source: SourceKind::Flatpak,
        },
        Op::Setup {
            source: SourceKind::Snap,
        },
    ]
}

fn plan() -> Plan {
    Plan {
        id: "plan-1".into(),
        ops: every_op(),
        steps: vec![step()],
    }
}

fn every_event() -> Vec<Event> {
    let plan = "plan-1".to_string();
    vec![
        Event::PlanStarted {
            plan: plan.clone(),
            steps: 1,
        },
        Event::AuthRequired { plan: plan.clone() },
        Event::StepStarted {
            plan: plan.clone(),
            step: 0,
            title: "t".into(),
        },
        Event::Progress {
            plan: plan.clone(),
            step: 0,
            fraction: None,
            message: Some("m".into()),
        },
        Event::Log {
            plan: plan.clone(),
            step: 0,
            line: "l".into(),
            stderr: false,
        },
        Event::StepFinished {
            plan: plan.clone(),
            step: 0,
            ok: true,
            message: None,
        },
        Event::PlanFinished {
            plan,
            ok: true,
            message: "Finished.".into(),
        },
    ]
}

fn release(version: &str) -> Release {
    Release {
        version: Version::parse(version).expect("a version"),
        tag: format!("v{version}"),
        name: version.to_string(),
        notes: "- Fixed".into(),
        published: Some(1),
        url: "https://github.com/Spillebulle/Brokey/releases/tag/v0.2.0".into(),
        assets: [
            "brokey-bin-0.2.0-1-x86_64.pkg.tar.zst",
            "brokey_0.2.0_amd64.deb",
            "brokey-0.2.0-1.x86_64.rpm",
            "Brokey-0.2.0-x86_64.AppImage",
        ]
        .iter()
        .map(|name| Asset {
            name: name.to_string(),
            browser_download_url: format!(
                "https://github.com/Spillebulle/Brokey/releases/download/v0.2.0/{name}"
            ),
            size: 1,
        })
        .collect(),
    }
}

#[test]
fn everything_the_page_receives_matches_types_ts() {
    let mut c = Checker {
        decls: parse(&types_ts()),
        errors: Vec::new(),
    };
    for name in [
        "SelfUpdate",
        "Package",
        "App",
        "Op",
        "Event",
        "SourceStatus",
        "Settings",
        "PlanStatus",
        "Picture",
    ] {
        assert!(
            c.decls.contains_key(name),
            "types.ts should declare {name}; the reader did not find it"
        );
    }

    c.value("Package", &package());
    c.value("PackageRef", &reference());
    c.value(
        "App",
        &App {
            key: "org.gimp.GIMP".into(),
            name: "GIMP".into(),
            kind: PackageKind::App,
            summary: None,
            icon: None,
            developer: None,
            categories: Vec::new(),
            installed: false,
            updated: None,
            popularity: None,
            relevance: 1.0,
            editions: vec![Edition {
                package: package(),
                matched_by: MatchedBy::Name,
                confidence: 0.8,
            }],
        },
    );
    for kind in [
        PackageKind::App,
        PackageKind::Package,
        PackageKind::Runtime,
        PackageKind::Font,
        PackageKind::Addon,
        PackageKind::Driver,
        PackageKind::Firmware,
    ] {
        c.value("PackageKind", &kind);
    }
    for kind in SourceKind::ALL {
        c.value("SourceKind", &kind);
    }
    for matched in [MatchedBy::AppStream, MatchedBy::Name, MatchedBy::Alone] {
        c.value("MatchedBy", &matched);
    }
    c.value(
        "Update",
        &Update {
            package: reference(),
            name: "Steam".into(),
            kind: PackageKind::App,
            summary: None,
            icon: Some(Picture::Url("https://x".into())),
            from: Some("1".into()),
            to: "2".into(),
            download_size: None,
            published: None,
            is_self: false,
        },
    );
    c.value(
        "SourceStatus",
        &SourceStatus {
            kind: SourceKind::Flatpak,
            available: false,
            reason: Some("Flatpak is not installed.".into()),
            detail: None,
            searchable: true,
            setup: Some(SourceSetup {
                label: "Install Flatpak".into(),
                sentence: "Installs Flatpak.".into(),
            }),
        },
    );
    for op in every_op() {
        c.value("Op", &op);
    }
    c.value("Plan", &plan());
    for event in every_event() {
        c.value("Event", &event);
    }
    c.value(
        "SystemInfo",
        &brokey_core::system::from_os_release("ID=cachyos\nID_LIKE=arch\n"),
    );
    // The Windows shape is checked on every runner, not only a Windows one.
    // A field the Linux half never fills is exactly the kind of thing that
    // would otherwise reach the page unchecked.
    c.value(
        "SystemInfo",
        &brokey_core::system::from_registry_version(
            &brokey_core::system::windows::RegistryVersion {
                product_name: Some("Windows 10 Pro".into()),
                edition_id: Some("Professional".into()),
                display_version: Some("26H1".into()),
                current_build: Some("28120".into()),
                ubr: Some(2738),
            },
        ),
    );
    c.value(
        "DriversReport",
        &DriversReport {
            manager: Some("chwd".into()),
            manager_note: None,
            devices: vec![DriverDevice {
                id: "0000:01:00.0".into(),
                name: "GPU".into(),
                vendor: Some("NVIDIA".into()),
                class: None,
                profiles: vec![DriverProfile {
                    id: "nvidia-open-dkms".into(),
                    name: "nvidia-open-dkms".into(),
                    description: None,
                    installed: true,
                    recommended: true,
                    packages: vec!["nvidia-utils".into()],
                }],
            }],
            firmware_available: true,
            firmware_note: None,
            firmware: vec![FirmwareDevice {
                id: "x".into(),
                name: "UEFI dbx".into(),
                vendor: None,
                version: Some("1".into()),
                update_version: Some("2".into()),
                update_summary: None,
                update_size: Some(1),
                needs_reboot: true,
            }],
        },
    );
    c.value("Query", &Query::new("steam"));
    c.value(
        "SearchResult",
        &SearchResult {
            apps: Vec::new(),
            failed: vec![(SourceKind::Aur, "The AUR did not answer.".into())],
            searched: vec![SourceKind::Pacman],
        },
    );
    c.value(
        "UpdateList",
        &UpdateList {
            updates: Vec::new(),
            failed: Vec::new(),
            checked_at: 1,
        },
    );
    c.value(
        "PlanPreview",
        &PlanPreview {
            plan: plan(),
            notices: vec!["Note.".into()],
        },
    );
    for state in [
        PlanState::Running,
        PlanState::Done,
        PlanState::Failed,
        PlanState::Cancelled,
    ] {
        c.value(
            "PlanStatus",
            &PlanStatus {
                plan: plan(),
                state,
                events: every_event(),
                started: 1,
            },
        );
    }
    c.value("Settings", &Settings::default());

    // The self-update answer in every shape it takes: nothing newer, and a
    // newer release for each way a copy can be installed, which between them
    // produce every remedy.
    let current = Version::parse("0.1.0").unwrap();
    let installations = [
        Installation::Flatpak,
        Installation::AppImage {
            path: "/home/me/Brokey.AppImage".into(),
        },
        Installation::Pacman {
            package: "brokey".into(),
        },
        Installation::Pacman {
            package: "brokey-bin".into(),
        },
        Installation::Dpkg { archive: true },
        Installation::Dpkg { archive: false },
        Installation::Rpm { archive: false },
        Installation::Portable,
        Installation::Unknown,
    ];
    let mut kinds = std::collections::BTreeSet::new();
    for installation in installations {
        let up_to_date = assemble(
            &current,
            Some(&release("0.1.0")),
            installation.clone(),
            "x86_64",
            None,
        );
        c.value("SelfUpdate", &up_to_date);
        let newer = assemble(
            &current,
            Some(&release("0.2.0")),
            installation,
            "x86_64",
            None,
        );
        if let Some(remedy) = serde_json::to_value(&newer)
            .unwrap()
            .get("remedy")
            .and_then(|r| r.get("kind"))
            .and_then(Value::as_str)
        {
            kinds.insert(remedy.to_string());
        }
        c.value("SelfUpdate", &newer);
    }
    let failed = assemble(
        &current,
        None,
        Installation::Unknown,
        "x86_64",
        Some("GitHub did not answer.".into()),
    );
    c.value("SelfUpdate", &failed);
    assert_eq!(
        kinds.len(),
        4,
        "the samples should produce all four remedies, and produced {kinds:?}"
    );

    assert!(
        c.errors.is_empty(),
        "the Rust side and frontend/src/types.ts disagree:\n  {}",
        c.errors.join("\n  ")
    );
}
