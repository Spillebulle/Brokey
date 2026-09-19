// A believable machine for the page to run against in a plain browser: the
// development machine itself (CachyOS, pacman, paru, no Flatpak, no snapd),
// thirty applications with editions across sources, updates, a plan runner
// that talks the way the real one does. Icons and screenshots are the real
// Flathub URLs so screenshots of the page show real pictures. Flatpak and Snap
// are searchable through their public stores and carry a setup, so a search
// lists their editions and an install sets the tool up first.
//
// Nothing here is reachable inside the window: api.ts only imports this module
// when `__TAURI_INTERNALS__` is absent.

import { FLATHUB, type FlathubEntry } from "./mock-flathub";
import type {
  App,
  DriversReport,
  Edition,
  Event,
  Op,
  Package,
  PackageKind,
  PackageRef,
  Plan,
  PlanPreview,
  PlanStatus,
  Query,
  Screenshot,
  SearchResult,
  SelfUpdate,
  Settings,
  SourceKind,
  SourceStatus,
  Step,
  SystemInfo,
  Update,
  UpdateList,
} from "./types";
import { sourceLabel } from "./types";

const NOW = Math.floor(Date.now() / 1000);
const DAY = 86400;
const MB = 1000 * 1000;

// ── Browser switches ────────────────────────────────────────────────────────
//
// Only a browser has a query string, and only a browser loads this module.
//   ?fast               every delay is zero (screenshots, tools/shots.mjs)
//   ?hold               the plan runner pauses part way through its first step
//   ?hold=auth          pauses while waiting for the password
//   ?hold=unknown       pauses inside a step that reports no fraction (an AUR build)
//   ?selfupdate=asset   the release check answers an install_asset remedy
//   ?selfupdate=none    this copy is the newest, so no notice
// A plan whose operations name a package called "fail-please" fails at that
// step; one naming "no-such-package" cannot be planned at all, so the confirm
// dialog shows what api.plan failing looks like.

const PARAMS = new URLSearchParams(window.location.search);
const FAST = PARAMS.has("fast");
const HOLD: string | null = PARAMS.get("hold");
const FAIL_ID = "fail-please";
const UNPLANNABLE_ID = "no-such-package";

function delay(ms: number): Promise<void> {
  return new Promise((resolve) => window.setTimeout(resolve, FAST ? 0 : ms));
}

/** Never resolves: the page is left at this point for a picture. */
function hold(): Promise<void> {
  return new Promise(() => undefined);
}

// ── The machine ─────────────────────────────────────────────────────────────

const SYSTEM: SystemInfo = {
  distro_id: "cachyos",
  distro_like: ["arch"],
  pretty_name: "CachyOS",
  arch: "x86_64",
  desktop: "COSMIC",
  session: "wayland",
  platform: "linux",
};

const SOURCES: SourceStatus[] = [
  { kind: "pacman", available: true, reason: null, detail: "core, extra, multilib, cachyos", searchable: true, setup: null },
  { kind: "aur", available: true, reason: null, detail: "paru 2.1.0", searchable: true, setup: null },
  {
    kind: "flatpak",
    available: false,
    reason: "Flatpak is not installed. Its applications are listed in search, and installing one sets Flatpak up first.",
    detail: null,
    searchable: true,
    setup: {
      label: "Install Flatpak",
      sentence: "Installs Flatpak and adds Flathub, then Flatpak applications can be installed and updated here.",
    },
  },
  {
    kind: "snap",
    available: false,
    reason: "snapd is not installed. Snap applications are listed in search, and installing one sets snapd up first.",
    detail: null,
    searchable: true,
    setup: {
      label: "Install snapd",
      sentence: "Installs snapd and starts its service, then Snap applications can be installed and updated here. snapd comes from the AUR.",
    },
  },
  {
    kind: "apt",
    available: false,
    reason: "apt is for Debian-based systems; this is CachyOS.",
    detail: null,
    searchable: false,
    setup: null,
  },
  {
    kind: "dnf",
    available: false,
    reason: "dnf is for Fedora-based systems; this is CachyOS.",
    detail: null,
    searchable: false,
    setup: null,
  },
  { kind: "github", available: true, reason: null, detail: "releases", searchable: true, setup: null },
  { kind: "fwupd", available: true, reason: null, detail: "fwupdmgr 2.1.7", searchable: false, setup: null },
  { kind: "chwd", available: true, reason: null, detail: "chwd", searchable: false, setup: null },
];

/** Formats set up while this mock "session" runs; ?notice=flatpak starts with one. */
const setUpThisSession = new Set<SourceKind>(
  (PARAMS.get("notice") ?? "")
    .split(",")
    .filter((k): k is SourceKind => k === "flatpak" || k === "snap"),
);

function available(kind: SourceKind): boolean {
  return SOURCES.some((s) => s.kind === kind && s.available);
}

/** Answers a search: the tool is here, or the source's public store answers without it. */
function searchable(kind: SourceKind): boolean {
  return SOURCES.some((s) => s.kind === kind && (s.available || s.searchable));
}

/** What a finished setup makes of the source: the tool is here and the setup is spent. */
function markAvailable(kind: SourceKind) {
  const status = SOURCES.find((s) => s.kind === kind);
  if (!status) return;
  // Set up during this session, so a real desktop would not list its applications until the next login.
  if (kind === "flatpak" || kind === "snap") setUpThisSession.add(kind);
  status.available = true;
  status.reason = null;
  status.detail = kind === "flatpak" ? "flathub" : kind === "snap" ? "snapd 2.72" : status.detail;
  status.setup = null;
}

// ── The applications ────────────────────────────────────────────────────────

interface PacmanSpec {
  id: string;
  repo: "extra" | "multilib" | "cachyos";
  version: string;
  installed?: boolean;
  update?: string;
  download: number;
  size: number;
}

interface AurSpec {
  id: string;
  version: string;
  installed?: boolean;
  update?: string;
  votes: number;
  outOfDate?: boolean;
  matchedByName?: boolean;
}

interface FlatpakSpec {
  installed?: boolean;
  update?: string;
  download: number;
  size: number;
  installs: number;
}

interface SnapSpec {
  id: string;
  version: string;
}

interface GithubSpec {
  repo: string;
  version: string;
  installed?: boolean;
  update?: string;
  asset: string;
  size: number;
}

interface AppSpec {
  key: string;
  flathub?: FlathubEntry;
  pacman?: PacmanSpec;
  aur?: AurSpec;
  flatpak?: FlatpakSpec;
  snap?: SnapSpec;
  github?: GithubSpec;
  popularity: number;
  daysAgo: number;
}

const MULLVAD: FlathubEntry = {
  name: "Mullvad VPN",
  summary: "Privacy is a universal right",
  description:
    "<p>The Mullvad VPN client. Connect to WireGuard servers in 40 countries, with DNS content blockers, split tunnelling and a kill switch.</p><ul><li>No account details: a numbered account and nothing else.</li><li>Multihop and DAITA against traffic analysis.</li></ul>",
  icon: "",
  developer: "Mullvad VPN AB",
  categories: ["Network", "Security"],
  licence: "GPL-3.0-only",
  homepage: "https://mullvad.net",
  version: "2026.6",
  updated: NOW - 9 * DAY,
  screenshots: [],
};

const fh = (id: string): FlathubEntry => FLATHUB[id];

