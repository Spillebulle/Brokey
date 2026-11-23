import { lazy, Suspense, useEffect, useRef } from "react";
import { ActivityPanel } from "./activity/ActivityPanel";
import { startOps, trackPlan } from "./activity/flow";
import { FlowDialog } from "./activity/FlowDialog";
import { afterPlan, useLaunch } from "./activity/launch";
import { onPlanDone, subscribeActivity, useActivity } from "./activity/store";
import * as api from "./api";
import { Toasts } from "./components/Toast";
import { toast } from "./components/toastStore";
import { AppDetailPage, DriversPage, FeaturesPage, InstalledPage, SearchPage, SettingsPage, UpdatesPage } from "./pages";
import { useShortcuts } from "./shell/keys";
import { Shell } from "./shell/Shell";
import { startShowing } from "./shell/show";
import { useShell } from "./shell/store";
import { applyTheme } from "./shell/theme";
import { useSearch } from "./pages/search/store";
import { refreshUpdateCount, startUpdateTimer } from "./shell/updates";
import { SOURCE_KINDS, sourceLabel, type Op, type PackageRef, type SourceKind } from "./types";

// The component sheet is a development aid for a browser only; the window
// never loads it, so it is split out of the bundle it would otherwise weigh on.
const Gallery = lazy(() => import("./dev/Gallery").then((m) => ({ default: m.Gallery })));

/**
 * What the page calls a package: its application's name where search has seen
 * it, else the name its source gives in its details, else the id's last part.
 */
async function packageName(ref: PackageRef): Promise<string> {
  const { results, remembered } = useSearch.getState();
  const apps = [...(results?.apps ?? []), ...Object.values(remembered)];
  const edition = apps.flatMap((a) => a.editions.map((e) => ({ app: a, e }))).find(({ e }) => e.package.source === ref.source && e.package.id === ref.id);
  if (edition) return edition.app.name;
  try {
    const [details] = await api.app_details([ref]);
    if (details?.name) return details.name;
  } catch {
    // The id is the name of last resort.
  }
  const parts = ref.id.split("/").filter(Boolean);
  if (ref.source === "flatpak" && parts.length >= 5) return parts[2].split(".").pop() ?? parts[2];
  return parts[parts.length - 1] ?? ref.id;
}

function Page() {
  const view = useShell((s) => s.view);
  const appKey = useShell((s) => s.appKey);
  switch (view) {
    case "search":
      return <SearchPage />;
    case "app":
      return <AppDetailPage appKey={appKey} />;
    case "installed":
      return <InstalledPage />;
    case "updates":
      return <UpdatesPage />;
    case "drivers":
      return <DriversPage />;
    case "features":
      return <FeaturesPage />;
    case "settings":
      return <SettingsPage />;
  }
}

async function checkUpdatesOnStart() {
  const { setStatus, setUpdateCount } = useShell.getState();
  setStatus("Checking for updates…");
  try {
    const list = await api.updates(false);
    setUpdateCount(list.updates.length);
    for (const [, sentence] of list.failed) toast(sentence, "caution");
  } catch (e) {
    toast(e instanceof Error ? e.message : String(e), "error");
  } finally {
    setStatus(null);
  }
}

// ── Browser-only switches for screenshots (tools/shots.mjs) ─────────────────
//
// A plain browser has a query string; the window never does. Besides ?view=
// (the shell) and ?fast (the mock), two start a transaction on load so the
// dialog and the panel can be pictured without a page to click in:
//
//   ?confirm=install:pacman:gimp,install:aur:spotify   opens the confirm dialog
//   ?run=install:pacman:gimp,install:aur:spotify       starts the plan at once
//   &log                                                with the log open
//
// An operation is op:source:id (install, remove, update), updateall:source,
// refresh:source or setup:source (the source's tool first, then the rest).

function parseOps(text: string): Op[] {
  const ops: Op[] = [];
  for (const item of text.split(",")) {
    const [op, source, ...rest] = item.split(":");
    const id = rest.join(":");
    if (!SOURCE_KINDS.includes(source as SourceKind)) continue;
    const kind = source as SourceKind;
    if (op === "updateall" || op === "refresh" || op === "setup") ops.push({ op, source: kind });
    else if ((op === "install" || op === "remove" || op === "update") && id) ops.push({ op, package: { source: kind, id } });
  }
  return ops;
}

