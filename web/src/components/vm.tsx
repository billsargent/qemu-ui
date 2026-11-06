import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useEffect, useState } from "react";
import { api, type VmAction } from "../api/client";
import type { VmState } from "../api/generated/VmState";
import type { VmSummary } from "../api/generated/VmSummary";
import { Badge, Button } from "./ui";

const STATE_TONE: Record<VmState, "green" | "amber" | "red" | "zinc" | "blue"> = {
  running: "green",
  starting: "blue",
  stopping: "blue",
  paused: "amber",
  stopped: "zinc",
  crashed: "red",
};

// Mirrors qemu_ui_core::vm::explain_pause.
const PAUSE_REASONS: Record<string, string> = {
  user: "Paused by request.",
  "io-error": "Paused after a disk I/O error (often a full host disk). Free space, then resume.",
  "guest-panicked": "The guest kernel panicked.",
  "internal-error": "QEMU hit an internal error; check the VM log.",
  debug: "Stopped by the debugger.",
  suspended: "The guest suspended itself (ACPI S3).",
  watchdog: "Stopped by the guest watchdog.",
};

export function explainPause(reason: string | null): string {
  return (reason && PAUSE_REASONS[reason]) || "Paused.";
}

export function StateBadge({ vm }: { vm: VmSummary }) {
  const label = vm.state === "paused" && vm.pause_reason && vm.pause_reason !== "user"
    ? `paused · ${vm.pause_reason}`
    : vm.state;
  return (
    <Badge tone={STATE_TONE[vm.state]} title={vm.state === "paused" ? explainPause(vm.pause_reason) : undefined}>
      {label}
    </Badge>
  );
}

export function AccelBadge({ accel }: { accel: string | null }) {
  if (!accel) return null;
  return accel === "tcg" ? (
    <Badge title="No hardware acceleration: every instruction is emulated">emulated (TCG)</Badge>
  ) : (
    <Badge tone="green">native ({accel})</Badge>
  );
}

export function useUptime(startedAt: number | null): string {
  const [now, setNow] = useState(() => Date.now() / 1000);
  useEffect(() => {
    if (!startedAt) return;
    const t = setInterval(() => setNow(Date.now() / 1000), 1000);
    return () => clearInterval(t);
  }, [startedAt]);
  if (!startedAt) return "";
  const s = Math.max(0, Math.floor(now - startedAt));
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  return h ? `${h}h ${m}m` : m ? `${m}m ${s % 60}s` : `${s}s`;
}

/** Actions valid for the VM's current state. Never optimistic: the state
 * shown always comes from the daemon. */
export function VmActions({ vm, compact }: { vm: VmSummary; compact?: boolean }) {
  const qc = useQueryClient();
  const [error, setError] = useState<string | null>(null);
  const m = useMutation({
    mutationFn: (a: VmAction) => api.action(vm.id, a),
    onMutate: () => setError(null),
    onError: (e: Error) => setError(e.message),
    onSettled: () => {
      qc.invalidateQueries({ queryKey: ["vms"] });
      qc.invalidateQueries({ queryKey: ["vm", vm.id] });
    },
  });
  const busy = m.isPending;
  const act = (a: VmAction) => () => m.mutate(a);
  const s = vm.state;
  return (
    <div className="flex flex-wrap items-center gap-1">
      {(s === "stopped" || s === "crashed") && (
        <Button onClick={act("start")} disabled={busy}>
          {busy ? "Starting…" : "Start"}
        </Button>
      )}
      {s === "running" && (
        <>
          <Button variant="ghost" onClick={act("stop")} disabled={busy} title="ACPI power button: the guest shuts down">
            Shut down
          </Button>
          <Button variant="ghost" onClick={act("pause")} disabled={busy}>
            Pause
          </Button>
          {!compact && (
            <Button variant="ghost" onClick={act("reset")} disabled={busy}>
              Reset
            </Button>
          )}
        </>
      )}
      {s === "paused" && (
        <Button onClick={act("resume")} disabled={busy}>
          Resume
        </Button>
      )}
      {(s === "running" || s === "paused" || s === "stopping" || s === "starting") && (
        <Button
          variant="ghost"
          onClick={act("force-stop")}
          disabled={busy}
          title="Stop immediately, like pulling the power cord"
        >
          Force stop
        </Button>
      )}
      {error && <span className="text-xs text-rose-600">{error}</span>}
    </div>
  );
}
