import { useMemo, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "../api/client";
import type { BinaryCaps } from "../api/generated/BinaryCaps";
import type { HostCaps } from "../api/generated/HostCaps";
import { Badge, Button, Card, ParityBadge } from "../components/ui";

export default function SystemPage() {
  const qc = useQueryClient();
  const q = useQuery({
    queryKey: ["capabilities"],
    queryFn: api.capabilities,
    // Poll while a scan is running.
    refetchInterval: (query) => (query.state.data?.scan.running ? 1500 : false),
  });
  const rescan = useMutation({
    mutationFn: (force: boolean) => api.rescan(force),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["capabilities"] }),
  });

  if (q.isPending) return <p className="text-sm text-zinc-500">Loading…</p>;
  if (q.isError) return <p className="text-sm text-rose-600">Cannot reach the daemon: {q.error.message}</p>;

  const { caps, scan } = q.data;
  return (
    <div className="max-w-6xl space-y-5">
      <div className="flex flex-wrap items-center gap-3">
        <h1 className="text-xl font-semibold">System</h1>
        {caps && (
          <span className="text-sm text-zinc-500">
            {caps.host.os} · {caps.host.arch} · scanned {new Date(caps.scanned_at * 1000).toLocaleString()}
          </span>
        )}
        <div className="ml-auto flex items-center gap-2">
          {scan.running && (
            <span className="text-xs text-zinc-500">
              Probing {scan.done}/{scan.total}…
            </span>
          )}
          {scan.error && <Badge tone="red">scan failed: {scan.error}</Badge>}
          <Button onClick={() => rescan.mutate(true)} disabled={scan.running || rescan.isPending}>
            Rescan
          </Button>
        </div>
      </div>

      {!caps ? (
        <p className="text-sm text-zinc-500">First scan in progress…</p>
      ) : caps.binaries.length === 0 && caps.failed.length === 0 ? (
        <NoQemu />
      ) : (
        <>
          <Accelerators caps={caps} />
          <Parity caps={caps} />
          <Emulators caps={caps} />
          <Firmware caps={caps} />
        </>
      )}
    </div>
  );
}

function NoQemu() {
  return (
    <Card title="No QEMU found">
      <p className="text-sm">
        No <code>qemu-system-*</code> binaries were found in the project's <code>.tools/qemu</code>, the configured
        directories, or <code>PATH</code>. Run <code>scripts/bootstrap.sh qemu</code> (Windows) or install your
        distribution's QEMU packages, then rescan.
      </p>
    </Card>
  );
}

function Accelerators({ caps }: { caps: HostCaps }) {
  const native = caps.binaries.filter((b) => b.native);
  return (
    <Card title="Acceleration">
      {native.length === 0 ? (
        <p className="text-sm text-zinc-500">
          No QEMU binary for this host's architecture ({caps.host.arch}); every guest will be emulated (TCG).
        </p>
      ) : (
        <ul className="space-y-3">
          {native.map((b) => (
            <li key={b.binary.id}>
              <div className="mb-1 font-mono text-xs text-zinc-500">qemu-system-{b.arch}</div>
              <div className="flex flex-wrap gap-2">
                {b.accels.map((a) => (
                  <div key={a.name} className="min-w-48 rounded border border-zinc-200 p-2 text-sm dark:border-zinc-800">
                    <div className="flex items-center gap-2">
                      <span className="font-mono font-medium">{a.name}</span>
                      {a.available ? (
                        <Badge tone="green">{a.hardware ? "hardware" : "available"}</Badge>
                      ) : (
                        <Badge tone="amber">unavailable</Badge>
                      )}
                      {!a.hardware && <Badge>emulation (slow)</Badge>}
                    </div>
                    {a.reason && <p className="mt-1 text-xs text-zinc-500">{a.reason}</p>}
                    {a.remediation && <p className="mt-1 text-xs">{a.remediation}</p>}
                  </div>
                ))}
              </div>
            </li>
          ))}
        </ul>
      )}
    </Card>
  );
}

