# qemu-ui — Specification & Design Discussion

A browser-based control plane for QEMU: every architecture, every machine type, every
command-line option, real disk/network/display management, and VM sessions that survive
closing the browser.

Status: **draft v2** — five product decisions locked (see §15). Nothing implemented yet.

---

## 1. Problem statement

Existing web UIs for QEMU fall into two camps, and both are unsatisfying:

| Camp | Examples | What you get | What you don't |
|---|---|---|---|
| Libvirt front-ends | virt-manager, Cockpit Machines, WebVirtMgr | Great lifecycle UX, good validation | Only what libvirt's XML models. Unknown/rare QEMU flags are rejected or silently dropped. Mostly x86/KVM-centric. |
| Thin wrappers | raw `-monitor` / shell scripts / bespoke panels | Anything QEMU can do | No validation, no state, no multi-VM, brittle |

The gap: **a UI that is as expressive as the QEMU command line, but as safe as a form.**

That is the whole product thesis. Every design decision below serves it.

### Non-goals (v1)
- Not a Type-1 hypervisor / cluster manager (no live migration between hosts in v1).
- Not a gaming/GPU-acceleration streaming stack (console is a management console, not GeForce Now).
- Not a replacement for libvirt on servers that already standardised on it — but it must be
  able to *import* from it.
- Not multi-tenant hostile-user safe by default (see §12 — this is root-equivalent software).

---

## 2. Opinionated design principles

These are the calls I'd make, with reasoning. Push back on any of them.

### P1 — Talk to QEMU directly, not through libvirt
`query-machines`, `query-cpu-definitions`, `device-list-properties`, `qom-list-properties`,
`query-command-line-options`, `query-qmp-schema`, `query-hotpluggable-cpus`,
`query-memdev`, `query-block`, `query-named-block-nodes`, `query-netdev` give you a
*self-describing* hypervisor. libvirt is a lossy abstraction *on purpose*; "support every
command-line option" and "route through libvirt domain XML" are mutually exclusive goals.
We borrow libvirt's best idea (capabilities probing → compatibility matrix) without the
schema tax, and we ship an XML importer/exporter for coexistence.

### P2 — Declarative VM spec → compiler → argv. Raw argv is an escape hatch, not the model.
The database holds a JSON "VM spec" (desired state). A compiler renders it to argv.
Two hard requirements on the compiler:
- **Deterministic and diffable** (pure function of spec + host capabilities → argv).
- **Lossless on the edges**: any option the model doesn't cover lives in a
  `passthrough` bucket that is emitted verbatim, ordered, and *preserved across edits*.
  The UI shows "3 options are set outside the visual editor" with a link to inspect them.
- Reverse direction is best-effort: we can parse our own argv back into a spec, and we
  import foreign command lines where we understand them, keeping the rest as passthrough.

### P3 — Generate the boring 80% of the UI from QEMU's own schema
There are thousands of `-device`/`-object`/`-netdev`/`-chardev`/`-audiodev` properties.
Hand-mapping them is a permanent maintenance treadmill. Instead introspect and render:
on/off/auto → tri-state switch, bool → checkbox, uint32/size → numeric with units,
enum → select, string → text with pattern hints, `*` list → repeatable rows.
Hand-crafted UI is reserved for the ~15 concepts a human actually reasons about
(machine, cpu, memory, disks, nics, display, firmware, boot).

### P4 — The authoritative validator is QEMU itself, not our rules
Our compatibility rules will always lag QEMU. So we add a **lint run**: spawn the real
command line with `-S` (CPUs halted), `-nodefaults`, `-display none`, a small `-m`,
microlifetime QMP connection, capture stderr, `quit`. Exit-nonzero or stderr containing
device/machine errors → surfaced as a pre-flight failure *before* touching real disks or RAM.
This catches "you can't put virtio-vga on an ISA-only machine" for machines we've never
seen, on architectures released after we shipped.

### P5 — The supervisor owns VMs; the browser is a client of the supervisor
Three tiers, each able to outlive the one above it:
```
browser tab  ──►  web/api process  ──►  supervisor (service)  ──►  qemu-system-* process
   (dies)              (restartable)         (survives browser)      (survives everything)
```
QEMU is started `-daemonize`-equivalent (detached, own process group) with a QMP unix
socket (or named pipe on Windows) + pidfile. On supervisor start it **re-attaches** by
socket + pidfile rather than assuming it owns anything. Killing the web UI never kills a VM.

### P6 — qcow2-first, backing chains as the copy-on-write primitive
Templates = read-only base image + per-VM overlay. "Clone VM" is a 1 MB overlay, not a
50 GB copy. This also gives free snapshot/rollback and cheap disposable VMs.

### P7 — Everything is a plugin behind a narrow interface
Storage backends (local dir, block passthrough, NBD, iSCSI, RBD, browser upload staging),
network backends (slirp, passt, tap/bridge, socket/mcast, passthrough), display backends
(VNC, SPICE later), accelerators (KVM, WHPX, HVF, TCG). Keeps the Windows-host story from
contaminating the Linux one.

---

## 3. Architecture

```
┌──────────────────────────── Browser (SPA) ────────────────────────────┐
│ React + TS + Vite · TanStack Router/Query · shadcn/ui + Tailwind      │
│  noVNC (canvas) · xterm.js (serial) · Monaco (raw argv / JSON)        │
└───────▲──────────────────────────────▲───────────────────────▲────────┘
        │ REST (JSON) + Zod          │ SSE/WS: state, QMP events │ WS: VNC/SPICE
┌───────┴──────────────────────────────┴───────────────────────┴────────┐
│ API layer: authn/z, spec CRUD, capability cache, task/job API         │
└───────▲───────────────────────────────────────────────────────────────┘
        │ control RPC (local socket, token-auth)
┌───────┴───────────────────────────────────────────────────────────────┐
│ SUPERVISOR (the only thing that talks to QEMU)                        │
│  · VM registry (SQLite)   · process spawn/reap   · QMP client pool    │
│  · capability cache       · image ops (qemu-img) · reattach on boot   │
│  · compat engine + lint-run validator · audit log · console recorder  │
└───────▲───────────────────────────────────────────────────────────────┘
        │ QMP (unix socket / npipe) + VNC (localhost TCP)
┌───────┴───────────────────────────────────────────────────────────────┐
│ qemu-system-<arch>  × N   (detached · pidfile · QMP · VNC/SPICE · GA)  │
│ guacd (optional, loopback-only) ──► SPICE/VNC rich display engine      │
└───────────────────────────────────────────────────────────────────────┘
```

