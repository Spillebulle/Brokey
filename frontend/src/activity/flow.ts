// The one way a page starts a transaction. Pages call `startOps` with the
// operations they want and a title; what happens next is this module's
// business, so every page starts an install the same way:
//
//   1. api.plan(ops) previews the steps and any sentence worth reading first;
//   2. <FlowDialog/> (mounted once in App.tsx) shows them and asks;
//   3. on confirm api.run_plan(ops) starts it and the activity panel tracks it.
//
// The promise startOps returns resolves when the dialog closes: with the
// plan's id when it started, null when it was cancelled or could not start.

import { create } from "zustand";
import * as api from "../api";
import { toast } from "../components/toastStore";
import { formatCount, plural } from "../format";
import { sourceLabel, type Op, type PlanPreview, type PlanStatus, type SourceKind, type Step } from "../types";
import { useActivity } from "./store";

/** "Turn on" and "Turn off" are a Windows feature's words: it is switched, never installed or removed. */
export type Verb = "Install" | "Remove" | "Update" | "Apply" | "Turn on" | "Turn off" | "Move" | "Restore" | "Empty";

export interface FlowState {
  open: boolean;
  title: string;
  /** A sentence the caller wants read first, in place of the removal sentence. */
  intro: string | null;
  ops: Op[];
  /** What api.plan answered; null until it has. */
  preview: PlanPreview | null;
  /** api.plan or api.run_plan failed with this sentence. */
  error: string | null;
  /** run_plan is in flight: the dialog refuses to close. */
  starting: boolean;
  /** Distinguishes a preview that arrives after the dialog was closed and reopened. */
  generation: number;
  confirm: () => Promise<void>;
  cancel: () => void;
}

let settle: ((id: string | null) => void) | null = null;

