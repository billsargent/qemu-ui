import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "../api/client";
import type { NetworkConfig } from "../api/generated/NetworkConfig";
import type { NetworkInfo } from "../api/generated/NetworkInfo";
import type { HostNetInfo } from "../api/generated/HostNetInfo";
import { Badge, Button, Card, Field, inputCls } from "../components/ui";

/** What each kind of network does, for the picker and the list. */
export const NETWORK_KINDS: {
  kind: NetworkConfig["kind"];
  label: string;
  about: string;
  linuxOnly?: string;
  windowsOnly?: string;
}[] = [
  {
    kind: "nat",
    label: "NAT",
    about: "Outbound internet through the host. Each VM gets its own private NAT; reach guests through port forwards.",
  },
  {
    kind: "passt",
    label: "NAT (passt)",
    about:
      "Faster NAT through a passt helper per NIC; the guest sees the host's own address. Port forwards are set when the VM starts. Needs the passt package.",
    linuxOnly: "passt only runs on Linux hosts; use NAT instead.",
  },
  {
    kind: "bridged",
    label: "Bridged",
    about:
      "Linux: guests join an existing host bridge and the LAN behind it, reachable directly. Needs the qemu-ui helper with the bridge allowlisted in helper.toml.",
    linuxOnly: "Bridged networks need a Linux host; on Windows use a TAP network.",
  },
  {
    kind: "host-only",
    label: "Host-only",
    about:
      "Linux: guests share a private bridge with the host and each other, with no outside access. Needs the qemu-ui helper with [network.host_only] in helper.toml.",
    linuxOnly: "Host-only networks need a Linux host; on Windows use an unbridged TAP network.",
  },
  {
    kind: "tap",
    label: "TAP",
    about:
      "Windows: guests use a TAP-Windows adapter. Bridge it to your physical adapter in Network Connections (admin) to put guests on the LAN.",
    windowsOnly: "TAP networks need a Windows host; on Linux use a bridged or host-only network.",
  },
  {
    kind: "isolated",
    label: "Isolated",
    about:
      "A private switch: VMs on this network reach each other and nothing else — no host, no internet, no port forwards. Give guests static addresses or run a DHCP server in one of them.",
  },
];

/** Which host OS a kind runs on (null: any). */
export function kindOs(kind: NetworkConfig["kind"]): string | null {
  const k = NETWORK_KINDS.find((x) => x.kind === kind);
  return k?.linuxOnly ? "linux" : k?.windowsOnly ? "windows" : null;
}

export function kindLabel(c: NetworkConfig): string {
  return NETWORK_KINDS.find((k) => k.kind === c.kind)?.label ?? c.kind;
}

/** One-line summary of a network's settings. */
export function networkSummary(c: NetworkConfig): string {
  switch (c.kind) {
    case "nat":
      return c.subnet ? `subnet ${c.subnet}` : "default subnet (10.0.2.0/24)";
    case "passt":
      return c.mtu ? `MTU ${c.mtu}` : "default MTU";
    case "bridged":
      return `bridge ${c.bridge}`;
    case "host-only":
      return "helper-managed bridge, no uplink";
    case "tap":
      return `adapter ${c.adapter}`;
    case "isolated":
      return "VMs only; no host or internet access";
  }
}

function blank(kind: NetworkConfig["kind"]): NetworkConfig {
  switch (kind) {
    case "nat":
      return { kind: "nat", subnet: null };
    case "passt":
      return { kind: "passt", mtu: null };
    case "bridged":
      return { kind: "bridged", bridge: "" };
    case "host-only":
      return { kind: "host-only" };
    case "tap":
      return { kind: "tap", adapter: "" };
    case "isolated":
      return { kind: "isolated" };
  }
}

/** Why this kind cannot be used on this host, if it cannot. */
function hostProblem(kind: NetworkConfig["kind"], host?: HostNetInfo): string | null {
  const os = kindOs(kind);
  if (!host || !os) return null;
  const label = NETWORK_KINDS.find((k) => k.kind === kind)?.label ?? kind;
  if (host.os !== os) return `${label} networks need a ${os === "linux" ? "Linux" : "Windows"} host.`;
  if (kind === "tap") return host.adapters.some((a) => a.is_tap) ? null : host.reason;
  if (!host.helper) return `The qemu-ui helper is not available: ${host.reason ?? "install and start it"}`;
  if (kind === "host-only" && !host.host_only) return "Host-only networking is disabled in the helper's helper.toml ([network.host_only]).";
  if (kind === "bridged" && host.bridges.length === 0) return "The helper allows no bridges; add them to [network] bridges in helper.toml.";
  return null;
}

