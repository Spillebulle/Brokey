// Installed: everything the readable sources have put on this machine, as
// application rows with the installed editions' badges, the version, and a
// way to remove one edition. The list is asked for on every visit and kept
// for the session, so coming back shows it at once.

import { ArrowUpDown, HardDrive, Layers, Play, RefreshCw } from "lucide-react";
import { useEffect, useMemo, type ReactNode } from "react";
import { startOps } from "../activity/flow";
import { launchKey, openPackage, useLaunchTargets } from "../activity/launch";
import { AppRow, Button, Count, Dropdown, EmptyState, Figure, ICON, ICON_EMPTY, MultiSelect, Notice, SearchField, Segmented, Skeleton, SkeletonAppRows } from "../components";
import type { DropdownOption } from "../components";
import { missingSentence, useShow } from "../shell/show";
import { useShell } from "../shell/store";
import { SOURCE_KINDS, sourceLabel, type App, type Edition, type SourceKind } from "../types";
import { useInstalled, type InstalledSort, type Scope } from "./lists/installedStore";
import { rememberApp } from "./lists/remember";
import { availableKinds, failedSentence, joinNames, off, onPlanDone, refKey, sourceOptions } from "./lists/shared";
import "./lists/lists.css";

const SORTS: DropdownOption<InstalledSort>[] = [
  { value: "name", label: "Name" },
  { value: "source", label: "Source" },
  { value: "size", label: "Size" },
  { value: "updated", label: "Updated" },
];

const SCOPES = [
  { value: "apps" as Scope, label: "Applications" },
  { value: "all" as Scope, label: "All packages" },
];

function installedEditions(app: App): Edition[] {
  return app.editions.filter((e) => e.package.installed);
}

function installedSize(app: App): number | null {
  let total = 0;
  let known = false;
  for (const e of installedEditions(app)) {
    if (e.package.installed_size !== null) {
      total += e.package.installed_size;
      known = true;
    }
  }
  return known ? total : null;
}

function sourceRank(app: App): number {
  const ranks = installedEditions(app).map((e) => SOURCE_KINDS.indexOf(e.package.source));
  return ranks.length ? Math.min(...ranks) : SOURCE_KINDS.length;
}

function compare(sort: InstalledSort): (a: App, b: App) => number {
  const byName = (a: App, b: App) => a.name.localeCompare(b.name);
  switch (sort) {
    case "name":
      return byName;
    case "source":
      return (a, b) => sourceRank(a) - sourceRank(b) || byName(a, b);
    case "size":
      return (a, b) => (installedSize(b) ?? -1) - (installedSize(a) ?? -1) || byName(a, b);
    case "updated":
      return (a, b) => (b.updated ?? -1) - (a.updated ?? -1) || byName(a, b);
  }
}

function matches(app: App, query: string): boolean {
  const q = query.trim().toLowerCase();
  if (!q) return true;
  if (app.name.toLowerCase().includes(q)) return true;
  if ((app.summary ?? "").toLowerCase().includes(q)) return true;
  return app.editions.some((e) => e.package.id.toLowerCase().includes(q));
}

/** "31 applications and packages across pacman, AUR and Flatpak". */
function HeadLine({ apps, loading }: { apps: App[] | null; loading: boolean }) {
  if (!apps) return loading ? <Skeleton width="40%" /> : null;
  const applications = apps.filter((a) => a.kind === "app").length;
  const packages = apps.length - applications;
  const sources = SOURCE_KINDS.filter((k) => apps.some((a) => installedEditions(a).some((e) => e.package.source === k)));
  let what: string;
  if (applications > 0 && packages > 0) what = "applications and packages";
  else if (packages > 0) what = packages === 1 ? "package" : "packages";
  else what = applications === 1 ? "application" : "applications";
  return (
    <>
      <Count value={apps.length} /> {what}
      {sources.length > 0 ? ` across ${joinNames(sources.map(sourceLabel))}` : ""}
      {loading ? ". Asking the sources again…" : ""}
    </>
  );
}

function InstalledRow({ app, onOpen, targets }: { app: App; onOpen: (app: App) => void; targets: Record<string, string | null> }) {
  const installed = installedEditions(app);
  // The row's badges are the installed editions; the detail page gets the whole application.
  const view = installed.length === app.editions.length ? app : { ...app, editions: installed };
  const first = installed[0]?.package;
  const versions = installed
    .filter((e) => e.package.installed_version)
    .map((e) => `${sourceLabel(e.package.source)} ${e.package.installed_version}`)
    .join(", ");
  const figure = first?.installed_version ? <Figure title={`Installed: ${versions}.`}>{first.installed_version}</Figure> : null;

  const remove = (e: Edition) => {
    const { source, id } = e.package;
    void startOps([{ op: "remove", package: { source, id } }], installed.length > 1 ? `Remove ${app.name} (${sourceLabel(source)})` : `Remove ${app.name}`);
  };

  // The first installed edition that has something to open; most applications have one.
  const openable = installed.find((e) => targets[launchKey(e.package)]);
  const open = openable ? (
    <Button kind="ghost" icon={<Play {...ICON} aria-hidden="true" />} title={`Opens ${targets[launchKey(openable.package)]}.`} onClick={() => void openPackage(openable.package, app.name)}>
      Open
    </Button>
  ) : null;

  let action: ReactNode = null;
  if (installed.length === 1) {
    action = (
      <Button title={`Remove ${app.name} from ${sourceLabel(installed[0].package.source)}.`} onClick={() => remove(installed[0])}>
        Remove…
      </Button>
    );
  } else if (installed.length > 1) {
    const options: DropdownOption<string>[] = installed.map((e) => ({
      value: refKey(e.package),
      label: sourceLabel(e.package.source),
      figure: e.package.installed_version ?? undefined,
    }));
    action = (
      <Dropdown
        name="Remove…"
        className="bl-dd-action"
        align="right"
        options={options}
        value={null}
        onChange={(key) => {
          const e = installed.find((x) => refKey(x.package) === key);
          if (e) remove(e);
        }}
      />
    );
  }

  return (
    <AppRow
      app={view}
      onOpen={() => onOpen(app)}
      figure={figure}
      action={
        open ? (
          <span className="bk-inline">
            {open}
            {action}
          </span>
        ) : (
          action
        )
      }
    />
  );
}

