# Spike M0-15: guacd SPICE support and Windows availability

Gates M5-04 (guacd display engine). Researched November 2026.

## Answers

### 1. SPICE in released guacamole-server

**Not released.** The latest guacamole-server release is **1.6.0** (19 June 2025); a 1.6.1
patch release was described as "near an RC" in the December 2025 board report. SPICE
support is tracked as GUACAMOLE-261, open since 2017:

- Server: guacamole-server PR #688 ("Add native SPICE protocol support
  (libguac-client-spice)", built on spice-client-glib) is an **open pull request**, not
  merged; an earlier attempt (PR #394) stalled with open items such as audio input. The
  PR's history shows it being carried as a vendored patch in a downstream build.
- Client: guacamole-client PR #1227 (SPICE connection form, protocol registration) and
  guacamole-manual PR #297 (docs) are likewise open proposals.
- A list reply about 1.6.1 says draft-state feature PRs are "likely not in 1.6.1".

So no released guacd speaks SPICE, and the maturity of clipboard, audio, multi-monitor
and USB redirection over guacd-SPICE cannot be assessed against a release. Using it would
mean building guacd from an unmerged branch, which we will not ship.

### 2. guacd on Windows

**No native Windows build.** guacd depends heavily on POSIX; libguac alone has
occasionally built on Windows (restored in 0.9.14, broken again later). GUACAMOLE-1841
(build and run guacamole-server on Windows via Cygwin/MinGW) is an open "Wish"; a
community MSYS2/MINGW64 effort builds libguac and the RDP plugin only, not a complete
guacd. Upstream guidance is to run guacd on Linux or in a Linux VM.

Options on Windows hosts: WSL2 (a distro plus networking into WSL), Docker/containers
(Docker Desktop/Hyper-V), Cygwin/MSYS2 (unsupported, partial). None fits a self-contained
app whose binaries live in a project-local `.tools/`; all are heavy external
dependencies. Windows parity: `needs-setup` at best.

### 3. guacd for VNC vs our noVNC path

guacd's VNC plugin would add:

- **Audio**, but only via a PulseAudio server reachable from guacd (guacd built with
  PulseAudio support). For QEMU that means `-audiodev pa` pointed at that server. Windows
  hosts have no PulseAudio, so no gain there.
- Server-side recording and the Guacamole protocol's bandwidth optimiser.

It would not add multi-monitor for VNC (QEMU's VNC is one head per display device, which
noVNC can already show one console per head), and clipboard and absolute pointer are
already handled by our vdagent + noVNC path. Recording (M5-03) is planned at our own proxy.
Cost: a second display stack (guacamole-common-js client, a guacd sidecar, a WS-to-guacd
bridge in the daemon), a new native dependency, and a second security surface.

### 4. Licensing, connection model, security

- Apache License 2.0 (server and client): compatible with this project.
- Connection model: the browser runs guacamole-common-js; the daemon bridges its
  WebSocket (same ticket and Origin rules as the VNC proxy) to guacd's TCP port and
  performs the Guacamole handshake (`select` protocol, `args`, then `size`, `audio`,
  `video`, `image`, then `connect` with the VNC/SPICE parameters), then relays
  instructions.
- Security: guacd has no authentication of its own; anything that can reach its port can
  make it connect anywhere. It must listen on loopback only (`-b 127.0.0.1`) on a random
  port started by the daemon, never be exposed, and only the daemon may choose
  connection parameters.

### 5. Linux feasibility

Distribution packages exist (Debian/Ubuntu `guacd`, Fedora `guacamole-server`) but often
lag upstream (1.4/1.5 in older LTS releases) and carry the distro's choice of optional
plugins (VNC/RDP/SSH, PulseAudio). Building from source needs a C toolchain, Cairo,
libjpeg, libpng, libuuid and per-protocol libraries (libvncserver; for SPICE,
spice-client-glib). `scripts/bootstrap.sh` could build a pinned release into `.tools/` on
Linux, but that is a sizeable native build for the limited benefit in section 3.

## Recommendation: no-go for now (defer M5-04)

guacd cannot deliver its headline feature (SPICE) from any release, and it does not run
natively on Windows. For VNC it adds only PulseAudio-dependent audio on Linux, at the cost
of a second display stack. Therefore:

- **Defer M5-04.** Re-open when a guacamole-server release ships GUACAMOLE-261 (watch
  PR #688 and the release notes), then re-run this spike. The likely outcome then is
  "Linux-only: detect a system guacd and guide setup".
- **Serve SPICE via M5-05** (native client handoff, `.vv` for remote-viewer): the honest
  path today for USB redirection, video and audio, on both OSes.
- **Browser audio**, if wanted sooner, is better done as its own small item (QEMU audio
  captured and streamed by the daemon) than by adopting guacd.

M5-04 becomes: "guacd engine: deferred; trigger is a guacamole-server release containing
SPICE support (GUACAMOLE-261). Until then SPICE is served by M5-05."

## Parity ledger update

| Feature | Linux | Windows |
|---|---|---|
| guacd engine | `unverified` to `needs-setup`: deferred; VNC-only via a system guacd, no released SPICE | `unverified` to `needs-setup`: deferred; no native guacd, WSL2/containers only |

## Sources

- GUACAMOLE-261 (SPICE): https://issues.apache.org/jira/browse/GUACAMOLE-261
- guacamole-server PR #688: https://github.com/apache/guacamole-server/pull/688
- guacamole-server PR #394: https://github.com/apache/guacamole-server/pull/394
- guacamole-client PR #1227: https://github.com/apache/guacamole-client/pull/1227
- guacamole-manual PR #297: https://github.com/apache/guacamole-manual/pull/297
- 1.6.0 release: https://guacamole.apache.org/releases/1.6.0/ and
  https://release-catalog.apache.org/guacamole-server/index.html
- 1.6.1 status: https://whimsy.apache.org/board/minutes/Guacamole.html and
  https://lists.apache.org/thread/fcjbc91927l2q0c6gkdwh66t7hjx675d
- Windows: https://issues.apache.org/jira/browse/GUACAMOLE-1841,
  https://issues.apache.org/jira/browse/GUACAMOLE-325,
  https://guacamole.apache.org/releases/0.9.14/,
  https://mail-archive.com/dev@guacamole.apache.org/msg06385.html
- VNC audio via PulseAudio: https://www.mail-archive.com/user@guacamole.apache.org/msg01313.html