function message(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

function close(id: string | null) {
  const done = settle;
  settle = null;
  useFlow.setState({ open: false, preview: null, error: null, starting: false });
  done?.(id);
}

/** What run_plan answered goes to the panel; the same step for a self-update. */
export function trackPlan(status: PlanStatus): string {
  useActivity.getState().track(status);
  return status.plan.id;
}

export const useFlow = create<FlowState>((set, get) => ({
  open: false,
  title: "",
  intro: null,
  ops: [],
  preview: null,
  error: null,
  starting: false,
  generation: 0,

  confirm: async () => {
    const { ops, preview, starting, generation } = get();
    if (!preview || starting) return;
    set({ starting: true, error: null });
    try {
      const status = await api.run_plan(ops);
      if (get().generation !== generation) return;
      close(trackPlan(status));
    } catch (e) {
      if (get().generation !== generation) return;
      set({ starting: false, error: message(e) });
    }
  },

  cancel: () => {
    if (get().starting) return;
    close(null);
  },
}));

/** Start a plan for these operations. Resolves with the plan's id, or null when it did not start. */
export function startOps(ops: Op[], title: string, intro: string | null = null): Promise<string | null> {
  if (useFlow.getState().open) {
    toast("Another transaction is waiting to be confirmed. Finish with that one first.", "caution");
    return Promise.resolve(null);
  }
  if (ops.length === 0) {
    toast("Nothing to do. Everything asked for is already in place.");
    return Promise.resolve(null);
  }
  return new Promise((resolve) => {
    settle = resolve;
    const generation = useFlow.getState().generation + 1;
    useFlow.setState({ open: true, title, intro, ops, preview: null, error: null, starting: false, generation });
    api
      .plan(ops)
      .then((preview) => {
        const now = useFlow.getState();
        if (!now.open || now.generation !== generation) return;
        if (preview.plan.steps.length === 0) {
          toast("Nothing to do. Everything asked for is already in place.");
          close(null);
          return;
        }
        useFlow.setState({ preview });
      })
      .catch((e) => {
        const now = useFlow.getState();
        if (!now.open || now.generation !== generation) return;
        useFlow.setState({ error: message(e) });
      });
  });
}

// ── Words for the dialog ────────────────────────────────────────────────────

/** The primary button's word: what the operations do. A setup is an install of the tool, so it never changes the word. */
export function verbFor(ops: Op[]): Verb {
  const kinds = new Set(ops.filter((o) => o.op !== "setup").map((o) => o.op));
  const features = ops.length > 0 && ops.every((o) => "package" in o && o.package.source === "features");
  if (features && kinds.size === 1 && kinds.has("install")) return "Turn on";
  if (features && kinds.size === 1 && kinds.has("remove")) return "Turn off";
  if (kinds.size === 1 && kinds.has("clearleftovers")) return "Move";
  if (kinds.size === 1 && kinds.has("restore")) return "Restore";
  if (kinds.size === 1 && kinds.has("emptyholding")) return "Empty";
  if (kinds.size === 1 && kinds.has("forceremove")) return "Remove";
  if (kinds.size === 0) return "Install";
  if (kinds.size === 1 && kinds.has("install")) return "Install";
  if (kinds.size === 1 && kinds.has("remove")) return "Remove";
  if ([...kinds].every((k) => k === "update" || k === "updateall" || k === "refresh")) return "Update";
  return "Apply";
}

/** "the AUR", "pacman", "Flathub": a source as the object of "from". */
export function fromSource(kind: SourceKind): string {
  switch (kind) {
    case "aur":
      return "the AUR";
    case "flatpak":
      return "Flathub";
    case "snap":
      return "the Snap Store";
    case "fwupd":
      return "fwupd";
    case "chwd":
      return "chwd";
    case "msix":
      return "the Store";
    default:
      return sourceLabel(kind);
  }
}

/** "1 package from pacman, 1 from the AUR.", or "Sets up Flatpak, then 1 package from Flathub." when a tool comes first. */
export function summarise(ops: Op[]): string {
  const perSource = new Map<SourceKind, number>();
  const whole: SourceKind[] = [];
  const refreshed: SourceKind[] = [];
  const setups: SourceKind[] = [];
  for (const op of ops) {
    if (op.op === "updateall") whole.push(op.source);
    else if (op.op === "refresh") refreshed.push(op.source);
    else if (op.op === "setup") setups.push(op.source);
    else if (op.op === "clearleftovers" || op.op === "restore" || op.op === "emptyholding") continue;
    else perSource.set(op.package.source, (perSource.get(op.package.source) ?? 0) + 1);
  }
  const parts: string[] = [];
  let first = true;
  for (const [source, n] of perSource) {
    if (source === "features") {
      parts.push(plural(n, "Windows feature"));
      first = false;
      continue;
    }
    const noun = source === "fwupd" ? "firmware update" : source === "chwd" ? "driver profile" : "package";
    parts.push(`${first ? plural(n, noun) : formatCount(n)} from ${fromSource(source)}`);
    first = false;
  }
  for (const source of whole) parts.push(`everything from ${fromSource(source)}`);
  for (const source of refreshed) parts.push(`the ${fromSource(source)} index`);
  if (setups.length > 0) {
    const tools = setups.map(sourceLabel);
    const setsUp = `Sets up ${tools.length === 1 ? tools[0] : `${tools.slice(0, -1).join(", ")} and ${tools[tools.length - 1]}`}`;
    return parts.length === 0 ? `${setsUp}.` : `${setsUp}, then ${parts.join(", ")}.`;
  }
  if (parts.length === 0) return "";
  return `${parts.join(", ")}.`;
}

/** The name a package reads as: the step that removes it says it; else the id, trimmed of a Flatpak ref's path. */
function nameOf(source: SourceKind, id: string, steps: Step[]): string {
  const step = steps.find((s) => s.source === source && s.title.startsWith("Removing ") && s.command.args.includes(id));
  if (step) return step.title.slice("Removing ".length);
  const segments = id.split("/");
  if (source === "flatpak" && segments.length >= 3) return segments[2];
  return id;
}

/** "This removes steam and what only it needed." */
export function removalSentence(ops: Op[], steps: Step[]): string | null {
  // A Windows feature turned off takes nothing else with it, so it has no sentence here.
  const names = ops.flatMap((o) => (o.op === "remove" && o.package.source !== "features" ? [nameOf(o.package.source, o.package.id, steps)] : []));
  if (names.length === 0) return null;
  if (names.length === 1) return `This removes ${names[0]} and what only it needed.`;
  if (names.length === 2) return `This removes ${names[0]} and ${names[1]} and what only they needed.`;
  const others = names.length - 2;
  return `This removes ${names[0]}, ${names[1]} and ${plural(others, "other")}, and what only they needed.`;
}
