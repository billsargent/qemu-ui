# qemu-ui

A browser-based control plane for QEMU: every architecture, every machine type,
every command-line option, real disk, network and display management, and VM
sessions that survive closing the browser or restarting the daemon.

qemu-ui is one daemon (`qemu-uid`) that discovers the QEMU binaries on the
host, probes what each can do, and runs each VM as its own detached QEMU
process. The web UI and the CLI (`qemu-ui`) talk to it over the same HTTP API.

## Where to start

- [Install](install.md): build from source, run as a service, first-run setup.
- [Platform notes](platforms.md): what works natively, what needs setup, and
  what is impossible on Linux and Windows.
- [Security model](security.md): read this before exposing qemu-ui to a network.
- [Caveats](caveats.md): limits we know about and do not hide.
- [Command-line client](cli.md): the `qemu-ui` CLI and config-as-code.
- [HTTP API](api.md): every endpoint, generated from the daemon.

## Reference

- [Validation rules](rules/README.md): every diagnostic id the editor shows.
- [Process survival](process-survival.md): how VMs outlive the daemon.
- [Privileged helper](design/privileged-helper.md): the small root/admin
  helper and its fixed request set.
- [Spike: guacd](spikes/m0-15-guacd.md): why the guacd display engine is deferred.

The full specification and roadmap is `plan.md` in the repository.
