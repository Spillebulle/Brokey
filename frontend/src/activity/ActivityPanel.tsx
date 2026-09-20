// The floating panel at the bottom right (§7.5) while a transaction runs or
// has just ended: the step in hand as the title, a rail with a fraction only
// when one is known (§7.18), "2 of 5", and the log a click away.

import { Activity, Terminal, X } from "lucide-react";
import { useEffect, useRef } from "react";
import { cancel_plan } from "../api";
import { Button } from "../components/Button";
import { Figure } from "../components/Figure";
import { IconButton } from "../components/IconButton";
import { ICON, ICON_LG } from "../components/icons";
import { Panel } from "../components/Panel";
import { Progress } from "../components/Progress";
import { toast } from "../components/toastStore";
import { selectIsWindows, useShell } from "../shell/store";
import type { PlanStatus } from "../types";
import { isLive, selectActivePlan, selectLivePlans, useActivity } from "./store";

/** How many log lines the block keeps; older ones scroll out of memory, not just out of view. */
const LOG_LINES = 200;

interface LogLine {
  line: string;
  stderr: boolean;
}

interface Snapshot {
  /** The step in hand, 1-based, and how many there are. */
  step: number | null;
  steps: number;
  stepTitle: string | null;
  fraction: number | null;
  /** The last sentence a progress event carried. */
  message: string | null;
  /** What the step that failed said. */
  failure: string | null;
  finished: string | null;
  log: LogLine[];
}

/** What the events say so far: the running step, the last fraction, the last sentence, the log. */
function snapshot(plan: PlanStatus): Snapshot {
  const s: Snapshot = {
    step: null,
    steps: plan.plan.steps.length,
    stepTitle: null,
    fraction: null,
    message: null,
    failure: null,
    finished: null,
    log: [],
  };
  for (const e of plan.events) {
    switch (e.event) {
      case "plan_started":
        if (e.steps > 0) s.steps = e.steps;
        break;
      case "step_started":
        s.step = e.step + 1;
        s.stepTitle = e.title;
        s.fraction = null;
        s.message = null;
        break;
      case "progress":
        s.fraction = e.fraction;
        if (e.message !== null) s.message = e.message;
        break;
      case "log":
        s.log.push({ line: e.line, stderr: e.stderr });
        break;
      case "step_finished":
        if (e.message) {
          s.log.push({ line: e.message, stderr: !e.ok });
          if (!e.ok) s.failure = e.message;
        }
        break;
      case "plan_finished":
        s.finished = e.message;
        break;
      case "auth_required":
        break;
    }
  }
  if (s.log.length > LOG_LINES) s.log = s.log.slice(-LOG_LINES);
  return s;
}

/** "Installing steam did not finish." out of "Installing steam did not finish. pacman said what went wrong in the log." */
function firstSentence(text: string): string {
  const at = text.indexOf(". ");
  return at >= 0 ? text.slice(0, at + 1) : text;
}

/** State shared by {@link heading} and {@link sentence}: whether the plan is being cancelled, and which platform is asking for elevation. */
interface PanelFlags {
  cancelling: boolean;
  windows: boolean;
}

/** The panel's title: the step in hand, or how it ended. */
function heading(plan: PlanStatus, snap: Snapshot, flags: PanelFlags): string {
  if (flags.cancelling && isLive(plan.state)) return "Cancelling.";
  switch (plan.state) {
    case "pending":
      return "Starting";
    case "authorising":
      return flags.windows ? "Waiting for Administrator" : "Waiting for your password";
    case "running":
      return snap.stepTitle ?? "Starting";
    case "done":
      return "Finished.";
    case "failed":
      return snap.finished ? firstSentence(snap.finished) : "The transaction did not finish.";
    case "cancelled":
      return "Cancelled.";
  }
}

