// "Connect to guest" cards (M3-08): the guest's addresses per NIC, and for
// common services (SSH, RDP, HTTP, HTTPS) either how to reach them through
// an existing port forward or a one-click forward on a free loopback port.
import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "../api/client";
import type { ActiveForward } from "../api/generated/ActiveForward";
import type { GuestAddr } from "../api/generated/GuestAddr";
import type { PortForward } from "../api/generated/PortForward";
import type { VmDetail } from "../api/generated/VmDetail";
import type { VmSpec } from "../api/generated/VmSpec";
import { Badge, Button, Card } from "./ui";

type Service = {
  name: string;
  guestPorts: number[];
  /** Preferred host port for a new forward. */
  hostPort: number;
  connect: (host: string, port: number) => { text: string; href?: string };
};

const SERVICES: Service[] = [
  { name: "SSH", guestPorts: [22], hostPort: 2222, connect: (h, p) => ({ text: `ssh -p ${p} user@${h}` }) },
  { name: "RDP", guestPorts: [3389], hostPort: 33389, connect: (h, p) => ({ text: `${h}:${p}` }) },
  {
    name: "HTTP",
    guestPorts: [80, 8080],
    hostPort: 8080,
    connect: (h, p) => ({ text: `http://${h}:${p}/`, href: `http://${h}:${p}/` }),
  },
  {
    name: "HTTPS",
    guestPorts: [443],
    hostPort: 8443,
    connect: (h, p) => ({ text: `https://${h}:${p}/`, href: `https://${h}:${p}/` }),
  },
];

type Fwd = PortForward & { nic: number };

function forwardsOf(spec: VmSpec): Fwd[] {
  return spec.nics.flatMap((n, nic) => (n.backend.kind === "user" ? n.backend.forwards.map((f) => ({ ...f, nic })) : []));
}

/** Where to connect for a forward bound to `addr`. */
function connectHost(addr: string): string {
  if (addr === "0.0.0.0" || addr === "::") return window.location.hostname || "127.0.0.1";
  return addr.includes(":") ? `[${addr}]` : addr;
}

/** A free host port near `want`, not used by any forward of this VM. */
function freePort(want: number, used: Set<number>): number {
  let p = want;
  while (used.has(p) && p < 65535) p++;
  return p;
}

const SOURCE: Record<GuestAddr["source"], string> = {
  "guest-agent": "guest agent",
  nat: "NAT",
  neighbor: "host neighbour table",
};

