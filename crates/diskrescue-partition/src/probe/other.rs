//! Recognise-only filesystems and containers.
//!
//! These are identified by their magic number plus a few sanity checks and
//! reported as [`FsKind::Other`] with the family name as the first note:
//! XFS (`XFSB` at 0), HFS+/HFSX (`H+`/`HX` at 1024), HFS classic (`BD` at
//! 1024), APFS (`NXSB` at 32), LUKS (`LUKS\xBA\xBE` at 0), squashfs (`hsqs`
//! at 0), btrfs (`_BHRfS_M` at 0x10040), ISO 9660 (`CD001` at 0x8001) and
//! Linux swap (`SWAPSPACE2` / `SWAP-SPACE` in the last ten bytes of the first
//! page, for 4, 8, 16 and 64 KiB pages). They are never parsed further.

use diskrescue_core::fs::FsKind;
use diskrescue_core::util::bytes::{ascii_trimmed, u16be, u16le, u32be, u32le, u64be, u64le};

use super::{uuid_text, Ctx, FsProbe};

fn other(name: &str) -> FsProbe {
    let mut p = FsProbe::new(FsKind::Other);
    p.notes.push(name.to_string());
    p.notes.push("recognised only; not parsed".to_string());
    p
}

fn label_of(bytes: &[u8]) -> Option<String> {
    let s = ascii_trimmed(bytes);
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn xfs(h: &[u8]) -> Option<FsProbe> {
    if h.get(0..4)? != b"XFSB" {
        return None;
    }
    let block_size = u32be(h, 4)?;
    let dblocks = u64be(h, 8)?;
    let sect_size = u16be(h, 102)?;
    if !block_size.is_power_of_two()
        || !(512..=65536).contains(&block_size)
        || dblocks == 0
        || !sect_size.is_power_of_two()
        || !(512..=32768).contains(&sect_size)
    {
        return None;
    }
    let mut p = other("xfs");
    p.size = u64::from(block_size).checked_mul(dblocks);
    p.cluster_size = Some(block_size);
    p.label = label_of(h.get(108..120)?);
    Some(p)
}

fn hfs(h: &[u8]) -> Option<FsProbe> {
    let sig = h.get(1024..1026)?;
    match sig {
        b"H+" | b"HX" => {
            let version = u16be(h, 1026)?;
            let block_size = u32be(h, 1024 + 40)?;
            let total_blocks = u32be(h, 1024 + 44)?;
            if !(4..=5).contains(&version)
                || !block_size.is_power_of_two()
                || block_size < 512
                || total_blocks == 0
            {
                return None;
            }
            let mut p = other(if sig == b"HX" { "hfsx" } else { "hfs+" });
            p.size = Some(u64::from(block_size) * u64::from(total_blocks));
            p.cluster_size = Some(block_size);
            Some(p)
        }
        b"BD" => {
            // HFS classic: drAlBlkSiz at +20 (u32), drNmAlBlks at +18 (u16).
            let al_blk_siz = u32be(h, 1024 + 20)?;
            let nm_al_blks = u16be(h, 1024 + 18)?;
            if al_blk_siz == 0 || al_blk_siz % 512 != 0 || nm_al_blks == 0 {
                return None;
            }
            let mut p = other("hfs");
            p.size = Some(u64::from(al_blk_siz) * u64::from(nm_al_blks));
            p.cluster_size = Some(al_blk_siz);
            Some(p)
        }
        _ => None,
    }
}

fn apfs(h: &[u8]) -> Option<FsProbe> {
    if h.get(32..36)? != b"NXSB" {
        return None;
    }
    let block_size = u32le(h, 36)?;
    let block_count = u64le(h, 40)?;
    if !block_size.is_power_of_two() || !(4096..=65536).contains(&block_size) || block_count == 0 {
        return None;
    }
    let mut p = other("apfs");
    p.size = u64::from(block_size).checked_mul(block_count);
    p.cluster_size = Some(block_size);
    Some(p)
}

fn luks(h: &[u8]) -> Option<FsProbe> {
    if h.get(0..6)? != b"LUKS\xba\xbe" {
        return None;
    }
    let version = u16be(h, 6)?;
    if !(1..=2).contains(&version) {
        return None;
    }
    let mut p = other("luks");
    p.notes.push(format!("LUKS version {version}"));
    if let Some(u) = h.get(168..208) {
        let t = ascii_trimmed(u);
        if !t.is_empty() {
            p.notes.push(format!("uuid {t}"));
        }
    }
    Some(p)
}

fn squashfs(h: &[u8]) -> Option<FsProbe> {
    if h.get(0..4)? != b"hsqs" {
        return None;
    }
    let block_size = u32le(h, 12)?;
    let block_log = u16le(h, 22)?;
    let major = u16le(h, 28)?;
    if major != 4 || !(12..=20).contains(&block_log) || block_size != 1u32 << block_log {
        return None;
    }
    let mut p = other("squashfs");
    p.size = Some(u64le(h, 40)?);
    p.cluster_size = Some(block_size);
    Some(p)
}

fn swap_from_head(h: &[u8], page: usize) -> Option<FsProbe> {
    let magic = h.get(page - 10..page)?;
    if magic != b"SWAPSPACE2" && magic != b"SWAP-SPACE" {
        return None;
    }
    let mut p = other("swap");
    if magic == b"SWAPSPACE2" {
        let version = u32le(h, 1024)?;
        let last_page = u64::from(u32le(h, 1028)?);
        p.size = (last_page + 1).checked_mul(page as u64);
        p.label = label_of(h.get(1052..1068)?);
        p.notes.push(format!("swap version {version}"));
        p.notes
            .push(format!("uuid {}", uuid_text(h.get(1036..1052)?)));
    }
    p.cluster_size = u32::try_from(page).ok();
    Some(p)
}

fn btrfs(ctx: &Ctx) -> Option<FsProbe> {
    let sb = ctx.read(0x10000, 0x22B)?;
    if sb.get(0x40..0x48)? != b"_BHRfS_M" {
        return None;
    }
    // The superblock records its own byte offset.
    if u64le(&sb, 0x30)? != 0x10000 {
        return None;
    }
    let total = u64le(&sb, 0x70)?;
    let sector = u32le(&sb, 0x90)?;
    if total == 0 || !sector.is_power_of_two() || sector < 512 {
        return None;
    }
    let mut p = other("btrfs");
    p.size = Some(total);
    p.cluster_size = Some(sector);
    p.label = label_of(sb.get(0x12B..0x22B)?);
    p.notes
        .push(format!("uuid {}", uuid_text(sb.get(0x20..0x30)?)));
    Some(p)
}

fn iso9660(ctx: &Ctx) -> Option<FsProbe> {
    let pvd = ctx.read(0x8000, 2048)?;
    if *pvd.first()? != 1 || pvd.get(1..6)? != b"CD001" || *pvd.get(6)? != 1 {
        return None;
    }
    let space = u64::from(u32le(&pvd, 80)?);
    let lbs = u32::from(u16le(&pvd, 128)?);
    if space == 0 || lbs == 0 || !lbs.is_power_of_two() {
        return None;
    }
    let mut p = other("iso9660");
    p.size = space.checked_mul(u64::from(lbs));
    p.cluster_size = Some(lbs);
    p.label = label_of(pvd.get(40..72)?);
    Some(p)
}

fn swap_other_pages(ctx: &Ctx) -> Option<FsProbe> {
    for page in [8192usize, 16384, 65536] {
        if let Some(h) = ctx.read((page - 10) as u64, 10) {
            if h == b"SWAPSPACE2" || h == b"SWAP-SPACE" {
                let mut p = other("swap");
                p.cluster_size = u32::try_from(page).ok();
                if let Some(hdr) = ctx.read(1024, 1100) {
                    if h == b"SWAPSPACE2" {
                        if let Some(last) = u32le(&hdr, 4) {
                            p.size = (u64::from(last) + 1).checked_mul(page as u64);
                        }
                        p.label = hdr.get(28..44).and_then(label_of);
                    }
                }
                return Some(p);
            }
        }
    }
    None
}

/// Try every recognise-only family.
pub(crate) fn probe(ctx: &Ctx) -> Option<FsProbe> {
    let h = ctx.head;
    xfs(h)
        .or_else(|| luks(h))
        .or_else(|| squashfs(h))
        .or_else(|| apfs(h))
        .or_else(|| hfs(h))
        .or_else(|| swap_from_head(h, 4096))
        .or_else(|| btrfs(ctx))
        .or_else(|| iso9660(ctx))
        .or_else(|| swap_other_pages(ctx))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xfs_header() {
        let mut h = vec![0u8; 4096];
        h[0..4].copy_from_slice(b"XFSB");
        h[4..8].copy_from_slice(&4096u32.to_be_bytes());
        h[8..16].copy_from_slice(&1000u64.to_be_bytes());
        h[102..104].copy_from_slice(&512u16.to_be_bytes());
        h[108..112].copy_from_slice(b"data");
        let p = xfs(&h).unwrap();
        assert_eq!(p.kind, FsKind::Other);
        assert_eq!(p.notes[0], "xfs");
        assert_eq!(p.size, Some(4096 * 1000));
        assert_eq!(p.label.as_deref(), Some("data"));
        h[4..8].copy_from_slice(&1000u32.to_be_bytes());
        assert!(xfs(&h).is_none());
    }

    #[test]
    fn hfs_plus_header() {
        let mut h = vec![0u8; 4096];
        h[1024..1026].copy_from_slice(b"H+");
        h[1026..1028].copy_from_slice(&4u16.to_be_bytes());
        h[1064..1068].copy_from_slice(&4096u32.to_be_bytes());
        h[1068..1072].copy_from_slice(&256u32.to_be_bytes());
        let p = hfs(&h).unwrap();
        assert_eq!(p.notes[0], "hfs+");
        assert_eq!(p.size, Some(4096 * 256));
        h[1064..1068].copy_from_slice(&0u32.to_be_bytes());
        assert!(hfs(&h).is_none());
    }

    #[test]
    fn apfs_luks_squashfs_swap() {
        let mut h = vec![0u8; 4096];
        h[32..36].copy_from_slice(b"NXSB");
        h[36..40].copy_from_slice(&4096u32.to_le_bytes());
        h[40..48].copy_from_slice(&100u64.to_le_bytes());
        assert_eq!(apfs(&h).unwrap().size, Some(409600));

        let mut l = vec![0u8; 4096];
        l[0..6].copy_from_slice(b"LUKS\xba\xbe");
        l[6..8].copy_from_slice(&2u16.to_be_bytes());
        assert_eq!(luks(&l).unwrap().notes[0], "luks");
        l[6..8].copy_from_slice(&9u16.to_be_bytes());
        assert!(luks(&l).is_none());

        let mut q = vec![0u8; 4096];
        q[0..4].copy_from_slice(b"hsqs");
        q[12..16].copy_from_slice(&131072u32.to_le_bytes());
        q[22..24].copy_from_slice(&17u16.to_le_bytes());
        q[28..30].copy_from_slice(&4u16.to_le_bytes());
        q[40..48].copy_from_slice(&5000u64.to_le_bytes());
        assert_eq!(squashfs(&q).unwrap().size, Some(5000));

        let mut w = vec![0u8; 4096];
        w[4086..4096].copy_from_slice(b"SWAPSPACE2");
        w[1024..1028].copy_from_slice(&1u32.to_le_bytes());
        w[1028..1032].copy_from_slice(&99u32.to_le_bytes());
        let p = swap_from_head(&w, 4096).unwrap();
        assert_eq!(p.size, Some(100 * 4096));
        assert_eq!(p.notes[0], "swap");
    }
}
