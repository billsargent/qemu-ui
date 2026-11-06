// Thin typed client for /api/v1. Types are generated from the Rust structs
// (cargo test -p qemu-ui-core writes them to ./generated).
import type { CapabilitiesResponse } from "./generated/CapabilitiesResponse";
import type { CompileResponse } from "./generated/CompileResponse";
import type { DeviceProperty } from "./generated/DeviceProperty";
import type { LintResult } from "./generated/LintResult";
import type { Diagnostic } from "./generated/Diagnostic";
import type { PoolInfo } from "./generated/PoolInfo";
import type { ScanStatus } from "./generated/ScanStatus";
import type { VmDetail } from "./generated/VmDetail";
import type { VmEvent } from "./generated/VmEvent";
import type { VmSpec } from "./generated/VmSpec";
import type { VmSummary } from "./generated/VmSummary";

export class ApiError extends Error {
  constructor(
    public status: number,
    message: string,
    public diagnostics: Diagnostic[] = [],
  ) {
    super(message);
  }
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(`/api/v1${path}`, {
    ...init,
    headers: init?.body ? { "content-type": "application/json", ...init?.headers } : init?.headers,
  });
  if (!res.ok) {
    let msg = res.statusText;
    let diagnostics: Diagnostic[] = [];
    try {
      const body = await res.json();
      msg = body.error ?? msg;
      diagnostics = body.diagnostics ?? [];
    } catch {
      /* not JSON */
    }
    throw new ApiError(res.status, msg, diagnostics);
  }
  if (res.status === 204 || res.status === 201) return undefined as T;
  const text = await res.text();
  return (text ? JSON.parse(text) : undefined) as T;
}

const post = <T>(path: string, body?: unknown) =>
  request<T>(path, { method: "POST", body: body === undefined ? undefined : JSON.stringify(body) });

export type VmAction = "start" | "stop" | "force-stop" | "pause" | "resume" | "reset";

export interface ConsoleTicket {
  ticket: string;
  protocol: string;
  vnc_password: string | null;
}

export const api = {
  capabilities: () => request<CapabilitiesResponse>("/host/capabilities"),
  rescan: (force: boolean) => post<ScanStatus>(`/host/rescan?force=${force}`),
  deviceProperties: (binaryId: string, device: string) =>
    request<DeviceProperty[]>(
      `/host/binaries/${encodeURIComponent(binaryId)}/devices/${encodeURIComponent(device)}/properties`,
    ),

  vms: () => request<VmSummary[]>("/vms"),
  vm: (id: string) => request<VmDetail>(`/vms/${id}`),
  vmEvents: (id: string) => request<VmEvent[]>(`/vms/${id}/events`),
  createVm: (spec: VmSpec) => post<VmDetail>("/vms", { spec }),
  updateVm: (id: string, spec: VmSpec) =>
    request<VmDetail>(`/vms/${id}`, { method: "PUT", body: JSON.stringify({ spec }) }),
  deleteVm: (id: string) => request<void>(`/vms/${id}`, { method: "DELETE" }),
  action: (id: string, action: VmAction) => post<VmSummary>(`/vms/${id}/actions/${action}`),
  defaults: (arch: string, name: string) =>
    request<{ spec: VmSpec; diagnostics: Diagnostic[] }>(
      `/vms/defaults?arch=${encodeURIComponent(arch)}&name=${encodeURIComponent(name)}`,
    ),
  compile: (spec: VmSpec, vmId?: string) => post<CompileResponse>("/vms/compile", { spec, vm_id: vmId }),
  lint: (spec: VmSpec, vmId?: string) => post<LintResult>("/vms/lint", { spec, vm_id: vmId }),
  serialLog: async (id: string) => {
    const res = await fetch(`/api/v1/vms/${id}/serial-log`);
    return res.ok ? res.text() : "";
  },
  consoleTicket: (id: string, kind: "vnc" | "serial") => post<ConsoleTicket>(`/vms/${id}/consoles/${kind}/ticket`),

  pool: (name = "default") => request<PoolInfo>(`/pools/${name}`),
  createImage: (name: string, sizeBytes: number, format = "qcow2") =>
    post<void>("/pools/default/images", { name, size_bytes: sizeBytes, format }),
};

export function wsUrl(path: string): string {
  const proto = window.location.protocol === "https:" ? "wss:" : "ws:";
  return `${proto}//${window.location.host}${path}`;
}
