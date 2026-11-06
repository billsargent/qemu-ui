import { useEffect, useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { ApiError, api } from "../api/client";
import type { BinaryCaps } from "../api/generated/BinaryCaps";
import type { CacheMode } from "../api/generated/CacheMode";
import type { CompileResponse } from "../api/generated/CompileResponse";
import type { Diagnostic } from "../api/generated/Diagnostic";
import type { DiskBus } from "../api/generated/DiskBus";
import type { DiskSpec } from "../api/generated/DiskSpec";
import type { Firmware } from "../api/generated/Firmware";
import type { Media } from "../api/generated/Media";
import type { NicSpec } from "../api/generated/NicSpec";
import type { VmSpec } from "../api/generated/VmSpec";
import { Badge, Button, Card, Field, inputCls } from "./ui";

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
  const pool = useQuery({ queryKey: ["pool", "default"], queryFn: () => api.pool() });
  const bin: BinaryCaps | undefined = caps.data?.caps?.binaries.find((b) => b.arch === spec.arch);

  // Live argv (M1-14): compile on every change, debounced.
  const debounced = useDebounced(spec, 300);
  const compiled = useQuery<CompileResponse>({
    queryKey: ["compile", vmId ?? "new", debounced],
    queryFn: () => api.compile(debounced, vmId),
    placeholderData: (prev) => prev,
  });
  const diags = useMemo(
    () => [...(compiled.data?.diagnostics ?? []), ...(saveError?.diagnostics ?? [])],
    [compiled.data, saveError],
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
  const images = pool.data?.images ?? [];

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
                      image: { pool: "default", name: images[0]?.name ?? "" },
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
                    <select className={inputCls} value={d.image.name} onChange={(e) => updateDisk(i, { image: { pool: "default", name: e.target.value } })}>
                      <option value="">— choose —</option>
                      {images.map((img) => (
                        <option key={img.name} value={img.name}>
                          {img.name} ({img.format})
                        </option>
                      ))}
                    </select>
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
                  <Field label="Bus">
                    <select className={inputCls} value={d.bus} onChange={(e) => updateDisk(i, { bus: e.target.value as DiskBus })}>
                      {(Object.keys(BUS_LABEL) as DiskBus[]).map((b) => (
                        <option key={b} value={b}>
                          {BUS_LABEL[b]}
                        </option>
                      ))}
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
                  <Field label="Cache">
                    <select className={inputCls} value={d.cache} onChange={(e) => updateDisk(i, { cache: e.target.value as CacheMode })}>
                      {(Object.keys(CACHE_LABEL) as CacheMode[]).map((c) => (
                        <option key={c} value={c}>
                          {CACHE_LABEL[c]}
                        </option>
                      ))}
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
                <Field label="Model">
                  <select className={inputCls} value={n.model} onChange={(e) => updateNic(i, { model: e.target.value })}>
                    {nicModels.map((d) => (
                      <option key={d.name} value={d.name}>
                        {d.name}
                      </option>
                    ))}
                  </select>
                </Field>
                <Field label="Backend" hint="NAT (slirp). More network types arrive in M3.">
                  <input className={inputCls} value="NAT" disabled />
                </Field>
                <Field label="MAC" hint={n.mac ? undefined : "assigned on save"}>
                  <input className={`${inputCls} font-mono`} value={n.mac ?? ""} onChange={(e) => updateNic(i, { mac: e.target.value || null })} />
                </Field>
              </div>
              <div className="mt-2 text-xs">
                <div className="mb-1 font-medium text-zinc-600 dark:text-zinc-400">Port forwards</div>
                {n.backend.forwards.map((f, j) => (
                  <div key={j} className="mb-1 flex items-center gap-1">
                    <select
                      className={inputCls}
                      value={f.proto}
                      onChange={(e) =>
                        updateNic(i, { backend: { kind: "user", forwards: n.backend.forwards.map((x, k) => (k === j ? { ...x, proto: e.target.value } : x)) } })
                      }
                    >
                      <option>tcp</option>
                      <option>udp</option>
                    </select>
                    <input
                      className={`${inputCls} w-28 font-mono`}
                      value={f.host_addr}
                      onChange={(e) =>
                        updateNic(i, { backend: { kind: "user", forwards: n.backend.forwards.map((x, k) => (k === j ? { ...x, host_addr: e.target.value } : x)) } })
                      }
                    />
                    :
                    <input
                      className={`${inputCls} w-20`}
                      type="number"
                      value={f.host_port}
                      onChange={(e) =>
                        updateNic(i, { backend: { kind: "user", forwards: n.backend.forwards.map((x, k) => (k === j ? { ...x, host_port: Number(e.target.value) } : x)) } })
                      }
                    />
                    → guest
                    <input
                      className={`${inputCls} w-20`}
                      type="number"
                      value={f.guest_port}
                      onChange={(e) =>
                        updateNic(i, { backend: { kind: "user", forwards: n.backend.forwards.map((x, k) => (k === j ? { ...x, guest_port: Number(e.target.value) } : x)) } })
                      }
                    />
                    <Button variant="ghost" onClick={() => updateNic(i, { backend: { kind: "user", forwards: n.backend.forwards.filter((_, k) => k !== j) } })}>
                      ✕
                    </Button>
                  </div>
                ))}
                <Button
                  variant="ghost"
                  onClick={() =>
                    updateNic(i, { backend: { kind: "user", forwards: [...n.backend.forwards, { proto: "tcp", host_addr: "127.0.0.1", host_port: 2222, guest_port: 22 }] } })
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

      <ArgvDrawer compiled={compiled.data} loading={compiled.isFetching} lastRunArgs={lastRunArgs ?? null} diags={diags} />
    </div>
  );
}

function ArgvDrawer({
  compiled,
  loading,
  lastRunArgs,
  diags,
}: {
  compiled?: CompileResponse;
  loading: boolean;
  lastRunArgs: string[] | null;
  diags: Diagnostic[];
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
        <p className="mt-3 text-xs text-zinc-500">
          Exactly what will run; QEMU is started paused (<code>-S</code>), checked, then resumed.
        </p>
      </Card>
    </div>
  );
}
