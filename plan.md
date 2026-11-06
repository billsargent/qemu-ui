# qemu-ui — Specification

A browser-based control plane for QEMU: every architecture, every machine type, every
command-line option, real disk/network/display management, and VM sessions that survive
closing the browser.

Status: **v3** — revision of the v2 draft. Corrects several factual errors (mostly about
Windows hosts), collapses the API/supervisor split into one daemon, makes `-blockdev` and
JSON-syntax argv the compiler's output format, and adds a numbered work breakdown (§16)
that maps 1:1 to GitHub issues. Changes from v2 are summarised in §0.

---

## 0. What changed from v2, and why

| Area | v2 said | v3 says | Why |
|---|---|---|---|
| Process layout | browser → web/api → supervisor → QEMU (three tiers) | browser → **one daemon** → QEMU, plus a small privileged helper | QEMU already outlives the supervisor and is re-attached on start, so a separate web tier protects nothing and doubles RPC/auth/service work. Module boundary kept, process boundary dropped. |
| Daemonizing | `-daemonize` | Supervisor spawns QEMU detached itself; never `-daemonize` | `-daemonize` does not exist on Windows and loses the child handle on Linux. |
| Process survival | assumed | Explicit rules: systemd `KillMode`/per-VM scope, job objects without kill-on-close, stdio to files, pid identity check | Without these, "survives everything" is false on both OSes. |
| Disk argv | `-drive if=none` | `-blockdev` with node names | Snapshots, block jobs, mirror/backup, hotplug and `query-named-block-nodes` all key on node names. Cheap now, very expensive to retrofit. |
| Option syntax | keyval (`a=b,c=d`) | JSON for `-blockdev`/`-device`/`-object`/`-netdev`; keyval only where JSON is not accepted, with comma doubling | A comma in a file name injects options in keyval syntax. This is the real injection vector, not the shell. |
| Machine type | stored as entered | Alias resolved to versioned machine (`q35` → `pc-q35-X.Y`) at create time and pinned | Guest ABI stability across QEMU upgrades. |
| Lint run | separate spawn with `-nodefaults`, small `-m` | "Save & Start" *is* the lint: spawn real argv with `-S`, check, `cont`. Separate dry lint only for "Check config"/CI, with side-effecting backends rewritten | v2's lint validated a different config and could collide with locks/ports of running VMs. |
| L2 rules | forbid/warn/auto-fix | **warn by default**; forbid only with a citation *and* a lint-run reproduction | Several v2 rules were wrong (NVMe, virtio-scsi-pci, virtio-vga-gl and vmport all work on i440fx). |
| QMP monitors | one | two per VM: supervisor-owned + expert/console | Raw user commands must not desync the supervisor's state machine or swallow events. |
| VNC/SPICE | loopback TCP, no password | Unix socket where available; otherwise loopback TCP + per-run password via `set_password` | Any local process could otherwise attach. |
| vdagent | `-device qemu-vdagent` | `-chardev qemu-vdagent` + `virtserialport` | qemu-vdagent is a chardev backend, not a device. |
| Suspend to disk | `migrate exec:cat > …` | `migrate file:` (+ `mapped-ram`) | `exec:` is unavailable on Windows and blocked by `-sandbox spawn=deny`. |
| Internal snapshots | `savevm` | `snapshot-save` QMP job; UEFI VARS stored as qcow2 | `savevm` fails when any writable drive (classically raw pflash VARS) is not qcow2. |
| slirp port forwards | creation-time only | Live add/remove via HMP `hostfwd_add`/`hostfwd_remove` | Those commands exist; no restart needed. |
| Windows NAT | "passt is the recommended Windows answer" | slirp on Windows; passt is Linux-only | passt does not run on Windows. |
| Windows TPM | Win11 lane with swtpm on a WHPX host | TPM availability is *probed*; `impossible` on Windows hosts today | Verified: the QEMU 11.1 Windows build rejects `-tpmdev` as an invalid option, and swtpm has no mainstream Windows build. The probe decides, not the spec. |
| Multiqueue NIC | `mq` with `-netdev user` | `mq` only with tap/vhost backends | slirp has no multiqueue. |
| guacd / SPICE | committed for M5 on both OSes | Gated on a spike (M0-15) | SPICE support in guacamole-server and a native Windows guacd both need verification. |
| Recording | proxy tap → WebM | Record raw RFB stream, replay in noVNC; transcode only on export | Avoids an RFB decoder + video encoder in the daemon. |
| Data model | `spec_json` *and* normalised `disk`/`nic` tables | spec is canonical; tables are derived indexes; adds `event` log | One source of truth; event replay needs storage. |
| Privilege | "low-priv service account" that also does TAP/physical disks | Unprivileged daemon + narrow, audited privileged helper | The two goals contradict without a broker. |
| Toolchain | unspecified | All required binaries (Rust, C toolchain on Windows, QEMU on Windows) installed into project-local `.tools/` by `scripts/bootstrap.sh` | Reproducible, nothing installed system-wide. |

---

## 1. Problem statement

Existing web UIs for QEMU fall into two camps:

| Camp | Examples | What you get | What you don't |
|---|---|---|---|
| Libvirt front-ends | virt-manager, Cockpit Machines, WebVirtMgr | Lifecycle UX, validation | Only what libvirt's XML models; rare QEMU flags rejected or dropped; mostly x86/KVM-centric |
| Thin wrappers | shell scripts, bespoke panels, raw monitor | Anything QEMU can do | No validation, no state, no multi-VM, brittle |

**Thesis: a UI that is as expressive as the QEMU command line, but as safe as a form.**

### Non-goals (v1)
- Cluster management, live migration between hosts.
- GPU-accelerated remote desktop. The console is a management console.
- Replacing libvirt where it is already standard — but we import/export its XML.
- Multi-tenant hostile-user safety (§13: this is root-equivalent software).
- Internet exposure. LAN only; document how to put it behind a VPN.
- macOS hosts are not excluded by design but are not in the CI matrix or parity ledger.

---

## 2. Design principles

**P1 — Talk to QEMU directly, not through libvirt.** QMP introspection
(`query-machines`, `query-cpu-definitions`, `qom-list-types`, `device-list-properties`,
`qom-list-properties`, `query-qmp-schema`, …) makes QEMU self-describing. libvirt is lossy on
purpose, which is incompatible with "every option". We borrow libvirt's capabilities-probing
idea and ship an XML importer/exporter.

