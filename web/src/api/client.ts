// Thin typed client for /api/v1. Types are generated from the Rust structs
// (cargo test -p qemu-ui-core writes them to ./generated).
import type { CapabilitiesResponse } from "./generated/CapabilitiesResponse";
import type { DeviceProperty } from "./generated/DeviceProperty";
import type { ScanStatus } from "./generated/ScanStatus";

export class ApiError extends Error {
  constructor(
    public status: number,
    message: string,
  ) {
    super(message);
  }
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(`/api/v1${path}`, init);
  if (!res.ok) {
    let msg = res.statusText;
    try {
      msg = (await res.json()).error ?? msg;
    } catch {
      /* not JSON */
    }
    throw new ApiError(res.status, msg);
  }
  return res.json() as Promise<T>;
}

export const api = {
  capabilities: () => request<CapabilitiesResponse>("/host/capabilities"),
  rescan: (force: boolean) =>
    request<ScanStatus>(`/host/rescan?force=${force}`, { method: "POST" }),
  deviceProperties: (binaryId: string, device: string) =>
    request<DeviceProperty[]>(
      `/host/binaries/${encodeURIComponent(binaryId)}/devices/${encodeURIComponent(device)}/properties`,
    ),
};
