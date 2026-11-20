import { useState } from "react";
import { useInfiniteQuery } from "@tanstack/react-query";
import { api } from "../api/client";
import type { AuditEntry } from "../api/generated/AuditEntry";
import type { AuditQuery } from "../api/generated/AuditQuery";
import { Badge, Button, Card, Field, inputCls } from "../components/ui";

/** `YYYY-MM-DDTHH:mm` from a datetime-local input → unix seconds. */
function toSecs(v: string): number | null {
  if (!v) return null;
  const t = new Date(v).getTime();
  return Number.isNaN(t) ? null : Math.floor(t / 1000);
}

function outcomeTone(o: string): "green" | "red" | "amber" | "zinc" {
  if (o === "ok") return "green";
  if (o === "denied") return "red";
  if (o.startsWith("error")) return "amber";
  return "zinc";
}

function Row({ e }: { e: AuditEntry }) {
  const [open, setOpen] = useState(false);
  const details = !!e.details && typeof e.details === "object" && Object.keys(e.details).length > 0;
  return (
    <>
      <tr className="border-t border-zinc-100 align-top dark:border-zinc-800">
        <td className="whitespace-nowrap py-1 pr-3 text-xs text-zinc-500">{new Date(e.time * 1000).toLocaleString()}</td>
        <td className="py-1 pr-3 text-xs">{e.actor}</td>
        <td className="py-1 pr-3 font-mono text-xs">{e.action}</td>
        <td className="max-w-xs truncate py-1 pr-3 font-mono text-xs" title={e.target}>
          {e.target}
        </td>
        <td className="py-1 pr-3">
          <Badge tone={outcomeTone(e.outcome)}>{e.outcome}</Badge>
        </td>
        <td className="py-1 pr-3 font-mono text-xs text-zinc-500">{e.ip ?? ""}</td>
        <td className="py-1 text-xs">
          {details && (
            <button className="text-zinc-500 underline" onClick={() => setOpen(!open)}>
              {open ? "hide" : "details"}
            </button>
          )}
        </td>
      </tr>
      {open && (
        <tr>
          <td colSpan={7} className="pb-2">
            <pre className="overflow-x-auto rounded bg-zinc-100 p-2 text-xs dark:bg-zinc-900">{JSON.stringify(e.details, null, 2)}</pre>
          </td>
        </tr>
      )}
    </>
  );
}

export default function AuditPage() {
  const [draft, setDraft] = useState({ actor: "", action: "", target: "", since: "", until: "" });
  const [filters, setFilters] = useState<AuditQuery>({
    actor: null,
    action: null,
    target: null,
    since: null,
    until: null,
    before: null,
    limit: null,
  });
  const q = useInfiniteQuery({
    queryKey: ["audit", filters],
    queryFn: ({ pageParam }) => api.audit({ ...filters, before: pageParam, limit: 100 }),
    initialPageParam: null as number | null,
    getNextPageParam: (last) => last.next_before,
  });
  const entries = q.data?.pages.flatMap((p) => p.entries) ?? [];
  const retention = q.data?.pages[0]?.retention;

  const apply = () =>
    setFilters({
      actor: draft.actor.trim() || null,
      action: draft.action.trim() || null,
      target: draft.target.trim() || null,
      since: toSecs(draft.since),
      until: toSecs(draft.until),
      before: null,
      limit: null,
    });

  return (
    <div className="max-w-6xl space-y-5">
      <div className="flex flex-wrap items-center gap-3">
        <h1 className="text-xl font-semibold">Audit log</h1>
        {retention && (
          <span className="text-sm text-zinc-500">
            kept for {retention.max_age_days} days, up to {retention.max_entries.toLocaleString()} entries
          </span>
        )}
        <div className="ml-auto flex gap-2">
          <a className="rounded px-2.5 py-1 text-xs font-medium hover:bg-zinc-100 dark:hover:bg-zinc-800" href={api.auditExportUrl(filters, "csv")}>
            Export CSV
          </a>
          <a className="rounded px-2.5 py-1 text-xs font-medium hover:bg-zinc-100 dark:hover:bg-zinc-800" href={api.auditExportUrl(filters, "json")}>
            Export JSON
          </a>
        </div>
      </div>
      <Card title="Filters">
        <form
          className="flex flex-wrap items-end gap-2"
          onSubmit={(e) => {
            e.preventDefault();
            apply();
          }}
        >
          <Field label="Actor">
            <input className={`${inputCls} w-32`} value={draft.actor} onChange={(e) => setDraft({ ...draft, actor: e.target.value })} placeholder="browser" />
          </Field>
          <Field label="Action" hint="exact, or a prefix like vm.">
            <input className={`${inputCls} w-40 font-mono`} value={draft.action} onChange={(e) => setDraft({ ...draft, action: e.target.value })} placeholder="vm." />
          </Field>
          <Field label="Target contains">
            <input className={`${inputCls} w-40 font-mono`} value={draft.target} onChange={(e) => setDraft({ ...draft, target: e.target.value })} />
          </Field>
          <Field label="From">
            <input type="datetime-local" className={inputCls} value={draft.since} onChange={(e) => setDraft({ ...draft, since: e.target.value })} />
          </Field>
          <Field label="To">
            <input type="datetime-local" className={inputCls} value={draft.until} onChange={(e) => setDraft({ ...draft, until: e.target.value })} />
          </Field>
          <Button type="submit">Apply</Button>
          <Button
            variant="ghost"
            onClick={() => {
              setDraft({ actor: "", action: "", target: "", since: "", until: "" });
              setFilters({ actor: null, action: null, target: null, since: null, until: null, before: null, limit: null });
            }}
          >
            Clear
          </Button>
        </form>
      </Card>
      <Card title="Entries">
        {q.isError && <p className="text-sm text-rose-600">{q.error.message}</p>}
        {q.isPending && <p className="text-sm text-zinc-500">Loading…</p>}
        {!q.isPending && entries.length === 0 && <p className="text-sm text-zinc-500">No matching entries.</p>}
        {entries.length > 0 && (
          <div className="overflow-x-auto">
            <table className="w-full text-left text-sm">
              <thead className="text-xs text-zinc-500">
                <tr>
                  <th className="pr-3 font-medium">Time</th>
                  <th className="pr-3 font-medium">Actor</th>
                  <th className="pr-3 font-medium">Action</th>
                  <th className="pr-3 font-medium">Target</th>
                  <th className="pr-3 font-medium">Outcome</th>
                  <th className="pr-3 font-medium">From</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                {entries.map((e) => (
                  <Row key={e.id} e={e} />
                ))}
              </tbody>
            </table>
          </div>
        )}
        {q.hasNextPage && (
          <div className="mt-3">
            <Button variant="ghost" disabled={q.isFetchingNextPage} onClick={() => q.fetchNextPage()}>
              {q.isFetchingNextPage ? "Loading…" : "Older entries"}
            </Button>
          </div>
        )}
      </Card>
    </div>
  );
}
