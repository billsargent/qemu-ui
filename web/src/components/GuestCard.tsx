// Guest integration (M7-02/03): the guest-agent channel and the cloud-init
// seed in the editor, and what the running guest's agent reports.
import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { api } from "../api/client";
import type { CloudInit } from "../api/generated/CloudInit";
import type { Diagnostic } from "../api/generated/Diagnostic";
import type { GuestSpec } from "../api/generated/GuestSpec";
import type { VmDetail } from "../api/generated/VmDetail";
import { Badge, Card, Field, inputCls } from "./ui";

const isHash = (p: string) => /^\$(6|5|y|2b|2y)\$/.test(p);

/** Editor card: guest agent + cloud-init. */
export function GuestCard({
  guest,
  onChange,
  diags,
}: {
  guest: GuestSpec;
  onChange: (g: GuestSpec) => void;
  diags: Diagnostic[];
}) {
  const ci = guest.cloud_init ?? null;
  const setCi = (patch: Partial<CloudInit>) => onChange({ ...guest, cloud_init: { ...(ci as CloudInit), ...patch } });
  const [raw, setRaw] = useState(ci?.user_data != null);
  const stored = ci?.password != null && isHash(ci.password);

  return (
    <Card title="Guest integration">
      <div className="space-y-3 text-xs">
        <label className="flex items-center gap-2" title="Needs qemu-guest-agent installed in the guest">
          <input type="checkbox" checked={guest.agent} onChange={(e) => onChange({ ...guest, agent: e.target.checked })} />
          QEMU guest agent channel: guest addresses, clean shutdown, consistent live snapshots
        </label>
        <label className="flex items-center gap-2" title="For cloud images (Ubuntu, Debian, Fedora cloud…): configures them on first boot">
          <input
            type="checkbox"
            checked={ci != null}
            onChange={(e) =>
              onChange({ ...guest, cloud_init: e.target.checked ? { hostname: null, user: null, password: null, ssh_keys: [], user_data: null } : null })
            }
          />
          cloud-init seed (NoCloud CD-ROM, rebuilt at each start)
        </label>
        {ci && (
          <div className="space-y-3 border-l-2 border-gray-200 pl-3">
            <div className="grid grid-cols-2 gap-3">
              <Field label="Host name">
                <input className={inputCls} placeholder="the VM name" value={ci.hostname ?? ""} onChange={(e) => setCi({ hostname: e.target.value || null })} />
              </Field>
              <label className="flex items-end gap-2 pb-1">
                <input type="checkbox" checked={raw} onChange={(e) => {
                  setRaw(e.target.checked);
                  if (!e.target.checked) setCi({ user_data: null });
                }} />
                Write my own user-data
              </label>
            </div>
            {raw ? (
              <Field label="user-data (#cloud-config or a #! script)">
                <textarea
                  className={`${inputCls} h-40 font-mono`}
                  spellCheck={false}
                  placeholder={"#cloud-config\npackages: [nginx]"}
                  value={ci.user_data ?? ""}
                  onChange={(e) => setCi({ user_data: e.target.value || null })}
                />
              </Field>
            ) : (
              <>
                <div className="grid grid-cols-2 gap-3">
                  <Field label="User">
                    <input className={inputCls} placeholder="the image's default user" value={ci.user ?? ""} onChange={(e) => setCi({ user: e.target.value || null })} />
                  </Field>
                  <Field label={stored ? "Password (set; type to replace)" : "Password"}>
                    <input
                      className={inputCls}
                      type="password"
                      autoComplete="new-password"
                      placeholder={stored ? "••••••••" : "none: SSH keys only"}
                      value={ci.password != null && !stored ? ci.password : ""}
                      onChange={(e) => setCi({ password: e.target.value || (stored ? ci.password : null) })}
                    />
                  </Field>
                </div>
                {stored && (
                  <button type="button" className="text-red-700 underline" onClick={() => setCi({ password: null })}>
                    Remove the password
                  </button>
                )}
                <Field label="SSH public keys (one per line)">
                  <textarea
                    className={`${inputCls} h-20 font-mono`}
                    spellCheck={false}
                    placeholder="ssh-ed25519 AAAA… you@laptop"
                    value={ci.ssh_keys.join("\n")}
                    onChange={(e) => setCi({ ssh_keys: e.target.value.split("\n").filter((l) => l.trim() !== "") })}
                  />
                </Field>
                <p className="text-gray-500">Passwords are stored only as a SHA-512-crypt hash. The user gets passwordless sudo.</p>
              </>
            )}
          </div>
        )}
        {diags.length > 0 && (
          <ul className="space-y-1">
            {diags.map((d, i) => (
              <li key={i} className={d.severity === "error" ? "text-red-700" : "text-amber-700"}>
                {d.field}: {d.message}
              </li>
            ))}
          </ul>
        )}
      </div>
    </Card>
  );
}

/** VM page card: what the guest agent reports. */
export function GuestInfoCard({ vm }: { vm: VmDetail }) {
  const spec = vm.live_spec ?? vm.spec;
  const running = vm.summary.state === "running" || vm.summary.state === "paused";
  const q = useQuery({
    queryKey: ["guest", vm.summary.id],
    queryFn: () => api.guestInfo(vm.summary.id),
    enabled: running && spec.guest.agent,
    refetchInterval: 15_000,
  });
  if (!spec.guest.agent || !running) return null;
  const g = q.data;
  return (
    <Card title="Guest">
      {!g ? (
        <p className="text-xs text-gray-500">Asking the guest agent…</p>
      ) : !g.connected ? (
        <p className="text-xs text-gray-600">
          <Badge tone="zinc">agent not answering</Badge> {g.error}
        </p>
      ) : (
        <div className="space-y-2 text-xs">
          <div className="flex flex-wrap items-center gap-2">
            <Badge tone="green">agent connected</Badge>
            {g.hostname && <span className="font-mono">{g.hostname}</span>}
            {g.os_name && <span>{g.os_name}</span>}
            {g.kernel && <span className="text-gray-500">kernel {g.kernel}</span>}
            {g.fsfreeze === "frozen" && <Badge tone="amber">file systems frozen</Badge>}
          </div>
          {g.interfaces.length > 0 && (
            <table className="w-full">
              <tbody>
                {g.interfaces.map((i) => (
                  <tr key={i.name} className="border-t border-gray-100">
                    <td className="py-1 pr-3 font-mono">{i.name}</td>
                    <td className="py-1 pr-3 font-mono text-gray-500">{i.mac}</td>
                    <td className="py-1 font-mono">{i.addresses.join(", ")}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </div>
      )}
    </Card>
  );
}
