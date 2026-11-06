import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "../api/client";
import type { PoolSummary } from "../api/generated/PoolSummary";
import { Badge, Button, Card, Field, inputCls } from "../components/ui";
import { navigate } from "../router";
import { ProgressBar } from "./JobsPage";

export function formatBytes(n: number): string {
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let i = 0;
  let v = n;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(v < 10 && i > 0 ? 1 : 0)} ${units[i]}`;
}

const GiB = 1024 ** 3;

export default function StoragePage({ pool, image }: { pool?: string; image?: string }) {
  const pools = useQuery({ queryKey: ["pools"], queryFn: api.pools });
  const current = pool ?? "default";
  return (
    <div className="max-w-7xl space-y-5">
      <h1 className="text-xl font-semibold">Storage</h1>
      <div className="grid grid-cols-1 gap-3 md:grid-cols-2 xl:grid-cols-3">
        {pools.data?.map((p) => <PoolCard key={p.name} p={p} selected={p.name === current} />)}
        <AddPool />
      </div>
      <div className="grid grid-cols-1 gap-4 xl:grid-cols-[minmax(0,3fr)_minmax(0,2fr)]">
        <PoolImages pool={current} selected={image} />
        {image ? <ImagePanel pool={current} name={image} /> : <CreateDisk pool={current} />}
      </div>
    </div>
  );
}

function PoolCard({ p, selected }: { p: PoolSummary; selected: boolean }) {
  const limit = p.quota_bytes ?? (p.fs_free_bytes != null ? p.used_bytes + p.fs_free_bytes : null);
  return (
    <button
      type="button"
      onClick={() => navigate(`/storage/${p.name}`)}
      className={`rounded-lg border bg-white p-3 text-left dark:bg-zinc-900 ${
        selected ? "border-zinc-900 dark:border-zinc-100" : "border-zinc-200 dark:border-zinc-800"
      }`}
    >
      <div className="flex items-center gap-2">
        <span className="font-medium">{p.name}</span>
        {p.readonly && <Badge tone="amber">read-only</Badge>}
        {p.builtin && <Badge>built-in</Badge>}
        <span className="ml-auto text-xs text-zinc-500">{p.image_count} images</span>
      </div>
      <p className="mt-1 truncate font-mono text-xs text-zinc-500" title={p.path}>
        {p.path}
      </p>
      <div className="mt-2">
        <ProgressBar value={limit ? Math.min(1, p.used_bytes / limit) : 0} />
        <p className="mt-1 text-xs text-zinc-500">
          {formatBytes(p.used_bytes)} used
          {p.quota_bytes != null && ` of ${formatBytes(p.quota_bytes)} quota`}
          {p.fs_free_bytes != null && ` · ${formatBytes(p.fs_free_bytes)} free on disk`}
        </p>
      </div>
    </button>
  );
}

function AddPool() {
  const qc = useQueryClient();
  const [open, setOpen] = useState(false);
  const [name, setName] = useState("");
  const [path, setPath] = useState("");
  const [quota, setQuota] = useState("");
  const add = useMutation({
    mutationFn: () => api.addPool(name, path, quota ? Number(quota) * GiB : null),
    onSuccess: () => {
      setOpen(false);
      setName("");
      setPath("");
      setQuota("");
      qc.invalidateQueries({ queryKey: ["pools"] });
    },
  });
  if (!open) {
    return (
      <button
        type="button"
        onClick={() => setOpen(true)}
        className="rounded-lg border border-dashed border-zinc-300 p-3 text-sm text-zinc-500 hover:bg-zinc-100 dark:border-zinc-700 dark:hover:bg-zinc-900"
      >
        + Add a pool (an existing folder, e.g. on a bigger drive)
      </button>
    );
  }
  return (
    <form
      className="space-y-2 rounded-lg border border-zinc-200 bg-white p-3 dark:border-zinc-800 dark:bg-zinc-900"
      onSubmit={(e) => {
        e.preventDefault();
        add.mutate();
      }}
    >
      <Field label="Name">
        <input className={inputCls} value={name} onChange={(e) => setName(e.target.value)} placeholder="big-disk" />
      </Field>
      <Field label="Folder (must exist)">
        <input className={`${inputCls} font-mono`} value={path} onChange={(e) => setPath(e.target.value)} />
      </Field>
      <Field label="Quota in GiB (optional)">
        <input className={inputCls} type="number" min={1} value={quota} onChange={(e) => setQuota(e.target.value)} />
      </Field>
      <div className="flex items-center gap-2">
        <Button type="submit" disabled={!name || !path || add.isPending}>
          Add pool
        </Button>
        <Button variant="ghost" onClick={() => setOpen(false)}>
          Cancel
        </Button>
      </div>
      {add.isError && <p className="text-xs text-rose-600">{add.error.message}</p>}
    </form>
  );
}

function PoolImages({ pool, selected }: { pool: string; selected?: string }) {
  const info = useQuery({ queryKey: ["pool", pool], queryFn: () => api.pool(pool) });
  return (
    <Card title={`Images in ${pool}`}>
      {info.isPending ? (
        <p className="text-sm text-zinc-500">Loading…</p>
      ) : info.isError ? (
        <p className="text-sm text-rose-600">{info.error.message}</p>
      ) : !info.data.images.length ? (
        <p className="text-sm text-zinc-500">
          No images. Create a disk, or copy files into <span className="font-mono">{info.data.path}</span>.
        </p>
      ) : (
        <table className="w-full text-sm">
          <thead className="text-left text-xs text-zinc-500">
            <tr>
              <th className="pb-1 font-medium">Name</th>
              <th className="pb-1 font-medium">Format</th>
              <th className="pb-1 text-right font-medium">Size</th>
              <th className="pb-1 text-right font-medium">On disk</th>
              <th className="pb-1 pl-4 font-medium">Used by</th>
            </tr>
          </thead>
          <tbody>
            {info.data.images.map((i) => (
              <tr
                key={i.name}
                onClick={() => navigate(`/storage/${pool}/${encodeURIComponent(i.name)}`)}
                className={`cursor-pointer hover:bg-zinc-50 dark:hover:bg-zinc-800/50 ${selected === i.name ? "bg-zinc-100 dark:bg-zinc-800" : ""}`}
              >
                <td className="py-1 font-mono text-xs">{i.name}</td>
                <td className="py-1 text-xs">{i.format}</td>
                <td className="py-1 text-right text-xs tabular-nums">{formatBytes(i.virtual_size)}</td>
                <td className="py-1 text-right text-xs tabular-nums">{formatBytes(i.actual_size)}</td>
                <td className="py-1 pl-4 text-xs">{i.used_by.join(", ")}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </Card>
  );
}

function CreateDisk({ pool }: { pool: string }) {
  const [name, setName] = useState("");
  const [size, setSize] = useState(20);
  const [format, setFormat] = useState("qcow2");
  const create = useMutation({
    mutationFn: () => api.createImage(name.includes(".") ? name : `${name}.${format}`, size * GiB, format, pool),
    onSuccess: () => setName(""),
  });
  return (
    <Card title={`New disk in ${pool}`}>
      <form
        className="space-y-3"
        onSubmit={(e) => {
          e.preventDefault();
          if (name) create.mutate();
        }}
      >
        <Field label="Name">
          <input className={inputCls} value={name} onChange={(e) => setName(e.target.value)} placeholder="debian.qcow2" />
        </Field>
        <div className="grid grid-cols-2 gap-3">
          <Field label="Size (GiB)">
            <input className={inputCls} type="number" min={1} value={size} onChange={(e) => setSize(Number(e.target.value))} />
          </Field>
          <Field label="Format" hint={format === "qcow2" ? "Thin-provisioned; supports snapshots and clones" : "Fixed layout; fastest"}>
            <select className={inputCls} value={format} onChange={(e) => setFormat(e.target.value)}>
              <option value="qcow2">qcow2</option>
              <option value="raw">raw</option>
            </select>
          </Field>
        </div>
        <Button type="submit" disabled={!name || create.isPending}>
          Create disk
        </Button>
        {create.isSuccess && <p className="text-xs text-zinc-500">Started — see Jobs.</p>}
        {create.isError && <p className="text-xs text-rose-600">{create.error.message}</p>}
      </form>
    </Card>
  );
}

function ImagePanel({ pool, name }: { pool: string; name: string }) {
  const qc = useQueryClient();
  const pools = useQuery({ queryKey: ["pools"], queryFn: api.pools });
  const d = useQuery({ queryKey: ["image", pool, name], queryFn: () => api.image(pool, name) });
  const check = useMutation({ mutationFn: () => api.checkImage(pool, name) });
  const [target, setTarget] = useState({ pool, name: "", format: "qcow2" });
  const convert = useMutation({ mutationFn: () => api.convertImage(pool, name, target.pool, target.name, target.format) });
  const del = useMutation({
    mutationFn: () => api.deleteImage(pool, name),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["pool", pool] });
      qc.invalidateQueries({ queryKey: ["pools"] });
      navigate(`/storage/${pool}`);
    },
  });
  if (d.isPending) return <Card title={name}><p className="text-sm text-zinc-500">Loading…</p></Card>;
  if (d.isError) return <Card title={name}><p className="text-sm text-rose-600">{d.error.message}</p></Card>;
  const x = d.data;
  return (
    <div className="space-y-4">
      <Card title={name} actions={<Button variant="ghost" onClick={() => navigate(`/storage/${pool}`)}>Close</Button>}>
        <dl className="grid grid-cols-[8rem_1fr] gap-y-1 text-sm">
          <dt className="text-zinc-500">Format</dt>
          <dd>{x.format}</dd>
          <dt className="text-zinc-500">Size</dt>
          <dd>
            {formatBytes(x.virtual_size)} <span className="text-xs text-zinc-500">({formatBytes(x.actual_size)} on disk)</span>
          </dd>
          <dt className="text-zinc-500">Used by</dt>
          <dd>{x.used_by.length ? x.used_by.join(", ") : <span className="text-zinc-500">—</span>}</dd>
          {x.in_use_by.length > 0 && (
            <>
              <dt className="text-zinc-500">Open now</dt>
              <dd>
                <Badge tone="green">{x.in_use_by.join(", ")}</Badge>
              </dd>
            </>
          )}
          <dt className="text-zinc-500">Snapshots</dt>
          <dd>{x.snapshots.length ? x.snapshots.join(", ") : <span className="text-zinc-500">none</span>}</dd>
        </dl>
        <h3 className="mt-4 mb-1 text-xs font-semibold text-zinc-600 dark:text-zinc-400">Backing chain</h3>
        {x.backing_chain.length === 0 ? (
          <p className="text-xs text-zinc-500">Standalone image (no backing file).</p>
        ) : (
          <ol className="space-y-1 text-xs">
            <li className="font-mono">{name}</li>
            {x.backing_chain.map((l, i) => (
              <li key={i} className="font-mono" style={{ paddingLeft: `${(i + 1) * 0.75}rem` }}>
                ↳{" "}
                {l.pool && l.name ? (
                  <a className="underline" href={`#/storage/${l.pool}/${encodeURIComponent(l.name)}`}>
                    {l.pool}/{l.name}
                  </a>
                ) : (
                  <span title={l.filename}>{l.filename}</span>
                )}
                {l.format && <span className="text-zinc-500"> ({l.format})</span>}
                {l.missing && <Badge tone="red">missing</Badge>}
                {!l.missing && !l.pool && <Badge tone="amber">outside pools</Badge>}
              </li>
            ))}
          </ol>
        )}
        <div className="mt-4 flex flex-wrap items-center gap-2">
          <Button variant="ghost" onClick={() => check.mutate()} disabled={check.isPending}>
            {check.isPending ? "Checking…" : "Check integrity"}
          </Button>
          {check.data &&
            (check.data.ok ? (
              <span className="text-xs text-emerald-700 dark:text-emerald-400">✓ No errors</span>
            ) : (
              <span className="text-xs text-rose-600">
                {check.data.corruptions} corruptions, {check.data.leaks} leaked clusters
              </span>
            ))}
          {check.isError && <span className="text-xs text-rose-600">{check.error.message}</span>}
        </div>
      </Card>
      <ResizeCard pool={pool} name={name} size={x.virtual_size} format={x.format} openIn={x.in_use_by} />
      <CompactCard pool={pool} name={name} format={x.format} actual={x.actual_size} openIn={x.in_use_by} snapshots={x.snapshots.length} />
      <RenameCard pool={pool} name={name} usedBy={x.used_by} />
      <Card title="Copy / convert">
        <form
          className="space-y-2"
          onSubmit={(e) => {
            e.preventDefault();
            convert.mutate();
          }}
        >
          <div className="grid grid-cols-3 gap-2">
            <Field label="Target pool">
              <select className={inputCls} value={target.pool} onChange={(e) => setTarget({ ...target, pool: e.target.value })}>
                {pools.data?.filter((p) => !p.readonly).map((p) => (
                  <option key={p.name}>{p.name}</option>
                ))}
              </select>
            </Field>
            <Field label="New name">
              <input className={inputCls} value={target.name} onChange={(e) => setTarget({ ...target, name: e.target.value })} />
            </Field>
            <Field label="Format">
              <select className={inputCls} value={target.format} onChange={(e) => setTarget({ ...target, format: e.target.value })}>
                {["qcow2", "raw", "vmdk", "vhdx", "vdi"].map((f) => (
                  <option key={f}>{f}</option>
                ))}
              </select>
            </Field>
          </div>
          <Button type="submit" disabled={!target.name || convert.isPending}>
            Start conversion
          </Button>
          {convert.isSuccess && <span className="ml-2 text-xs text-zinc-500">Started — see Jobs.</span>}
          {convert.isError && <p className="text-xs text-rose-600">{convert.error.message}</p>}
        </form>
      </Card>
      <Card title="Delete">
        <div className="flex items-center gap-2">
          <Button
            variant="danger"
            disabled={del.isPending || x.used_by.length > 0}
            onClick={() => window.confirm(`Delete ${name}? This removes the file.`) && del.mutate()}
          >
            Delete image
          </Button>
          <span className="text-xs text-zinc-500">
            {x.used_by.length ? `Used by ${x.used_by.join(", ")}.` : "Removes the file from the pool."}
          </span>
        </div>
        {del.isError && <p className="mt-1 text-xs text-rose-600">{del.error.message}</p>}
      </Card>
    </div>
  );
}