const SPECS: AppSpec[] = [
  {
    key: "com.valvesoftware.Steam",
    flathub: fh("com.valvesoftware.Steam"),
    pacman: { id: "steam", repo: "multilib", version: "1.0.0.85-1", installed: true, download: 3.4 * MB, size: 5.8 * MB },
    aur: { id: "steam-native-runtime", version: "1.0.0.85-1", votes: 412, matchedByName: true },
    flatpak: { download: 12.1 * MB, size: 41 * MB, installs: 214_530 },
    snap: { id: "steam", version: "1.0.0.85" },
    popularity: 0.98,
    daysAgo: 12,
  },
  {
    key: "org.gimp.GIMP",
    flathub: fh("org.gimp.GIMP"),
    pacman: { id: "gimp", repo: "extra", version: "3.2.6-1", download: 34 * MB, size: 152 * MB },
    aur: { id: "gimp-devel", version: "3.3.1-1", votes: 88, matchedByName: true },
    flatpak: { installed: true, download: 121 * MB, size: 412 * MB, installs: 98_410 },
    snap: { id: "gimp", version: "3.2.6" },
    popularity: 0.9,
    daysAgo: 4,
  },
  {
    key: "org.mozilla.firefox",
    flathub: fh("org.mozilla.firefox"),
    pacman: { id: "firefox", repo: "extra", version: "155.0-1", installed: true, update: "155.0.1-1", download: 71 * MB, size: 262 * MB },
    aur: { id: "firefox-nightly-bin", version: "157.0a1-1", votes: 150, matchedByName: true },
    flatpak: { download: 96 * MB, size: 301 * MB, installs: 310_200 },
    snap: { id: "firefox", version: "155.0.1" },
    popularity: 0.97,
    daysAgo: 1,
  },
  {
    key: "org.videolan.VLC",
    flathub: fh("org.videolan.VLC"),
    pacman: { id: "vlc", repo: "extra", version: "3.0.23-2", download: 21 * MB, size: 79 * MB },
    flatpak: { download: 44 * MB, size: 132 * MB, installs: 188_900 },
    snap: { id: "vlc", version: "3.0.23" },
    popularity: 0.88,
    daysAgo: 30,
  },
  {
    key: "org.blender.Blender",
    flathub: fh("org.blender.Blender"),
    pacman: { id: "blender", repo: "extra", version: "17:5.2-1", download: 132 * MB, size: 480 * MB },
    aur: { id: "blender-bin", version: "5.2-1", votes: 210, matchedByName: true },
    flatpak: { download: 310 * MB, size: 980 * MB, installs: 76_300 },
    snap: { id: "blender", version: "5.2" },
    popularity: 0.86,
    daysAgo: 6,
  },
  {
    key: "org.inkscape.Inkscape",
    flathub: fh("org.inkscape.Inkscape"),
    pacman: { id: "inkscape", repo: "extra", version: "1.4.4-1", installed: true, download: 28 * MB, size: 190 * MB },
    flatpak: { download: 88 * MB, size: 290 * MB, installs: 62_700 },
    snap: { id: "inkscape", version: "1.4.4" },
    popularity: 0.8,
    daysAgo: 40,
  },
  {
    key: "com.discordapp.Discord",
    flathub: fh("com.discordapp.Discord"),
    pacman: { id: "discord", repo: "extra", version: "1.0.157-1", download: 78 * MB, size: 235 * MB },
    aur: { id: "discord-canary", version: "0.0.812-1", votes: 190, matchedByName: true },
    flatpak: { installed: true, update: "1.0.158", download: 82 * MB, size: 251 * MB, installs: 255_100 },
    snap: { id: "discord", version: "1.0.157" },
    popularity: 0.93,
    daysAgo: 2,
  },
  {
    key: "com.spotify.Client",
    flathub: fh("com.spotify.Client"),
    aur: { id: "spotify", version: "1.2.95.453-1", votes: 1257 },
    flatpak: { download: 110 * MB, size: 330 * MB, installs: 402_800 },
    snap: { id: "spotify", version: "1.2.95.453" },
    popularity: 0.92,
    daysAgo: 8,
  },
  {
    key: "com.obsproject.Studio",
    flathub: fh("com.obsproject.Studio"),
    pacman: { id: "obs-studio", repo: "extra", version: "32.2.2-1", installed: true, download: 14 * MB, size: 58 * MB },
    aur: { id: "obs-studio-git", version: "32.2.2.r14-1", votes: 61, matchedByName: true },
    flatpak: { download: 160 * MB, size: 520 * MB, installs: 143_000 },
    snap: { id: "obs-studio", version: "32.2.2" },
    popularity: 0.85,
    daysAgo: 5,
  },
  {
    key: "org.kde.kdenlive",
    flathub: fh("org.kde.kdenlive"),
    pacman: { id: "kdenlive", repo: "extra", version: "26.08.1-1", download: 21 * MB, size: 88 * MB },
    flatpak: { download: 240 * MB, size: 760 * MB, installs: 51_200 },
    snap: { id: "kdenlive", version: "26.08.1" },
    popularity: 0.7,
    daysAgo: 3,
  },
  {
    key: "org.libreoffice.LibreOffice",
    flathub: fh("org.libreoffice.LibreOffice"),
    pacman: { id: "libreoffice-fresh", repo: "extra", version: "26.8.0-1", installed: true, update: "26.8.0.3-1", download: 190 * MB, size: 690 * MB },
    aur: { id: "libreoffice-still", version: "26.2.6-1", votes: 34, matchedByName: true },
    flatpak: { download: 340 * MB, size: 1020 * MB, installs: 271_400 },
    snap: { id: "libreoffice", version: "26.8.0.3" },
    popularity: 0.9,
    daysAgo: 1,
  },
  {
    key: "org.kde.krita",
    flathub: fh("org.kde.krita"),
    pacman: { id: "krita", repo: "extra", version: "5.3.3-1", download: 96 * MB, size: 310 * MB },
    aur: { id: "krita-plus-bin", version: "5.3.3-1", votes: 27, matchedByName: true },
    flatpak: { download: 150 * MB, size: 480 * MB, installs: 84_600 },
    snap: { id: "krita", version: "5.3.3" },
    popularity: 0.78,
    daysAgo: 14,
  },
  {
    key: "org.mozilla.Thunderbird",
    flathub: fh("org.mozilla.Thunderbird"),
    pacman: { id: "thunderbird", repo: "extra", version: "140.10.1-1", installed: true, update: "140.10.2-1", download: 66 * MB, size: 240 * MB },
    flatpak: { download: 92 * MB, size: 280 * MB, installs: 96_100 },
    snap: { id: "thunderbird", version: "140.10.2esr" },
    popularity: 0.82,
    daysAgo: 2,
  },
  {
    key: "org.telegram.desktop",
    flathub: fh("org.telegram.desktop"),
    pacman: { id: "telegram-desktop", repo: "extra", version: "7.1.5-1", download: 38 * MB, size: 140 * MB },
    aur: { id: "telegram-desktop-bin", version: "7.1.5-1", votes: 230, matchedByName: true },
    flatpak: { installed: true, download: 52 * MB, size: 170 * MB, installs: 168_300 },
    snap: { id: "telegram-desktop", version: "7.1.5" },
    popularity: 0.87,
    daysAgo: 3,
  },
  {
    key: "org.signal.Signal",
    flathub: fh("org.signal.Signal"),
    pacman: { id: "signal-desktop", repo: "extra", version: "8.27.0-1", download: 98 * MB, size: 310 * MB },
    aur: { id: "signal-desktop-beta", version: "8.28.0.beta.1-1", votes: 44, matchedByName: true },
    flatpak: { download: 120 * MB, size: 360 * MB, installs: 141_700 },
    snap: { id: "signal-desktop", version: "8.27.0" },
    popularity: 0.84,
    daysAgo: 4,
  },
  {
    key: "com.bitwarden.desktop",
    flathub: fh("com.bitwarden.desktop"),
    pacman: { id: "bitwarden", repo: "extra", version: "2026.7.2-1", installed: true, update: "2026.8.0-1", download: 88 * MB, size: 270 * MB },
    flatpak: { download: 104 * MB, size: 320 * MB, installs: 73_900 },
    snap: { id: "bitwarden", version: "2026.8.0" },
    popularity: 0.79,
    daysAgo: 6,
  },
  {
    key: "com.usebottles.bottles",
    flathub: fh("com.usebottles.bottles"),
    aur: { id: "bottles", version: "67.3-1", votes: 156 },
    flatpak: { installed: true, download: 46 * MB, size: 160 * MB, installs: 132_600 },
    popularity: 0.81,
    daysAgo: 20,
  },
  {
    key: "net.lutris.Lutris",
    flathub: fh("net.lutris.Lutris"),
    pacman: { id: "lutris", repo: "extra", version: "0.5.22-1", installed: true, download: 4.2 * MB, size: 15 * MB },
    aur: { id: "lutris-git", version: "0.5.22.r30-1", votes: 72, matchedByName: true },
    flatpak: { download: 120 * MB, size: 400 * MB, installs: 158_200 },
    popularity: 0.83,
    daysAgo: 33,
  },
  {
    key: "com.heroicgameslauncher.hgl",
    flathub: fh("com.heroicgameslauncher.hgl"),
    aur: { id: "heroic-games-launcher-bin", version: "2.22.1-1", votes: 168 },
    flatpak: { download: 130 * MB, size: 410 * MB, installs: 121_900 },
    popularity: 0.8,
    daysAgo: 11,
  },
  {
    key: "net.davidotek.pupgui2",
    flathub: fh("net.davidotek.pupgui2"),
    aur: { id: "protonup-qt", version: "2.15.1-1", votes: 90 },
    flatpak: { installed: true, update: "2.16.0", download: 60 * MB, size: 190 * MB, installs: 95_400 },
    popularity: 0.72,
    daysAgo: 7,
  },
  {
    key: "dev.zed.Zed",
    flathub: fh("dev.zed.Zed"),
    pacman: { id: "zed", repo: "extra", version: "1.19.1-1", installed: true, update: "1.19.2-1", download: 76 * MB, size: 240 * MB },
    aur: { id: "zed-git", version: "1.19.2.r120-1", votes: 39, matchedByName: true },
    flatpak: { download: 98 * MB, size: 310 * MB, installs: 44_700 },
    popularity: 0.74,
    daysAgo: 1,
  },
  {
    key: "com.visualstudio.code",
    flathub: fh("com.visualstudio.code"),
    pacman: { id: "code", repo: "extra", version: "1.137.0-1", download: 92 * MB, size: 350 * MB },
    aur: { id: "visual-studio-code-bin", version: "1.136.1-1", installed: true, update: "1.137.0-1", votes: 1802 },
    flatpak: { download: 130 * MB, size: 420 * MB, installs: 380_500 },
    snap: { id: "code", version: "1.137.0" },
    popularity: 0.95,
    daysAgo: 1,
  },
  {
    key: "md.obsidian.Obsidian",
    flathub: fh("md.obsidian.Obsidian"),
    pacman: { id: "obsidian", repo: "extra", version: "1.13.7-1", installed: true, download: 84 * MB, size: 280 * MB },
    aur: { id: "obsidian-bin", version: "1.13.7-1", votes: 120, matchedByName: true },
    flatpak: { download: 100 * MB, size: 330 * MB, installs: 176_800 },
    snap: { id: "obsidian", version: "1.13.7" },
    popularity: 0.86,
    daysAgo: 9,
  },
  {
    key: "io.mpv.Mpv",
    flathub: fh("io.mpv.Mpv"),
    pacman: { id: "mpv", repo: "extra", version: "1:0.40.0-3", installed: true, update: "1:0.41.0-1", download: 1.9 * MB, size: 4.6 * MB },
    aur: { id: "mpv-git", version: "0.41.0.r45-1", votes: 250, matchedByName: true },
    flatpak: { download: 34 * MB, size: 110 * MB, installs: 87_300 },
    snap: { id: "mpv", version: "0.41.0" },
    popularity: 0.77,
    daysAgo: 2,
  },
  {
    key: "org.audacityteam.Audacity",
    flathub: fh("org.audacityteam.Audacity"),
    pacman: { id: "audacity", repo: "extra", version: "1:3.7.8-1", download: 12 * MB, size: 48 * MB },
    flatpak: { download: 58 * MB, size: 180 * MB, installs: 102_200 },
    snap: { id: "audacity", version: "3.7.8" },
    popularity: 0.75,
    daysAgo: 18,
  },
  {
    key: "org.godotengine.Godot",
    flathub: fh("org.godotengine.Godot"),
    pacman: { id: "godot", repo: "extra", version: "4.7.2-1", download: 48 * MB, size: 150 * MB },
    aur: { id: "godot-mono-bin", version: "4.7.2-1", votes: 41, matchedByName: true },
    flatpak: { download: 70 * MB, size: 220 * MB, installs: 66_500 },
    snap: { id: "godot", version: "4.7.2" },
    popularity: 0.73,
    daysAgo: 10,
  },
  {
    key: "org.prismlauncher.PrismLauncher",
    flathub: fh("org.prismlauncher.PrismLauncher"),
    pacman: { id: "prismlauncher", repo: "extra", version: "11.1.0-1", download: 6.5 * MB, size: 22 * MB },
    aur: { id: "prismlauncher-git", version: "11.1.0.r12-1", votes: 55, matchedByName: true },
    flatpak: { download: 40 * MB, size: 130 * MB, installs: 58_900 },
    popularity: 0.68,
    daysAgo: 15,
  },
  {
    key: "net.mullvad.MullvadVPN",
    flathub: MULLVAD,
    aur: { id: "mullvad-vpn-bin", version: "2026.5-1", installed: true, update: "2026.6-1", votes: 95 },
    github: { repo: "mullvad/mullvadvpn-app", version: "2026.6", asset: "MullvadVPN-2026.6_x86_64.rpm", size: 96 * MB },
    popularity: 0.66,
    daysAgo: 9,
  },
  {
    key: "im.riot.Riot",
    flathub: fh("im.riot.Riot"),
    pacman: { id: "element-desktop", repo: "extra", version: "1.12.27-1", download: 110 * MB, size: 340 * MB },
    flatpak: { download: 130 * MB, size: 400 * MB, installs: 49_800 },
    snap: { id: "element-desktop", version: "1.12.27" },
    popularity: 0.64,
    daysAgo: 5,
  },
  {
    key: "com.github.johnfactotum.Foliate",
    flathub: fh("com.github.johnfactotum.Foliate"),
    pacman: { id: "foliate", repo: "extra", version: "3.3.0-1", download: 1.1 * MB, size: 4.2 * MB },
    flatpak: { installed: true, update: "3.3.1", download: 8.5 * MB, size: 28 * MB, installs: 31_400 },
    popularity: 0.6,
    daysAgo: 22,
  },
];

