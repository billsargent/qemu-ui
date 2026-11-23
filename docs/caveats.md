# Caveats

Things that do not work, or work less well than you might expect. The
[platform notes](platforms.md) list per-platform gaps feature by feature.

## Display

- Desktop use over a LAN is fine. Video playback over VNC is poor.
- 3D and GPU acceleration in the browser are out of scope.
- Use over a WAN is out of scope; put qemu-ui behind a VPN.
- The guacd display engine (audio, SPICE in the browser) is deferred: there is
  no released guacd with SPICE support and no native Windows build.
- On Windows, VNC runs over loopback TCP with a per-run password instead of a
  unix socket.

## Security

- qemu-ui is root-equivalent; see the [security model](security.md).
- Per-VM users on Linux and restricted-token QEMU on Windows are not
  implemented yet.

## Storage and snapshots

- After a live external snapshot, internal snapshot operations, disk
  hot-unplug and media changes are refused until the VM restarts.
- Config-as-code manifests reference disk images but do not create them, and
  do not yet cover pools, shared folders or templates.

## Lifecycle

- Resuming a suspended VM rebuilds devices that were hot-plugged at their
  boot-time positions. If QEMU rejects the saved state, discard it and start
  fresh.
- The autostart crash-loop counter lives in memory, so restarting the daemon
  resets it.
- Cloning a VM gives the clone a fresh TPM; TPM state is not copied.

## Windows guests

- Windows 11 needs a TPM, which needs `swtpm` and a QEMU build with TPM
  support. The upstream Windows QEMU builds have none; qemu-ui then explains
  Microsoft's documented setup bypass and asks before using it.
- Installer and virtio-win ISOs are not downloaded for you.

## Verification

Some paths are tested only on one platform in CI or skip when a tool is not
installed (for example `swtpm`). The GitHub issues track known gaps and flaky
tests.
