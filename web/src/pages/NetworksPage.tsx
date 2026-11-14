import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "../api/client";
import type { NetworkConfig } from "../api/generated/NetworkConfig";
import type { NetworkInfo } from "../api/generated/NetworkInfo";
import { Badge, Button, Card, Field, inputCls } from "../components/ui";

/** What each kind of network does, for the picker and the list. */
export const NETWORK_KINDS: { kind: NetworkConfig["kind"]; label: string; about: string }[] = [
  {
    kind: "nat",
    label: "NAT",
    about: "Outbound internet through the host. Each VM gets its own private NAT; reach guests through port forwards.",
  },
];

export function kindLabel(c: NetworkConfig): string {
  return NETWORK_KINDS.find((k) => k.kind === c.kind)?.label ?? c.kind;
}

/** One-line summary of a network's settings. */
export function networkSummary(c: NetworkConfig): string {
  switch (c.kind) {
    case "nat":
      return c.subnet ? `subnet ${c.subnet}` : "default subnet (10.0.2.0/24)";
  }
}

function blank(kind: NetworkConfig["kind"]): NetworkConfig {
  switch (kind) {
    case "nat":
      return { kind: "nat", subnet: null };
  }
}

/** Kind-specific settings. */
function ConfigFields({ config, onChange }: { config: NetworkConfig; onChange: (c: NetworkConfig) => void }) {
  switch (config.kind) {
    case "nat":
      return (
        <Field label="Subnet" hint="IPv4 CIDR; empty for QEMU's default">
          <input
            className={`${inputCls} w-40 font-mono`}
            placeholder="10.0.2.0/24"
            value={config.subnet ?? ""}
            onChange={(e) => onChange({ ...config, subnet: e.target.value.trim() || null })}
          />
        </Field>
      );
  }
}

function NetworkRow({ n }: { n: NetworkInfo }) {
  const qc = useQueryClient();
  const [editing, setEditing] = useState(false);
  const [name, setName] = useState(n.name);
  const [config, setConfig] = useState<NetworkConfig>(n.config);
  const done = () => qc.invalidateQueries({ queryKey: ["networks"] });
  const save = useMutation({
    mutationFn: () => api.updateNetwork(n.id, name, config),
    onSuccess: () => {
      setEditing(false);
      done();
    },
  });
  const remove = useMutation({ mutationFn: () => api.deleteNetwork(n.id), onSuccess: done });
  const err = save.error ?? remove.error;

  return (
    <li className="rounded border border-zinc-200 p-3 dark:border-zinc-800">
      {!editing ? (
        <div className="flex flex-wrap items-center gap-2">
          <span className="font-medium">{n.name}</span>
          <Badge>{kindLabel(n.config)}</Badge>
          <span className="text-xs text-zinc-500">{networkSummary(n.config)}</span>
          <span className="ml-auto flex items-center gap-2">
            <span className="text-xs text-zinc-500" title={n.used_by.join(", ")}>
              {n.used_by.length === 0 ? "not used" : `used by ${n.used_by.join(", ")}`}
            </span>
            <Button variant="ghost" onClick={() => setEditing(true)}>
              Edit
            </Button>
            <Button
              variant="ghost"
              disabled={remove.isPending || n.used_by.length > 0}
              title={n.used_by.length > 0 ? "Detach the VMs using it first" : undefined}
              onClick={() => remove.mutate()}
            >
              Delete
            </Button>
          </span>
        </div>
      ) : (
        <form
          className="flex flex-wrap items-end gap-2"
          onSubmit={(e) => {
            e.preventDefault();
            save.mutate();
          }}
        >
          <Field label="Name">
            <input className={inputCls} value={name} onChange={(e) => setName(e.target.value)} />
          </Field>
          <ConfigFields config={config} onChange={setConfig} />
          <Button type="submit" disabled={!name || save.isPending}>
            Save
          </Button>
          <Button
            variant="ghost"
            onClick={() => {
              setEditing(false);
              setName(n.name);
              setConfig(n.config);
            }}
          >
            Cancel
          </Button>
          {n.used_by.length > 0 && <p className="w-full text-xs text-zinc-500">Running VMs pick up changes at their next start.</p>}
        </form>
      )}
      {err && <p className="mt-1 text-xs text-rose-600">{err.message}</p>}
    </li>
  );
}

export default function NetworksPage() {
  const qc = useQueryClient();
  const nets = useQuery({ queryKey: ["networks"], queryFn: api.networks });
  const [name, setName] = useState("");
  const [config, setConfig] = useState<NetworkConfig>(blank("nat"));
  const create = useMutation({
    mutationFn: () => api.createNetwork(name, config),
    onSuccess: () => {
      setName("");
      setConfig(blank(config.kind));
      qc.invalidateQueries({ queryKey: ["networks"] });
    },
  });
  const about = NETWORK_KINDS.find((k) => k.kind === config.kind)?.about;

  return (
    <div className="max-w-4xl space-y-6">
      <h1 className="text-xl font-semibold">Networks</h1>
      <Card title="Networks">
        <p className="mb-3 text-xs text-zinc-500">
          Named networks that VM NICs attach to. A NIC can also use its own private NAT without a network object.
        </p>
        {nets.error && <p className="text-sm text-rose-600">{nets.error.message}</p>}
        {nets.data && nets.data.length === 0 && <p className="text-sm text-zinc-500">No networks yet.</p>}
        <ul className="space-y-2">
          {nets.data?.map((n) => (
            <NetworkRow key={n.id} n={n} />
          ))}
        </ul>
      </Card>
      <Card title="New network">
        <form
          className="space-y-2"
          onSubmit={(e) => {
            e.preventDefault();
            create.mutate();
          }}
        >
          <div className="flex flex-wrap items-end gap-2">
            <Field label="Name">
              <input className={inputCls} value={name} onChange={(e) => setName(e.target.value)} placeholder="lab" />
            </Field>
            <Field label="Kind">
              <select
                className={inputCls}
                value={config.kind}
                onChange={(e) => setConfig(blank(e.target.value as NetworkConfig["kind"]))}
              >
                {NETWORK_KINDS.map((k) => (
                  <option key={k.kind} value={k.kind}>
                    {k.label}
                  </option>
                ))}
              </select>
            </Field>
            <ConfigFields config={config} onChange={setConfig} />
            <Button type="submit" disabled={!name || create.isPending}>
              Create
            </Button>
          </div>
          {about && <p className="text-xs text-zinc-500">{about}</p>}
          {create.error && <p className="text-xs text-rose-600">{create.error.message}</p>}
        </form>
      </Card>
    </div>
  );
}
