//! Pure, I/O-free core of qemu-ui: capability model, parsers for QEMU's
//! help output and firmware descriptors, and the platform parity ledger.
//!
//! Everything here is deterministic and unit-testable without a QEMU binary;
//! the daemon does the I/O and feeds the results through these types.

pub mod arch;
pub mod buses;
pub mod caps;
pub mod compile;
pub mod devices;
pub mod display;
pub mod export_libvirt;
pub mod fetch;
pub mod firmware;
pub mod guest;
pub mod guest_net;
pub mod help;
pub mod helper;
pub mod hostnet;
pub mod import;
pub mod import_argv;
pub mod import_libvirt;
pub mod iso9660;
pub mod limits;
pub mod lint;
pub mod manifest;
pub mod network;
pub mod nic;
pub mod parity;
pub mod recording;
pub mod rfb;
pub mod rules;
pub mod security;
pub mod snapshot;
pub mod spec;
pub mod storage;
pub mod structural;
pub mod templates;
pub mod upload;
pub mod usb;
pub mod vm;
