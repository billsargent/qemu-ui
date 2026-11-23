//! A minimal ISO 9660 image writer with Joliet names (M7-03).
//!
//! Enough for a cloud-init NoCloud seed: one root directory of small files,
//! no subdirectories. The primary volume carries 8.3 upper-case names; the
//! Joliet supplementary volume carries the real names (`meta-data`,
//! `user-data`), which is what Linux, BSD and Windows mount by default.
//! No system tools are needed, and the output is deterministic.

const SECTOR: usize = 2048;

/// Build an image labelled `volume_id` holding `files` in its root.
/// Names should be short ASCII (they are used verbatim for Joliet and
/// mapped to 8.3 for the primary volume).
pub fn build(volume_id: &str, files: &[(&str, &[u8])]) -> Vec<u8> {
    // Layout: 16 system sectors, PVD, SVD, terminator, then 4 path tables,
    // the two root directories, then file data.
    const PVD: usize = 16;
    const SVD: usize = 17;
    const TERM: usize = 18;
    const PT_L: usize = 19;
    const PT_M: usize = 20;
    const JPT_L: usize = 21;
    const JPT_M: usize = 22;
    const ROOT: usize = 23;
    const JROOT: usize = 24;
    const DATA: usize = 25;

    let mut extents = Vec::new();
    let mut next = DATA;
    for (_, data) in files {
        extents.push(next);
        next += data.len().div_ceil(SECTOR).max(1);
    }
    let total = next;

    // Directory records, sorted by identifier as the standard asks.
    let mut primary: Vec<(Vec<u8>, usize)> = files
        .iter()
        .enumerate()
        .map(|(i, (name, _))| (primary_name(name), i))
        .collect();
    primary.sort();
    let mut joliet: Vec<(Vec<u8>, usize)> = files
        .iter()
        .enumerate()
        .map(|(i, (name, _))| (joliet_name(name), i))
        .collect();
    joliet.sort();

    let dir = |entries: &[(Vec<u8>, usize)], own: usize| -> Vec<u8> {
        let mut d = Vec::new();
        d.extend(record(&[0], own, SECTOR, true));
        d.extend(record(&[1], own, SECTOR, true));
        for (name, i) in entries {
            d.extend(record(name, extents[*i], files[*i].1.len(), false));
        }
        assert!(d.len() <= SECTOR, "too many files for one directory sector");
        d
    };

    let mut img = vec![0u8; total * SECTOR];
    let put = |img: &mut Vec<u8>, sector: usize, bytes: &[u8]| {
        img[sector * SECTOR..sector * SECTOR + bytes.len()].copy_from_slice(bytes);
    };

    let path_table = |root: usize, big: bool| -> Vec<u8> {
        let mut p = vec![1u8, 0];
        if big {
            p.extend((root as u32).to_be_bytes());
            p.extend(1u16.to_be_bytes());
        } else {
            p.extend((root as u32).to_le_bytes());
            p.extend(1u16.to_le_bytes());
        }
        p.extend([0, 0]); // root name (one NUL byte) + padding
        p
    };
    let pt_size = path_table(ROOT, false).len();

    let descriptor = |kind: u8, label: &[u8], root: usize, l: usize, m: usize| -> Vec<u8> {
        let mut v = vec![0u8; SECTOR];
        v[0] = kind;
        v[1..6].copy_from_slice(b"CD001");
        v[6] = 1;
        fill(&mut v[8..40], b"", kind == 2);
        fill(&mut v[40..72], label, kind == 2);
        both32(&mut v[80..88], total as u32);
        if kind == 2 {
            // UCS-2 level 3.
            v[88..91].copy_from_slice(b"%/E");
        }
        both16(&mut v[120..124], 1);
        both16(&mut v[124..128], 1);
        both16(&mut v[128..132], SECTOR as u16);
        both32(&mut v[132..140], pt_size as u32);
        v[140..144].copy_from_slice(&(l as u32).to_le_bytes());
        v[148..152].copy_from_slice(&(m as u32).to_be_bytes());
        let r = record(&[0], root, SECTOR, true);
        v[156..156 + r.len()].copy_from_slice(&r);
        for f in [190..318, 318..446, 446..574, 574..702] {
            fill(&mut v[f], b"", kind == 2);
        }
        for f in [702..739, 739..776, 776..813] {
            fill(&mut v[f], b"", kind == 2);
        }
        for f in [813..830, 830..847, 847..864, 864..881] {
            // "Not specified": sixteen '0' digits and a zero offset.
            v[f.start..f.end - 1].fill(b'0');
        }
        v[881] = 1;
        v
    };

    let upper: Vec<u8> = volume_id.bytes().map(|b| b.to_ascii_uppercase()).collect();
    put(&mut img, PVD, &descriptor(1, &upper, ROOT, PT_L, PT_M));
    put(
        &mut img,
        SVD,
        &descriptor(2, &ucs2(volume_id), JROOT, JPT_L, JPT_M),
    );
    let mut term = vec![0u8; 7];
    term[0] = 255;
    term[1..6].copy_from_slice(b"CD001");
    term[6] = 1;
    put(&mut img, TERM, &term);
    put(&mut img, PT_L, &path_table(ROOT, false));
    put(&mut img, PT_M, &path_table(ROOT, true));
    put(&mut img, JPT_L, &path_table(JROOT, false));
    put(&mut img, JPT_M, &path_table(JROOT, true));
    put(&mut img, ROOT, &dir(&primary, ROOT));
    put(&mut img, JROOT, &dir(&joliet, JROOT));
    for ((_, data), at) in files.iter().zip(&extents) {
        put(&mut img, *at, data);
    }
    img
}

