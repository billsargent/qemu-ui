import { useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { api } from "../api/client";
import type { PropField } from "../api/generated/PropField";
import type { Widget } from "../api/generated/Widget";
import { inputCls } from "./ui";

type PropValue = string | number | boolean;

/** Parse a size like 64M / 1.5G / 4096 into bytes (mirrors core `parse_size`). */
export function parseSize(s: string): number | null {
  const m = /^\s*([0-9]*\.?[0-9]+)\s*([kKmMgGtTbB]?)\s*$/.exec(s);
  if (!m) return null;
  const mult = { "": 1, b: 1, k: 1024, m: 1024 ** 2, g: 1024 ** 3, t: 1024 ** 4 }[m[2].toLowerCase()] ?? 1;
  return Math.round(Number(m[1]) * mult);
}

/** Editor input → the typed value QEMU expects (mirrors core `encode_prop`). */
export function encodeProp(w: Widget, input: string): { value?: PropValue; error?: string } {
  switch (w.kind) {
    case "toggle":
      return input === "true" ? { value: true } : input === "false" ? { value: false } : { error: "expected on or off" };
    case "integer": {
      if (!/^-?\d+$/.test(input.trim())) return { error: "expected a whole number" };
      const v = Number(input);
      if (v < w.min || v > w.max) return { error: `must be between ${w.min} and ${w.max}` };
      return { value: v };
    }
    case "size": {
      const v = parseSize(input);
      return v === null ? { error: "expected a size like 64M" } : { value: v };
    }
    case "select":
      return w.options.includes(input) ? { value: input } : { error: `expected one of ${w.options.join(", ")}` };
    case "text":
      if (w.pattern && !new RegExp(w.pattern).test(input)) return { error: `expected ${w.hint ?? "a different format"}` };
      return { value: input };
    case "hidden":
      return { error: `not editable: ${w.reason}` };
  }
}

function show(v: unknown): string {
  if (v === null || v === undefined) return "";
  if (typeof v === "object") return JSON.stringify(v);
  return String(v);
}

/** One property row: the widget, its default, and local validation. */
function PropRow({
  f,
  value,
  onChange,
}: {
  f: PropField;
  value: PropValue | undefined;
  onChange: (v: PropValue | undefined) => void;
}) {
  const [draft, setDraft] = useState<string>(value === undefined ? "" : String(value));
  const [error, setError] = useState<string | null>(null);
  const w = f.widget;
  const commit = (s: string) => {
    setDraft(s);
    if (s === "") {
      setError(null);
      onChange(undefined); // back to QEMU's default
      return;
    }
    const r = encodeProp(w, s);
    setError(r.error ?? null);
    if (r.value !== undefined) onChange(r.value);
  };
  const changed = value !== undefined;
  const def = show(f.default_value);

  let input;
  switch (w.kind) {
    case "toggle":
      input = (
        <select className={inputCls} value={value === undefined ? "" : String(value)} onChange={(e) => commit(e.target.value)}>
          <option value="">default{def ? ` (${def === "true" ? "on" : def === "false" ? "off" : def})` : ""}</option>
          <option value="true">on</option>
          <option value="false">off</option>
        </select>
      );
      break;
    case "select":
      input = (
        <select className={inputCls} value={value === undefined ? "" : String(value)} onChange={(e) => commit(e.target.value)}>
          <option value="">default{def ? ` (${def})` : ""}</option>
          {w.options.map((o) => (
            <option key={o}>{o}</option>
          ))}
        </select>
      );
      break;
    default:
      input = (
        <input
          className={`${inputCls} ${w.kind === "text" ? "" : "font-mono"}`}
          value={draft}
          placeholder={def || (w.kind === "size" ? "e.g. 64M" : w.kind === "text" ? (w.hint ?? "") : "")}
          inputMode={w.kind === "integer" ? "numeric" : undefined}
          onChange={(e) => commit(e.target.value)}
        />
      );
  }

  return (
    <div className="grid grid-cols-[minmax(0,11rem)_minmax(0,1fr)] items-start gap-2 py-1">
      <div className="min-w-0">
        <div className={`truncate font-mono text-xs ${changed ? "font-semibold text-sky-700 dark:text-sky-400" : ""}`} title={f.name}>
          {f.name}
        </div>
        <div className="truncate text-[10px] text-zinc-500" title={f.type_name}>
          {f.type_name}
        </div>
      </div>
      <div className="min-w-0">
        {input}
        {f.description && <div className="mt-0.5 text-[11px] text-zinc-500">{f.description}</div>}
        {error && <div className="mt-0.5 text-[11px] text-rose-600">{error}</div>}
      </div>
    </div>
  );
}

/**
 * Properties of a device type, generated from QEMU's introspection (M4-02).
 * Only properties the user changed end up in `values`.
 */
export function PropertyEditor({
  binaryId,
  driver,
  values,
  onChange,
}: {
  binaryId: string;
  driver: string;
  values: Record<string, PropValue>;
  onChange: (values: Record<string, PropValue>) => void;
}) {
  const fields = useQuery({
    queryKey: ["device-fields", binaryId, driver],
    queryFn: () => api.deviceFields(binaryId, driver),
    staleTime: Infinity,
  });
  const [filter, setFilter] = useState("");
  const [showHidden, setShowHidden] = useState(false);
  const list = useMemo(() => {
    const q = filter.trim().toLowerCase();
    return (fields.data ?? []).filter(
      (f) => (showHidden || f.widget.kind !== "hidden") && (!q || f.name.toLowerCase().includes(q) || f.description?.toLowerCase().includes(q)),
    );
  }, [fields.data, filter, showHidden]);
  const hiddenCount = (fields.data ?? []).filter((f) => f.widget.kind === "hidden").length;

  if (fields.isLoading) return <p className="text-xs text-zinc-500">Asking QEMU for {driver}'s properties…</p>;
  if (fields.error) return <p className="text-xs text-rose-600">{fields.error.message}</p>;
  const set = (name: string, v: PropValue | undefined) => {
    const next = { ...values };
    if (v === undefined) delete next[name];
    else next[name] = v;
    onChange(next);
  };

  return (
    <div>
      <div className="mb-2 flex flex-wrap items-center gap-2">
        <input className={`${inputCls} w-48`} placeholder="Filter properties" value={filter} onChange={(e) => setFilter(e.target.value)} />
        <span className="text-xs text-zinc-500">
          {Object.keys(values).length} changed · {(fields.data ?? []).length} total
        </span>
        {hiddenCount > 0 && (
          <label className="ml-auto flex items-center gap-1 text-xs text-zinc-500">
            <input type="checkbox" checked={showHidden} onChange={(e) => setShowHidden(e.target.checked)} />
            show {hiddenCount} managed
          </label>
        )}
      </div>
      <div className="max-h-96 divide-y divide-zinc-100 overflow-auto pr-1 dark:divide-zinc-800">
        {list.map((f) =>
          f.widget.kind === "hidden" ? (
            <div key={f.name} className="py-1 text-xs text-zinc-500">
              <span className="font-mono">{f.name}</span> — {f.widget.reason}
            </div>
          ) : (
            <PropRow key={f.name} f={f} value={values[f.name]} onChange={(v) => set(f.name, v)} />
          ),
        )}
        {list.length === 0 && <p className="py-2 text-xs text-zinc-500">No properties match.</p>}
      </div>
    </div>
  );
}