// Plain packages: what a search shows when "Show packages" is on.
interface PlainSpec {
  id: string;
  repo: "core" | "extra" | "cachyos";
  version: string;
  summary: string;
  installed?: boolean;
  update?: string;
  download: number;
  size: number;
}

const PLAIN: PlainSpec[] = [
  { id: "linux-cachyos", repo: "cachyos", version: "7.1.3-2", summary: "The Linux CachyOS scheduler kernel with other patches and improvements", installed: true, update: "7.1.4-1", download: 190 * MB, size: 210 * MB },
  { id: "mesa", repo: "extra", version: "1:25.2.3-1", summary: "Open-source OpenGL drivers", installed: true, update: "1:25.2.4-1", download: 38 * MB, size: 170 * MB },
  { id: "nvidia-open-dkms", repo: "extra", version: "590.44.02-1", summary: "NVIDIA open kernel modules, DKMS", installed: true, download: 60 * MB, size: 210 * MB },
  { id: "pipewire", repo: "extra", version: "1:1.6.2-1", summary: "Low-latency audio/video router and processor", installed: true, download: 1.4 * MB, size: 5.1 * MB },
  { id: "ripgrep", repo: "extra", version: "15.1.0-1", summary: "A search tool that combines the usability of ag with the raw speed of grep", installed: true, download: 1.6 * MB, size: 4.9 * MB },
  { id: "fd", repo: "extra", version: "10.3.0-1", summary: "Simple, fast and user-friendly alternative to find", download: 0.9 * MB, size: 3.1 * MB },
  { id: "python", repo: "core", version: "3.14.1-1", summary: "The Python programming language", installed: true, download: 24 * MB, size: 110 * MB },
];

