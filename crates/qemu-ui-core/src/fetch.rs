//! URL fetch guard (plan.md §9.4, M2-08): the pure parts.
//!
//! The daemon downloads images on behalf of the browser, which makes it a
//! server-side request forgery target: a URL could point it at the daemon
//! itself, at other services on the host or LAN, or at cloud metadata
//! endpoints. Rules enforced here and by the daemon's resolver:
//!
//! * scheme `https`; plain `http` only together with a SHA-256 checksum (the
//!   checksum, not the transport, then guarantees integrity);
//! * no user info (`user:pass@host`) in URLs;
//! * every resolved address must be public: loopback, link-local (including
//!   169.254.169.254 metadata), multicast, unspecified, broadcast,
//!   documentation/benchmark/reserved ranges and the IPv6 forms that embed
//!   them are always refused; private ranges (RFC 1918, CGNAT, ULA) only
//!   with an explicit opt-in;
//! * the daemon connects to exactly the addresses it checked (no second
//!   resolution), so DNS rebinding cannot swap them, and every redirect hop
//!   is checked again.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use url::Url;

use crate::spec::valid_image_name;

/// Longest URL accepted.
pub const MAX_URL_LEN: usize = 2048;
/// Redirects followed before giving up.
pub const MAX_REDIRECTS: u32 = 5;

/// What kind of network an address belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpClass {
    Public,
    /// RFC 1918, CGNAT (100.64/10), IPv6 ULA and site-local.
    Private,
    Loopback,
    /// 169.254/16 (incl. cloud metadata), fe80::/10.
    LinkLocal,
    Multicast,
    Unspecified,
    /// Broadcast, documentation, benchmarking, protocol-assignment,
    /// future-use and tunnelling ranges.
    Reserved,
}

impl IpClass {
    pub fn describe(self) -> &'static str {
        match self {
            IpClass::Public => "a public address",
            IpClass::Private => "a private network address",
            IpClass::Loopback => "this computer (loopback)",
            IpClass::LinkLocal => "a link-local address (this includes cloud metadata services)",
            IpClass::Multicast => "a multicast address",
            IpClass::Unspecified => "an unspecified address",
            IpClass::Reserved => "a reserved address range",
        }
    }
}

fn classify_v4(ip: Ipv4Addr) -> IpClass {
    let o = ip.octets();
    match o {
        [0, ..] => IpClass::Unspecified, // "this network", incl. 0.0.0.0
        [127, ..] => IpClass::Loopback,
        [10, ..] | [192, 168, ..] => IpClass::Private,
        [172, b, ..] if (16..=31).contains(&b) => IpClass::Private,
        [100, b, ..] if (64..=127).contains(&b) => IpClass::Private, // CGNAT
        [169, 254, ..] => IpClass::LinkLocal,
        [a, ..] if (224..=239).contains(&a) => IpClass::Multicast,
        [255, 255, 255, 255] => IpClass::Reserved,
        [a, ..] if a >= 240 => IpClass::Reserved,
        [192, 0, 0, _]
        | [192, 0, 2, _]
        | [198, 51, 100, _]
        | [203, 0, 113, _]
        | [192, 88, 99, _] => IpClass::Reserved,
        [198, b, ..] if b == 18 || b == 19 => IpClass::Reserved, // benchmarking
        _ => IpClass::Public,
    }
}

fn classify_v6(ip: Ipv6Addr) -> IpClass {
    if ip.is_unspecified() {
        return IpClass::Unspecified;
    }
    if ip.is_loopback() {
        return IpClass::Loopback;
    }
    let s = ip.segments();
    // Forms that embed an IPv4 address are judged by that address.
    if let Some(v4) = ip.to_ipv4_mapped() {
        return classify_v4(v4);
    }
    if s[0..6] == [0, 0, 0, 0, 0, 0] {
        // Deprecated IPv4-compatible ::a.b.c.d
        return classify_v4(Ipv4Addr::new(
            (s[6] >> 8) as u8,
            s[6] as u8,
            (s[7] >> 8) as u8,
            s[7] as u8,
        ));
    }
    if s[0..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
        // NAT64 well-known prefix
        return classify_v4(Ipv4Addr::new(
            (s[6] >> 8) as u8,
            s[6] as u8,
            (s[7] >> 8) as u8,
            s[7] as u8,
        ));
    }
    if s[0] == 0x2002 {
        // 6to4 embeds an IPv4 address in bits 16..48
        return classify_v4(Ipv4Addr::new(
            (s[1] >> 8) as u8,
            s[1] as u8,
            (s[2] >> 8) as u8,
            s[2] as u8,
        ));
    }
    match s[0] {
        x if x & 0xff00 == 0xff00 => IpClass::Multicast,
        x if x & 0xffc0 == 0xfe80 => IpClass::LinkLocal,
        x if x & 0xffc0 == 0xfec0 => IpClass::Private, // deprecated site-local
        x if x & 0xfe00 == 0xfc00 => IpClass::Private, // ULA
        0x2001 if s[1] == 0x0db8 => IpClass::Reserved, // documentation
        0x2001 if s[1] == 0 => IpClass::Reserved,      // Teredo tunnelling
        0x0100 if s[1..4] == [0, 0, 0] => IpClass::Reserved, // discard
        x if x & 0xe000 == 0x2000 => IpClass::Public,  // global unicast 2000::/3
        _ => IpClass::Reserved,
    }
}

