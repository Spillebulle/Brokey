import { Check, ChevronLeft, ChevronRight, Download, ExternalLink, Package, Play, RotateCw, Trash } from "lucide-react";
import { Fragment, useEffect, useMemo, useState, type ReactNode } from "react";
import { startOps } from "../activity/flow";
import { openPackage, useLaunch, useLaunchTarget } from "../activity/launch";
import { useActivity } from "../activity/store";
import * as api from "../api";
import {
  AppIcon,
  ArtCard,
  Badge,
  Button,
  Bytes,
  Description,
  Dialog,
  Dropdown,
  EmptyState,
  Figure,
  ICON,
  ICON_EMPTY,
  ICON_MARK,
  MediaRail,
  Notice,
  Skeleton,
  toast,
  When,
  type DropdownOption,
} from "../components";
import { formatCount, looksNumeric } from "../format";
import { useShell } from "../shell/store";
import type { App, Edition, Package as Pkg, Screenshot } from "../types";
import { editionKey, editionLabel, installTitle, needsSetup, opsToInstall, refKey, refOf, setupNote, useBusyRefs } from "./app/editions";
import { findApp, useSearch } from "./search/store";
import "./app/app.css";

/** "PackageManager" reads as code; "Package Manager" reads as a word. The ids are AppStream's, so nothing else is changed. */
function categoryLabel(id: string): string {
  return id.replace(/([a-z])([A-Z])/g, "$1 $2");
}

interface Fact {
  key: string;
  value: ReactNode;
  /** A figure: mono, tabular, strong. */
  mono?: boolean;
}

/** The key/value list (§10). Values can be components here, which the Facts component does not take. */
function FactList({ items }: { items: Fact[] }) {
  if (items.length === 0) return null;
  return (
    <dl className="bk-facts">
      {items.map((f, i) => (
        <Fragment key={`${f.key}-${i}`}>
          <dt>{f.key}</dt>
          <dd className={f.mono ? "n" : undefined}>{f.value}</dd>
        </Fragment>
      ))}
    </dl>
  );
}

function FactSkeleton() {
  return (
    <div className="bk-appdetail-skel" aria-busy="true" aria-label="Loading the details">
      {[34, 52, 28, 46, 40, 58].map((w, i) => (
        <div key={i} className="bk-appdetail-skel-row">
          <Skeleton width="12%" className="bk-skel--text" />
          <Skeleton width={`${w}%`} className="bk-skel--text" />
        </div>
      ))}
    </div>
  );
}

function DescriptionSkeleton() {
  return (
    <div className="bk-appdetail-skel" aria-busy="true" aria-label="Loading the description">
      <Skeleton width="62%" className="bk-skel--text" />
      <Skeleton width="58%" className="bk-skel--text" />
      <Skeleton width="44%" className="bk-skel--text" />
    </div>
  );
}

function facts(p: Pkg): Fact[] {
  const out: Fact[] = [];
  if (p.version) out.push({ key: "Version", value: p.version, mono: true });
  if (p.installed && p.installed_version) out.push({ key: "Installed version", value: p.installed_version, mono: true });
  if (p.repo) out.push({ key: "Repository", value: p.repo });
  if (p.licence) out.push({ key: "Licence", value: p.licence });
  if (p.developer) out.push({ key: "Developer", value: p.developer });
  if (p.updated !== null) out.push({ key: "Updated", value: <When value={p.updated} />, mono: true });
  if (p.download_size !== null) out.push({ key: "Download size", value: <Bytes value={p.download_size} />, mono: true });
  if (p.installed_size !== null) out.push({ key: "Installed size", value: <Bytes value={p.installed_size} />, mono: true });
  if (p.popularity_label) out.push({ key: "Popularity", value: p.popularity_label });
  if (p.homepage) {
    const url = p.homepage;
    out.push({
      key: "Homepage",
      value: (
        <a
          href={url}
          title="Opens in the browser."
          onClick={(e) => {
            e.preventDefault();
            void api.openUrl(url);
          }}
        >
          {url}
        </a>
      ),
    });
  }
  const taken = new Set(out.map((f) => f.key.toLowerCase()));
  for (const [key, value] of p.facts) {
    // The PKGBUILD has its own block, and Updates is drawn beside Rebuild.
    if (key === "PKGBUILD" || key === "Updates" || taken.has(key.toLowerCase())) continue;
    out.push({ key, value, mono: looksNumeric(value) });
  }
  return out;
}

