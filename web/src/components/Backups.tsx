import { useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { ApiError, api } from "../api/client";
import type { BackupInfo } from "../api/generated/BackupInfo";
import { formatBytes } from "../pages/StoragePage";
import { Badge, Button, Card, Field, inputCls } from "./ui";

function message(err: unknown): string {
  const ex = err as ApiError;
  return ex.diagnostics?.length ? ex.diagnostics.map((x) => x.message).join(" ") : ex.message;
}

/** Export a VM to a backup bundle (X-03). Works on running VMs too. */
export function BackupCard({ id, active }: { id: string; active: boolean }) {
  const qc = useQueryClient();
  const [snapshots, setSnapshots] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [started, setStarted] = useState<string | null>(null);
  return (
    <Card title="Backup">
      <div className="flex flex-wrap items-center gap-3">
        <Button
          disabled={busy}
          onClick={async () => {
            setBusy(true);
            setError(null);
            try {
              const r = await api.exportVm(id, snapshots);
              setStarted(r.job.title);
              qc.invalidateQueries({ queryKey: ["jobs"] });
            } catch (e) {
              setError(message(e));
            } finally {
              setBusy(false);
            }
          }}
        >
          {busy ? "Starting…" : "Export bundle"}
        </Button>
        <label className="flex items-center gap-1.5 text-xs text-zinc-600 dark:text-zinc-400" title="Copy qcow2 images as they are instead of flattening them">
          <input type="checkbox" checked={snapshots} disabled={active} onChange={(e) => setSnapshots(e.target.checked)} />
          Keep internal snapshots
        </label>
      </div>
      <p className="mt-2 text-xs text-zinc-500">
        Writes the configuration, every writable disk, UEFI variables and TPM state to one file under Storage → Backups.
        {active
          ? " The VM keeps running: QEMU copies all disks from one point in time, quiesced when the guest agent answers."
          : " Read-only media such as ISOs are not included."}
      </p>
      {started && <p className="mt-1 text-xs text-zinc-500">{started} — see Jobs.</p>}
      {error && <p className="mt-1 text-xs text-rose-600">{error}</p>}
    </Card>
  );
}

/** Bundles in the backups directory: download, upload, import, delete. */
export function BackupsPanel() {
  const qc = useQueryClient();
  const backups = useQuery({ queryKey: ["backups"], queryFn: api.backups, refetchInterval: 10_000 });
  const [error, setError] = useState<string | null>(null);
  const [uploading, setUploading] = useState(false);
  return (
    <Card
      title="Backups"
      actions={
        <label className="cursor-pointer text-xs text-sky-700 hover:underline dark:text-sky-400">
          {uploading ? "Uploading…" : "Upload bundle"}
          <input
            type="file"
            accept=".tar"
            className="hidden"
            disabled={uploading}
            onChange={async (e) => {
              const f = e.target.files?.[0];
              e.target.value = "";
              if (!f) return;
              setUploading(true);
              setError(null);
              try {
                await api.uploadBackup(f.name, f);
                qc.invalidateQueries({ queryKey: ["backups"] });
              } catch (err) {
                setError(message(err));
              } finally {
                setUploading(false);
              }
            }}
          />
        </label>
      }
    >
      {backups.data?.length === 0 && <p className="text-sm text-zinc-500">No bundles yet. Export one from a VM's Settings tab.</p>}
      <ul className="divide-y divide-zinc-200 dark:divide-zinc-800">
        {backups.data?.map((b) => <BackupRow key={b.name} b={b} onError={setError} />)}
      </ul>
      {error && <p className="mt-2 text-xs text-rose-600">{error}</p>}
    </Card>
  );
}

function BackupRow({ b, onError }: { b: BackupInfo; onError: (e: string | null) => void }) {
  const qc = useQueryClient();
  const pools = useQuery({ queryKey: ["pools"], queryFn: api.pools });
  const [open, setOpen] = useState(false);
  const [name, setName] = useState(b.manifest ? `${b.manifest.source_name}-restored` : "");
  const [pool, setPool] = useState("default");
  const [busy, setBusy] = useState(false);
  const [started, setStarted] = useState(false);
  const m = b.manifest;
  return (
    <li className="py-2 text-sm">
      <div className="flex flex-wrap items-center gap-2">
        <span className="font-mono text-xs">{b.name}</span>
        <span className="text-xs text-zinc-500">{formatBytes(b.size)}</span>
        {m?.live && <Badge tone={m.quiesced ? "green" : "amber"} title={m.quiesced ? "Guest file systems were frozen" : "Crash-consistent copy"}>{m.quiesced ? "live, quiesced" : "live"}</Badge>}
        {m?.snapshots_included && <Badge>snapshots</Badge>}
        {!m && <Badge tone="red" title={b.error ?? undefined}>unreadable</Badge>}
        <span className="ml-auto flex gap-3 text-xs">
          {m && (
            <button className="text-sky-700 hover:underline dark:text-sky-400" onClick={() => setOpen(!open)}>
              Import…
            </button>
          )}
          <a className="text-sky-700 hover:underline dark:text-sky-400" href={`/api/v1/backups/${encodeURIComponent(b.name)}`} download>
            Download
          </a>
          <button
            className="text-rose-700 hover:underline dark:text-rose-400"
            onClick={async () => {
              if (!window.confirm(`Delete ${b.name}?`)) return;
              try {
                await api.deleteBackup(b.name);
                qc.invalidateQueries({ queryKey: ["backups"] });
              } catch (e) {
                onError(message(e));
              }
            }}
          >
            Delete
          </button>
        </span>
      </div>
      {m && (
        <p className="text-xs text-zinc-500">
          {m.source_name} · {m.spec.arch} · {m.files.filter((f) => f.role.kind === "disk").length} disk(s)
          {m.files.some((f) => f.role.kind === "nvram") && " · UEFI vars"}
          {m.files.some((f) => f.role.kind === "tpm") && " · TPM"} · {new Date(m.created_at * 1000).toLocaleString()}
        </p>
      )}
      {open && m && (
        <form
          className="mt-2 flex flex-wrap items-end gap-2"
          onSubmit={async (e) => {
            e.preventDefault();
            setBusy(true);
            onError(null);
            try {
              await api.importBackup(b.name, name, pool);
              setStarted(true);
              qc.invalidateQueries({ queryKey: ["jobs"] });
            } catch (err) {
              onError(message(err));
            } finally {
              setBusy(false);
            }
          }}
        >
          <Field label="New VM name">
            <input className={inputCls} value={name} onChange={(e) => setName(e.target.value)} />
          </Field>
          <Field label="Disks in pool">
            <select className={inputCls} value={pool} onChange={(e) => setPool(e.target.value)}>
              {pools.data?.filter((p) => !p.readonly).map((p) => <option key={p.name}>{p.name}</option>)}
            </select>
          </Field>
          <Button type="submit" disabled={busy || !name}>
            {busy ? "Starting…" : "Import"}
          </Button>
          {started && <span className="text-xs text-zinc-500">Unpacking — see Jobs; the VM appears on the dashboard when done.</span>}
        </form>
      )}
    </li>
  );
}
