import { useMemo, useState } from "react";
import { useMutation, useQuery } from "@tanstack/react-query";
import { api } from "../api/client";
import type { SnapshotInfo } from "../api/generated/SnapshotInfo";
import type { VmSummary } from "../api/generated/VmSummary";
import { ProgressBar } from "../pages/JobsPage";
import { Badge, Button, Card, Field, inputCls } from "./ui";

/** Snapshots of one VM: a tree with the current position, take/revert/delete. */
export function SnapshotsTab({ vm }: { vm: VmSummary }) {
  const list = useQuery({ queryKey: ["snapshots", vm.id], queryFn: () => api.snapshots(vm.id) });
  const jobs = useQuery({ queryKey: ["jobs"], queryFn: api.jobs });
  const running = vm.state === "running" || vm.state === "paused";
  const active = (jobs.data ?? []).filter(
    (j) => (j.target as { vm?: string }).vm === vm.id && (j.status === "queued" || j.status === "running"),
  );
  const failed = (jobs.data ?? []).find(
    (j) => (j.target as { vm?: string }).vm === vm.id && j.kind.startsWith("vm.snapshot") && j.status === "failed",
  );

  const children = useMemo(() => {
    const m = new Map<string | null, SnapshotInfo[]>();
    const ids = new Set((list.data?.snapshots ?? []).map((s) => s.id));
    for (const s of list.data?.snapshots ?? []) {
      const key = s.parent_id && ids.has(s.parent_id) ? s.parent_id : null;
      m.set(key, [...(m.get(key) ?? []), s]);
    }
    return m;
  }, [list.data]);

  if (list.isPending) return <p className="text-sm text-zinc-500">Loading…</p>;
  if (list.isError) return <p className="text-sm text-rose-600">{list.error.message}</p>;
  const head = list.data.head;

  return (
    <div className="grid grid-cols-1 gap-4 lg:grid-cols-[minmax(0,3fr)_minmax(0,2fr)]">
      <Card title="Snapshot tree">
        {active.map((j) => (
          <div key={j.id} className="mb-3">
            <p className="mb-1 text-xs text-zinc-600 dark:text-zinc-400">
              {j.title}
              {j.message ? ` — ${j.message}` : ""}
            </p>
            <ProgressBar value={j.progress} />
          </div>
        ))}
        {failed && !active.length && (
          <p className="mb-3 text-xs text-rose-600">
            Last snapshot operation failed: {failed.error}
          </p>
        )}
        {!list.data.snapshots.length ? (
          <p className="text-sm text-zinc-500">No snapshots yet.</p>
        ) : (
          <Tree nodes={children.get(null) ?? []} childrenOf={children} head={head} vm={vm} depth={0} busy={active.length > 0} />
        )}
        {head === null && list.data.snapshots.length > 0 && (
          <p className="mt-3 text-xs text-zinc-500">The current state is not based on any snapshot.</p>
        )}
      </Card>
      <TakeSnapshot vm={vm} running={running} blocked={list.data.unsupported_reason} busy={active.length > 0} />
    </div>
  );
}

function Tree({
  nodes,
  childrenOf,
  head,
  vm,
  depth,
  busy,
}: {
  nodes: SnapshotInfo[];
  childrenOf: Map<string | null, SnapshotInfo[]>;
  head: string | null;
  vm: VmSummary;
  depth: number;
  busy: boolean;
}) {
  return (
    <ul className={depth ? "ml-4 border-l border-zinc-200 pl-3 dark:border-zinc-800" : ""}>
      {nodes.map((s) => (
        <li key={s.id} className="py-1">
          <Node s={s} isHead={s.id === head} vm={vm} busy={busy} />
          {(childrenOf.get(s.id) ?? []).length > 0 && (
            <Tree nodes={childrenOf.get(s.id)!} childrenOf={childrenOf} head={head} vm={vm} depth={depth + 1} busy={busy} />
          )}
        </li>
      ))}
    </ul>
  );
}

