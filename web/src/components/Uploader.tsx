// Resumable uploads into a pool (M2-07).
//
// Files are sent in chunks with PATCH + Upload-Offset. Network errors retry
// with backoff; a 409 resynchronises to the server's offset. Uploads live in
// a module-level store, so they keep running while you move between pages.
// After a reload, picking the same file (same name, size and modification
// time) resumes the server-side session where it stopped.
import { useRef, useState, useSyncExternalStore } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { ApiError, api } from "../api/client";
import { ProgressBar } from "../pages/JobsPage";
import { Button, Card } from "./ui";

type Status = "starting" | "uploading" | "paused" | "retrying" | "done" | "error";

interface Task {
  key: string;
  file: File;
  pool: string;
  id?: string;
  chunk: number;
  offset: number;
  status: Status;
  message?: string;
  /** Bytes per second, smoothed. */
  speed: number;
  paused: boolean;
  ctrl: AbortController;
}

// ------------------------------------------------------------------ store

const tasks = new Map<string, Task>();
const listeners = new Set<() => void>();
let version = 0;

function changed() {
  version++;
  listeners.forEach((l) => l());
  const active = [...tasks.values()].some((t) => t.status === "uploading" || t.status === "retrying" || t.status === "starting");
  window.onbeforeunload = active ? () => "Uploads are in progress." : null;
}

function useTasks(): Task[] {
  useSyncExternalStore(
    (l) => {
      listeners.add(l);
      return () => listeners.delete(l);
    },
    () => version,
  );
  return [...tasks.values()];
}

export function clientKey(f: File): string {
  return `${f.name}:${f.size}:${f.lastModified}`;
}

function sleep(ms: number, signal: AbortSignal): Promise<void> {
  return new Promise((resolve) => {
    const t = setTimeout(resolve, ms);
    signal.addEventListener("abort", () => {
      clearTimeout(t);
      resolve();
    });
  });
}

/** A permanent failure (not worth retrying). */
function permanent(e: unknown): boolean {
  return e instanceof ApiError && e.status >= 400 && e.status < 500 && e.status !== 408 && e.status !== 429;
}

async function run(t: Task, onDone: () => void) {
  t.paused = false;
  t.ctrl = new AbortController();
  t.status = "starting";
  t.message = undefined;
  changed();
  let delay = 1000;
  while (!t.paused) {
    try {
      if (!t.id) {
        const s = await api.createUpload(t.pool, t.file.name, t.file.size, clientKey(t.file));
        t.id = s.id;
        t.offset = s.offset;
        t.chunk = s.chunk_size;
      }
      t.status = "uploading";
      t.message = undefined;
      changed();
      const end = Math.min(t.offset + t.chunk, t.file.size);
      const began = performance.now();
      const r = await api.uploadChunk(t.id, t.offset, t.file.slice(t.offset, end), t.ctrl.signal);
      if ("conflict" in r) {
        t.offset = r.conflict;
        continue;
      }
      const secs = Math.max(0.001, (performance.now() - began) / 1000);
      const rate = (r.offset - t.offset) / secs;
      t.speed = t.speed ? t.speed * 0.7 + rate * 0.3 : rate;
      t.offset = r.offset;
      delay = 1000;
      if (r.complete) {
        t.status = "done";
        t.message = r.format ? `Uploaded as ${r.format}` : "Uploaded";
        changed();
        onDone();
        return;
      }
      changed();
    } catch (e) {
      if (t.paused) break;
      if (permanent(e)) {
        t.status = "error";
        t.message = (e as Error).message;
        changed();
        return;
      }
      t.status = "retrying";
      t.message = `Connection problem; retrying in ${Math.round(delay / 1000)} s`;
      changed();
      await sleep(delay, t.ctrl.signal);
      delay = Math.min(delay * 2, 30_000);
    }
  }
  t.status = "paused";
  t.message = undefined;
  changed();
}

// --------------------------------------------------------------- component

function fmt(n: number): string {
  const u = ["B", "KiB", "MiB", "GiB", "TiB"];
  let i = 0;
  while (n >= 1024 && i < u.length - 1) {
    n /= 1024;
    i++;
  }
  return `${n.toFixed(n < 10 && i > 0 ? 1 : 0)} ${u[i]}`;
}

function eta(t: Task): string {
  if (!t.speed || t.status !== "uploading") return "";
  const s = Math.round((t.file.size - t.offset) / t.speed);
  return s >= 3600 ? `${Math.floor(s / 3600)} h ${Math.round((s % 3600) / 60)} min` : s >= 60 ? `${Math.floor(s / 60)} min ${s % 60} s` : `${s} s`;
}