function picture(url: string | null | undefined): Package["icon"] {
  return url ? { kind: "url", value: url } : null;
}

function screenshots(entry: FlathubEntry): Screenshot[] {
  return entry.screenshots.map((s) => ({
    image: { kind: "url", value: s.image },
    thumbnail: { kind: "url", value: s.thumb },
    caption: s.caption,
    width: s.width || null,
    height: s.height || null,
  }));
}

function basePackage(source: SourceKind, id: string, entry: FlathubEntry, kind: PackageKind = "app"): Package {
  return {
    source,
    id,
    name: entry.name,
    kind,
    summary: entry.summary,
    description: entry.description,
    version: null,
    installed_version: null,
    installed: false,
    repo: null,
    licence: entry.licence,
    homepage: entry.homepage,
    developer: entry.developer,
    updated: entry.updated,
    download_size: null,
    installed_size: null,
    popularity: null,
    popularity_label: null,
    icon: picture(entry.icon),
    screenshots: screenshots(entry),
    categories: entry.categories,
    appstream_id: null,
    out_of_date: false,
    sandboxed: false,
    facts: [],
  };
}

function buildApp(spec: AppSpec): App {
  const entry = spec.flathub ?? MULLVAD;
  const updated = NOW - spec.daysAgo * DAY;
  const editions: Edition[] = [];

  if (spec.pacman) {
    const p = spec.pacman;
    const pkg = basePackage("pacman", p.id, entry);
    pkg.version = p.update ?? p.version;
    pkg.installed = Boolean(p.installed);
    pkg.installed_version = p.installed ? p.version : null;
    pkg.repo = p.repo;
    pkg.updated = updated;
    pkg.download_size = p.download;
    pkg.installed_size = p.size;
    pkg.popularity = spec.popularity;
    pkg.appstream_id = spec.key;
    pkg.facts = [
      ["Repository", p.repo],
      ["Packager", "CachyOS build service"],
      ["Build date", new Date(updated * 1000).toLocaleDateString("en-GB")],
    ];
    editions.push({ package: pkg, matched_by: "appstream", confidence: 1 });
  }
  if (spec.aur) {
    const a = spec.aur;
    const pkg = basePackage("aur", a.id, entry);
    pkg.version = a.update ?? a.version;
    pkg.installed = Boolean(a.installed);
    pkg.installed_version = a.installed ? a.version : null;
    pkg.repo = "aur";
    pkg.updated = updated - 2 * DAY;
    pkg.popularity = Math.min(1, a.votes / 2000);
    pkg.popularity_label = `${a.votes} votes`;
    pkg.out_of_date = Boolean(a.outOfDate);
    pkg.facts = [
      ["Maintainer", "spillebulle"],
      ["Votes", String(a.votes)],
      ["Popularity", (a.votes / 87).toFixed(2)],
      ["First submitted", "14 Mar 2021"],
    ];
    editions.push({
      package: pkg,
      matched_by: a.matchedByName ? "name" : "appstream",
      confidence: a.matchedByName ? 0.82 : 1,
    });
    if (!a.matchedByName) pkg.appstream_id = spec.key;
  }
  if (spec.flatpak) {
    const f = spec.flatpak;
    const pkg = basePackage("flatpak", `flathub/app/${spec.key}/x86_64/stable`, entry);
    // A machine without Flatpak has no Flatpak application on it, whatever the fixture says.
    const on = Boolean(f.installed) && available("flatpak");
    pkg.version = f.update ?? entry.version;
    pkg.installed = on;
    pkg.installed_version = on ? entry.version : null;
    pkg.repo = "flathub";
    pkg.updated = updated;
    pkg.download_size = f.download;
    pkg.installed_size = f.size;
    pkg.popularity = Math.min(1, f.installs / 400_000);
    pkg.popularity_label = `${f.installs.toLocaleString("en-GB").replace(/,/g, " ")} installs last month`;
    pkg.sandboxed = true;
    pkg.appstream_id = spec.key;
    pkg.facts = [
      ["Remote", "flathub"],
      ["Runtime", "org.freedesktop.Platform 25.08"],
      ["Installs last month", f.installs.toLocaleString("en-GB").replace(/,/g, " ")],
    ];
    editions.push({ package: pkg, matched_by: "appstream", confidence: 1 });
  }
  if (spec.snap) {
    const s = spec.snap;
    const pkg = basePackage("snap", s.id, entry);
    pkg.version = s.version;
    pkg.repo = "stable";
    pkg.updated = updated;
    pkg.sandboxed = true;
    pkg.appstream_id = spec.key;
    pkg.facts = [["Channel", "stable"]];
    editions.push({ package: pkg, matched_by: "appstream", confidence: 1 });
  }
  if (spec.github) {
    const g = spec.github;
    const pkg = basePackage("github", g.repo, entry);
    pkg.version = g.update ?? g.version;
    pkg.installed = Boolean(g.installed);
    pkg.installed_version = g.installed ? g.version : null;
    pkg.repo = "releases";
    pkg.updated = updated;
    pkg.download_size = g.size;
    pkg.facts = [
      ["Repository", g.repo],
      ["Release", g.version],
      ["Asset", g.asset],
    ];
    editions.push({ package: pkg, matched_by: "name", confidence: 0.7 });
  }
  if (editions.length === 1) editions[0].matched_by = "alone";

  return {
    key: spec.key,
    name: entry.name,
    kind: "app",
    summary: entry.summary,
    icon: picture(entry.icon),
    developer: entry.developer,
    categories: entry.categories,
    installed: editions.some((e) => e.package.installed),
    updated,
    popularity: spec.popularity,
    relevance: 0,
    editions,
  };
}

function buildPlain(spec: PlainSpec): App {
  const pkg: Package = {
    source: "pacman",
    id: spec.id,
    name: spec.id,
    kind: "package",
    summary: spec.summary,
    description: `<p>${spec.summary}.</p>`,
    version: spec.update ?? spec.version,
    installed_version: spec.installed ? spec.version : null,
    installed: Boolean(spec.installed),
    repo: spec.repo,
    licence: "GPL-2.0-only",
    homepage: null,
    developer: null,
    updated: NOW - 3 * DAY,
    download_size: spec.download,
    installed_size: spec.size,
    popularity: 0.3,
    popularity_label: null,
    icon: null,
    screenshots: [],
    categories: [],
    appstream_id: null,
    out_of_date: false,
    sandboxed: false,
    facts: [["Repository", spec.repo]],
  };
  return {
    key: `name:${spec.id}`,
    name: spec.id,
    kind: "package",
    summary: spec.summary,
    icon: null,
    developer: null,
    categories: [],
    installed: pkg.installed,
    updated: pkg.updated,
    popularity: 0.3,
    relevance: 0,
    editions: [{ package: pkg, matched_by: "alone", confidence: 1 }],
  };
}

