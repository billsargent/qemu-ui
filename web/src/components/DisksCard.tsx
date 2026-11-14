import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { ApiError, api } from "../api/client";
import type { DiskBus } from "../api/generated/DiskBus";
import type { DiskChange } from "../api/generated/DiskChange";
import type { Media } from "../api/generated/Media";
import type { VmDetail } from "../api/generated/VmDetail";
import { Badge, Button, Card, Field, inputCls } from "./ui";

const BUS_NAME: Record<DiskBus, string> = {
  virtio: "virtio",
  scsi: "SCSI",
  nvme: "NVMe",
  ide: "IDE/SATA",
  usb: "USB",
};

/**
 * Disks of a VM (M2-09). While it runs this shows what the VM has right
 * now, including disks added or removed live; changes apply immediately
 * (hotplug) and are saved for the next start. While stopped, changes are
 * saved only.
 */
export function DisksCard({ vm }: { vm: VmDetail }) {
  const qc = useQueryClient();
  const id = vm.summary.id;
  const running = vm.live_spec != null;
  const spec = vm.live_spec ?? vm.spec;
  const images = useQuery({ queryKey: ["images", "all"], queryFn: api.allImages });
  const buses = useQuery({
    queryKey: ["buses", spec.arch, spec.machine.type],
    queryFn: () => api.diskBuses(spec.arch, spec.machine.type),
    staleTime: Infinity,
  });
  const [note, setNote] = useState<{ text: string; pending: boolean } | null>(null);
  const done = (c: DiskChange) => {
    setNote({ text: c.message, pending: c.pending });
    qc.invalidateQueries({ queryKey: ["vm", id] });
    qc.invalidateQueries({ queryKey: ["vms"] });
    qc.invalidateQueries({ queryKey: ["images"] });
    qc.invalidateQueries({ queryKey: ["pool"] });
  };
  const remove = useMutation({
    mutationFn: ({ n, force }: { n: number; force: boolean }) => api.removeDisk(id, n, force),
    onSuccess: done,
  });
  const media = useMutation({
    mutationFn: ({ n, value }: { n: number; value: string }) => {
      const [pool, ...rest] = value.split("/");
      return api.changeMedia(id, n, pool, rest.join("/"));
    },
    onSuccess: done,
  });

  // Add-disk form.
  const support = (b: DiskBus) => buses.data?.find((s) => s.bus === b);
  const usable = (b: DiskBus) => {
    const s = support(b);
    if (!s) return true; // machine not probed: let the daemon decide
    return s.available && (!running || s.hotplug);
  };
  const [form, setForm] = useState<{ image: string; media: Media; bus: DiskBus }>({
    image: "",
    media: "disk",
    bus: "virtio",
  });
  const bus = usable(form.bus) ? form.bus : ((Object.keys(BUS_NAME) as DiskBus[]).find(usable) ?? form.bus);
  const add = useMutation({
    mutationFn: () => {
      const [pool, ...rest] = form.image.split("/");
      return api.addDisk(id, {
        image: { pool, name: rest.join("/") },
        bus,
        media: form.media,
        readonly: form.media === "cdrom",
        boot_index: null,
        cache: "none",
        aio: null,
        discard: true,
      });
    },
    onSuccess: (c) => {
      done(c);
      setForm({ ...form, image: "" });
    },
  });
  const error = add.error ?? remove.error ?? media.error;
  const busy = add.isPending || remove.isPending || media.isPending;
  const isos = (images.data ?? []).filter((i) => i.format === "raw" || i.name.toLowerCase().endsWith(".iso"));

  return (
    <Card title={running ? "Disks (live)" : "Disks"}>
      <ul className="divide-y divide-zinc-100 text-sm dark:divide-zinc-800">
        {spec.disks.map((x, i) => {
          const n = x.slot ?? i;
          const s = support(x.bus);
          const noUnplug = running && (x.bus === "ide" || s?.hotplug === false);
          return (
            <li key={`${n}-${x.image.pool}/${x.image.name}`} className="flex flex-wrap items-center gap-2 py-1.5">
              <span className="font-mono">{x.image.name}</span>
              <span className="text-xs text-zinc-500">
                {x.media === "cdrom" ? "CD-ROM" : "disk"} on {BUS_NAME[x.bus]}
                {x.boot_index != null ? ` · boot ${x.boot_index}` : ""}
                {x.readonly && x.media !== "cdrom" ? " · read-only" : ""}
              </span>
              <span className="font-mono text-xs text-zinc-400" title="QEMU id index (qui-disk<n>)">
                #{n}
              </span>
              <div className="ml-auto flex items-center gap-1">
                {x.media === "cdrom" && (
                  <select
                    className={`${inputCls} text-xs`}
                    value=""
                    disabled={busy}
                    title={running ? "Swap the disc in the running VM" : "Used at the next start"}
                    onChange={(e) => e.target.value && media.mutate({ n, value: e.target.value })}
                  >
                    <option value="">Change media…</option>
                    {isos
                      .filter((img) => !(img.pool === x.image.pool && img.name === x.image.name))
                      .map((img) => (
                        <option key={`${img.pool}/${img.name}`} value={`${img.pool}/${img.name}`}>
                          {img.pool === "default" ? "" : `${img.pool} / `}
                          {img.name}
                        </option>
                      ))}
                  </select>
                )}
                <Button
                  variant="ghost"
                  disabled={busy || noUnplug}
                  title={
                    noUnplug
                      ? "IDE/SATA disks cannot be removed while the VM runs; stop it first"
                      : running
                        ? "Detach from the running VM (the guest is asked to release it)"
                        : "Remove from the configuration (the image file is kept)"
                  }
                  onClick={() => {
                    const boot = running && x.boot_index != null;
                    const ok = window.confirm(
                      boot
                        ? `${x.image.name} is a boot disk. Removing it from a running guest can crash it. Remove anyway?`
                        : `Remove ${x.image.name} from ${vm.summary.name}? The image file is kept.`,
                    );
                    if (ok) remove.mutate({ n, force: boot });
                  }}
                >
                  Remove
                </Button>
              </div>
            </li>
          );
        })}
        {spec.disks.length === 0 && <li className="py-1.5 text-zinc-500">No disks.</li>}
      </ul>

      <form
        className="mt-3 flex flex-wrap items-end gap-2 border-t border-zinc-200 pt-3 dark:border-zinc-800"
        onSubmit={(e) => {
          e.preventDefault();
          if (form.image) add.mutate();
        }}
      >
        <Field label="Image">
          <select className={inputCls} value={form.image} onChange={(e) => setForm({ ...form, image: e.target.value })}>
            <option value="">— choose —</option>
            {(images.data ?? []).map((img) => (
              <option key={`${img.pool}/${img.name}`} value={`${img.pool}/${img.name}`}>
                {img.pool === "default" ? "" : `${img.pool} / `}
                {img.name} ({img.format})
              </option>
            ))}
          </select>
        </Field>
        <Field label="As">
          <select className={inputCls} value={form.media} onChange={(e) => setForm({ ...form, media: e.target.value as Media })}>
            <option value="disk">Disk</option>
            <option value="cdrom">CD-ROM</option>
          </select>
        </Field>
        <Field label="Bus" hint={support(bus)?.reason ?? undefined}>
          <select className={inputCls} value={bus} onChange={(e) => setForm({ ...form, bus: e.target.value as DiskBus })}>
            {(Object.keys(BUS_NAME) as DiskBus[]).map((b) => {
              const s = support(b);
              const why = !s ? undefined : !s.available ? s.reason : running && !s.hotplug ? s.reason : undefined;
              const cdromBad = form.media === "cdrom" && (b === "virtio" || b === "nvme");
              return (
                <option key={b} value={b} disabled={!usable(b) || cdromBad} title={why ?? undefined}>
                  {BUS_NAME[b]}
                  {!usable(b) ? (running ? " — not hotpluggable" : " — not on this machine") : ""}
                </option>
              );
            })}
          </select>
        </Field>
        <Button type="submit" disabled={!form.image || busy}>
          {running ? "Attach now" : "Add disk"}
        </Button>
      </form>

      {note && (
        <p className={`mt-2 text-xs ${note.pending ? "text-amber-700 dark:text-amber-400" : "text-zinc-500"}`}>
          {note.pending && <Badge tone="amber">pending</Badge>} {note.text}
        </p>
      )}
      {error && (
        <p className="mt-2 text-xs text-rose-600">
          {error instanceof ApiError && error.diagnostics.length ? error.diagnostics[0].message : error.message}
        </p>
      )}
    </Card>
  );
}
