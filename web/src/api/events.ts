// Live updates: one EventSource for the whole app. The browser resends
// Last-Event-ID on reconnect, and the daemon replays what was missed.
import { useEffect, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import type { VmEvent } from "./generated/VmEvent";

type Listener = (ev: VmEvent) => void;
const listeners = new Set<Listener>();
let source: EventSource | null = null;
let connected = false;
const statusListeners = new Set<(c: boolean) => void>();

function ensureSource() {
  if (source) return;
  source = new EventSource("/api/v1/events");
  source.onopen = () => {
    connected = true;
    statusListeners.forEach((l) => l(true));
  };
  source.onerror = () => {
    connected = false;
    statusListeners.forEach((l) => l(false));
  };
  source.onmessage = (m) => {
    try {
      const ev = JSON.parse(m.data) as VmEvent;
      listeners.forEach((l) => l(ev));
    } catch {
      /* ignore malformed */
    }
  };
}

export function onVmEvent(l: Listener): () => void {
  ensureSource();
  listeners.add(l);
  return () => listeners.delete(l);
}

/** Keep react-query caches fresh from the event stream. Mount once. */
export function useLiveInvalidation() {
  const qc = useQueryClient();
  useEffect(
    () =>
      onVmEvent((ev) => {
        if (ev.kind === "job") {
          qc.invalidateQueries({ queryKey: ["jobs"] });
          const status = (ev.data as { status?: string }).status;
          if (status === "succeeded") {
            qc.invalidateQueries({ queryKey: ["pools"] });
            qc.invalidateQueries({ queryKey: ["pool"] });
            qc.invalidateQueries({ queryKey: ["images"] });
            qc.invalidateQueries({ queryKey: ["image"] });
          }
          return;
        }
        if (["state", "created", "updated", "deleted", "started", "exited", "reattached"].includes(ev.kind)) {
          qc.invalidateQueries({ queryKey: ["vms"] });
          qc.invalidateQueries({ queryKey: ["vm", ev.vm_id] });
        }
        qc.invalidateQueries({ queryKey: ["vm-events", ev.vm_id] });
      }),
    [qc],
  );
}

export function useEventStreamConnected(): boolean {
  const [c, setC] = useState(connected);
  useEffect(() => {
    ensureSource();
    statusListeners.add(setC);
    return () => {
      statusListeners.delete(setC);
    };
  }, []);
  return c;
}