export function ConnectCards({ vm }: { vm: VmDetail }) {
  const qc = useQueryClient();
  const id = vm.summary.id;
  const running = vm.summary.state === "running" || vm.summary.state === "paused";
  const net = useQuery({
    queryKey: ["guest-net", id, vm.summary.state],
    queryFn: () => api.guestNet(id),
    refetchInterval: running ? 10000 : false,
  });
  const [copied, setCopied] = useState<string | null>(null);
  const add = useMutation({
    mutationFn: ({ nic, fwd }: { nic: number; fwd: PortForward }) => api.addForward(vm, nic, fwd),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["vm", id] });
      qc.invalidateQueries({ queryKey: ["guest-net", id] });
    },
  });

  const saved = forwardsOf(vm.spec);
  const active: ActiveForward[] = net.data?.nics.flatMap((n) => n.active_forwards) ?? [];
  const isActive = (f: PortForward) =>
    active.some((a) => a.proto === f.proto && a.host_port === f.host_port && a.guest_port === f.guest_port);
  const natNic = vm.spec.nics.findIndex((n) => n.backend.kind === "user");
  const used = new Set(saved.map((f) => f.host_port));

  const copy = async (text: string) => {
    try {
      await navigator.clipboard.writeText(text);
      setCopied(text);
      setTimeout(() => setCopied(null), 1500);
    } catch {
      /* clipboard unavailable (non-secure context) */
    }
  };

  return (
    <Card title="Connect to guest">
      {vm.spec.nics.length === 0 ? (
        <p className="text-sm text-zinc-500">This VM has no network.</p>
      ) : (
        <div className="space-y-4">
          <div>
            <h3 className="mb-1 text-xs font-medium text-zinc-600 dark:text-zinc-400">Guest addresses</h3>
            {!running ? (
              <p className="text-sm text-zinc-500">Start the VM to detect its addresses.</p>
            ) : (
              <ul className="space-y-1 text-sm">
                {net.data?.nics.map((n) => (
                  <li key={n.nic} className="flex flex-wrap items-center gap-2">
                    <span className="text-zinc-500">NIC {n.nic}</span>
                    {n.addresses.length === 0 && <span className="text-zinc-500">not detected</span>}
                    {n.addresses.slice(0, 3).map((a) => (
                      <span key={a.ip} className="inline-flex items-center gap-1" title={a.label}>
                        <span className="font-mono">{a.ip}</span>
                        <Badge tone={a.confirmed ? "green" : "zinc"}>{SOURCE[a.source]}</Badge>
                      </span>
                    ))}
                  </li>
                ))}
                {net.data?.nics.some((n) => n.addresses.some((a) => a.source === "nat")) && (
                  <li className="text-xs text-zinc-500">
                    NAT addresses are internal to QEMU; reach the guest through port forwards.
                  </li>
                )}
                {net.data?.agent_error && <li className="text-xs text-amber-600">{net.data.agent_error}</li>}
              </ul>
            )}
          </div>

          <div className="grid grid-cols-1 gap-2 sm:grid-cols-2">
            {SERVICES.map((svc) => {
              const fwd = saved.find((f) => f.proto === "tcp" && svc.guestPorts.includes(f.guest_port));
              if (fwd) {
                const c = svc.connect(connectHost(fwd.host_addr), fwd.host_port);
                const pending = running && net.data && !isActive(fwd);
                return (
                  <div key={svc.name} className="rounded border border-zinc-200 p-2 dark:border-zinc-800">
                    <div className="mb-1 flex items-center gap-2 text-xs">
                      <span className="font-semibold">{svc.name}</span>
                      <span className="text-zinc-500">
                        {fwd.host_addr}:{fwd.host_port} → guest {fwd.guest_port}
                      </span>
                      {pending && <Badge tone="amber">applies at next start</Badge>}
                    </div>
                    <div className="flex items-center gap-2">
                      {c.href ? (
                        <a className="font-mono text-xs text-sky-700 underline dark:text-sky-400" href={c.href} target="_blank" rel="noreferrer">
                          {c.text}
                        </a>
                      ) : (
                        <code className="text-xs">{c.text}</code>
                      )}
                      <Button variant="ghost" onClick={() => copy(c.text)}>
                        {copied === c.text ? "Copied" : "Copy"}
                      </Button>
                    </div>
                  </div>
                );
              }
              const port = freePort(svc.hostPort, used);
              return (
                <div key={svc.name} className="rounded border border-dashed border-zinc-300 p-2 dark:border-zinc-700">
                  <div className="mb-1 text-xs">
                    <span className="font-semibold">{svc.name}</span>{" "}
                    <span className="text-zinc-500">no forward to guest port {svc.guestPorts[0]}</span>
                  </div>
                  {natNic < 0 ? (
                    <p className="text-xs text-zinc-500">Forwards need a NAT network.</p>
                  ) : (
                    <Button
                      variant="ghost"
                      disabled={add.isPending}
                      title={running ? "Saved now; takes effect at the next start" : undefined}
                      onClick={() =>
                        add.mutate({
                          nic: natNic,
                          fwd: { proto: "tcp", host_addr: "127.0.0.1", host_port: port, guest_port: svc.guestPorts[0] },
                        })
                      }
                    >
                      Add forward 127.0.0.1:{port} → {svc.guestPorts[0]}
                    </Button>
                  )}
                </div>
              );
            })}
          </div>
          {add.error && <p className="text-xs text-rose-600">{String(add.error.message)}</p>}
        </div>
      )}
    </Card>
  );
}