/** The sentence beside the rail. With no known fraction it is the whole report, so there is always one. */
function sentence(plan: PlanStatus, snap: Snapshot, flags: PanelFlags): string | null {
  if (flags.cancelling && isLive(plan.state)) return snap.message ?? "Waiting for the current step to finish.";
  switch (plan.state) {
    case "pending":
      return "Starting.";
    case "authorising":
      return flags.windows ? "Allow the change in the Windows dialog." : "Enter your password in the system dialog.";
    case "running":
      return snap.message ?? (snap.fraction === null ? "No progress has been reported yet." : null);
    case "done":
      return snap.finished;
    case "failed":
      return snap.failure ?? snap.finished ?? "The transaction did not finish.";
    case "cancelled":
      return snap.finished ?? "Cancelled.";
  }
}

function Log({ lines }: { lines: LogLine[] }) {
  const box = useRef<HTMLPreElement>(null);
  useEffect(() => {
    const el = box.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [lines.length]);
  return (
    <pre ref={box} className="bk-activity-log" aria-label="Log" tabIndex={0}>
      {lines.length === 0 ? "Nothing has been written yet." : null}
      {lines.map((l, i) => (
        <span key={i} className={l.stderr ? "err" : undefined}>
          {l.line}
          {"\n"}
        </span>
      ))}
    </pre>
  );
}

/** The floating panel at the bottom right while a transaction runs: step, rail, log toggle. */
export function ActivityPanel() {
  const plan = useActivity(selectActivePlan);
  const liveCount = useActivity((s) => selectLivePlans(s).length);
  const cancelling = useActivity((s) => (plan ? s.cancelling[plan.plan.id] !== undefined : false));
  const showLog = useActivity((s) => s.showLog);
  const toggleLog = useActivity((s) => s.toggleLog);
  const markCancelling = useActivity((s) => s.markCancelling);
  const dismiss = useActivity((s) => s.dismiss);
  const windows = useShell(selectIsWindows);
  if (!plan) return null;

  const snap = snapshot(plan);
  const live = isLive(plan.state);
  const flags: PanelFlags = { cancelling, windows };
  const title = heading(plan, snap, flags);
  const line = sentence(plan, snap, flags);
  const fraction = plan.state === "done" ? 1 : snap.fraction;

  const cancel = async () => {
    markCancelling(plan.plan.id);
    try {
      await cancel_plan(plan.plan.id);
    } catch (e) {
      toast(e instanceof Error ? e.message : String(e), "error");
    }
  };

  return (
    <Panel
      float
      className="bk-activity"
      title={title}
      icon={<Activity {...ICON_LG} aria-hidden="true" />}
      count={liveCount > 1 ? liveCount : undefined}
      commands={
        <>
          {live && !cancelling ? (
            <Button kind="ghost" onClick={() => void cancel()} title="Stop after the current step. Nothing half done is left behind.">
              Cancel
            </Button>
          ) : null}
          {live && cancelling ? (
            <Button kind="ghost" disabled disabledReason="Cancel was asked for. The current step finishes first.">
              Cancel
            </Button>
          ) : null}
          <IconButton size="sm" label={showLog ? "Hide log" : "Show log"} active={showLog} icon={<Terminal {...ICON} aria-hidden="true" />} onClick={toggleLog} />
          {live ? null : <IconButton size="sm" label="Dismiss" icon={<X {...ICON} aria-hidden="true" />} onClick={() => dismiss(plan.plan.id)} />}
        </>
      }
    >
      <div className="bk-activity-body">
        {fraction !== null ? <Progress fraction={fraction} message={line} /> : <Progress fraction={null} message={line ?? "Working."} />}
        {snap.step !== null && snap.steps > 0 ? (
          <div className="bk-activity-steps">
            <Figure className="bk-dim">
              {snap.step} of {snap.steps}
            </Figure>
          </div>
        ) : null}
        {showLog ? <Log lines={snap.log} /> : null}
      </div>
    </Panel>
  );
}
