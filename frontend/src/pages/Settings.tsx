// Settings in the §9 shape: groups of rows under eyebrows, every change
// saved as it is made through the shell store, Restore all at the foot and
// no Save button anywhere. The About block asks the self-updater once for
// the version and how this copy was installed.

import { Bug, Code, Download, RefreshCw } from "lucide-react";
import { useEffect, useState, type ReactNode } from "react";
import pkg from "../../../package.json";
import { startOps } from "../activity/flow";
import * as api from "../api";
import { Button, Dropdown, Figure, Notice, Segmented, Skeleton, Toggle, toast, type DropdownOption } from "../components";
import { ICON } from "../components/icons";
import { useShell } from "../shell/store";
import { SOURCE_KINDS, sourceLabel, type AurHelper, type SelfUpdate, type Settings, type SourceKind, type SourceSetup, type Theme } from "../types";
import { useBusy } from "./system/busy";
import { ThemeCard } from "./system/ThemeCard";
import "./system/system.css";

const REPO_URL = "https://github.com/Spillebulle/Brokey";
const ISSUES_URL = `${REPO_URL}/issues`;

/** What Restore all settings puts back. */
const DEFAULTS: Settings = {
  theme: "system",
  enabled_sources: [...SOURCE_KINDS],
  show_packages: false,
  aur_helper: "auto",
  flatpak_scope: "system",
  check_updates_on_start: true,
  self_update_check: true,
  update_check_minutes: 60,
  split: [],
};

const THEMES: { theme: Theme; name: string }[] = [
  { theme: "dark", name: "Dark" },
  { theme: "light", name: "Light" },
  { theme: "system", name: "System" },
];

const AUR_HELPERS: DropdownOption<AurHelper>[] = [
  { value: "auto", label: "Automatic" },
  { value: "paru", label: "paru" },
  { value: "yay", label: "yay" },
  { value: "builtin", label: "Built-in makepkg" },
];

const INTERVALS = [15, 30, 60, 180];

function minutesLabel(minutes: number): string {
  if (minutes < 60) return `${minutes} minutes`;
  const hours = minutes / 60;
  if (Number.isInteger(hours)) return hours === 1 ? "1 hour" : `${hours} hours`;
  return `${minutes} minutes`;
}

/** The four choices, plus whatever the file holds if it is none of them, so the control never shows nothing. */
function intervalOptions(current: number): DropdownOption[] {
  const values = INTERVALS.includes(current) ? INTERVALS : [current, ...INTERVALS].sort((a, b) => a - b);
  return values.map((m) => ({ value: String(m), label: minutesLabel(m) }));
}

