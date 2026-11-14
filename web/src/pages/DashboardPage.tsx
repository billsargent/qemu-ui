import { useQuery } from "@tanstack/react-query";
import { api } from "../api/client";
import type { VmSummary } from "../api/generated/VmSummary";
import { Badge, Button, Card } from "../components/ui";
import { AccelBadge, StateBadge, VmActions, explainPause, useUptime } from "../components/vm";

export default function DashboardPage() {
  const vms = useQuery({ queryKey: ["vms"], queryFn: api.vms });
  return (
    <div className="max-w-6xl space-y-5">
      <div className="flex items-center gap-3">
        <h1 className="text-xl font-semibold">Virtual machines</h1>
        <div className="ml-auto">
          <a href="#/vms/new">
            <Button>New VM</Button>
          </a>
        </div>
      </div>
      {vms.isPending ? (
        <p className="text-sm text-zinc-500">Loading…</p>
      ) : vms.isError ? (
        <p className="text-sm text-rose-600">Cannot reach the daemon: {vms.error.message}</p>
      ) : vms.data.length === 0 ? (
        <Card title="No VMs yet">
          <p className="text-sm">
            Create one with <a className="underline" href="#/vms/new">New VM</a>. Put installer ISOs and disks in a
            pool first: see <a className="underline" href="#/storage">Storage</a>.
          </p>
        </Card>
      ) : (
        <div className="grid grid-cols-1 gap-3 md:grid-cols-2 xl:grid-cols-3">
          {vms.data.map((vm) => (
            <VmCard key={vm.id} vm={vm} />
          ))}
        </div>
      )}
    </div>
  );
}

function VmCard({ vm }: { vm: VmSummary }) {
  const uptime = useUptime(vm.started_at);
  return (
    <div className="flex flex-col gap-2 rounded-lg border border-zinc-200 bg-white p-3 dark:border-zinc-800 dark:bg-zinc-900">
      <div className="flex items-center gap-2">
        <a href={`#/vms/${vm.id}`} className="truncate font-medium hover:underline">
          {vm.name}
        </a>
        <StateBadge vm={vm} />
        <span className="ml-auto font-mono text-xs text-zinc-500">{vm.arch}</span>
      </div>
      <div className="flex flex-wrap items-center gap-2 text-xs text-zinc-500">
        {vm.state !== "stopped" && <AccelBadge accel={vm.accel} />}
        {uptime && <span>up {uptime}</span>}
        {vm.restart_pending && (
          <Badge tone="blue" title="Settings changed since this run started">
            restart to apply changes
          </Badge>
        )}
      </div>
      {vm.state === "paused" && vm.pause_reason !== "user" && (
        <p className="text-xs text-amber-700 dark:text-amber-400">{explainPause(vm.pause_reason)}</p>
      )}
      {vm.last_error && vm.state === "crashed" && (
        <p className="line-clamp-3 font-mono text-xs whitespace-pre-wrap text-rose-600" title={vm.last_error}>
          {vm.last_error}
        </p>
      )}
      <div className="mt-auto flex items-center gap-1 pt-1">
        <VmActions vm={vm} compact />
        {vm.has_vnc && (
          <a href={`#/vms/${vm.id}/console`} className="ml-auto">
            <Button variant="ghost">Console</Button>
          </a>
        )}
      </div>
    </div>
  );
}