const SELF_PACKAGE: Package = {
  source: "aur",
  id: "brokey-bin",
  name: "Brokey",
  kind: "app",
  summary: "One store for every place a Linux machine gets software",
  description:
    "<p>Search the distribution's repositories, the AUR, Flatpak, Snap and GitHub releases in one place, install through one flow, and keep drivers and firmware up to date.</p>",
  version: "0.2.0-1",
  installed_version: "0.1.0-1",
  installed: true,
  repo: "aur",
  licence: "GPL-3.0-or-later",
  homepage: "https://github.com/spillebulle/brokey",
  developer: "spillebulle",
  updated: NOW - 1 * DAY,
  download_size: 9.2 * MB,
  installed_size: 24 * MB,
  popularity: 0.1,
  popularity_label: "12 votes",
  icon: null,
  screenshots: [],
  categories: ["System", "PackageManager"],
  appstream_id: "io.github.spillebulle.brokey",
  out_of_date: false,
  sandboxed: false,
  facts: [
    ["Maintainer", "spillebulle"],
    ["Votes", "12"],
  ],
};

const APPS: App[] = [
  ...SPECS.map(buildApp),
  {
    key: "io.github.spillebulle.brokey",
    name: "Brokey",
    kind: "app",
    summary: SELF_PACKAGE.summary,
    icon: null,
    developer: "spillebulle",
    categories: SELF_PACKAGE.categories,
    installed: true,
    updated: SELF_PACKAGE.updated,
    popularity: 0.1,
    relevance: 0,
    editions: [{ package: SELF_PACKAGE, matched_by: "alone", confidence: 1 }],
  },
  ...PLAIN.map(buildPlain),
];

function allPackages(): Package[] {
  return APPS.flatMap((a) => a.editions.map((e) => e.package));
}

function findPackage(ref: PackageRef): Package | undefined {
  return allPackages().find((p) => p.source === ref.source && p.id === ref.id);
}

// ── Search ──────────────────────────────────────────────────────────────────

function score(app: App, text: string): number {
  const q = text.trim().toLowerCase();
  if (!q) return 0.5 + (app.popularity ?? 0) / 2;
  const name = app.name.toLowerCase();
  let s = 0;
  if (name === q) s = 1;
  else if (name.startsWith(q)) s = 0.9;
  else if (name.includes(q)) s = 0.75;
  else if (app.editions.some((e) => e.package.id.toLowerCase().includes(q))) s = 0.6;
  else if ((app.summary ?? "").toLowerCase().includes(q)) s = 0.45;
  else if (app.categories.some((c) => c.toLowerCase().includes(q))) s = 0.3;
  else if ((app.developer ?? "").toLowerCase().includes(q)) s = 0.25;
  if (s === 0) return 0;
  return Math.min(1, s + (app.popularity ?? 0) * 0.08);
}

function restrict(app: App, sources: SourceKind[]): App | null {
  const editions = app.editions.filter((e) => sources.includes(e.package.source));
  if (editions.length === 0) return null;
  return {
    ...app,
    editions,
    installed: editions.some((e) => e.package.installed),
  };
}

export async function system_info(): Promise<SystemInfo> {
  return SYSTEM;
}

export async function sources(): Promise<SourceStatus[]> {
  return SOURCES.map((s) => ({ ...s }));
}

export async function search(query: Query): Promise<SearchResult> {
  await delay(180);
  const wanted = (query.sources ?? SOURCES.map((s) => s.kind)).filter(searchable);
  const apps = APPS.map((a) => restrict(a, wanted))
    .filter((a): a is App => a !== null)
    .map((a) => ({ ...a, relevance: score(a, query.text) }))
    .filter((a) => a.relevance > 0)
    .sort((a, b) => b.relevance - a.relevance || (b.popularity ?? 0) - (a.popularity ?? 0))
    .slice(0, query.limit || 200);
  return { apps, failed: [], searched: wanted };
}

export async function installed(): Promise<SearchResult> {
  await delay(120);
  const wanted = SOURCES.filter((s) => s.available).map((s) => s.kind);
  const apps = APPS.map((a) => restrict(a, wanted))
    .filter((a): a is App => a !== null && a.installed)
    .map((a) => ({ ...a, relevance: 1 }))
    .sort((a, b) => a.name.localeCompare(b.name));
  return { apps, failed: [], searched: wanted };
}

export async function app_details(refs: PackageRef[]): Promise<Package[]> {
  await delay(240);
  const found: Package[] = [];
  for (const ref of refs) {
    const pkg = findPackage(ref);
    if (!pkg) throw new Error(`${sourceLabel(ref.source)} has no package called ${ref.id}. It may have been removed since the search.`);
    found.push(pkg);
  }
  return found;
}

// ── Updates ─────────────────────────────────────────────────────────────────

function updateList(): Update[] {
  const list: Update[] = [];
  for (const app of APPS) {
    for (const e of app.editions) {
      const p = e.package;
      if (!p.installed || !p.installed_version || !p.version || p.version === p.installed_version) continue;
      list.push({
        package: { source: p.source, id: p.id },
        name: app.name,
        kind: p.kind,
        summary: p.summary,
        icon: app.icon,
        from: p.installed_version,
        to: p.version,
        download_size: p.download_size,
        published: p.updated,
        is_self: p.appstream_id === "io.github.spillebulle.brokey",
      });
    }
  }
  return list.sort((a, b) => Number(b.is_self) - Number(a.is_self) || a.name.localeCompare(b.name));
}

let lastChecked = NOW - 40 * 60;

export async function updates(force: boolean): Promise<UpdateList> {
  await delay(force ? 1400 : 600);
  if (force) lastChecked = Math.floor(Date.now() / 1000);
  return { updates: updateList(), failed: [], checked_at: lastChecked };
}

// ── Plans ───────────────────────────────────────────────────────────────────

let planCounter = 0;

function nameOf(ref: PackageRef): string {
  const known = findPackage(ref)?.name;
  if (known) return known;
  // A GitHub release is owner/repo; the repository is the name it goes by.
  return ref.source === "github" ? (ref.id.split("/").pop() ?? ref.id) : ref.id;
}

