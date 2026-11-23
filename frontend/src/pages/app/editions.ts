// What both pages say about an edition: its label in a dropdown, which one
// is primary, whether a transaction is already running for it, and what
// installing it takes when its source's tool is not on the machine yet (a
// setup op first, and a title that says so). The "needs setup" decision is
// made here and nowhere else.

import { useMemo } from "react";
import { useActivity } from "../../activity/store";
import type { DropdownOption } from "../../components";
import { sourceLabel, type App, type Edition, type Op, type PackageRef, type SourceKind, type SourceSetup, type SourceStatus } from "../../types";

export function refOf(e: Edition): PackageRef {
  return { source: e.package.source, id: e.package.id };
}

export function refKey(ref: PackageRef): string {
  return `${ref.source}:${ref.id}`;
}

export function editionKey(e: Edition): string {
  return refKey(refOf(e));
}

/** The edition a row speaks for: the installed one, else the first (sources are in interface order). */
export function primaryEdition(app: App): Edition | undefined {
  return app.editions.find((e) => e.package.installed) ?? app.editions[0];
}

/** "pacman · 1.0.0.85-1 · multilib", "AUR · 1.2.95.453-1", "Flatpak · 3.2.6 · flathub · installed". */
export function editionLabel(e: Edition): string {
  const p = e.package;
  const source = sourceLabel(p.source);
  const parts = [source];
  if (p.version) parts.push(p.version);
  if (p.repo && p.repo.toLowerCase() !== source.toLowerCase()) parts.push(p.repo);
  if (p.installed) parts.push("installed");
  return parts.join(" · ");
}

/** The editions as dropdown rows; an installed one is listed but cannot be chosen for an install, and one whose tool is missing reads "not installed". */
export function installOptions(app: App, statuses: SourceStatus[]): DropdownOption[] {
  return app.editions.map((e) => ({
    value: editionKey(e),
    label: editionLabel(e),
    hint: needsSetup(e, statuses) ? "not installed" : undefined,
    disabled: e.package.installed,
    disabledReason: e.package.installed ? "Already installed." : undefined,
  }));
}

const LIVE = new Set(["pending", "authorising", "running"]);

function opRef(op: Op): PackageRef | null {
  return "package" in op ? op.package : null;
}

/** Package refs with a plan still running, mapped to what that plan does to them. */
export function useBusyRefs(): Map<string, Op["op"]> {
  const stamp = useActivity((s) =>
    s.order
      .map((id) => s.plans[id])
      .filter((p) => p && LIVE.has(p.state))
      .flatMap((p) => p.plan.ops.map((op) => (opRef(op) ? `${op.op} ${refKey(opRef(op) as PackageRef)}` : "")))
      .filter(Boolean)
      .join("\n"),
  );
  return useMemo(() => {
    const map = new Map<string, Op["op"]>();
    for (const line of stamp.split("\n")) {
      if (!line) continue;
      const at = line.indexOf(" ");
      map.set(line.slice(at + 1), line.slice(0, at) as Op["op"]);
    }
    return map;
  }, [stamp]);
}

// ── Sources whose tool is missing ───────────────────────────────────────────

/** The source whose tool an install from `source` runs: the Store installs through winget, everything else through itself. */
export function toolOf(source: SourceKind): SourceKind {
  return source === "msix" ? "winget" : source;
}

/** The setup a source still needs: the tool it installs through is not available and the application knows how to make it so. */
export function setupFor(source: SourceKind, statuses: SourceStatus[]): SourceSetup | null {
  const status = statuses.find((s) => s.kind === toolOf(source));
  if (!status || status.available) return null;
  return status.setup;
}

/** Installing this edition sets its source's tool up first. */
export function needsSetup(edition: Edition, statuses: SourceStatus[]): boolean {
  return setupFor(edition.package.source, statuses) !== null;
}

/** The operations that install an edition: the tool's setup first when it is missing, then the package. A Store install sets winget up. */
export function opsToInstall(edition: Edition, statuses: SourceStatus[]): Op[] {
  const install: Op = { op: "install", package: refOf(edition) };
  return needsSetup(edition, statuses) ? [{ op: "setup", source: toolOf(edition.package.source) }, install] : [install];
}

/** "Install GIMP", or "Set up Flatpak and install GIMP" when the tool comes first. */
export function installTitle(app: App, edition: Edition, statuses: SourceStatus[]): string {
  return needsSetup(edition, statuses) ? `Set up ${sourceLabel(toolOf(edition.package.source))} and install ${app.name}` : `Install ${app.name}`;
}

/** The badge's tooltip for an edition whose tool is missing: "Flatpak is not installed. Installing from it sets Flatpak up first." */
export function setupHint(source: SourceKind, statuses: SourceStatus[]): string | null {
  if (setupFor(source, statuses) === null) return null;
  const tool = sourceLabel(toolOf(source));
  if (toolOf(source) !== source) return `${tool} is not installed. Installing from the ${sourceLabel(source)} sets ${tool} up first.`;
  return `${tool} is not installed. Installing from it sets ${tool} up first.`;
}

/** The hero's note under the edition picker: "Flatpak is not installed. Install sets it up first." */
export function setupNote(source: SourceKind, statuses: SourceStatus[]): string | null {
  if (setupFor(source, statuses) === null) return null;
  return `${sourceLabel(toolOf(source))} is not installed. Install sets it up first.`;
}

/** "pacman, AUR and Flatpak": plain English, no Oxford comma. */
export function joinAnd(words: string[]): string {
  if (words.length === 0) return "";
  if (words.length === 1) return words[0];
  return `${words.slice(0, -1).join(", ")} and ${words[words.length - 1]}`;
}
