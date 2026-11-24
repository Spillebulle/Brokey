// The page's one door to the Rust side: one function per Tauri command, named
// exactly as crates/brokey/src/commands.rs registers them. Inside the
// window every call is `invoke`; in a plain browser (`vite dev`, screenshots)
// the mock world in ./mock.ts answers instead, so the page renders without a
// backend. Nothing else in frontend/src imports @tauri-apps.

import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { openUrl as openerOpenUrl } from "@tauri-apps/plugin-opener";
import type {
  DriversReport,
  Event,
  FeaturesReport,
  Findings,
  HoldingListing,
  Op,
  Package,
  PackageRef,
  PlanPreview,
  PlanStatus,
  Picture,
  Query,
  SearchResult,
  SelfUpdate,
  Settings,
  Shown,
  SourceKind,
  SourceStatus,
  SystemInfo,
  UpdateList,
} from "./types";

/** Tauri stamps this on the window before any script runs; its absence means a browser. */
export const inTauri: boolean =
  typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

// The mock world carries 60 kB of fixtures, so it is loaded on demand and Vite
// splits it into a chunk the window never fetches. A static import would ship
// every Flathub description inside the application.
function mock() {
  return import("./mock");
}

/** What a command failed with, as the sentence brokey_core::Error carries. */
export class ApiError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "ApiError";
  }
}

function sentence(e: unknown): string {
  if (typeof e === "string") return e;
  if (e && typeof e === "object" && "message" in e && typeof e.message === "string") {
    return e.message;
  }
  return "The application did not answer. Restart it and try again.";
}

async function call<T>(name: string, args?: Record<string, unknown>): Promise<T> {
  try {
    return await invoke<T>(name, args);
  } catch (e) {
    throw new ApiError(sentence(e));
  }
}

export function system_info(): Promise<SystemInfo> {
  return inTauri ? call("system_info") : mock().then((m) => m.system_info());
}

export function sources(): Promise<SourceStatus[]> {
  return inTauri ? call("sources") : mock().then((m) => m.sources());
}

export function search(query: Query): Promise<SearchResult> {
  return inTauri ? call("search", { query }) : mock().then((m) => m.search(query));
}

export function installed(): Promise<SearchResult> {
  return inTauri ? call("installed") : mock().then((m) => m.installed());
}

export function app_details(refs: PackageRef[]): Promise<Package[]> {
  return inTauri ? call("app_details", { refs }) : mock().then((m) => m.app_details(refs));
}

export function updates(force: boolean): Promise<UpdateList> {
  return inTauri ? call("updates", { force }) : mock().then((m) => m.updates(force));
}

export function plan(ops: Op[]): Promise<PlanPreview> {
  return inTauri ? call("plan", { ops }) : mock().then((m) => m.plan(ops));
}

/**
 * Start a plan. The application answers with the plan's id and streams its
 * events; the status is read back so every caller gets the plan's steps at
 * once, as the mock already gives them.
 */
export function run_plan(ops: Op[]): Promise<PlanStatus> {
  if (!inTauri) return mock().then((m) => m.run_plan(ops));
  return call<string>("run_plan", { ops }).then(async (id) => {
    const known = (await active_plans()).find((s) => s.plan.id === id);
    return (
      known ?? {
        plan: { id, ops, steps: [] },
        state: "running",
        events: [],
        started: Math.floor(Date.now() / 1000),
      }
    );
  });
}

export function cancel_plan(id: string): Promise<void> {
  return inTauri ? call("cancel_plan", { id }) : mock().then((m) => m.cancel_plan(id));
}

export function active_plans(): Promise<PlanStatus[]> {
  return inTauri ? call("active_plans") : mock().then((m) => m.active_plans());
}

/** The scan a removal made, or null when this session has no scan by that id. */
export function leftovers_get(scan: string): Promise<Findings | null> {
  return inTauri ? call("leftovers_get", { scan }) : mock().then((m) => m.leftovers_get(scan));
}

/** The user saying the uninstaller a plan is waiting on has finished. */
export function await_finished(plan: string): Promise<void> {
  return inTauri ? call("await_finished", { plan }) : mock().then((m) => m.await_finished(plan));
}

