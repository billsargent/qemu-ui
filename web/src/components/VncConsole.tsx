import { useEffect, useRef, useState } from "react";
import RFB from "@novnc/novnc";
import { api, wsUrl } from "../api/client";
import { Button } from "./ui";

type Status = "connecting" | "connected" | "disconnected" | "error";

// X11 keysyms for the send-key palette.
const KEYS = {
  ctrl: [0xffe3, "ControlLeft"],
  alt: [0xffe9, "AltLeft"],
  del: [0xffff, "Delete"],
  f: (n: number) => [0xffbd + n, `F${n}`] as [number, string],
} as const;

export function VncConsole({ vmId, running }: { vmId: string; running: boolean }) {
  const target = useRef<HTMLDivElement>(null);
  const rfb = useRef<RFB | null>(null);
  const [status, setStatus] = useState<Status>("connecting");
  const [message, setMessage] = useState<string>("");
  const [scale, setScale] = useState(true);
  const [attempt, setAttempt] = useState(0);

  useEffect(() => {
    if (!running || !target.current) return;
    let cancelled = false;
    setStatus("connecting");
    setMessage("");
    (async () => {
      try {
        // A fresh single-use ticket per connection.
        const t = await api.consoleTicket(vmId, "vnc");
        if (cancelled || !target.current) return;
        const r = new RFB(target.current, wsUrl(`/ws/vnc/${vmId}`), {
          wsProtocols: [t.protocol, "binary"],
          credentials: t.vnc_password ? { password: t.vnc_password } : undefined,
        });
        r.scaleViewport = scale;
        r.background = "#09090b";
        r.addEventListener("connect", () => setStatus("connected"));
        r.addEventListener("disconnect", (e) => {
          const clean = (e as CustomEvent<{ clean: boolean }>).detail?.clean;
          setStatus(clean ? "disconnected" : "error");
          if (!clean) setMessage("Connection lost.");
        });
        r.addEventListener("credentialsrequired", () => {
          if (t.vnc_password) r.sendCredentials({ password: t.vnc_password });
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
    };
    // `scale` is applied live below; reconnecting for it is not needed.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [vmId, running, attempt]);

  useEffect(() => {
    if (rfb.current) rfb.current.scaleViewport = scale;
  }, [scale]);

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

  return (
    <div className="flex h-full min-h-0 flex-col gap-2">
      <div className="flex flex-wrap items-center gap-1 text-xs">
        <span
          className={`mr-2 inline-block h-2 w-2 rounded-full ${
            status === "connected" ? "bg-emerald-500" : status === "connecting" ? "bg-amber-400" : "bg-rose-500"
          }`}
          title={status}
        />
        <Button variant="ghost" onClick={() => rfb.current?.sendCtrlAltDel()}>
          Ctrl+Alt+Del
        </Button>
        {[1, 2, 7].map((n) => (
          <Button key={n} variant="ghost" onClick={() => combo([KEYS.ctrl, KEYS.alt, KEYS.f(n)])}>
            Ctrl+Alt+F{n}
          </Button>
        ))}
        <Button variant="ghost" onClick={() => setScale((s) => !s)}>
          {scale ? "1:1" : "Fit"}
        </Button>
        <Button variant="ghost" onClick={fullscreen}>
          Fullscreen
        </Button>
        {status !== "connected" && status !== "connecting" && (
          <Button onClick={() => setAttempt((a) => a + 1)}>Reconnect</Button>
        )}
        {message && <span className="ml-2 text-rose-600">{message}</span>}
      </div>
      <div ref={target} className="min-h-96 flex-1 overflow-hidden rounded bg-zinc-950" />
    </div>
  );
}
