# Operations

How qemu-ui behaves in the situations a long-running host meets sooner or later.

## QEMU upgraded while VMs run

A running VM keeps the QEMU process it started with; upgrading the package or
replacing the binary does not touch it. qemu-ui records which build each run
started with. After the next capability scan (rescan from the System page, or a
daemon restart) a VM still on the old build shows **restarts on QEMU N.M**: its
next start uses the new binary. The spec stays pinned to its versioned machine
type (`pc-q35-11.1`, not `q35`), so the guest sees the same hardware after the
upgrade.

## Host sleep and hibernate

The daemon notices when the host was suspended (a housekeeping tick arrives far
later than scheduled). Guest clocks are behind after a resume:

- VMs with a guest agent get their clock set from the host (`guest-set-time`).
- Every running VM gets a `host-resumed` event saying whether its clock was
  corrected. Without an agent, the guest's own time sync (NTP) has to catch up.

Pausing VMs before the host suspends is not done yet.

## Disk full

Disks are attached with `werror=enospc,rerror=report`: when the host disk under
a VM's image fills up, QEMU pauses the guest instead of failing its writes, and
nothing is lost. The VM shows **paused · io-error** with the disk that hit the
error. Free space on the host disk, then press **Resume**. Other read and write
errors are reported to the guest as I/O errors, as real hardware would.

## Logs

Per VM, under its data directory:

- `serial.log`: serial console output across runs. It rotates to `serial.log.1`
  past 8 MiB, at start and (on Linux and macOS) while the VM runs.
- `logs/run-<time>.log`: QEMU's own output for each run, plus helper logs
  (`swtpm`, `virtiofsd`, `passt`) with the same prefix. The newest 10 runs are
  kept, within 64 MiB.
