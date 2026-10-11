// Every component drawn once on one sheet against the mock world, so the
// stylesheet can be looked at rather than trusted (§11's advice for icons,
// applied to the whole kit). Reached with ?gallery in a browser; the window
// never loads it. Not a page: nothing here is reachable from the sidebar.

import { Check, Download, ExternalLink, Layers, Plus, RefreshCw, Trash } from "lucide-react";
import { useEffect, useState, type ReactNode } from "react";
import { useActivity } from "../activity/store";
import * as api from "../api";
import {
  AppIcon,
  AppRow,
  ArtCard,
  Badge,
  Button,
  Bytes,
  Checkbox,
  Chip,
  Count,
  Description,
  Dialog,
  Dropdown,
  EmptyState,
  Facts,
  Field,
  Figure,
  IconButton,
  ICON,
  ICON_EMPTY,
  ICON_LG,
  Keycap,
  MediaRail,
  MultiSelect,
  Notice,
  Panel,
  Progress,
  Row,
  SearchField,
  Segmented,
  Skeleton,
  SkeletonAppRows,
  SourceBadge,
  toast,
  Toggle,
  When,
  type DropdownOption,
} from "../components";
import { useShell } from "../shell/store";
import { sourceLabel, type App, type Package, type SourceKind, type Theme } from "../types";

function Section({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section className="bk-stack" style={{ gap: "12px" }}>
      <h3 className="bk-section-title">{title}</h3>
      {children}
    </section>
  );
}

const SORTS: DropdownOption[] = [
  { value: "relevance", label: "Relevance" },
  { value: "name", label: "Name" },
  { value: "updated", label: "Recently updated" },
  { value: "popularity", label: "Popularity" },
];

const CATEGORIES: DropdownOption[] = [
  "Audio", "Development", "Education", "Games", "Graphics", "Network", "Office", "Science", "Settings", "System", "Utility", "Video",
].map((c) => ({ value: c.toLowerCase(), label: c, group: c < "M" ? "A to L" : "M to Z" }));

function ThemeCard({ theme, name, on, onPick }: { theme: Theme; name: string; on: boolean; onPick: () => void }) {
  const half = (kind: "dark" | "light") => (
    <div className={`bk-theme-prev-half bk-theme-prev--${kind}`}>
      <div className="bk-theme-prev-side" />
      <div className="bk-theme-prev-rows">
        <i className="mark" />
        <i style={{ width: "70%" }} />
        <i style={{ width: "45%" }} />
        <i style={{ width: "60%" }} />
      </div>
    </div>
  );
  return (
    <button type="button" className={on ? "bk-card on" : "bk-card"} onClick={onPick} aria-pressed={on}>
      <div className="bk-card-prev">
        <div className="bk-theme-prev">
          {theme === "system" ? (
            <>
              {half("dark")}
              {half("light")}
            </>
          ) : (
            half(theme)
          )}
        </div>
      </div>
      <div className="bk-card-cap">
        <span>{name}</span>
        {on ? <small>in use</small> : null}
      </div>
    </button>
  );
}