pub fn classify(ip: IpAddr) -> IpClass {
    match ip {
        IpAddr::V4(v4) => classify_v4(v4),
        IpAddr::V6(v6) => classify_v6(v6),
    }
}

/// May the daemon connect to `ip`? Returns the refusal reason otherwise.
pub fn check_ip(ip: IpAddr, allow_private: bool) -> Result<(), String> {
    match classify(ip) {
        IpClass::Public => Ok(()),
        IpClass::Private if allow_private => Ok(()),
        IpClass::Private => Err(format!(
            "{ip} is {}; downloads from the local network are disabled (start the daemon with \
             --allow-private-fetch to allow them)",
            IpClass::Private.describe()
        )),
        c => Err(format!(
            "{ip} is {}, which downloads may never reach",
            c.describe()
        )),
    }
}

/// Parse and validate a download URL. `has_checksum`: plain `http` is only
/// allowed when a SHA-256 checksum will verify the result.
pub fn validate_url(s: &str, has_checksum: bool) -> Result<Url, String> {
    if s.len() > MAX_URL_LEN {
        return Err(format!("the URL is longer than {MAX_URL_LEN} characters"));
    }
    let url = Url::parse(s.trim()).map_err(|e| format!("not a valid URL: {e}"))?;
    check_url(&url, has_checksum)?;
    Ok(url)
}

/// Rules every URL (including redirect targets) must satisfy.
pub fn check_url(url: &Url, has_checksum: bool) -> Result<(), String> {
    match url.scheme() {
        "https" => {}
        "http" if has_checksum => {}
        "http" => {
            return Err(
                "plain http is only allowed together with a SHA-256 checksum, because nothing else would \
                 detect a tampered download; use https or provide the checksum"
                    .into(),
            );
        }
        other => return Err(format!("'{other}' URLs are not supported; use https")),
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("URLs with a user name or password are not allowed".into());
    }
    if url.host().is_none() {
        return Err("the URL has no host".into());
    }
    Ok(())
}

/// Resolve a redirect `Location` against the current URL and check it.
pub fn redirect_target(current: &Url, location: &str, has_checksum: bool) -> Result<Url, String> {
    let next = current
        .join(location.trim())
        .map_err(|e| format!("the server redirected to an invalid location: {e}"))?;
    check_url(&next, has_checksum).map_err(|e| format!("refused redirect to {next}: {e}"))?;
    Ok(next)
}

