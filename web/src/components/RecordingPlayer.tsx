import { useCallback, useEffect, useRef, useState } from "react";
import RFB from "@novnc/novnc";
import type { Bookmark } from "../api/generated/Bookmark";
import { Button, inputCls } from "./ui";

/** One recorded server message (record 0 is the replay preamble). */
export interface ReplayRecord {
  t: number;
  data: Uint8Array;
}

/** Parse the daemon's recording format (see qemu_ui_core::recording). */
export function parseRecording(buf: ArrayBuffer): { width: number; height: number; records: ReplayRecord[] } {
  const bytes = new Uint8Array(buf);
  const view = new DataView(buf);
  const magic = new TextDecoder().decode(bytes.subarray(0, 8));
  if (magic !== "QUIREC01") throw new Error("not a qemu-ui recording");
  const n = view.getUint32(8);
  const header = JSON.parse(new TextDecoder().decode(bytes.subarray(12, 12 + n))) as { width: number; height: number };
  const records: ReplayRecord[] = [];
  let at = 12 + n;
  while (at + 8 <= bytes.length) {
    const t = view.getUint32(at);
    const len = view.getUint32(at + 4);
    if (at + 8 + len > bytes.length) break; // truncated by a crash
    records.push({ t, data: bytes.subarray(at + 8, at + 8 + len) });
    at += 8 + len;
  }
  return { width: header.width, height: header.height, records };
}

/**
 * A WebSocket stand-in for noVNC: it delivers recorded server messages and
 * ignores what the client sends. Messages pushed before noVNC attaches are
 * queued until it sets `onmessage`.
 */
class ReplayChannel {
  binaryType = "arraybuffer";
  protocol = "";
  readyState: string = "open";
  onopen: ((e: unknown) => void) | null = null;
  onerror: ((e: unknown) => void) | null = null;
  onclose: ((e: unknown) => void) | null = null;
  private handler: ((e: { data: ArrayBuffer }) => void) | null = null;
  private queue: ArrayBuffer[] = [];

  get onmessage() {
    return this.handler;
  }
  set onmessage(h: ((e: { data: ArrayBuffer }) => void) | null) {
    this.handler = h;
    if (h) {
      const q = this.queue;
      this.queue = [];
      for (const d of q) h({ data: d });
    }
  }
  send() {}
  close() {
    this.readyState = "closed";
    this.onclose?.({ code: 1000, reason: "", wasClean: true });
  }
  push(data: Uint8Array) {
    const copy = data.slice().buffer;
    if (this.handler) this.handler({ data: copy });
    else this.queue.push(copy);
  }
}

function fmt(ms: number) {
  const s = Math.floor(ms / 1000);
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;
}

/** Plays a recording in noVNC: play/pause, speed, seek and bookmarks. */
export function RecordingPlayer({ records, bookmarks }: { records: ReplayRecord[]; bookmarks: Bookmark[] }) {
  const screen = useRef<HTMLDivElement>(null);
  const rfb = useRef<RFB | null>(null);
  const channel = useRef<ReplayChannel | null>(null);
  const next = useRef(0);
  const clock = useRef({ pos: 0, wall: 0 });
  const [playing, setPlaying] = useState(false);
  const [speed, setSpeed] = useState(1);
  const [pos, setPos] = useState(0);
  const end = records.length ? records[records.length - 1].t : 0;

  /** Restart the session and fast-forward to `t` (RFB is stateful). */
  const seek = useCallback(
    (t: number) => {
      rfb.current?.disconnect();
      if (!screen.current) return;
      screen.current.innerHTML = "";
      const ch = new ReplayChannel();
      channel.current = ch;
      const r = new RFB(screen.current, ch as unknown as WebSocket, { shared: true });
      r.viewOnly = true;
      r.scaleViewport = true;
      r.background = "#18181b";
      rfb.current = r;
      let i = 0;
      while (i < records.length && records[i].t <= t) ch.push(records[i++].data);
      next.current = i;
      clock.current = { pos: t, wall: performance.now() };
      setPos(t);
    },
    [records],
  );

  useEffect(() => {
    seek(0);
    return () => rfb.current?.disconnect();
  }, [seek]);

  useEffect(() => {
    if (!playing) return;
    clock.current = { pos: clock.current.pos, wall: performance.now() };
    const timer = window.setInterval(() => {
      const now = clock.current.pos + (performance.now() - clock.current.wall) * speed;
      while (next.current < records.length && records[next.current].t <= now) {
        channel.current?.push(records[next.current++].data);
      }
      setPos(Math.min(now, end));
      if (next.current >= records.length) {
        clock.current = { pos: end, wall: performance.now() };
        setPlaying(false);
      }
    }, 40);
    return () => {
      window.clearInterval(timer);
      clock.current = { pos: clock.current.pos + (performance.now() - clock.current.wall) * speed, wall: performance.now() };
    };
  }, [playing, speed, records, end]);

  return (
    <div className="space-y-2">
      <div ref={screen} className="h-[28rem] w-full overflow-hidden rounded bg-zinc-900" />
      <div className="flex flex-wrap items-center gap-2 text-xs">
        <Button
          onClick={() => {
            if (!playing && pos >= end) seek(0);
            setPlaying(!playing);
          }}
        >
          {playing ? "Pause" : "Play"}
        </Button>
        <input
          type="range"
          className="min-w-40 flex-1"
          min={0}
          max={end}
          value={pos}
          onChange={(e) => {
            setPlaying(false);
            seek(Number(e.target.value));
          }}
          aria-label="Position"
        />
        <span className="font-mono text-zinc-500">
          {fmt(pos)} / {fmt(end)}
        </span>
        <select className={inputCls} value={speed} onChange={(e) => setSpeed(Number(e.target.value))} aria-label="Speed">
          {[0.5, 1, 2, 4, 8].map((s) => (
            <option key={s} value={s}>
              {s}×
            </option>
          ))}
        </select>
      </div>
      {bookmarks.length > 0 && (
        <div className="flex flex-wrap gap-1 text-xs">
          {bookmarks.map((b, i) => (
            <Button
              key={i}
              variant="ghost"
              onClick={() => {
                setPlaying(false);
                seek(Number(b.t_ms));
              }}
            >
              {fmt(Number(b.t_ms))} · {b.label}
            </Button>
          ))}
        </div>
      )}
    </div>
  );
}
