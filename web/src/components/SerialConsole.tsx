import { useEffect, useRef, useState } from "react";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import "@xterm/xterm/css/xterm.css";
import { api, wsUrl } from "../api/client";
import { Button } from "./ui";

/** Serial console: scrollback from the persistent log, then live I/O. */
export function SerialConsole({ vmId, running }: { vmId: string; running: boolean }) {
  const host = useRef<HTMLDivElement>(null);
  const [status, setStatus] = useState<"connecting" | "connected" | "closed">("connecting");
  const [attempt, setAttempt] = useState(0);

  useEffect(() => {
    if (!host.current) return;
    const term = new Terminal({
      convertEol: true,
      fontFamily: "ui-monospace, 'Cascadia Code', Menlo, monospace",
      fontSize: 13,
      scrollback: 10000,
      theme: { background: "#09090b" },
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(host.current);
    fit.fit();
    const onResize = () => fit.fit();
    window.addEventListener("resize", onResize);
    const ro = new ResizeObserver(() => fit.fit());
    ro.observe(host.current);

    let ws: WebSocket | null = null;
    let cancelled = false;
    (async () => {
      const log = await api.serialLog(vmId);
      if (cancelled) return;
      if (log) term.write(log);
      if (!running) {
        setStatus("closed");
        term.write("\r\n\x1b[90m[VM is not running — showing the saved serial log]\x1b[0m\r\n");
        return;
      }
      try {
        const t = await api.consoleTicket(vmId, "serial");
        if (cancelled) return;
        ws = new WebSocket(wsUrl(`/ws/serial/${vmId}`), [t.protocol]);
        ws.binaryType = "arraybuffer";
        ws.onopen = () => setStatus("connected");
        ws.onclose = () => {
          setStatus("closed");
          if (!cancelled) term.write("\r\n\x1b[90m[serial console disconnected]\x1b[0m\r\n");
        };
        ws.onmessage = (m) => term.write(new Uint8Array(m.data as ArrayBuffer));
        term.onData((d) => ws?.readyState === WebSocket.OPEN && ws.send(new TextEncoder().encode(d)));
      } catch (e) {
        setStatus("closed");
        term.write(`\r\n\x1b[31m[${(e as Error).message}]\x1b[0m\r\n`);
      }
    })();

    return () => {
      cancelled = true;
      ws?.close();
      ro.disconnect();
      window.removeEventListener("resize", onResize);
      term.dispose();
    };
  }, [vmId, running, attempt]);

  return (
    <div className="flex h-full min-h-0 flex-col gap-2">
      <div className="flex items-center gap-2 text-xs">
        <span
          className={`inline-block h-2 w-2 rounded-full ${
            status === "connected" ? "bg-emerald-500" : status === "connecting" ? "bg-amber-400" : "bg-zinc-400"
          }`}
        />
        <span className="text-zinc-500">Serial (ttyS0)</span>
        {running && status === "closed" && <Button onClick={() => setAttempt((a) => a + 1)}>Reconnect</Button>}
      </div>
      <div ref={host} className="min-h-64 flex-1 overflow-hidden rounded bg-zinc-950 p-1" />
    </div>
  );
}
