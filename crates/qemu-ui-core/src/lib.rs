//! Pure, I/O-free core of qemu-ui: capability model, parsers for QEMU's
//! help output and firmware descriptors, and the platform parity ledger.
//!
//! Everything here is deterministic and unit-testable without a QEMU binary;
//! the daemon does the I/O and feeds the results through these types.

pub mod arch;
pub mod caps;
pub mod compile;
pub mod firmware;
pub mod help;
pub mod lint;
pub mod parity;
pub mod snapshot;
pub mod spec;
pub mod storage;
pub mod vm;
