import { useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { api } from "../api/client";
import type { PaletteEntry } from "../api/generated/PaletteEntry";
import { Badge, Button, inputCls } from "./ui";

/** Devices that can be added to this machine, by category (M4-05). */
export function DevicePalette({
  arch,
  machine,
  onAdd,
  onClose,
}: {
  arch: string;
  machine: string;
  onAdd: (d: PaletteEntry) => void;
  onClose: () => void;
}) {
  const palette = useQuery({
    queryKey: ["device-palette", arch, machine],
    queryFn: () => api.devicePalette(arch, machine),
    enabled: !!machine,
    staleTime: Infinity,
  });
  const [q, setQ] = useState("");
  const [all, setAll] = useState(false);
  const groups = useMemo(() => {
    const needle = q.trim().toLowerCase();
    const m = new Map<string, PaletteEntry[]>();
    for (const e of palette.data ?? []) {
      if (!all && !e.available) continue;
      if (needle && !e.name.toLowerCase().includes(needle) && !e.description?.toLowerCase().includes(needle)) continue;
      m.set(e.category, [...(m.get(e.category) ?? []), e]);
    }
    return [...m.entries()];
  }, [palette.data, q, all]);
  const unavailable = (palette.data ?? []).filter((e) => !e.available).length;

  return (
    <div className="rounded border border-zinc-200 p-2 dark:border-zinc-800">
      <div className="mb-2 flex flex-wrap items-center gap-2">
        <input autoFocus className={`${inputCls} w-56`} placeholder="Search devices" value={q} onChange={(e) => setQ(e.target.value)} />
        {unavailable > 0 && (
          <label className="flex items-center gap-1 text-xs text-zinc-500">
            <input type="checkbox" checked={all} onChange={(e) => setAll(e.target.checked)} />
            show {unavailable} that don't fit this machine
          </label>
        )}
        <span className="ml-auto">
          <Button variant="ghost" onClick={onClose}>
            Close
          </Button>
        </span>
      </div>
      {palette.isLoading && <p className="text-xs text-zinc-500">Loading devices…</p>}
      {palette.error && <p className="text-xs text-rose-600">{palette.error.message}</p>}
      <div className="max-h-80 space-y-3 overflow-auto pr-1">
        {groups.map(([cat, entries]) => (
          <div key={cat}>
            <div className="mb-1 text-xs font-medium text-zinc-600 dark:text-zinc-400">{cat}</div>
            <ul className="space-y-0.5">
              {entries.map((e) => (
                <li key={e.name} className="flex items-start gap-2 text-xs">
                  <Button variant="ghost" disabled={!e.available} title={e.note ?? undefined} onClick={() => onAdd(e)}>
                    Add
                  </Button>
                  <div className="min-w-0">
                    <span className="font-mono">{e.name}</span>
                    {e.bus && (
                      <span className="ml-1">
                        <Badge>{e.bus}</Badge>
                      </span>
                    )}
                    {e.description && <span className="ml-1 text-zinc-500">{e.description}</span>}
                    {e.note && <div className={e.available ? "text-zinc-500" : "text-amber-700 dark:text-amber-400"}>{e.note}</div>}
                  </div>
                </li>
              ))}
            </ul>
          </div>
        ))}
        {palette.data && groups.length === 0 && <p className="text-xs text-zinc-500">No devices match.</p>}
      </div>
    </div>
  );
}
