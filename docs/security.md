# Security model

**qemu-ui is root-equivalent.** Anyone who can configure a VM can attach host
disks and devices to it, and expert mode can pass arbitrary options to QEMU.
Treat access to qemu-ui like an administrator login on the host.

The model is **one admin, reachable on a LAN**. Exposing it to the internet is
out of scope; put it behind a VPN such as WireGuard or Tailscale.

## Network exposure

- The daemon listens on loopback by default, and refuses any other address
  until first-run setup has created the admin account.
- Beyond loopback it serves HTTPS (`--tls auto`), with a generated CA and
  server certificate, or your own certificate. HSTS is sent over TLS.
- Plain HTTP is only allowed on loopback.

## Authentication

- One admin account, password hashed with argon2id. Setup is only accepted
  from loopback.
- Browser sessions: `HttpOnly; SameSite=Strict` cookies (`Secure` over TLS),
  12 hours idle and 7 days at most. Unsafe requests need a CSRF token.
- Failed logins back off exponentially per client address.
- API tokens for the CLI and scripts, stored hashed, revocable, and unable to
  manage accounts. A bootstrap token in the data directory works from the
  daemon's own machine only.
- WebSocket consoles need a single-use ticket and a matching `Origin`; no
  secrets travel in URLs.

## Expert surfaces

Passthrough options, raw QEMU arguments and the QMP console reach QEMU
unvalidated. They are off by default, need expert mode (admin only), and every
enable and use is written to the audit log.

## Audit log

Every state-changing API call, every refused request, and events such as
sign-in, certificate issue and expert-mode changes are recorded with actor,
target, outcome and client address. Request bodies are never stored. Entries
are kept for 365 days or 200,000 entries; export as CSV or JSON.

## QEMU isolation

- **Linux:** `-sandbox` when QEMU supports seccomp; per-VM cgroup v2 limits via
  the privileged helper; an AppArmor template under `data/apparmor/`. Running
  each VM as its own user is designed but not implemented yet.
- **Windows:** per-VM job objects for memory and CPU limits; a low-privilege
  service account. Starting QEMU with a restricted token is not wired in yet.
  Windows isolation is weaker than Linux, and the [platform notes](platforms.md)
  say so.

## Injection and paths

QEMU is spawned with argument arrays, never through a shell. Names, MACs, sizes
and enums are validated; paths are checked against storage pool allowlists.
URL downloads refuse loopback, link-local and (unless allowed) private
addresses.

## Privileged helper

A small separate binary does the few things that need root or administrator
rights (taps, bridges, physical disks, cgroups) from a fixed set of requests,
each validated and audited. See [Privileged helper](design/privileged-helper.md).