function stepsFor(op: Op): Step[] {
  const cmd = (program: string, ...args: string[]) => ({ program, args, env: [] as [string, string][], cwd: null });
  switch (op.op) {
    case "install": {
      const name = nameOf(op.package);
      switch (op.package.source) {
        case "pacman":
          return [{ source: "pacman", title: `Installing ${name} and updating the system`, command: cmd("pacman", "-Syu", "--noconfirm", "--needed", op.package.id), needs_root: true, weight: 3 }];
        case "aur":
          return [{ source: "aur", title: `Building ${name} from the AUR`, command: cmd("paru", "-S", "--noconfirm", "--sudo", "pkexec", op.package.id), needs_root: false, weight: 5 }];
        case "flatpak":
          return [{ source: "flatpak", title: `Installing ${name} from Flathub`, command: cmd("flatpak", "install", "-y", "--noninteractive", "--system", "flathub", op.package.id), needs_root: false, weight: 3 }];
        case "github":
          return [
            { source: "github", title: `Downloading ${name}`, command: cmd("brokey", "download", op.package.id), needs_root: false, weight: 2 },
            { source: "github", title: `Installing ${name}`, command: cmd("pacman", "-U", "--noconfirm", `/var/cache/brokey/${op.package.id.replace("/", "-")}.pkg.tar.zst`), needs_root: true, weight: 2 },
          ];
        default:
          return [{ source: op.package.source, title: `Installing ${name}`, command: cmd(op.package.source, "install", op.package.id), needs_root: true, weight: 3 }];
      }
    }
    case "remove": {
      const name = nameOf(op.package);
      const program = op.package.source === "flatpak" ? "flatpak" : op.package.source === "aur" ? "pacman" : op.package.source;
      const args = op.package.source === "flatpak" ? ["uninstall", "-y", "--system", op.package.id] : ["-Rs", "--noconfirm", op.package.id];
      return [{ source: op.package.source, title: `Removing ${name}`, command: cmd(program, ...args), needs_root: op.package.source !== "flatpak", weight: 2 }];
    }
    case "update": {
      const name = nameOf(op.package);
      if (op.package.source === "github") {
        const asset = `${op.package.id.split("/").pop() ?? "release"}-0.2.0-1-x86_64.pkg.tar.zst`;
        return [
          { source: "github", title: `Downloading ${name} 0.2.0`, command: cmd("brokey", "download", op.package.id, asset), needs_root: false, weight: 2 },
          { source: "github", title: `Installing ${name} 0.2.0`, command: cmd("pacman", "-U", "--noconfirm", `/var/cache/brokey/${asset}`), needs_root: true, weight: 2 },
        ];
      }
      if (op.package.source === "aur") {
        return [{ source: "aur", title: `Updating ${name}`, command: cmd("paru", "-S", "--noconfirm", "--sudo", "pkexec", op.package.id), needs_root: false, weight: 5 }];
      }
      if (op.package.source === "flatpak") {
        return [{ source: "flatpak", title: `Updating ${name}`, command: cmd("flatpak", "update", "-y", "--noninteractive", op.package.id), needs_root: false, weight: 3 }];
      }
      return [{ source: op.package.source, title: `Updating ${name}`, command: cmd("pacman", "-S", "--noconfirm", op.package.id), needs_root: true, weight: 3 }];
    }
    case "updateall":
      switch (op.source) {
        case "pacman":
          return [{ source: "pacman", title: "Updating the system", command: cmd("pacman", "-Syu", "--noconfirm"), needs_root: true, weight: 8 }];
        case "aur":
          return [{ source: "aur", title: "Updating AUR packages", command: cmd("paru", "-Sua", "--noconfirm", "--sudo", "pkexec"), needs_root: false, weight: 6 }];
        case "flatpak":
          return [{ source: "flatpak", title: "Updating Flatpak applications", command: cmd("flatpak", "update", "-y", "--noninteractive"), needs_root: false, weight: 4 }];
        default:
          return [{ source: op.source, title: `Updating ${sourceLabel(op.source)}`, command: cmd(op.source, "update"), needs_root: true, weight: 4 }];
      }
    case "refresh":
      // pacman refreshes without root into the store's cache, so it plans
      // nothing here, as the real source does.
      if (op.source === "pacman") return [];
      return [{ source: op.source, title: `Refreshing ${sourceLabel(op.source)}`, command: cmd(op.source, "update"), needs_root: false, weight: 1 }];
    case "setup":
      // The tool from the distribution first, then what makes it usable; the
      // steps carry the source they set up so the dialog groups them with it.
      switch (op.source) {
        case "flatpak":
          return [
            { source: "flatpak", title: "Installing Flatpak", command: cmd("pacman", "-Syu", "--noconfirm", "--needed", "flatpak"), needs_root: true, weight: 3 },
            { source: "flatpak", title: "Adding Flathub", command: cmd("flatpak", "remote-add", "--if-not-exists", "--system", "flathub", "https://dl.flathub.org/repo/flathub.flatpakrepo"), needs_root: true, weight: 1 },
          ];
        case "snap":
          return [
            { source: "snap", title: "Building snapd from the AUR", command: cmd("paru", "-S", "--noconfirm", "--needed", "--sudo", "pkexec", "--skipreview", "snapd"), needs_root: false, weight: 5 },
            { source: "snap", title: "Starting the snapd service", command: cmd("systemctl", "enable", "--now", "snapd.socket"), needs_root: true, weight: 1 },
            { source: "snap", title: "Linking /snap", command: cmd("ln", "-sfn", "/var/lib/snapd/snap", "/snap"), needs_root: true, weight: 1 },
          ];
        default:
          return [];
      }
  }
}

/** "It is installed and Flathub is added first": what a setup does, as the middle of a sentence. */
function setupDoes(kind: SourceKind): string {
  switch (kind) {
    case "flatpak":
      return "It is installed and Flathub is added";
    case "snap":
      return "It is installed and its service is started";
    default:
      return "It is installed";
  }
}

/** The tool a setup brings: "Flatpak" for Flatpak, "snapd" for Snap. */
function toolName(kind: SourceKind): string {
  return kind === "snap" ? "snapd" : sourceLabel(kind);
}

/** "Flatpak is not installed. It is installed and Flathub is added first, then GNU Image Manipulation Program is installed from it." */
function setupNotice(kind: SourceKind, ops: Op[]): string {
  const names = ops.flatMap((o) => (o.op === "install" && o.package.source === kind ? [nameOf(o.package)] : []));
  const tool = toolName(kind);
  if (names.length === 0) {
    const status = SOURCES.find((s) => s.kind === kind);
    return status?.setup?.sentence ?? `${tool} is not installed. ${setupDoes(kind)}.`;
  }
  const list = names.length === 1 ? names[0] : `${names.slice(0, -1).join(", ")} and ${names[names.length - 1]}`;
  return `${tool} is not installed. ${setupDoes(kind)} first, then ${list} ${names.length === 1 ? "is" : "are"} installed from it.`;
}

function buildPlan(ops: Op[]): Plan {
  planCounter += 1;
  // Steps run in the order the operations were given: a download comes before
  // the install that needs it. The helper is asked for the password once, so
  // root steps need no grouping to share the prompt.
  const steps = ops.flatMap(stepsFor);
  return { id: `plan-${planCounter}`, ops, steps };
}

export async function plan(ops: Op[]): Promise<PlanPreview> {
  await delay(80);
  const unknown = ops.find((o) => "package" in o && o.package.id === UNPLANNABLE_ID);
  if (unknown && "package" in unknown) {
    throw new Error(`${sourceLabel(unknown.package.source)} has no package called ${UNPLANNABLE_ID}. It may have been renamed or dropped; search for it again.`);
  }
  const built = buildPlan(ops);
  const notices: string[] = [];
  const partial = ops.some((o) => o.op === "update" && o.package.source === "pacman");
  const whole = ops.some((o) => o.op === "updateall" && o.source === "pacman");
  if (partial && !whole) {
    notices.push("Arch does not support partial upgrades, so updating any pacman package updates every pacman package. Update all is what runs.");
  }
  for (const op of ops) {
    if (op.op === "setup") {
      notices.push(setupNotice(op.source, ops));
      if ((op.source === "flatpak" || op.source === "snap") && !available(op.source)) {
        notices.push(`Your launcher lists ${sourceLabel(op.source)} applications only after you log out and back in once. Until then, open them from Brokey.`);
      }
    }
  }
  if (built.steps.some((s) => s.command.program === "paru" && s.title.startsWith("Building"))) {
    notices.push("AUR packages are built on this machine as your user. A large build can take several minutes.");
  }
  return { plan: built, notices };
}

const target = new EventTarget();
const EVENT_NAME = "transaction://event";
const plans = new Map<string, PlanStatus>();
const cancelled = new Set<string>();
/** The step each running plan is in, so a cancel can say whether it has to wait for it. */
const running = new Map<string, Step>();

function emit(event: Event) {
  const status = plans.get(event.plan);
  if (status) {
    status.events.push(event);
    if (event.event === "auth_required") status.state = "authorising";
    if (event.event === "step_started") status.state = "running";
    if (event.event === "plan_finished") {
      status.state = cancelled.has(event.plan) ? "cancelled" : event.ok ? "done" : "failed";
    }
  }
  target.dispatchEvent(new CustomEvent<Event>(EVENT_NAME, { detail: event }));
}

function applyOp(op: Op) {
  switch (op.op) {
    case "install": {
      const p = findPackage(op.package);
      if (p) {
        p.installed = true;
        p.installed_version = p.version;
      }
      break;
    }
    case "remove": {
      const p = findPackage(op.package);
      if (p) {
        p.installed = false;
        p.installed_version = null;
      }
      break;
    }
    case "update": {
      const p = findPackage(op.package);
      if (p) p.installed_version = p.version;
      break;
    }
    case "updateall":
      for (const p of allPackages()) {
        if (p.source === op.source && p.installed) p.installed_version = p.version;
      }
      break;
    case "refresh":
      break;
    case "setup":
      markAvailable(op.source);
      if (!settings.enabled_sources.includes(op.source)) settings.enabled_sources.push(op.source);
      break;
  }
  for (const app of APPS) app.installed = app.editions.some((e) => e.package.installed);
}

