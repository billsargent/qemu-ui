import { useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { api } from "../api/client";
import type { ImportFormat } from "../api/generated/ImportFormat";
import type { ImportResult } from "../api/generated/ImportResult";
import type { VmSpec } from "../api/generated/VmSpec";
import { Badge, Button, Card, Field, inputCls } from "./ui";

/** Guess the format from the text: XML is libvirt, anything else argv. */
function detect(text: string): ImportFormat {
  return text.trimStart().startsWith("<") ? "libvirt" : "argv";
}

/**
 * Import a VM from a QEMU command line or a libvirt domain XML (M4-07).
 * Nothing is created here: the result opens in the normal editor.
 */
export function ImportCard({ onImported }: { onImported: (spec: VmSpec) => void }) {
  const [open, setOpen] = useState(false);
  const [text, setText] = useState("");
  const [format, setFormat] = useState<ImportFormat | "auto">("auto");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [result, setResult] = useState<ImportResult | null>(null);
  // Per entry of files_to_map: "pool/name", or "" to keep the placeholder.
  const [choice, setChoice] = useState<string[]>([]);
  const images = useQuery({ queryKey: ["images", "all"], queryFn: api.allImages, enabled: result !== null });
  const hostDisks = useQuery({ queryKey: ["host-disks"], queryFn: api.hostDisks, enabled: result !== null, staleTime: 30_000 });

  const run = async () => {
    setBusy(true);
    setError(null);
    try {
      const r = await api.importVm(format === "auto" ? detect(text) : format, text);
      setResult(r);
      setChoice(
        r.files_to_map.map((f) => {
          if (f.mapped) return `${f.mapped.pool}/${f.mapped.name}`;
          return "";
        }),
      );
    } catch (e) {
      setError((e as Error).message);
      setResult(null);
    } finally {
      setBusy(false);
    }
  };

  const imageOptions = useMemo(() => (images.data ?? []).map((i) => `${i.pool}/${i.name}`), [images.data]);

  // Suggest an existing image with the same file name.
  const suggestion = (name: string) => (images.data ?? []).find((i) => i.name === name);

  const apply = () => {
    if (!result) return;
    const spec: VmSpec = structuredClone(result.spec);
    result.files_to_map.forEach((f, i) => {
      const c = choice[i];
      if (!c) return;
      const slash = c.indexOf("/");
      spec.disks[f.disk] = { ...spec.disks[f.disk], image: { pool: c.slice(0, slash), name: c.slice(slash + 1) } };
    });
    onImported(spec);
    setOpen(false);
  };

  if (!open) {
    return (
      <div className="flex items-center gap-2 text-sm text-zinc-500">
        <Button variant="ghost" onClick={() => setOpen(true)}>
          Import…
        </Button>
        <span>from a QEMU command line or a libvirt domain XML</span>
      </div>
    );
  }

  return (
    <Card
      title="Import a VM"
      actions={
        <Button variant="ghost" onClick={() => setOpen(false)}>
          Close
        </Button>
      }
    >
      <div className="space-y-3">
        <p className="text-xs text-zinc-500">
          Paste a QEMU command line or a libvirt domain XML, or pick a file. Nothing is created until you save the VM in the
          editor. Host paths are never kept in the spec: map each referenced file to an image in a pool.
        </p>
        <textarea
          className={`${inputCls} h-40 w-full font-mono text-xs`}
          placeholder="qemu-system-x86_64 -machine q35 -m 4G -drive file=…   or   <domain type='kvm'>…</domain>"
          value={text}
          onChange={(e) => setText(e.target.value)}
        />
        <div className="flex flex-wrap items-end gap-2">
          <Field label="Format">
            <select className={inputCls} value={format} onChange={(e) => setFormat(e.target.value as ImportFormat | "auto")}>
              <option value="auto">Detect</option>
              <option value="argv">QEMU command line</option>
              <option value="libvirt">libvirt domain XML</option>
            </select>
          </Field>
          <Field label="Or a file">
            <input
              type="file"
              accept=".xml,.txt,.sh,.cmd,.bat,.ps1,text/*"
              className="text-xs"
              onChange={async (e) => {
                const f = e.target.files?.[0];
                if (f) setText(await f.text());
              }}
            />
          </Field>
          <Button onClick={run} disabled={!text.trim() || busy}>
            {busy ? "Reading…" : "Read"}
          </Button>
        </div>
        {error && <p className="text-xs text-rose-600">{error}</p>}

        {result && (
          <div className="space-y-3">
            <div className="text-sm">
              <span className="font-medium">{result.spec.name}</span>{" "}
              <span className="text-zinc-500">
                {result.spec.arch} · {result.spec.machine.type} · {result.spec.cpu.count} vCPU · {result.spec.memory.size_mib} MiB ·{" "}
                {result.spec.disks.length} disk(s) · {result.spec.nics.length} NIC(s)
              </span>
            </div>

            {result.warnings.length > 0 && (
              <div>
                <div className="mb-1 text-xs font-medium text-zinc-600 dark:text-zinc-400">Notes ({result.warnings.length})</div>
                <ul className="list-disc space-y-0.5 pl-5 text-xs text-amber-700 dark:text-amber-400">
                  {result.warnings.map((w) => (
                    <li key={w}>{w}</li>
                  ))}
                </ul>
              </div>
            )}

            {result.files_to_map.length > 0 && (
              <div>
                <div className="mb-1 text-xs font-medium text-zinc-600 dark:text-zinc-400">Files</div>
                <table className="w-full text-xs">
                  <tbody>
                    {result.files_to_map.map((f, i) => {
                      const sug = suggestion(f.suggested_name);
                      return (
                        <tr key={i} className="border-t border-zinc-200 dark:border-zinc-800">
                          <td className="py-1 pr-2 align-top">
                            <div className="font-mono break-all">{f.path}</div>
                            <div className="text-zinc-500">
                              disk {f.disk}
                              {f.format ? ` · ${f.format}` : ""}
                              {f.usage === "host-device" && (
                                <>
                                  {" "}
                                  <Badge tone="amber">host device</Badge>
                                </>
                              )}
                              {f.mapped && (
                                <>
                                  {" "}
                                  <Badge tone="green">found in pool</Badge>
                                </>
                              )}
                            </div>
                          </td>
                          <td className="py-1 align-top">
                            <select
                              className={`${inputCls} w-64`}
                              value={choice[i] ?? ""}
                              onChange={(e) => setChoice(choice.map((c, j) => (j === i ? e.target.value : c)))}
                            >
                              <option value="">
                                Map later (placeholder {result.spec.disks[f.disk].image.name})
                              </option>
                              {sug && !f.mapped && (
                                <option value={`${sug.pool}/${sug.name}`}>Same name: {`${sug.pool}/${sug.name}`}</option>
                              )}
                              <optgroup label="Pool images">
                                {imageOptions.map((o) => (
                                  <option key={o} value={o}>
                                    {o}
                                  </option>
                                ))}
                              </optgroup>
                              {f.usage === "host-device" && hostDisks.data?.available && (
                                <optgroup label="Host disks (via the qemu-ui helper)">
                                  {hostDisks.data.disks.map((d) => (
                                    <option key={d.id} value={`@host/${d.id}`} disabled={d.system}>
                                      {d.label || d.id}
                                    </option>
                                  ))}
                                </optgroup>
                              )}
                            </select>
                          </td>
                        </tr>
                      );
                    })}
                  </tbody>
                </table>
                <p className="mt-1 text-xs text-zinc-500">
                  Files that are not in a pool yet: upload or copy them into one (Storage page), or keep the placeholder and pick an
                  image in the editor.
                </p>
              </div>
            )}

            <Button onClick={apply}>Open in editor</Button>
          </div>
        )}
      </div>
    </Card>
  );
}

/** Download the VM as libvirt domain XML. */
export function ExportCard({ id }: { id: string }) {
  return (
    <Card title="Export">
      <div className="flex flex-wrap items-center gap-3 text-sm">
        <a className="rounded px-2.5 py-1 text-xs font-medium hover:bg-zinc-100 dark:hover:bg-zinc-800" href={`/api/v1/vms/${id}/export/libvirt`} download>
          Download libvirt XML
        </a>
        <span className="text-xs text-zinc-500">
          For <code>virsh define</code>. What libvirt can't express is listed in a comment at the top of the file.
        </span>
      </div>
    </Card>
  );
}