/** A title for the switch's dialog, in the words a page would use: "Set up Flatpak and install org.gimp.GIMP". */
function switchTitle(ops: Op[]): string {
  const setups = ops.flatMap((o) => (o.op === "setup" ? [sourceLabel(o.source)] : []));
  const rest = ops.filter((o) => o.op !== "setup");
  const verb = rest[0]?.op === "remove" ? "Remove" : "Install";
  const what = rest.length === 1 && "package" in rest[0] ? rest[0].package.id.split("/").find((s) => s.includes(".")) ?? rest[0].package.id : rest.length === 0 ? "" : "software";
  if (setups.length === 0) return `${verb} ${what}`;
  const tools = setups.join(" and ");
  return what ? `Set up ${tools} and ${verb.toLowerCase()} ${what}` : `Install ${tools}`;
}

function browserSwitches(): { confirm: Op[]; run: Op[]; log: boolean; gallery: boolean } {
  if (api.inTauri) return { confirm: [], run: [], log: false, gallery: false };
  const params = new URLSearchParams(window.location.search);
  return {
    confirm: parseOps(params.get("confirm") ?? ""),
    run: parseOps(params.get("run") ?? ""),
    log: params.has("log"),
    gallery: params.has("gallery"),
  };
}

export default function App() {
  const load = useShell((s) => s.load);
  const loadSources = useShell((s) => s.loadSources);
  const theme = useShell((s) => s.settings?.theme);
  const checkOnStart = useShell((s) => s.settings?.check_updates_on_start);
  const selfCheck = useShell((s) => s.settings?.self_update_check);
  const checkMinutes = useShell((s) => s.settings?.update_check_minutes);
  const checkSelfUpdate = useShell((s) => s.checkSelfUpdate);
  const checked = useRef(false);
  const selfChecked = useRef(false);
  const switched = useRef(false);
  useShortcuts();

  useEffect(() => {
    const unsubscribe = subscribeActivity();
    void load();
    return unsubscribe;
  }, [load]);

  useEffect(() => {
    applyTheme(theme ?? "system");
  }, [theme]);

  // Once, after settings have loaded and only if they ask for it.
  useEffect(() => {
    if (checkOnStart !== true || checked.current) return;
    checked.current = true;
    void checkUpdatesOnStart();
  }, [checkOnStart]);

  // The release check, once, unless Settings turned it off.
  useEffect(() => {
    if (selfCheck !== true || selfChecked.current) return;
    selfChecked.current = true;
    void checkSelfUpdate(false);
  }, [selfCheck, checkSelfUpdate]);

  // The Updates count stays true: after a plan ends well, and on the timer.
  useEffect(() => onPlanDone(() => void refreshUpdateCount()), []);
  // The application re-detects its sources after a plan (a setup brings a tool); the page asks again so Settings and the status bar follow.
  useEffect(() => onPlanDone(() => void loadSources()), [loadSources]);
  // Name what a plan installed, offer to open it, and say when the launcher cannot list it yet.
  useEffect(() => onPlanDone((status) => void afterPlan(status, packageName)), []);
  useEffect(() => void useLaunch.getState().loadNotices(), []);
  useEffect(() => startUpdateTimer(checkMinutes ?? 0), [checkMinutes]);
  // `brokey --show <source>:<id>`: an installed application's page, Remove focused (shell/show.ts).
  useEffect(() => startShowing(), []);

  useEffect(() => {
    if (switched.current) return;
    switched.current = true;
    const s = browserSwitches();
    if (s.log) useActivity.setState({ showLog: true });
    if (s.run.length > 0) void api.run_plan(s.run).then(trackPlan);
    if (s.confirm.length > 0) void startOps(s.confirm, switchTitle(s.confirm));
  }, []);

  const gallery = browserSwitches().gallery;

  return (
    <>
      <Shell>{gallery ? <Suspense fallback={null}><Gallery /></Suspense> : <Page />}</Shell>
      <div className="bk-corner">
        <Toasts />
        <ActivityPanel />
      </div>
      <FlowDialog />
    </>
  );
}
