// `brokey --show <source>:<id>`: another application (Heft) hands the user to
// one installed application's page with Remove ready. docs/brokey-handoff.md
// is the contract; crates/brokey/src/show.rs holds the reference until the
// page takes it and finds the application that holds it.
//
// The reference is taken, never pushed: the page asks with show_take when it
// loads and again whenever brokey://show arrives. An event that fired before
// the listener was attached loses nothing, because the reference is still
// waiting to be taken, and a second ask gets null. So there is no race to win.
//
// Nothing here starts a plan. The detail page selects the edition and focuses
// its Remove button; pressing it and accepting the confirm step is the user's.

import { create } from "zustand";
import * as api from "../api";
import { toast } from "../components/toastStore";
import { refKey } from "../pages/app/editions";
import { rememberApp } from "../pages/search/store";
import type { PackageRef } from "../types";
import { useShell } from "./store";

interface ShowState {
  /** The edition the detail page selects and whose Remove it focuses, as source:id. Taken once by the page. */
  focus: string | null;
  /** The reference nothing installed matched, as the caller wrote it, for the notice on Installed. */
  missing: string | null;
  clearFocus: () => void;
  clearMissing: () => void;
}

export const useShow = create<ShowState>((set) => ({
  focus: null,
  missing: null,
  clearFocus: () => set({ focus: null }),
  clearMissing: () => set({ missing: null }),
}));

/** The sentence Installed shows when nothing matched. */
export function missingSentence(reference: string): string {
  return `Nothing installed matches ${reference}. It may already have been removed.`;
}

async function show(reference: PackageRef): Promise<void> {
  const { go, openApp } = useShell.getState();
  // Installed at once: finding the application reads every source's installed list, which takes a moment on Windows.
  useShow.setState({ missing: null });
  go("installed");
  let found;
  try {
    found = await api.show_resolve(reference);
  } catch (e) {
    toast(e instanceof Error ? e.message : String(e), "error");
    return;
  }
  if (!found) {
    useShow.setState({ missing: refKey(reference) });
    return;
  }
  rememberApp(found.app);
  useShow.setState({ focus: refKey(found.edition) });
  openApp(found.app.key);
}

async function take(): Promise<void> {
  let reference: PackageRef | null;
  try {
    reference = await api.show_take();
  } catch {
    // Nothing to show is the ordinary answer; a failure to ask is no different to the user.
    return;
  }
  if (reference) await show(reference);
}

/** Listen for a hand-off, then take whatever is already waiting. Returns the unsubscribe. */
export function startShowing(): () => void {
  const unsubscribe = api.onShow(() => void take());
  void take();
  return unsubscribe;
}
