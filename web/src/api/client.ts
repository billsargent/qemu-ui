// Thin typed client for /api/v1. Types are generated from the Rust structs
// (cargo test -p qemu-ui-core writes them to ./generated).
import type { CapabilitiesResponse } from "./generated/CapabilitiesResponse";
import type { CompileResponse } from "./generated/CompileResponse";
import type { DeviceProperty } from "./generated/DeviceProperty";
import type { LintResult } from "./generated/LintResult";
import type { Diagnostic } from "./generated/Diagnostic";
import type { CheckResult } from "./generated/CheckResult";
import type { ImageDetail } from "./generated/ImageDetail";
import type { Job } from "./generated/Job";
import type { PoolInfo } from "./generated/PoolInfo";
import type { PoolSummary } from "./generated/PoolSummary";
import type { UploadProgress } from "./generated/UploadProgress";
import type { UploadSession } from "./generated/UploadSession";
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

  pools: () => request<PoolSummary[]>("/pools"),
  addPool: (name: string, path: string, quotaBytes: number | null) =>
    post<void>("/pools", { name, path, quota_bytes: quotaBytes }),
  updatePool: (name: string, quotaBytes: number | null, readonly: boolean) =>
    request<void>(`/pools/${encodeURIComponent(name)}`, {
      method: "PUT",
      body: JSON.stringify({ quota_bytes: quotaBytes, readonly }),
    }),
  removePool: (name: string) => request<void>(`/pools/${encodeURIComponent(name)}`, { method: "DELETE" }),
  pool: (name = "default") => request<PoolInfo>(`/pools/${encodeURIComponent(name)}`),
  /** Every image in every pool. */
  allImages: async () => {
    const pools = await request<PoolSummary[]>("/pools");
    const infos = await Promise.all(pools.map((p) => request<PoolInfo>(`/pools/${encodeURIComponent(p.name)}`)));
    return infos.flatMap((i) => i.images);
  },
  image: (pool: string, name: string) =>
    request<ImageDetail>(`/pools/${encodeURIComponent(pool)}/images/${encodeURIComponent(name)}`),
  createImage: (name: string, sizeBytes: number, format = "qcow2", pool = "default") =>
    post<Job>(`/pools/${encodeURIComponent(pool)}/images`, { name, size_bytes: sizeBytes, format }),
  deleteImage: (pool: string, name: string) =>
    request<void>(`/pools/${encodeURIComponent(pool)}/images/${encodeURIComponent(name)}`, { method: "DELETE" }),
  checkImage: (pool: string, name: string) =>
    post<CheckResult>(`/pools/${encodeURIComponent(pool)}/images/${encodeURIComponent(name)}/check`),
  convertImage: (pool: string, name: string, targetPool: string, targetName: string, format: string) =>
    post<Job>(`/pools/${encodeURIComponent(pool)}/images/${encodeURIComponent(name)}/convert`, {
      target_pool: targetPool,
      target_name: targetName,
      format,
    }),

  uploads: () => request<UploadSession[]>("/uploads"),
  createUpload: (pool: string, name: string, sizeBytes: number, clientKey: string) =>
    post<UploadSession>("/uploads", { pool, name, size_bytes: sizeBytes, client_key: clientKey }),
  deleteUpload: (id: string) => request<void>(`/uploads/${id}`, { method: "DELETE" }),
  /** Send one chunk. Resolves with progress, or with `{conflict}` carrying
   * the server's offset when the client is out of sync. */
  uploadChunk: async (
    id: string,
    offset: number,
    chunk: Blob,
    signal?: AbortSignal,
  ): Promise<UploadProgress | { conflict: number }> => {
    const res = await fetch(`/api/v1/uploads/${id}`, {
      method: "PATCH",
      headers: { "upload-offset": String(offset), "content-type": "application/offset+octet-stream" },
      body: chunk,
      signal,
    });
    const body = await res.json().catch(() => ({}));
    if (res.status === 409 && typeof body.offset === "number") return { conflict: body.offset };
    if (!res.ok) throw new ApiError(res.status, body.error ?? res.statusText);
    return body as UploadProgress;
  },

  jobs: () => request<Job[]>("/jobs"),
  cancelJob: (id: string) => post<void>(`/jobs/${id}/cancel`),
  jobLog: async (id: string) => {
    const res = await fetch(`/api/v1/jobs/${id}/log`);
    return res.ok ? res.text() : "";
  },
};

export function wsUrl(path: string): string {
  const proto = window.location.protocol === "https:" ? "wss:" : "ws:";
  return `${proto}//${window.location.host}${path}`;
}