/** Kind-specific settings. */
function ConfigFields({
  config,
  onChange,
  host,
}: {
  config: NetworkConfig;
  onChange: (c: NetworkConfig) => void;
  host?: HostNetInfo;
}) {
  switch (config.kind) {
    case "bridged":
      return (
        <Field label="Bridge" hint="allowlisted in helper.toml">
          <input
            className={`${inputCls} w-40 font-mono`}
            list="qui-bridges"
            placeholder="br0"
            value={config.bridge}
            onChange={(e) => onChange({ ...config, bridge: e.target.value.trim() })}
          />
          <datalist id="qui-bridges">
            {(host?.bridges ?? []).map((b) => (
              <option key={b} value={b} />
            ))}
          </datalist>
        </Field>
      );
    case "host-only":
      return null;
    case "tap": {
      const taps = (host?.adapters ?? []).filter((a) => a.is_tap);
      return (
        <Field label="Adapter" hint="a TAP-Windows adapter">
          {taps.length > 0 ? (
            <select className={inputCls} value={config.adapter} onChange={(e) => onChange({ ...config, adapter: e.target.value })}>
              <option value="">choose…</option>
              {taps.map((a) => (
                <option key={a.name} value={a.name}>
                  {a.name} ({a.status})
                </option>
              ))}
              {config.adapter && !taps.some((a) => a.name === config.adapter) && (
                <option value={config.adapter}>{config.adapter} (not found)</option>
              )}
            </select>
          ) : (
            <input className={inputCls} value={config.adapter} onChange={(e) => onChange({ ...config, adapter: e.target.value })} />
          )}
        </Field>
      );
    }
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
    case "passt":
      return (
        <Field label="MTU" hint="empty for passt's default">
          <input
            className={`${inputCls} w-28`}
            type="number"
            placeholder="65520"
            value={config.mtu ?? ""}
            onChange={(e) => onChange({ ...config, mtu: e.target.value ? Number(e.target.value) : null })}
          />
        </Field>
      );
    case "isolated":
      return null;
  }
}

function NetworkRow({ n }: { n: NetworkInfo }) {
  const qc = useQueryClient();
  const [editing, setEditing] = useState(false);
  const [name, setName] = useState(n.name);
  const [config, setConfig] = useState<NetworkConfig>(n.config);
  const host = useQuery({ queryKey: ["host-net"], queryFn: api.hostNet, staleTime: 30_000 });
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
          <ConfigFields config={config} onChange={setConfig} host={host.data} />
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
  const caps = useQuery({ queryKey: ["capabilities"], queryFn: api.capabilities });
  const hostOs = caps.data?.caps?.host.os;
  const unavailable = (k: (typeof NETWORK_KINDS)[number]) =>
    k.linuxOnly && hostOs && hostOs !== "linux"
      ? k.linuxOnly
      : k.windowsOnly && hostOs && hostOs !== "windows"
        ? k.windowsOnly
        : null;
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
  const host = useQuery({ queryKey: ["host-net"], queryFn: api.hostNet, staleTime: 30_000 });
  const problem = hostProblem(config.kind, host.data);

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
                  <option key={k.kind} value={k.kind} disabled={!!unavailable(k)} title={unavailable(k) ?? undefined}>
                    {k.label}
                    {unavailable(k) ? (k.windowsOnly ? " (Windows only)" : " (Linux only)") : ""}
                  </option>
                ))}
              </select>
            </Field>
            <ConfigFields config={config} onChange={setConfig} host={host.data} />
            <Button type="submit" disabled={!name || create.isPending}>
              Create
            </Button>
          </div>
          {about && <p className="text-xs text-zinc-500">{about}</p>}
          {problem && (
            <p className="text-xs text-amber-700 dark:text-amber-400">
              <Badge tone="amber">needs setup</Badge> {problem}
            </p>
          )}
          {create.error && <p className="text-xs text-rose-600">{create.error.message}</p>}
        </form>
      </Card>
    </div>
  );
}
