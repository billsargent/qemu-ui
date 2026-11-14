import { useEffect, useMemo, useState } from "react";
import { useMutation, useQuery } from "@tanstack/react-query";
import { ApiError, api } from "../api/client";
import type { BinaryCaps } from "../api/generated/BinaryCaps";
import type { CacheMode } from "../api/generated/CacheMode";
import type { CompileResponse } from "../api/generated/CompileResponse";
import type { Diagnostic } from "../api/generated/Diagnostic";
import type { DiskBus } from "../api/generated/DiskBus";
import type { DiskSpec } from "../api/generated/DiskSpec";
import type { Firmware } from "../api/generated/Firmware";
import type { LintResult } from "../api/generated/LintResult";
import type { Media } from "../api/generated/Media";
import type { NicSpec } from "../api/generated/NicSpec";
import type { PortForward } from "../api/generated/PortForward";
import type { ShareSpec } from "../api/generated/ShareSpec";
import type { VmSpec } from "../api/generated/VmSpec";
import { MacField, NicModelField, VirtioNetFields } from "./NicFields";
import { Badge, Button, Card, Field, inputCls } from "./ui";
import { kindLabel } from "../pages/NetworksPage";

// ---------------------------------------------------------------- helpers

function globMatch(pattern: string, s: string): boolean {
  const re = new RegExp(`^${pattern.split("*").map((p) => p.replace(/[.+?^${}()|[\]\\]/g, "\\$&")).join(".*")}$`);
  return re.test(s);
}

function firmwareFor(fw: Firmware[], arch: string, machine: string): Firmware[] {
  return fw.filter((f) => f.complete && f.targets.some((t) => t.architecture === arch && t.machines.some((m) => globMatch(m, machine))));
}

function useDebounced<T>(value: T, ms: number): T {
  const [v, setV] = useState(value);
  useEffect(() => {
    const t = setTimeout(() => setV(value), ms);
    return () => clearTimeout(t);
  }, [value, ms]);
  return v;
}

/** Token diff (LCS) between the last run's argv and the new one. */
function diffTokens(a: string[], b: string[]): { op: "same" | "add" | "del"; t: string }[] {
  const n = a.length;
  const m = b.length;
  const dp = Array.from({ length: n + 1 }, () => new Array<number>(m + 1).fill(0));
  for (let i = n - 1; i >= 0; i--)
    for (let j = m - 1; j >= 0; j--) dp[i][j] = a[i] === b[j] ? dp[i + 1][j + 1] + 1 : Math.max(dp[i + 1][j], dp[i][j + 1]);
  const out: { op: "same" | "add" | "del"; t: string }[] = [];
  let i = 0;
  let j = 0;
  while (i < n && j < m) {
    if (a[i] === b[j]) {
      out.push({ op: "same", t: a[i] });
      i++;
      j++;
    } else if (dp[i + 1][j] >= dp[i][j + 1]) out.push({ op: "del", t: a[i++] });
    else out.push({ op: "add", t: b[j++] });
  }
  while (i < n) out.push({ op: "del", t: a[i++] });
  while (j < m) out.push({ op: "add", t: b[j++] });
  return out;
}

function Diags({ list }: { list: Diagnostic[] }) {
  if (!list.length) return null;
  return (
    <ul className="mt-2 space-y-1">
      {list.map((d, i) => (
        <li key={i} className={`text-xs ${d.severity === "error" ? "text-rose-600" : "text-amber-700 dark:text-amber-400"}`}>
          {d.severity === "error" ? "✕" : "!"} {d.message}{" "}
          <span className="font-mono text-zinc-400">[{d.rule}]</span>
        </li>
      ))}
    </ul>
  );
}

const BUS_LABEL: Record<DiskBus, string> = {
  virtio: "virtio-blk (fastest; needs virtio drivers)",
  scsi: "virtio-scsi",
  nvme: "NVMe",
  ide: "IDE / SATA (works with any installer)",
  usb: "USB storage",
};
// Trade-offs shown under the Bus and Cache fields (M2-09).
const BUS_HINT: Record<DiskBus, string> = {
  virtio: "Fastest. Linux has drivers built in; Windows needs the virtio-win drivers during setup.",
  scsi: "virtio-scsi: many disks, discard/TRIM, CD-ROMs, hotplug. Needs virtio drivers on Windows.",
  nvme: "Fast and natively supported by modern OSes; one controller per disk.",
  ide: "IDE/SATA works with every installer, but is slow and cannot be hotplugged.",
  usb: "USB storage: removable, hotpluggable, slow. Handy for small images.",
};
const CACHE_HINT: Record<CacheMode, string> = {
  none: "Bypasses the host page cache (best default). Windows hosts cannot do direct I/O and use writeback.",
  writeback: "Uses the host cache; fast, and safe as long as the guest flushes.",
  writethrough: "Every write reaches the disk before it completes; safe but slow.",
  directsync: "Direct I/O plus synchronous writes; safest, slowest.",
  unsafe: "Ignores flushes. Only for throwaway VMs: a host crash can corrupt the image.",
};