function message(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

/** One sentence for the toast after a check (§12: say what happened). */
function checkSentence(result: SelfUpdate): string {
  if (result.error) return result.error;
  if (result.latest?.newer) {
    return result.remedy?.sentence ?? `Brokey ${result.latest.version} is available. This copy is ${result.current}.`;
  }
  if (!result.latest) return "No release of Brokey has been published yet.";
  return `Brokey ${result.current} is the newest version.`;
}

function Group({ eyebrow, children }: { eyebrow: string; children: ReactNode }) {
  return (
    <section className="bk-settings-group" aria-label={eyebrow}>
      <div className="bk-eyebrow">{eyebrow}</div>
      {children}
    </section>
  );
}

/** The button beside a source whose tool is missing: it starts the setup plan, and says so while one runs. */
function SetupButton({ kind, setup, running }: { kind: SourceKind; setup: SourceSetup; running: boolean }) {
  if (running) {
    return (
      <Button icon={<Download {...ICON} aria-hidden="true" />} disabled disabledReason={`${sourceLabel(kind)} is being set up. The activity panel shows progress.`}>
        Installing…
      </Button>
    );
  }
  return (
    <Button icon={<Download {...ICON} aria-hidden="true" />} title={setup.sentence} onClick={() => void startOps([{ op: "setup", source: kind }], setup.label)}>
      {setup.label}
    </Button>
  );
}

function Setting({ label, note, control }: { label: ReactNode; note?: ReactNode; control?: ReactNode }) {
  return (
    <div className="bk-setting">
      <div className="bk-setting-text">
        <span className="bk-setting-label">{label}</span>
        {note ? <span className="bk-setting-note">{note}</span> : null}
      </div>
      {control ? <div className="bk-setting-control">{control}</div> : null}
    </div>
  );
}

/** Rows with the geometry of a setting while the settings file is read. */
function SkeletonSettings() {
  return (
    <div className="bk-settings sy-settings" aria-hidden="true">
      {[3, 4, 2].map((rows, g) => (
        <div key={g} className="bk-settings-group">
          <Skeleton width="64px" className="bk-skel--text" />
          {Array.from({ length: rows }, (_, i) => (
            <div key={i} className="bk-setting">
              <div className="sy-skel-lines">
                <Skeleton width={`${24 + ((i * 11 + g * 7) % 20)}%`} />
              </div>
              <Skeleton className="sy-skel-control" />
            </div>
          ))}
        </div>
      ))}
    </div>
  );
}

export function SettingsPage() {
  const settings = useShell((s) => s.settings);
  const sources = useShell((s) => s.sources);
  const loadError = useShell((s) => s.loadError);
  const load = useShell((s) => s.load);
  const saveSettings = useShell((s) => s.saveSettings);

  const busy = useBusy();

  const [self, setSelf] = useState<SelfUpdate | null>(null);
  const [selfError, setSelfError] = useState<string | null>(null);
  const [checking, setChecking] = useState(false);

  useEffect(() => {
    let alive = true;
    api
      .self_update_check(false)
      .then((r) => {
        if (alive) setSelf(r);
      })
      .catch((e) => {
        if (alive) setSelfError(message(e));
      });
    return () => {
      alive = false;
    };
  }, []);

  const save = async (patch: Partial<Settings>) => {
    try {
      await saveSettings(patch);
    } catch (e) {
      toast(message(e), "error");
    }
  };

  const check = async () => {
    setChecking(true);
    try {
      const result = await api.self_update_check(true);
      setSelf(result);
      setSelfError(null);
      // A check GitHub did not answer is a failure, whatever else the answer holds.
      toast(checkSentence(result), result.error ? "error" : "good");
    } catch (e) {
      setSelfError(message(e));
      toast(message(e), "error");
    } finally {
      setChecking(false);
    }
  };

  const restore = async () => {
    await save(DEFAULTS);
    toast("Every setting is back to its default.", "good");
  };

  const forgetSplit = async () => {
    await save({ split: [] });
    toast("Split editions are grouped again.", "good");
  };

  const setSource = (kind: SourceKind, on: boolean) => {
    if (!settings) return;
    const next = SOURCE_KINDS.filter((k) => (k === kind ? on : settings.enabled_sources.includes(k)));
    void save({ enabled_sources: next });
  };

  const version = self?.current ?? pkg.version;

  return (
    <div className="bk-page">
      <div className="bk-page-head">
        <h1 className="bk-page-title">Settings</h1>
        <p className="bk-page-sub">The interface should disappear behind your work. Changes save as you make them.</p>
      </div>

      {loadError ? (
        <Notice
          actions={
            <Button kind="ghost" onClick={() => void load()}>
              Try again
            </Button>
          }
        >
          {loadError}
        </Notice>
      ) : null}

      {!settings && !loadError ? <SkeletonSettings /> : null}

      {settings ? (
        <div className="bk-settings sy-settings">
          <Group eyebrow="Appearance">
            <div className="bk-theme-cards">
              {THEMES.map((t) => (
                <ThemeCard key={t.theme} theme={t.theme} name={t.name} on={settings.theme === t.theme} onPick={() => void save({ theme: t.theme })} />
              ))}
            </div>
          </Group>

          <Group eyebrow="Sources">
            {
              // Only the kinds this machine's store actually reported: nine
              // on Linux, one on Windows, out of the fifteen SourceKind
              // holds so both platforms' types are checked whole. A row for
              // a source that could never exist here would read as Brokey
              // waiting to hear from something it never will.
              SOURCE_KINDS.flatMap((kind) => {
                const status = sources.find((s) => s.kind === kind);
                if (!status) return [];
                const label = sourceLabel(kind);
                const on = settings.enabled_sources.includes(kind);
                if (!status.available) {
                  const reason = status.reason ?? `${label} is not available on this machine.`;
                  const setup = status.setup ? <SetupButton kind={kind} setup={status.setup} running={busy.setups.has(kind)} /> : null;
                  // A source that is searched through its public store before its tool is here
                  // still has a meaningful switch: whether search asks it. The button beside it
                  // is how the tool gets here.
                  if (status.searchable) {
                    return [
                      <Setting
                        key={kind}
                        label={label}
                        note={reason}
                        control={
                          <>
                            {setup}
                            <Toggle label={label} on={on} onChange={(v) => setSource(kind, v)} />
                          </>
                        }
                      />,
                    ];
                  }
                  // Nothing here can use it, so the switch is drawn off: a lit switch that
                  // cannot be moved reads as a setting that is on and broken.
                  return [
                    <Setting
                      key={kind}
                      label={label}
                      note={reason}
                      control={
                        <>
                          {setup}
                          <Toggle label={label} on={false} onChange={() => undefined} disabled disabledReason={reason} />
                        </>
                      }
                    />,
                  ];
                }
                return [<Setting key={kind} label={label} note={status.detail ?? undefined} control={<Toggle label={label} on={on} onChange={(v) => setSource(kind, v)} />} />];
              })
            }
            <Setting
              label="AUR helper"
              note="Automatic uses paru or yay when one is installed, else the built-in makepkg."
              control={<Dropdown name="AUR helper" alone form options={AUR_HELPERS} value={settings.aur_helper} onChange={(v) => void save({ aur_helper: v })} />}
            />
            <Setting
              label="Flatpak installation"
              note="System asks for your password; User installs into your home."
              control={
                <Segmented
                  name="Flatpak installation"
                  value={settings.flatpak_scope}
                  onChange={(v) => void save({ flatpak_scope: v })}
                  options={[
                    { value: "system", label: "System" },
                    { value: "user", label: "User" },
                  ]}
                />
              }
            />
          </Group>

          <Group eyebrow="Search">
            <Setting
              label="Show packages, not only applications"
              note="Libraries, tools and fonts appear in results beside applications."
              control={<Toggle label="Show packages, not only applications" on={settings.show_packages} onChange={(v) => void save({ show_packages: v })} />}
            />
          </Group>

          <Group eyebrow="Updates">
            <Setting
              label="Check for updates when the window opens"
              control={<Toggle label="Check for updates when the window opens" on={settings.check_updates_on_start} onChange={(v) => void save({ check_updates_on_start: v })} />}
            />
            <Setting
              label="Tell me about new versions of Brokey"
              note="Asks GitHub for the newest release. Nothing is sent but the version."
              control={<Toggle label="Tell me about new versions of Brokey" on={settings.self_update_check} onChange={(v) => void save({ self_update_check: v })} />}
            />
            <Setting
              label="Check again every"
              control={
                <Dropdown
                  name="Check again every"
                  alone
                  form
                  options={intervalOptions(settings.update_check_minutes)}
                  value={String(settings.update_check_minutes)}
                  onChange={(v) => void save({ update_check_minutes: Number(v) })}
                />
              }
            />
          </Group>

          <Group eyebrow="About">
            <Setting
              label={
                <>
                  Brokey <Figure>{version}</Figure>
                </>
              }
              note={
                self ? (
                  self.error ? (
                    self.error
                  ) : self.latest?.newer ? (
                    <>
                      <Figure>{self.latest.version}</Figure> is available. {self.remedy?.sentence ?? ""}
                    </>
                  ) : self.latest ? (
                    "This is the newest version."
                  ) : (
                    "No release has been published yet."
                  )
                ) : selfError ? (
                  selfError
                ) : (
                  "Asking GitHub for the newest release…"
                )
              }
              control={
                checking ? (
                  <Button icon={<RefreshCw {...ICON} aria-hidden="true" />} disabled disabledReason="A check is already running.">
                    Checking…
                  </Button>
                ) : (
                  <Button icon={<RefreshCw {...ICON} aria-hidden="true" />} title="Ask GitHub for the newest release now." onClick={() => void check()}>
                    Check for a new version
                  </Button>
                )
              }
            />
            {self ? (
              <Setting label={`Installed as ${self.installation_label}.`} />
            ) : selfError ? (
              <Setting label="How this copy was installed is not known until the check answers." />
            ) : (
              <div className="bk-setting" aria-hidden="true">
                <div className="sy-skel-lines">
                  <Skeleton width="40%" />
                </div>
              </div>
            )}
            <div className="sy-links">
              <Button kind="ghost" icon={<Code {...ICON} aria-hidden="true" />} title={`Open ${REPO_URL} in the browser.`} onClick={() => void api.openUrl(REPO_URL)}>
                Source code
              </Button>
              <Button kind="ghost" icon={<Bug {...ICON} aria-hidden="true" />} title={`Open ${ISSUES_URL} in the browser.`} onClick={() => void api.openUrl(ISSUES_URL)}>
                Report a problem
              </Button>
            </div>
            <p className="sy-licence">GPL-3.0-or-later. Archivo is bundled under the SIL Open Font Licence; icons are Lucide, ISC.</p>
          </Group>

          {settings.split.length > 0 ? (
            <Group eyebrow="Danger">
              <Setting
                label="Editions you split from their rows are grouped again."
                note={settings.split.length === 1 ? "One row is split." : `${settings.split.length} rows are split.`}
                control={
                  <Button kind="danger" title="Forget every split and group the editions again." onClick={() => void forgetSplit()}>
                    Forget split rows
                  </Button>
                }
              />
            </Group>
          ) : null}

          <div className="bk-settings-foot">
            <span>Changes save as you make them.</span>
            <Button kind="outline" title="Put every setting back to its default." onClick={() => void restore()}>
              Restore all settings
            </Button>
          </div>
        </div>
      ) : null}
    </div>
  );
}