**Why a separate supervisor and not one process?** Because "keep sessions running" is a
*process supervision and re-attachment* problem, not a web problem. It also means we can
ship a headless `qemu-ui` daemon that a future CLI/TUI/mobile client can drive, and it keeps
QMP credentials off the HTTP surface entirely.

### Stack recommendation (my opinion)

| Layer | Pick | Why |
|---|---|---|
| Supervisor | **Rust** — tokio + axum, `serde`/`serde_json`, `tokio::process` ✅ **decided** | Process supervision, signal handling, spawn-without-shell, one static binary, precise argv control, typed QMP. Best long-term fit for a systems daemon that must be reliable for weeks. |
| Crates | `tokio`, `axum`, `rusqlite` (or `sqlx`), `serde`, `tracing`, `argon2`, `rustls`, `toml`, `notify`, `clap` (CLI parity) | Small, boring, well-maintained. |
| Rejected | Go (nearly as good, simpler for contributors); Python/FastAPI (best for a spike, worst deployment story on Windows) | Kept here so the reasoning is auditable, not to reopen it. |
| Frontend | React 19 + TypeScript + Vite | Ecosystem: noVNC, xterm.js, TanStack, shadcn. Nothing else is close for this UI class. |
| State | SQLite + JSON spec blobs, files for images | Zero-ops, transactional, one file to back up. Postgres later if multi-node. |
| Display | noVNC over QEMU's own VNC websocket (proxied) | See §7. |
| Serial | xterm.js ⟷ QMP `chardev` socket | Serial console is the fallback when graphics die. Non-negotiable. |

I'd *not* put the web server and the supervisor in one process even though it's tempting:
restarting a web server must not orphan VMs, and a supervisor bug must not take down the UI.

---

## 4. "All architectures, all machine types" — capability discovery

Do not hardcode a list of architectures. At install/first-boot and on a schedule
(configurable "rescan"), build a **host capability database**:

1. **Find binaries.** Scan `PATH` + configured dirs for `qemu-system-*` and `qemu-img`.
   Map exe → target arch (`x86_64`, `i386`, `aarch64`, `arm`, `riscv32/64`, `ppc64`,
   `s390x`, `mips*`, `loongarch64`, `sparc`, `avr`, …). Get `--version` per binary.
2. **Enumerate machines** per binary: `-machine help` (authoritative name list including
   versioned aliases like `pc-q35-9.1`) plus `query-machines` (flags: `hotpluggable-cpus`,
   `numa-mem-supported`, `deprecated`, `default-cpu-type`, `default-ram-id`, `alias`).
3. **Enumerate CPUs**: `query-cpu-definitions`, and `-cpu help` as fallback. Per-model
   features via `query-cpu-model-expansion` (expensive — cache, and only on demand).
4. **Enumerate devices**: `-device help` (name + bus + description) then, per machine,
   `device-list-properties` for the devices the user tries to add (lazy, cached by
   (binary, machine, device)). This is what makes "which properties does *this* device have
   *on this* machine" correct.
5. **Enumerate objects/chards/nets/audiodevs**: `-object help`, `query-command-line-options`
   (gives option → parameters → types/enums for many options), `-netdev help`,
   `-chardev help`, `-audiodev help`, `-accel help`, `-display help`, `-nic help`.
6. **Enumerate accels and their availability**: KVM (Linux, `/dev/kvm`), WHPX (Windows,
   needs Hyper-V platform), HVF (macOS), NVMM (NetBSD), TCG (always). Record *why* one is
   unavailable so the UI can explain it.
7. **Firmware inventory**: scan for `bios-256k.bin`, `edk2-x86_64-code.fd`,
   `edk2-aarch64-code.fd`, `edk2-riscv-code.fd`, `u-boot.e500`, `openbios-sparc64`,
   `s390-ccw.img`, UEFI var templates. Machine → bootable-without-firmware vs needs-firmware.
8. **QAPI schema**: `query-qmp-schema` once per binary → typed commands/events. Bonus:
   this tells us which *runtime* ops exist (hotplug, blockdev, migration, stats) so we can
   hide buttons QEMU can't do for this version instead of disabling at click time.

Capability snapshot is versioned and stored; the UI tags every VM spec with the
`(binary, qemu version, machine alias)` it was built against, so re-probing never
silently mutates an existing VM.

**Cross-arch reality check:** non-native archs are TCG-only (no KVM/WHPX/HVF), so they're
10–100× slower. The UI must show an honest badge — `native (WHPX)`, `emulated (TCG, slow)`
— rather than letting people build a riscv64 desktop and wonder why it crawls.

---

## 5. Compatibility engine (the "knows enough about QEMU" part)

Three layers, in order of trust:

### L1 — Structural, derived from introspection (high confidence)
- Machine has buses: PCI/PCIe (q35), ISA (i440fx/malta), USB (piix3-uhci/pcie), SCSI,
  virtio-mmio (arm `virt`, riscv `virt`), CCW (s390x).
- Device declares its bus (`-device help` second column, or `qom-list-properties`); if the
  machine has no such bus → incompatible.
- Properties exist per (machine, device) — `device-list-properties` is the source of truth.
- `query-hotpluggable-cpus` → whether CPU topology is editable at runtime.
- Machine max RAM (`query-machines` + chipset tables), max CPUs, NUMA support.
- Firmware requirement came from the scan.

### L2 — Curated semantic rules (medium confidence, versioned)
Things QEMU doesn't introspect. Shipped as a data file, override-able, each rule with a
citation and a `verified_against: ["qemu 9.1", "qemu 10.0"]` field:
- `i440fx` has no PCIe → `nvme`, `virtio-vga-gl`, PCIe root-port features rejected.
- `ps2-*` requires `i8042`; `usb-kbd` requires a USB controller.
- `virtio-blk-pci` needs PCI; on `virt` (arm) prefer `virtio-blk-device`/mmio.
- `vmport` requires i440fx; `hpet`/`acpi` quirks per chipset.
- ARM `virt` + `-cpu host` invalid without KVM; `max` vs named models per arch.
- `-smp` sockets×cores×threads > machine max.
- OVMF + `-machine ... ,smm=on` for Secure Boot; swtpm required for Windows 11 TPM.
- Audio: `ich9-intel-hda` vs `AC97` vs `virtio-sound` by chipset/arch.
- GPU: `virtio-vga` needs PCI and a BIOS/UEFI GOP story; `VGA`+`bochs-display` tradeoffs.
- s390x: no PCI graphics at all → "graphics not supported on this target" (honest messaging).
Each rule can *forbid*, *warn*, or *auto-fix* (e.g. swap `virtio-blk-pci` →
`virtio-blk-device` on a mmio-only machine, with a visible "we changed this" chip).

