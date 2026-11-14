import { useState } from "react";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { ApiError, api } from "../api/client";
import type { ForwardChange } from "../api/generated/ForwardChange";
import type { PortForward } from "../api/generated/PortForward";
import type { VmDetail } from "../api/generated/VmDetail";
import { Badge, Button, Card, Field, inputCls } from "./ui";

const LOOPBACK = "127.0.0.1";
const ANY = "0.0.0.0";

const isLoopback = (addr: string) => addr.startsWith("127.") || addr === "::1";
const same = (a: PortForward, b: PortForward) =>
  a.proto === b.proto && a.host_addr === b.host_addr && a.host_port === b.host_port && a.guest_port === b.guest_port;

/**
 * Port forwards on NAT adapters (M3-02). Changes apply to the running VM
 * immediately and are saved for the next start; while stopped they are
 * saved only. Forwards bind to loopback unless deliberately widened.
 */
export function ForwardsCard({ vm }: { vm: VmDetail }) {
  const qc = useQueryClient();
  const id = vm.summary.id;
  const running = vm.live_spec != null;
  const nics = vm.spec.nics.map((n, i) => ({ n, i })).filter(({ n }) => n.backend.kind === "user");
  const liveForwards = (i: number): PortForward[] => {
    const b = vm.live_spec?.nics[i]?.backend;
    return b && b.kind === "user" ? b.forwards : [];
  };

  const [note, setNote] = useState<string | null>(null);
  const done = (c: ForwardChange) => {
    setNote(c.message);
    qc.invalidateQueries({ queryKey: ["vm", id] });
    qc.invalidateQueries({ queryKey: ["vms"] });
  };
  const remove = useMutation({
    mutationFn: ({ i, j }: { i: number; j: number }) => api.removeForward(id, i, j),
    onSuccess: done,
  });
  const [form, setForm] = useState({
    nic: nics[0]?.i ?? 0,
    proto: "tcp",
    hostPort: "",
    guestPort: "",
    exposed: false,
  });
  const add = useMutation({
    mutationFn: () =>
      api.addForward(id, form.nic, {
        proto: form.proto,
        host_addr: form.exposed ? ANY : LOOPBACK,
        host_port: Number(form.hostPort),
        guest_port: Number(form.guestPort),
      }),
    onSuccess: (c) => {
      done(c);
      setForm({ ...form, hostPort: "", guestPort: "", exposed: false });
    },
  });
  const error = add.error ?? remove.error;
  const busy = add.isPending || remove.isPending;
  const port = (s: string) => /^\d+$/.test(s) && Number(s) > 0 && Number(s) < 65536;

  if (nics.length === 0) return null;
  return (
    <Card title={running ? "Port forwards (live)" : "Port forwards"}>
      <ul className="divide-y divide-zinc-100 text-sm dark:divide-zinc-800">
        {nics.flatMap(({ n, i }) =>
          n.backend.kind !== "user"
            ? []
            : n.backend.forwards.map((f, j) => {
                const live = liveForwards(i).some((g) => same(f, g));
                return (
                  <li key={`${i}-${j}`} className="flex flex-wrap items-center gap-2 py-1.5">
                    <span className="font-mono">
                      {f.host_addr}:{f.host_port}
                    </span>
                    <span className="text-zinc-400">→</span>
                    <span className="font-mono">guest :{f.guest_port}</span>
                    <span className="text-xs text-zinc-500">{f.proto}</span>
                    {nics.length > 1 && <span className="text-xs text-zinc-500">NIC {i}</span>}
                    {!isLoopback(f.host_addr) && (
                      <Badge tone="amber" title="Reachable from other machines on the network">
                        exposed
                      </Badge>
                    )}
                    {running && !live && (
                      <Badge tone="zinc" title="Saved; applies at the next start">
                        next start
                      </Badge>
                    )}
                    <div className="ml-auto">
                      <Button
                        variant="ghost"
                        disabled={busy}
                        title={running ? "Stop forwarding now (and at later starts)" : "Remove from the configuration"}
                        onClick={() => remove.mutate({ i, j })}
                      >
                        Remove
                      </Button>
                    </div>
                  </li>
                );
              }),
        )}
        {nics.every(({ n }) => n.backend.kind !== "user" || n.backend.forwards.length === 0) && (
          <li className="py-1.5 text-zinc-500">No forwards. The guest can reach out, but nothing reaches in.</li>
        )}
      </ul>

      <form
        className="mt-3 flex flex-wrap items-end gap-2 border-t border-zinc-200 pt-3 dark:border-zinc-800"
        onSubmit={(e) => {
          e.preventDefault();
          if (port(form.hostPort) && port(form.guestPort)) add.mutate();
        }}
      >
        {nics.length > 1 && (
          <Field label="Adapter">
            <select className={inputCls} value={form.nic} onChange={(e) => setForm({ ...form, nic: Number(e.target.value) })}>
              {nics.map(({ n, i }) => (
                <option key={i} value={i}>
                  NIC {i} · {n.model}
                </option>
              ))}
            </select>
          </Field>
        )}
        <Field label="Protocol">
          <select className={inputCls} value={form.proto} onChange={(e) => setForm({ ...form, proto: e.target.value })}>
            <option value="tcp">TCP</option>
            <option value="udp">UDP</option>
          </select>
        </Field>
        <Field label="Host port">
          <input
            className={`${inputCls} w-24`}
            inputMode="numeric"
            placeholder="2222"
            value={form.hostPort}
            onChange={(e) => setForm({ ...form, hostPort: e.target.value.trim() })}
          />
        </Field>
        <Field label="Guest port">
          <input
            className={`${inputCls} w-24`}
            inputMode="numeric"
            placeholder="22"
            value={form.guestPort}
            onChange={(e) => setForm({ ...form, guestPort: e.target.value.trim() })}
          />
        </Field>
        <label
          className="flex items-center gap-1 pb-2 text-xs text-zinc-600 dark:text-zinc-400"
          title="Bind on all interfaces instead of loopback: other machines on your network can connect"
        >
          <input type="checkbox" checked={form.exposed} onChange={(e) => setForm({ ...form, exposed: e.target.checked })} />
          Reachable from other machines
        </label>
        <Button type="submit" disabled={busy || !port(form.hostPort) || !port(form.guestPort)}>
          {running ? "Forward now" : "Add forward"}
        </Button>
      </form>
      {form.exposed && (
        <p className="mt-2 text-xs text-amber-700 dark:text-amber-400">
          Anyone who can reach this computer on port {form.hostPort || "…"} will reach the guest.
        </p>
      )}
      {note && <p className="mt-2 text-xs text-zinc-500">{note}</p>}
      {error && (
        <p className="mt-2 text-xs text-rose-600">
          {error instanceof ApiError && error.diagnostics.length ? error.diagnostics[0].message : error.message}
        </p>
      )}
    </Card>
  );
}
