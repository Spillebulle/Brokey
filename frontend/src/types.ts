// The page's view of crates/brokey-core/src/model.rs, field for field. The Rust
// side is the truth: a field added there is added here in the same commit.
// Enums are serde `rename_all = "lowercase"` unless stated; tagged enums say
// which field carries the tag.

export type SourceKind =
  | "pacman"
  | "aur"
  | "flatpak"
  | "snap"
  | "apt"
  | "dnf"
  | "github"
  | "fwupd"
  | "chwd";

/** Interface order, which is also search order (`SourceKind::ALL`). */
export const SOURCE_KINDS: SourceKind[] = [
  "pacman",
  "aur",
  "flatpak",
  "snap",
  "apt",
  "dnf",
  "github",
  "fwupd",
  "chwd",
];

/** What a badge says (`SourceKind::label`). Neutral words; colour is never per source. */
export function sourceLabel(kind: SourceKind): string {
  switch (kind) {
    case "pacman":
      return "pacman";
    case "aur":
      return "AUR";
    case "flatpak":
      return "Flatpak";
    case "snap":
      return "Snap";
    case "apt":
      return "apt";
    case "dnf":
      return "dnf";
    case "github":
      return "GitHub";
    case "fwupd":
      return "Firmware";
    case "chwd":
      return "Drivers";
  }
}

export type PackageKind =
  | "app"
  | "package"
  | "runtime"
  | "font"
  | "addon"
  | "driver"
  | "firmware";

export interface PackageRef {
  source: SourceKind;
  id: string;
}

/** `#[serde(tag = "kind", content = "value")]`: a URL the page fetches, or a file served through the asset protocol. */
export type Picture = { kind: "url"; value: string } | { kind: "file"; value: string };

export interface Screenshot {
  image: Picture;
  thumbnail: Picture | null;
  caption: string | null;
  width: number | null;
  height: number | null;
}

export interface Package {
  source: SourceKind;
  id: string;
  name: string;
  kind: PackageKind;
  summary: string | null;
  /** AppStream markup (p, ul, ol, li, em, code) or plain text. `Description` sanitises it. */
  description: string | null;
  version: string | null;
  installed_version: string | null;
  installed: boolean;
  repo: string | null;
  licence: string | null;
  homepage: string | null;
  developer: string | null;
  /** Unix seconds. */
  updated: number | null;
  download_size: number | null;
  installed_size: number | null;
  /** 0 to 1 within the source, for sorting only. */
  popularity: number | null;
  popularity_label: string | null;
  icon: Picture | null;
  screenshots: Screenshot[];
  categories: string[];
  appstream_id: string | null;
  out_of_date: boolean;
  sandboxed: boolean;
  /** Key/value facts in draw order. A Rust tuple is a two-element array in JSON. */
  facts: [string, string][];
}

export type MatchedBy = "appstream" | "name" | "alone";

export interface Edition {
  package: Package;
  matched_by: MatchedBy;
  /** 1 for an AppStream join, lower for a name join; the page says "matched by name" under 1. */
  confidence: number;
}

export interface App {
  key: string;
  name: string;
  kind: PackageKind;
  summary: string | null;
  icon: Picture | null;
  developer: string | null;
  categories: string[];
  installed: boolean;
  updated: number | null;
  popularity: number | null;
  relevance: number;
  editions: Edition[];
}

export interface Update {
  package: PackageRef;
  name: string;
  kind: PackageKind;
  summary: string | null;
  icon: Picture | null;
  from: string | null;
  to: string;
  download_size: number | null;
  published: number | null;
  is_self: boolean;
}

/** How a source whose tool is missing gets it: the button's word and the sentence behind it. */
export interface SourceSetup {
  /** "Install Flatpak", "Install snapd". */
  label: string;
  /** "Installs Flatpak and adds Flathub, then Flatpak applications can be installed and updated here." */
  sentence: string;
}

export interface SourceStatus {
  kind: SourceKind;
  available: boolean;
  reason: string | null;
  detail: string | null;
  /** The source answers a search through its public store even when `available` is false. */
  searchable: boolean;
  /** How to make the source available (a `setup` op), or null when nothing here can. */
  setup: SourceSetup | null;
}

/**
 * `#[serde(tag = "op", rename_all = "lowercase")]`: `UpdateAll` lower-cases to
 * `updateall`. `setup` installs the source's tool (Flatpak and Flathub, snapd)
 * so the installs after it in the same plan can run.
 */
export type Op =
  | { op: "install"; package: PackageRef }
  | { op: "remove"; package: PackageRef }
  | { op: "update"; package: PackageRef }
  | { op: "updateall"; source: SourceKind }
  | { op: "refresh"; source: SourceKind }
  | { op: "setup"; source: SourceKind };

export interface Command {
  program: string;
  args: string[];
  env: [string, string][];
  cwd: string | null;
}

export interface Step {
  source: SourceKind;
  title: string;
  command: Command;
  needs_root: boolean;
  weight: number;
}

export interface Plan {
  id: string;
  ops: Op[];
  steps: Step[];
}

