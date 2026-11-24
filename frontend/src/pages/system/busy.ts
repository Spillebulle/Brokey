// Which packages a running plan is already working on, read from the activity
// store, so a row's button can say "being installed" instead of starting a
// second plan for the same thing.

import { useMemo } from "react";
import { useActivity } from "../../activity/store";
import type { PackageRef, SourceKind } from "../../types";

const LIVE = new Set(["pending", "authorising", "running"]);

export interface Busy {
  /** "source:id" for every package a live plan names. */
  refs: Set<string>;
  /** Sources with a live update-all. */
  sources: Set<SourceKind>;
  /** Sources whose tool a live plan is setting up. */
  setups: Set<SourceKind>;
}

export function refKey(ref: PackageRef): string {
  return `${ref.source}:${ref.id}`;
}

export function useBusy(): Busy {
  const plans = useActivity((s) => s.plans);
  return useMemo(() => {
    const refs = new Set<string>();
    const sources = new Set<SourceKind>();
    const setups = new Set<SourceKind>();
    for (const status of Object.values(plans)) {
      if (!LIVE.has(status.state)) continue;
      for (const op of status.plan.ops) {
        if (op.op === "updateall") sources.add(op.source);
        else if (op.op === "setup") setups.add(op.source);
        else if (op.op === "refresh" || op.op === "clearleftovers" || op.op === "restore" || op.op === "emptyholding") continue;
        else refs.add(refKey(op.package));
      }
    }
    return { refs, sources, setups };
  }, [plans]);
}

/** Whether a live plan already covers this package, on its own or through an update-all of its source. */
export function isBusy(busy: Busy, ref: PackageRef): boolean {
  return busy.refs.has(refKey(ref)) || busy.sources.has(ref.source);
}