/** Every holding entry, newest first, and the roots that could not be read. */
export function holding_list(): Promise<HoldingListing> {
  return inTauri ? call("holding_list") : mock().then((m) => m.holding_list());
}

export function drivers(): Promise<DriversReport> {
  return inTauri ? call("drivers") : mock().then((m) => m.drivers());
}

export function features(): Promise<FeaturesReport> {
  return inTauri ? call("features") : mock().then((m) => m.features());
}

export function settings_get(): Promise<Settings> {
  return inTauri ? call("settings_get") : mock().then((m) => m.settings_get());
}

export function settings_set(settings: Settings): Promise<Settings> {
  return inTauri ? call("settings_set", { settings }) : mock().then((m) => m.settings_set(settings));
}

export function self_update_check(force: boolean): Promise<SelfUpdate> {
  return inTauri ? call("self_update_check", { force }) : mock().then((m) => m.self_update_check(force));
}

export function self_update_apply(): Promise<PlanStatus> {
  return inTauri ? call("self_update_apply") : mock().then((m) => m.self_update_apply());
}

/** For each reference, what Open would open (an entry's file name or a command line), or null when there is nothing to open. */
export function launch_targets(refs: PackageRef[]): Promise<(string | null)[]> {
  return inTauri ? call("launch_targets", { refs }) : mock().then((m) => m.launch_targets(refs));
}

/** Open an installed package in the user's session. */
export function open_app(pkg: PackageRef): Promise<void> {
  return inTauri ? call("open_app", { package: pkg }) : mock().then((m) => m.open_app(pkg));
}

/** Sources whose installed applications the running desktop session cannot list yet, with the sentence that says so. */
export function launcher_notices(): Promise<[SourceKind, string][]> {
  return inTauri ? call("launcher_notices") : mock().then((m) => m.launcher_notices());
}

export function group_split(pkg: PackageRef): Promise<Settings> {
  return inTauri ? call("group_split", { package: pkg }) : mock().then((m) => m.group_split(pkg));
}

/**
 * Events from a running transaction, forwarded by the window as
 * `transaction://event`. Returns the unsubscribe. The Tauri listener is
 * asynchronous to set up, so the returned function waits for it before
 * unlistening rather than leaking a listener that was still being attached.
 */
export function onTransactionEvent(handler: (event: Event) => void): () => void {
  if (!inTauri) {
    const pending = mock().then((m) => m.onTransactionEvent(handler));
    return () => {
      void pending.then((unsubscribe) => unsubscribe());
    };
  }
  const pending = listen<Event>("transaction://event", (e) => handler(e.payload));
  return () => {
    void pending.then((unlisten) => unlisten());
  };
}

/** The reference Brokey was started with (`--show <source>:<id>`) or handed by a second launch, taken: null almost always, and null the second time it is asked. */
export function show_take(): Promise<PackageRef | null> {
  return inTauri ? call("show_take") : mock().then((m) => m.show_take());
}

/** The installed application holding a reference, or null when nothing installed matches it. */
export function show_resolve(reference: PackageRef): Promise<Shown | null> {
  return inTauri ? call("show_resolve", { reference }) : mock().then((m) => m.show_resolve(reference));
}

/**
 * A second `brokey --show` handed the running window a reference, sent as
 * `brokey://show` with nothing in it: the reference waits for show_take.
 * A browser has no second launch, so it never fires there.
 */
export function onShow(handler: () => void): () => void {
  if (!inTauri) return () => {};
  const pending = listen("brokey://show", () => handler());
  return () => {
    void pending.then((unlisten) => unlisten());
  };
}

/** Something an <img> can load: the URL as-is, or a file through the asset protocol. */
export function pictureSrc(picture: Picture | null | undefined): string | null {
  if (!picture) return null;
  if (picture.kind === "url") return picture.value;
  return inTauri ? convertFileSrc(picture.value) : picture.value;
}

/** A link leaves the window through the opener plugin; a browser opens a tab. */
export async function openUrl(url: string): Promise<void> {
  if (inTauri) {
    await openerOpenUrl(url);
    return;
  }
  window.open(url, "_blank", "noopener");
}