### L3 — Live lint run (authoritative)
Pre-flight spawn as described in P4. Runs: on "Save & Start", on demand ("Check config"),
and in CI against a matrix of seeded specs. Results cached by argv hash. Stderr is parsed
into `(severity, message, arg_index)` so the error is pinned to the exact form field.
**This is the escape hatch that keeps us honest on machines/archs we've never modelled.**

The UI contract: incompatible things are *explained*, not just greyed out —
"You can't add a virtio-scsi controller here because machine `pc-i440fx-9.1` has no PCIe bus."
Greyed-out controls with a hover reason beat mystery validation errors.

---

## 6. Session lifecycle & persistence

### VM states
`defined` → `starting` → `running` ⇄ `paused` / `suspended (state on disk)` → `stopped` /
`crashed`. Plus `waiting-for-install-media`.

### Operations
| Op | Mechanism |
|---|---|
| Start | supervisor spawns detached QEMU + QMP connect + readiness wait |
| Stop (graceful) | QMP `system_powerdown`, timer, then ACPI-less fallback prompt |
| Stop (hard) | `quit` via QMP; escalate to SIGTERM/SIGKILL (Windows: `TerminateProcess`) |
| Reset | QMP `system_reset` |
| Pause | QMP `stop` (freezes vCPUs, keeps RAM) |
| **Suspend to disk** | `migrate` to file (`migrate "exec:cat > vm.state"`) + stop; resume with `-incoming file:` — full RAM state persisted, survives host reboot |
| Snapshots | qcow2 internal (`savevm`) or external overlay chain; per-disk snapshot sets |
| Autostart | per-VM policy at supervisor boot (with a "crash-looped" circuit breaker — important, because a VM that dies on start will otherwise thrash) |
| Console recording | noVNC frames → WebM/PNG-sequence; and/or Guacamole session recording |
| Reattach | on supervisor start: read registry, probe pidfile + QMP socket, reconcile actual vs expected state, mark orphans `crashed` |

### Events
Subscribe to QMP events (`SHUTDOWN`, `RESET`, `STOP`, `GUEST_PANICKED`,
`BLOCK_IO_ERROR`, `DEVICE_TROUBLE`, `DEVICE_DELETED`, `MIGRATION`, `SPICE_*`, `VNC_*`,
`BALLOON_CHANGE`, `MEM_UNPLUG_ERROR`) and fan out to browsers over SSE. Browsers are
stateless: reconnect → full snapshot + event replay from cursor. No sticky sessions.

### Multi-VM
VM library with tags, search, folders; **wallboard view** with live thumbnails
(`screendump` → PNG every N seconds, downscaled, cached — cheap and shockingly useful);
tab strip of open consoles; per-VM resource sparklines (`query-stats`, `/proc`, guest agent).

---

## 7. Display in the browser

**Decision: go all-in, but stage it so v1 isn't held hostage by the hardest engine.**

### 7.1 Engine strategy — three engines, one internal interface

| Engine | Role | Why it exists |
|---|---|---|
| **noVNC over our own WSS proxy** | default fast path | ~40 KB client, zero extra daemons, works with *every* QEMU build and every arch, lowest latency for installers/desktops/terminals. |
| **Guacamole (`guacd`)** | rich path: SPICE, audio, multi-monitor, SFTP file transfer into the guest, printing, session recording | guacd already speaks VNC *and* SPICE and packages years of protocol work (audio, recording, file transfer) behind one socket. Cheaper than reimplementing it. |
| **Native handoff** (`remote-viewer` / `.vv` download) | escape hatch | When someone needs real USB redirection or hardware video decode, hand them the native client in one click instead of pretending the browser can do it. |

Display engine is a **per-VM setting with a per-viewer override**:
`vnc:novnc` (default) · `vnc:guac` (same VNC, but you get recording/SFTP) ·
`spice:guac` (audio + multi-monitor + video codecs) · `spice:native`.

One internal abstraction so the UI never guesses:

```rust
trait DisplayEngine {
    fn attach(&self, vm: &VmId, viewer: &Viewer) -> Result<Attachment>; // ticket + transport
    fn capabilities(&self) -> EngineCaps;  // clipboard, audio, usb, multihead, record, files
    async fn screenshot(&self, vm: &VmId) -> Result<Png>;
}
```

Every console control (Ctrl+Alt+Del, clipboard, sound, USB, record) is rendered from
`EngineCaps`, so a control is *absent* rather than broken when the engine can't do it.

### 7.2 VNC path (default, v1)

QEMU's VNC is served **loopback-only** (`-vnc 127.0.0.1:<allocated>`) and proxied by us:

```
noVNC  ──wss──►  /ws/vnc/{vmId}  ──►  127.0.0.1:<port>
   (ticket-authenticated upgrade)         (one VNC port per VM, allocated from a pool)
```

Proxy responsibilities, and why we don't just use QEMU's built-in `websocket=`:
- auth per viewer (short-lived ticket → never a long-lived token in a query string),
- Origin/CSRF checks on the upgrade (WebSocket upgrades bypass normal form protections),
- TLS termination, connection limits, idle timeouts,
- viewer presence ("2 viewers, 1 read-only"), take-control lock, kick,
- optional frame tap for recording and dashboard thumbnails.

Details that make it feel native rather than laggy:
- **`-device qemu-vdagent,clipboard=on,mouse=on`** → real text clipboard both ways and
  *absolute* pointer positioning, so the cursor doesn't drift. Needs the guest driver
  (`spice-vdagent` on Linux, virtio-win on Windows). This single device is the difference
  between "remote desktop from 2003" and "it just works".
- Encodings: prefer `tight` → `zrle` → `hextile`; TCP_NODELAY on; never `raw` on LAN.
- Controls: fullscreen, fit/scale/1:1, send-key chord builder, dedicated Ctrl+Alt+Del,
  Ctrl+Alt+F1…F12, grab/detach mouse, screenshot (`screendump`), record, paste-as-keystrokes.