function Detail({ app }: { app: App }) {
  const back = useShell((s) => s.back);
  const statuses = useShell((s) => s.sources);
  const busy = useBusyRefs();

  const refs = useMemo(() => app.editions.map(refOf), [app]);
  const refsKey = refs.map(refKey).join(",");

  // Every finished plan that touched one of these packages: a change means the details are read again.
  const doneStamp = useActivity((s) =>
    s.order
      .filter((id) => {
        const p = s.plans[id];
        return p && p.state === "done" && p.plan.ops.some((op) => "package" in op && refsKey.split(",").includes(refKey(op.package)));
      })
      .join(","),
  );

  const [details, setDetails] = useState<Record<string, Pkg>>({});
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [chosenKey, setChosenKey] = useState<string | null>(null);
  const [showPkgbuild, setShowPkgbuild] = useState(false);
  const [shot, setShot] = useState<number | null>(null);

  useEffect(() => {
    let live = true;
    setLoading(true);
    setError(null);
    api
      .app_details(refs)
      .then((packages) => {
        if (!live) return;
        setDetails(Object.fromEntries(packages.map((p) => [refKey(p), p])));
      })
      .catch((e: unknown) => {
        if (live) setError(e instanceof Error ? e.message : String(e));
      })
      .finally(() => {
        if (live) setLoading(false);
      });
    return () => {
      live = false;
    };
    // refs is derived from refsKey; doneStamp asks for a fresh read after a plan.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [refsKey, doneStamp]);

  const editions = useMemo<Edition[]>(
    () => app.editions.map((e) => ({ ...e, package: details[editionKey(e)] ?? e.package })),
    [app, details],
  );
  const chosen = editions.find((e) => editionKey(e) === chosenKey) ?? editions.find((e) => e.package.installed) ?? editions[0];
  const pkg = chosen.package;

  const shots: Screenshot[] = pkg.screenshots.length > 0 ? pkg.screenshots : (editions.find((e) => e.package.screenshots.length > 0)?.package.screenshots ?? []);
  const backdrop = shots[0] ?? null;
  const description = pkg.description ?? editions.find((e) => e.package.description)?.package.description ?? null;
  const pkgbuild = pkg.facts.find(([k]) => k === "PKGBUILD")?.[1] ?? null;
  // An installed version-control package (foo-git) whose version only
  // changes when it is built: the source says so, and Rebuild is the update.
  const rebuildNote = pkg.installed ? (pkg.facts.find(([k]) => k === "Updates")?.[1] ?? null) : null;
  const homepage = pkg.homepage ?? editions.find((e) => e.package.homepage)?.package.homepage ?? null;
  const busyOp = busy.get(editionKey(chosen)) ?? null;
  const setupSentence = setupNote(pkg.source, statuses);
  const openTarget = useLaunchTarget(pkg.installed ? refOf(chosen) : null);
  const launcherNotice = useLaunch((s) => s.notices[pkg.source]);

  const editionOptions: DropdownOption[] = editions.map((e) => ({
    value: editionKey(e),
    label: editionLabel(e),
    hint: needsSetup(e, statuses) ? "not installed" : undefined,
  }));

  // The edition's tool may be missing; editions.ts decides whether a setup comes first.
  const install = () => {
    void startOps(opsToInstall(chosen, statuses), installTitle(app, chosen, statuses));
  };
  const remove = () => {
    void startOps([{ op: "remove", package: refOf(chosen) }], `Remove ${app.name}`);
  };
  const rebuild = () => {
    void startOps([{ op: "update", package: refOf(chosen) }], `Rebuild ${app.name}`);
  };
  const split = async () => {
    try {
      await api.group_split(refOf(chosen));
      toast("Split. It will be its own row from now on.", "good");
      useSearch.getState().refresh();
      back();
    } catch (e) {
      toast(e instanceof Error ? e.message : String(e), "error");
    }
  };

  // Backspace or Escape goes back, unless something else owns the key: a
  // dialog, an open menu, or a field being typed in.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.defaultPrevented || (e.key !== "Escape" && e.key !== "Backspace")) return;
      if (document.querySelector(".bk-dimmer, .bk-menu")) return;
      const target = e.target as HTMLElement | null;
      if (target && (target.tagName === "INPUT" || target.tagName === "TEXTAREA" || target.isContentEditable)) return;
      e.preventDefault();
      back();
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [back]);

  // Inside the screenshot dialog the arrows walk the set.
  useEffect(() => {
    if (shot === null || shots.length < 2) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "ArrowRight") setShot((shot + 1) % shots.length);
      else if (e.key === "ArrowLeft") setShot((shot - 1 + shots.length) % shots.length);
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [shot, shots.length]);

  const current = shot !== null ? shots[shot] : null;
  const shotTitle = (s: Screenshot, i: number) => s.caption ?? `Screenshot ${formatCount(i + 1)}`;
  const shotAlt = (s: Screenshot, i: number) => s.caption ?? `${app.name}, screenshot ${formatCount(i + 1)}`;

  return (
    <div className="bk-appdetail">
      <div className="bk-toolbar">
        <Button kind="ghost" icon={<ChevronLeft {...ICON} aria-hidden="true" />} onClick={back} title="Back to the list. Backspace does the same.">
          Back
        </Button>
      </div>
      <div className="bk-appdetail-scroll">
        <div className="bk-detail-head">
          {backdrop ? (
            <div className="bk-backdrop">
              <img src={api.pictureSrc(backdrop.image) ?? undefined} alt="" aria-hidden="true" />
              <div className="bk-backdrop-fade" />
            </div>
          ) : (
            <div className="bk-backdrop bk-backdrop--none" />
          )}
          <div className={backdrop ? "bk-hero bk-hero--over" : "bk-hero"}>
            <AppIcon picture={app.icon ?? pkg.icon} name={app.name} size="hero" />
            <div className="bk-hero-text">
              <h1 className="bk-hero-title">{app.name}</h1>
              {app.developer || app.summary || pkg.summary ? (
                <div className="bk-hero-sub">{[app.developer ?? pkg.developer, app.summary ?? pkg.summary].filter(Boolean).join(" · ")}</div>
              ) : null}
              {app.categories.length > 0 ? (
                <div className="bk-inline bk-wrap bk-appdetail-cats">
                  {app.categories.map((c) => (
                    <Badge key={c}>{categoryLabel(c)}</Badge>
                  ))}
                </div>
              ) : null}
              <div className="bk-hero-actions">
                <Dropdown name="Edition" alone form className="bk-appdetail-edition" options={editionOptions} value={editionKey(chosen)} onChange={setChosenKey} />
                {pkg.installed ? (
                  <>
                    {openTarget ? (
                      <Button kind="primary" icon={<Play {...ICON} aria-hidden="true" />} onClick={() => void openPackage(refOf(chosen), app.name)} title={`Opens ${openTarget}.`}>
                        Open
                      </Button>
                    ) : null}
                    <Badge tone="good" icon={<Check {...ICON_MARK} aria-hidden="true" />} title={pkg.installed_version ? `Version ${pkg.installed_version} is on this machine.` : "This edition is on this machine."}>
                      Installed
                    </Badge>
                    {rebuildNote ? (
                      busyOp === "update" ? (
                        <Button disabled disabledReason="It is being rebuilt. The activity panel shows progress.">
                          Rebuilding…
                        </Button>
                      ) : (
                        <Button icon={<RotateCw {...ICON} aria-hidden="true" />} onClick={rebuild} title="Builds the latest commits and installs the result.">
                          Rebuild
                        </Button>
                      )
                    ) : null}
                    {busyOp === "remove" ? (
                      <Button disabled disabledReason="It is being removed. The activity panel shows progress.">
                        Removing…
                      </Button>
                    ) : (
                      <Button icon={<Trash {...ICON} aria-hidden="true" />} onClick={remove} title="Asks before anything is removed.">
                        Remove…
                      </Button>
                    )}
                  </>
                ) : busyOp === "install" ? (
                  <Button kind="primary" disabled disabledReason="It is being installed. The activity panel shows progress.">
                    Installing…
                  </Button>
                ) : (
                  <Button kind="primary" icon={<Download {...ICON} aria-hidden="true" />} onClick={install} title={setupSentence ? `Install ${editionLabel(chosen)}. ${setupSentence}` : `Install ${editionLabel(chosen)}.`}>
                    Install
                  </Button>
                )}
                {homepage ? (
                  <Button kind="ghost" icon={<ExternalLink {...ICON} aria-hidden="true" />} onClick={() => void api.openUrl(homepage)} title={homepage}>
                    Website
                  </Button>
                ) : null}
              </div>
              {rebuildNote ? (
                <div className="bk-appdetail-note">
                  <span className="bk-hero-note">{rebuildNote}</span>
                </div>
              ) : null}
              {pkg.installed && launcherNotice ? (
                <div className="bk-appdetail-note">
                  <span className="bk-hero-note">Your launcher does not list it until you log out and back in once. Open works now.</span>
                </div>
              ) : null}
              {setupSentence && !pkg.installed ? (
                <div className="bk-appdetail-note">
                  <span className="bk-hero-note">{setupSentence}</span>
                </div>
              ) : null}
              {chosen.matched_by === "name" ? (
                <div className="bk-appdetail-note">
                  <span className="bk-hero-note">Matched by name, not by id.</span>
                  <Button kind="ghost" onClick={() => void split()} title="Lists this edition on its own from now on.">
                    Not the same application? Split
                  </Button>
                </div>
              ) : null}
            </div>
          </div>
        </div>

        <div className="bk-page bk-appdetail-body">
          {error ? <Notice>{error}</Notice> : null}

          <section className="bk-appdetail-section" aria-label="Details">
            <h3 className="bk-section-title">Details</h3>
            {loading ? <FactSkeleton /> : <FactList items={facts(pkg)} />}
          </section>

          {pkgbuild ? (
            <section className="bk-appdetail-section" aria-label="PKGBUILD">
              <div className="bk-appdetail-section-head">
                <h3 className="bk-section-title">PKGBUILD</h3>
                <Button kind="ghost" onClick={() => setShowPkgbuild((v) => !v)}>
                  {showPkgbuild ? "Hide" : "Show"}
                </Button>
              </div>
              {showPkgbuild ? <pre className="bk-appdetail-code">{pkgbuild}</pre> : null}
            </section>
          ) : null}

          {loading || description ? (
            <section className="bk-appdetail-section" aria-label="About">
              <h3 className="bk-section-title">About</h3>
              {loading && !description ? <DescriptionSkeleton /> : <Description markup={description} />}
            </section>
          ) : null}

          {shots.length > 0 ? (
            <MediaRail title="Screenshots" count={shots.length}>
              {shots.map((s, i) => (
                <ArtCard
                  key={i}
                  picture={s.thumbnail ?? s.image}
                  alt={shotAlt(s, i)}
                  size="hero"
                  landscape
                  title={shotTitle(s, i)}
                  figure={s.width && s.height ? `${formatCount(s.width)} × ${formatCount(s.height)}` : undefined}
                  onClick={() => setShot(i)}
                />
              ))}
            </MediaRail>
          ) : null}
        </div>
      </div>

      <Dialog
        open={current !== null}
        size="large"
        title={current && shot !== null ? shotTitle(current, shot) : ""}
        subtitle={app.name}
        onClose={() => setShot(null)}
        note={
          shot !== null ? (
            <span>
              <Figure>{formatCount(shot + 1)}</Figure> of <Figure>{formatCount(shots.length)}</Figure>
            </span>
          ) : undefined
        }
        actions={
          shots.length > 1 ? (
            <>
              <Button kind="ghost" icon={<ChevronLeft {...ICON} aria-hidden="true" />} onClick={() => setShot(((shot ?? 0) - 1 + shots.length) % shots.length)} title="The previous screenshot. Left arrow does the same.">
                Previous
              </Button>
              <Button kind="ghost" onClick={() => setShot(((shot ?? 0) + 1) % shots.length)} title="The next screenshot. Right arrow does the same.">
                Next
                <ChevronRight {...ICON} aria-hidden="true" />
              </Button>
            </>
          ) : undefined
        }
      >
        {current && shot !== null ? (
          <div className="bk-appdetail-shot">
            <img src={api.pictureSrc(current.image) ?? undefined} alt={shotAlt(current, shot)} />
          </div>
        ) : null}
      </Dialog>
    </div>
  );
}

export function AppDetailPage({ appKey }: { appKey?: string }) {
  const back = useShell((s) => s.back);
  const settings = useShell((s) => s.settings);
  const statuses = useShell((s) => s.sources);
  const app = useSearch((s) => (appKey ? findApp(s, appKey) : undefined));
  const searching = useSearch((s) => s.loading);
  const initialised = useSearch((s) => s.initialised);
  const seeded = useSearch((s) => s.text.trim().length >= 2);
  const init = useSearch((s) => s.init);

  // Opened by URL (a screenshot, the dev server) before the search page has
  // run: the store starts from settings here too and runs the seeded query,
  // which is where the application will be found.
  useEffect(() => {
    if (settings && statuses.length > 0) init(settings, statuses);
  }, [settings, statuses, init]);

  if (!app && (searching || (!initialised && seeded))) {
    // A search that may hold this application is still running: the hero's geometry, shimmering.
    return (
      <div className="bk-page">
        <div className="bk-appdetail-skel-row" aria-busy="true" aria-label="Loading">
          <Skeleton width="var(--app-icon-hero)" height="var(--app-icon-hero)" />
          <div className="bk-appdetail-skel bk-grow">
            <Skeleton width="30%" />
            <Skeleton width="52%" className="bk-skel--text" />
          </div>
        </div>
      </div>
    );
  }
  if (!app) {
    return (
      <EmptyState
        fill
        icon={<Package {...ICON_EMPTY} aria-hidden="true" />}
        action={
          <Button icon={<ChevronLeft {...ICON} aria-hidden="true" />} onClick={back}>
            Back
          </Button>
        }
      >
        This application is no longer in the results.
      </EmptyState>
    );
  }
  return <Detail key={app.key} app={app} />;
}