/** `#[serde(tag = "event", rename_all = "snake_case")]`. `fraction` is null whenever the total is not known. */
export type Event =
  | { event: "plan_started"; plan: string; steps: number }
  | { event: "auth_required"; plan: string }
  | { event: "step_started"; plan: string; step: number; title: string }
  | {
      event: "progress";
      plan: string;
      step: number;
      fraction: number | null;
      message: string | null;
    }
  | { event: "log"; plan: string; step: number; line: string; stderr: boolean }
  | {
      event: "step_finished";
      plan: string;
      step: number;
      ok: boolean;
      message: string | null;
    }
  | { event: "plan_finished"; plan: string; ok: boolean; message: string };

export interface SystemInfo {
  distro_id: string;
  distro_like: string[];
  pretty_name: string;
  arch: string;
  desktop: string | null;
  session: string | null;
  platform: "linux" | "windows";
}

export interface DriverProfile {
  id: string;
  name: string;
  description: string | null;
  installed: boolean;
  recommended: boolean;
  packages: string[];
}

export interface DriverDevice {
  id: string;
  name: string;
  vendor: string | null;
  class: string | null;
  profiles: DriverProfile[];
}

export interface FirmwareDevice {
  id: string;
  name: string;
  vendor: string | null;
  version: string | null;
  update_version: string | null;
  update_summary: string | null;
  update_size: number | null;
  needs_reboot: boolean;
}

export interface DriversReport {
  manager: string | null;
  manager_note: string | null;
  devices: DriverDevice[];
  firmware_available: boolean;
  firmware_note: string | null;
  firmware: FirmwareDevice[];
}

// ── What the commands exchange (brokey_core::Query, SearchResult, updates::UpdateList,
//    and the shapes crates/brokey/src/commands.rs returns) ─────────────────

export interface Query {
  /** Editions split out of their rows (`source:id`); the application fills it from settings when absent. */
  split?: string[];
  text: string;
  /** null means every available source. */
  sources: SourceKind[] | null;
  /** Per source. */
  limit: number;
}

export interface SearchResult {
  apps: App[];
  /** A failed source is never silent: the kind and the sentence it failed with. */
  failed: [SourceKind, string][];
  searched: SourceKind[];
}

export interface UpdateList {
  updates: Update[];
  failed: [SourceKind, string][];
  /** Unix seconds when the list was made. */
  checked_at: number;
}

/** What `plan(ops)` returns before anything runs: the steps and any sentence the user should read first (the Arch partial-upgrade notice, an AUR build that will take a while). */
export interface PlanPreview {
  plan: Plan;
  notices: string[];
}

export type PlanState =
  | "pending"
  | "authorising"
  | "running"
  | "done"
  | "failed"
  | "cancelled";

export interface PlanStatus {
  plan: Plan;
  state: PlanState;
  events: Event[];
  /** Unix seconds when the plan was started. */
  started: number;
}

export type Theme = "dark" | "light" | "system";

export type AurHelper = "auto" | "paru" | "yay" | "builtin";

export type FlatpakScope = "system" | "user";

export interface Settings {
  theme: Theme;
  enabled_sources: SourceKind[];
  /** Show plain packages (libraries, tools) in search results, not only applications. */
  show_packages: boolean;
  aur_helper: AurHelper;
  flatpak_scope: FlatpakScope;
  check_updates_on_start: boolean;
  self_update_check: boolean;
  update_check_minutes: number;
  /** App keys the user has split apart because the name match was wrong. */
  split: string[];
}

export interface SelfUpdateRelease {
  version: string;
  /** Markdown, from the release body. */
  notes: string;
  /** Unix seconds. */
  published: number | null;
  url: string;
  /** Newer than this copy; `remedy` is set exactly when it is. */
  newer: boolean;
}

/**
 * How this copy was installed (`#[serde(tag = "kind", rename_all = "lowercase")]`):
 * flatpak, appimage (with its path), pacman (with the package, brokey or
 * brokey-bin), dpkg and rpm (with whether the Spillebulle archive is set up),
 * portable, unknown. `SelfUpdate.installation_label` is the sentence to draw.
 */
export interface SelfUpdateInstallation {
  kind: "flatpak" | "appimage" | "pacman" | "dpkg" | "rpm" | "portable" | "unknown";
  path?: string;
  package?: string;
  archive?: boolean;
}

export type SelfUpdateInstaller = "pacmanu" | "dpkgi" | "rpmu" | "flatpakbundle";

/**
 * The one true thing to say about getting the new version (§18.3): never a
 * command that cannot work on this machine. Only install_asset and
 * replace_file are something Brokey runs (`self_update_apply`).
 */
export type SelfUpdateRemedy =
  | { kind: "updates_page"; source: SourceKind; package: string; sentence: string }
  | { kind: "install_asset"; asset: string; url: string; installer: SelfUpdateInstaller; sentence: string }
  | { kind: "replace_file"; path: string; asset: string; url: string; sentence: string }
  | { kind: "sentence"; sentence: string };

export interface SelfUpdate {
  current: string;
  /** Null when GitHub has no release yet, or could not be asked (then `error` says why). */
  latest: SelfUpdateRelease | null;
  installation: SelfUpdateInstallation;
  /** "a pacman package installed from a file", for Settings. */
  installation_label: string;
  remedy: SelfUpdateRemedy | null;
  /** Why GitHub could not be asked, in a sentence. */
  error: string | null;
}
