import { Cpu, HardDrive, RefreshCw, Search, Settings } from "lucide-react";
import type { ReactNode } from "react";
import pkg from "../../../package.json";
import { isLive, selectActivePlan, useActivity } from "../activity/store";
import { ICON } from "../components/icons";
import { formatCount, plural } from "../format";
import { sourceLabel, type PlanStatus } from "../types";
import { SelfUpdateNotice } from "./SelfUpdateNotice";
import { selectIsWindows, selectNav, useShell, type View } from "./store";

interface NavItem {
  view: View;
  label: string;
  icon: ReactNode;
}

const SOFTWARE: NavItem[] = [
  { view: "search", label: "Search", icon: <Search {...ICON} aria-hidden="true" /> },
  { view: "installed", label: "Installed", icon: <HardDrive {...ICON} aria-hidden="true" /> },
  { view: "updates", label: "Updates", icon: <RefreshCw {...ICON} aria-hidden="true" /> },
];

const MACHINE: NavItem[] = [
  { view: "drivers", label: "Drivers", icon: <Cpu {...ICON} aria-hidden="true" /> },
  { view: "settings", label: "Settings", icon: <Settings {...ICON} aria-hidden="true" /> },
];

/** Desktop shell (§6.1): 34 px menu bar, 240 px dock sidebar, content over window, 26 px status bar. */
export function Shell({ children }: { children: ReactNode }) {
  return (
    <div className="bk-app">
      <MenuBar />
      <div className="bk-body">
        <Sidebar />
        <main className="bk-content">
          <SelfUpdateNotice />
          {children}
        </main>
      </div>
      <StatusBar />
    </div>
  );
}

function MenuBar() {
  const status = useShell((s) => s.status);
  return (
    <header className="bk-menubar">
      <span className="bk-mark" aria-hidden="true" />
      <span className="bk-appname">Brokey</span>
      <span className="bk-menubar-status" role="status" aria-live="polite">
        {status ?? ""}
      </span>
    </header>
  );
}

function NavRow({ item, on, count }: { item: NavItem; on: boolean; count?: number }) {
  const go = useShell((s) => s.go);
  return (
    <button type="button" className={on ? "bk-navrow on" : "bk-navrow"} aria-current={on ? "page" : undefined} onClick={() => go(item.view)}>
      {item.icon}
      <span className="bk-navrow-label">{item.label}</span>
      {count !== undefined && count > 0 ? <span className="bk-navrow-count">{formatCount(count)}</span> : null}
    </button>
  );
}

function Sidebar() {
  const nav = useShell(selectNav);
  const updateCount = useShell((s) => s.updateCount);
  return (
    <nav className="bk-sidebar" aria-label="Pages">
      <div className="bk-nav">
        <div className="bk-eyebrow bk-nav-eyebrow">Software</div>
        {SOFTWARE.map((item) => (
          <NavRow key={item.view} item={item} on={nav === item.view} count={item.view === "updates" ? updateCount : undefined} />
        ))}
        <div className="bk-eyebrow bk-nav-eyebrow">Machine</div>
        {MACHINE.map((item) => (
          <NavRow key={item.view} item={item} on={nav === item.view} />
        ))}
      </div>
      <div className="bk-sidebar-foot">v{pkg.version} · GPL-3.0</div>
    </nav>
  );
}

/** State {@link planLine} needs beside the plan: whether it is being cancelled, and which platform is asking for elevation. */
interface PlanLineFlags {
  cancelling: boolean;
  windows: boolean;
}

/** What the status bar says about a running plan: the step in hand, and nothing when idle. */
function planLine(plan: PlanStatus | null, flags: PlanLineFlags): string | null {
  if (!plan || !isLive(plan.state)) return null;
  if (flags.cancelling) return "Cancelling…";
  const what = plural(plan.plan.ops.length, "operation");
  switch (plan.state) {
    case "pending":
      return `Starting ${what}…`;
    case "authorising":
      return flags.windows ? "Waiting for Administrator…" : "Waiting for your password…";
    default: {
      const started = [...plan.events].reverse().find((e) => e.event === "step_started");
      return `${started && started.event === "step_started" ? started.title : `Running ${what}`}…`;
    }
  }
}

function StatusBar() {
  const system = useShell((s) => s.system);
  const sources = useShell((s) => s.sources);
  const loadError = useShell((s) => s.loadError);
  const windows = useShell(selectIsWindows);
  const plan = useActivity(selectActivePlan);
  const cancelling = useActivity((s) => (plan ? s.cancelling[plan.plan.id] !== undefined : false));
  const live = sources.filter((s) => s.available).map((s) => sourceLabel(s.kind));
  const line = planLine(plan, { cancelling, windows });
  return (
    <footer className="bk-status">
      <div className="bk-status-side">
        {system ? <span className="bk-status-group">{system.pretty_name}</span> : null}
        {live.length > 0 ? <span className="bk-status-group">{live.join(", ")}</span> : null}
        {loadError ? <span className="bk-status-group">{loadError}</span> : null}
      </div>
      <div className="bk-status-side bk-status-side--end">{line ? <span className="bk-status-group">{line}</span> : null}</div>
    </footer>
  );
}
