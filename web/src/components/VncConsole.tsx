import { useEffect, useRef, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import RFB from "@novnc/novnc";
import { api, wsUrl } from "../api/client";
import { onVmEvent } from "../api/events";
import type { ConsolePresence } from "../api/generated/ConsolePresence";
import type { DisplayInfo } from "../api/generated/DisplayInfo";
import { Badge, Button } from "./ui";

type Status = "connecting" | "connected" | "disconnected" | "error";

// X11 keysyms for the send-key palette.
const KEYS = {
  ctrl: [0xffe3, "ControlLeft"],
  alt: [0xffe9, "AltLeft"],
  del: [0xffff, "Delete"],
  f: (n: number) => [0xffbd + n, `F${n}`] as [number, string],
} as const;

/** Keep the display query's presence current from live events. */
function usePresenceUpdates(vmId: string) {
  const qc = useQueryClient();
  useEffect(
    () =>
      onVmEvent((ev) => {
        if (ev.vm_id !== vmId || ev.kind !== "console-presence") return;
        qc.setQueryData<DisplayInfo>(["display", vmId], (old) =>
          old ? { ...old, presence: ev.data as ConsolePresence } : old,
        );
      }),
    [qc, vmId],
  );
}

export function VncConsole({ vmId, running }: { vmId: string; running: boolean }) {
  const target = useRef<HTMLDivElement>(null);
  const rfb = useRef<RFB | null>(null);
  const [status, setStatus] = useState<Status>("connecting");
  const [message, setMessage] = useState<string>("");
  const [scale, setScale] = useState(true);
  const [attempt, setAttempt] = useState(0);
  const [watchOnly, setWatchOnly] = useState(false);
  const [viewerId, setViewerId] = useState<number | null>(null);

  const display = useQuery({
    queryKey: ["display", vmId],
    queryFn: () => api.display(vmId),
    enabled: running,
  });
  usePresenceUpdates(vmId);
  const engine = display.data?.engines.find((e) => e.available) ?? null;
  const caps = engine?.caps;
  const presence = display.data?.presence;
  const me = presence?.viewers.find((v) => v.id === viewerId);
  const controlling = !!me?.controlling;
  const engineId = engine?.id;

  const takeControl = useMutation({
    mutationFn: () => api.takeControl(vmId, viewerId!),
  });

  useEffect(() => {
    if (!running || !target.current || !engineId) return;
    let cancelled = false;
    setStatus("connecting");
    setMessage("");
    (async () => {
      try {
        // A fresh single-use ticket per connection.
        const a = await api.attachDisplay(vmId, engineId, watchOnly);
        if (cancelled || !target.current) return;
        setViewerId(a.viewer_id);
        const r = new RFB(target.current, wsUrl(a.path), {
          wsProtocols: [a.protocol, "binary"],
          credentials: a.vnc_password ? { password: a.vnc_password } : undefined,
        });
        r.scaleViewport = scale;
        r.background = "#09090b";
        // Input is enabled once presence says this viewer holds control.
        r.viewOnly = true;
        r.addEventListener("connect", () => setStatus("connected"));
        r.addEventListener("disconnect", (e) => {
          const clean = (e as CustomEvent<{ clean: boolean }>).detail?.clean;
          setStatus(clean ? "disconnected" : "error");
          if (!clean) setMessage("Connection lost (or closed for being idle).");
        });
        r.addEventListener("credentialsrequired", () => {
          if (a.vnc_password) r.sendCredentials({ password: a.vnc_password });
        });
        r.addEventListener("securityfailure", (e) => {
          setStatus("error");
          setMessage(`Authentication failed: ${(e as CustomEvent<{ reason?: string }>).detail?.reason ?? ""}`);
        });
        rfb.current = r;
      } catch (e) {
        if (!cancelled) {
          setStatus("error");
          setMessage((e as Error).message);
        }
      }
    })();
    return () => {
      cancelled = true;
      rfb.current?.disconnect();
      rfb.current = null;
      setViewerId(null);
    };
    // `scale` is applied live below; reconnecting for it is not needed.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [vmId, running, attempt, engineId, watchOnly]);

  useEffect(() => {
    if (rfb.current) rfb.current.scaleViewport = scale;
  }, [scale]);

  useEffect(() => {
    if (rfb.current) rfb.current.viewOnly = !controlling;
  }, [controlling, status]);

  const combo = (keys: (readonly [number, string])[]) => {
    const r = rfb.current;
    if (!r) return;
    keys.forEach(([sym, code]) => r.sendKey(sym, code, true));
    [...keys].reverse().forEach(([sym, code]) => r.sendKey(sym, code, false));
    r.focus();
  };

  const fullscreen = () => target.current?.requestFullscreen?.();

  if (!running) {
    return (
      <div className="flex h-96 items-center justify-center rounded border border-dashed border-zinc-300 text-sm text-zinc-500 dark:border-zinc-700">
        Start the VM to see its display.
      </div>
    );
  }
  if (display.data && !engine) {
    const why = display.data.engines.map((e) => e.reason).find(Boolean);
    return (
      <div className="flex h-96 items-center justify-center rounded border border-dashed border-zinc-300 text-sm text-zinc-500 dark:border-zinc-700">
        {why ?? "No display engine is available for this VM."}
      </div>
    );
  }

  const keysEnabled = controlling && status === "connected";
  return (
    <div className="flex h-full min-h-0 flex-col gap-2">
      <div className="flex flex-wrap items-center gap-1 text-xs">
        <span
          className={`mr-2 inline-block h-2 w-2 rounded-full ${
            status === "connected" ? "bg-emerald-500" : status === "connecting" ? "bg-amber-400" : "bg-rose-500"
          }`}
          title={status}
        />
        {caps?.keys && (
          <>
            <Button variant="ghost" disabled={!keysEnabled} onClick={() => rfb.current?.sendCtrlAltDel()}>
              Ctrl+Alt+Del
            </Button>
            {[1, 2, 7].map((n) => (
              <Button key={n} variant="ghost" disabled={!keysEnabled} onClick={() => combo([KEYS.ctrl, KEYS.alt, KEYS.f(n)])}>
                Ctrl+Alt+F{n}
              </Button>
            ))}
          </>
        )}
        {caps?.scale && (
          <Button variant="ghost" onClick={() => setScale((s) => !s)}>
            {scale ? "1:1" : "Fit"}
          </Button>
        )}
        {caps?.fullscreen && (
          <Button variant="ghost" onClick={fullscreen}>
            Fullscreen
          </Button>
        )}
        {caps?.screenshot && (
          <a className="rounded px-2.5 py-1 font-medium hover:bg-zinc-100 dark:hover:bg-zinc-800" href={api.screenshotUrl(vmId)} target="_blank" rel="noreferrer">
            Screenshot
          </a>
        )}
        {caps?.view_only && (
          <label className="ml-1 flex items-center gap-1" title="Join without the ability to type or click">
            <input type="checkbox" checked={watchOnly} onChange={(e) => setWatchOnly(e.target.checked)} />
            Watch only
          </label>
        )}
        {status !== "connected" && status !== "connecting" && (
          <Button onClick={() => setAttempt((a) => a + 1)}>Reconnect</Button>
        )}
        {message && <span className="ml-2 text-rose-600">{message}</span>}
      </div>
      {caps?.take_control && presence && presence.viewers.length > 0 && (
        <div className="flex flex-wrap items-center gap-1 text-xs" data-testid="console-presence">
          <span className="text-zinc-500">Viewers:</span>
          {presence.viewers.map((v) => (
            <Badge key={v.id} tone={v.controlling ? "green" : "zinc"} title={v.controlling ? "Has control" : v.read_only ? "Watch only" : "Watching"}>
              {v.label}
              {v.id === viewerId ? " (you)" : ""}
              {v.controlling ? " · control" : v.read_only ? " · watch-only" : ""}
            </Badge>
          ))}
          {me && !controlling && !me.read_only && (
            <Button onClick={() => takeControl.mutate()} disabled={takeControl.isPending}>
              Take control
            </Button>
          )}
          {me && !controlling && <span className="text-zinc-500">Your keyboard and mouse are not sent to the VM.</span>}
          {takeControl.error && <span className="text-rose-600">{takeControl.error.message}</span>}
        </div>
      )}
      <div ref={target} className="min-h-96 flex-1 overflow-hidden rounded bg-zinc-950" />
    </div>
  );
}
