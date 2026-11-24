// Every transaction the page has seen, by plan id, with its events appended
// as they arrive. One subscription to `transaction://event` feeds it; the
// panel and the status bar read from it. A plan that finished well leaves on
// its own after six seconds; one that failed stays until it is dismissed, so
// the log can still be read.

import { create } from "zustand";
import { onTransactionEvent } from "../api";
import { namesItsOwnOutcome } from "./launch";
import { toast } from "../components/toastStore";
import type { Event, PlanState, PlanStatus } from "../types";

/** How long a finished plan's panel stays before it dismisses itself (the toast's own lifetime). */
const LINGER_MS = 6000;

interface ActivityState {
  plans: Record<string, PlanStatus>;
  /** Plan ids, oldest first. */
  order: string[];
  /** Plans the user asked to cancel and that have not yet reported the end. */
  cancelling: Record<string, true>;
  showLog: boolean;
  toggleLog: () => void;
  /** What run_plan returned: the plan's steps, before or after its first event. */
  track: (status: PlanStatus) => void;
  apply: (event: Event) => void;
  /** The user asked for a cancel; the header says so until the runner reports the end. */
  markCancelling: (id: string) => void;
  dismiss: (id: string) => void;
}

/** An event can arrive before run_plan has returned; a stub holds it until the plan is known. */
function stub(id: string): PlanStatus {
  return {
    plan: { id, ops: [], steps: [] },
    state: "pending",
    events: [],
    started: Math.floor(Date.now() / 1000),
  };
}

const LIVE: ReadonlySet<PlanState> = new Set<PlanState>(["pending", "authorising", "running"]);

/** The scan a removal ended with, or null when it made none. */
export function leftoversOf(plan: PlanStatus): { scan: string; count: number } | null {
  for (const e of plan.events) if (e.event === "leftovers") return { scan: e.scan, count: e.count };
  return null;
}

export function isLive(state: PlanState): boolean {
  return LIVE.has(state);
}

/** Listeners told when a plan ends well, so the shell can refresh what it shows (the updates count). */
const finished: Array<(status: PlanStatus) => void> = [];

export function onPlanDone(listener: (status: PlanStatus) => void): () => void {
  finished.push(listener);
  return () => {
    const at = finished.indexOf(listener);
    if (at >= 0) finished.splice(at, 1);
  };
}

export const useActivity = create<ActivityState>((set, get) => ({
  plans: {},
  order: [],
  cancelling: {},
  showLog: false,
  toggleLog: () => set((s) => ({ showLog: !s.showLog })),

  track: (status) => {
    const { plans, order } = get();
    const existing = plans[status.plan.id];
    const merged: PlanStatus = existing
      ? { ...existing, plan: status.plan, started: status.started }
      : { ...status, events: [...status.events] };
    set({
      plans: { ...plans, [status.plan.id]: merged },
      order: existing ? order : [...order, status.plan.id],
    });
  },

  apply: (event) => {
    const { plans, order, cancelling } = get();
    const current = plans[event.plan] ?? stub(event.plan);
    const next: PlanStatus = { ...current, events: [...current.events, event] };
    let ended = false;
    switch (event.event) {
      case "auth_required":
        next.state = "authorising";
        break;
      case "plan_started":
      case "step_started":
      case "progress":
      case "log":
      case "step_finished":
        if (next.state !== "authorising" || event.event === "step_started") next.state = "running";
        break;
      case "plan_finished": {
        const wasCancelled = cancelling[event.plan] !== undefined || event.message.startsWith("Cancelled");
        next.state = event.ok ? "done" : wasCancelled ? "cancelled" : "failed";
        ended = true;
        break;
      }
    }
    const rest = { ...cancelling };
    if (ended) delete rest[event.plan];
    set({
      plans: { ...plans, [event.plan]: next },
      order: plans[event.plan] ? order : [...order, event.plan],
      cancelling: rest,
    });
    if (ended && event.event === "plan_finished") {
      // A failure stays on screen until it is looked at; anything else leaves with its toast.
      // A plan that installed something is named in its own toast, with an Open action (launch.ts).
      if (!(next.state === "done" && namesItsOwnOutcome(next))) {
        toast(event.message, next.state === "done" ? "good" : next.state === "failed" ? "error" : "neutral");
      }
      // A removal that found leftovers stays until the list has been answered.
      if (next.state !== "failed" && !(next.state === "done" && leftoversOf(next) !== null)) {
        window.setTimeout(() => {
          const now = get().plans[event.plan];
          if (now && now.state === next.state) get().dismiss(event.plan);
        }, LINGER_MS);
      }
      // The list a clear came from stays until the clear has finished well, so a clear that fails or is
      // cancelled can be tried again from it.
      if (next.state === "done") {
        for (const op of next.plan.ops) {
          if (op.op !== "clearleftovers") continue;
          for (const other of Object.values(get().plans)) {
            if (other.plan.id !== next.plan.id && leftoversOf(other)?.scan === op.scan) get().dismiss(other.plan.id);
          }
        }
      }
      if (next.state === "done") for (const listener of [...finished]) listener(next);
    }
  },

  markCancelling: (id) => set((s) => ({ cancelling: { ...s.cancelling, [id]: true } })),

  dismiss: (id) => {
    const { plans, order, cancelling } = get();
    const rest = { ...plans };
    delete rest[id];
    const stillCancelling = { ...cancelling };
    delete stillCancelling[id];
    set({ plans: rest, order: order.filter((o) => o !== id), cancelling: stillCancelling });
  },
}));

/** Plans still running, newest last. */
export function selectLivePlans(state: ActivityState): PlanStatus[] {
  return state.order.map((id) => state.plans[id]).filter((p): p is PlanStatus => p !== undefined && isLive(p.state));
}

/** The plan the panel shows: the newest one still running, else the newest one that ended and has not been dismissed. */
export function selectActivePlan(state: ActivityState): PlanStatus | null {
  for (let i = state.order.length - 1; i >= 0; i -= 1) {
    const p = state.plans[state.order[i]];
    if (p && isLive(p.state)) return p;
  }
  for (let i = state.order.length - 1; i >= 0; i -= 1) {
    const p = state.plans[state.order[i]];
    if (p) return p;
  }
  return null;
}

let unsubscribe: (() => void) | null = null;

/** Subscribe once for the life of the page. Returns the unsubscribe for the caller that started it. */
export function subscribeActivity(): () => void {
  if (unsubscribe) return () => {};
  unsubscribe = onTransactionEvent((event) => useActivity.getState().apply(event));
  return () => {
    unsubscribe?.();
    unsubscribe = null;
  };
}