- **Serial console in xterm.js alongside** — the graphics+serial split pane is how you
  actually debug a guest that won't boot. Independent of the graphics engine.

### 7.3 SPICE path (the "rich" engine)

Backed by QEMU SPICE on loopback + guacd as the browser-facing protocol translator:

```
-spice port=<alloc>,addr=127.0.0.1,disable-ticketing=on,image-compression=auto
-device virtio-serial-pci
-chardev spicevmc,id=vdagent,name=vdagent -device virtserialport,chardev=vdagent,name=com.redhat.spice.0
-chardev spicevmc,id=usbredir1,name=usbredir -device usb-redir,chardev=usbredir1
-audiodev spice,id=snd0
```

What you actually gain: audio in the browser, multi-monitor heads, video-streaming
detection (mjpeg/h264 for real motion), smartcard/USB redirection plumbing, better
cursor handling, and dynamic resolution changes.

**Honest caveats to put in the docs, not bury:**
- Upstream SPICE is in *maintenance mode*. It works and it's stable, but don't plan new
  features on it. That's exactly why it's engine #2 and not the foundation.
- `spice-html5` is stale/dead — do **not** build on it. guacd (or a native-client handoff)
  is the pragmatic browser path.
- Browser **USB redirection is genuinely hard** (WebUSB + usbredir tunnel, plus per-device
  driver/permission chaos). Plan it as "experimental, mouse/keyboard/serial dongles only"
  and keep native handoff as the real answer.
- `guacd` is a separate JVM-free C daemon (good) but a separate process with its own
  connection auth — it must only ever listen on loopback and accept connections from us.

### 7.4 Recording, screenshots, thumbnails

One capture path serving three features:
- **Dashboard thumbnails**: `screendump` → PNG every N s → downscale → cache. Cheap, and it
  turns the VM list into something you actually glance at.
- **Screenshots**: on demand, stored with the VM's timeline.
- **Recording**: for noVNC we tap the proxy's framebuffer updates and mux to WebM (no guest
  involvement, works on every arch); for guacd engines we can use native session recording.
  Recording markers ("bookmark this moment") + export clip.

### 7.5 Performance expectations (LAN, single user — the easy case)

- 1080p desktop: 30–60 fps, feels local. 1440p/4K desktop on wired LAN: usable with `tight`;
  dial colour depth down for slow guests.
- Video playback: fine-ish on SPICE, poor on VNC. Say so.
- 3D/GPU: out of scope for the browser. If it's ever needed, it's `virtio-gpu` + native
  client, and it gets its own milestone — not a checkbox in v1.
- WAN/remote access: out of scope by decision; the reverse-proxy/TLS story is documented so
  someone can put it behind Tailscale/WireGuard without us building auth for the internet.

---

## 8. Storage

### 8.1 Image store
A configured set of **storage pools** (dirs), each with: quota, default format, allowed
operations, and an "shared/read-only" flag. Images are first-class records
(`id, pool, path, format, virtual_size, actual_size, backing_chain, checksum, locked_by`).

Features:
- Create (`qemu-img create`, format/size/preallocation/cluster size/LUKS encryption),
  resize (`qemu-img resize`, plus live `block_resize`), convert/import (raw, qcow2, vmdk,
  vhd/vhdx, vdi, vpc, qed, ISO), check/repair, compact (`qemu-img convert -O qcow2` + sparsify).
- `qemu-img info --output=json` for truth; `qemu-img map` for allocated extents;
  `qemu-img measure` to *predict* size before an operation (great for a confirmation dialog).
- Backing chains rendered as a visual chain; "merge/flatten" and "rebasing is dangerous" warnings.
- Per-image advisory locks + a real `flock`/lockfile so two VMs can't corrupt one disk.
- **Snapshot UI**: tree of qcow2 snapshots per image, revert/delete, plus "runtime snapshot"
  grouping a memory snapshot with the disk snapshot set.

### 8.2 Physical locations as disks
- Linux: `host_device`/`host_cdrom` on `/dev/sdX`, `/dev/nvme0n1`, `/dev/sr0`, LVM LVs,
  and `zvol`s. Must be unmounted; we check (`/proc/mounts`, `/sys/block/*/holders`) and
  refuse rather than corrupt.
- Windows: `\\.\PhysicalDriveN` works only if the disk is **offline / has no mounted
  volumes** (Windows locks mounted volumes). So the UX must be: detect → explain → offer
  `diskpart`-free guidance (or a "USB attach" alternative). This is the single biggest
  Windows-host wart; be honest about it in the UI rather than failing cryptically.
- Whole-disk read-only attach is the safe default (forensics, installers, recovery).
- **Better default for "map a physical thing":** USB passthrough (`usb-host` by
  vendor:product or bus:addr, hot-pluggable at runtime — great for USB sticks,
  YubiKeys, serial adapters) and folder sharing (9p `-virtfs` on Linux/macOS;
  virtiofs via `virtiofsd` where available). Both avoid the offline-disk problem.

### 8.3 Attaching images from elsewhere (browser + remote)
Three distinct flows — worth naming separately in the UI because users conflate them:

1. **Upload from browser** → chunked, resumable (tus-style: create/HEAD/PATCH/complete),
   checksummed, progress with pause/resume, survives tab close (server-side session),
   writes into a pool. For multi-GB ISOs this is the main path, so it must be resumable and
   not buffered in memory. Optional: write straight into a preallocated qcow2/raw file.
2. **URL fetch** → server-side download job with progress/ETA, or — where it's an immutable
   image — QEMU's own block drivers: `https://` (read-only, curl-backed), `nbd://`,
   `iscsi://`, `rbd:`, `ssh://`, `gluster://`. Read-only remote attach is a genuinely nice
   trick: boot an installer ISO straight off a URL with zero local disk use.
3. **Peer attach** → another qemu-ui instance exposes a pool as NBD (`qemu-nbd`), or the
   browser uploads *to a remote* via the same resumable API. Also covers "attach a disk
   that lives on my NAS".

All flows land in the same image registry so a disk is a disk regardless of origin.
Upload of a *live* disk is refused; upload-then-attach is a two-step job with a queue.

