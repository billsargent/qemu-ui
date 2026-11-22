//! Guest integration (M7-02, M7-03): the QEMU guest agent channel and the
//! cloud-init NoCloud seed.
//!
//! * **Guest agent**: a `virtserialport` named `org.qemu.guest_agent.0` on a
//!   unix-socket chardev `qui-ga0`. The daemon talks to `qemu-ga` in the
//!   guest through it (addresses, host name, OS, graceful shutdown, freezing
//!   file systems around live snapshots).
//! * **cloud-init**: a small ISO 9660 image labelled `cidata` with
//!   `meta-data` and `user-data`, attached read-only as a CD-ROM on its own
//!   virtio-scsi controller. Built by the daemon at each start.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::compile::Diagnostic;
use crate::spec::VmSpec;

/// Guest-side integration settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, TS, JsonSchema)]
#[ts(export)]
pub struct GuestSpec {
    /// Add a guest-agent channel (needs `qemu-guest-agent` in the guest).
    #[serde(default)]
    pub agent: bool,
    /// Generate a cloud-init NoCloud seed and attach it as a CD-ROM.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud_init: Option<CloudInit>,
}

/// What the seed configures. Either fill in the fields, or give complete
/// `user_data` (which then replaces the generated `#cloud-config`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, TS, JsonSchema)]
#[ts(export)]
pub struct CloudInit {
    /// Guest host name; defaults to the VM name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    /// Account to create; `None` configures the image's default user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Login password as a crypt(3) hash (`$6$…`). A plain password sent
    /// to the API is hashed before the spec is stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// Authorized SSH public keys.
    #[serde(default)]
    pub ssh_keys: Vec<String>,
    /// Raw user-data (`#cloud-config…` or a `#!` script), used verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_data: Option<String>,
}

/// The chardev the guest-agent port uses.
pub const AGENT_CHARDEV: &str = "qui-ga0";
/// The port name qemu-ga looks for.
pub const AGENT_PORT: &str = "org.qemu.guest_agent.0";
/// The seed's volume label, which NoCloud looks for.
pub const SEED_LABEL: &str = "cidata";

/// A crypt(3) hash we accept as already hashed.
pub fn is_password_hash(s: &str) -> bool {
    ["$6$", "$5$", "$y$", "$2b$", "$2y$"]
        .iter()
        .any(|p| s.starts_with(p))
        && !s.contains(char::is_whitespace)
}

/// RFC 1123 label rules, joined by dots.
fn valid_hostname(h: &str) -> bool {
    !h.is_empty()
        && h.len() <= 253
        && h.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
}

/// The host name a VM name maps to: invalid characters become '-'.
pub fn hostname_for(vm_name: &str) -> String {
    let h: String = vm_name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .take(63)
        .collect();
    let h = h.trim_matches('-').to_ascii_lowercase();
    if h.is_empty() { "vm".into() } else { h }
}

