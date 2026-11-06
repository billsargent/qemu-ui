import { useEffect, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "../api/client";
import type { VmSpec } from "../api/generated/VmSpec";
import { VmEditor } from "../components/VmEditor";
import { navigate } from "../router";

export default function NewVmPage() {
  const qc = useQueryClient();
  const caps = useQuery({ queryKey: ["capabilities"], queryFn: api.capabilities });
  const [arch, setArch] = useState<string | null>(null);
  const [spec, setSpec] = useState<VmSpec | null>(null);
  const [saving, setSaving] = useState(false);

  // Default to the host's native architecture.
  useEffect(() => {
    const c = caps.data?.caps;
    if (!arch && c?.binaries.length) setArch(c.binaries.find((b) => b.native && b.arch === c.host.arch)?.arch ?? c.binaries[0].arch);
  }, [caps.data, arch]);

  useEffect(() => {
    if (!arch) return;
    let cancelled = false;
    api.defaults(arch, spec?.name ?? "new-vm").then((d) => !cancelled && setSpec(d.spec));
    return () => {
      cancelled = true;
    };
    // Only refetch defaults when the architecture changes.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [arch]);

  if (caps.isPending || !spec) return <p className="text-sm text-zinc-500">Loading…</p>;
  if (!caps.data?.caps?.binaries.length) {
    return <p className="text-sm">No QEMU binaries were found. See the System page.</p>;
  }

  return (
    <div className="max-w-7xl space-y-4">
      <h1 className="text-xl font-semibold">New virtual machine</h1>
      <VmEditor
        initial={spec}
        archEditable
        onArchChange={setArch}
        saving={saving}
        saveLabel="Create VM"
        onSave={async (s) => {
          setSaving(true);
          try {
            const vm = await api.createVm(s);
            qc.invalidateQueries({ queryKey: ["vms"] });
            navigate(`/vms/${vm.summary.id}`);
          } finally {
            setSaving(false);
          }
        }}
      />
    </div>
  );
}
