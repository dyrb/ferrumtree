//! NTFS mapping pairs ("data runs") decoder.

use crate::error::{Error, Result};

/// A contiguous run of clusters. `lcn == None` is a sparse (unallocated) run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    pub vcn: u64,
    pub len: u64,
    pub lcn: Option<u64>,
}

#[inline]
fn read_uint(b: &[u8]) -> u64 {
    let mut v = 0u64;
    for (i, &x) in b.iter().enumerate() {
        v |= (x as u64) << (8 * i);
    }
    v
}

#[inline]
fn read_int(b: &[u8]) -> i64 {
    let v = read_uint(b);
    let bits = 8 * b.len() as u32;
    if bits == 0 || bits >= 64 {
        return v as i64;
    }
    let sh = 64 - bits; // sign-extend
    ((v << sh) as i64) >> sh
}

/// Decode the mapping pairs of one attribute segment starting at `start_vcn`.
/// LCN deltas restart from zero for every segment.
pub fn decode(mut p: &[u8], start_vcn: u64, out: &mut Vec<Extent>) -> Result<()> {
    let mut vcn = start_vcn;
    let mut lcn: i64 = 0;
    while let Some(&h) = p.first() {
        if h == 0 {
            break;
        }
        let ls = (h & 0x0F) as usize;
        let os = (h >> 4) as usize;
        if ls == 0 || ls > 8 || os > 8 || p.len() < 1 + ls + os {
            return Err(Error::BadRunlist);
        }
        let len = read_uint(&p[1..1 + ls]);
        if len == 0 {
            return Err(Error::BadRunlist);
        }
        if os == 0 {
            out.push(Extent { vcn, len, lcn: None });
        } else {
            lcn = lcn.checked_add(read_int(&p[1 + ls..1 + ls + os])).ok_or(Error::BadRunlist)?;
            if lcn < 0 {
                return Err(Error::BadRunlist);
            }
            out.push(Extent { vcn, len, lcn: Some(lcn as u64) });
        }
        vcn = vcn.checked_add(len).ok_or(Error::BadRunlist)?;
        p = &p[1 + ls + os..];
    }
    Ok(())
}

/// Count the allocated (non-sparse) clusters of one attribute segment without building
/// extents. This is the true on-disk footprint: header "allocated size" fields over-report
/// for sparse runs that are not flagged sparse (e.g. `$BadClus:$Bad` spans the volume).
#[inline]
pub fn allocated_clusters(mut p: &[u8]) -> Option<u64> {
    let mut total = 0u64;
    while let Some(&h) = p.first() {
        if h == 0 {
            break;
        }
        let ls = (h & 0x0F) as usize;
        let os = (h >> 4) as usize;
        if ls == 0 || ls > 8 || os > 8 || p.len() < 1 + ls + os {
            return None;
        }
        if os != 0 {
            total = total.checked_add(read_uint(&p[1..1 + ls]))?;
        }
        p = &p[1 + ls + os..];
    }
    Some(total)
}

/// Encode extents back to mapping pairs (used by tests and synthetic images).
pub fn encode(extents: &[Extent]) -> Vec<u8> {
    fn min_bytes_u(v: u64) -> usize {
        (64 - v.leading_zeros() as usize).div_ceil(8).max(1)
    }
    fn min_bytes_i(v: i64) -> usize {
        (1..=8)
            .find(|&n| {
                let sh = 64 - 8 * n as u32;
                n == 8 || ((v << sh) >> sh) == v
            })
            .unwrap()
    }
    let mut out = Vec::new();
    let mut prev = 0i64;
    for e in extents {
        let ls = min_bytes_u(e.len);
        match e.lcn {
            None => {
                out.push(ls as u8);
                out.extend_from_slice(&e.len.to_le_bytes()[..ls]);
            }
            Some(l) => {
                let d = l as i64 - prev;
                let os = min_bytes_i(d);
                out.push((os << 4 | ls) as u8);
                out.extend_from_slice(&e.len.to_le_bytes()[..ls]);
                out.extend_from_slice(&d.to_le_bytes()[..os]);
                prev = l as i64;
            }
        }
    }
    out.push(0);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_known_runs() {
        // 21 18 34 56 : len 0x18, lcn 0x5634
        // 11 10 F0    : len 0x10, lcn delta -16
        // 01 08       : sparse, 8 clusters
        let raw = [0x21, 0x18, 0x34, 0x56, 0x11, 0x10, 0xF0, 0x01, 0x08, 0x00];
        let mut v = Vec::new();
        decode(&raw, 0, &mut v).unwrap();
        assert_eq!(
            v,
            vec![
                Extent { vcn: 0, len: 0x18, lcn: Some(0x5634) },
                Extent { vcn: 0x18, len: 0x10, lcn: Some(0x5634 - 16) },
                Extent { vcn: 0x28, len: 8, lcn: None },
            ]
        );
    }

    #[test]
    fn roundtrip() {
        let ext = vec![
            Extent { vcn: 0, len: 300, lcn: Some(1_000_000) },
            Extent { vcn: 300, len: 7, lcn: Some(5) },
            Extent { vcn: 307, len: 1 << 20, lcn: None },
            Extent { vcn: 307 + (1 << 20), len: 2, lcn: Some(0x7F_FFFF_FFFF) },
        ];
        let mut v = Vec::new();
        decode(&encode(&ext), 0, &mut v).unwrap();
        assert_eq!(v, ext);
    }

    #[test]
    fn allocated_clusters_skips_sparse_runs() {
        let raw = [0x21, 0x18, 0x34, 0x56, 0x11, 0x10, 0xF0, 0x01, 0x08, 0x00];
        assert_eq!(allocated_clusters(&raw), Some(0x18 + 0x10));
        assert_eq!(allocated_clusters(&[0x33, 0x01]), None);
    }

    #[test]
    fn rejects_truncated() {
        let mut v = Vec::new();
        assert!(decode(&[0x33, 0x01], 0, &mut v).is_err());
    }
}