export function Uploader({ pool }: { pool: string }) {
  const qc = useQueryClient();
  const list = useTasks().filter((t) => t.pool === pool);
  const sessions = useQuery({ queryKey: ["uploads"], queryFn: api.uploads, refetchInterval: 15_000 });
  const input = useRef<HTMLInputElement>(null);
  const [dragging, setDragging] = useState(false);

  const refresh = () => {
    qc.invalidateQueries({ queryKey: ["pool"] });
    qc.invalidateQueries({ queryKey: ["pools"] });
    qc.invalidateQueries({ queryKey: ["images"] });
    qc.invalidateQueries({ queryKey: ["uploads"] });
  };

  const add = (files: FileList | null) => {
    for (const file of Array.from(files ?? [])) {
      const key = `${pool}/${clientKey(file)}`;
      const existing = tasks.get(key);
      if (existing && existing.status !== "error") continue;
      const t: Task = {
        key,
        file,
        pool,
        chunk: 16 * 1024 * 1024,
        offset: 0,
        status: "starting",
        speed: 0,
        paused: false,
        ctrl: new AbortController(),
      };
      tasks.set(key, t);
      void run(t, refresh);
    }
    changed();
  };

  const pause = (t: Task) => {
    t.paused = true;
    t.ctrl.abort();
    changed();
  };

  const cancel = async (t: Task) => {
    pause(t);
    if (t.id) await api.deleteUpload(t.id).catch(() => undefined);
    tasks.delete(t.key);
    changed();
    refresh();
  };

  // Server-side sessions with no task in this tab: interrupted by a reload,
  // another tab, or a daemon restart.
  const local = new Set(list.map((t) => t.id).filter(Boolean));
  const orphans = (sessions.data ?? []).filter((s) => s.pool === pool && !local.has(s.id));

  return (
    <Card title={`Upload to ${pool}`}>
      <div
        onDragOver={(e) => {
          e.preventDefault();
          setDragging(true);
        }}
        onDragLeave={() => setDragging(false)}
        onDrop={(e) => {
          e.preventDefault();
          setDragging(false);
          add(e.dataTransfer.files);
        }}
        onClick={() => input.current?.click()}
        className={`cursor-pointer rounded border-2 border-dashed p-4 text-center text-sm ${
          dragging ? "border-sky-500 bg-sky-50 dark:bg-sky-950" : "border-zinc-300 text-zinc-500 dark:border-zinc-700"
        }`}
      >
        Drop ISOs or disk images here, or click to choose files.
        <div className="mt-1 text-xs">Uploads resume after network errors, reloads and daemon restarts.</div>
        <input
          ref={input}
          type="file"
          multiple
          className="hidden"
          onChange={(e) => {
            add(e.target.files);
            e.target.value = "";
          }}
        />
      </div>

      {list.length > 0 && (
        <ul className="mt-3 space-y-3">
          {list.map((t) => (
            <li key={t.key} className="text-sm">
              <div className="flex items-center gap-2">
                <span className="truncate font-mono text-xs">{t.file.name}</span>
                <span className="ml-auto text-xs whitespace-nowrap text-zinc-500 tabular-nums">
                  {fmt(t.offset)} / {fmt(t.file.size)}
                  {t.status === "uploading" && t.speed > 0 && ` · ${fmt(t.speed)}/s · ${eta(t)} left`}
                </span>
              </div>
              <div className="mt-1 flex items-center gap-2">
                <ProgressBar value={t.file.size ? t.offset / t.file.size : 0} />
                {(t.status === "uploading" || t.status === "retrying" || t.status === "starting") && (
                  <Button variant="ghost" onClick={() => pause(t)}>
                    Pause
                  </Button>
                )}
                {(t.status === "paused" || t.status === "error") && (
                  <Button variant="ghost" onClick={() => void run(t, refresh)}>
                    Resume
                  </Button>
                )}
                {t.status !== "done" ? (
                  <Button variant="ghost" onClick={() => void cancel(t)}>
                    Cancel
                  </Button>
                ) : (
                  <Button
                    variant="ghost"
                    onClick={() => {
                      tasks.delete(t.key);
                      changed();
                    }}
                  >
                    Clear
                  </Button>
                )}
              </div>
              {t.message && (
                <p className={`mt-0.5 text-xs ${t.status === "error" ? "text-rose-600" : "text-zinc-500"}`}>{t.message}</p>
              )}
              {t.status === "paused" && <p className="mt-0.5 text-xs text-zinc-500">Paused</p>}
            </li>
          ))}
        </ul>
      )}

      {orphans.length > 0 && (
        <div className="mt-3 border-t border-zinc-200 pt-2 dark:border-zinc-800">
          <p className="mb-1 text-xs font-medium text-zinc-600 dark:text-zinc-400">Unfinished uploads</p>
          <ul className="space-y-1">
            {orphans.map((s) => (
              <li key={s.id} className="flex items-center gap-2 text-xs">
                <span className="truncate font-mono">{s.name}</span>
                <span className="text-zinc-500 tabular-nums">
                  {Math.floor((s.offset / Math.max(1, s.size)) * 100)}% of {fmt(s.size)}
                </span>
                <span className="text-zinc-500">— choose the same file again to resume</span>
                <Button
                  variant="ghost"
                  onClick={async () => {
                    await api.deleteUpload(s.id).catch(() => undefined);
                    refresh();
                  }}
                >
                  Discard
                </Button>
              </li>
            ))}
          </ul>
        </div>
      )}
    </Card>
  );
}
