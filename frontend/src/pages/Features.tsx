// Windows optional features: the page that takes the Drivers slot on Windows.
// One panel for the handful a person is likely to be looking for, one for
// every feature Windows lists, each row with the switch drawn as the same
// buttons the Drivers page uses. Every change starts a plan through startOps,
// which confirms it and says that Administrator is needed; whether Windows
// then needs a restart is said at the end of that plan, by the plan itself.

import { Blocks, Star } from "lucide-react";
import { useCallback, useEffect, useMemo, useState } from "react";
import { startOps } from "../activity/flow";
import { onPlanDone } from "../activity/store";
import * as api from "../api";
import { Badge, Button, EmptyState, Notice, Panel, SearchField, Skeleton } from "../components";
import { ICON_EMPTY, ICON_LG } from "../components/icons";
import type { FeaturesReport, PackageRef, WindowsFeature } from "../types";
import { isBusy, useBusy, type Busy } from "./system/busy";
import "./system/system.css";

const SUB =
  "Parts of Windows that are switched on rather than downloaded. Changing one asks to allow it as Administrator, and most changes finish when Windows restarts.";
const DOWNLOADS = "Turning it on downloads it from Windows Update first.";
const UNKNOWN = "Windows did not say whether this feature is on, so Brokey does not offer to change it.";
const NO_FEATURES = "Windows listed no optional features.";

function message(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

function FeatureRow({ feature, busy }: { feature: WindowsFeature; busy: Busy }) {
  const ref: PackageRef = { source: "features", id: feature.id };
  const working = isBusy(busy, ref);
  const on = feature.state === "enabled";
  const turnOn = () => void startOps([{ op: "install", package: ref }], `Turn on ${feature.name}`);
  const turnOff = () => void startOps([{ op: "remove", package: ref }], `Turn off ${feature.name}`);

  let action;
  if (feature.state === "unknown") {
    action = (
      <Button kind="ghost" disabled disabledReason={UNKNOWN}>
        Turn on
      </Button>
    );
  } else if (working) {
    action = (
      <Button kind={on ? "ghost" : undefined} disabled disabledReason="This feature is being changed. The activity panel shows its progress.">
        {on ? "Turning off…" : "Turning on…"}
      </Button>
    );
  } else if (on) {
    action = (
      <Button kind="ghost" title={`Turn ${feature.name} off. You are asked to confirm first.`} onClick={turnOff}>
        Turn off…
      </Button>
    );
  } else {
    action = (
      <Button title={feature.state === "absent" ? `Turn ${feature.name} on. ${DOWNLOADS}` : `Turn ${feature.name} on.`} onClick={turnOn}>
        Turn on
      </Button>
    );
  }

  return (
    <div className="sy-profile">
      <div className="sy-profile-text">
        <div className={on ? "sy-profile-name on" : "sy-profile-name"}>
          {on ? <span className="bk-dot bk-dot--accent" title="On" aria-hidden="true" /> : <span className="sy-dot-slot" aria-hidden="true" />}
          <span className="bk-row-label">{feature.name}</span>
          <span className="bk-row-badges">
            {on ? <Badge tone="good">on</Badge> : null}
            {feature.state === "absent" ? <Badge title={DOWNLOADS}>not downloaded</Badge> : null}
            {feature.state === "unknown" ? <Badge tone="caution" title={UNKNOWN}>unknown</Badge> : null}
          </span>
        </div>
        {feature.summary ? <div className="sy-profile-desc">{feature.summary}</div> : null}
        {feature.name !== feature.id ? <div className="sy-profile-pkgs">{feature.id}</div> : null}
      </div>
      <div className="sy-profile-action">{action}</div>
    </div>
  );
}

function FeatureRows({ features, busy }: { features: WindowsFeature[]; busy: Busy }) {
  return (
    <div className="sy-profiles">
      {features.map((f) => (
        <FeatureRow key={f.id} feature={f} busy={busy} />
      ))}
    </div>
  );
}

/** A panel of three-line rows, the geometry of the list, while Windows answers. */
function SkeletonPanel({ rows }: { rows: number }) {
  return (
    <div className="bk-panel" aria-hidden="true">
      <div className="sy-skel-head">
        <Skeleton width="28%" />
      </div>
      <div className="bk-panel-body">
        <div className="sy-profiles">
          {Array.from({ length: rows }, (_, i) => (
            <div key={i} className="sy-profile">
              <div className="sy-skel-lines">
                <Skeleton width={`${18 + ((i * 13) % 20)}%`} />
                <Skeleton width={`${50 + ((i * 23) % 30)}%`} className="bk-skel--text" />
              </div>
              <Skeleton width="64px" height="26px" />
            </div>
          ))}
        </div>
      </div>
    </div>
  );
}

function matches(f: WindowsFeature, query: string): boolean {
  const q = query.trim().toLowerCase();
  if (!q) return true;
  return f.name.toLowerCase().includes(q) || f.id.toLowerCase().includes(q) || (f.summary ?? "").toLowerCase().includes(q);
}

export function FeaturesPage() {
  const [report, setReport] = useState<FeaturesReport | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [query, setQuery] = useState("");
  const busy = useBusy();

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      setReport(await api.features());
    } catch (e) {
      setError(message(e));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  // A plan that turned something on or off changes what Windows reports.
  useEffect(() => onPlanDone(() => void load()), [load]);

  const common = useMemo(() => (report?.features ?? []).filter((f) => f.well_known), [report]);
  const rest = useMemo(() => (report?.features ?? []).filter((f) => !f.well_known && matches(f, query)), [report, query]);
  const others = report ? report.features.length - common.length : 0;

  return (
    <div className="bk-page" aria-busy={loading}>
      <div className="bk-page-head">
        <h1 className="bk-page-title">Windows features</h1>
        <p className="bk-page-sub">{SUB}</p>
      </div>

      {error ? (
        <Notice
          actions={
            <Button kind="ghost" onClick={() => void load()}>
              Try again
            </Button>
          }
        >
          {error}
        </Notice>
      ) : null}

      {loading && !report ? (
        <div aria-label="Loading" className="sy-skel-panels">
          <SkeletonPanel rows={4} />
          <SkeletonPanel rows={6} />
        </div>
      ) : null}

      {report && !report.available ? (
        <EmptyState fill icon={<Blocks {...ICON_EMPTY} aria-hidden="true" />}>
          {report.note ?? NO_FEATURES}
        </EmptyState>
      ) : null}

      {report && report.available ? (
        <>
          {common.length > 0 ? (
            <Panel title="Common features" icon={<Star {...ICON_LG} aria-hidden="true" />} count={common.length}>
              <FeatureRows features={common} busy={busy} />
            </Panel>
          ) : null}
          <Panel
            title="All features"
            icon={<Blocks {...ICON_LG} aria-hidden="true" />}
            count={others}
            commands={<SearchField label="Filter features" value={query} onChange={setQuery} placeholder="Filter by name" width="calc(var(--s6) * 7)" />}
          >
            {others === 0 ? (
              <EmptyState icon={<Blocks {...ICON_EMPTY} aria-hidden="true" />}>{report.note ?? NO_FEATURES}</EmptyState>
            ) : rest.length === 0 ? (
              <EmptyState>No feature matches {query.trim()}.</EmptyState>
            ) : (
              <FeatureRows features={rest} busy={busy} />
            )}
          </Panel>
        </>
      ) : null}
    </div>
  );
}
