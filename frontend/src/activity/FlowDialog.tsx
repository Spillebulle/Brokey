// The confirm step of every transaction (§7.17 modal): the steps api.plan
// worked out, any sentence worth reading first, whether a password will be
// asked for, and one primary button that says what happens. Mounted once in
// App.tsx; startOps in flow.ts opens it.

import { Badge } from "../components/Badge";
import { Button } from "../components/Button";
import { Dialog } from "../components/Dialog";
import { Notice } from "../components/Notice";
import { Skeleton } from "../components/Skeleton";
import { useShell } from "../shell/store";
import type { Step } from "../types";
import { removalSentence, summarise, useFlow, verbFor } from "./flow";

/** Whether this window is talking to a Windows machine. Linux is the default while `system` is still loading. */
function isWindows(platform: "linux" | "windows" | undefined): boolean {
  return platform === "windows";
}

/**
 * The word and the sentence for a step that runs elevated. There is no root
 * on Windows, only Administrator, and UAC asks the user to allow the step
 * rather than asking for a password, so the two platforms are not the same
 * sentence with one word swapped.
 */
function elevatedCopy(platform: "linux" | "windows" | undefined): { badge: string; title: string } {
  if (isWindows(platform)) {
    return { badge: "needs Administrator", title: "This step runs as Administrator. Windows will ask you to allow it." };
  }
  return { badge: "needs your password", title: "This step runs as root through the helper." };
}

/** A step as one line for the dialog: the program and its arguments. */
function commandLine(step: Step): string {
  return [step.command.program, ...step.command.args].join(" ");
}

/**
 * Whether a step that does not run through the helper still raises a password
 * prompt of its own: an AUR build installs what it built through pkexec, a
 * system-wide Flatpak and fwupd ask polkit themselves.
 */
function asksItself(step: Step): boolean {
  if (step.needs_root) return false;
  const { program, args } = step.command;
  if (program === "paru" || program === "yay" || program === "makepkg") return true;
  if (program === "flatpak") return !args.includes("--user");
  return program === "fwupdmgr";
}

/**
 * How many password prompts the plan raises: one for each run of consecutive
 * root steps (the runner starts the helper once per run) and one for each step
 * that asks for itself. The dialog promises no fewer than this.
 */
export function promptCount(steps: Step[]): number {
  let count = 0;
  let inRootRun = false;
  for (const step of steps) {
    if (step.needs_root) {
      if (!inRootRun) count += 1;
      inRootRun = true;
    } else {
      inRootRun = false;
      if (asksItself(step)) count += 1;
    }
  }
  return count;
}

/**
 * The sentence under the step list: how many times the user will be asked to
 * approve something. On Windows that approval is UAC's "allow this", never a
 * password, since the window has no polkit or AUR builds asking for one there.
 */
function promptSentence(count: number, platform: "linux" | "windows" | undefined): string {
  const word = isWindows(platform) ? "to allow this" : "for your password";
  if (count === 0) return isWindows(platform) ? "Nothing here needs Administrator." : "Nothing here needs your password.";
  if (count === 1) return `You will be asked ${word} once.`;
  return `You may be asked ${word} ${count} times: once for each group of steps marked below.`;
}

function StepRow({ step, elevated }: { step: Step; elevated: { badge: string; title: string } }) {
  return (
    <li className="bk-plan-step">
      <div className="bk-plan-step-head">
        <span className="bk-plan-step-title">{step.title}</span>
        {step.needs_root ? <Badge title={elevated.title}>{elevated.badge}</Badge> : null}
        {asksItself(step) ? <Badge title={`${step.command.program} asks for the password itself for this step.`}>asks for your password</Badge> : null}
      </div>
      <div className="bk-plan-step-cmd" title={commandLine(step)}>
        {commandLine(step)}
      </div>
    </li>
  );
}

/** Rows of the same geometry while api.plan is still answering. */
function StepSkeleton() {
  return (
    <ul className="bk-plan-steps" aria-busy="true" aria-label="Working out the steps">
      {[0, 1].map((i) => (
        <li key={i} className="bk-plan-step">
          <div className="bk-plan-step-head">
            <Skeleton width={i === 0 ? "38%" : "52%"} />
          </div>
          <Skeleton width="72%" className="bk-skel--text" />
        </li>
      ))}
    </ul>
  );
}

export function FlowDialog() {
  const open = useFlow((s) => s.open);
  const title = useFlow((s) => s.title);
  const ops = useFlow((s) => s.ops);
  const preview = useFlow((s) => s.preview);
  const error = useFlow((s) => s.error);
  const starting = useFlow((s) => s.starting);
  const confirm = useFlow((s) => s.confirm);
  const cancel = useFlow((s) => s.cancel);
  const platform = useShell((s) => s.system?.platform);
  if (!open) return null;

  const verb = verbFor(ops);
  const steps = preview?.plan.steps ?? [];
  const prompts = promptCount(steps);
  const elevated = elevatedCopy(platform);
  const removes = removalSentence(ops, steps);
  const disabledReason = error ?? (preview ? null : "The steps are still being worked out.");

  const primary =
    disabledReason !== null ? (
      <Button kind={verb === "Remove" ? "danger" : "primary"} disabled disabledReason={disabledReason}>
        {verb}
      </Button>
    ) : (
      <Button kind={verb === "Remove" ? "danger" : "primary"} onClick={() => void confirm()}>
        {verb}
      </Button>
    );

  return (
    <Dialog
      open={open}
      title={title}
      subtitle={summarise(ops)}
      size={steps.length > 4 ? "standard" : "small"}
      onClose={cancel}
      busy={starting ? "The transaction is starting. Wait a moment." : undefined}
      actions={
        <>
          <Button kind="ghost" onClick={cancel}>
            Cancel
          </Button>
          {primary}
        </>
      }
    >
      <div className="bk-stack bk-plan">
        {removes ? <p>{removes}</p> : null}
        {error ? <Notice>{error}</Notice> : null}
        {preview?.notices.map((sentence, i) => (
          <Notice key={i}>{sentence}</Notice>
        ))}
        {preview ? (
          <ul className="bk-plan-steps" aria-label="Steps">
            {steps.map((step, i) => (
              <StepRow key={i} step={step} elevated={elevated} />
            ))}
          </ul>
        ) : error ? null : (
          <StepSkeleton />
        )}
        {preview ? (
          <p className="bk-dim bk-small">{promptSentence(prompts, platform)}</p>
        ) : null}
      </div>
    </Dialog>
  );
}