### 8.4 Disk attachment semantics
- Controllers: `virtio-blk-pci`, `virtio-scsi-pci` (+ per-disk `scsi-hd`), `nvme`, `ide-hd`,
  `ahci`, `usb-storage`. Picker filtered by machine (see §5) with an explanation of tradeoffs
  (virtio = fastest but needs drivers; IDE/AHCI = works with stock installers).
- Cache modes (`none/writeback/writethrough/directsync/unsafe`) and `aio`
  (`threads/native/io_uring`) with real guidance: `cache=none,aio=io_uring,discard=unmap`
  for Linux guests; warn on `unsafe`.
- Media: disk vs CD-ROM vs floppy; boot order per device; `readonly=on`; `snapshot=on`
  (ephemeral).
- Hotplug/unplug live via `device_add`/`device_del` (with `query-block` confirmation and
  an "eject before unplug" guard).

---

## 9. Networking

### 9.1 Network objects
Reusable, named network definitions (so 5 VMs share "LAN" without duplicating widgets):

| Type | Backend | Host privileges | Use |
|---|---|---|---|
| NAT (user/slirp) | `-netdev user` | none | default; outbound + `hostfwd` port forwards |
| NAT (passt) | passt/pasta backend | none | better perf, IPv6, live port-forward control socket |
| Isolated | `-netdev socket,mcast=` or userspace vswitch | none | VM↔VM only, no host stack |
| Bridged | `tap` + bridge / Windows external vSwitch | admin/root | real LAN presence, DHCP from LAN |
| Host-only | `tap` + host bridge w/o uplink | admin | host↔VM |
| Passthrough | VFIO PCI NIC | root + IOMMU | near-native; single VM |
| None | — | — | explicit |

### 9.2 Models & features
`-nic model=`: `virtio-net-pci` (+ multiqueue, `vhost=on`, `vhost-user`),
`e1000/e1000e`, `rtl8139`, `ne2k_pci`, `vmxnet3`; arch-appropriate (virtio-net-device on mmio).
Optional virtio-net features surfaced as toggles: `mq` + queue count, `csum/guest_csum`,
`gso`, `tso`, `ufo`, rx/tx queue size, MAC address (with locally-administered random
default), `vlan`/bridge, MTU.

### 9.3 Port forwarding UX
- slirp `hostfwd` is **creation-time only** → UI must say "requires VM restart", offer
  "apply & restart" as an explicit action.
- passt: dynamic forwarding via its control socket where the build supports it → live
  add/remove; verify per QEMU/passt version at runtime and disable the control otherwise.
- Auto-detect guest IPs via guest agent (`guest-network-get-interfaces`) and show a
  "connect to guest" card (RDP 3389, SSH 22, HTTP 80) that proposes the right hostfwd.
- Basic firewall/TLS advice, plus "expose on all interfaces" as an explicit dangerous toggle.
- Built-in port scan of the guest (optional) to propose forwards.

### 9.4 Bridging on Windows (be blunt in the docs)
Requires the TAP-Windows driver (OpenVPN) or a Hyper-V external switch, plus admin rights at
QEMU start. There is no unprivileged bridge. passt is the recommended Windows answer.

---

## 10. UI / UX

### Information architecture
1. **Dashboard** — VM cards (state, thumbnail, CPU/RAM/disk sparkline, IP, uptime),
   quick actions (start/stop/console/snapshot), global health (accel available? pools low?
   orphaned VMs?).
2. **New VM wizard** — 3 lanes, not one long form:
   - *Guided*: OS picker (Linux/Windows/BSD/Other) + arch + ISO → sane defaults, all
     pre-validated. Windows lane pre-adds TPM 2.0, UEFI, virtio-win ISO, Hyper-V enlightenments.
   - *Custom*: full machine/cpu/mem/device editor with live validation rail.
   - *Import*: libvirt XML, existing disk image, or raw QEMU command line.
3. **VM detail** — tabs: Overview · Console (graphics/serial split) · Hardware · Disks ·
   Network · Snapshots · Logs/Events · Settings. A persistent **"argv preview"** drawer showing
   the exact command line (copyable) and the diff from the last run.
4. **Hardware editor** — the schema-driven device tree: buses → devices → properties,
   with drag-to-add from a filtered palette, and "why is this greyed out?" everywhere.
5. **Images** — pools, uploads with resume, import/convert jobs, chain visualiser.
6. **Networks** — network objects + per-VM attachments + port-forward table.
7. **System** — capability report (what QEMU binaries/accels/firmware we found and why),
   storage pools, host resources, audit log, API tokens.
8. **Expert** — raw argv playground (lint-run backed), QMP console (auth-gated, scary colour),
   schema browser.

### Interaction details that decide whether this feels good
- **Live argv**: every form change shows the compiled command line and a diff. This is the
  single feature that makes "full CLI support" tangible.
- **Never lie about state**: optimistic UI is fine for cosmetic things; never for "running".
  Show `starting…` until QMP says running.
- **Explain every disable.** Tooltip with the rule (`machine has no PCIe bus`) + docs link.
- **Warnings ≠ errors.** "You have no accel; this will be slow" is a warning you can override.
  "This device needs a bus you don't have" is an error.
- **Job model for slow things**: qemu-img convert, uploads, downloads, snapshot merges are
  jobs with progress, cancel, and logs — not spinners on a button.
- **Keyboard-first**: `g d` dashboard, `g c` console, `/` search, send-key palette.
- **Small-screen fallback**: console still works; editors collapse to accordions.

### The hard UI problem, stated plainly
"Full support for all command-line options in the UI" cannot mean 4,000 bespoke widgets.
It means: **everything QEMU advertises is reachable** — through a generated editor for the
long tail, a curated editor for the short head, and a raw escape hatch with validation and
round-tripping. If we can't render a property, we must still be able to *reach* it.

---

## 11. Data model (sketch)

