// The Holding area section of Settings: every cleanup Brokey moved aside, with
// what it is, when it was made and how big it is, and the two ways out of it.
// Restore and Delete now are plans like any other, so they are confirmed and
// logged; machine items go through the helper.

import { RotateCcw, Trash2 } from "lucide-react";
import { useEffect, useState } from "react";
import { startOps } from "../../activity/flow";
import { onPlanDone, useActivity } from "../../activity/store";
import * as api from "../../api";
import { Button, Bytes, ICON, Notice, Skeleton, toast } from "../../components";
import { plural } from "../../format";
import type { HoldingEntry, HoldingListing } from "../../types";

function when(created: string): string {
  const at = new Date(created);
  return Number.isNaN(at.getTime()) ? created : at.toLocaleDateString("en-GB", { day: "numeric", month: "short", year: "numeric" });
}

/** What reads under an entry's name. */
function detail(e: HoldingEntry): string {
  const kind = e.scope_machine ? "Needs Administrator to restore or delete." : "Held in your account.";
  return `Held since ${when(e.created)}, ${plural(e.items, "item")}. ${kind}`;
}

function EntryRow({ entry, busy }: { entry: HoldingEntry; busy: boolean }) {
  const reason = "A change is running. Wait for it to finish.";
  return (
    <div className="bk-setting lo-entry">
      <div className="bk-setting-text">
        <span className="bk-setting-label">{entry.name}</span>
        <span className="bk-setting-note">{entry.waiting ? `${detail(entry)} Waiting to be emptied.` : detail(entry)}</span>
      </div>
      <Bytes value={entry.size} />
      <div className="bk-setting-control">
        {busy ? (
          <>
            <Button icon={<RotateCcw {...ICON} aria-hidden="true" />} disabled disabledReason={reason}>
              Restore
            </Button>
            <Button kind="danger" icon={<Trash2 {...ICON} aria-hidden="true" />} disabled disabledReason={reason}>
              Delete now
            </Button>
          </>
        ) : (
          <>
            <Button icon={<RotateCcw {...ICON} aria-hidden="true" />} title="Puts everything back where it was. Anything that has a new owner there is left in the holding area." onClick={() => void startOps([{ op: "restore", holding: entry.id }], `Restore ${entry.name}`, `Brokey puts ${entry.name}'s leftovers back where they were.`)}>
              Restore
            </Button>
            <Button kind="danger" icon={<Trash2 {...ICON} aria-hidden="true" />} title="Deletes it for good. It cannot be put back." onClick={() => void startOps([{ op: "emptyholding", holding: entry.id }], `Delete ${entry.name} now`, `Brokey deletes ${entry.name}'s leftovers from the holding area for good. They cannot be put back.`)}>
              Delete now
            </Button>
          </>
        )}
      </div>
    </div>
  );
}

/** The section's body: the list, what could not be read, the rule and Empty. */
export function Holding() {
  const [listing, setListing] = useState<HoldingListing | null>(null);
  const [error, setError] = useState<string | null>(null);
  const running = useActivity((s) => Object.values(s.plans).some((p) => ["pending", "authorising", "running"].includes(p.state)));

  const load = () => {
    api
      .holding_list()
      .then((l) => {
        setListing(l);
        setError(null);
      })
      .catch((e) => setError(e instanceof Error ? e.message : String(e)));
  };

  // Read on arrival, and again whenever a plan ends, since a clear, a restore or an empty changes it.
  useEffect(() => {
    load();
    return onPlanDone(load);
  }, []);

  if (error) {
    return (
      <Notice
        actions={
          <Button kind="ghost" onClick={load}>
            Try again
          </Button>
        }
      >
        {error}
      </Notice>
    );
  }
  if (!listing) {
    return (
      <div className="bk-setting" aria-hidden="true">
        <div className="sy-skel-lines">
          <Skeleton width="30%" />
        </div>
      </div>
    );
  }

  const empty = () => {
    void startOps([{ op: "emptyholding", holding: null }], "Empty the holding area", "Brokey deletes everything in the holding area for good. Nothing in it can be put back afterwards.").then((id) => {
      if (id === null) toast("The holding area was left as it is.");
    });
  };

  return (
    <>
      {listing.unreadable.map((place) => (
        <Notice key={place}>{`Brokey could not read ${place}, so anything held there is not listed. Check that the folder is there and that this account can open it.`}</Notice>
      ))}
      {listing.entries.length === 0 ? (
        <div className="bk-setting">
          <div className="bk-setting-text">
            <span className="bk-setting-label">Nothing is in the holding area.</span>
            <span className="bk-setting-note">What you move from a leftovers list waits here until you delete it.</span>
          </div>
        </div>
      ) : (
        listing.entries.map((e) => <EntryRow key={e.id} entry={e} busy={running} />)
      )}
      <div className="bk-setting">
        <div className="bk-setting-text">
          <span className="bk-setting-note">
            Brokey empties anything older than 30 days by itself. Items that need Administrator wait until the next change that asks for it, or until you press Empty.
          </span>
        </div>
        <div className="bk-setting-control">
          {listing.entries.length === 0 ? (
            <Button kind="danger" disabled disabledReason="There is nothing in the holding area to empty.">
              Empty
            </Button>
          ) : running ? (
            <Button kind="danger" disabled disabledReason="A change is running. Wait for it to finish.">
              Empty
            </Button>
          ) : (
            <Button kind="danger" icon={<Trash2 {...ICON} aria-hidden="true" />} title="Deletes everything in the holding area for good. Asks first." onClick={empty}>
              Empty
            </Button>
          )}
        </div>
      </div>
    </>
  );
}