/// One directory record.
fn record(name: &[u8], extent: usize, len: usize, is_dir: bool) -> Vec<u8> {
    let mut r = vec![0u8; 33];
    both32(&mut r[2..10], extent as u32);
    both32(&mut r[10..18], len as u32);
    // 2000-01-01 00:00:00 UTC: fixed, so images are reproducible.
    r[18..25].copy_from_slice(&[100, 1, 1, 0, 0, 0, 0]);
    r[25] = if is_dir { 2 } else { 0 };
    both16(&mut r[28..32], 1);
    r[32] = name.len() as u8;
    r.extend_from_slice(name);
    if r.len() % 2 == 1 {
        r.push(0);
    }
    r[0] = r.len() as u8;
    r
}

/// `meta-data` → `METADATA.;1`: d-characters only, 8.3.
fn primary_name(name: &str) -> Vec<u8> {
    let (stem, ext) = name.rsplit_once('.').unwrap_or((name, ""));
    let clean = |s: &str, n: usize| -> String {
        s.chars()
            .map(|c| c.to_ascii_uppercase())
            .filter(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == '_')
            .take(n)
            .collect()
    };
    format!("{}.{};1", clean(stem, 8), clean(ext, 3)).into_bytes()
}

fn joliet_name(name: &str) -> Vec<u8> {
    ucs2(&format!("{name};1"))
}

fn ucs2(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(u16::to_be_bytes).collect()
}

/// Pad a text field with spaces (UCS-2 spaces for Joliet).
fn fill(field: &mut [u8], text: &[u8], wide: bool) {
    if wide {
        for pair in field.chunks_mut(2) {
            pair.copy_from_slice(&[0, b' '][..pair.len()]);
        }
    } else {
        field.fill(b' ');
    }
    let n = text.len().min(field.len());
    field[..n].copy_from_slice(&text[..n]);
}

fn both32(f: &mut [u8], v: u32) {
    f[..4].copy_from_slice(&v.to_le_bytes());
    f[4..8].copy_from_slice(&v.to_be_bytes());
}

fn both16(f: &mut [u8], v: u16) {
    f[..2].copy_from_slice(&v.to_le_bytes());
    f[2..4].copy_from_slice(&v.to_be_bytes());
}

#[cfg(test)]
pub(crate) mod read {
    //! A reader for the tests: walks a descriptor's root directory.

    pub fn label(img: &[u8], sector: usize) -> Vec<u8> {
        img[sector * 2048 + 40..sector * 2048 + 72].to_vec()
    }

    /// (identifier, contents) of the files in the root of the volume whose
    /// descriptor is at `sector`.
    pub fn files(img: &[u8], sector: usize) -> Vec<(Vec<u8>, Vec<u8>)> {
        let d = &img[sector * 2048..];
        assert_eq!(&d[1..6], b"CD001");
        let root = &d[156..190];
        let at = u32::from_le_bytes(root[2..6].try_into().unwrap()) as usize;
        let len = u32::from_le_bytes(root[10..14].try_into().unwrap()) as usize;
        let dir = &img[at * 2048..at * 2048 + len];
        let mut out = Vec::new();
        let mut i = 0;
        while i < dir.len() && dir[i] != 0 {
            let r = &dir[i..i + dir[i] as usize];
            let name = r[33..33 + r[32] as usize].to_vec();
            if r[25] & 2 == 0 {
                let at = u32::from_le_bytes(r[2..6].try_into().unwrap()) as usize;
                let n = u32::from_le_bytes(r[10..14].try_into().unwrap()) as usize;
                out.push((name, img[at * 2048..at * 2048 + n].to_vec()));
            }
            i += r.len();
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_and_joliet_trees_hold_the_files() {
        let img = build(
            "cidata",
            &[("user-data", b"#cloud-config\n"), ("meta-data", b"a: b\n")],
        );
        assert_eq!(img.len() % SECTOR, 0);
        assert_eq!(&img[16 * SECTOR + 1..16 * SECTOR + 6], b"CD001");
        assert_eq!(&read::label(&img, 16)[..6], b"CIDATA");
        assert_eq!(&read::label(&img, 17)[..12], &ucs2("cidata")[..]);
        assert_eq!(img[18 * SECTOR], 255, "terminator");

        let p = read::files(&img, 16);
        assert_eq!(p[0], (b"METADATA.;1".to_vec(), b"a: b\n".to_vec()));
        assert_eq!(p[1].0, b"USERDATA.;1");
        let j = read::files(&img, 17);
        assert_eq!(j[0].0, ucs2("meta-data;1"));
        assert_eq!(j[1], (ucs2("user-data;1"), b"#cloud-config\n".to_vec()));
        // Deterministic.
        assert_eq!(
            img,
            build(
                "cidata",
                &[("user-data", b"#cloud-config\n"), ("meta-data", b"a: b\n")]
            )
        );
    }

    #[test]
    fn large_files_span_sectors() {
        let big = vec![7u8; 5000];
        let img = build("cidata", &[("user-data", &big), ("meta-data", b"")]);
        let j = read::files(&img, 17);
        assert_eq!(
            j.iter().find(|f| f.0 == ucs2("user-data;1")).unwrap().1,
            big
        );
        assert!(
            j.iter()
                .any(|f| f.0 == ucs2("meta-data;1") && f.1.is_empty())
        );
    }
}
