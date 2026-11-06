import { useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "../api/client";
import type { VmDetail } from "../api/generated/VmDetail";
import { SerialConsole } from "../components/SerialConsole";
import { SnapshotsTab } from "../components/SnapshotsTab";
import { VncConsole } from "../components/VncConsole";
import { VmEditor } from "../components/VmEditor";
import { Badge, Button, Card } from "../components/ui";
import { AccelBadge, StateBadge, VmActions, explainPause, useUptime } from "../components/vm";
import { navigate } from "../router";

const TABS = ["overview", "console", "snapshots", "settings", "events"] as const;

export default function VmPage({ id, tab }: { id: string; tab: string }) {
  const vm = useQuery({ queryKey: ["vm", id], queryFn: () => api.vm(id) });
  if (vm.isPending) return <p className="text-sm text-zinc-500">Loading…</p>;
  if (vm.isError) return <p className="text-sm text-rose-600">{vm.error.message}</p>;
  const d = vm.data;
  const active = d.summary.state !== "stopped" && d.summary.state !== "crashed";
  const current = (TABS as readonly string[]).includes(tab) ? tab : "overview";

  return (
    <div className="flex h-full max-w-7xl flex-col gap-4">
      <Header d={d} />
      <nav className="flex gap-1 border-b border-zinc-200 dark:border-zinc-800">
        {TABS.map((t) => (
          <a
            key={t}
            href={`#/vms/${id}/${t}`}
            className={`-mb-px border-b-2 px-3 py-1.5 text-sm capitalize ${
              current === t ? "border-zinc-900 font-medium dark:border-zinc-100" : "border-transparent text-zinc-500"
            }`}
          >
            {t}
          </a>
        ))}
      </nav>
      {current === "overview" && <Overview d={d} />}
      {current === "console" && (
        <div className="grid min-h-[36rem] flex-1 grid-cols-1 gap-4 2xl:grid-cols-[3fr_2fr]">
          <VncConsole vmId={id} running={active && d.spec.display.vnc} />
          <SerialConsole vmId={id} running={active} />
        </div>
      )}
      {current === "snapshots" && <SnapshotsTab vm={d.summary} />}
      {current === "settings" && <Settings d={d} />}
      {current === "events" && <Events id={id} />}
    </div>
  );
}

function Header({ d }: { d: VmDetail }) {
  const s = d.summary;
  const uptime = useUptime(s.started_at);
  return (
    <div className="flex flex-wrap items-center gap-3">
      <a href="#/dashboard" className="text-sm text-zinc-500 hover:underline">
        VMs /
      </a>
      <h1 className="text-xl font-semibold">{s.name}</h1>
      <StateBadge vm={s} />
      {s.state !== "stopped" && <AccelBadge accel={s.accel} />}
      {uptime && <span className="text-xs text-zinc-500">up {uptime}</span>}
      {s.restart_pending && <Badge tone="blue">restart to apply changes</Badge>}
      <div className="ml-auto">
        <VmActions vm={s} />
      </div>
    </div>
  );
}

function Overview({ d }: { d: VmDetail }) {
  const s = d.summary;
  const spec = d.spec;
  return (
    <div className="grid grid-cols-1 gap-4 lg:grid-cols-2">
      {(s.state === "crashed" || (s.state === "paused" && s.pause_reason !== "user")) && (
        <div className="lg:col-span-2">
          <Card title={s.state === "crashed" ? "Last run failed" : "Paused"}>
            {s.state === "paused" ? (
              <p className="text-sm">{explainPause(s.pause_reason)}</p>
            ) : (
              <pre className="text-xs whitespace-pre-wrap text-rose-600">{s.last_error}</pre>
            )}
          </Card>
        </div>
      )}
      <Card title="Hardware">
        <dl className="grid grid-cols-[8rem_1fr] gap-y-1 text-sm">
          <dt className="text-zinc-500">Architecture</dt>
          <dd className="font-mono">{spec.arch}</dd>
          <dt className="text-zinc-500">Machine</dt>
          <dd className="font-mono">{spec.machine.type}</dd>
          <dt className="text-zinc-500">CPU</dt>
          <dd>
            {spec.cpu.count} × <span className="font-mono">{spec.cpu.model ?? "default"}</span>
          </dd>
          <dt className="text-zinc-500">Memory</dt>
          <dd>{spec.memory.size_mib} MiB</dd>
          <dt className="text-zinc-500">Firmware</dt>
          <dd className="font-mono">{spec.firmware.kind === "descriptor" ? spec.firmware.id : "default"}</dd>
          <dt className="text-zinc-500">Display</dt>
          <dd className="font-mono">{spec.display.device ?? "none"}</dd>
        </dl>
      </Card>
      <Card title="Storage and network">
        <ul className="space-y-1 text-sm">
          {spec.disks.map((x, i) => (
            <li key={i}>
              <span className="font-mono">{x.image.name}</span>{" "}
              <span className="text-xs text-zinc-500">
                {x.media} on {x.bus}
                {x.boot_index != null ? ` · boot ${x.boot_index}` : ""}
                {x.readonly ? " · read-only" : ""}
              </span>
            </li>
          ))}
          {spec.nics.map((n, i) => (
            <li key={`n${i}`}>
              NAT · <span className="font-mono">{n.model}</span>{" "}
              <span className="font-mono text-xs text-zinc-500">{n.mac}</span>
              {n.backend.forwards.map((f, j) => (
                <span key={j} className="ml-2 text-xs text-zinc-500">
                  {f.host_addr}:{f.host_port} → {f.guest_port}/{f.proto}
                </span>
              ))}
            </li>
          ))}
        </ul>
      </Card>
    </div>
  );
}

function Settings({ d }: { d: VmDetail }) {
  const qc = useQueryClient();
  const [saving, setSaving] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const stopped = d.summary.state === "stopped" || d.summary.state === "crashed";
  return (
    <div className="space-y-4">
      {!stopped && (
        <p className="text-sm text-zinc-600 dark:text-zinc-400">
          The VM is running. Saved changes apply the next time it starts.
        </p>
      )}
      <VmEditor
        initial={d.spec}
        vmId={d.summary.id}
        lastRunArgs={d.last_run_args}
        saving={saving}
        saveLabel="Save changes"
        onSave={async (spec) => {
          setSaving(true);
          try {
            await api.updateVm(d.summary.id, spec);
            qc.invalidateQueries({ queryKey: ["vm", d.summary.id] });
            qc.invalidateQueries({ queryKey: ["vms"] });
          } finally {
            setSaving(false);
          }
        }}
      />
      <Card title="Danger zone">
        <div className="flex items-center gap-3">
          <Button
            variant="danger"
            disabled={!stopped || deleting}
            onClick={async () => {
              if (!window.confirm(`Delete ${d.summary.name}? Its disk images stay in the pool.`)) return;
              setDeleting(true);
              try {
                await api.deleteVm(d.summary.id);
                qc.invalidateQueries({ queryKey: ["vms"] });
                navigate("/dashboard");
              } catch (e) {
                setError((e as Error).message);
              } finally {
                setDeleting(false);
              }
            }}
          >
            Delete VM
          </Button>
          <span className="text-xs text-zinc-500">
            {stopped ? "Disk images are kept in the pool." : "Stop the VM before deleting it."}
          </span>
          {error && <span className="text-xs text-rose-600">{error}</span>}
        </div>
      </Card>
    </div>
  );
}

function Events({ id }: { id: string }) {
  const ev = useQuery({ queryKey: ["vm-events", id], queryFn: () => api.vmEvents(id) });
  if (ev.isPending) return <p className="text-sm text-zinc-500">Loading…</p>;
  if (ev.isError) return <p className="text-sm text-rose-600">{ev.error.message}</p>;
  return (
    <Card title="Recent events">
      <table className="w-full text-xs">
        <tbody>
          {[...ev.data].reverse().map((e) => (
            <tr key={e.seq} className="align-top">
              <td className="py-0.5 pr-3 whitespace-nowrap text-zinc-500">{new Date(e.at_us / 1000).toLocaleString()}</td>
              <td className="py-0.5 pr-3 font-mono">{e.kind}</td>
              <td className="py-0.5 font-mono break-all text-zinc-500">
                {e.data && Object.keys(e.data as object).length ? JSON.stringify(e.data) : ""}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </Card>
  );
}