const CACHE_LABEL: Record<CacheMode, string> = {
  none: "none (direct I/O)",
  writeback: "writeback",
  writethrough: "writethrough",
  directsync: "directsync",
  unsafe: "unsafe (ignores flushes)",
};

// ----------------------------------------------------------------- editor

export function VmEditor({
  initial,
  vmId,
  lastRunArgs,
  saving,
  onSave,
  saveLabel,
  archEditable,
  onArchChange,
}: {
  initial: VmSpec;
  vmId?: string;
  lastRunArgs?: string[] | null;
  saving: boolean;
  onSave: (spec: VmSpec) => Promise<void>;
  saveLabel: string;
  archEditable?: boolean;
  onArchChange?: (arch: string) => void;
}) {
  const [spec, setSpec] = useState<VmSpec>(initial);
  const [saveError, setSaveError] = useState<{ message: string; diagnostics: Diagnostic[] } | null>(null);
  useEffect(() => setSpec(initial), [initial]);

  const caps = useQuery({ queryKey: ["capabilities"], queryFn: api.capabilities });
  const pool = useQuery({ queryKey: ["images", "all"], queryFn: api.allImages });
  const hostDisks = useQuery({ queryKey: ["host-disks"], queryFn: api.hostDisks, staleTime: 30_000 });
  const networks = useQuery({ queryKey: ["networks"], queryFn: api.networks });
  const bin: BinaryCaps | undefined = caps.data?.caps?.binaries.find((b) => b.arch === spec.arch);
  // Which disk buses this machine actually has (from QEMU introspection).
  const busSupport = useQuery({
    queryKey: ["buses", spec.arch, spec.machine.type],
    queryFn: () => api.diskBuses(spec.arch, spec.machine.type),
    enabled: !!spec.machine.type,
    staleTime: Infinity,
  });
  const busInfo = (b: DiskBus) => busSupport.data?.find((s) => s.bus === b);

  // Live argv (M1-14): compile on every change, debounced.
  const debounced = useDebounced(spec, 300);
  const compiled = useQuery<CompileResponse>({
    queryKey: ["compile", vmId ?? "new", debounced],
    queryFn: () => api.compile(debounced, vmId),
    placeholderData: (prev) => prev,
  });
  // "Check config" (M1-10): QEMU itself validates, without side effects.
  const lint = useMutation({ mutationFn: () => api.lint(spec, vmId) });
  const lintFresh = lint.data;
  useEffect(() => lint.reset(), [debounced]); // eslint-disable-line react-hooks/exhaustive-deps
  const diags = useMemo(
    () => [
      ...(compiled.data?.diagnostics ?? []),
      ...(saveError?.diagnostics ?? []),
      ...(lintFresh?.diagnostics.filter((d) => d.rule === "qemu.lint") ?? []),
    ],
    [compiled.data, saveError, lintFresh],
  );
  const diagsFor = (prefix: string) => diags.filter((d) => d.field === prefix || d.field.startsWith(`${prefix}.`) || d.field.startsWith(`${prefix}[`));
  const hasErrors = diags.some((d) => d.severity === "error");

  const set = (patch: Partial<VmSpec>) => {
    setSaveError(null);
    setSpec((s) => ({ ...s, ...patch }));
  };

  const machines = useMemo(
    () => [...(bin?.machines ?? [])].filter((m) => m.name !== "none").sort((a, b) => Number(a.deprecated) - Number(b.deprecated)),
    [bin],
  );
  const cpus = bin?.cpus ?? [];
  const hardware = bin?.accels.some((a) => a.available && a.hardware);
  const displayDevices = (bin?.devices ?? []).filter((d) => d.category === "Display");
  const nicModels = (bin?.devices ?? []).filter((d) => d.category === "Network");
  const firmware = firmwareFor(caps.data?.caps?.firmware ?? [], spec.arch, spec.machine.type);
  const images = pool.data ?? [];

  const save = async () => {
    setSaveError(null);
    try {
      await onSave(spec);
    } catch (e) {
      if (e instanceof ApiError) setSaveError({ message: e.message, diagnostics: e.diagnostics });
      else setSaveError({ message: (e as Error).message, diagnostics: [] });
    }
  };

  const updateDisk = (i: number, patch: Partial<DiskSpec>) =>
    set({ disks: spec.disks.map((d, j) => (j === i ? { ...d, ...patch } : d)) });
  const updateNic = (i: number, patch: Partial<NicSpec>) =>
    set({ nics: spec.nics.map((n, j) => (j === i ? { ...n, ...patch } : n)) });
  const setForwards = (i: number, forwards: PortForward[]) =>
    updateNic(i, { backend: { ...spec.nics[i].backend, forwards } });

  return (
    <div className="grid grid-cols-1 gap-4 xl:grid-cols-[minmax(0,1fr)_minmax(0,30rem)]">
      <div className="space-y-4">
        <Card title="General">
          <div className="grid grid-cols-2 gap-3">
            <Field label="Name">
              <input className={inputCls} value={spec.name} onChange={(e) => set({ name: e.target.value })} />
            </Field>
            <Field label="Architecture" hint={bin && (bin.native ? "Hardware acceleration possible" : "Emulated with TCG (slow)")}>
              <select
                className={inputCls}
                value={spec.arch}
                disabled={!archEditable}
                onChange={(e) => onArchChange?.(e.target.value)}
              >
                {(caps.data?.caps?.binaries ?? []).map((b) => (
                  <option key={b.arch} value={b.arch}>
                    {b.arch}
                    {b.native ? " (native)" : ""}
                  </option>
                ))}
              </select>
            </Field>
            <Field label="Machine" hint={machines.find((m) => m.name === spec.machine.type)?.description}>
              <select className={inputCls} value={spec.machine.type} onChange={(e) => set({ machine: { ...spec.machine, type: e.target.value } })}>
                {machines.map((m) => (
                  <option key={m.name} value={m.name}>
                    {m.name}
                    {m.alias ? ` (${m.alias})` : ""}
                    {m.deprecated ? " — deprecated" : ""}
                  </option>
                ))}
              </select>
            </Field>
            <Field label="Accelerator">
              <select className={inputCls} value={spec.accel} onChange={(e) => set({ accel: e.target.value })}>
                <option value="auto">auto (best available)</option>
                {(bin?.accels ?? []).map((a) => (
                  <option key={a.name} value={a.name} disabled={!a.available}>
                    {a.name}
                    {a.available ? "" : " — unavailable"}
                  </option>
                ))}
              </select>
            </Field>
          </div>
          <Diags list={[...diagsFor("name"), ...diagsFor("arch"), ...diagsFor("machine"), ...diagsFor("accel")]} />
        </Card>

        <Card title="CPU and memory">
          <div className="grid grid-cols-3 gap-3">
            <Field label="CPU model">
              <select
                className={inputCls}
                value={spec.cpu.model ?? ""}
                onChange={(e) => set({ cpu: { ...spec.cpu, model: e.target.value || null } })}
              >
                <option value="">machine default</option>
                {hardware && <option value="host">host (pass through host CPU)</option>}
                {cpus.map((c) => (
                  <option key={c.name} value={c.name}>
                    {c.name}
                    {c.deprecated ? " — deprecated" : ""}
                  </option>
                ))}
              </select>
            </Field>
            <Field label="vCPUs">
              <input
                className={inputCls}
                type="number"
                min={1}
                value={spec.cpu.count}
                onChange={(e) => set({ cpu: { ...spec.cpu, count: Number(e.target.value) } })}
              />
            </Field>
            <Field label="Memory (MiB)">
              <input
                className={inputCls}
                type="number"
                min={16}
                step={256}
                value={spec.memory.size_mib}
                onChange={(e) => set({ memory: { size_mib: Number(e.target.value) } })}
              />
            </Field>
          </div>
          <label className="mt-3 flex items-center gap-2 text-xs">
            <input
              type="checkbox"
              checked={!!spec.cpu.topology}
              onChange={(e) =>
                set({
                  cpu: { ...spec.cpu, topology: e.target.checked ? { sockets: 1, cores: spec.cpu.count, threads: 1 } : null },
                })
              }
            />
            Explicit topology
          </label>
          {spec.cpu.topology && (
            <div className="mt-2 grid grid-cols-3 gap-3">
              {(["sockets", "cores", "threads"] as const).map((k) => (
                <Field key={k} label={k}>
                  <input
                    className={inputCls}
                    type="number"
                    min={1}
                    value={spec.cpu.topology![k]}
                    onChange={(e) => set({ cpu: { ...spec.cpu, topology: { ...spec.cpu.topology!, [k]: Number(e.target.value) } } })}
                  />
                </Field>
              ))}
            </div>
          )}
          <Diags list={[...diagsFor("cpu"), ...diagsFor("memory")]} />
        </Card>

        <Card title="Firmware">
          <select
            className={inputCls}
            value={spec.firmware.kind === "descriptor" ? spec.firmware.id : ""}
            onChange={(e) => set({ firmware: e.target.value ? { kind: "descriptor", id: e.target.value } : { kind: "default" } })}
          >
            <option value="">Machine default (BIOS on x86)</option>
            {firmware.map((f) => (
              <option key={f.id} value={f.id}>
                {f.description || f.id}
                {f.features.includes("secure-boot") ? " — Secure Boot" : ""}
              </option>
            ))}
          </select>
          {firmware.length === 0 && <p className="mt-2 text-xs text-zinc-500">No installed firmware matches this machine.</p>}
          <Diags list={diagsFor("firmware")} />
        </Card>

        <Card
          title="Disks"
          actions={
            <Button
              variant="ghost"
              onClick={() =>
                set({
                  disks: [
                    ...spec.disks,
                    {
                      image: { pool: images[0]?.pool ?? "default", name: images[0]?.name ?? "" },
                      bus: "virtio",
                      media: "disk",
                      readonly: false,
                      boot_index: null,
                      cache: "none",
                      aio: null,
                      discard: true,
                    },
                  ],
                })
              }
            >
              Add disk
            </Button>
          }
        >
          {spec.disks.length === 0 && <p className="text-sm text-zinc-500">No disks. Add an installer ISO and a system disk.</p>}
          <div className="space-y-3">
            {spec.disks.map((d, i) => (
              <div key={i} className="rounded border border-zinc-200 p-2 dark:border-zinc-800">
                <div className="grid grid-cols-2 gap-2 md:grid-cols-4">
                  <Field label="Image">
                    <select
                      className={inputCls}
                      value={d.image.name ? `${d.image.pool}/${d.image.name}` : ""}
                      onChange={(e) => {
                        const [p, ...rest] = e.target.value.split("/");
                        updateDisk(i, { image: { pool: p, name: rest.join("/") } });
                      }}
                    >
                      <option value="">— choose —</option>
                      {images.map((img) => (
                        <option key={`${img.pool}/${img.name}`} value={`${img.pool}/${img.name}`}>
                          {img.pool === "default" ? "" : `${img.pool} / `}
                          {img.name} ({img.format})
                        </option>
                      ))}
                      {/* Physical disks, exported by the privileged helper. */}
                      {hostDisks.data?.available && (
                        <optgroup label="Host disks (via the qemu-ui helper)">
                          {hostDisks.data.disks.map((hd) => (
                            <option key={hd.id} value={`@host/${hd.id}`} disabled={hd.system}>
                              {hd.label} · {Math.round(hd.size_bytes / 1024 ** 3)} GiB · {hd.bus}
                              {hd.system ? " (system disk)" : hd.in_use.length ? " (in use)" : ""}
                            </option>
                          ))}
                        </optgroup>
                      )}
                      {d.image.pool === "@host" && !hostDisks.data?.disks.some((hd) => hd.id === d.image.name) && (
                        <option value={`@host/${d.image.name}`}>Host disk {d.image.name} (not visible now)</option>
                      )}
                    </select>
                    {d.image.pool === "@host" && (
                      <span className="text-xs text-zinc-500">
                        Physical disk, exported read-only unless allowed in the helper&apos;s configuration. Attach while the VM is stopped.
                      </span>
                    )}
                    {!hostDisks.data?.available && hostDisks.data?.reason && (
                      <span className="text-xs text-zinc-500" title={hostDisks.data.reason}>
                        Host disks need the qemu-ui helper (not running).
                      </span>
                    )}
                  </Field>
                  <Field label="Media">
                    <select
                      className={inputCls}
                      value={d.media}
                      onChange={(e) => {
                        const media = e.target.value as Media;
                        updateDisk(i, { media, readonly: media === "cdrom" ? true : d.readonly, bus: media === "cdrom" && (d.bus === "virtio" || d.bus === "nvme") ? "ide" : d.bus });
                      }}
                    >
                      <option value="disk">Disk</option>
                      <option value="cdrom">CD-ROM</option>
                    </select>
                  </Field>
                  <Field
                    label="Bus"
                    hint={busInfo(d.bus)?.available === false ? busInfo(d.bus)?.reason : BUS_HINT[d.bus]}
                  >
                    <select className={inputCls} value={d.bus} onChange={(e) => updateDisk(i, { bus: e.target.value as DiskBus })}>
                      {(Object.keys(BUS_LABEL) as DiskBus[]).map((b) => {
                        const s = busInfo(b);
                        const unavailable = s?.available === false;
                        return (
                          <option key={b} value={b} disabled={unavailable && b !== d.bus} title={unavailable ? (s?.reason ?? "") : BUS_HINT[b]}>
                            {BUS_LABEL[b]}
                            {unavailable ? " — not on this machine" : ""}
                          </option>
                        );
                      })}
                    </select>
                  </Field>
                  <Field label="Boot order">
                    <input
                      className={inputCls}
                      type="number"
                      min={0}
                      placeholder="—"
                      value={d.boot_index ?? ""}
                      onChange={(e) => updateDisk(i, { boot_index: e.target.value === "" ? null : Number(e.target.value) })}
                    />
                  </Field>
                  <Field label="Cache" hint={CACHE_HINT[d.cache]}>
                    <select className={inputCls} value={d.cache} onChange={(e) => updateDisk(i, { cache: e.target.value as CacheMode })}>
                      {(Object.keys(CACHE_LABEL) as CacheMode[]).map((c) => (
                        <option key={c} value={c}>
                          {CACHE_LABEL[c]}
                        </option>
                      ))}
                    </select>
                  </Field>
                  <Field
                    label="I/O mode"
                    hint={
                      d.aio === "io_uring"
                        ? "Linux 5.1+ hosts with a QEMU built with io_uring; Check with QEMU confirms."
                        : d.aio === "native"
                          ? "Linux AIO; needs cache none or directsync (direct I/O)."
                          : "Thread pool: works on every host."
                    }
                  >
                    <select className={inputCls} value={d.aio ?? ""} onChange={(e) => updateDisk(i, { aio: e.target.value || null })}>
                      <option value="">threads (default)</option>
                      <option value="native">native (Linux)</option>
                      <option value="io_uring">io_uring (Linux)</option>
                    </select>
                  </Field>
                  <label className="flex items-end gap-2 pb-1 text-xs">
                    <input type="checkbox" checked={d.readonly} onChange={(e) => updateDisk(i, { readonly: e.target.checked })} />
                    Read-only
                  </label>
                  <div className="flex items-end justify-end md:col-span-2">
                    <Button variant="ghost" onClick={() => set({ disks: spec.disks.filter((_, j) => j !== i) })}>
                      Remove
                    </Button>
                  </div>
                </div>
                <Diags list={diagsFor(`disks[${i}]`)} />
              </div>
            ))}
          </div>
        </Card>

        <Card
          title="Network"
          actions={
            <Button
              variant="ghost"
              onClick={() =>
                set({ nics: [...spec.nics, { backend: { kind: "user", forwards: [] }, model: nicModels[0]?.name ?? "e1000", mac: null }] })
              }
            >
              Add NIC
            </Button>
          }
        >
          {spec.nics.length === 0 && <p className="text-sm text-zinc-500">No network.</p>}
          {spec.nics.map((n, i) => (
            <div key={i} className="mb-3 rounded border border-zinc-200 p-2 dark:border-zinc-800">
              <div className="grid grid-cols-3 gap-2">
                <NicModelField
                  arch={spec.arch}
                  machine={spec.machine.type}
                  value={n.model}
                  fallback={nicModels}
                  onChange={(model) => updateNic(i, { model })}
                />
                <Field
                  label="Network"
                  hint={
                    n.backend.kind === "user" ? (
                      "this VM's own NAT"
                    ) : (
                      <a className="underline" href="#/networks">
                        manage networks
                      </a>
                    )
                  }
                >
                  <select
                    className={inputCls}
                    value={n.backend.kind === "user" ? "" : n.backend.network}
                    onChange={(e) =>
                      updateNic(i, {
                        backend: e.target.value
                          ? { kind: "network", network: e.target.value, forwards: n.backend.forwards }
                          : { kind: "user", forwards: n.backend.forwards },
                      })
                    }
                  >
                    <option value="">Private NAT</option>
                    {(networks.data ?? []).map((net) => (
                      <option key={net.id} value={net.id}>
                        {net.name} ({kindLabel(net.config)})
                      </option>
                    ))}
                    {n.backend.kind === "network" &&
                      ((id) =>
                        !(networks.data ?? []).some((net) => net.id === id) && <option value={id}>missing network</option>)(
                        n.backend.network,
                      )}
                  </select>
                </Field>
                <MacField value={n.mac} onChange={(mac) => updateNic(i, { mac })} />
              </div>
              <VirtioNetFields nic={n} onChange={(patch) => updateNic(i, patch)} />
              <div className="mt-2 text-xs">
                <div className="mb-1 font-medium text-zinc-600 dark:text-zinc-400">Port forwards</div>
                {n.backend.forwards.map((f, j) => (
                  <div key={j} className="mb-1 flex items-center gap-1">
                    <select
                      className={inputCls}
                      value={f.proto}
                      onChange={(e) =>
                        setForwards(i, n.backend.forwards.map((x, k) => (k === j ? { ...x, proto: e.target.value } : x)))
                      }
                    >
                      <option>tcp</option>
                      <option>udp</option>
                    </select>
                    <input
                      className={`${inputCls} w-28 font-mono`}
                      value={f.host_addr}
                      onChange={(e) =>
                        setForwards(i, n.backend.forwards.map((x, k) => (k === j ? { ...x, host_addr: e.target.value } : x)))
                      }
                    />
                    :
                    <input
                      className={`${inputCls} w-20`}
                      type="number"
                      value={f.host_port}
                      onChange={(e) =>
                        setForwards(i, n.backend.forwards.map((x, k) => (k === j ? { ...x, host_port: Number(e.target.value) } : x)))
                      }
                    />
                    → guest
                    <input
                      className={`${inputCls} w-20`}
                      type="number"
                      value={f.guest_port}
                      onChange={(e) =>
                        setForwards(i, n.backend.forwards.map((x, k) => (k === j ? { ...x, guest_port: Number(e.target.value) } : x)))
                      }
                    />
                    <Button variant="ghost" onClick={() => setForwards(i, n.backend.forwards.filter((_, k) => k !== j))}>
                      ✕
                    </Button>
                  </div>
                ))}
                <Button
                  variant="ghost"
                  onClick={() =>
                    setForwards(i, [...n.backend.forwards, { proto: "tcp", host_addr: "127.0.0.1", host_port: 2222, guest_port: 22 }])
                  }
                >
                  Add forward
                </Button>
              </div>
              <div className="mt-2 flex justify-end">
                <Button variant="ghost" onClick={() => set({ nics: spec.nics.filter((_, j) => j !== i) })}>
                  Remove NIC
                </Button>
              </div>
              <Diags list={diagsFor(`nics[${i}]`)} />
            </div>
          ))}
        </Card>

        <Card title="Display and input">
          <div className="grid grid-cols-2 gap-3">
            <Field label="Graphics device">
              <select className={inputCls} value={spec.display.device ?? ""} onChange={(e) => set({ display: { ...spec.display, device: e.target.value || null } })}>
                <option value="">none (headless)</option>
                {displayDevices.map((d) => (
                  <option key={d.name} value={d.name}>
                    {d.name}
                  </option>
                ))}
              </select>
            </Field>
            <div className="flex flex-col justify-end gap-1 text-xs">
              <label className="flex items-center gap-2">
                <input type="checkbox" checked={spec.display.vnc} onChange={(e) => set({ display: { ...spec.display, vnc: e.target.checked } })} />
                Browser console (VNC)
              </label>
              <label className="flex items-center gap-2" title="Clipboard sharing and a pointer that never drifts; needs spice-vdagent / virtio-win guest tools">
                <input type="checkbox" checked={spec.display.vdagent} onChange={(e) => set({ display: { ...spec.display, vdagent: e.target.checked } })} />
                Guest agent clipboard + absolute pointer (qemu-vdagent)
              </label>
              <label className="flex items-center gap-2">
                <input type="checkbox" checked={spec.input.usb_tablet} onChange={(e) => set({ input: { usb_tablet: e.target.checked } })} />
                USB tablet (absolute pointer without guest tools)
              </label>
            </div>
          </div>
          <Diags list={[...diagsFor("display"), ...diagsFor("input")]} />
        </Card>

        <SharesCard
          shares={spec.shares}
          onChange={(shares) => set({ shares })}
          diagsFor={(i) => diagsFor(`shares[${i}]`)}
          other={diagsFor("shares").filter((d) => d.field === "shares")}
        />

        <Card title="Extra QEMU options (passthrough)">
          <p className="mb-2 text-xs text-zinc-500">
            One argv token per line, appended verbatim after the generated options. Options the editor manages are refused.
          </p>
          <textarea
            className={`${inputCls} h-24 w-full font-mono text-xs`}
            value={spec.passthrough.join("\n")}
            onChange={(e) => set({ passthrough: e.target.value.split("\n").filter((t) => t.length > 0) })}
            placeholder={"-global\nICH9-LPC.disable_s3=1"}
          />
          {spec.passthrough.length > 0 && (
            <p className="mt-1 text-xs">
              <Badge tone="amber">{spec.passthrough.length} token(s) set outside the editor</Badge>
            </p>
          )}
          <Diags list={diagsFor("passthrough")} />
        </Card>

        <div className="flex items-center gap-3">
          <Button onClick={save} disabled={saving || hasErrors}>
            {saving ? "Saving…" : saveLabel}
          </Button>
          {hasErrors && <span className="text-xs text-rose-600">Fix the errors above to save.</span>}
          {saveError && <span className="text-xs text-rose-600">{saveError.message}</span>}
        </div>
      </div>

      <ArgvDrawer
        compiled={compiled.data}
        loading={compiled.isFetching}
        lastRunArgs={lastRunArgs ?? null}
        diags={diags}
        lint={{ run: () => lint.mutate(), pending: lint.isPending, result: lint.data ?? null, error: lint.error?.message ?? null }}
      />
    </div>
  );
}

