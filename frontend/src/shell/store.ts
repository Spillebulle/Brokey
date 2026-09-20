// The shell's state: which view is in front and how we got there, the global
// status line, and what every page needs from the machine (system, sources,
// settings). Routing is view state rather than URLs because a window has no
// address bar and no history to share; a small stack gives Back its meaning.

import { create } from "zustand";
import * as api from "../api";
import type { SelfUpdate, Settings, SourceStatus, SystemInfo } from "../types";

export type View = "search" | "app" | "installed" | "updates" | "drivers" | "settings";

export interface Location {
  view: View;
  /** The App key when `view` is "app". */
  appKey?: string;
}

const VIEWS: View[] = ["search", "app", "installed", "updates", "drivers", "settings"];
const HISTORY_LIMIT = 50;

/** Only a browser has a query string; the window never does. Screenshots and the dev server use ?view=. */
function initialLocation(): Location {
  if (api.inTauri) return { view: "search" };
  const params = new URLSearchParams(window.location.search);
  const view = params.get("view");
  const appKey = params.get("app") ?? undefined;
  if (view && VIEWS.includes(view as View)) return { view: view as View, appKey };
  return { view: "search" };
}

interface ShellState extends Location {
  history: Location[];
  go: (view: View, appKey?: string) => void;
  openApp: (appKey: string) => void;
  back: () => void;

  /** The top bar's global status: "Checking for updates…", or nothing. */
  status: string | null;
  setStatus: (status: string | null) => void;

  system: SystemInfo | null;
  sources: SourceStatus[];
  settings: Settings | null;
  /** What went wrong loading the above, as a sentence for the page. */
  loadError: string | null;
  updateCount: number;
  setUpdateCount: (count: number) => void;

  /** What self_update_check answered; null until it has, or when the check is off. */
  selfUpdate: SelfUpdate | null;
  /** "Later" was pressed: the notice stays away for this session. */
  selfUpdateHidden: boolean;
  checkSelfUpdate: (force: boolean) => Promise<void>;
  hideSelfUpdate: () => void;

  load: () => Promise<void>;
  /** Ask the application which sources it has again (after a plan: a setup may have brought a tool). A failure keeps what was known. */
  loadSources: () => Promise<void>;
  /** Settings apply live: the patch is saved and the store takes what came back. */
  saveSettings: (patch: Partial<Settings>) => Promise<void>;
}

function message(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

export const useShell = create<ShellState>((set, get) => ({
  ...initialLocation(),
  history: [],

  go: (view, appKey) => {
    const { view: here, appKey: hereKey, history } = get();
    if (view === here && appKey === hereKey) return;
    set({
      view,
      appKey,
      history: [...history.slice(-(HISTORY_LIMIT - 1)), { view: here, appKey: hereKey }],
    });
  },
  openApp: (appKey) => get().go("app", appKey),
  back: () => {
    const { history } = get();
    const previous = history[history.length - 1];
    if (!previous) {
      set({ view: "search", appKey: undefined });
      return;
    }
    set({ view: previous.view, appKey: previous.appKey, history: history.slice(0, -1) });
  },

  status: null,
  setStatus: (status) => set({ status }),

  system: null,
  sources: [],
  settings: null,
  loadError: null,
  updateCount: 0,
  setUpdateCount: (updateCount) => set({ updateCount }),

  selfUpdate: null,
  selfUpdateHidden: false,
  checkSelfUpdate: async (force) => {
    try {
      set({ selfUpdate: await api.self_update_check(force) });
    } catch {
      // A release check that fails is not news: the notice simply does not appear.
    }
  },
  hideSelfUpdate: () => set({ selfUpdateHidden: true }),

  load: async () => {
    try {
      const [system, sources, settings] = await Promise.all([api.system_info(), api.sources(), api.settings_get()]);
      set({ system, sources, settings, loadError: null });
    } catch (e) {
      set({ loadError: message(e) });
    }
  },

  loadSources: async () => {
    try {
      set({ sources: await api.sources() });
    } catch {
      // What was known stays; the next full load reports the failure.
    }
  },

  saveSettings: async (patch) => {
    const current = get().settings;
    if (!current) return;
    const next = { ...current, ...patch };
    // Optimistic: the control moves at once; the saved copy replaces it when it lands.
    set({ settings: next });
    try {
      set({ settings: await api.settings_set(next) });
    } catch (e) {
      set({ settings: current, loadError: message(e) });
      throw e;
    }
  },
}));

/** The nav row that owns the current view: an application page belongs to the page it was opened from. */
export function selectNav(state: ShellState): View {
  if (state.view !== "app") return state.view;
  for (let i = state.history.length - 1; i >= 0; i -= 1) {
    if (state.history[i].view !== "app") return state.history[i].view;
  }
  return "search";
}

/** Whether this window is talking to a Windows machine. Linux is the default while `system` is still loading. */
export function selectIsWindows(state: ShellState): boolean {
  return state.system?.platform === "windows";
}