export function Gallery() {
  const sources = useShell((s) => s.sources);
  const settings = useShell((s) => s.settings);
  const saveSettings = useShell((s) => s.saveSettings);
  const [apps, setApps] = useState<App[]>([]);
  const [gimp, setGimp] = useState<Package | null>(null);
  const [blender, setBlender] = useState<Package | null>(null);
  const [query, setQuery] = useState("");
  const [picked, setPicked] = useState<SourceKind[]>(["pacman", "aur", "flatpak"]);
  const [kind, setKind] = useState<"all" | "app" | "package">("all");
  const [sort, setSort] = useState<string | null>("relevance");
  const [category, setCategory] = useState<string | null>(null);
  const [installedOnly, setInstalledOnly] = useState(false);
  const [tick, setTick] = useState<boolean | "mixed">("mixed");
  const [name, setName] = useState("");
  const [dialog, setDialog] = useState(false);
  const [busy, setBusy] = useState(false);
  const [selectedShot, setSelectedShot] = useState(0);

  useEffect(() => {
    void api.search({ text: "", sources: null, limit: 6 }).then((r) => setApps(r.apps));
    void api
      .app_details([
        { source: "flatpak", id: "flathub/app/org.gimp.GIMP/x86_64/stable" },
        { source: "pacman", id: "blender" },
      ])
      .then(([g, b]) => {
        setGimp(g);
        setBlender(b);
      });
  }, []);

  const sourceOptions: DropdownOption<SourceKind>[] = sources.map((s) => ({
    value: s.kind,
    label: sourceLabel(s.kind),
    figure: s.detail ?? undefined,
    disabled: !s.available,
    disabledReason: s.reason ?? undefined,
  }));

  const runPlan = async () => {
    const status = await api.run_plan([
      { op: "install", package: { source: "pacman", id: "steam" } },
      { op: "install", package: { source: "aur", id: "spotify" } },
    ]);
    useActivity.getState().track(status);
  };

  return (
    <>
      <div className="bk-toolbar">
        <span className="bk-toolbar-title">Search</span>
        <SearchField label="Search" value={query} onChange={setQuery} placeholder="Search applications" width="240px" />
        <MultiSelect name="Sources" options={sourceOptions} values={picked} onChange={setPicked} alone />
        <Segmented
          name="Kind"
          value={kind}
          onChange={setKind}
          options={[
            { value: "all", label: "All" },
            { value: "app", label: "Applications" },
            { value: "package", label: "Packages" },
          ]}
        />
        <Dropdown name="Sort" options={SORTS} value={sort} onChange={setSort} />
        <span className="bk-inline bk-small">
          Installed only
          <Toggle label="Installed only" on={installedOnly} onChange={setInstalledOnly} />
        </span>
        <div className="bk-toolbar-end">
          <button type="button" className="bk-toolbar-link">
            <RefreshCw {...ICON} aria-hidden="true" />
            Refresh
          </button>
        </div>
      </div>

      <div className="bk-page">
        <div className="bk-page-head">
          <div className="bk-page-title">Component sheet</div>
          <div className="bk-page-sub">Every painted control from app.css, drawn against the mock world. Not a page of the application.</div>
        </div>

        <Section title="Buttons">
          <div className="bk-inline bk-wrap">
            <Button kind="primary" icon={<Download {...ICON} aria-hidden="true" />}>
              Install
            </Button>
            <Button>Show details</Button>
            <Button kind="outline">Restore all settings</Button>
            <Button kind="ghost">Cancel</Button>
            <Button kind="danger" icon={<Trash {...ICON} aria-hidden="true" />}>
              Remove
            </Button>
            <Button disabled disabledReason="Nothing is selected.">
              Update selected
            </Button>
            <IconButton label="Add a source" icon={<Plus {...ICON} aria-hidden="true" />} />
            <IconButton label="Open on Flathub" icon={<ExternalLink {...ICON} aria-hidden="true" />} />
            <IconButton label="Layers" kind="tool" active icon={<Layers {...ICON} aria-hidden="true" />} />
            <IconButton label="Layers" kind="tool" icon={<Layers {...ICON} aria-hidden="true" />} />
            <Button onClick={runPlan}>Run a mock plan</Button>
          </div>
        </Section>

        <Section title="Dropdowns, toggle, segments, fields, checkboxes">
          <div className="bk-inline bk-wrap" style={{ alignItems: "flex-start", gap: "24px" }}>
            <div className="bk-stack" style={{ width: "260px" }}>
              <Dropdown name="Category" options={CATEGORIES} value={category} onChange={setCategory} alone form full />
              <Dropdown name="Sort" options={SORTS} value={sort} onChange={setSort} alone form full />
              <MultiSelect name="Sources" options={sourceOptions} values={picked} onChange={setPicked} alone form full />
              <Field label="Name" value={name} onChange={setName} placeholder="A name for this bottle" full error={name.length > 12 ? "Keep the name under twelve characters." : undefined} />
              <SearchField label="Search" value={query} onChange={setQuery} placeholder="Search brushes" full />
            </div>
            <div className="bk-stack" style={{ width: "280px" }}>
              <div className="bk-setting">
                <div className="bk-setting-text">
                  <span className="bk-setting-label">Check for updates on start</span>
                  <span className="bk-setting-note">Asks each source once per launch.</span>
                </div>
                <div className="bk-setting-control">
                  <Toggle label="Check for updates on start" on={settings?.check_updates_on_start ?? true} onChange={(on) => void saveSettings({ check_updates_on_start: on })} />
                </div>
              </div>
              <div className="bk-setting">
                <div className="bk-setting-text">
                  <span className="bk-setting-label">Flatpak scope</span>
                </div>
                <div className="bk-setting-control">
                  <Segmented
                    name="Flatpak scope"
                    value={settings?.flatpak_scope ?? "system"}
                    onChange={(v) => void saveSettings({ flatpak_scope: v })}
                    options={[
                      { value: "system", label: "System" },
                      { value: "user", label: "User" },
                    ]}
                  />
                </div>
              </div>
              <div className="bk-inline bk-wrap">
                <Checkbox checked={tick} onChange={setTick}>
                  Include hidden packages
                </Checkbox>
                <Checkbox checked={false} onChange={() => undefined} disabled disabledReason="Snap is not installed.">
                  Snap
                </Checkbox>
              </div>
              <div className="bk-inline bk-wrap">
                <Badge>pacman</Badge>
                <Badge tone="good">Installed</Badge>
                <Badge tone="caution">Out of date</Badge>
                <Badge tone="critical">Failed</Badge>
                <SourceBadge source="flatpak" installed repo="flathub" />
                <SourceBadge source="aur" repo="aur" />
              </div>
              <div className="bk-inline bk-wrap">
                <Chip label="Size" value="1.5 GB" />
                <Chip label="Votes" value={<Count value={1257} />} />
                <Chip onRemove={() => toast("Removed the Games filter.", "neutral")}>Games</Chip>
                <Keycap>Ctrl</Keycap>
                <Keycap>F</Keycap>
                <Keycap clash>B</Keycap>
              </div>
              <div className="bk-inline bk-wrap bk-dim bk-tiny">
                <Bytes value={1_500_000_000} />
                <Count value={100548} />
                <When value={Math.floor(Date.now() / 1000) - 180} />
                <When value={Math.floor(Date.now() / 1000) - 86400 * 40} />
                <Figure>2048 × 2048</Figure>
              </div>
            </div>
          </div>
        </Section>

        <Section title="Application rows, sized by the icon">
          <div className="bk-well">
            <div className="bk-list">
              {apps.map((app, i) => (
                <AppRow
                  key={app.key}
                  app={app}
                  selected={i === 1}
                  onOpen={() => toast(`Opened ${app.name}.`, "neutral")}
                  figure={app.editions[0]?.package.version ?? undefined}
                  action={
                    app.installed ? (
                      <Button kind="ghost">Open</Button>
                    ) : (
                      <Button icon={<Download {...ICON} aria-hidden="true" />}>Install</Button>
                    )
                  }
                />
              ))}
            </div>
          </div>
          <SkeletonAppRows count={2} />
          <div className="bk-inline" style={{ alignItems: "flex-start", gap: "24px" }}>
            <div className="bk-well" style={{ width: "260px" }}>
              <div className="bk-list">
                <Row leading={<span className="bk-dot bk-dot--good" />} trailing="up 41 d" onClick={() => undefined}>
                  proxmox-01
                </Row>
                <Row leading={<span className="bk-dot bk-dot--good" />} trailing="24 ports" selected onClick={() => undefined}>
                  switch-core
                </Row>
                <Row leading={<span className="bk-dot bk-dot--critical" />} trailing="on battery" active onClick={() => undefined}>
                  ups-rack
                </Row>
                <Row plain trailing="1 s">
                  Stroke
                </Row>
              </div>
            </div>
            <div className="bk-table-wrap bk-grow">
              <table className="bk-table">
                <thead>
                  <tr>
                    <th>Device</th>
                    <th>Profile</th>
                    <th className="n">Packages</th>
                    <th className="n">Priority</th>
                  </tr>
                </thead>
                <tbody>
                  <tr>
                    <td>GeForce RTX 2070 Mobile</td>
                    <td>nvidia-open-dkms.prime</td>
                    <td className="n">7</td>
                    <td className="n">11</td>
                  </tr>
                  <tr>
                    <td>UHD Graphics 630</td>
                    <td>intel</td>
                    <td className="n">5</td>
                    <td className="n">4</td>
                  </tr>
                </tbody>
              </table>
            </div>
          </div>
        </Section>

        <Section title="Backdrop header and detail hero">
          <div className="bk-well" style={{ background: "var(--window)" }}>
            <div className="bk-detail-head">
              <div className="bk-backdrop">
                {blender?.screenshots[0] ? <img src={api.pictureSrc(blender.screenshots[0].image) ?? undefined} alt="Blender's viewport" /> : null}
                <div className="bk-backdrop-fade" />
              </div>
              <div className="bk-hero bk-hero--over">
                <div className="bk-hero-icon">
                  <AppIcon picture={blender?.icon ?? null} name="Blender" size="hero" />
                </div>
                <div className="bk-hero-text">
                  <div className="bk-hero-title">{blender?.name ?? "Blender"}</div>
                  <div className="bk-hero-sub">{blender?.summary}</div>
                  <div className="bk-hero-actions">
                    <Button kind="primary" icon={<Download {...ICON} aria-hidden="true" />}>
                      Install
                    </Button>
                    <Dropdown name="Edition" alone form value="pacman" onChange={() => undefined} options={[{ value: "pacman", label: "pacman (extra) 17:5.2-1" }, { value: "flatpak", label: "Flatpak (flathub) 5.2" }]} />
                    <span className="bk-hero-note">matched by name</span>
                  </div>
                </div>
              </div>
            </div>
            <div className="bk-page" style={{ paddingTop: "var(--s4)" }}>
              <Facts items={[["Developer", blender?.developer ?? ""], ["Licence", blender?.licence ?? ""], ["Download", "132 MB"], ["Installed size", "480 MB"], ["Version", "17:5.2-1"]]} />
            </div>
          </div>
        </Section>

        <Section title="Media rail and art cards">
          {gimp ? (
            <MediaRail title="Screenshots" count={gimp.screenshots.length} onShowAll={() => toast("Every screenshot would open here.", "neutral")}>
              {gimp.screenshots.map((s, i) => (
                <ArtCard
                  key={i}
                  picture={s.thumbnail ?? s.image}
                  alt={s.caption ?? `GIMP screenshot ${i + 1}`}
                  size="card"
                  title={s.caption ?? `Screenshot ${i + 1}`}
                  figure={s.width && s.height ? `${s.width} × ${s.height}` : undefined}
                  selected={selectedShot === i}
                  onClick={() => setSelectedShot(i)}
                  state={i === 0 ? <Check size={11} strokeWidth={2.5} aria-hidden="true" /> : undefined}
                />
              ))}
              <ArtCard picture={null} alt="A missing picture" size="card" title="Missing picture" figure="fallback" />
            </MediaRail>
          ) : null}
        </Section>

        <Section title="Panel, notice, progress, skeleton, empty state">
          <div className="bk-inline" style={{ alignItems: "flex-start", gap: "24px" }}>
            <Panel
              title="GeForce RTX 2070 Mobile"
              icon={<Layers {...ICON_LG} aria-hidden="true" />}
              count={3}
              commands={
                <>
                  <IconButton size="sm" label="Refresh" icon={<RefreshCw {...ICON} aria-hidden="true" />} />
                  <IconButton size="sm" label="Add" icon={<Plus {...ICON} aria-hidden="true" />} />
                </>
              }
              className="bk-grow"
            >
              <div className="bk-list">
                <Row selected trailing="installed">
                  nvidia-open-dkms.prime
                </Row>
                <Row trailing="10" onClick={() => undefined}>
                  nvidia-open-dkms
                </Row>
                <Row trailing="3" onClick={() => undefined}>
                  fallback
                </Row>
              </div>
            </Panel>
            <div className="bk-stack bk-grow" style={{ gap: "16px" }}>
              <Notice actions={<Button kind="ghost">Update all</Button>}>
                Arch does not support partial upgrades, so updating any pacman package updates every pacman package. Update all is what runs.
              </Notice>
              <Progress fraction={0.64} message="Downloading firefox-155.0.1-1-x86_64.pkg.tar.zst" />
              <Progress fraction={null} message="makepkg is building. No progress is reported for this step." />
              <Progress fraction={null} sliding message="Fetching the AUR index. Its size is not announced." />
              <div className="bk-inline" style={{ gap: "6px" }}>
                <Skeleton width="40%" />
                <Skeleton width="20%" />
              </div>
              <div className="bk-well">
                <EmptyState icon={<Layers {...ICON_EMPTY} aria-hidden="true" />} action={<Button>Refresh</Button>}>
                  Nothing matches "zsh-theme". Try a shorter word, or turn on Packages in the toolbar.
                </EmptyState>
              </div>
            </div>
          </div>
        </Section>

        <Section title="Theme cards">
          <div className="bk-theme-cards">
            {(["dark", "light", "system"] as Theme[]).map((t) => (
              <ThemeCard key={t} theme={t} name={t === "dark" ? "Graphite" : t === "light" ? "Paper" : "Follow the system"} on={(settings?.theme ?? "dark") === t} onPick={() => void saveSettings({ theme: t })} />
            ))}
          </div>
        </Section>

        <Section title="Description and facts">
          <div className="bk-inline" style={{ alignItems: "flex-start", gap: "32px" }}>
            <Description markup={gimp?.description} />
            <Facts items={gimp?.facts ?? []} />
          </div>
        </Section>

        <Section title="Dialog and toasts">
          <div className="bk-inline bk-wrap">
            <Button onClick={() => setDialog(true)}>Open a dialog</Button>
            <Button onClick={() => toast("Installed Steam.", "good")}>Good toast</Button>
            <Button onClick={() => toast("pacman could not lock the database. Another package manager is running.", "error", { label: "Details", onClick: () => undefined })}>
              Error toast
            </Button>
            <Button onClick={() => toast("Flathub did not answer. Search results leave Flatpak out.", "caution")}>Caution toast</Button>
          </div>
        </Section>
      </div>

      <Dialog
        open={dialog}
        title="Remove Steam"
        subtitle="Removing takes the launcher off this machine. Games in your library stay on disk."
        onClose={() => setDialog(false)}
        busy={busy ? "Steam is being removed. Wait for it to finish." : undefined}
        note="Removed with pacman -Rs."
        actions={
          <>
            <Button kind="ghost" onClick={() => setDialog(false)}>
              Cancel
            </Button>
            <Button kind="danger" onClick={() => setBusy((b) => !b)}>
              {busy ? "Removing" : "Remove"}
            </Button>
          </>
        }
      >
        <div className="bk-stack">
          <div className="bk-setting">
            <div className="bk-setting-text">
              <span className="bk-setting-label">Also remove the configuration in ~/.steam</span>
            </div>
            <div className="bk-setting-control">
              <Toggle label="Also remove the configuration" on={false} onChange={() => undefined} />
            </div>
          </div>
          <Notice>Dependencies nothing else needs are removed too: lib32-mesa and 14 others.</Notice>
        </div>
      </Dialog>
    </>
  );
}
