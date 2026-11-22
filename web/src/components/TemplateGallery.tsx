import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { ApiError, api } from "../api/client";
import type { ImageRef } from "../api/generated/ImageRef";
import type { TemplateInfo } from "../api/generated/TemplateInfo";
import type { TemplatePlan } from "../api/generated/TemplatePlan";
import { Badge, Button, Card, Field, inputCls } from "./ui";

/** What the user picked besides the plan itself. */
export interface TemplateChoice {
  plan: TemplatePlan;
  pool: string;
  install: ImageRef | null;
  drivers: ImageRef | null;
}

const refKey = (r: ImageRef | null) => (r ? `${r.pool}/${r.name}` : "");

function NeedBadges({ t }: { t: TemplateInfo }) {
  const tpl = t.template;
  return (
    <div className="flex flex-wrap gap-1">
      {tpl.firmware === "uefi" && <Badge>UEFI</Badge>}
      {tpl.firmware === "uefi-secure-boot" && <Badge tone="blue">Secure Boot</Badge>}
      {tpl.tpm !== "none" &&
        (t.tpm_unavailable ? (
          <Badge tone="amber" title={t.tpm_unavailable}>
            {tpl.tpm === "required" ? "Needs TPM — unavailable here" : "No TPM here"}
          </Badge>
        ) : (
          <Badge tone="green">TPM 2.0</Badge>
        ))}
      {tpl.drivers && <Badge title="Paravirtual drivers come on a separate ISO">{tpl.drivers}</Badge>}
    </div>
  );
}

/**
 * Template gallery (M7-04): pick a guest OS, its installer and (for
 * Windows) the virtio-win drivers ISO; the daemon plans a spec for this host.
 */
export function TemplateGallery({ name, onChoose }: { name: string; onChoose: (c: TemplateChoice) => void }) {
  const templates = useQuery({ queryKey: ["templates"], queryFn: api.templates });
  const images = useQuery({ queryKey: ["all-images"], queryFn: api.allImages });
  const pools = useQuery({ queryKey: ["pools"], queryFn: api.pools });
  const [picked, setPicked] = useState<TemplateInfo | null>(null);
  const [arch, setArch] = useState("");
  const [install, setInstall] = useState<ImageRef | null>(null);
  const [drivers, setDrivers] = useState<ImageRef | null>(null);
  const [pool, setPool] = useState("default");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const list = (templates.data ?? []).filter((t) => t.arches.length > 0);
  const isos = (images.data ?? []).filter((i) => i.format === "raw" && i.name.toLowerCase().endsWith(".iso"));
  const pick = (t: TemplateInfo) => {
    setPicked(t);
    setArch(t.arches[0]);
    setError(null);
  };
  const isoSelect = (value: ImageRef | null, set: (r: ImageRef | null) => void) => (
    <select
      className={inputCls}
      value={refKey(value)}
      onChange={(e) => set(isos.map((i) => ({ pool: i.pool, name: i.name })).find((r) => refKey(r) === e.target.value) ?? null)}
    >
      <option value="">None</option>
      {isos.map((i) => (
        <option key={refKey(i)} value={refKey(i)}>
          {i.pool}/{i.name}
        </option>
      ))}
    </select>
  );

  const use = async () => {
    if (!picked) return;
    setBusy(true);
    setError(null);
    try {
      const plan = await api.planTemplate(picked.template.id, arch, name, drivers !== null);
      onChoose({ plan, pool, install, drivers });
    } catch (e) {
      setError(e instanceof ApiError ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Card title="Start from a template">
      {templates.isPending ? (
        <p className="text-sm text-zinc-500">Loading…</p>
      ) : list.length === 0 ? (
        <p className="text-sm text-zinc-500">No template fits the installed QEMU binaries.</p>
      ) : (
        <div className="space-y-4">
          <div className="grid gap-2 sm:grid-cols-2 lg:grid-cols-3">
            {list.map((t) => (
              <button
                key={t.template.id}
                type="button"
                onClick={() => pick(t)}
                className={`rounded-md border p-3 text-left text-sm transition ${
                  picked?.template.id === t.template.id
                    ? "border-sky-500 ring-1 ring-sky-500"
                    : "border-zinc-200 hover:border-zinc-400 dark:border-zinc-800 dark:hover:border-zinc-600"
                }`}
              >
                <div className="font-medium">{t.template.name}</div>
                <div className="mt-1 text-xs text-zinc-500">{t.template.description}</div>
                <div className="mt-2 text-xs text-zinc-500">
                  {t.template.memory_mib >= 1024 ? `${t.template.memory_mib / 1024} GiB` : `${t.template.memory_mib} MiB`} ·{" "}
                  {t.template.cpus} vCPU · {t.template.disk_gib} GiB disk
                </div>
                <div className="mt-2">
                  <NeedBadges t={t} />
                </div>
              </button>
            ))}
          </div>

          {picked && (
            <div className="grid gap-3 border-t border-zinc-200 pt-4 sm:grid-cols-2 dark:border-zinc-800">
              {picked.arches.length > 1 && (
                <Field label="Architecture">
                  <select className={inputCls} value={arch} onChange={(e) => setArch(e.target.value)}>
                    {picked.arches.map((a) => (
                      <option key={a}>{a}</option>
                    ))}
                  </select>
                </Field>
              )}
              <Field label="Installer ISO" hint="Upload or fetch it on the Storage page first.">
                {isoSelect(install, setInstall)}
              </Field>
              {picked.template.drivers && (
                <Field
                  label={`${picked.template.drivers} drivers ISO`}
                  hint={
                    picked.template.drivers_url ? (
                      <a className="underline" href={picked.template.drivers_url} target="_blank" rel="noreferrer">
                        Where to get it
                      </a>
                    ) : undefined
                  }
                >
                  {isoSelect(drivers, setDrivers)}
                </Field>
              )}
              <Field label="Pool for the new disk">
                <select className={inputCls} value={pool} onChange={(e) => setPool(e.target.value)}>
                  {(pools.data ?? [{ name: "default" }]).map((p) => (
                    <option key={p.name}>{p.name}</option>
                  ))}
                </select>
              </Field>
              {picked.tpm_unavailable && picked.template.tpm !== "none" && (
                <p className="text-xs text-amber-700 sm:col-span-2 dark:text-amber-400">
                  No TPM on this host: {picked.tpm_unavailable}.
                </p>
              )}
              <div className="sm:col-span-2">
                <Button onClick={use} disabled={busy}>
                  {busy ? "Planning…" : `Use ${picked.template.name}`}
                </Button>
                {error && <p className="mt-2 text-sm text-rose-600">{error}</p>}
              </div>
            </div>
          )}
        </div>
      )}
    </Card>
  );
}
