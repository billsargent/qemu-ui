import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "../api/client";
import type { RecordingInfo } from "../api/generated/RecordingInfo";
import type { VmDetail } from "../api/generated/VmDetail";
import { formatBytes } from "../pages/StoragePage";
import { RecordingPlayer, parseRecording, type ReplayRecord } from "./RecordingPlayer";
import { Badge, Button, Card, inputCls } from "./ui";

const TONE = { recording: "red", finished: "green", capped: "amber", failed: "red", interrupted: "amber" } as const;

function duration(ms: number) {
  const s = Math.round(ms / 1000);
  return s < 60 ? `${s}s` : `${Math.floor(s / 60)}m ${s % 60}s`;
}

/** Record the VM's display, and list, play, export or delete recordings. */
export function RecordingsTab({ d }: { d: VmDetail }) {
  const id = d.summary.id;
  const qc = useQueryClient();
  const list = useQuery({
    queryKey: ["recordings", id],
    queryFn: () => api.recordings(id),
    refetchInterval: (q) => (q.state.data?.recordings.some((r) => r.status === "recording") ? 1000 : false),
  });
  const refresh = () => qc.invalidateQueries({ queryKey: ["recordings", id] });
  const [label, setLabel] = useState("");
  const [open, setOpen] = useState<{ rec: RecordingInfo; records: ReplayRecord[] } | null>(null);
  const [loading, setLoading] = useState<string | null>(null);

  const start = useMutation({ mutationFn: () => api.startRecording(id), onSuccess: refresh });
  const stop = useMutation({ mutationFn: (rid: string) => api.stopRecording(id, rid), onSuccess: refresh });
  const mark = useMutation({
    mutationFn: (rid: string) => api.bookmarkRecording(id, rid, label || "bookmark"),
    onSuccess: () => {
      setLabel("");
      refresh();
    },
  });
  const remove = useMutation({
    mutationFn: (rid: string) => api.deleteRecording(id, rid),
    onSuccess: (_, rid) => {
      if (open?.rec.id === rid) setOpen(null);
      refresh();
    },
  });
  const exportWebm = useMutation({ mutationFn: (rid: string) => api.exportRecording(id, rid), onSuccess: refresh });
  const [playError, setPlayError] = useState<string | null>(null);

  const play = async (rec: RecordingInfo) => {
    setLoading(rec.id);
    setPlayError(null);
    try {
      const parsed = parseRecording(await api.recordingStream(id, rec.id));
      setOpen({ rec, records: parsed.records });
    } catch (e) {
      setPlayError((e as Error).message);
    } finally {
      setLoading(null);
    }
  };

  const running = d.summary.state === "running" || d.summary.state === "paused";
  const recordable = running && d.spec.display.vnc;
  const active = list.data?.recordings.find((r) => r.status === "recording");
  const err = start.error ?? stop.error ?? mark.error ?? remove.error ?? exportWebm.error;

  return (
    <div className="space-y-4">
      <Card title="Record the display">
        <div className="flex flex-wrap items-center gap-2 text-sm">
          {!active ? (
            <>
              <Button disabled={!recordable || start.isPending} onClick={() => start.mutate()}>
                Start recording
              </Button>
              <span className="text-xs text-zinc-500">
                {recordable
                  ? "Records what the VM's screen shows, whether or not a console is open. Clipboard contents are not recorded."
                  : "Start the VM (with a display) to record it."}
              </span>
            </>
          ) : (
            <>
              <Badge tone="red">● recording</Badge>
              <span className="font-mono text-xs text-zinc-500">
                {duration(Number(active.duration_ms))} · {formatBytes(Number(active.size_bytes))}
              </span>
              <Button disabled={stop.isPending} onClick={() => stop.mutate(active.id)}>
                Stop
              </Button>
              <input
                className={`${inputCls} w-48`}
                placeholder="bookmark label"
                value={label}
                onChange={(e) => setLabel(e.target.value)}
              />
              <Button variant="ghost" disabled={mark.isPending} onClick={() => mark.mutate(active.id)}>
                Add bookmark
              </Button>
            </>
          )}
        </div>
        {err && <p className="mt-2 text-xs text-rose-600">{err.message}</p>}
      </Card>

      {open && (
        <Card
          title={`Playing ${new Date(Number(open.rec.started_at) * 1000).toLocaleString()}`}
          actions={
            <Button variant="ghost" onClick={() => setOpen(null)}>
              Close
            </Button>
          }
        >
          <RecordingPlayer key={open.rec.id} records={open.records} bookmarks={open.rec.bookmarks} />
        </Card>
      )}

      <Card title="Recordings">
        {playError && <p className="mb-2 text-xs text-rose-600">{playError}</p>}
        {list.data && list.data.recordings.length === 0 && <p className="text-sm text-zinc-500">No recordings yet.</p>}
        <ul className="space-y-1 text-sm">
          {list.data?.recordings.map((r) => (
            <li key={r.id} className="flex flex-wrap items-center gap-2 rounded border border-zinc-200 px-2 py-1.5 dark:border-zinc-800">
              <span>{new Date(Number(r.started_at) * 1000).toLocaleString()}</span>
              <Badge tone={TONE[r.status]}>{r.status}</Badge>
              <span className="text-xs text-zinc-500">
                {duration(Number(r.duration_ms))} · {formatBytes(Number(r.size_bytes))}
                {r.bookmarks.length > 0 && ` · ${r.bookmarks.length} bookmark${r.bookmarks.length > 1 ? "s" : ""}`}
              </span>
              {r.error && <span className="text-xs text-rose-600">{r.error}</span>}
              <span className="ml-auto flex gap-1">
                {r.status !== "recording" && (
                  <>
                    <Button variant="ghost" disabled={loading === r.id} onClick={() => play(r)}>
                      {loading === r.id ? "Loading…" : "Play"}
                    </Button>
                    {r.webm ? (
                      <a className="rounded px-2.5 py-1 text-xs font-medium hover:bg-zinc-100 dark:hover:bg-zinc-800" href={api.recordingWebmUrl(id, r.id)}>
                        Download WebM
                      </a>
                    ) : (
                      list.data?.export_available && (
                        <Button
                          variant="ghost"
                          disabled={exportWebm.isPending}
                          title="Encode to WebM with ffmpeg; progress shows under Jobs"
                          onClick={() => exportWebm.mutate(r.id)}
                        >
                          Export WebM
                        </Button>
                      )
                    )}
                    <Button variant="ghost" disabled={remove.isPending} onClick={() => remove.mutate(r.id)}>
                      Delete
                    </Button>
                  </>
                )}
              </span>
            </li>
          ))}
        </ul>
        {list.data && !list.data.export_available && (
          <p className="mt-2 text-xs text-zinc-500">Install ffmpeg on the host to export recordings as WebM video.</p>
        )}
      </Card>
    </div>
  );
}
