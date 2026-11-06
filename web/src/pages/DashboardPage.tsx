import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "../api/client";
import type { VmSummary } from "../api/generated/VmSummary";
import { Badge, Button, Card, Field, inputCls } from "../components/ui";
import { AccelBadge, StateBadge, VmActions, explainPause, useUptime } from "../components/vm";

export default function DashboardPage() {
  const vms = useQuery({ queryKey: ["vms"], queryFn: api.vms });
  return (
    <div className="max-w-6xl space-y-5">
      <div className="flex items-center gap-3">
        <h1 className="text-xl font-semibold">Virtual machines</h1>
        <div className="ml-auto">
          <a href="#/vms/new">
            <Button>New VM</Button>
          </a>
        </div>
      </div>
      {vms.isPending ? (
        <p className="text-sm text-zinc-500">Loading…</p>
      ) : vms.isError ? (
        <p className="text-sm text-rose-600">Cannot reach the daemon: {vms.error.message}</p>
      ) : vms.data.length === 0 ? (
        <Card title="No VMs yet">
          <p className="text-sm">
            Create one with <a className="underline" href="#/vms/new">New VM</a>. Put installer ISOs in the image
            pool below first (uploads arrive in M2).
          </p>
        </Card>
      ) : (
        <div className="grid grid-cols-1 gap-3 md:grid-cols-2 xl:grid-cols-3">
          {vms.data.map((vm) => (
            <VmCard key={vm.id} vm={vm} />
          ))}
        </div>
      )}
      <ImagePool />
    </div>
  );
}

function VmCard({ vm }: { vm: VmSummary }) {
  const uptime = useUptime(vm.started_at);
  return (
    <div className="flex flex-col gap-2 rounded-lg border border-zinc-200 bg-white p-3 dark:border-zinc-800 dark:bg-zinc-900">
      <div className="flex items-center gap-2">
        <a href={`#/vms/${vm.id}`} className="truncate font-medium hover:underline">
          {vm.name}
        </a>
        <StateBadge vm={vm} />
        <span className="ml-auto font-mono text-xs text-zinc-500">{vm.arch}</span>
      </div>
      <div className="flex flex-wrap items-center gap-2 text-xs text-zinc-500">
        {vm.state !== "stopped" && <AccelBadge accel={vm.accel} />}
        {uptime && <span>up {uptime}</span>}
        {vm.restart_pending && (
          <Badge tone="blue" title="Settings changed since this run started">
            restart to apply changes
          </Badge>
        )}
      </div>
      {vm.state === "paused" && vm.pause_reason !== "user" && (
        <p className="text-xs text-amber-700 dark:text-amber-400">{explainPause(vm.pause_reason)}</p>
      )}
      {vm.last_error && vm.state === "crashed" && (
        <p className="line-clamp-3 font-mono text-xs whitespace-pre-wrap text-rose-600" title={vm.last_error}>
          {vm.last_error}
        </p>
      )}
      <div className="mt-auto flex items-center gap-1 pt-1">
        <VmActions vm={vm} compact />
        {vm.has_vnc && (
          <a href={`#/vms/${vm.id}/console`} className="ml-auto">
            <Button variant="ghost">Console</Button>
          </a>
        )}
      </div>
    </div>
  );
}

function formatBytes(n: number): string {
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let i = 0;
  let v = n;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(v < 10 && i > 0 ? 1 : 0)} ${units[i]}`;
}

function ImagePool() {
  const qc = useQueryClient();
  const pool = useQuery({ queryKey: ["pool", "default"], queryFn: () => api.pool() });
  const [name, setName] = useState("");
  const [size, setSize] = useState(20);
  const create = useMutation({
    mutationFn: () => api.createImage(name.endsWith(".qcow2") ? name : `${name}.qcow2`, size * 1024 ** 3),
    onSuccess: () => {
      setName("");
      qc.invalidateQueries({ queryKey: ["pool"] });
    },
  });
  return (
    <Card
      title="Images (default pool)"
      actions={
        <Button variant="ghost" onClick={() => qc.invalidateQueries({ queryKey: ["pool"] })}>
          Refresh
        </Button>
      }
    >
      {pool.data && (
        <p className="mb-3 text-xs text-zinc-500">
          Folder: <span className="font-mono">{pool.data.path}</span> — copy ISOs and disk images here, then refresh.
        </p>
      )}
      {pool.data?.images.length ? (
        <table className="mb-4 w-full text-sm">
          <thead className="text-left text-xs text-zinc-500">
            <tr>
              <th className="pb-1 font-medium">Name</th>
              <th className="pb-1 font-medium">Format</th>
              <th className="pb-1 text-right font-medium">Size</th>
              <th className="pb-1 text-right font-medium">On disk</th>
              <th className="pb-1 pl-4 font-medium">Used by</th>
            </tr>
          </thead>
          <tbody>
            {pool.data.images.map((i) => (
              <tr key={i.name}>
                <td className="py-0.5 font-mono text-xs">{i.name}</td>
                <td className="py-0.5 text-xs">{i.format}</td>
                <td className="py-0.5 text-right text-xs tabular-nums">{formatBytes(i.virtual_size)}</td>
                <td className="py-0.5 text-right text-xs tabular-nums">{formatBytes(i.actual_size)}</td>
                <td className="py-0.5 pl-4 text-xs">{i.used_by.join(", ")}</td>
              </tr>
            ))}
          </tbody>
        </table>
      ) : (
        <p className="mb-4 text-sm text-zinc-500">No images yet.</p>
      )}
      <form
        className="flex flex-wrap items-end gap-2"
        onSubmit={(e) => {
          e.preventDefault();
          if (name) create.mutate();
        }}
      >
        <Field label="New disk name">
          <input className={inputCls} value={name} onChange={(e) => setName(e.target.value)} placeholder="debian.qcow2" />
        </Field>
        <Field label="Size (GiB)">
          <input
            className={`${inputCls} w-24`}
            type="number"
            min={1}
            value={size}
            onChange={(e) => setSize(Number(e.target.value))}
          />
        </Field>
        <Button type="submit" disabled={!name || create.isPending}>
          Create qcow2 disk
        </Button>
        {create.isError && <span className="text-xs text-rose-600">{create.error.message}</span>}
      </form>
    </Card>
  );
}