function ArgvDrawer({
  compiled,
  loading,
  lastRunArgs,
  diags,
  lint,
}: {
  compiled?: CompileResponse;
  loading: boolean;
  lastRunArgs: string[] | null;
  diags: Diagnostic[];
  lint: { run: () => void; pending: boolean; result: LintResult | null; error: string | null };
}) {
  const [mode, setMode] = useState<"argv" | "diff">("argv");
  const [copied, setCopied] = useState(false);
  const args = compiled?.args ?? null;
  const diff = useMemo(() => (args && lastRunArgs ? diffTokens(lastRunArgs, args) : null), [args, lastRunArgs]);
  const changed = diff?.some((d) => d.op !== "same");
  const unpinned = diags.filter((d) => !d.field);
  return (
    <div className="xl:sticky xl:top-4 xl:self-start">
      <Card
        title="QEMU command line"
        actions={
          <div className="flex items-center gap-1">
            {loading && <span className="text-xs text-zinc-400">compiling…</span>}
            {lastRunArgs && (
              <Button variant={mode === "diff" ? "default" : "ghost"} onClick={() => setMode(mode === "diff" ? "argv" : "diff")}>
                {changed ? "Changes since last start" : "No changes since last start"}
              </Button>
            )}
            {compiled?.command && (
              <Button
                variant="ghost"
                onClick={() => {
                  void navigator.clipboard.writeText(compiled.command!);
                  setCopied(true);
                  setTimeout(() => setCopied(false), 1500);
                }}
              >
                {copied ? "Copied" : "Copy"}
              </Button>
            )}
          </div>
        }
      >
        {!args ? (
          <div className="text-sm text-zinc-500">
            The configuration does not compile yet.
            <Diags list={diags.filter((d) => d.severity === "error")} />
          </div>
        ) : mode === "diff" && diff ? (
          <pre className="max-h-[70vh] overflow-auto text-xs leading-5 whitespace-pre-wrap">
            {diff.map((d, i) => (
              <div
                key={i}
                className={
                  d.op === "add"
                    ? "bg-emerald-100 text-emerald-900 dark:bg-emerald-950 dark:text-emerald-200"
                    : d.op === "del"
                      ? "bg-rose-100 text-rose-900 line-through dark:bg-rose-950 dark:text-rose-200"
                      : "text-zinc-500"
                }
              >
                {d.op === "add" ? "+ " : d.op === "del" ? "- " : "  "}
                {d.t}
              </div>
            ))}
          </pre>
        ) : (
          <pre className="max-h-[70vh] overflow-auto text-xs leading-5 whitespace-pre-wrap">
            {args.map((a, i) =>
              a.startsWith("-") ? (
                <div key={i} className="mt-1 font-semibold text-sky-700 dark:text-sky-400">
                  {a}
                </div>
              ) : (
                <div key={i} className="pl-4 break-all">
                  {a}
                </div>
              ),
            )}
          </pre>
        )}
        {unpinned.length > 0 && <Diags list={unpinned} />}
        <div className="mt-3 flex items-center gap-2 border-t border-zinc-200 pt-3 dark:border-zinc-800">
          <Button onClick={lint.run} disabled={!args || lint.pending} title="Start QEMU paused with scratch disks and no network ports, then quit">
            {lint.pending ? "Checking…" : "Check with QEMU"}
          </Button>
          {lint.result &&
            (lint.result.ok ? (
              <span className="text-xs text-emerald-700 dark:text-emerald-400">
                ✓ QEMU {lint.result.qemu_version} accepts this configuration ({lint.result.cached ? "cached" : `${lint.result.duration_ms} ms`})
              </span>
            ) : (
              <span className="text-xs text-rose-600">✕ QEMU rejected this configuration — see the messages above.</span>
            ))}
          {lint.error && <span className="text-xs text-rose-600">{lint.error}</span>}
        </div>
        <p className="mt-3 text-xs text-zinc-500">
          Exactly what will run; QEMU is started paused (<code>-S</code>), checked, then resumed.
        </p>
      </Card>
    </div>
  );
}