function Node({ s, isHead, vm, busy }: { s: SnapshotInfo; isHead: boolean; vm: VmSummary; busy: boolean }) {
  const [confirm, setConfirm] = useState<"revert" | "delete" | null>(null);
  const [safety, setSafety] = useState(true);
  const running = vm.state === "running" || vm.state === "paused";
  const revert = useMutation({ mutationFn: () => api.revertSnapshot(vm.id, s.id, safety), onSuccess: () => setConfirm(null) });
  const del = useMutation({ mutationFn: () => api.deleteSnapshot(vm.id, s.id), onSuccess: () => setConfirm(null) });
  const revertBlocked = running && !s.with_memory ? "Disk-only snapshot: stop the VM to revert to it" : null;
  return (
    <div className={`rounded px-2 py-1.5 ${isHead ? "bg-sky-50 dark:bg-sky-950/40" : ""}`}>
      <div className="flex flex-wrap items-center gap-2 text-sm">
        <span className="font-medium">{s.name}</span>
        {isHead && <Badge tone="blue">you are here</Badge>}
        {s.with_memory && <Badge tone="green" title="Includes RAM and device state">memory</Badge>}
        {s.auto && <Badge title="Taken automatically before a risky operation">auto</Badge>}
        <span className="text-xs text-zinc-500">{new Date(s.created_at * 1000).toLocaleString()}</span>
        <div className="ml-auto flex gap-1">
          <Button variant="ghost" disabled={busy || !!revertBlocked} title={revertBlocked ?? undefined} onClick={() => setConfirm("revert")}>
            Revert
          </Button>
          <Button variant="ghost" disabled={busy} onClick={() => setConfirm("delete")}>
            Delete
          </Button>
        </div>
      </div>
      {s.description && <p className="text-xs text-zinc-500">{s.description}</p>}
      {confirm === "revert" && (
        <div className="mt-2 rounded border border-zinc-200 p-2 text-xs dark:border-zinc-800">
          <p>
            {running
              ? "The VM jumps back to this snapshot's memory and disk state."
              : s.with_memory
                ? "The disks go back to this snapshot; the VM boots from them next time (memory state is not restored for a stopped VM)."
                : "The disks go back to this snapshot."}
          </p>
          <label className="mt-1 flex items-center gap-2">
            <input type="checkbox" checked={safety} onChange={(e) => setSafety(e.target.checked)} />
            Snapshot the current state first (recommended)
          </label>
          <div className="mt-2 flex gap-2">
            <Button onClick={() => revert.mutate()} disabled={revert.isPending}>
              Revert
            </Button>
            <Button variant="ghost" onClick={() => setConfirm(null)}>
              Cancel
            </Button>
          </div>
          {revert.isError && <p className="mt-1 text-rose-600">{revert.error.message}</p>}
        </div>
      )}
      {confirm === "delete" && (
        <div className="mt-2 flex items-center gap-2 rounded border border-zinc-200 p-2 text-xs dark:border-zinc-800">
          <span>Delete this snapshot? Its children stay, attached to its parent.</span>
          <Button variant="danger" onClick={() => del.mutate()} disabled={del.isPending}>
            Delete
          </Button>
          <Button variant="ghost" onClick={() => setConfirm(null)}>
            Cancel
          </Button>
          {del.isError && <span className="text-rose-600">{del.error.message}</span>}
        </div>
      )}
    </div>
  );
}

function TakeSnapshot({
  vm,
  running,
  blocked,
  busy,
}: {
  vm: VmSummary;
  running: boolean;
  blocked: string | null;
  busy: boolean;
}) {
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [memory, setMemory] = useState(true);
  const take = useMutation({
    mutationFn: () => api.takeSnapshot(vm.id, name, description, running && memory),
    onSuccess: () => {
      setName("");
      setDescription("");
    },
  });
  return (
    <Card title="Take a snapshot">
      {blocked ? (
        <p className="text-sm text-amber-700 dark:text-amber-400">{blocked}</p>
      ) : (
        <form
          className="space-y-3"
          onSubmit={(e) => {
            e.preventDefault();
            if (name.trim()) take.mutate();
          }}
        >
          <Field label="Name">
            <input className={inputCls} value={name} onChange={(e) => setName(e.target.value)} placeholder="before upgrade" />
          </Field>
          <Field label="Description (optional)">
            <input className={inputCls} value={description} onChange={(e) => setDescription(e.target.value)} />
          </Field>
          <label className={`flex items-center gap-2 text-xs ${running ? "" : "opacity-50"}`}>
            <input type="checkbox" disabled={!running} checked={running && memory} onChange={(e) => setMemory(e.target.checked)} />
            Include memory (resume exactly where it was; VM must be running)
          </label>
          {!running && <p className="text-xs text-zinc-500">The VM is stopped: this snapshot covers its disks.</p>}
          {running && !memory && (
            <p className="text-xs text-zinc-500">Disk-only while running is crash-consistent, like pulling the plug.</p>
          )}
          <Button type="submit" disabled={!name.trim() || take.isPending || busy}>
            Take snapshot
          </Button>
          {take.isError && <p className="text-xs text-rose-600">{take.error.message}</p>}
        </form>
      )}
    </Card>
  );
}
