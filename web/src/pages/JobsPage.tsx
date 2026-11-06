import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { api } from "../api/client";
import type { Job } from "../api/generated/Job";
import type { JobStatus } from "../api/generated/JobStatus";
import { Badge, Button, Card } from "../components/ui";

const TONE: Record<JobStatus, "green" | "amber" | "red" | "zinc" | "blue"> = {
  queued: "zinc",
  running: "blue",
  succeeded: "green",
  failed: "red",
  cancelled: "zinc",
  interrupted: "amber",
};

export function ProgressBar({ value }: { value: number | null }) {
  return (
    <div className="h-1.5 w-full overflow-hidden rounded bg-zinc-200 dark:bg-zinc-800">
      {value == null ? (
        <div className="h-full w-1/3 animate-pulse bg-sky-500" />
      ) : (
        <div className="h-full bg-sky-500 transition-all" style={{ width: `${Math.round(value * 100)}%` }} />
      )}
    </div>
  );
}

export function JobRow({ job }: { job: Job }) {
  const [log, setLog] = useState<string | null>(null);
  const active = job.status === "queued" || job.status === "running";
  return (
    <li className="border-b border-zinc-100 py-2 last:border-0 dark:border-zinc-800">
      <div className="flex items-center gap-2 text-sm">
        <Badge tone={TONE[job.status]}>{job.status}</Badge>
        <span className="truncate">{job.title}</span>
        <span className="ml-auto text-xs whitespace-nowrap text-zinc-500">
          {new Date(job.created_at * 1000).toLocaleString()}
        </span>
        {job.cancellable && active && (
          <Button variant="ghost" onClick={() => void api.cancelJob(job.id)}>
            Cancel
          </Button>
        )}
        <Button variant="ghost" onClick={async () => setLog(log === null ? await api.jobLog(job.id) : null)}>
          {log === null ? "Log" : "Hide log"}
        </Button>
      </div>
      {active && (
        <div className="mt-1.5 flex items-center gap-2">
          <ProgressBar value={job.progress} />
          <span className="w-12 text-right text-xs text-zinc-500 tabular-nums">
            {job.progress != null ? `${Math.round(job.progress * 100)}%` : ""}
          </span>
        </div>
      )}
      {job.message && <p className="mt-1 text-xs text-zinc-500">{job.message}</p>}
      {job.error && <p className="mt-1 font-mono text-xs whitespace-pre-wrap text-rose-600">{job.error}</p>}
      {log !== null && (
        <pre className="mt-1 max-h-48 overflow-auto rounded bg-zinc-100 p-2 text-xs dark:bg-zinc-950">{log || "(empty)"}</pre>
      )}
    </li>
  );
}

export default function JobsPage() {
  const jobs = useQuery({ queryKey: ["jobs"], queryFn: api.jobs });
  return (
    <div className="max-w-5xl space-y-4">
      <h1 className="text-xl font-semibold">Jobs</h1>
      <Card title="Recent jobs">
        {jobs.isPending ? (
          <p className="text-sm text-zinc-500">Loading…</p>
        ) : !jobs.data?.length ? (
          <p className="text-sm text-zinc-500">No jobs yet. Disk creation, conversions, uploads and downloads appear here.</p>
        ) : (
          <ul>
            {jobs.data.map((j) => (
              <JobRow key={j.id} job={j} />
            ))}
          </ul>
        )}
      </Card>
    </div>
  );
}
