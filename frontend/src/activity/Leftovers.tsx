// The list a removal ends with (spec section 6), in the activity panel's place
// for the plan that removed the application. Certain and Likely arrive ticked,
// Possible and Your data do not. Nothing here deletes: the one action moves
// what is ticked to the holding area, through a plan the user confirms.

import { ListChecks } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { leftovers_get } from "../api";
import { Badge, Button, Bytes, Checkbox, Figure, ICON_LG, Notice, Panel, Row } from "../components";
import { formatBytes, formatCount, plural } from "../format";
import type { Confidence, Findings, ItemKind, LeftoverItem } from "../types";
import { startOps } from "./flow";
import { useActivity } from "./store";

/** The words on a badge, exactly. */
const CONFIDENCE: Record<Confidence, string> = {
  certain: "Certain",
  likely: "Likely",
  possible: "Possible",
  yourdata: "Your data",
};

const GROUPS: { title: string; kinds: ItemKind[] }[] = [
  { title: "Files and folders", kinds: ["file", "folder"] },
  { title: "Registry", kinds: ["registry_key", "registry_value"] },
  { title: "Shortcuts", kinds: ["shortcut"] },
  { title: "Services and tasks", kinds: ["service", "task", "firewall_rule"] },
];

/** Long paths keep both ends, where the drive and the name are. The full text is the row's tooltip. */
export function middleTruncate(text: string, max = 60): string {
  if (text.length <= max) return text;
  const tail = Math.ceil((max - 1) * 0.6);
  const head = max - 1 - tail;
  return `${text.slice(0, head)}…${text.slice(text.length - tail)}`;
}

/** What the footer says about what was not offered: one sentence each, or none. */
function footerSentences(found: Findings): string[] {
  const out: string[] = [];
  if (found.kept.length > 0) {
    const n = found.kept.length;
    out.push(`${plural(n, "item")} kept because other applications use ${n === 1 ? "it" : "them"}.`);
  }
  if (found.not_offered_cloud > 0) {
    const n = found.not_offered_cloud;
    out.push(`${plural(n, "item")} in a cloud-synced folder ${n === 1 ? "was" : "were"} not offered, because removing ${n === 1 ? "it" : "them"} here would remove ${n === 1 ? "it" : "them"} from the cloud too.`);
  }
  if (found.unreadable > 0) {
    const n = found.unreadable;
    out.push(`${plural(n, "place")} could not be read, so anything in ${n === 1 ? "it" : "them"} is not listed.`);
  }
  return out;
}

/** What a row shows: a firewall rule by its name, since its id (often a GUID) only tells two same-named rules apart and stays in the tooltip. */
export function shownLocation(item: LeftoverItem): string {
  if (item.kind !== "firewall_rule") return item.location;
  const bare = item.location.replace(/^firewall:/, "");
  const cut = bare.lastIndexOf(" | ");
  return cut > 0 ? bare.slice(0, cut) : bare;
}

function ItemRow({ item, ticked, onToggle }: { item: LeftoverItem; ticked: boolean; onToggle: (on: boolean) => void }) {
  return (
    <Row
      plain
      className="lo-row"
      title={`${item.location}\n${item.reason}`}
      leading={<Checkbox hit name={`Tick ${item.location}`} checked={ticked} onChange={onToggle} />}
      trailing={
        <>
          <span className="lo-size">{item.size === null ? null : <Bytes value={item.size} />}</span>
          <span className="lo-conf">
            <Badge>{CONFIDENCE[item.confidence]}</Badge>
          </span>
        </>
      }
    >
      {middleTruncate(shownLocation(item))}
    </Row>
  );
}