fn valid_user(u: &str) -> bool {
    let mut c = u.chars();
    c.next().is_some_and(|f| f.is_ascii_lowercase() || f == '_')
        && u.len() <= 32
        && c.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

fn valid_ssh_key(k: &str) -> bool {
    let mut parts = k.split_whitespace();
    let kind = parts.next().unwrap_or_default();
    let body = parts.next().unwrap_or_default();
    (kind.starts_with("ssh-") || kind.starts_with("ecdsa-") || kind.starts_with("sk-"))
        && body.len() >= 16
        && !k.contains(['\n', '\r'])
}

/// Checks for `spec.guest`. `has_device`/`has_chardev` describe the binary.
pub fn validate(
    spec: &VmSpec,
    has_device: impl Fn(&str) -> bool,
    has_unix_chardev: bool,
) -> Vec<Diagnostic> {
    let mut d = Vec::new();
    let g = &spec.guest;
    if g.agent && !(has_device("virtio-serial-pci") && has_device("virtserialport")) {
        d.push(Diagnostic::error(
            "guest.agent",
            "guest.agent-unsupported",
            "this QEMU lacks virtio-serial-pci, which the guest agent channel needs",
        ));
    }
    if g.agent && !has_unix_chardev {
        d.push(Diagnostic::error(
            "guest.agent",
            "guest.agent-unsupported",
            "this QEMU has no socket chardev for the guest agent channel",
        ));
    }
    let Some(ci) = &g.cloud_init else {
        return d;
    };
    if !(has_device("virtio-scsi-pci") && has_device("scsi-cd")) {
        d.push(Diagnostic::error(
            "guest.cloud_init",
            "guest.seed-unsupported",
            "this QEMU lacks virtio-scsi-pci or scsi-cd, which the cloud-init seed CD-ROM needs",
        ));
    }
    if let Some(h) = &ci.hostname
        && !valid_hostname(h)
    {
        d.push(Diagnostic::error(
            "guest.cloud_init.hostname",
            "guest.hostname-invalid",
            "use letters, digits and '-' in dot-separated labels of up to 63 characters",
        ));
    }
    if let Some(u) = &ci.user
        && !valid_user(u)
    {
        d.push(Diagnostic::error(
            "guest.cloud_init.user",
            "guest.user-invalid",
            "use up to 32 lower-case letters, digits, '_' or '-', starting with a letter or '_'",
        ));
    }
    if let Some(p) = &ci.password
        && !is_password_hash(p)
    {
        d.push(Diagnostic::error(
            "guest.cloud_init.password",
            "guest.password-not-hashed",
            "the password must be stored as a crypt hash; send it through the API to have it hashed",
        ));
    }
    for (i, k) in ci.ssh_keys.iter().enumerate() {
        if !valid_ssh_key(k) {
            d.push(Diagnostic::error(
                format!("guest.cloud_init.ssh_keys[{i}]"),
                "guest.ssh-key-invalid",
                "expected one OpenSSH public key line, e.g. 'ssh-ed25519 AAAA… comment'",
            ));
        }
    }
    if let Some(u) = &ci.user_data {
        if !(u.starts_with("#cloud-config")
            || u.starts_with("#!")
            || u.starts_with("Content-Type:"))
        {
            d.push(Diagnostic::error(
                "guest.cloud_init.user_data",
                "guest.user-data-invalid",
                "user-data must start with '#cloud-config', '#!' or a MIME header",
            ));
        }
        if ci.user.is_some() || ci.password.is_some() || !ci.ssh_keys.is_empty() {
            d.push(Diagnostic::warning(
                "guest.cloud_init.user_data",
                "guest.user-data-overrides",
                "custom user-data is used as is; the user, password and SSH key fields are ignored",
            ));
        }
    }
    if ci.user_data.as_deref().map_or(0, str::len) > 1 << 20 {
        d.push(Diagnostic::error(
            "guest.cloud_init.user_data",
            "guest.user-data-too-large",
            "user-data is limited to 1 MiB",
        ));
    }
    d
}

/// Quote a YAML scalar (JSON strings are valid YAML).
fn y(s: &str) -> String {
    serde_json::Value::from(s).to_string()
}

/// The seed's files: `meta-data` and `user-data`.
///
/// `instance_id` changes when the configuration does, so cloud-init applies
/// an edited seed on the next boot instead of treating it as already done.
pub fn seed_files(spec: &VmSpec, vm_id: &str) -> Option<Vec<(&'static str, Vec<u8>)>> {
    let ci = spec.guest.cloud_init.as_ref()?;
    let hostname = ci
        .hostname
        .clone()
        .unwrap_or_else(|| hostname_for(&spec.name));
    let user_data = match &ci.user_data {
        Some(u) => u.clone(),
        None => {
            let mut u = String::from("#cloud-config\n");
            u.push_str(&format!("hostname: {}\n", y(&hostname)));
            let keys = |indent: &str| -> String {
                let mut s = String::new();
                if !ci.ssh_keys.is_empty() {
                    s.push_str(&format!("{indent}ssh_authorized_keys:\n"));
                    for k in &ci.ssh_keys {
                        s.push_str(&format!("{indent}  - {}\n", y(k.trim())));
                    }
                }
                s
            };
            match &ci.user {
                Some(user) => {
                    u.push_str("users:\n  - default\n");
                    u.push_str(&format!("  - name: {}\n", y(user)));
                    u.push_str("    shell: /bin/bash\n");
                    u.push_str("    sudo: \"ALL=(ALL) NOPASSWD:ALL\"\n");
                    u.push_str("    groups: [sudo, wheel, adm]\n");
                    if let Some(p) = &ci.password {
                        u.push_str("    lock_passwd: false\n");
                        u.push_str(&format!("    passwd: {}\n", y(p)));
                    }
                    u.push_str(&keys("    "));
                }
                None => {
                    if let Some(p) = &ci.password {
                        // The image's default user.
                        u.push_str("chpasswd:\n  expire: false\n  users:\n");
                        u.push_str(&format!("    - {{name: default, password: {}}}\n", y(p)));
                    }
                    u.push_str(&keys(""));
                }
            }
            if ci.password.is_some() {
                u.push_str("ssh_pwauth: true\n");
            }
            u
        }
    };
    let digest = {
        use std::hash::{DefaultHasher, Hash, Hasher};
        let mut h = DefaultHasher::new();
        hostname.hash(&mut h);
        user_data.hash(&mut h);
        h.finish()
    };
    let meta = format!(
        "instance-id: {}\nlocal-hostname: {}\n",
        y(&format!("qui-{vm_id}-{:08x}", digest as u32)),
        y(&hostname)
    );
    Some(vec![
        ("meta-data", meta.into_bytes()),
        ("user-data", user_data.into_bytes()),
    ])
}

/// The seed image for `spec`, or `None` when cloud-init is off.
pub fn seed_image(spec: &VmSpec, vm_id: &str) -> Option<Vec<u8>> {
    let files = seed_files(spec, vm_id)?;
    let refs: Vec<(&str, &[u8])> = files.iter().map(|(n, b)| (*n, b.as_slice())).collect();
    Some(crate::iso9660::build(SEED_LABEL, &refs))
}

// ------------------------------------------------------------------ status

/// One guest interface as the agent reports it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
pub struct GuestInterface {
    pub name: String,
    pub mac: Option<String>,
    pub addresses: Vec<String>,
}

/// `GET /vms/{id}/guest`: what the guest agent says about the guest.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
pub struct GuestInfo {
    /// The VM is configured with an agent channel.
    pub agent: bool,
    pub running: bool,
    /// The agent answered.
    pub connected: bool,
    /// Why it did not, when it has a channel.
    pub error: Option<String>,
    pub hostname: Option<String>,
    pub os_name: Option<String>,
    pub os_version: Option<String>,
    pub kernel: Option<String>,
    pub interfaces: Vec<GuestInterface>,
    /// Frozen / thawed, from `guest-fsfreeze-status`.
    pub fsfreeze: Option<String>,
}

/// Interfaces from a `guest-network-get-interfaces` reply (loopback skipped).
pub fn parse_interfaces(v: &serde_json::Value) -> Vec<GuestInterface> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter(|i| i["name"] != "lo")
        .map(|i| GuestInterface {
            name: i["name"].as_str().unwrap_or_default().to_string(),
            mac: i["hardware-address"].as_str().map(String::from),
            addresses: i["ip-addresses"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|a| {
                    Some(format!(
                        "{}/{}",
                        a["ip-address"].as_str()?,
                        a["prefix"].as_u64()?
                    ))
                })
                .collect(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::Severity;

    fn base() -> VmSpec {
        serde_json::from_value(serde_json::json!({
            "spec_version": 1, "name": "t", "arch": "x86_64",
            "machine": {"type": "pc-q35-11.1"},
            "cpu": {"count": 1}, "memory": {"size_mib": 256}
        }))
        .unwrap()
    }

    fn spec(ci: CloudInit) -> VmSpec {
        let mut s = base();
        s.name = "web_01".into();
        s.guest.cloud_init = Some(ci);
        s
    }

    fn errors(s: &VmSpec) -> Vec<String> {
        validate(s, |_| true, true)
            .into_iter()
            .filter(|d| d.severity == Severity::Error)
            .map(|d| d.rule)
            .collect()
    }

    #[test]
    fn validation() {
        let ok = spec(CloudInit {
            hostname: Some("web-1.lab".into()),
            user: Some("ops".into()),
            password: Some("$6$salt$hash".into()),
            ssh_keys: vec!["ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExample me@box".into()],
            user_data: None,
        });
        assert!(errors(&ok).is_empty(), "{:?}", errors(&ok));
        let bad = spec(CloudInit {
            hostname: Some("-bad".into()),
            user: Some("Root".into()),
            password: Some("hunter2".into()),
            ssh_keys: vec!["not a key".into()],
            user_data: Some("hello".into()),
        });
        let e = errors(&bad);
        for r in [
            "guest.hostname-invalid",
            "guest.user-invalid",
            "guest.password-not-hashed",
            "guest.ssh-key-invalid",
            "guest.user-data-invalid",
        ] {
            assert!(e.iter().any(|x| x == r), "{r} in {e:?}");
        }
        let mut agent = base();
        agent.guest.agent = true;
        assert!(
            validate(&agent, |d| d != "virtio-serial-pci", true)
                .iter()
                .any(|d| d.rule == "guest.agent-unsupported")
        );
    }

    #[test]
    fn generated_user_data() {
        let s = spec(CloudInit {
            user: Some("ops".into()),
            password: Some("$6$x$y".into()),
            ssh_keys: vec!["ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExample".into()],
            ..Default::default()
        });
        let files = seed_files(&s, "abc").unwrap();
        let meta = String::from_utf8(files[0].1.clone()).unwrap();
        let user = String::from_utf8(files[1].1.clone()).unwrap();
        assert!(meta.contains("local-hostname: \"web-01\""), "{meta}");
        assert!(meta.contains("instance-id: \"qui-abc-"), "{meta}");
        assert!(user.starts_with("#cloud-config\n"));
        assert!(user.contains("  - name: \"ops\"\n"), "{user}");
        assert!(user.contains("    passwd: \"$6$x$y\"\n"), "{user}");
        assert!(
            user.contains("    ssh_authorized_keys:\n      - \"ssh-ed25519"),
            "{user}"
        );

        // Editing the config changes the instance id; the same config keeps it.
        let mut s2 = s.clone();
        s2.guest.cloud_init.as_mut().unwrap().hostname = Some("other".into());
        let meta2 = String::from_utf8(seed_files(&s2, "abc").unwrap()[0].1.clone()).unwrap();
        assert_ne!(meta.lines().next(), meta2.lines().next());
        assert_eq!(seed_files(&s, "abc"), seed_files(&s, "abc"));

        // Default user: chpasswd for its password, keys at top level.
        let d = spec(CloudInit {
            password: Some("$6$x$y".into()),
            ssh_keys: vec!["ssh-rsa AAAAB3NzaC1yc2EAAAADAQAB".into()],
            ..Default::default()
        });
        let user = String::from_utf8(seed_files(&d, "abc").unwrap()[1].1.clone()).unwrap();
        assert!(user.contains("chpasswd:"), "{user}");
        assert!(
            user.contains("\nssh_authorized_keys:\n  - \"ssh-rsa"),
            "{user}"
        );

        // Raw user-data is used verbatim.
        let raw = spec(CloudInit {
            user_data: Some("#!/bin/sh\necho hi\n".into()),
            ..Default::default()
        });
        assert_eq!(
            seed_files(&raw, "abc").unwrap()[1].1,
            b"#!/bin/sh\necho hi\n"
        );
    }

    #[test]
    fn seed_image_has_the_nocloud_layout() {
        let s = spec(CloudInit::default());
        let img = seed_image(&s, "abc").unwrap();
        assert_eq!(&crate::iso9660::read::label(&img, 16)[..6], b"CIDATA");
        let names: Vec<Vec<u8>> = crate::iso9660::read::files(&img, 17)
            .into_iter()
            .map(|f| f.0)
            .collect();
        let ucs = |s: &str| -> Vec<u8> { s.encode_utf16().flat_map(u16::to_be_bytes).collect() };
        assert_eq!(names, [ucs("meta-data;1"), ucs("user-data;1")]);
        assert!(seed_image(&base(), "abc").is_none());
    }

    #[test]
    fn hostnames_from_vm_names() {
        assert_eq!(hostname_for("Web_01"), "web-01");
        assert_eq!(hostname_for("__"), "vm");
        assert!(valid_hostname(&hostname_for("a.b_c")));
    }

    #[test]
    fn interfaces_from_the_agent() {
        let v = serde_json::json!([
            {"name": "lo", "ip-addresses": [{"ip-address": "127.0.0.1", "prefix": 8}]},
            {"name": "eth0", "hardware-address": "52:54:00:12:34:56",
             "ip-addresses": [{"ip-address": "10.0.2.15", "ip-address-type": "ipv4", "prefix": 24}]}
        ]);
        let i = parse_interfaces(&v);
        assert_eq!(i.len(), 1);
        assert_eq!(i[0].addresses, ["10.0.2.15/24"]);
    }
}