function ResizeCard({ pool, name, size, format, openIn }: { pool: string; name: string; size: number; format: string; openIn: string[] }) {
  const [gib, setGib] = useState(() => Math.ceil(size / GiB));
  const newSize = Math.round(gib * GiB);
  const shrinking = newSize < size;
  const live = openIn.length > 0;
  const resize = useMutation({ mutationFn: () => api.resizeImage(pool, name, newSize, shrinking) });
  const supported = format === "qcow2" || format === "raw";
  return (
    <Card title="Resize">
      <div className="flex flex-wrap items-end gap-2">
        <Field label={`New size (GiB) — now ${formatBytes(size)}`}>
          <input className={`${inputCls} w-28`} type="number" min={0.001} step={1} value={gib} onChange={(e) => setGib(Number(e.target.value))} />
        </Field>
        <Button
          variant={shrinking ? "danger" : "default"}
          disabled={!supported || resize.isPending || newSize === size || (live && shrinking)}
          onClick={() =>
            (!shrinking || window.confirm(`Shrink ${name}? Data beyond ${formatBytes(newSize)} is lost.`)) && resize.mutate()
          }
        >
          {shrinking ? "Shrink" : "Grow"}
        </Button>
      </div>
      <p className="mt-2 text-xs text-zinc-500">
        {!supported
          ? `Resizing ${format} images is not supported; convert to qcow2 first.`
          : live
            ? `Open in ${openIn.join(", ")}: grows live, and the guest sees the new size at once (extend the partition inside the guest). Shrinking needs the VM stopped.`
            : "The VM is stopped: grows or shrinks the image file. Shrinking cuts off data at the end."}
      </p>
      {resize.isSuccess && <p className="mt-1 text-xs text-zinc-500">Started — see Jobs.</p>}
      {resize.isError && <p className="mt-1 text-xs text-rose-600">{resize.error.message}</p>}
    </Card>
  );
}