/** The list for one scan, in a floating panel in the activity panel's place. */
export function Leftovers({ plan, scan }: { plan: string; scan: string }) {
  const dismiss = useActivity((s) => s.dismiss);
  const [found, setFound] = useState<Findings | null | undefined>(undefined);
  const [error, setError] = useState<string | null>(null);
  const [ticked, setTicked] = useState<Set<string>>(new Set());
  const [starting, setStarting] = useState(false);

  useEffect(() => {
    let alive = true;
    leftovers_get(scan)
      .then((f) => {
        if (!alive) return;
        setFound(f);
        setTicked(new Set((f?.items ?? []).filter((i) => i.ticked).map((i) => i.id)));
      })
      .catch((e) => {
        if (alive) setError(e instanceof Error ? e.message : String(e));
      });
    return () => {
      alive = false;
    };
  }, [scan]);

  const sections = useMemo(() => {
    if (!found) return [];
    return GROUPS.map((g) => ({ title: g.title, items: found.items.filter((i) => g.kinds.includes(i.kind)) })).filter((g) => g.items.length > 0);
  }, [found]);

  const close = () => dismiss(plan);
  const icon = <ListChecks {...ICON_LG} aria-hidden="true" />;

  if (error || found === null) {
    return (
      <Panel float className="lo-panel" title="Leftovers" icon={icon}>
        <div className="lo-body">
          <Notice
            actions={
              <Button kind="ghost" onClick={close}>
                Dismiss
              </Button>
            }
          >
            {error ?? "This scan is no longer available. Brokey keeps a scan only until it closes. Remove the application again to scan again."}
          </Notice>
        </div>
      </Panel>
    );
  }
  if (found === undefined) {
    return (
      <Panel float className="lo-panel" title="Leftovers" icon={icon}>
        <div className="lo-body">
          <p className="bk-dim">Reading what the scan found.</p>
        </div>
      </Panel>
    );
  }

  const total = found.items.reduce((sum, i) => sum + (i.size ?? 0), 0);
  const chosen = found.items.filter((i) => ticked.has(i.id));
  const footer = footerSentences(found);
  const keptNames = found.kept.map((k) => `${k.location}: ${k.used_by}`).join("\n");

  const toggle = (id: string, on: boolean) =>
    setTicked((prev) => {
      const next = new Set(prev);
      if (on) next.add(id);
      else next.delete(id);
      return next;
    });

  const move = async () => {
    setStarting(true);
    const intro = `Brokey moves ${plural(chosen.length, "item")} from ${found.name} to the holding area. Nothing is deleted yet: Settings can put them back, and Brokey empties the holding area after 30 days.`;
    // The list stays behind the clear's own panel: if the clear fails, that panel is the way back to it.
    await startOps([{ op: "clearleftovers", scan: found.scan, items: chosen.map((i) => i.id) }], `Move leftovers from ${found.name}`, intro);
    setStarting(false);
  };

  return (
    <Panel
      float
      className="lo-panel"
      title={`Leftovers from ${found.name}`}
      icon={icon}
      commands={
        found.items.length === 0 ? null : (
          <span className="lo-total">
            <Figure>{plural(found.items.length, "item")}</Figure>
            <Figure>{formatBytes(total)}</Figure>
          </span>
        )
      }
    >
      <div className="lo-body">
        {found.items.length === 0 ? (
          <p>Nothing was left behind.</p>
        ) : (
          <div className="lo-scroll">
            {sections.map((g) => (
              <section key={g.title} className="lo-group" aria-label={g.title}>
                <div className="bk-eyebrow">{g.title}</div>
                {g.items.map((item) => (
                  <ItemRow key={item.id} item={item} ticked={ticked.has(item.id)} onToggle={(on) => toggle(item.id, on)} />
                ))}
              </section>
            ))}
          </div>
        )}
        {footer.length > 0 ? (
          <div className="lo-foot">
            {footer.map((sentence, i) => (
              <p key={i} className="bk-dim bk-small" title={i === 0 && found.kept.length > 0 ? keptNames : undefined}>
                {sentence}
              </p>
            ))}
          </div>
        ) : null}
        <div className="lo-actions">
          <Button kind="ghost" onClick={close} title="Leave everything where it is.">
            {found.items.length === 0 ? "Dismiss" : "Keep everything"}
          </Button>
          {found.items.length === 0 ? null : chosen.length === 0 || starting ? (
            <Button kind="primary" disabled disabledReason={starting ? "The steps are being worked out." : "Tick what to move to the holding area."}>
              Move {formatCount(chosen.length)} to holding area
            </Button>
          ) : (
            <Button kind="primary" onClick={() => void move()} title="Moves what is ticked to the holding area. Nothing is deleted yet.">
              Move {formatCount(chosen.length)} to holding area
            </Button>
          )}
        </div>
      </div>
    </Panel>
  );
}
