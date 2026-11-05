//! Target-architecture helpers.

/// Extract the target architecture from a QEMU system emulator file name.
///
/// Accepts `qemu-system-x86_64`, `qemu-system-x86_64.exe`. Returns `None`
/// for anything that is not a system emulator.
pub fn target_from_filename(file_name: &str) -> Option<&str> {
    let stem = file_name.strip_suffix(".exe").unwrap_or(file_name);
    let arch = stem.strip_prefix("qemu-system-")?;
    (!arch.is_empty()).then_some(arch)
}

/// Normalise a host architecture name (as reported by `std::env::consts::ARCH`)
/// to QEMU's target naming.
pub fn host_to_qemu(host: &str) -> &str {
    match host {
        "x86" => "i386",
        "powerpc64" => "ppc64",
        "loongarch64" => "loongarch64",
        other => other,
    }
}

/// Whether `target` can run with hardware acceleration on `host` (both in
/// QEMU naming). Hardware accelerators only run guests of the host's own
/// architecture family; everything else is TCG emulation.
pub fn is_native(target: &str, host: &str) -> bool {
    if target == host {
        return true;
    }
    matches!(
        (host, target),
        ("x86_64", "i386") | ("aarch64", "arm") | ("ppc64", "ppc")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_target_names() {
        assert_eq!(
            target_from_filename("qemu-system-x86_64.exe"),
            Some("x86_64")
        );
        assert_eq!(target_from_filename("qemu-system-riscv64"), Some("riscv64"));
        assert_eq!(target_from_filename("qemu-img.exe"), None);
        assert_eq!(target_from_filename("qemu-system-"), None);
    }

    #[test]
    fn native_mapping() {
        assert!(is_native("x86_64", "x86_64"));
        assert!(is_native("i386", "x86_64"));
        assert!(!is_native("aarch64", "x86_64"));
        assert!(is_native("arm", "aarch64"));
    }
}
