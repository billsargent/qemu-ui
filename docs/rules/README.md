# Validation rules

Every diagnostic qemu-ui shows carries a stable rule id. This page explains
each one; the UI links to `#<rule-id-without-dots>` here
(`qemu_ui_core::structural::docs_link`).

Layers (plan.md §6):

- **L1 structural**: facts the probed QEMU binary reports about itself:
  its devices, CPU models, machines and the machine's bus tree. High
  confidence, so most are errors.
- **L2 curated**: semantic rules kept as data (`data/rules/`), warnings by
  default.
- **L3 QEMU**: QEMU's own verdict from "Check config" or a start. Those
  messages are mapped back to spec fields by the reserved `qui-` ids.

## L1 structural rules (M4-01)

### `device.unknown`
A `-device` driver in passthrough that this QEMU binary does not have
(`qemu-system-<arch> -device help` lists the ones it has). QEMU would refuse
to start. Devices chosen in the editor have their own rules
(`nic.model-missing`, `display.device-missing`, `disk.device-missing`).

### `device.no-user`
The device exists but QEMU only creates it internally; it cannot be added
with `-device`. Pick another model, or turn off the feature that implies it.

### `device.bus-unavailable`
The device plugs into a bus the machine does not have, e.g. a PCI display
adapter on a machine without PCI. Applies to the display device, the USB
controller implied by a USB tablet, USB disks or USB passthrough, the
virtio-serial controller used by the clipboard agent, and passthrough
`-device`s. NICs report `nic.model-bus`, disks `disk.bus-unavailable`.

### `cpu.deprecated`
Warning: the CPU model is marked deprecated by this QEMU version and may be
removed in a later one.

### `cpu.unavailable-features`
Warning, under TCG only: QEMU reports features of this CPU model that TCG
cannot emulate on this host. QEMU turns them off (or refuses with
`enforce`), so the guest sees a different CPU than the name suggests. `max`
or `qemu64` avoid this.

## Other structural checks

These existed before M4-01 and live next to the code they guard.

| Rule | Meaning |
|---|---|
| `arch.mismatch` | The spec's arch differs from the binary's. |
| `machine.unknown`, `machine.type` | The machine type isn't offered by this binary, or is empty. |
| `machine.unpinned-alias` | An alias like `q35` instead of a versioned machine (pinned at creation). |
| `machine.deprecated` | Warning: the machine type is deprecated. |
| `machine.props`, `machine.pflash-owned` | Machine properties that are malformed or owned by the firmware settings. |
| `accel.*` | Accelerator unknown, unavailable on this host, or TCG on a native arch (warning). |
| `cpu.unknown`, `cpu.host-needs-accel` | CPU model not offered; `host` without a hardware accelerator. |
| `cpu.zero`, `cpu.max-exceeded` | No vCPUs; more vCPUs than the machine's `cpu-max`. |
| `cpu.topology*` | Topology with zeros, or whose product differs from the vCPU count. |
| `memory.too-small` | Less memory than any guest can use. |
| `disk.*` | Image, bus, cache and AIO checks; `disk.bus-unavailable` uses the machine's bus tree. |
| `nic.*` | Model presence and bus (`nic.model-bus`), MACs, queues and virtio options, network attachment. |
| `display.*`, `input.*` | Display device and clipboard agent presence; VNC without a device; tablet support. |
| `usb.*` | USB passthrough rules. |
| `share.*` | Shared folders: roots, paths, tags and host support. |
| `forward.*`, `net.*` | Port forwards and host support for network kinds. |
| `firmware.*` | Firmware descriptors resolved for machine and arch; SMM for Secure Boot. |
| `passthrough.*` | Raw options the compiler owns, or ids in the reserved `qui-` namespace. |

Not a rule on purpose: `query-machines` reports `numa-mem-supported: false`
for current machine types, but that flag is about the legacy
`-numa node,mem=` form; memory backends (`memdev=`, used for virtiofs) still
work.