```
host_capability(id, qemu_version, binary_path, target_arch, probed_at, raw_json)
machine(id, arch, binary, name, alias, is_default, max_cpus, max_ram_mb, flags_json)
device(id, arch, binary, machine, name, bus, abstract, user_creatable, props_json)
image(id, pool, path, format, virtual_size, actual_size, backing_id, mode, locked_by_vm)
pool(id, name, path, quota_bytes, formats_allowed, shared)
vm(id, name, spec_json, arch, machine, accel, state, qmp_socket, pid, started_at,
   created_at, spec_version, last_argv, last_argv_hash, autostart, crash_count)
vm_run(id, vm, argv, started_at, ended_at, exit_code, stderr_path)
disk(id, vm, image_id, bus, controller, boot_index, cache_mode, aio, readonly, media)
nic(id, vm, network_id, model, mac, mtu, mq, queues, link_up)
network(id, name, kind, bridge, cidr, dhcp, forwards_json, host_priv_required)
snapshot(id, vm, kind, name, mem_state_path, disks_json, created_at, parent_id)
port_forward(id, nic, proto, host_ip, host_port, guest_ip, guest_port)
job(id, kind, status, progress, message, log_path, created_at, finished_at)
user(id, name, hash, role), token(id, user, hash, scope, expires_at)
audit(id, at, user, action, target, detail_json, argv_hash, result)
```

VM spec (the compiler input) is a versioned JSON document; specs are also exportable as
files so people can keep them in git ("infrastructure as code", §16).

---

## 12. Security & threat model

**Blunt truth: a UI that can spawn QEMU with arbitrary disks, host devices, and network
backends is equivalent to root on the host.** Plan accordingly.

**Decided model: single user, exposed on the LAN.** That is *not* the same as "no auth" —
a LAN is a shared network (Wi-Fi passwords get shared, roommates/guests/clients exist), and
browser APIs we need (clipboard, WebUSB, crypto) require a **secure context**, so TLS is
mandatory anyway. Concretely:

- **Auth**: first-run setup creates one admin account (argon2id password). Every request and
  every WebSocket upgrade is authenticated; there is no anonymous mode, ever. Sessions are
  `HttpOnly; Secure; SameSite=Strict` cookies; CSRF token on state-changing requests.
- **WS tickets**: console/serial/event sockets never accept the session cookie or a token in
  the query string. The client asks the API for a single-use, 30-second ticket and passes it
  as a WebSocket subprotocol; the proxy validates `Origin` too.
- **Transport**: TLS by default on first run (self-signed cert + guided "trust this cert"
  flow), WSS for consoles, HSTS, no mixed content. Bring-your-own cert supported. HTTP-only
  mode is loopback-only and opt-in.
- **Designed for multi-user later, not built now**: all authorization funnels through one
  middleware (`can(actor, action, vm) -> bool`) with a single-user implementation. Adding
  RBAC/owners/share links later is additive, not a rewrite. The schema already carries
  `user`/`token`/`audit` tables so the migration doesn't hurt.
- **Keep audit + quotas even single-user** — they're for debugging and forensics (and for the
  "which config did I run at 2am" question), not just for tenants.
- **Treat the LAN as hostile**: no debug endpoints, no directory listings, no `/proc` reads,
  rate limits on auth attempts, and a loud "this is root-equivalent, don't port-forward it"
  warning at first run.