function Parity({ caps }: { caps: HostCaps }) {
  return (
    <Card title="Platform support">
      <table className="w-full text-sm">
        <thead className="text-left text-xs text-zinc-500">
          <tr>
            <th className="pb-2 font-medium">Feature</th>
            <th className="pb-2 font-medium">This host</th>
            <th className="pb-2 font-medium">Linux</th>
            <th className="pb-2 font-medium">Windows</th>
          </tr>
        </thead>
        <tbody className="divide-y divide-zinc-100 dark:divide-zinc-800">
          {caps.parity.map((r) => (
            <tr key={r.id} className="align-top">
              <td className="py-2 pr-4">{r.feature}</td>
              <td className="py-2 pr-4">
                {r.current ? (
                  <div>
                    <ParityBadge status={r.current.status} />
                    {r.probed && (
                      <span className="ml-1 text-xs text-zinc-500" title="Determined by probing this host">
                        probed
                      </span>
                    )}
                    {r.current.note && <p className="mt-1 max-w-sm text-xs text-zinc-500">{r.current.note}</p>}
                  </div>
                ) : (
                  <span className="text-xs text-zinc-500">not modelled</span>
                )}
              </td>
              <td className="py-2 pr-4" title={r.linux.note}>
                <ParityBadge status={r.linux.status} />
              </td>
              <td className="py-2" title={r.windows.note}>
                <ParityBadge status={r.windows.status} />
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </Card>
  );
}

function Emulators({ caps }: { caps: HostCaps }) {
  const [open, setOpen] = useState<string | null>(null);
  return (
    <Card title={`Emulators (${caps.binaries.length})`}>
      <table className="w-full text-sm">
        <thead className="text-left text-xs text-zinc-500">
          <tr>
            <th className="pb-2 font-medium">Architecture</th>
            <th className="pb-2 font-medium">Version</th>
            <th className="pb-2 font-medium">Mode</th>
            <th className="pb-2 font-medium">Default machine</th>
            <th className="pb-2 text-right font-medium">Machines</th>
            <th className="pb-2 text-right font-medium">CPUs</th>
            <th className="pb-2 text-right font-medium">Devices</th>
          </tr>
        </thead>
        <tbody className="divide-y divide-zinc-100 dark:divide-zinc-800">
          {caps.binaries.map((b) => {
            const best = b.accels.find((a) => a.available && a.hardware);
            const isOpen = open === b.binary.id;
            return [
              <tr
                key={b.binary.id}
                className="cursor-pointer hover:bg-zinc-50 dark:hover:bg-zinc-800/50"
                onClick={() => setOpen(isOpen ? null : b.binary.id)}
              >
                <td className="py-1.5 font-mono">
                  <span className="mr-1 inline-block w-3 text-zinc-400">{isOpen ? "▾" : "▸"}</span>
                  {b.arch}
                </td>
                <td className="py-1.5">{b.binary.version}</td>
                <td className="py-1.5">
                  {best ? <Badge tone="green">native ({best.name})</Badge> : <Badge>emulated (TCG)</Badge>}
                </td>
                <td className="py-1.5 font-mono text-xs">{b.machines.find((m) => m.is_default)?.name ?? "—"}</td>
                <td className="py-1.5 text-right tabular-nums">{b.machines.length}</td>
                <td className="py-1.5 text-right tabular-nums">{b.cpus.length || "—"}</td>
                <td className="py-1.5 text-right tabular-nums">{b.devices.length}</td>
              </tr>,
              isOpen && (
                <tr key={`${b.binary.id}-detail`}>
                  <td colSpan={7} className="bg-zinc-50 p-3 dark:bg-zinc-950">
                    <EmulatorDetail b={b} />
                  </td>
                </tr>
              ),
            ];
          })}
        </tbody>
      </table>
      {caps.failed.length > 0 && (
        <div className="mt-4">
          <h3 className="mb-1 text-xs font-semibold text-rose-600">Failed to probe</h3>
          <ul className="space-y-1 text-xs">
            {caps.failed.map((f) => (
              <li key={f.binary.path}>
                <span className="font-mono">{f.binary.path}</span>: {f.error}
              </li>
            ))}
          </ul>
        </div>
      )}
    </Card>
  );
}

function EmulatorDetail({ b }: { b: BinaryCaps }) {
  const [tab, setTab] = useState<"machines" | "devices" | "cpus" | "backends">("machines");
  const [filter, setFilter] = useState("");
  const f = filter.toLowerCase();
  return (
    <div className="space-y-3">
      <div className="text-xs text-zinc-500">
        <span className="font-mono">{b.binary.path}</span> · found via {b.binary.source} · QMP schema{" "}
        <span className="font-mono">{b.qmp_schema_hash.slice(0, 12)}</span>
      </div>
      <div className="flex items-center gap-1">
        {(["machines", "devices", "cpus", "backends"] as const).map((t) => (
          <Button key={t} variant={tab === t ? "default" : "ghost"} onClick={() => setTab(t)}>
            {t}
          </Button>
        ))}
        {tab !== "backends" && (
          <input
            className="ml-3 rounded border border-zinc-300 bg-transparent px-2 py-1 text-xs dark:border-zinc-700"
            placeholder="Filter…"
            value={filter}
            onChange={(e) => setFilter(e.target.value)}
          />
        )}
      </div>
      {tab === "machines" && (
        <ul className="max-h-80 overflow-auto text-xs">
          {b.machines
            .filter((m) => !f || m.name.includes(f) || m.description.toLowerCase().includes(f))
            .map((m) => (
              <li key={m.name} className="flex gap-3 py-0.5">
                <span className="w-56 shrink-0 font-mono">{m.name}</span>
                <span className="text-zinc-600 dark:text-zinc-400">{m.description}</span>
                {m.alias && <Badge tone="blue">alias {m.alias}</Badge>}
                {m.is_default && <Badge tone="green">default</Badge>}
                {m.deprecated && <Badge tone="amber">deprecated</Badge>}
                {m.cpu_max != null && <span className="ml-auto text-zinc-500">max {m.cpu_max} CPUs</span>}
              </li>
            ))}
        </ul>
      )}
      {tab === "devices" && <Devices b={b} filter={f} />}
      {tab === "cpus" && (
        <div className="flex max-h-80 flex-wrap gap-1 overflow-auto">
          {b.cpus.length === 0 && <p className="text-xs text-zinc-500">This target has no named CPU models.</p>}
          {b.cpus
            .filter((c) => !f || c.name.toLowerCase().includes(f))
            .map((c) => (
              <span
                key={c.name}
                className="rounded bg-zinc-200/60 px-1.5 py-0.5 font-mono text-xs dark:bg-zinc-800"
                title={c.unavailable_features.length ? `Unavailable features: ${c.unavailable_features.join(", ")}` : undefined}
              >
                {c.name}
                {c.deprecated && " (deprecated)"}
              </span>
            ))}
        </div>
      )}
      {tab === "backends" && (
        <dl className="grid grid-cols-[8rem_1fr] gap-y-1 text-xs">
          {Object.entries(b.backends).map(([k, v]) => (
            <div key={k} className="contents">
              <dt className="font-medium">{k}</dt>
              <dd className="font-mono">{v ? v.join(", ") : <Badge tone="red">not compiled in</Badge>}</dd>
            </div>
          ))}
        </dl>
      )}
    </div>
  );
}

function Devices({ b, filter }: { b: BinaryCaps; filter: string }) {
  const [selected, setSelected] = useState<string | null>(null);
  const groups = useMemo(() => {
    const m = new Map<string, typeof b.devices>();
    for (const d of b.devices) {
      if (filter && !d.name.includes(filter) && !(d.description ?? "").toLowerCase().includes(filter)) continue;
      const key = d.category || "Other";
      m.set(key, [...(m.get(key) ?? []), d]);
    }
    return [...m.entries()];
  }, [b.devices, filter]);

  return (
    <div className="grid grid-cols-[1fr_1fr] gap-4">
      <div className="max-h-96 overflow-auto text-xs">
        {groups.map(([cat, devs]) => (
          <div key={cat} className="mb-2">
            <div className="sticky top-0 bg-zinc-50 py-0.5 font-semibold dark:bg-zinc-950">{cat}</div>
            {devs.map((d) => (
              <button
                type="button"
                key={d.name}
                onClick={() => setSelected(d.name)}
                className={`flex w-full gap-2 rounded px-1 py-0.5 text-left hover:bg-zinc-200/60 dark:hover:bg-zinc-800 ${
                  selected === d.name ? "bg-zinc-200 dark:bg-zinc-800" : ""
                }`}
              >
                <span className="font-mono">{d.name}</span>
                {d.bus && <Badge>{d.bus}</Badge>}
                <span className="truncate text-zinc-500">{d.description}</span>
              </button>
            ))}
          </div>
        ))}
      </div>
      <div className="max-h-96 overflow-auto">
        {selected ? <DeviceProps binaryId={b.binary.id} device={selected} /> : (
          <p className="text-xs text-zinc-500">Select a device to see its properties (queried live from QEMU).</p>
        )}
      </div>
    </div>
  );
}

function DeviceProps({ binaryId, device }: { binaryId: string; device: string }) {
  const q = useQuery({
    queryKey: ["device-props", binaryId, device],
    queryFn: () => api.deviceProperties(binaryId, device),
    staleTime: Infinity,
  });
  if (q.isPending) return <p className="text-xs text-zinc-500">Querying QEMU…</p>;
  if (q.isError) return <p className="text-xs text-rose-600">{q.error.message}</p>;
  return (
    <table className="w-full text-xs">
      <caption className="mb-1 text-left font-mono font-semibold">{device}</caption>
      <tbody>
        {q.data.map((p) => (
          <tr key={p.name} className="align-top">
            <td className="py-0.5 pr-2 font-mono">{p.name}</td>
            <td className="py-0.5 pr-2 font-mono text-zinc-500">{p.type}</td>
            <td className="py-0.5 pr-2 font-mono">{p.default_value !== undefined && p.default_value !== null ? JSON.stringify(p.default_value) : ""}</td>
            <td className="py-0.5 text-zinc-500">{p.description}</td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}

function Firmware({ caps }: { caps: HostCaps }) {
  return (
    <Card title={`Firmware (${caps.firmware.length})`}>
      {caps.firmware.length === 0 ? (
        <p className="text-sm text-zinc-500">No firmware descriptors found.</p>
      ) : (
        <ul className="space-y-2 text-sm">
          {caps.firmware.map((fw) => (
            <li key={fw.id} className="flex flex-wrap items-center gap-2">
              <span className="font-mono text-xs">{fw.id}</span>
              <span>{fw.description}</span>
              {fw.interface_types.map((t) => (
                <Badge key={t} tone="blue">{t}</Badge>
              ))}
              {fw.features.includes("secure-boot") && <Badge tone="green">secure boot</Badge>}
              {!fw.complete && <Badge tone="red">files missing</Badge>}
              <span className="text-xs text-zinc-500">
                {fw.targets.map((t) => `${t.architecture}: ${t.machines.join(", ")}`).join(" · ")}
              </span>
            </li>
          ))}
        </ul>
      )}
    </Card>
  );
}
