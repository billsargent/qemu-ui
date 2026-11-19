import { useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { api } from "../api/client";
import type { DeviceNode } from "../api/generated/DeviceNode";
import type { DeviceSpec } from "../api/generated/DeviceSpec";
import type { Diagnostic } from "../api/generated/Diagnostic";
import type { HardwareTree as Tree } from "../api/generated/HardwareTree";
import type { VmSpec } from "../api/generated/VmSpec";
import { DevicePalette } from "./DevicePalette";
import { PropertyEditor } from "./PropertyEditor";
import { Badge, Button, Card, Field, inputCls } from "./ui";

/** Where a curated device is edited, for the "managed by" note. */
function fieldLabel(field: string | null): string {
  if (!field) return "the machine";
  const m = /^(\w+)(\[(\d+)\])?/.exec(field);
  const names: Record<string, string> = {
    disks: "Disks",
    nics: "Network",
    display: "Display",
    input: "Display and input",
    shares: "Shared folders",
    usb: "USB passthrough",
    firmware: "Firmware",
    passthrough: "Extra QEMU options",
  };
  const base = names[m?.[1] ?? ""] ?? field;
  return m?.[3] !== undefined ? `${base} #${Number(m[3]) + 1}` : base;
}

function DeviceRow({
  n,
  depth,
  children,
  selected,
  onSelect,
  hasError,
}: {
  n: DeviceNode;
  depth: number;
  children: DeviceNode[];
  selected: string | null;
  onSelect: (key: string) => void;
  hasError: boolean;
}) {
  const key = n.id || `${n.driver}@${depth}`;
  return (
    <button
      type="button"
      onClick={() => onSelect(key)}
      className={`flex w-full items-center gap-2 rounded px-1 py-0.5 text-left text-xs ${
        selected === key ? "bg-sky-100 dark:bg-sky-950" : "hover:bg-zinc-100 dark:hover:bg-zinc-900"
      }`}
      style={{ paddingLeft: `${depth * 16 + 4}px` }}
    >
      <span className="font-mono">{n.driver}</span>
      {n.id && <span className="text-zinc-500">{n.id}</span>}
      {n.generic !== null && <Badge tone="blue">added</Badge>}
      {n.passthrough && <Badge tone="amber">passthrough</Badge>}
      {hasError && <Badge tone="red">error</Badge>}
      {children.length > 0 && <span className="text-zinc-400">({children.length})</span>}
    </button>
  );
}

/** Buses → devices, nested by controller / root port. */
function TreeView({
  tree,
  selected,
  onSelect,
  errorFields,
}: {
  tree: Tree;
  selected: string | null;
  onSelect: (key: string) => void;
  errorFields: Set<string>;
}) {
  const childrenOf = (id: string) => tree.devices.filter((d) => d.parent === id);
  const render = (n: DeviceNode, depth: number): React.ReactNode[] => {
    const kids = n.id ? childrenOf(n.id) : [];
    return [
      <DeviceRow
        key={`${n.id}-${n.driver}-${depth}`}
        n={n}
        depth={depth}
        children={kids}
        selected={selected}
        onSelect={onSelect}
        hasError={!!n.field && errorFields.has(n.field)}
      />,
      ...kids.flatMap((k) => render(k, depth + 1)),
    ];
  };
  return (
    <div className="space-y-2">
      {tree.buses.map((b) => {
        const on = tree.devices.filter((d) => d.bus === b.name && !d.parent);
        if (on.length === 0) return null;
        return (
          <div key={b.name}>
            <div className="text-xs font-medium text-zinc-600 dark:text-zinc-400">
              <span className="font-mono">{b.name}</span>
              {b.bus_type && <span className="ml-1 text-zinc-400">{b.bus_type}</span>}
            </div>
            {on.flatMap((d) => render(d, 1))}
          </div>
        );
      })}
    </div>
  );
}

/** Hardware card: device tree, palette, and the property editor (M4-02/05). */
export function HardwareCard({
  spec,
  treeSpec,
  vmId,
  binaryId,
  onChange,
  diagsFor,
}: {
  spec: VmSpec;
  /** Debounced spec the tree is computed from. */
  treeSpec: VmSpec;
  vmId?: string;
  binaryId?: string;
  onChange: (devices: DeviceSpec[]) => void;
  diagsFor: (prefix: string) => Diagnostic[];
}) {
  const tree = useQuery({
    queryKey: ["hardware", vmId ?? "new", treeSpec],
    queryFn: () => api.hardwareTree(treeSpec, vmId),
    placeholderData: (prev) => prev,
    retry: false,
  });
  const [selected, setSelected] = useState<string | null>(null);
  const [adding, setAdding] = useState(false);
  const devices = spec.devices ?? [];

  const errorFields = useMemo(() => {
    const s = new Set<string>();
    for (const d of diagsFor("devices")) if (d.severity === "error") s.add(d.field.replace(/^(devices\[\d+\]).*/, "$1"));
    return s;
  }, [diagsFor]);

  const node = tree.data?.devices.find((d) => (d.id || "") === selected || `${d.driver}@` === selected);
  // A freshly added device is selected by index before the tree catches up.
  const genericIndex = node?.generic ?? (selected?.startsWith("new:") ? Number(selected.slice(4)) : null);
  const dev = genericIndex !== null && genericIndex !== undefined ? devices[genericIndex] : undefined;

  const update = (i: number, patch: Partial<DeviceSpec>) => onChange(devices.map((d, j) => (j === i ? { ...d, ...patch } : d)));
  const remove = (i: number) => {
    onChange(devices.filter((_, j) => j !== i));
    setSelected(null);
  };

  return (
    <Card
      title="Hardware"
      actions={
        <Button variant="ghost" onClick={() => setAdding(!adding)} disabled={!spec.machine.type}>
          {adding ? "Hide devices" : "Add device"}
        </Button>
      }
    >
      <p className="mb-2 text-xs text-zinc-500">
        Every device QEMU gets, by bus. Devices added here are yours to configure; the rest follow the settings above.
        {tree.data && !tree.data.probed && " The machine's buses could not be probed, so placement is approximate."}
      </p>
      {adding && (
        <div className="mb-3">
          <DevicePalette
            arch={spec.arch}
            machine={spec.machine.type}
            onClose={() => setAdding(false)}
            onAdd={(e) => {
              onChange([...devices, { driver: e.name, bus: null, props: {} }]);
              setSelected(`new:${devices.length}`);
              setAdding(false);
            }}
          />
        </div>
      )}
      <div className="grid grid-cols-1 gap-3 lg:grid-cols-[minmax(0,1fr)_minmax(0,1.2fr)]">
        <div className="max-h-[28rem] overflow-auto">
          {tree.data ? (
            <TreeView tree={tree.data} selected={selected} onSelect={setSelected} errorFields={errorFields} />
          ) : tree.error ? (
            <p className="text-xs text-zinc-500">The device tree appears once the configuration compiles.</p>
          ) : (
            <p className="text-xs text-zinc-500">Loading…</p>
          )}
        </div>
        <div className="min-w-0">
          {dev && genericIndex !== null && genericIndex !== undefined ? (
            <div className="space-y-2">
              <div className="flex items-center gap-2">
                <span className="font-mono text-sm font-medium">{dev.driver}</span>
                <span className="text-xs text-zinc-500">qui-xdev{genericIndex}</span>
                <span className="ml-auto">
                  <Button variant="ghost" onClick={() => remove(genericIndex)}>
                    Remove
                  </Button>
                </span>
              </div>
              <Field label="Bus" hint="empty: the machine's default for this device">
                <input
                  className={`${inputCls} w-48 font-mono`}
                  value={dev.bus ?? ""}
                  placeholder={node?.bus ?? node?.parent ?? "auto"}
                  onChange={(e) => update(genericIndex, { bus: e.target.value.trim() || null })}
                />
              </Field>
              {diagsFor(`devices[${genericIndex}]`).map((d, i) => (
                <p key={i} className={`text-xs ${d.severity === "error" ? "text-rose-600" : "text-amber-700"}`}>
                  {d.message} <span className="text-zinc-400">({d.rule})</span>
                </p>
              ))}
              {binaryId ? (
                <PropertyEditor
                  binaryId={binaryId}
                  driver={dev.driver}
                  values={dev.props as Record<string, string | number | boolean>}
                  onChange={(props) => update(genericIndex, { props })}
                />
              ) : (
                <p className="text-xs text-zinc-500">No QEMU binary for {spec.arch}.</p>
              )}
            </div>
          ) : node ? (
            <div className="space-y-1 text-xs">
              <div>
                <span className="font-mono text-sm font-medium">{node.driver}</span> <span className="text-zinc-500">{node.id}</span>
              </div>
              <p className="text-zinc-500">
                {node.passthrough
                  ? "Added through extra QEMU options; edit it there."
                  : `Managed by qemu-ui from ${fieldLabel(node.field)}; change it there.`}
              </p>
              <p className="text-zinc-500">
                On {node.parent ? <span className="font-mono">{node.parent}</span> : <span className="font-mono">{node.bus}</span>}
              </p>
            </div>
          ) : (
            <p className="text-xs text-zinc-500">Select a device to see or edit its properties.</p>
          )}
        </div>
      </div>
      {diagsFor("devices")
        .filter((d) => d.field === "devices")
        .map((d, i) => (
          <p key={i} className="text-xs text-rose-600">
            {d.message}
          </p>
        ))}
    </Card>
  );
}
