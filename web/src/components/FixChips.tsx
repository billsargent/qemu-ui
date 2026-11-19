// Auto-fix for curated rules (M4-04): "Fix" buttons on diagnostics whose
// rule has a fix, and visible "changed" chips for what a fix touched.
import { createContext, useContext, useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { api } from "../api/client";
import type { VmSpec } from "../api/generated/VmSpec";

type Fix = { description: string };

type FixState = {
  /** The fix offered for a diagnostic's rule id, if any. */
  fixFor: (rule: string) => Fix | undefined;
  apply: (rule: string) => void;
  pending: string | null;
  error: string | null;
};

const Ctx = createContext<FixState | null>(null);

/** A field a fix changed, with the fix that changed it. */
export type Changed = { path: string; by: string };

/**
 * Fix state for an editor: call `provide(...)` around the editor's content,
 * `changed`/`clear` drive the chips. `setSpec` receives the fixed spec.
 */
export function useFixes(spec: VmSpec, setSpec: (s: VmSpec) => void) {
  const rules = useQuery({ queryKey: ["rules"], queryFn: api.rules, staleTime: Infinity });
  const [changed, setChanged] = useState<Changed[]>([]);
  const [pending, setPending] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const fixes = useMemo(() => {
    const m = new Map<string, Fix>();
    for (const r of rules.data ?? []) {
      if (!r.autofix) continue;
      m.set(r.id, { description: r.autofix });
      for (const c of r.covers) m.set(c, { description: r.autofix });
    }
    return m;
  }, [rules.data]);

  const apply = async (rule: string) => {
    setPending(rule);
    setError(null);
    try {
      const res = await api.fixSpec(spec, rule);
      setSpec(res.spec);
      setChanged((prev) => [
        ...prev.filter((c) => !res.changed.includes(c.path)),
        ...res.changed.map((path) => ({ path, by: res.description })),
      ]);
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setPending(null);
    }
  };

  const state: FixState = { fixFor: (r) => fixes.get(r), apply: (r) => void apply(r), pending, error };
  return {
    changed,
    clear: () => {
      setChanged([]);
      setError(null);
    },
    provide: (children: React.ReactNode) => <Ctx.Provider value={state}>{children}</Ctx.Provider>,
  };
}

/** "Fix" button for a diagnostic, when its rule has an automatic fix. */
export function FixButton({ rule }: { rule: string }) {
  const ctx = useContext(Ctx);
  const fix = ctx?.fixFor(rule);
  if (!ctx || !fix) return null;
  return (
    <button
      type="button"
      className="ml-1 rounded border border-current px-1.5 text-[11px] font-medium hover:bg-amber-50 disabled:opacity-50 dark:hover:bg-amber-950"
      disabled={ctx.pending !== null}
      title={fix.description}
      onClick={() => ctx.apply(rule)}
    >
      {ctx.pending === rule ? "Fixing…" : `Fix: ${fix.description}`}
    </button>
  );
}

/** The last fix error, if any. */
export function FixError() {
  const ctx = useContext(Ctx);
  return ctx?.error ? <p className="text-xs text-rose-600">{ctx.error}</p> : null;
}

/** Chips naming every field a fix changed; dismissed by editing or saving. */
export function ChangedChips({ changed, onDismiss }: { changed: Changed[]; onDismiss: () => void }) {
  if (!changed.length) return null;
  return (
    <div className="flex flex-wrap items-center gap-1.5 rounded border border-sky-300 bg-sky-50 px-3 py-2 text-xs dark:border-sky-800 dark:bg-sky-950">
      <span className="font-medium text-sky-800 dark:text-sky-300">Changed by fixes (not saved yet):</span>
      {changed.map((c) => (
        <span
          key={c.path}
          title={c.by}
          className="rounded-full bg-sky-200 px-2 py-0.5 font-mono text-sky-900 dark:bg-sky-800 dark:text-sky-100"
        >
          changed · {c.path}
        </span>
      ))}
      <button type="button" className="ml-auto text-sky-700 underline dark:text-sky-300" onClick={onDismiss}>
        dismiss
      </button>
    </div>
  );
}

/** A small "changed" chip next to a field, if a fix touched it. */
export function ChangedChip({ changed, field }: { changed: Changed[]; field: string }) {
  const hit = changed.find((c) => c.path === field || c.path.startsWith(`${field}.`) || c.path.startsWith(`${field}[`));
  if (!hit) return null;
  return (
    <span title={hit.by} className="ml-1 rounded-full bg-sky-200 px-1.5 text-[10px] font-medium text-sky-900 dark:bg-sky-800 dark:text-sky-100">
      changed
    </span>
  );
}
