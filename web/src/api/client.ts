// Thin typed client for /api/v1. Types are generated from the Rust structs
// (cargo test -p qemu-ui-core writes them to ./generated).
import type { CapabilitiesResponse } from "./generated/CapabilitiesResponse";
import type { CompileResponse } from "./generated/CompileResponse";
import type { DeviceProperty } from "./generated/DeviceProperty";
import type { DiskBusSupport } from "./generated/DiskBusSupport";
import type { DiskChange } from "./generated/DiskChange";
import type { DiskSpec } from "./generated/DiskSpec";
import type { ForwardChange } from "./generated/ForwardChange";
import type { PortForward } from "./generated/PortForward";
import type { LintResult } from "./generated/LintResult";
import type { Diagnostic } from "./generated/Diagnostic";
import type { CheckResult } from "./generated/CheckResult";
import type { GuestNetInfo } from "./generated/GuestNetInfo";
import type { HostDisk } from "./generated/HostDisk";
import type { ImageDetail } from "./generated/ImageDetail";
import type { Job } from "./generated/Job";
import type { MeasureResult } from "./generated/MeasureResult";
import type { NicModel } from "./generated/NicModel";
import type { PoolInfo } from "./generated/PoolInfo";
import type { PoolSummary } from "./generated/PoolSummary";
import type { HostUsb } from "./generated/HostUsb";
import type { UploadProgress } from "./generated/UploadProgress";
import type { UsbPassthrough } from "./generated/UsbPassthrough";
import type { UploadSession } from "./generated/UploadSession";
import type { ScanStatus } from "./generated/ScanStatus";
import type { ShareRoot } from "./generated/ShareRoot";
import type { SharesInfo } from "./generated/SharesInfo";
import type { Network } from "./generated/Network";
import type { NetworkConfig } from "./generated/NetworkConfig";
import type { NetworkInfo } from "./generated/NetworkInfo";
import type { HostNetInfo } from "./generated/HostNetInfo";
import type { SnapshotList } from "./generated/SnapshotList";
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
  /** Disk-bus support for a machine; `null` when it could not be probed. */
  diskBuses: (arch: string, machine: string) =>
    request<DiskBusSupport[] | null>(
      `/host/buses?arch=${encodeURIComponent(arch)}&machine=${encodeURIComponent(machine)}`,
    ),
  /** NIC models for a machine, recommended first (M3-07). */
  nicModels: (arch: string, machine: string) =>
    request<NicModel[]>(`/host/nic-models?arch=${encodeURIComponent(arch)}&machine=${encodeURIComponent(machine)}`),
  /** Add a disk; hotplugged when the VM runs. */
  addDisk: (id: string, disk: DiskSpec) => post<DiskChange>(`/vms/${id}/disks`, { disk }),
  /** Remove disk `n` (its `qui-disk<n>` index); hot-unplugged when running. */
  removeDisk: (id: string, n: number, force = false) =>
    request<DiskChange>(`/vms/${id}/disks/${n}?force=${force}`, { method: "DELETE" }),
  changeMedia: (id: string, n: number, pool: string, name: string) =>
    post<DiskChange>(`/vms/${id}/disks/${n}/media`, { image: { pool, name } }),
  /** Add a port forward to NIC `i`; applied live when the VM runs. */
  addForward: (id: string, i: number, forward: PortForward) =>
    post<ForwardChange>(`/vms/${id}/nics/${i}/forwards`, { forward }),
  /** Remove forward `j` of NIC `i` (positions in the saved spec). */
  removeForward: (id: string, i: number, j: number) =>
    request<ForwardChange>(`/vms/${id}/nics/${i}/forwards/${j}`, { method: "DELETE" }),
  serialLog: async (id: string) => {
    const res = await fetch(`/api/v1/vms/${id}/serial-log`);
    return res.ok ? res.text() : "";
  },
  consoleTicket: (id: string, kind: "vnc" | "serial") => post<ConsoleTicket>(`/vms/${id}/consoles/${kind}/ticket`),

  networks: () => request<NetworkInfo[]>("/networks"),
  /** Host support for tap-backed networks: helper bridges (Linux), TAP adapters (Windows). */
  hostNet: () => request<HostNetInfo>("/host/net-adapters"),
  createNetwork: (name: string, config: NetworkConfig) => post<Network>("/networks", { name, config }),
  updateNetwork: (id: string, name: string, config: NetworkConfig) =>
    request<Network>(`/networks/${id}`, { method: "PUT", body: JSON.stringify({ name, config }) }),
  deleteNetwork: (id: string) => request<void>(`/networks/${id}`, { method: "DELETE" }),
  shares: () => request<SharesInfo>("/shares"),
  addShareRoot: (name: string, path: string) => post<ShareRoot>("/shares/roots", { name, path }),
  removeShareRoot: (name: string) =>
    request<void>(`/shares/roots/${encodeURIComponent(name)}`, { method: "DELETE" }),
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
  /** Physical disks the privileged helper can export (M2-11). */
  hostDisks: () =>
    request<{ available: boolean; reason: string | null; disks: HostDisk[] }>("/host/disks"),
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

  measureImage: (pool: string, name: string, format: string) =>
    post<MeasureResult>(`/pools/${encodeURIComponent(pool)}/images/${encodeURIComponent(name)}/measure`, { format }),
  resizeImage: (pool: string, name: string, sizeBytes: number, shrink: boolean) =>
    post<Job>(`/pools/${encodeURIComponent(pool)}/images/${encodeURIComponent(name)}/resize`, {
      size_bytes: sizeBytes,
      shrink,
    }),
  compactImage: (pool: string, name: string, compress: boolean) =>
    post<Job>(`/pools/${encodeURIComponent(pool)}/images/${encodeURIComponent(name)}/compact`, { compress }),
  renameImage: (pool: string, name: string, newName: string) =>
    post<void>(`/pools/${encodeURIComponent(pool)}/images/${encodeURIComponent(name)}/rename`, { new_name: newName }),
  setTemplate: (pool: string, name: string, template: boolean) =>
    request<void>(`/pools/${encodeURIComponent(pool)}/images/${encodeURIComponent(name)}/template`, {
      method: "PUT",
      body: JSON.stringify({ template }),
    }),
  cloneImage: (pool: string, name: string, targetPool: string, targetName: string) =>
    post<{ pool: string; name: string }>(`/pools/${encodeURIComponent(pool)}/images/${encodeURIComponent(name)}/clone`, {
      target_pool: targetPool,
      target_name: targetName,
    }),
  flattenImage: (pool: string, name: string) =>
    post<Job>(`/pools/${encodeURIComponent(pool)}/images/${encodeURIComponent(name)}/flatten`),
  commitImage: (pool: string, name: string) =>
    post<Job>(`/pools/${encodeURIComponent(pool)}/images/${encodeURIComponent(name)}/commit`),
  cloneVm: (id: string, name: string, mode: "linked" | "full", pool: string) =>
    post<{ vm: VmDetail | null; job: Job | null }>(`/vms/${id}/clone`, { name, mode, pool }),
  hostUsb: () => request<HostUsb>("/host/usb"),
  attachUsb: (id: string, rule: UsbPassthrough) => post<VmDetail>(`/vms/${id}/usb`, rule),
  detachUsb: (id: string, index: number) => request<VmDetail>(`/vms/${id}/usb/${index}`, { method: "DELETE" }),
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

  fetchUrl: (pool: string, url: string, name?: string, sha256?: string) =>
    post<Job>(`/pools/${encodeURIComponent(pool)}/fetch`, { url, name, sha256 }),

  jobs: () => request<Job[]>("/jobs"),
  cancelJob: (id: string) => post<void>(`/jobs/${id}/cancel`),
  jobLog: async (id: string) => {
    const res = await fetch(`/api/v1/jobs/${id}/log`);
    return res.ok ? res.text() : "";
  },

  snapshots: (vmId: string) => request<SnapshotList>(`/vms/${vmId}/snapshots`),
  takeSnapshot: (vmId: string, name: string, description: string, withMemory: boolean) =>
    post<Job>(`/vms/${vmId}/snapshots`, { name, description, with_memory: withMemory }),
  revertSnapshot: (vmId: string, snapshotId: string, safetySnapshot: boolean) =>
    post<Job>(`/vms/${vmId}/snapshots/${snapshotId}/revert`, { safety_snapshot: safetySnapshot }),
  deleteSnapshot: (vmId: string, snapshotId: string) =>
    request<Job>(`/vms/${vmId}/snapshots/${snapshotId}`, { method: "DELETE" }),

  /** Guest addresses per NIC, best source first (M3-08). */
  guestNet: (vmId: string) => request<GuestNetInfo>(`/vms/${vmId}/guest-net`),
};

export function wsUrl(path: string): string {
  const proto = window.location.protocol === "https:" ? "wss:" : "ws:";
  return `${proto}//${window.location.host}${path}`;
}