**P2 — Declarative spec → deterministic compiler → argv.** The spec (JSON, versioned) is
desired state. The compiler is a pure function of `(spec, capability snapshot, platform)`
and its output is golden-tested. Anything the model does not cover lives in an ordered
`passthrough` list that is preserved across edits and clearly labelled in the UI.

**P3 — Generate the long tail from QEMU's schema; hand-craft the short head.** ~15 concepts
(machine, cpu, memory, firmware, boot, disks, NICs, display, serial, TPM, USB, audio, shares)
get curated UI. Everything else is rendered from QOM/QAPI property types. If we cannot render
a property, it is still reachable through passthrough.

**P4 — QEMU is the authoritative validator.** Our rules lag QEMU; QEMU's own startup does
not. Starting with `-S` and checking health before `cont` is the final word (§6.3).

**P5 — The daemon owns VMs, QEMU outlives the daemon.** QEMU processes are spawned detached,
with stdio to files, a pidfile, and socket-based monitors. On start the daemon re-attaches and
reconciles. Killing or upgrading the daemon never kills a VM.

**P6 — qcow2 + backing chains are the copy-on-write primitive.** Templates are read-only
bases; clones are overlays; snapshots are cheap. This is designed in from M1 because
"VM as a link" (§18) depends on it.

**P7 — Narrow plugin interfaces at the platform seams.** Storage, network, display and
accelerator backends sit behind small traits so Windows specifics never leak into Linux code
paths or the frontend.

**P8 — Capabilities are data, never `if windows`.** Every "can this host do X" answer comes
from the probe (§4) or the parity ledger (§14), both of which the UI and CI consume. A control
that cannot work is *absent or disabled with a reason*, never broken.

---

## 3. Architecture

```
┌──────────────────────────── Browser (SPA) ────────────────────────────┐
│ React + TS + Vite · TanStack Router/Query · shadcn/ui + Tailwind      │
│ noVNC · xterm.js · Monaco (argv / JSON)                               │
└──────▲──────────────────────────▲──────────────────────────▲──────────┘
       │ REST /api/v1 (JSON)      │ SSE /api/v1/events       │ WS /ws/{vnc,serial}/{vm}
┌──────┴──────────────────────────┴──────────────────────────┴──────────┐
│ qemu-uid  (single Rust daemon)                                        │
│  http: axum, auth, OpenAPI        console: WS↔VNC/serial proxies      │
│  supervisor: spawn · reap · reattach · state machine · autostart      │
│  qmp: session pool (2 monitors/VM) · event log                        │
│  caps: binary discovery · probe · snapshots · parity ledger           │
│  compiler: spec → argv (pure crate)   store: SQLite                   │
│  jobs: qemu-img, uploads, downloads   audit                           │
└──────▲─────────────────────────────▲──────────────────────────────────┘
       │ local socket, request allowlist          │ QMP / VNC / serial (unix sockets, or
┌──────┴──────────────────┐                       │ loopback TCP + password on Windows)
│ qemu-ui-helper          │            ┌──────────┴───────────────────────┐
│ (privileged, tiny)      │            │ qemu-system-<arch> × N           │
│ tap/bridge, phys disks, │            │ detached · pidfile · stdio→files │
│ per-VM uid/cgroup setup │            │ swtpm / virtiofsd / passt sidecars│
└─────────────────────────┘            └──────────────────────────────────┘
          (CLI `qemu-ui` speaks the same HTTP API as the browser)
```

### 3.1 Why one daemon (changed from v2)
The v2 argument for splitting web and supervisor was "restarting the web server must not
orphan VMs". P5 already guarantees that — VMs outlive the supervisor too — so the split buys
nothing and costs an internal RPC protocol, a second auth boundary and a second service. Code
stays modular (separate crates), so splitting later is mechanical if ever needed.

### 3.2 Process survival rules (new)
"QEMU survives the daemon" only holds if all of these are true:
- **No pipes.** QEMU's stdout/stderr go to per-run log files, never pipes; otherwise QEMU gets
  EPIPE/SIGPIPE when the daemon exits.
- **Linux/systemd.** Each VM runs in its own transient scope (`systemd-run --scope`, via the
  helper or the user manager), or the daemon unit uses `KillMode=process`. Default
  `KillMode=control-group` kills every VM when the service stops. Per-VM scopes also give us
  per-VM cgroups for free (§13).
- **Windows.** If VMs are placed in job objects for containment, the job must not set
  `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` on a handle whose lifetime is the daemon's. Spawn with
  `CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS | CREATE_BREAKAWAY_FROM_JOB` where permitted.
- **Identity.** Re-attach trusts a pid only if the process start time (Linux
  `/proc/<pid>/stat` field 22; Windows `GetProcessTimes`) matches what we recorded and the
  monitor socket answers with the expected VM name. Pids are reused.

### 3.3 Repository layout
```
crates/
  qemu-ui-core/      spec types, compiler, capability model, parity ledger — pure, no I/O
  qemu-ui-qmp/       async QMP client (framing, negotiation, commands, events, transports)
  qemu-ui-daemon/    qemu-uid binary: http, supervisor, probe, store, jobs
  qemu-ui-cli/       qemu-ui binary: HTTP client for the daemon
  qemu-ui-helper/    privileged helper (M2+)
web/                 React SPA
data/                parity ledger, curated rules, golden specs (data, not code)
scripts/             bootstrap.sh, env.sh, dev helpers
.tools/              project-local toolchains + QEMU (gitignored)
```

### 3.4 Stack