function summary(ops: Op[], ok: boolean, failed?: Step): string {
  const setups = ops.flatMap((o) => (o.op === "setup" ? [toolName(o.source)] : []));
  const rest = ops.filter((o) => o.op !== "setup");
  const count = rest.length;
  const kinds = new Set(rest.map((o) => o.op));
  const noun = count === 1 ? "package" : "packages";
  if (!ok) {
    const what = failed ? failed.title.charAt(0).toLowerCase() + failed.title.slice(1) : "the transaction";
    return `${what.charAt(0).toUpperCase() + what.slice(1)} did not finish. ${failed?.command.program ?? "pacman"} said what went wrong in the log. Nothing was changed.`;
  }
  if (setups.length > 0) {
    const tools = setups.join(" and ");
    if (count === 0) return `Set up ${tools}.`;
    if (kinds.size === 1 && kinds.has("install")) return `Set up ${tools} and installed ${count} ${noun}.`;
    return `Set up ${tools} and finished ${count} ${count === 1 ? "operation" : "operations"}.`;
  }
  if (kinds.size === 1 && kinds.has("install")) return `Installed ${count} ${noun}.`;
  if (kinds.size === 1 && kinds.has("remove")) return `Removed ${count} ${noun}.`;
  if (kinds.has("updateall")) return "Everything is up to date.";
  if (kinds.size === 1 && kinds.has("update")) return `Updated ${count} ${noun}.`;
  return `Finished ${count} ${count === 1 ? "operation" : "operations"}.`;
}

function logLines(step: Step): string[] {
  const id = step.command.args[step.command.args.length - 1] ?? "";
  // A setup step speaks as the program it runs, not as the source it sets up.
  const program = step.command.program;
  const voice: SourceKind | "other" =
    program === "pacman" ? "pacman" : program === "paru" ? "aur" : program === "flatpak" ? (step.command.args[0] === "remote-add" ? "other" : "flatpak") : "other";
  switch (voice) {
    case "pacman":
      return [
        ":: Synchronising package databases...",
        " extra is up to date",
        "resolving dependencies...",
        "looking for conflicting packages...",
        `Packages (1) ${id}`,
        ":: Retrieving packages...",
        ":: Processing package changes...",
        "checking keys in keyring",
        "loading package files",
        `installing ${id}...`,
        ":: Running post-transaction hooks...",
      ];
    case "aur":
      return [
        `:: Resolving dependencies...`,
        `:: Calculating conflicts...`,
        `:: Calculating inner conflicts...`,
        `Aur (1) ${id}`,
        `:: Downloading PKGBUILDs...`,
        `==> Making package: ${id} (${new Date().toUTCString()})`,
        `==> Retrieving sources...`,
        `==> Validating source files with sha256sums...`,
        `==> Starting build()...`,
        `==> Creating package "${id}"...`,
        `==> Finished making: ${id}`,
        `loading packages...`,
      ];
    case "flatpak":
      return [
        `Looking for matches...`,
        `Installing ${id}`,
        `Installing runtime org.freedesktop.Platform/x86_64/25.08`,
        `Installation complete.`,
      ];
    default:
      return [`${step.command.program} ${step.command.args.join(" ")}`];
  }
}

/** Whether this step is the one that fails: its command names the package called fail-please. */
function fails(step: Step): boolean {
  return step.command.args.includes(FAIL_ID);
}

async function run(status: PlanStatus) {
  const { plan: p } = status;
  const id = p.id;
  const alive = () => !cancelled.has(id);
  emit({ event: "plan_started", plan: id, steps: p.steps.length });
  if (p.steps.some((s) => s.needs_root)) {
    await delay(300);
    if (!alive()) return;
    emit({ event: "auth_required", plan: id });
    if (HOLD === "auth") await hold();
    await delay(1400);
  }
  const total = p.steps.reduce((sum, s) => sum + s.weight, 0);
  let done = 0;
  for (let i = 0; i < p.steps.length; i += 1) {
    if (!alive()) return;
    const step = p.steps[i];
    running.set(id, step);
    emit({ event: "step_started", plan: id, step: i, title: step.title });
    const lines = logLines(step);
    const known = step.command.program !== "paru";
    const failing = fails(step);
    const stopAt = failing ? Math.min(lines.length, 4) : lines.length;
    for (let n = 0; n < stopAt; n += 1) {
      await delay(step.source === "aur" ? 260 : 140);
      if (!alive()) return;
      emit({ event: "log", plan: id, step: i, line: lines[n], stderr: false });
      const within = (n + 1) / lines.length;
      emit({
        event: "progress",
        plan: id,
        step: i,
        fraction: known ? (done + step.weight * within) / total : null,
        message: known ? null : "makepkg is building. No progress is reported for this step.",
      });
      // The picture of the panel mid-plan: part way through the first step, or the first unknown one.
      const midway = n + 1 === Math.ceil(lines.length * 0.6);
      if (midway && ((HOLD === "" || HOLD === "step") && i === 0)) await hold();
      if (midway && HOLD === "unknown" && !known) await hold();
    }
    if (failing) {
      await delay(200);
      if (!alive()) return;
      const reason = `error: target not found: ${FAIL_ID}`;
      emit({ event: "log", plan: id, step: i, line: reason, stderr: true });
      emit({ event: "log", plan: id, step: i, line: "error: failed to prepare transaction (target not found)", stderr: true });
      emit({ event: "step_finished", plan: id, step: i, ok: false, message: `${step.command.program} could not find a package called ${FAIL_ID}.` });
      running.delete(id);
      emit({ event: "plan_finished", plan: id, ok: false, message: summary(p.ops, false, step) });
      return;
    }
    done += step.weight;
    emit({ event: "step_finished", plan: id, step: i, ok: true, message: null });
  }
  running.delete(id);
  if (!alive()) return;
  for (const op of p.ops) applyOp(op);
  emit({ event: "plan_finished", plan: id, ok: true, message: summary(p.ops, true) });
}

export async function run_plan(ops: Op[]): Promise<PlanStatus> {
  const built = buildPlan(ops);
  const status: PlanStatus = {
    plan: built,
    state: "pending",
    events: [],
    started: Math.floor(Date.now() / 1000),
  };
  plans.set(built.id, status);
  // The runner starts after this call returns, the way the real one spawns a
  // thread: the page always sees the pending status before the first event.
  window.setTimeout(() => {
    void run(status);
  }, 0);
  return { ...status, events: [] };
}

export async function cancel_plan(id: string): Promise<void> {
  const status = plans.get(id);
  if (!status) throw new Error(`There is no running transaction called ${id}.`);
  if (status.state === "done" || status.state === "failed" || status.state === "cancelled") return;
  cancelled.add(id);
  const step = running.get(id);
  if (step?.needs_root) {
    // A root step is not killed half way: the helper lets it finish, then stops.
    const index = status.plan.steps.indexOf(step);
    emit({ event: "progress", plan: id, step: index, fraction: null, message: `${step.command.program} is finishing the current step first. A root step is never stopped half way.` });
    await delay(1200);
  }
  running.delete(id);
  emit({ event: "plan_finished", plan: id, ok: false, message: "Cancelled. Nothing was changed." });
}

export async function active_plans(): Promise<PlanStatus[]> {
  return [...plans.values()].filter((s) => s.state !== "done" && s.state !== "failed" && s.state !== "cancelled");
}