/** Host folders shared into the guest (M2-13). Linux hosts only. */
function SharesCard({
  shares,
  onChange,
  diagsFor,
  other,
}: {
  shares: ShareSpec[];
  onChange: (s: ShareSpec[]) => void;
  diagsFor: (i: number) => Diagnostic[];
  other: Diagnostic[];
}) {
  const info = useQuery({ queryKey: ["shares"], queryFn: api.shares });
  const roots = info.data?.roots ?? [];
  const update = (i: number, patch: Partial<ShareSpec>) => onChange(shares.map((s, j) => (j === i ? { ...s, ...patch } : s)));
  const supported = info.data?.supported ?? true;
  return (
    <Card
      title="Shared folders"
      actions={
        supported && (
          <Button
            variant="ghost"
            disabled={!roots.length}
            title={roots.length ? undefined : "Register a share root on the Storage page first"}
            onClick={() =>
              onChange([...shares, { root: roots[0]?.name ?? "", path: "", tag: `share${shares.length}`, readonly: false }])
            }
          >
            Add folder
          </Button>
        )
      }
    >
      {!supported ? (
        <p className="text-sm text-zinc-500">{info.data?.reason}</p>
      ) : shares.length === 0 ? (
        <p className="text-sm text-zinc-500">
          No shared folders.{" "}
          {roots.length ? "" : "Register a folder as a share root on the Storage page to share it with VMs."}
        </p>
      ) : (
        <div className="space-y-3">
          {shares.map((s, i) => (
            <div key={i} className="rounded border border-zinc-200 p-2 dark:border-zinc-800">
              <div className="grid grid-cols-2 gap-2 md:grid-cols-4">
                <Field label="Share root">
                  <select className={inputCls} value={s.root} onChange={(e) => update(i, { root: e.target.value })}>
                    {!roots.some((r) => r.name === s.root) && <option value={s.root}>{s.root || "— choose —"}</option>}
                    {roots.map((r) => (
                      <option key={r.name} value={r.name} title={r.path}>
                        {r.name}
                      </option>
                    ))}
                  </select>
                </Field>
                <Field label="Folder inside it">
                  <input className={`${inputCls} font-mono`} value={s.path} placeholder="(the whole root)" onChange={(e) => update(i, { path: e.target.value })} />
                </Field>
                <Field label="Mount tag">
                  <input className={`${inputCls} font-mono`} value={s.tag} onChange={(e) => update(i, { tag: e.target.value })} />
                </Field>
                <label className="flex items-end gap-2 pb-1 text-xs">
                  <input type="checkbox" checked={s.readonly} onChange={(e) => update(i, { readonly: e.target.checked })} />
                  Read-only
                </label>
              </div>
              <div className="mt-2 flex items-center gap-2">
                <code className="text-xs text-zinc-500">
                  {s.readonly || !info.data?.virtiofs
                    ? `mount -t 9p -o trans=virtio,version=9p2000.L ${s.tag} /mnt`
                    : `mount -t virtiofs ${s.tag} /mnt`}
                </code>
                <span className="ml-auto">
                  <Button variant="ghost" onClick={() => onChange(shares.filter((_, j) => j !== i))}>
                    Remove
                  </Button>
                </span>
              </div>
              <Diags list={diagsFor(i)} />
            </div>
          ))}
        </div>
      )}
      <Diags list={other} />
    </Card>
  );
}