- **Command execution**: never a shell. argv arrays only, `exec` without `shell=True`, no
  string interpolation into `-drive file=`. Paths validated against the pool allowlist with
  canonicalisation (block `../`, symlinks out of pool, UNC/`\\.\` outside an explicit
  allowlist). Machine names, MACs, sizes, and formats are enum/regex validated.
- **QMP exposure**: unix socket (0600) or Windows named pipe ACL'd to the service account,
  never TCP, never proxied to the browser except through our own audited command allowlist.
- **QEMU hardening**: `-sandbox on,obsolete=deny,elevateprivileges=deny,spawn=deny,resourcecontrol=deny`
  where the platform supports it; no `-usb-host`/VFIO in shared mode; guest agent socket
  never mounted into the guest.
- **Isolation**: Linux → one unprivileged UID per VM, cgroups v2 for CPU/mem/IO limits,
  AppArmor/SELinux profile per QEMU; Windows → low-privilege service account, restricted
  token, job objects for process containment (and note: containment is weaker than Linux).
- **Resource abuse**: RAM/CPU/disk quotas per user and per VM; refuse overcommit beyond a
  threshold; per-VM disk IO throttling (`throttling.*`).
- **Supply chain / secrets**: image checksums, optional signature verification for ISOs;
  LUKS keys never stored in the spec, referenced from OS keyring/DPAPI.
- **Audit**: every state-changing action logged with actor, argv hash, result.
- **CSRF/SSRF**: URL-fetch jobs are an SSRF vector — allowlist schemes, block link-local/
  metadata IPs, cap size, require admin.
- **The `-monitor`/QMP console in the UI is root by another name** — admin-only, off by
  default, prominent warning, optionally entirely disabled by config.

---

## 13. Platform notes (this is where projects die)

| Concern | Linux (recommended host) | Windows (your current box) | macOS |
|---|---|---|---|
| Accel | KVM (near-native) | WHPX (needs Hyper-V platform; conflicts with VMware/VBox) | HVF |
| Bridge networking | `ip tuntap` + bridge, trivial | needs TAP-Windows driver / Hyper-V vSwitch + admin | `vmnet` (privileged helper) |
| Physical disk | `host_device` (/dev/sdX) | `\\.\PhysicalDriveN` **only if volumes offline** | `/dev/rdisk` (needs privileges) |
| Folder share | 9p `-virtfs`, virtiofs | none practical | 9p / virtiofs |
| USB passthrough | usb-host | usb-host works (limited device support, WinUSB/libusb) | usb-host, needs entitlements |
| Sandbox | `-sandbox on` + seccomp | unavailable | partial |
| Service | systemd unit | Windows service (needs a service wrapper) | launchd |
| Passthrough (VFIO) | yes | no (Hyper-V/VBS off, DDA unsupported for QEMU) | no |

**Decided: both platforms are first-class, with parity *where physically possible*.** The
honest formulation is a per-feature parity ledger, not a platform preference:

`native` · `degraded (works, worse)` · `needs-setup (we guide you through it)` · `impossible`

| Feature | Linux | Windows |
|---|---|---|
| Hardware accel | `native` KVM | `needs-setup` WHPX — detect and give real remediation (Hyper-V platform, `bcdedit`, reboot), incl. the "WHPX and VMware/VBox collide" warning |
| Bridged net | `native` tap+bridge | `needs-setup` — detect TAP-Windows; offer guided driver install or push users to passt |
| Physical disk | `native` | `needs-setup` — detect online volumes, explain offline requirement, offer a guided `diskpart` snippet + a USB-attach alternative |
| Folder share | `native` 9p / virtiofs | `impossible` — say so plainly, point at SFTP/virtio-win or a network share |
| USB passthrough | `native` | `degraded` (device/driver dependent) |
| QEMU sandbox | `native` | `impossible` — compensate with a low-priv service account + restricted token |
| Per-VM UID | `native` | `impossible` — compensate with job objects |
| VFIO PCI passthrough | `native` | `impossible` |
| Everything else (QMP, lifecycle, disks, snapshots, VNC, SPICE, archs, argvs) | `native` | `native` |

Two engineering consequences:
1. The ledger is **data, not code** — shipped as a capability table keyed by
   `(platform, qemu version)`, consumed by the UI to grey out with a reason and by CI to
   assert expectations. No `if windows` sprinkled through the frontend.
2. **CI runs the golden-spec matrix on both OSes**, so "Windows is supported" stays a tested
   claim rather than a README sentence. Divergence shows up as a failing test, not a bug report.

The capability probe (§4) makes most of this automatic: if bridging isn't available, the
"Bridged" option simply isn't offered, with a "why?" that names the missing driver.

Also: QEMU isn't installed on this machine. M0 includes an installer/detector that finds or
fetches QEMU (`winget`, `brew`, `apt`, or a managed download into the app data dir), verifies
it, and probes it.

---

## 14. Milestones

- **M0 — Skeleton & capability probe.** Rust supervisor + API + minimal SPA. Discover QEMU
  binaries, probe machines/cpus/devices/accels/firmware into SQLite, render "what can this
  host do". QEMU installer/detector (`winget`, `apt`, `brew`, or managed download). Platform
  parity ledger populated for both Windows and Linux from the first commit.
- **M1 — One VM, end to end.** Spec editor (machine/cpu/mem/firmware/boot/one disk), argv
  compiler, lint-run, detached start, QMP lifecycle, **noVNC console + xterm.js serial**,
  dashboard, live argv drawer. Persistence across web-process restart (re-attach). **This is
  the vertical slice that proves the thesis.**
- **M2 — Storage.** Pools, image CRUD, qcow2 snapshots, resumable upload, URL fetch,
  controllers filtered by machine, hotplug, backing chains/templates, physical disk attach
  (with the Windows offline-volume flow as a first-class guided path).
- **M3 — Networking.** Network objects, slirp + passt, port forwards (+ guest-agent IP
  detection), isolated nets, bridged (Linux native, Windows via TAP with detection/guidance),
  models/multiqueue.
- **M4 — Compatibility engine maturation.** Curated rule set, warn/auto-fix, per-machine
  device palette, seeded lint-run matrix across every installed arch, libvirt XML + raw argv
  import.
- **M5 — Rich display.** SPICE backend + `guacd` engine (audio, multi-monitor, SFTP, native
  session recording), per-viewer engine override, thumbnail tap, WebM recording, native
  client handoff. Consolidates the `DisplayEngine` abstraction built in M1.
- **M6 — Hardening (single-user, LAN).** TLS-first setup with cert trust flow, argon2id
  auth, WS tickets, audit log, quotas, QEMU `-sandbox` where available, per-VM UID on Linux,
  restricted-token service on Windows, CSRF/Origin enforcement.
- **M7 — Power features.** Suspend-to-disk (migration-to-file), guest agent integration,
  generic job system, cloud-init seeds, templates gallery, API/CLI parity (`qemu-ui` CLI
  speaking the same supervisor RPC).

Cross-cutting from M0: **golden tests** — a corpus of VM specs (one per arch/machine family)
run through compile + lint-run in CI **on both Windows and Linux**. That corpus is the
compatibility engine's regression suite, its documentation, and the proof that the parity
ledger is honest.

---

## 15. Decisions & remaining questions

### Locked

| # | Decision | Consequence baked into this spec |
|---|---|---|
| 1 | **Windows and Linux both first-class**, parity where physically possible | Parity ledger (§13) is data + a CI matrix on both OSes; no platform-preference ordering in milestones |
| 2 | **Rust supervisor** | Single static binary, `tokio::process` spawn without a shell, typed `serde` QMP models, one CLI binary that speaks the same RPC as the web UI |
| 3 | **Display: all-in, staged** | noVNC fast path in M1; SPICE + guacd + audio + multi-monitor + recording in M5; native client handoff always available |
| 4 | **Single user, LAN-exposed** | No RBAC now, but one authorization middleware + user/token/audit tables so multi-user is additive; TLS mandatory (secure-context + shared-network reality) |
| 5 | **Schema-generated long tail + curated short head + raw escape hatch** | §5 + §10: nothing QEMU advertises is unreachable; the raw argv drawer with lint-run is a v1 feature, not a "later" |

### Still open (I can decide these myself, but they change ergonomics)

1. **guacd distribution** — bundle per-platform `guacd` binaries with the app (heavier install,
   zero setup) or detect-and-guide an external install (lighter, one manual step)?
   *My lean: detect-and-guide for v1, bundle later once the packaging story is proven.*
2. **QEMU distribution** — use whatever's installed and probe it (supports side-by-side
   versions, which the capability model handles beautifully), or pin/bundle a known-good
   build? *My lean: probe-and-use-installed, with an optional managed download.*
3. **Default image location** — OS app-data dir (zero-config) vs force the user to pick a
   pool on first run (better for people with a big second drive). *My lean: propose a pool
   on first run, pre-filled with a sensible default.*
4. **Do you want "VM as a link" (disposable clones on demand) in the roadmap now?** It's the
   highest-wow feature in §16 and it forces the template/backing-chain design early — cheap
   to plan for, expensive to retrofit.
5. **macOS host?** Nothing in this design excludes HVF, but it has no place in the CI matrix
   yet; say if you care and I'll model it.

---

## 16. Spitball — things that would make this genuinely better than the alternatives

Ordered by "value per unit of effort", not by how cool they are.

**Cheap and high-impact**
- **Live thumbnails on the dashboard** via periodic `screendump`. Turns a VM list into
  something you actually glance at.
- **Guest agent integration** (qemu-guest-agent over a chardev socket): IP addresses,
  free disk space, graceful shutdown, filesystem freeze for snapshots, and "run this command"
  for provisioning. Huge UX win, small surface.
- **Copy-pasteable argv** on every screen. People will trust the tool once they can see it's
  just QEMU.
- **Cloud-init / NoCloud seed ISO generator** — "create Ubuntu VM with SSH key and package X"
  in one screen instead of an installer walkthrough.
- **Config-as-code export** — spec files in git, `qemu-ui apply spec.json`. Makes the whole
  thing scriptable and diff-reviewable.
- **"Explain this QEMU option"** inline docs from `-option help` + a local docs index.
- **Pre-flight lint before every start** with a friendly error pinned to the field.
- **Diff view**: "what changed since this VM last started?"

**Medium effort, high payoff**
- **Template gallery** with backing-chain clones (Ubuntu/Debian/Alpine/Windows/Win11+TPM,
  HAProxy appliance, k3s node, pfSense…). Instant clones = the reason people use Proxmox.
- **Snapshot timeline UI** with branch/merge visualisation, and auto-snapshot-before-risky-op.
- **Windows 11 lane** that does everything right: UEFI + Secure Boot keys + swtpm + virtio-win
  ISO + Hyper-V enlightenments + TPM + CPU topology. This alone is a differentiator.
- **Recording + replay**: QEMU record/replay (`-icount` + `-record`/`-replay`) gives
  deterministic VM replay with a scrubber and reverse execution. Genuinely novel in a web UI.
- **TCG plugin integration** (`-plugin`): instruction counters, hot-branch profiles, cache
  simulation, rendered per-VM. For anyone doing arch/firmware work, this is a killer feature
  no other web UI has.
- **GDB stub panel**: "attach gdb to this VM", surface breakpoints/backtrace in-browser.
- **Console recording to WebM** + shareable clips (great for bug reports / tutorials).
- **USB device shelf**: list host USB devices (lsusb/WMI), one-click hotplug, remember per-VM.
- **Multi-monitor** (VNC supports several heads; noVNC can render them as tabs).
- **Migration between two qemu-ui hosts** over the network (QMP `migrate` + shared storage or
  block migration). Turns it into a small private cloud.
- **Resource throttling UI** (cgroups on Linux) so a runaway VM can't eat the desktop.

**Ambitious / speculative**
- **Record-and-share "VM as a link"**: signed URL boots a disposable clone of a template on
  demand, auto-destroys. Great for demos, docs, and support ("here, reproduce my bug").
- **Time-travel debugging**: snapshot + replay + gdb stub + TCG plugin counters, all in one
  pane. Research-grade but nobody has it in a browser.
- **Firmware/OS autodetection**: peek at the first sectors of an image / the ISO volume label,
  infer the OS, pre-select drivers and the right boot setup.
- **Guided "why is my guest slow"** advisor: accel in use, missing virtio drivers (device
  manager heuristics via guest agent), `cache=unsafe` vs io_uring, CPU topology mismatch,
  memory ballooning, THP/KSM status.
- **HA-ish supervisor**: crash-loop detection, exponential backoff, "last known good config"
  rollback, alerting hooks.
- **Plugin SDK** for third-party lifecycle hooks (pre-start/post-stop), so people can wire in
  their own provisioning without us modelling it.
- **Mobile "monitor" mode**: list + thumbnails + stop button only.

---

## 17. What to steal from whom

| Project | Steal |
|---|---|
| **Cockpit Machines** | Console UX details, "why is this disabled" copy, snapshot tree UI |
| **virt-manager** | The new-VM wizard flow, device "add hardware" palette, CPU model picker |
| **Archoera / WebVirtMgr** | Simple web lifecycle patterns, noVNC wiring |
| **Proxmox** | Template/clone ergonomics, task/job panel, guest agent integration |
| **GNOME Boxes** | Zero-config "just boot the ISO" onboarding |
| **libvirt** | Capabilities-XML idea (machine+device matrix), domain XML import/export |
| **`qmp-shell` / `aqemu`** | QMP ergonomics, raw command surfaces |
| **Guacamole** | Display gateway features: recording, SFTP, audio, printing |
| **UTM (macOS)** | Polished QEMU-config UI on top of a raw hypervisor — closest spiritual cousin |

---

## Appendix A — Compiler sketch (illustrative, not final)

```json
{
  "name": "win11-dev",
  "arch": "x86_64",
  "accel": "whpx",
  "machine": {"type": "pc-q35-9.1", "props": {"smm": "on"}},
  "cpu": {"model": "host", "topology": {"sockets": 1, "cores": 8, "threads": 2}},
  "memory": {"size_mb": 16384, "balloon": true},
  "firmware": {"type": "uefi", "vars": "win11-dev_VARS.fd", "secure_boot": true},
  "tpm": {"backend": "swtpm", "version": "2.0"},
  "disks": [
    {"image": "win11-dev.qcow2", "bus": "virtio-blk-pci", "boot": 1,
     "cache": "none", "aio": "threads", "discard": "unmap"}
  ],
  "media": [{"image": "Win11.iso", "bus": "sata", "readonly": true}],
  "nics": [{"network": "nat", "model": "virtio-net-pci", "queues": 4}],
  "display": {"backend": "vnc", "vdagent": true},
  "passthrough": ["-global", "ICH9-LPC.disable_s3=0"]
}
```

```text
qemu-system-x86_64 -name win11-dev -accel whpx -machine pc-q35-9.1,smm=on
  -cpu host -smp sockets=1,cores=8,threads=2 -m 16384
  -drive if=pflash,format=raw,readonly=on,file=edk2-x86_64-code.fd
  -drive if=pflash,format=raw,file=win11-dev_VARS.fd
  -chardev socket,id=chrtpm,path=/run/swtpm/win11-dev.sock
  -tpmdev emulator,id=tpm0,chardev=chrtpm -device tpm-tis,tpmdev=tpm0
  -drive file=win11-dev.qcow2,if=none,id=disk0,cache=none,aio=threads,discard=unmap
  -device virtio-blk-pci,drive=disk0,bootindex=1
  -drive file=Win11.iso,media=cdrom,readonly=on,if=none,id=cd0
  -device ide-cd,drive=cd0,bus=ide.1
  -netdev user,id=net0 -device virtio-net-pci,netdev=net0,mq=on,queues=4
  -vnc 127.0.0.1:3 -device qemu-vdagent,clipboard=on,mouse=on
  -qmp unix:/run/qemu-ui/win11-dev.qmp,server=on,wait=off
  -pidfile /run/qemu-ui/win11-dev.pid -daemonize
  -global ICH9-LPC.disable_s3=0
```

Every token in that argv maps to a field in the spec, except the two `-global` entries which
come from `passthrough` — and the UI will say so. That traceability is the contract.