export function InstalledPage() {
  const shellSources = useShell((s) => s.sources);
  const settings = useShell((s) => s.settings);
  const openApp = useShell((s) => s.openApp);
  const result = useInstalled((s) => s.result);
  const loading = useInstalled((s) => s.loading);
  const error = useInstalled((s) => s.error);
  const query = useInstalled((s) => s.query);
  const sources = useInstalled((s) => s.sources);
  const scope = useInstalled((s) => s.scope);
  const sort = useInstalled((s) => s.sort);
  const { load, setQuery, setSources, setScope, setSort } = useInstalled.getState();
  // `brokey --show` named something that is not installed (shell/show.ts). It stays until dismissed or the page is left.
  const missing = useShow((s) => s.missing);
  const clearMissing = useShow((s) => s.clearMissing);
  useEffect(() => clearMissing, [clearMissing]);

  useEffect(() => {
    void load();
    // A finished transaction changes what is installed; ask again rather than show the old list.
    return onPlanDone(() => void load());
  }, [load]);

  const options = useMemo(() => sourceOptions(shellSources), [shellSources]);
  const chosen = useMemo(() => sources ?? availableKinds(shellSources), [sources, shellSources]);
  const effectiveScope: Scope = scope ?? (settings?.show_packages ? "all" : "apps");
  const apps = result?.apps ?? null;

  const shown = useMemo(() => {
    if (!apps) return [];
    const wanted = new Set<SourceKind>(chosen);
    return apps
      .filter((a) => effectiveScope === "all" || a.kind === "app")
      .filter((a) => installedEditions(a).some((e) => wanted.has(e.package.source)))
      .filter((a) => matches(a, query))
      .sort(compare(sort));
  }, [apps, chosen, effectiveScope, query, sort]);

  const open = (app: App) => {
    rememberApp(app);
    openApp(app.key);
  };

  // Only applications have something to open, so only their editions are asked about.
  const openRefs = useMemo(
    () => shown.filter((a) => a.kind === "app").flatMap((a) => installedEditions(a).map((e) => ({ source: e.package.source, id: e.package.id }))),
    [shown],
  );
  const targets = useLaunchTargets(openRefs);

  const filtered = query.trim() !== "" || sources !== null || scope !== null;
  const clearFilter = () => {
    setQuery("");
    setSources(availableKinds(shellSources));
    setScope(settings?.show_packages ? "all" : "apps");
  };

  let list: ReactNode;
  if (!apps) {
    list = loading ? (
      <div className="bk-well">
        <SkeletonAppRows count={8} />
      </div>
    ) : null;
  } else if (apps.length === 0) {
    list = <EmptyState fill icon={<HardDrive {...ICON_EMPTY} aria-hidden="true" />}>Nothing is installed from the sources Brokey can read.</EmptyState>;
  } else if (shown.length === 0) {
    list = (
      <EmptyState
        fill
        icon={<HardDrive {...ICON_EMPTY} aria-hidden="true" />}
        action={
          filtered ? (
            <Button onClick={clearFilter}>Clear filter</Button>
          ) : undefined
        }
      >
        Nothing installed matches this filter.
      </EmptyState>
    );
  } else {
    list = (
      <div className="bk-well">
        <div className="bk-list">
          {shown.map((app) => (
            <InstalledRow key={app.key} app={app} onOpen={open} targets={targets} />
          ))}
        </div>
      </div>
    );
  }

  return (
    <div className="bl-page">
      <div className="bk-toolbar bl-toolbar">
        <SearchField label="Filter installed" value={query} onChange={setQuery} placeholder="Filter by name or summary" width="calc(var(--s6) * 8)" />
        <MultiSelect<SourceKind> name="Sources" icon={<Layers {...ICON} aria-hidden="true" />} options={options} values={chosen} onChange={setSources} />
        <Segmented<Scope> name="Which packages to list" options={SCOPES} value={effectiveScope} onChange={setScope} />
        <Dropdown<InstalledSort> name="Sort" icon={<ArrowUpDown {...ICON} aria-hidden="true" />} options={SORTS} value={sort} onChange={setSort} />
        <div className="bk-toolbar-end">
          {apps ? (
            <span className="bk-small bk-dim">
              <Count value={shown.length} /> shown
            </span>
          ) : null}
          <Button kind="outline" icon={<RefreshCw {...ICON} aria-hidden="true" />} title="Ask every source again." onClick={() => void load()} {...off(loading, "The sources are being asked now.")}>
            Refresh
          </Button>
        </div>
      </div>
      <div className="bk-page bl-body">
        <div className="bk-page-head">
          <h1 className="bk-page-title">Installed</h1>
          <div className="bk-page-sub">
            <HeadLine apps={apps} loading={loading} />
          </div>
        </div>
        {missing ? (
          <Notice
            actions={
              <Button kind="ghost" onClick={clearMissing}>
                Dismiss
              </Button>
            }
          >
            {missingSentence(missing)}
          </Notice>
        ) : null}
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
        {result?.failed.map(([kind, sentence]) => (
          <Notice key={kind}>{failedSentence(kind, sentence, "read")}</Notice>
        ))}
        {list}
      </div>
    </div>
  );
}
