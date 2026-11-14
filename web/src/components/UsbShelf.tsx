// USB passthrough shelf (M2-12): host devices on one side, the VM's
// remembered devices on the other. Attaching to a running VM hot-plugs the
// device; rules persist in the VM's spec, and QEMU attaches the device
// whenever it is plugged in.
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "../api/client";
import type { UsbDevice } from "../api/generated/UsbDevice";
import type { UsbPassthrough } from "../api/generated/UsbPassthrough";
import type { VmDetail } from "../api/generated/VmDetail";
import { Badge, Button, Card } from "./ui";

function label(d: UsbDevice): string {
  if (d.product && d.manufacturer && !d.product.includes(d.manufacturer)) return `${d.manufacturer} ${d.product}`;
  return d.product ?? `${d.vendor_id}:${d.product_id}`;
}

function matches(d: UsbDevice, r: UsbPassthrough): boolean {
  return (
    d.vendor_id === r.vendor_id.toLowerCase() &&
    d.product_id === r.product_id.toLowerCase() &&
    (r.host_bus == null || d.bus === r.host_bus) &&
    (r.host_port == null || d.port === r.host_port)
  );
}

export function UsbShelf({ vm }: { vm: VmDetail }) {
  const qc = useQueryClient();
  const host = useQuery({ queryKey: ["host-usb"], queryFn: api.hostUsb, refetchInterval: 10000 });
  const running = vm.summary.state === "running" || vm.summary.state === "paused";
  const refresh = () => {
    qc.invalidateQueries({ queryKey: ["vm", vm.summary.id] });
    qc.invalidateQueries({ queryKey: ["vms"] });
  };
  const attach = useMutation({
    mutationFn: (d: UsbDevice) =>
      api.attachUsb(vm.summary.id, {
        vendor_id: d.vendor_id,
        product_id: d.product_id,
        // On Linux, pin to the physical port so identical devices stay apart.
        host_bus: d.bus,
        host_port: d.port,
        label: label(d),
      }),
    onSuccess: refresh,
  });
  const detach = useMutation({ mutationFn: (i: number) => api.detachUsb(vm.summary.id, i), onSuccess: refresh });
  const rules = vm.spec.usb;
  const devices = host.data?.devices ?? [];
  const err = attach.error ?? detach.error;

  return (
    <Card
      title="USB devices"
      actions={
        <Button variant="ghost" onClick={() => host.refetch()} disabled={host.isFetching}>
          Rescan
        </Button>
      }
    >
      {host.data && !host.data.qemu_supported && (
        <p className="mb-3 text-xs text-rose-600">
          The installed QEMU was built without USB passthrough (usb-host / libusb), so host devices cannot be given to
          VMs.
        </p>
      )}
      <div className="grid grid-cols-1 gap-4 lg:grid-cols-2">
        <div>
          <h3 className="mb-2 text-xs font-semibold text-zinc-600 dark:text-zinc-400">Given to this VM</h3>
          {rules.length === 0 ? (
            <p className="text-xs text-zinc-500">None. Attach a device from the host list.</p>
          ) : (
            <ul className="space-y-1">
              {rules.map((r, i) => {
                const present = devices.some((d) => matches(d, r));
                return (
                  <li key={`${r.vendor_id}:${r.product_id}:${i}`} className="flex items-center gap-2 text-sm">
                    <span className="truncate">{r.label ?? `${r.vendor_id}:${r.product_id}`}</span>
                    <span className="font-mono text-xs text-zinc-500">
                      {r.vendor_id}:{r.product_id}
                      {r.host_port ? ` @ ${r.host_bus ?? "?"}-${r.host_port}` : ""}
                    </span>
                    {present ? (
                      <Badge tone="green" title="Plugged into the host">present</Badge>
                    ) : (
                      <Badge title="QEMU attaches it as soon as it is plugged in">waiting for device</Badge>
                    )}
                    <Button
                      variant="ghost"
                      disabled={detach.isPending}
                      onClick={() => detach.mutate(i)}
                      title={running ? "Unplug from the running VM and forget it" : "Forget this device"}
                    >
                      Detach
                    </Button>
                  </li>
                );
              })}
            </ul>
          )}
        </div>
        <div>
          <h3 className="mb-2 text-xs font-semibold text-zinc-600 dark:text-zinc-400">On this host</h3>
          {host.isPending ? (
            <p className="text-xs text-zinc-500">Looking for USB devices…</p>
          ) : host.data?.error ? (
            <p className="text-xs text-rose-600">{host.data.error}</p>
          ) : devices.length === 0 ? (
            <p className="text-xs text-zinc-500">No USB devices found.</p>
          ) : (
            <ul className="space-y-2">
              {devices.map((d, i) => {
                const taken = rules.some((r) => matches(d, r));
                return (
                  <li key={`${d.vendor_id}:${d.product_id}:${d.bus ?? ""}:${d.port ?? i}`} className="text-sm">
                    <div className="flex items-center gap-2">
                      <span className="truncate">{label(d)}</span>
                      <span className="font-mono text-xs text-zinc-500">
                        {d.vendor_id}:{d.product_id}
                        {d.port ? ` @ ${d.bus}-${d.port}` : ""}
                      </span>
                      {d.usability === "ready" && <Badge tone="green">ready</Badge>}
                      {d.usability === "needs-setup" && <Badge tone="amber">needs setup</Badge>}
                      {d.usability === "unsupported" && <Badge>not passable</Badge>}
                      <span className="ml-auto">
                        <Button
                          variant="ghost"
                          disabled={taken || d.usability === "unsupported" || attach.isPending || !host.data?.qemu_supported}
                          title={running ? "Hot-plug into the running VM" : "Give to this VM when it starts"}
                          onClick={() => attach.mutate(d)}
                        >
                          {taken ? "Attached" : "Attach"}
                        </Button>
                      </span>
                    </div>
                    {d.note && <p className="mt-0.5 text-xs text-zinc-500">{d.note}</p>}
                  </li>
                );
              })}
            </ul>
          )}
        </div>
      </div>
      <p className="mt-3 text-xs text-zinc-500">
        A passed-through device belongs to the VM while it is attached; the host cannot use it at the same time.
        {running ? " Changes apply immediately." : " Attached devices connect when the VM starts."}
      </p>
      {err && <p className="mt-1 text-xs text-rose-600">{err.message}</p>}
    </Card>
  );
}
