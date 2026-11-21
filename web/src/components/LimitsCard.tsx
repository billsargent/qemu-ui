import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "../api/client";
import type { Diagnostic } from "../api/generated/Diagnostic";
import type { ResourceLimits } from "../api/generated/ResourceLimits";
import { Button, Card, Field, inputCls } from "./ui";

const MiB = 1024 * 1024;

/** A number input where empty means "no limit". */
function NumberOrNone({
  value,
  onChange,
  placeholder,
  scale = 1,
}: {
  value: number | null;
  onChange: (v: number | null) => void;
  placeholder?: string;
  scale?: number;
}) {
  return (
    <input
      className={`${inputCls} w-28`}
      type="number"
      min={1}
      placeholder={placeholder ?? "no limit"}
      value={value == null ? "" : value / scale}
      onChange={(e) => onChange(e.target.value === "" ? null : Math.round(Number(e.target.value) * scale))}
    />
  );
}

/** CPU, memory and disk IO limits (M6-07). */
export function LimitsCard({
  limits,
  onChange,
  vmId,
  diags,
}: {
  limits: ResourceLimits;
  onChange: (l: ResourceLimits) => void;
  vmId?: string;
  diags: Diagnostic[];
}) {
  const qc = useQueryClient();
  const support = useQuery({ queryKey: ["limits-support"], queryFn: api.limitsSupport, staleTime: 60_000 });
  const apply = useMutation({
    mutationFn: () => api.setLimits(vmId!, limits),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["vm", vmId] }),
  });
  const io = limits.io;
  const setIo = (patch: Partial<ResourceLimits["io"]>) => onChange({ ...limits, io: { ...io, ...patch } });

  return (
    <Card title="Resource limits">
      <div className="space-y-3 text-sm">
        <div className="flex flex-wrap gap-3">
          <Field label="CPU cap (% of one core)" hint="100 per vCPU at most">
            <NumberOrNone value={limits.cpu_max_percent} onChange={(v) => onChange({ ...limits, cpu_max_percent: v })} />
          </Field>
          <Field label="CPU weight" hint="share vs other VMs; 100 = default (Linux)">
            <NumberOrNone value={limits.cpu_weight} onChange={(v) => onChange({ ...limits, cpu_weight: v })} />
          </Field>
          <Field label="Memory limit (MiB)" hint="guest RAM + QEMU overhead">
            <NumberOrNone value={limits.memory_max_mib} onChange={(v) => onChange({ ...limits, memory_max_mib: v })} />
          </Field>
        </div>
        {support.data?.reason && (limits.cpu_max_percent != null || limits.cpu_weight != null || limits.memory_max_mib != null) && (
          <p className="text-xs text-amber-600">{support.data.reason}. They are saved but not enforced.</p>
        )}
        <div className="flex flex-wrap gap-3">
          <Field label="Disk read (MiB/s)">
            <NumberOrNone value={io.read_bytes_per_sec} scale={MiB} onChange={(v) => setIo({ read_bytes_per_sec: v })} />
          </Field>
          <Field label="Disk write (MiB/s)">
            <NumberOrNone value={io.write_bytes_per_sec} scale={MiB} onChange={(v) => setIo({ write_bytes_per_sec: v })} />
          </Field>
          <Field label="Read IOPS">
            <NumberOrNone value={io.read_iops} onChange={(v) => setIo({ read_iops: v })} />
          </Field>
          <Field label="Write IOPS">
            <NumberOrNone value={io.write_iops} onChange={(v) => setIo({ write_iops: v })} />
          </Field>
        </div>
        <p className="text-xs text-zinc-500">
          Disk limits are shared by all of the VM's disks. Empty fields mean no limit.
        </p>
        {vmId && (
          <div className="flex items-center gap-2">
            <Button variant="ghost" disabled={apply.isPending} onClick={() => apply.mutate()}>
              Apply now (live when running)
            </Button>
            {apply.data && <span className="text-xs text-zinc-500">{apply.data.message}</span>}
            {apply.error && <span className="text-xs text-rose-600">{apply.error.message}</span>}
          </div>
        )}
        {diags.map((d, i) => (
          <p key={i} className={`text-xs ${d.severity === "error" ? "text-rose-600" : "text-amber-600"}`}>
            {d.message}
          </p>
        ))}
      </div>
    </Card>
  );
}