export function onTransactionEvent(handler: (event: Event) => void): () => void {
  const listener = (e: globalThis.Event) => handler((e as CustomEvent<Event>).detail);
  target.addEventListener(EVENT_NAME, listener);
  return () => target.removeEventListener(EVENT_NAME, listener);
}

// ── Drivers and firmware: this machine, as chwd and fwupd report it ─────────

export async function drivers(): Promise<DriversReport> {
  await delay(200);
  return {
    manager: "chwd",
    manager_note: null,
    devices: [
      {
        id: "0000:00:02.0",
        name: "UHD Graphics 630",
        vendor: "Intel Corporation",
        class: "VGA compatible controller",
        profiles: [
          {
            id: "intel",
            name: "intel",
            description: "The kernel's i915 driver with Mesa. What the display runs on when the NVIDIA card is idle.",
            installed: true,
            recommended: true,
            packages: ["mesa", "vulkan-intel", "intel-media-driver", "lib32-mesa", "lib32-vulkan-intel"],
          },
          {
            id: "fallback",
            name: "fallback",
            description: "Mesa only. Use this if the intel profile fails to start a session.",
            installed: false,
            recommended: false,
            packages: ["mesa", "lib32-mesa"],
          },
        ],
      },
      {
        id: "0000:01:00.0",
        name: "GeForce RTX 2070 Mobile",
        vendor: "NVIDIA Corporation",
        class: "VGA compatible controller",
        profiles: [
          {
            id: "nvidia-open-dkms.prime",
            name: "nvidia-open-dkms.prime",
            description: "NVIDIA open kernel modules with PRIME render offload, so the Intel GPU drives the display and the NVIDIA GPU takes the games.",
            installed: true,
            recommended: true,
            packages: ["nvidia-open-dkms", "nvidia-utils", "lib32-nvidia-utils", "nvidia-settings", "egl-wayland", "nvidia-prime", "switcheroo-control"],
          },
          {
            id: "nvidia-open-dkms",
            name: "nvidia-open-dkms",
            description: "NVIDIA open kernel modules driving the display directly.",
            installed: false,
            recommended: false,
            packages: ["nvidia-open-dkms", "nvidia-utils", "lib32-nvidia-utils", "nvidia-settings", "egl-wayland"],
          },
          {
            id: "fallback",
            name: "fallback",
            description: "The nouveau driver from Mesa. No CUDA, no ray tracing, works everywhere.",
            installed: false,
            recommended: false,
            packages: ["mesa", "lib32-mesa"],
          },
        ],
      },
    ],
    firmware_available: true,
    firmware_note: null,
    firmware: [
      {
        id: "adf9b09c46cea62d1b16348b52885bd168430bcc",
        name: "System Firmware",
        vendor: "Intel(R) Client Systems",
        version: "158",
        update_version: "160",
        update_summary: "Fixes a fan curve fault after resume and updates the CPU microcode.",
        update_size: 12.6 * MB,
        needs_reboot: true,
      },
      {
        id: "e11623b2caa18fee292058a5c09ca4e6152f7ecf",
        name: "WDC WDS100T2B0C-00PXH0",
        vendor: "Sandisk",
        version: "211210WD",
        update_version: null,
        update_summary: null,
        update_size: null,
        needs_reboot: true,
      },
    ],
  };
}

// ── Settings ────────────────────────────────────────────────────────────────

let settings: Settings = {
  theme: "dark",
  // A searchable source is wanted even before its tool is here: the search lists it and an install sets it up.
  enabled_sources: SOURCES.filter((s) => s.available || s.searchable).map((s) => s.kind),
  show_packages: false,
  aur_helper: "paru",
  flatpak_scope: "system",
  check_updates_on_start: true,
  self_update_check: true,
  update_check_minutes: 60,
  split: [],
};

export async function settings_get(): Promise<Settings> {
  return { ...settings, enabled_sources: [...settings.enabled_sources], split: [...settings.split] };
}

export async function settings_set(next: Settings): Promise<Settings> {
  settings = { ...next, enabled_sources: [...next.enabled_sources], split: [...next.split] };
  return settings_get();
}

export async function launch_targets(refs: PackageRef[]): Promise<(string | null)[]> {
  await delay(60);
  return refs.map((ref) => {
    const p = findPackage(ref);
    if (!p || !p.installed || p.kind !== "app") return null;
    if (p.source === "flatpak") return `${p.appstream_id ?? p.name}.desktop`;
    if (p.source === "snap") return `${p.id}_${p.id}.desktop`;
    return `${p.id}.desktop`;
  });
}

export async function open_app(pkg: PackageRef): Promise<void> {
  await delay(150);
  const target = (await launch_targets([pkg]))[0];
  if (!target) throw new Error(`${pkg.id} is not something Brokey can open. It may not be installed, or it has no application to start.`);
}

export async function launcher_notices(): Promise<[SourceKind, string][]> {
  await delay(40);
  return [...setUpThisSession].map((kind) => {
    const label = sourceLabel(kind);
    return [kind, `${label} applications are installed but this desktop session started before ${label} was set up, so your launcher does not list them yet. Log out and back in once to see them there; until then, open them from Brokey.`];
  });
}

export async function group_split(pkg: PackageRef): Promise<Settings> {
  const app = APPS.find((a) => a.editions.some((e) => e.package.source === pkg.source && e.package.id === pkg.id));
  if (!app) throw new Error(`${sourceLabel(pkg.source)} has no package called ${pkg.id}, so there is nothing to split.`);
  if (!settings.split.includes(app.key)) settings.split.push(app.key);
  return settings_get();
}

// ── Self-update ─────────────────────────────────────────────────────────────

const SELF_UPDATE: SelfUpdate = {
  current: "0.1.0",
  latest: {
    version: "0.2.0",
    notes: "- Search across pacman, the AUR and Flatpak in one list.\n- Updates page with a self-update notice.\n- Drivers and firmware through chwd and fwupd.",
    published: NOW - 1 * DAY,
    url: "https://github.com/Spillebulle/Brokey/releases/tag/v0.2.0",
    newer: true,
  },
  installation: { kind: "pacman", package: "brokey" },
  installation_label: "the brokey package from the AUR",
  remedy: {
    kind: "updates_page",
    source: "aur",
    package: "brokey",
    sentence: "Brokey 0.2.0 is published. This copy updates through the AUR package brokey, which appears in Updates once the AUR has it.",
  },
  error: null,
};

const SELF_ASSET = "brokey-bin-0.2.0-1-x86_64.pkg.tar.zst";

/** The release check as the switch asks: the AUR remedy by default, an asset install, or nothing new. */
function selfUpdate(): SelfUpdate {
  switch (PARAMS.get("selfupdate")) {
    case "asset":
      return {
        ...SELF_UPDATE,
        installation: { kind: "pacman", package: "brokey-bin" },
        installation_label: "the brokey-bin package",
        remedy: {
          kind: "install_asset",
          asset: SELF_ASSET,
          url: `https://github.com/Spillebulle/Brokey/releases/download/v0.2.0/${SELF_ASSET}`,
          installer: "pacmanu",
          sentence: `Brokey 0.2.0 is not in a repository this machine uses. Brokey will download ${SELF_ASSET} and install it with pacman.`,
        },
      };
    case "none":
      return { ...SELF_UPDATE, latest: null, remedy: null };
    default:
      return SELF_UPDATE;
  }
}

export async function self_update_check(force: boolean): Promise<SelfUpdate> {
  await delay(force ? 900 : 300);
  return selfUpdate();
}

export async function self_update_apply(): Promise<PlanStatus> {
  await delay(60);
  const remedy = selfUpdate().remedy;
  if (remedy?.kind !== "install_asset" && remedy?.kind !== "replace_file") {
    throw new Error("This copy of Brokey updates through the Updates page. Tick brokey-bin there.");
  }
  return run_plan([{ op: "update", package: { source: "github", id: "spillebulle/brokey" } }]);
}