function CompactCard({
  pool,
  name,
  format,
  actual,
  openIn,
  snapshots,
}: {
  pool: string;
  name: string;
  format: string;
  actual: number;
  openIn: string[];
  snapshots: number;
}) {
  const [compress, setCompress] = useState(false);
  const estimate = useQuery({
    queryKey: ["measure", pool, name, format],
    queryFn: () => api.measureImage(pool, name, format),
    enabled: format === "qcow2" || format === "raw",
  });
  const compact = useMutation({ mutationFn: () => api.compactImage(pool, name, compress) });
  const blocked = openIn.length
    ? `Open in ${openIn.join(", ")}; stop it first.`
    : snapshots
      ? "Has internal snapshots, which compaction would discard."
      : format !== "qcow2" && format !== "raw"
        ? `Compacting ${format} images is not supported.`
        : null;
  return (
    <Card title="Compact">
      <p className="text-xs text-zinc-500">
        Rewrites the image without unused space, verifies the copy, then replaces the original. Overlays keep their backing file.
      </p>
      <p className="mt-1 text-xs">
        On disk now: {formatBytes(actual)}
        {estimate.data && <> · after compaction about {formatBytes(estimate.data.required)}</>}
      </p>
      <div className="mt-2 flex items-center gap-3">
        {format === "qcow2" && (
          <label className="flex items-center gap-1 text-xs">
            <input type="checkbox" checked={compress} onChange={(e) => setCompress(e.target.checked)} />
            Compress (smaller, slower reads)
          </label>
        )}
        <Button disabled={!!blocked || compact.isPending} onClick={() => compact.mutate()}>
          Compact
        </Button>
      </div>
      {blocked && <p className="mt-1 text-xs text-amber-700 dark:text-amber-400">{blocked}</p>}
      {compact.isSuccess && <p className="mt-1 text-xs text-zinc-500">Started — see Jobs.</p>}
      {compact.isError && <p className="mt-1 text-xs text-rose-600">{compact.error.message}</p>}
    </Card>
  );
}

function RenameCard({ pool, name, usedBy }: { pool: string; name: string; usedBy: string[] }) {
  const qc = useQueryClient();
  const [to, setTo] = useState(name);
  const rename = useMutation({
    mutationFn: () => api.renameImage(pool, name, to),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["pool", pool] });
      navigate(`/storage/${pool}/${encodeURIComponent(to)}`);
    },
  });
  return (
    <Card title="Rename">
      <div className="flex items-end gap-2">
        <Field label="New name">
          <input className={inputCls} value={to} onChange={(e) => setTo(e.target.value)} />
        </Field>
        <Button disabled={usedBy.length > 0 || !to || to === name || rename.isPending} onClick={() => rename.mutate()}>
          Rename
        </Button>
      </div>
      {usedBy.length > 0 && <p className="mt-1 text-xs text-zinc-500">Used by {usedBy.join(", ")}; detach it first.</p>}
      {rename.isError && <p className="mt-1 text-xs text-rose-600">{rename.error.message}</p>}
    </Card>
  );
}
