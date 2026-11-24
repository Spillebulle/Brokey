// When a removal's uninstaller did not remove the application, the Rust side
// ends the plan with one sentence (cleanup::internal::not_removed). The page
// offers forced removal on that sentence and on nothing else: the two ends
// below are the sentence's own, and the Steam variant has neither.

import type { PackageRef, PlanStatus } from "../types";

const BEGIN = "The uninstaller did not remove ";
const END = " Forced removal clears it without the uninstaller.";

/** An Add/Remove id for a Steam game: Steam keeps its own record, and the Rust side refuses a forced removal of one. */
export function isSteamGame(ref: PackageRef): boolean {
  return /Steam App \d+$/i.test(ref.id);
}

/** The application a failed removal plan offers a forced removal of, with its name, or null. */
export function forcedRemovalOffer(status: PlanStatus): { package: PackageRef; name: string } | null {
  if (status.state !== "failed") return null;
  const op = status.plan.ops.find((o) => o.op === "remove" && o.package.source === "arp");
  if (!op || op.op !== "remove" || isSteamGame(op.package)) return null;
  for (const e of status.events) {
    const said = e.event === "step_finished" && !e.ok ? e.message : e.event === "plan_finished" && !e.ok ? e.message : null;
    const at = said === null ? -1 : said.indexOf(BEGIN);
    if (said && at >= 0 && said.includes(END)) {
      const name = said.slice(at + BEGIN.length, said.indexOf(". It may have been cancelled.", at));
      return { package: op.package, name: name || op.package.id };
    }
  }
  return null;
}

/** The sentence the confirm step reads first. */
export function forcedRemovalIntro(name: string): string {
  return `Forced removal does not run ${name}'s uninstaller. Brokey lists its folder, its registry entry and whatever else it left, and you choose what moves to the holding area.`;
}