| Layer | Pick | Notes |
|---|---|---|
| Daemon | Rust: tokio, axum, serde, rusqlite (bundled), tracing, clap | Decided (v2 #2). |
| Windows toolchain | `x86_64-pc-windows-gnu` + w64devkit | No Visual Studio required; all in `.tools/`. |
| API types | Rust structs → OpenAPI (`utoipa`) → generated TS client | One source of truth; no hand-written Zod drift. |
| Frontend | React 19 + TS + Vite, TanStack Router/Query, shadcn/ui | |
| State | SQLite (WAL) + files for images, logs, run dirs | One DB file to back up. |
| Display | noVNC via our WS proxy (v1); guacd engine gated on M0-15 | §8 |
| Serial | xterm.js ↔ chardev socket | Non-negotiable fallback console. |

### 3.5 Project-local toolchain (new)
`scripts/bootstrap.sh` installs every required binary under `.tools/` (gitignored) and
`scripts/env.sh` puts them on `PATH` for the current shell only:

| Component | Windows | Linux |
|---|---|---|
| Rust (rustup, cargo, clippy, rustfmt) | `.tools/rustup`, `.tools/cargo` (GNU host) | same, native host |
| C toolchain (bundled SQLite, ring) | `.tools/w64devkit` | system cc |
| QEMU | `.tools/qemu` (extracted from the upstream Windows build, never "installed") | distro packages; probe also checks `.tools/qemu` |
| Node | system Node ≥ 20 (npm deps in `web/node_modules`) | same |

Versions are pinned in `bootstrap.sh`; bumping one is a deliberate commit. Binary discovery
(§4) searches `.tools/qemu` first, then configured dirs, then `PATH`, and side-by-side
versions are first-class.

---

## 4. Capability discovery

Never hardcode architectures, machines or devices. Build a **capability snapshot** per QEMU
binary at first run, on demand ("Rescan"), and when a binary's version/mtime changes.

1. **Discover binaries**: `qemu-system-*` and `qemu-img` in `.tools/qemu`, configured dirs,
   `PATH`. Target arch from the file name; version from `--version`.
2. **Probe via one long-lived process per binary**:
   `qemu-system-<arch> -machine none -nodefaults -nographic -S -qmp stdio`
   and issue QMP commands, rather than parsing many `-x help` outputs:
   - `query-version`, `query-qmp-schema` (hash it; typed commands/events; which runtime ops exist)
   - `query-machines` (name, alias, default, deprecated, cpu-max, default-cpu-type, …)
   - `query-cpu-definitions` (+ `query-cpu-model-expansion` lazily, on demand)
   - `qom-list-types` (devices: `abstract=false`, `implements=device`) and
     `device-list-properties` lazily per device, cached by `(binary hash, device)`
   - `query-accels` where available; otherwise the help-text fallback below
   - `query-command-line-options` as a supplement only (incomplete for several options)
3. **Help-text fallbacks** for things not exposed over QMP: `-accel help`, `-netdev help`,
   `-chardev help`, `-audiodev help`, `-display help`, `-tpmdev help`, `-machine help`.
4. **Accelerator availability with reasons**: KVM (`/dev/kvm` exists and is accessible —
   distinguish "absent" from "permission denied: add user to group kvm"); WHPX (try
   `-accel whpx` with `-machine none`; map failure to "enable Windows Hypervisor Platform",
   plus the VMware/VirtualBox coexistence note); HVF; TCG always.
5. **Firmware inventory**: read QEMU's firmware interop descriptors
   (`share/qemu/firmware/*.json` on Linux distros, `share/firmware/*.json` in the Windows build; the `firmware.json` QAPI format) which state architecture,
   machines, features (secure-boot, enrolled-keys, requires-smm) and the code/vars pair. Fall
   back to scanning known file names.
6. **Persist** the snapshot, keyed by `(binary path, version, binary hash)`. Every VM records
   the snapshot it was created against; re-probing never silently mutates a VM.

**Cross-arch honesty:** non-native archs are TCG-only. The UI shows `native (KVM|WHPX|HVF)`
or `emulated (TCG, slow)` on every VM card and in the wizard.

---

## 5. VM spec and compiler

### 5.1 Spec
A versioned JSON document (`spec_version`), with serde types in `qemu-ui-core` and an exported
JSON Schema. Spec migrations are code, tested on fixtures. The spec references images by
registry id, networks by id, firmware by descriptor id — never raw host paths (except in
passthrough).

### 5.2 Compiler output rules
- Starts with `-nodefaults -no-user-config`; everything QEMU would add implicitly is explicit.
- JSON syntax for `-blockdev`, `-device`, `-object`, `-netdev`, `-audiodev` (accepted by
  every QEMU in our supported range — verified by the probe). Keyval only for options without
  JSON support, with `,` doubled in every value.
- Disks: `-blockdev` protocol node + format node, named `<role><n>-file` / `<role><n>`;
  devices reference node names. Never `-drive` except `if=pflash` fallbacks where required.
- Machine: the pinned versioned name. Accelerator via `-accel`.
- IDs live in a reserved namespace (`qui-…`); passthrough using a reserved id is an error.
- Monitors: two QMP chardev sockets (`qmp`, `qmp-user`), one serial chardev with `logfile=`,
  `-pidfile`, `-name guest=<name>,debug-threads=on`, `-msg timestamp=on`.
- Linux: `-sandbox on,obsolete=deny,elevateprivileges=deny,spawn=deny,resourcecontrol=deny`
  when the probe reports seccomp support.
- Output is an argv array (`Vec<OsString>`), never a string. A display rendering with
  shell-quoting exists for the UI's "copy command" only.

### 5.3 Passthrough
- Ordered list of argv tokens, appended after generated options.
- Conflict detection: reserved ids, duplicate singleton options (`-machine`, `-accel`, `-m`,
  `-smp`, `-cpu`, `-bios`) → error with a pointer to the clashing field.
- Passthrough can reference arbitrary host paths, so it bypasses the pool allowlist. It is an
  **admin/expert surface** (§13) and the UI says "N options are set outside the editor".

### 5.4 Round-tripping
Our own argv parses back to an identical spec (property test). Foreign command lines import
best-effort; unrecognised tokens go to passthrough.

---

## 6. Validation — three layers

### 6.1 L1 Structural (from introspection, high confidence)
Bus availability per machine, device `user-creatable`, property existence/types per device,
max CPUs, NUMA support, hotpluggable CPUs, firmware matching machine + arch.

### 6.2 L2 Curated semantic rules (data, warn by default)
`data/rules/*.toml`, each with `id`, `severity` (`warn` default; `error` only with citation +
lint reproduction), `verified_against` QEMU versions, optional `autofix`, and a test case.
Examples worth shipping: ARM `virt` + `-cpu host` needs KVM; topology product > `cpu-max`;
Secure Boot needs `smm=on` + secure pflash + SB-enabled firmware descriptor; virtio-mmio vs
virtio-pci variants per machine; no graphics on s390x; virtio drivers needed by Windows
installers (suggest virtio-win ISO). Auto-fixes are shown as a visible "changed" chip.

### 6.3 L3 QEMU itself (authoritative)
- **Save & Start:** spawn the real argv with `-S`; wait for QMP; check `query-status`,
  `query-block`, stderr; then `cont`. If startup fails, the failure *is* the lint result —
  nothing extra to spawn, nothing to diverge.
- **Check config (dry lint):** the same argv with side effects rewritten: disks replaced with
  same-format scratch overlays (`-blockdev` with `read-only` + throwaway overlay), host
  network backends replaced by `user`, sockets moved to a temp dir, `-display none`. Labelled
  "authoritative for the device/machine model, not for host resources".
- **Error mapping:** stderr is matched to spec fields via the reserved id namespace (QEMU
  messages usually name the device/node id) — a heuristic, labelled as such. Where possible,
  objects/netdevs/blockdevs are added via QMP after `-S` to get structured per-item errors.
- Results cached by argv hash. CI runs the golden spec matrix through dry lint.

**UI contract:** incompatible things are *explained*, with the rule id and a docs link.

---

## 7. Lifecycle and persistence

### 7.1 States
`defined → starting → running ⇄ paused(reason) → stopping → stopped | crashed`, plus
`suspended` (state file on disk) and `waiting-for-media`. `paused` always carries a reason
from `query-status`/events: `user`, `io-error` (show which disk and errno, e.g. "host disk
full"), `guest-panicked`, `debug`, `migration`, `suspended`, `watchdog`.

### 7.2 Operations
| Op | Mechanism |
|---|---|
| Start | detached spawn with `-S` → health checks (§6.3) → `cont` |
| Stop (graceful) | `system_powerdown`; configurable timeout; then offer hard stop |
| Stop (hard) | QMP `quit`; escalate to SIGTERM/SIGKILL (Windows: `TerminateProcess`) |
| Reset / Pause / Resume | `system_reset` / `stop` / `cont` |
| Suspend to disk | `migrate` to `file:` URI with `mapped-ram` when supported; resume with `-incoming` (M7) |
| Snapshots | internal: `snapshot-save`/`snapshot-load`/`snapshot-delete` jobs; external: `blockdev-snapshot-sync` overlays (M2) |
| Autostart | per-VM policy at daemon start; crash-loop circuit breaker with exponential backoff |
| Reattach | §3.2 identity check → reconnect both monitors → `query-status` → reconcile; unknown → `crashed` |

### 7.3 Monitors and events
- Two QMP monitors per VM. The supervisor owns `qmp`; the expert console and power users get
  `qmp-user`. The supervisor never depends on state it did not observe itself.
- All QMP events are written to an append-only `event` table with a per-VM sequence, then
  fanned out over SSE. Clients reconnect with `Last-Event-ID` and get a snapshot + replay.
  Events missed while the daemon was down are reconciled by queries on reattach.

### 7.4 Operational cases (new)
- **QEMU upgraded under running VMs:** running VMs keep their binary; the VM shows "will
  restart on new QEMU N.M"; the spec stays pinned to its machine version.
- **Host sleep/hibernate:** detect resume; check guest clocks; optionally pause VMs on suspend.
- **Disk full:** default `werror=enospc`-style stop on the guest disk; surfaces as
  `paused(io-error)` with a "free space and resume" action.
- **Log rotation:** per-run QEMU logs and serial logs with size caps.
- **Backup/export:** a VM exports as spec + images (+ VARS + TPM state) bundle.

---

## 8. Display

### 8.1 Engines behind one interface
```rust
trait DisplayEngine {
    fn attach(&self, vm: &VmId, viewer: &Viewer) -> Result<Attachment>; // ticket + transport
    fn capabilities(&self) -> EngineCaps; // clipboard, audio, usb, multihead, record, files
    async fn screenshot(&self, vm: &VmId) -> Result<Png>;
}
```
Console controls render from `EngineCaps`: absent, not broken.

| Engine | Status |
|---|---|
| `vnc:novnc` — noVNC over our WS proxy | default, M1 |
| `spice:guac` / `vnc:guac` — guacd | M5, **gated on spike M0-15** (SPICE support in guacamole-server; guacd has no native Windows build, so Windows needs WSL/container or is `degraded`) |
| `spice:native` — `.vv` handoff to remote-viewer | M5; the honest answer for USB redirection and video |

### 8.2 VNC path (M1)
- QEMU serves VNC on a **unix socket** (`-vnc unix:<rundir>/vnc.sock`) where supported;
  otherwise loopback TCP from a port pool with `password=on` and a per-run password set via
  QMP `set_password`.
- `/ws/vnc/{vm}` proxy: single-use ticket (passed as WS subprotocol), `Origin` check, idle
  timeout, connection limit, viewer presence, take-control lock, read-only viewers.
- **Clipboard + absolute pointer:**
  `-chardev qemu-vdagent,id=qui-vdagent,name=vdagent,clipboard=on,mouse=on`
  `-device virtio-serial-pci` `-device virtserialport,chardev=qui-vdagent,name=com.redhat.spice.0`.
  Needs spice-vdagent (Linux) or virtio-win guest tools (Windows); `usb-tablet` as fallback
  pointer.
- Controls: fit/scale/1:1, fullscreen, send-key palette (Ctrl+Alt+Del, Ctrl+Alt+Fn),
  paste-as-keystrokes, screenshot.
- **Serial console beside graphics** in xterm.js; independent of the display engine.

### 8.3 Thumbnails, screenshots, recording
- Thumbnails: `screendump` (PNG format) every N s for visible VMs only, downscaled, cached.
- Recording: capture the RFB byte stream at the proxy (cheap, lossless, tiny); replay it in
  noVNC; transcode to WebM only on explicit export (ffmpeg sidecar, optional). Bookmarks.

### 8.4 Expectations to document
Desktop use on LAN is fine; video playback is poor over VNC; 3D/GPU is out of scope for the
browser; WAN is out of scope (put it behind WireGuard/Tailscale).

---

## 9. Storage

### 9.1 Pools and images
- Pools are directories with quota, allowed formats, read-only/shared flag. First run proposes
  a default pool in the app data dir and lets the user pick another (big second drive).
- Path safety: canonicalise, reject symlink escapes, `..`, device paths and UNC paths outside
  an explicit allowlist.
- Image registry: `qemu-img info --output=json` is truth; `check`, `map`, `measure` (predict
  sizes in confirmation dialogs).
- Locking: rely on QEMU's own image locking (on by default) plus a registry `locked_by`.
- Jobs for create/resize/convert/import (raw, qcow2, vmdk, vhdx, vdi, vpc)/compact.

### 9.2 Backing chains, templates, clones
Template = read-only base; VM disk = qcow2 overlay. Linked clone in O(1). Chain visualiser;
flatten (`block-stream` live / `qemu-img convert` offline) and commit, with warnings.
Rebase is expert-only.

### 9.3 Snapshots
Internal via `snapshot-save` jobs (requires all writable nodes qcow2 — UEFI VARS are stored as
qcow2 for this reason) and external via `blockdev-snapshot-sync`. Snapshot tree UI with
memory + disk-set grouping; auto-snapshot before risky operations.

### 9.4 Attach from elsewhere
1. **Browser upload:** resumable (tus-style create/HEAD/PATCH), streamed to disk, checksummed,
   survives tab close.
2. **URL fetch:** server-side download job with progress — SSRF-guarded (scheme allowlist,
   block loopback/link-local/metadata ranges after DNS resolution, size cap, admin only) — or
   read-only remote attach via QEMU's `https`/`nbd` block drivers (boot an ISO from a URL).
3. **Peer/NAS:** NBD export from another host, iSCSI, etc. through the same registry.

### 9.5 Disk attachment
Controllers filtered by machine (L1): `virtio-blk-pci`/`-device`, `virtio-scsi-pci` +
`scsi-hd`, `nvme`, `ide-hd`/`ide-cd`, AHCI, `usb-storage`, with plain-language tradeoffs.
Cache/aio guidance: Linux hosts `cache.direct=on`, `aio=io_uring` (probe), `discard=unmap`;
Windows hosts `aio=threads`. Warn on `cache=unsafe`. Hotplug via `blockdev-add` +
`device_add`; unplug waits for `DEVICE_DELETED`; eject-before-unplug guard.

### 9.6 Physical devices
- Physical disks go through the **privileged helper**. Linux: `host_device` on block
  devices; refuse if mounted or held (`/proc/mounts`, `/sys/block/*/holders`). Windows:
  `\\.\PhysicalDriveN` only when no volume is online; guided flow explains offlining; read-only
  default.
- Better defaults for "attach a physical thing": USB passthrough (`usb-host`, hotplug,
  remembered per VM) and folder sharing (virtiofs via `virtiofsd` or 9p on Linux hosts).

---

## 10. Networking

### 10.1 Network objects
| Type | Backend | Privilege | Hosts |
|---|---|---|---|
| NAT | `-netdev user` (slirp) | none | Linux, Windows |
| NAT (fast) | passt (`-netdev stream` to passt socket, or native `passt` netdev if probed) | none | Linux only |
| Isolated | `-netdev dgram` (multicast) / `stream` | none | Linux, Windows |
| Bridged | tap + bridge (helper) | helper | Linux; Windows via TAP-Windows driver, guided |
| Host-only | tap + bridge without uplink | helper | Linux; Windows guided |
| VFIO NIC | vfio-pci | root + IOMMU | Linux |
| None | — | — | all |

### 10.2 Port forwards
slirp: creation-time `hostfwd=` *and* live `hostfwd_add`/`hostfwd_remove` (HMP via
`human-monitor-command`). passt: its own forwarding config. Binding to all interfaces is an
explicit dangerous toggle. Guest-agent IP detection drives "connect to guest" cards
(SSH/RDP/HTTP) that propose forwards.

### 10.3 NIC models
`virtio-net-pci`/`-device` (multiqueue and vhost only with tap), `e1000`/`e1000e`, `rtl8139`,
`vmxnet3`, `ne2k_pci`; filtered by machine. MAC defaults to a stable random locally
administered address (`52:54:00:…`), unique across the registry.

---

## 11. UI/UX

1. **Dashboard** — VM cards (state + pause reason, accel badge, thumbnail, IP, uptime), quick
   actions, host health (accel, pool space, orphaned/crashed VMs).
2. **New VM** — *Guided* (OS + arch + ISO → validated defaults), *Custom*, *Import*
   (raw argv, libvirt XML, existing disk).
3. **VM detail** — Overview · Console (graphics + serial split) · Hardware · Disks · Network ·
   Snapshots · Events/Logs · Settings. Persistent **argv drawer**: exact command line,
   copyable, with diff since last start.
4. **Hardware editor** — bus/device tree, palette filtered by machine, generated property
   forms, "why is this disabled?" everywhere.
5. **Images** — pools, uploads, jobs, chain visualiser.
6. **Networks** — objects, attachments, forwards.
7. **System** — capability report (binaries, machines, accels with remediation, firmware,
   parity ledger), pools, audit, tokens.
8. **Expert** — argv playground (dry lint), QMP console on `qmp-user` (admin-only, off by
   default), schema browser.

Rules: never show optimistic `running`; warnings are overridable, errors are not; slow work is
a job with progress/cancel/logs; keyboard-first (`g d`, `g c`, `/`); small screens keep the
console usable.

---

## 12. Data model

The **spec is canonical**. Tables marked *(derived)* are indexes rebuilt from specs.

```
binary(id, path, arch, version, sha256, discovered_at)
cap_snapshot(id, binary_id, probed_at, schema_hash, data_json)
vm(id, name, spec_json, spec_version, cap_snapshot_id, state, pause_reason,
   autostart, crash_count, created_at, updated_at)
vm_run(id, vm_id, argv_json, argv_hash, pid, proc_start_time, run_dir,
   started_at, ended_at, exit_status, log_path)
event(id, vm_id, seq, at, kind, data_json)            -- SSE replay source
pool(id, name, path, quota_bytes, formats_allowed, readonly)
image(id, pool_id, rel_path, format, virtual_size, actual_size, backing_image_id,
   checksum, locked_by_vm)
vm_disk(vm_id, image_id, role)          (derived)
vm_nic(vm_id, network_id, mac)          (derived)
network(id, name, kind, config_json)
snapshot(id, vm_id, kind, name, parent_id, mem_state_path, disks_json, created_at)
job(id, kind, status, progress, message, log_path, created_at, finished_at)
user(id, name, pw_hash, role), session(...), token(id, user_id, hash, scope, expires_at)
audit(id, at, actor, action, target, detail_json, argv_hash, result)
```

Specs export as files (`qemu-ui export`, `qemu-ui apply spec.json`) for config-as-code.

---

## 13. Security

**This software is root-equivalent.** Anyone who can configure a VM can attach host disks and
devices. Decided model: **single user, LAN-exposed** (v2 #4).

- **Bind loopback-only until M6 lands.** LAN exposure is enabled by the M6 setup flow, never
  by default in earlier builds.
- **Auth (M6):** first-run admin account (argon2id); `HttpOnly; Secure; SameSite=Strict`
  session cookies; CSRF token; auth-attempt rate limits; API tokens for the CLI.
- **WebSockets:** single-use 30 s tickets as a subprotocol; `Origin` check; no cookies or
  tokens in query strings.
- **Transport:** TLS by default with a self-signed cert + trust flow; BYO cert; HSTS. Plain
  HTTP only on loopback.
- **Authorization funnel:** one `can(actor, action, target)` used everywhere; single-user
  implementation now; RBAC later is additive.
- **No injection:** argv arrays only; JSON-syntax options; comma doubling elsewhere; paths
  canonicalised against pool allowlists; names/MACs/sizes/enums validated.
- **Monitors:** AF_UNIX sockets in a per-VM run dir (`0700` on Unix; on Windows the run dir's
  ACL grants only the service account), on **both** platforms — decided by spike M0-14, see
  §14.1. Socket paths must stay under 108 bytes, so run dirs live under a short root. Never exposed to the network. The browser reaches QMP
  only through audited endpoints, or the expert console.
- **Expert surfaces = root:** passthrough, raw argv, QMP console. Admin-only, off by default,
  each enable is audited.
- **QEMU hardening:** `-sandbox` on Linux; run each VM as its own UID (via helper) in its own
  cgroup with CPU/mem/IO limits; AppArmor/SELinux profiles where present. Windows: restricted
  token, low-privilege service account, job objects (§3.2) — weaker, and the ledger says so.
- **Privileged helper:** tiny binary, fixed request set (create tap, attach to bridge, open
  physical disk and pass fd, create per-VM uid/scope), validates every request, audited.
- **Secrets:** LUKS passphrases via QEMU `secret` objects fed from the OS keyring/DPAPI,
  never in the spec. ISO checksum/signature verification.
- **SSRF:** see §9.4.

---

## 14. Platform parity ledger

Ledger values: `native` · `degraded` · `needs-setup` · `impossible` · `unverified`.
The ledger is **data** (`data/parity.toml`), keyed by feature and platform, refined at runtime
by the probe (probe wins), consumed by the UI (reasons) and by CI (assertions).

| Feature | Linux | Windows |
|---|---|---|
| Hardware accel | `native` KVM (needs-setup if `/dev/kvm` perms) | `needs-setup` WHPX (enable Windows Hypervisor Platform; VMware/VBox coexistence note) |
| NAT (slirp) incl. live forwards | `native` | `native` |
| NAT (passt) | `native` | `impossible` |
| Isolated VM↔VM net | `native` | `native` |
| Bridged / host-only | `native` via helper | `needs-setup` TAP-Windows + admin |
| Physical disk | `native` via helper | `needs-setup` offline volumes + admin |
| Folder share (virtiofs/9p) | `native` | `impossible` (offer SMB share or SFTP) |
| USB passthrough | `native` | `degraded` (libusb/WinUSB driver per device) |
| TPM 2.0 (swtpm) | `native` | `impossible` — confirmed: the upstream Windows build (QEMU 11.1) does not compile `-tpmdev` in; the probe re-checks per binary |
| QEMU `-sandbox` | `native` | `impossible` |
| Per-VM UID + cgroups | `native` | `impossible` (job objects instead) |
| VFIO passthrough | `native` | `impossible` |
| QMP + serial over AF_UNIX sockets | `native` | `native` (spike M0-14; Windows client bridges a blocking socket) |
| VNC over unix socket | `native` | `degraded` — loopback TCP + per-run password |
| guacd engine | `unverified` — spike M0-15 | `unverified` — spike M0-15 |
| Lifecycle, blockdev disks, snapshots, VNC console, serial, all target archs | `native` | `native` |

### 14.1 Spike M0-14 result: monitor and console transports on Windows

Tested against QEMU 11.1 on a Windows host:

| Transport | Result | Decision |
|---|---|---|
| Named pipe (`-chardev pipe`) | QEMU creates a single pipe instance; after the first client disconnects, every new connection fails with "All pipe instances are busy". Any local process that opens the pipe first takes it. | **Rejected** — reattach is impossible. |
| Loopback TCP | Reconnects fine; no access control (any local process can connect). | Fallback only, never for QMP. |
| AF_UNIX (`-chardev socket,path=`) | Works and reconnects; protected by filesystem ACLs. 108-byte path limit applies on Windows too. | **QMP and serial on all platforms.** Tokio has no AF_UNIX on Windows, so the client bridges a blocking `uds_windows` socket to an async stream (two threads per connection). |
| VNC `unix:` | Accepted without error but no socket is created. | VNC on Windows uses loopback TCP with `password=on` and a per-run password set via QMP `set_password`. |

Also found: **`-mon` is deprecated in QEMU 11.1** in favour of
`-object monitor-qmp,id=…,chardev=…`. The probe records `-object` types; the compiler emits
`monitor-qmp` objects when available and `-mon` otherwise. And a QMP client must fully close
its connection when done: QEMU serves one client per monitor chardev and stays silent to the
next one until the previous connection is closed.

**Consequence for the Windows 11 lane:** on a Windows host it can do UEFI + Secure Boot +
virtio-win, but not TPM unless the probe finds a backend. The wizard says so and offers the
documented registry bypass as the user's choice, not ours.

CI runs the golden corpus on **both** Windows and Linux; ledger claims are asserted by tests.

---

## 15. Decisions

### Locked (from v2)
| # | Decision |
|---|---|
| 1 | Windows and Linux both first-class, parity where physically possible (§14) |
| 2 | Rust daemon |
| 3 | Display all-in, staged: noVNC M1; rich engines M5; native handoff always |
| 4 | Single user, LAN-exposed (loopback-only until M6) |
| 5 | Schema-generated long tail + curated head + raw escape hatch |

### Resolved in v3 (previously open)
| # | Question | Decision |
|---|---|---|
| 6 | Process layout | One daemon + privileged helper (§3.1) |
| 7 | guacd distribution | Detect-and-guide; gated on spike M0-15; bundling revisited after |
| 8 | QEMU distribution | Use installed QEMU *and* project/app-local managed copies side by side; Windows dev builds use `.tools/qemu` |
| 9 | Default image location | First-run proposes app-data pool, user can choose another |
| 10 | "VM as a link" | Plan now (blockdev + overlays from M1/M2); feature itself in backlog |
| 11 | macOS | Not excluded, not modelled, not in CI |
| 12 | Disk argv | `-blockdev` + JSON syntax everywhere possible |
| 13 | Toolchain | Project-local `.tools/`, Windows uses the GNU Rust host + w64devkit |

---

## 16. Milestones and work breakdown

Each ID is one GitHub issue; PRs reference IDs. "Spike" = time-boxed investigation whose
output is a decision recorded in this file.

### M0 — Foundations
- **M0-01** Repo conventions: licence, `.gitattributes` (LF), editorconfig, CONTRIBUTING, PR/issue templates
- **M0-02** Project-local toolchain bootstrap (`scripts/bootstrap.sh`, `scripts/env.sh`, `.tools/`)
- **M0-03** Cargo workspace + crate layout (§3.3) + web scaffold
- **M0-04** QMP client crate: framing, greeting, `qmp_capabilities`, commands, events, transports (stdio, unix, tcp)
- **M0-05** QEMU binary discovery (`.tools/qemu`, configured dirs, `PATH`) with version + hash
- **M0-06** Capability probe via long-lived `-machine none` QMP session (§4.2–4.3)
- **M0-07** Firmware inventory from QEMU firmware descriptors
- **M0-08** Accelerator availability with remediation reasons
- **M0-09** SQLite store + migrations; capability snapshots persisted
- **M0-10** HTTP API skeleton (axum), `/api/v1/host/*`, OpenAPI + generated TS client
- **M0-11** Web SPA skeleton: host capability report page
- **M0-12** Parity ledger as data + API + UI rendering
- **M0-13** CI on Windows + Linux: fmt, clippy, tests, probe smoke against TCG
- **M0-14** Spike: QMP/VNC/serial transport on Windows (AF_UNIX vs named pipe vs loopback TCP + password)
- **M0-15** Spike: guacd SPICE support and Windows availability (gates M5-04)

### M1 — One VM end to end (the thesis slice)
- **M1-01** VM spec schema v1 + JSON Schema export + migration framework
- **M1-02** Compiler spec → argv (JSON syntax, `-blockdev`, `-nodefaults`, reserved ids) + golden tests
- **M1-03** Passthrough: preservation, ordering, conflict detection
- **M1-04** Machine alias pinning at create
- **M1-05** Supervisor spawn: detached, run dir, stdio→files, pidfile, process identity
- **M1-06** Process survival: systemd scope/KillMode; Windows job-object rules; dev runner
- **M1-07** QMP session manager: two monitors per VM, reconnect, event log
- **M1-08** Lifecycle state machine incl. pause reasons; start with `-S` → check → `cont`
- **M1-09** Reattach + reconcile on daemon start
- **M1-10** Dry lint ("Check config") with side-effect rewriting and stderr → field mapping
- **M1-11** VNC console: socket/password VNC, WS proxy with tickets, noVNC client, vdagent
- **M1-12** Serial console: chardev socket + logfile, xterm.js
- **M1-13** Event stream: SSE with `Last-Event-ID` replay
- **M1-14** Spec editor (machine/cpu/mem/firmware/boot/one disk) + live argv drawer + diff
- **M1-15** Dashboard with VM cards and actions
- **M1-16** Loopback-only binding + local token until M6

### M2 — Storage
- **M2-01** Pools: config, quotas, path canonicalisation/allowlist
- **M2-02** Image registry + `qemu-img info/check/map/measure`
- **M2-03** Generic job system (progress, cancel, logs) — used by everything after
- **M2-04** Image create/resize/convert/import/compact jobs
- **M2-05** Backing chains, templates, linked clones; flatten/commit
- **M2-06** Snapshots: internal (`snapshot-save`, qcow2 VARS) + external + tree UI
- **M2-07** Resumable browser upload
- **M2-08** URL fetch job with SSRF guard; read-only remote attach (https/nbd)
- **M2-09** Disk controllers filtered by machine; cache/aio guidance; hotplug/unplug
- **M2-10** Privileged helper: design, request allowlist, Linux + Windows implementations
- **M2-11** Physical disk attach (Linux checks; Windows offline-volume guided flow)
- **M2-12** USB passthrough shelf (hotplug, remembered per VM)
- **M2-13** Folder sharing (virtiofs/9p on Linux hosts)

### M3 — Networking
- **M3-01** Network objects + per-VM attachments
- **M3-02** slirp NAT with live port forwards
- **M3-03** passt NAT (Linux)
- **M3-04** Isolated networks (dgram/stream)
- **M3-05** Bridged / host-only on Linux via helper
- **M3-06** Windows TAP detection and guided setup
- **M3-07** NIC models and virtio-net feature toggles; MAC allocation
- **M3-08** Guest IP detection + "connect to guest" cards

### M4 — Compatibility engine
- **M4-01** L1 structural rules from introspection
- **M4-02** Schema-generated property editor (QOM/QAPI → widgets)
- **M4-03** L2 curated rules as data, warn-by-default, with test harness
- **M4-04** Auto-fix with visible "changed" chips
- **M4-05** Hardware editor device tree + filtered palette
- **M4-06** Lint matrix across every installed arch in CI
- **M4-07** Importers (raw argv, libvirt XML) and libvirt XML exporter

### M5 — Rich display
- **M5-01** `DisplayEngine` abstraction; `EngineCaps`-driven console controls
- **M5-02** Dashboard thumbnails via `screendump`
- **M5-03** Recording: RFB capture, noVNC replay, optional WebM export
- **M5-04** guacd engine (SPICE/VNC, audio, multi-monitor) — gated on M0-15
- **M5-05** Native client handoff (`.vv`)
- **M5-06** Multi-viewer presence, take-control, read-only viewers

### M6 — Hardening for LAN exposure
- **M6-01** First-run setup, argon2id auth, sessions, CSRF, rate limits
- **M6-02** TLS by default, trust flow, BYO cert, HSTS
- **M6-03** WS tickets + `Origin` enforcement on all sockets
- **M6-04** Audit log + UI
- **M6-05** Linux isolation: per-VM UID, cgroups, `-sandbox`, LSM profiles
- **M6-06** Windows isolation: service account, restricted token, job objects
- **M6-07** Quotas and IO throttling
- **M6-08** Expert surfaces gating (passthrough, argv playground, QMP console)
- **M6-09** Service install: systemd unit / Windows service

### M7 — Power features
- **M7-01** Suspend to disk via `file:` migration
- **M7-02** Guest agent integration
- **M7-03** Cloud-init NoCloud seed generator
- **M7-04** Template gallery
- **M7-05** Config-as-code: export / `qemu-ui apply`
- **M7-06** CLI parity
- **M7-07** Windows 11 lane (UEFI + SB + virtio-win; TPM where the probe allows)
- **M7-08** Autostart with crash-loop breaker and backoff

### Cross-cutting
- **X-01** Golden spec corpus (one per arch/machine family) run in CI on both OSes
- **X-02** Operational cases: QEMU upgrades under running VMs, host sleep, disk-full pauses, log rotation
- **X-03** VM backup/export bundles (spec + images + VARS + TPM state)
- **X-04** Documentation site: install, platform notes, security model, honest caveats

---

## 17. Prior art — what to borrow

| Project | Borrow |
|---|---|
| Cockpit Machines | Console UX, "why is this disabled" copy, snapshot tree |
| virt-manager | New-VM wizard, add-hardware palette, CPU model picker |
| Proxmox | Template/clone ergonomics, task panel, guest agent use |
| GNOME Boxes | Zero-config "just boot the ISO" |
| libvirt | Capabilities matrix, domain XML import/export, firmware descriptor use |
| qmp-shell | QMP ergonomics |
| Guacamole | Recording, file transfer, audio gateway |
| UTM | Polished config UI over raw QEMU — closest cousin |

---

## 18. Ideas backlog (not scheduled)

Each is an `idea`-labelled issue; promoted into a milestone when picked up.

- **VM as a link** — signed URL boots a disposable linked clone, auto-destroyed.
- **Record/replay debugging** — `-icount` + record/replay with a scrubber; reverse execution.
- **TCG plugin panel** — instruction counts, hot blocks, cache sim per VM.
- **GDB stub panel** — attach gdb, breakpoints/backtrace in browser.
- **Time-travel debugging** — snapshots + replay + gdb + plugins in one pane.
- **"Why is my guest slow?" advisor** — accel, drivers, cache/aio, topology, ballooning.
- **OS autodetection** from ISO volume labels / boot sectors.
- **"Explain this option"** inline docs from QEMU help + docs index.
- **Migration between two qemu-ui hosts.**
- **Resource sparklines** via `query-stats` + host metrics.
- **Plugin SDK** for lifecycle hooks.
- **Mobile monitor mode** — list, thumbnails, stop.
- **Multi-monitor in noVNC** (multiple heads as tabs).

---

## Appendix A — Example spec and compiled argv (Linux host)

```json
{
  "spec_version": 1,
  "name": "win11-dev",
  "arch": "x86_64",
  "accel": "kvm",
  "machine": { "type": "pc-q35-9.1", "props": { "smm": "on" } },
  "cpu": { "model": "host", "props": { "hv-passthrough": "on" },
           "topology": { "sockets": 1, "cores": 8, "threads": 2 } },
  "memory": { "size_mib": 16384 },
  "firmware": { "descriptor": "ovmf-x86_64-ms-4m", "vars_image": "img-vars-01" },
  "tpm": { "model": "tpm-crb", "backend": "swtpm" },
  "disks": [ { "image": "img-0001", "bus": "virtio-blk", "boot_index": 1,
               "cache": "none", "aio": "io_uring", "discard": "unmap" } ],
  "media": [ { "image": "img-iso-07", "bus": "ide", "readonly": true } ],
  "nics": [ { "network": "net-nat", "model": "virtio-net-pci", "mac": "52:54:00:3a:91:0c" } ],
  "display": { "engine": "vnc:novnc", "vdagent": true },
  "passthrough": ["-global", "ICH9-LPC.disable_s3=1"]
}
```

```text
qemu-system-x86_64
  -name guest=win11-dev,debug-threads=on -msg timestamp=on
  -nodefaults -no-user-config
  -sandbox on,obsolete=deny,elevateprivileges=deny,spawn=deny,resourcecontrol=deny
  -accel kvm
  -machine pc-q35-9.1,smm=on,pflash0=qui-fw-code,pflash1=qui-fw-vars
  -global driver=cfi.pflash01,property=secure,value=on
  -cpu host,hv-passthrough=on -smp 16,sockets=1,cores=8,threads=2 -m 16384
  -blockdev {"node-name":"qui-fw-code","driver":"file","filename":"<share>/edk2-x86_64-secure-code.fd","read-only":true}
  -blockdev {"node-name":"qui-fw-vars-file","driver":"file","filename":"<pool>/win11-dev/VARS.qcow2"}
  -blockdev {"node-name":"qui-fw-vars","driver":"qcow2","file":"qui-fw-vars-file"}
  -chardev socket,id=qui-tpm,path=<run>/swtpm.sock
  -tpmdev emulator,id=qui-tpm0,chardev=qui-tpm
  -device {"driver":"tpm-crb","tpmdev":"qui-tpm0"}
  -blockdev {"node-name":"qui-disk0-file","driver":"file","filename":"<pool>/win11-dev/disk0.qcow2","aio":"io_uring","cache":{"direct":true},"discard":"unmap"}
  -blockdev {"node-name":"qui-disk0","driver":"qcow2","file":"qui-disk0-file","discard":"unmap"}
  -device {"driver":"virtio-blk-pci","id":"qui-vd0","drive":"qui-disk0","bootindex":1}
  -blockdev {"node-name":"qui-cd0-file","driver":"file","filename":"<pool>/iso/Win11.iso","read-only":true}
  -blockdev {"node-name":"qui-cd0","driver":"raw","file":"qui-cd0-file","read-only":true}
  -device {"driver":"ide-cd","id":"qui-cd0-dev","drive":"qui-cd0","bus":"ide.1"}
  -netdev {"type":"user","id":"qui-net0"}
  -device {"driver":"virtio-net-pci","id":"qui-nic0","netdev":"qui-net0","mac":"52:54:00:3a:91:0c"}
  -device {"driver":"virtio-vga","id":"qui-gpu0"}
  -device {"driver":"virtio-serial-pci","id":"qui-vser0"}
  -chardev qemu-vdagent,id=qui-vdagent,name=vdagent,clipboard=on,mouse=on
  -device {"driver":"virtserialport","chardev":"qui-vdagent","name":"com.redhat.spice.0"}
  -vnc unix:<run>/vnc.sock                       (Windows: -vnc 127.0.0.1:<n>,password=on)
  -chardev socket,id=qui-qmp,path=<run>/qmp.sock,server=on,wait=off
  -object monitor-qmp,id=qui-mon,chardev=qui-qmp            (older QEMU: -mon chardev=…,mode=control)
  -chardev socket,id=qui-qmp-user,path=<run>/qmp-user.sock,server=on,wait=off
  -object monitor-qmp,id=qui-mon-user,chardev=qui-qmp-user
  -chardev socket,id=qui-ser0,path=<run>/serial.sock,server=on,wait=off,logfile=<run>/serial.log
  -serial chardev:qui-ser0
  -pidfile <run>/qemu.pid
  -S
  -global ICH9-LPC.disable_s3=1
```

Every token traces to a spec field except the final `-global`, which is passthrough and is
labelled as such in the UI. `-S` is followed by a QMP `cont` once start-up checks pass (§6.3).
JSON arguments are passed as single argv elements; no shell is involved.

**On a Windows host** the same spec compiles with `-accel whpx`, `aio=threads`, no
`-sandbox`, VNC on loopback TCP with a per-run password (§14.1), and **no TPM** unless the probe reports a TPM backend
(the editor shows the TPM field as unavailable with that reason).