/// A file name for the downloaded image, from the last path segment.
pub fn default_name(url: &Url) -> Option<String> {
    let seg = url.path_segments()?.rev().find(|s| !s.is_empty())?;
    let decoded = percent_decode(seg);
    valid_image_name(&decoded).then_some(decoded)
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (
                (b[i + 1] as char).to_digit(16),
                (b[i + 2] as char).to_digit(16),
            )
        {
            out.push((h * 16 + l) as u8);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Is `s` a SHA-256 in hex?
pub fn valid_sha256(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(s: &str) -> IpClass {
        classify(s.parse().unwrap())
    }

    #[test]
    fn ipv4_classes() {
        for (ip, want) in [
            ("8.8.8.8", IpClass::Public),
            ("1.1.1.1", IpClass::Public),
            ("127.0.0.1", IpClass::Loopback),
            ("127.255.0.9", IpClass::Loopback),
            ("10.1.2.3", IpClass::Private),
            ("172.16.0.1", IpClass::Private),
            ("172.31.255.255", IpClass::Private),
            ("172.32.0.1", IpClass::Public),
            ("192.168.1.10", IpClass::Private),
            ("100.64.0.1", IpClass::Private),
            ("100.128.0.1", IpClass::Public),
            ("169.254.169.254", IpClass::LinkLocal),
            ("224.0.0.1", IpClass::Multicast),
            ("0.0.0.0", IpClass::Unspecified),
            ("0.1.2.3", IpClass::Unspecified),
            ("255.255.255.255", IpClass::Reserved),
            ("240.0.0.1", IpClass::Reserved),
            ("192.0.2.1", IpClass::Reserved),
            ("198.18.0.1", IpClass::Reserved),
            ("192.0.0.170", IpClass::Reserved),
        ] {
            assert_eq!(c(ip), want, "{ip}");
        }
    }

    #[test]
    fn ipv6_classes_including_embedded_ipv4() {
        for (ip, want) in [
            ("2606:4700:4700::1111", IpClass::Public),
            ("::1", IpClass::Loopback),
            ("::", IpClass::Unspecified),
            ("fe80::1", IpClass::LinkLocal),
            ("fd00::1", IpClass::Private),
            ("fc00::1", IpClass::Private),
            ("fec0::1", IpClass::Private),
            ("ff02::1", IpClass::Multicast),
            ("2001:db8::1", IpClass::Reserved),
            ("2001::1", IpClass::Reserved),
            // IPv4-mapped / compatible / NAT64 / 6to4 forms of blocked IPv4
            ("::ffff:127.0.0.1", IpClass::Loopback),
            ("::ffff:169.254.169.254", IpClass::LinkLocal),
            ("::ffff:10.0.0.1", IpClass::Private),
            ("::ffff:8.8.8.8", IpClass::Public),
            ("::127.0.0.1", IpClass::Loopback),
            ("64:ff9b::a9fe:a9fe", IpClass::LinkLocal),
            ("2002:7f00:1::", IpClass::Loopback),
            ("2002:0808:0808::", IpClass::Public),
        ] {
            assert_eq!(c(ip), want, "{ip}");
        }
    }

    #[test]
    fn check_ip_policy() {
        assert!(check_ip("8.8.8.8".parse().unwrap(), false).is_ok());
        assert!(check_ip("192.168.1.2".parse().unwrap(), false).is_err());
        assert!(check_ip("192.168.1.2".parse().unwrap(), true).is_ok());
        // The opt-in never opens loopback or metadata.
        assert!(check_ip("127.0.0.1".parse().unwrap(), true).is_err());
        let e = check_ip("169.254.169.254".parse().unwrap(), true).unwrap_err();
        assert!(e.contains("metadata"), "{e}");
    }

    #[test]
    fn url_rules() {
        assert!(validate_url("https://example.org/a.iso", false).is_ok());
        assert!(
            validate_url("http://example.org/a.iso", false)
                .unwrap_err()
                .contains("SHA-256")
        );
        assert!(validate_url("http://example.org/a.iso", true).is_ok());
        assert!(validate_url("ftp://example.org/a.iso", true).is_err());
        assert!(validate_url("file:///etc/passwd", true).is_err());
        assert!(validate_url("https://user:pw@example.org/a.iso", false).is_err());
        assert!(validate_url("https://user@example.org/a.iso", false).is_err());
        assert!(validate_url("not a url", false).is_err());
        assert!(validate_url(&format!("https://e.org/{}", "a".repeat(3000)), false).is_err());
    }

    #[test]
    fn redirects_are_rechecked() {
        let base = Url::parse("https://mirror.example/pub/x.iso").unwrap();
        assert_eq!(
            redirect_target(&base, "/other/y.iso", false)
                .unwrap()
                .as_str(),
            "https://mirror.example/other/y.iso"
        );
        assert!(
            redirect_target(&base, "http://plain.example/y.iso", false).is_err(),
            "https → http downgrade"
        );
        assert!(redirect_target(&base, "http://plain.example/y.iso", true).is_ok());
        assert!(redirect_target(&base, "file:///etc/passwd", true).is_err());
        assert!(redirect_target(&base, "https://u:p@evil.example/", false).is_err());
    }

    #[test]
    fn names_from_urls() {
        let n = |s: &str| default_name(&Url::parse(s).unwrap());
        assert_eq!(
            n("https://e.org/pub/debian-12.iso"),
            Some("debian-12.iso".into())
        );
        assert_eq!(
            n("https://e.org/pub/my%20disk.qcow2?x=1"),
            Some("my disk.qcow2".into())
        );
        assert_eq!(n("https://e.org/dir/"), Some("dir".into()));
        assert_eq!(n("https://e.org/"), None);
        assert_eq!(n("https://e.org/nul.iso"), None, "Windows device name");
        assert_eq!(n("https://e.org/..%2Fescape"), None);
    }

    #[test]
    fn checksums() {
        assert!(valid_sha256(&"a".repeat(64)));
        assert!(!valid_sha256(&"g".repeat(64)));
        assert!(!valid_sha256("abc"));
    }
}
