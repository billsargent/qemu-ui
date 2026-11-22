import { useEffect, useRef, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { ApiError, api } from "../api/client";
import type { VmSpec } from "../api/generated/VmSpec";
import { VmEditor } from "../components/VmEditor";
import { ImportCard } from "../components/ImportExport";
import { TemplateGallery, type TemplateChoice } from "../components/TemplateGallery";
import { Button, Card, Field, inputCls } from "../components/ui";
import { navigate } from "../router";

/** The plan's notes, the new disk's size and, when needed, consent to go without a TPM. */
function TemplatePanel({
  choice,
  diskGib,
  setDiskGib,
  consent,
  setConsent,
  onCancel,
}: {
  choice: TemplateChoice;
  diskGib: number;
  setDiskGib: (n: number) => void;
  consent: boolean;
  setConsent: (b: boolean) => void;
  onCancel: () => void;
}) {
  const { plan } = choice;
  return (
    <Card
      title={`From template: ${plan.template}`}
      actions={
        <Button variant="ghost" onClick={onCancel}>
          Back to the gallery
        </Button>
      }
    >
      <div className="space-y-3 text-sm">
        {plan.notes.length > 0 && (
          <ul className="space-y-1.5">
            {plan.notes.map((n, i) => (
              <li key={i} className={n.level === "warning" ? "text-amber-700 dark:text-amber-400" : "text-zinc-600 dark:text-zinc-400"}>
                {n.level === "warning" ? "⚠ " : "• "}
                {n.text}
              </li>
            ))}
          </ul>
        )}
        <div className="flex flex-wrap gap-4">
          <Field label="System disk (GiB)" hint={`New qcow2 image in pool '${choice.pool}', ${plan.disk_bus} bus`}>
            <input
              className={inputCls}
              type="number"
              min={1}
              value={diskGib}
              onChange={(e) => setDiskGib(Math.max(1, Number(e.target.value)))}
            />
          </Field>
          <Field label="Installer">
            <span className="py-1">{choice.install ? `${choice.install.pool}/${choice.install.name}` : "none"}</span>
          </Field>
          {choice.drivers && (
            <Field label="Drivers">
              <span className="py-1">{`${choice.drivers.pool}/${choice.drivers.name}`}</span>
            </Field>
          )}
        </div>
        {plan.tpm_missing && (
          <label className="flex items-start gap-2 rounded border border-amber-300 bg-amber-50 p-2 text-amber-900 dark:border-amber-800 dark:bg-amber-950 dark:text-amber-200">
            <input type="checkbox" className="mt-0.5" checked={consent} onChange={(e) => setConsent(e.target.checked)} />
            <span>Create this VM without a TPM. Setup will need the registry bypass described above, and the installation is unsupported.</span>
          </label>
        )}
      </div>
    </Card>
  );
}

export default function NewVmPage() {
  const qc = useQueryClient();
  const caps = useQuery({ queryKey: ["capabilities"], queryFn: api.capabilities });
  const [arch, setArch] = useState<string | null>(null);
  const [spec, setSpec] = useState<VmSpec | null>(null);
  const [saving, setSaving] = useState(false);
  const [choice, setChoice] = useState<TemplateChoice | null>(null);
  const [diskGib, setDiskGib] = useState(20);
  const [consent, setConsent] = useState(false);
  // An import or a template sets the architecture itself; don't replace its spec with defaults.
  const keep = useRef(false);

  // Default to the host's native architecture.
  useEffect(() => {
    const c = caps.data?.caps;
    if (!arch && c?.binaries.length) setArch(c.binaries.find((b) => b.native && b.arch === c.host.arch)?.arch ?? c.binaries[0].arch);
  }, [caps.data, arch]);

  useEffect(() => {
    if (!arch) return;
    if (keep.current) {
      keep.current = false;
      return;
    }
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

  const adopt = (s: VmSpec) => {
    if (s.arch !== arch) keep.current = true;
    setArch(s.arch);
    setSpec(s);
  };

  return (
    <div className="max-w-7xl space-y-4">
      <h1 className="text-xl font-semibold">New virtual machine</h1>
      {choice ? (
        <TemplatePanel
          choice={choice}
          diskGib={diskGib}
          setDiskGib={setDiskGib}
          consent={consent}
          setConsent={setConsent}
          onCancel={() => setChoice(null)}
        />
      ) : (
        <>
          <TemplateGallery
            name={spec.name}
            onChoose={(c) => {
              setChoice(c);
              setDiskGib(Number(c.plan.disk_gib));
              setConsent(false);
              adopt(c.plan.spec);
            }}
          />
          <ImportCard
            onImported={(s) => {
              adopt(s);
            }}
          />
        </>
      )}
      <VmEditor
        key={choice ? `tpl-${choice.plan.template}-${spec.arch}` : "plain"}
        initial={spec}
        archEditable={!choice}
        onArchChange={setArch}
        saving={saving}
        saveLabel="Create VM"
        onSave={async (s) => {
          if (choice?.plan.tpm_missing && !s.tpm && !consent) {
            throw new ApiError(409, "Confirm creating the VM without a TPM first (see above).", []);
          }
          setSaving(true);
          try {
            const vm = choice
              ? await api.createFromTemplate({
                  template: choice.plan.template,
                  spec: s,
                  disk: { pool: choice.pool, size_gib: diskGib, bus: choice.plan.disk_bus },
                  install: choice.install ?? undefined,
                  drivers: choice.drivers ?? undefined,
                  accept_without_tpm: consent,
                })
              : await api.createVm(s);
            qc.invalidateQueries({ queryKey: ["vms"] });
            qc.invalidateQueries({ queryKey: ["all-images"] });
            navigate(`/vms/${vm.summary.id}`);
          } finally {
            setSaving(false);
          }
        }}
      />
    </div>
  );
}
