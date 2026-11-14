// NIC model picker, MAC field and virtio-net options (M3-07).
import { useQuery } from "@tanstack/react-query";
import { api } from "../api/client";
import type { DeviceInfo } from "../api/generated/DeviceInfo";
import type { NicSpec } from "../api/generated/NicSpec";
import type { VirtioNetOptions } from "../api/generated/VirtioNetOptions";
import { Button, Field, inputCls } from "./ui";

/** NIC models for the machine, preferred first; unavailable ones disabled. */
export function NicModelField({
  arch,
  machine,
  value,
  fallback,
  onChange,
}: {
  arch: string;
  machine: string;
  value: string;
  /** Used until (or if) the machine-aware list is available. */
  fallback: DeviceInfo[];
  onChange: (model: string) => void;
}) {
  const models = useQuery({
    queryKey: ["nic-models", arch, machine],
    queryFn: () => api.nicModels(arch, machine),
    enabled: !!machine,
    staleTime: Infinity,
  });
  const list =
    models.data ??
    fallback.map((d) => ({ name: d.name, description: d.description, guest_note: null, preferred: false, available: true, reason: null }));
  const current = list.find((m) => m.name === value);
  const preferred = list.filter((m) => m.preferred);
  const others = list.filter((m) => !m.preferred);
  const option = (m: (typeof list)[number]) => (
    <option key={m.name} value={m.name} disabled={!m.available && m.name !== value} title={m.reason ?? m.description ?? undefined}>
      {m.name}
      {m.description && !m.preferred ? ` · ${m.description}` : ""}
      {m.available ? "" : " (not on this machine)"}
    </option>
  );
  return (
    <Field label="Model" hint={current?.reason ?? current?.guest_note ?? undefined}>
      <select className={inputCls} value={value} onChange={(e) => onChange(e.target.value)}>
        {!current && <option value={value}>{value}</option>}
        {preferred.length > 0 ? (
          <>
            <optgroup label="Recommended">{preferred.map(option)}</optgroup>
            {others.length > 0 && <optgroup label="Other models">{others.map(option)}</optgroup>}
          </>
        ) : (
          others.map(option)
        )}
      </select>
    </Field>
  );
}

/** MAC with a "new" button: clearing it gets a registry-unique one on save. */
export function MacField({ value, onChange }: { value: string | null; onChange: (mac: string | null) => void }) {
  return (
    <Field label="MAC" hint={value ? undefined : "a new unique address is assigned on save"}>
      <div className="flex gap-1">
        <input className={`${inputCls} font-mono`} value={value ?? ""} onChange={(e) => onChange(e.target.value || null)} />
        <Button variant="ghost" onClick={() => onChange(null)} title="Assign a new unique MAC on save">
          New
        </Button>
      </div>
    </Field>
  );
}

type Tri = boolean | undefined;
const triValue = (v: Tri) => (v === undefined ? "" : v ? "on" : "off");
const fromTri = (s: string): Tri => (s === "" ? undefined : s === "on");

/** virtio-net offloads and queues; only shown for virtio-net models. */
export function VirtioNetFields({ nic, onChange }: { nic: NicSpec; onChange: (patch: Partial<NicSpec>) => void }) {
  if (!nic.model.startsWith("virtio-net")) return null;
  const v: VirtioNetOptions = nic.virtio ?? {};
  const setOpt = (k: keyof VirtioNetOptions, val: Tri) => {
    const next: VirtioNetOptions = { ...v };
    if (val === undefined) delete next[k];
    else next[k] = val;
    onChange({ virtio: Object.keys(next).length ? next : undefined });
  };
  const tri = (k: keyof VirtioNetOptions, label: string, hint: string) => (
    <Field label={label} hint={hint}>
      <select className={inputCls} value={triValue(v[k])} onChange={(e) => setOpt(k, fromTri(e.target.value))}>
        <option value="">default (on)</option>
        <option value="on">on</option>
        <option value="off">off</option>
      </select>
    </Field>
  );
  return (
    <details className="mt-2 text-xs">
      <summary className="cursor-pointer text-zinc-600 dark:text-zinc-400">virtio-net options</summary>
      <div className="mt-2 grid grid-cols-4 gap-2">
        {tri("csum", "Checksum offload", "turn off to debug odd packet loss")}
        {tri("tso", "Segmentation offload", "needs checksum offload")}
        {tri("mrg_rxbuf", "Mergeable rx buffers", "")}
        <Field label="Queues" hint="multiqueue needs a bridged or tap network">
          <input
            className={inputCls}
            type="number"
            min={1}
            value={nic.queues ?? 1}
            onChange={(e) => {
              const q = Number(e.target.value);
              onChange({ queues: q > 1 ? q : undefined });
            }}
          />
        </Field>
      </div>
    </details>
  );
}
