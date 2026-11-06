//! NTFS boot sector (first sector of the volume).

use crate::bytes::*;
use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootSector {
    pub bytes_per_sector: u32,
    pub cluster_size: u32,
    pub total_sectors: u64,
    pub mft_lcn: u64,
    pub mftmirr_lcn: u64,
    /// Size of one FILE record (usually 1024, 4096 on 4Kn drives).
    pub record_size: u32,
    pub index_record_size: u32,
    pub serial: u64,
}

impl BootSector {
    pub fn parse(b: &[u8]) -> Result<Self> {
        if b.len() < 512 {
            return Err(Error::BadBootSector("short read"));
        }
        if &b[3..11] != b"NTFS    " {
            return Err(Error::NotNtfs);
        }
        let bps = u16le(b, 0x0B) as u32;
        if !bps.is_power_of_two() || !(256..=4096).contains(&bps) {
            return Err(Error::BadBootSector("bytes per sector"));
        }
        // Values above 0x80 encode 2^(256 - v) sectors (clusters > 64 KiB, Win10 1709+).
        let spc_raw = u8at(b, 0x0D) as u32;
        let spc = if spc_raw <= 0x80 {
            spc_raw
        } else {
            let sh = 256 - spc_raw;
            if sh > 20 {
                return Err(Error::BadBootSector("sectors per cluster"));
            }
            1 << sh
        };
        if spc == 0 || !spc.is_power_of_two() {
            return Err(Error::BadBootSector("sectors per cluster"));
        }
        let cluster_size = bps * spc;
        let decode_rec = |v: i8| -> Result<u32> {
            let s = if v > 0 {
                (v as u32).checked_mul(cluster_size).ok_or(Error::BadBootSector("record size"))?
            } else {
                let sh = (-(v as i32)) as u32;
                if !(8..=16).contains(&sh) {
                    return Err(Error::BadBootSector("record size"));
                }
                1u32 << sh
            };
            if !s.is_power_of_two() || !(512..=65536).contains(&s) {
                return Err(Error::BadBootSector("record size"));
            }
            Ok(s)
        };
        let record_size = decode_rec(u8at(b, 0x40) as i8)?;
        let index_record_size = decode_rec(u8at(b, 0x44) as i8)?;
        if record_size < bps {
            return Err(Error::BadBootSector("record smaller than sector"));
        }
        let total_sectors = u64le(b, 0x28);
        let mft_lcn = u64le(b, 0x30);
        let mftmirr_lcn = u64le(b, 0x38);
        let clusters = total_sectors / spc as u64;
        if mft_lcn == 0 || mft_lcn >= clusters {
            return Err(Error::BadBootSector("$MFT location"));
        }
        Ok(BootSector {
            bytes_per_sector: bps,
            cluster_size,
            total_sectors,
            mft_lcn,
            mftmirr_lcn,
            record_size,
            index_record_size,
            serial: u64le(b, 0x48),
        })
    }

    pub fn total_clusters(&self) -> u64 {
        self.volume_bytes() / self.cluster_size as u64
    }

    pub fn volume_bytes(&self) -> u64 {
        self.total_sectors * self.bytes_per_sector as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(spc: u8, cpr: i8) -> Vec<u8> {
        let mut b = vec![0u8; 512];
        b[3..11].copy_from_slice(b"NTFS    ");
        b[0x0B..0x0D].copy_from_slice(&512u16.to_le_bytes());
        b[0x0D] = spc;
        b[0x28..0x30].copy_from_slice(&(1u64 << 34).to_le_bytes()); // 8 TiB
        b[0x30..0x38].copy_from_slice(&786432u64.to_le_bytes());
        b[0x38..0x40].copy_from_slice(&2u64.to_le_bytes());
        b[0x40] = cpr as u8;
        b[0x44] = (-12i8) as u8; // 4096-byte index records
        b
    }

    #[test]
    fn typical_4k_clusters() {
        let bs = BootSector::parse(&sample(8, -10)).unwrap();
        assert_eq!(bs.cluster_size, 4096);
        assert_eq!(bs.record_size, 1024);
        assert_eq!(bs.index_record_size, 4096);
        assert_eq!(bs.mft_lcn, 786432);
    }

    #[test]
    fn large_cluster_encoding() {
        // 0xF4 => 2^(256-244) = 4096 sectors => 2 MiB clusters
        let bs = BootSector::parse(&sample(0xF4, -10)).unwrap();
        assert_eq!(bs.cluster_size, 2 * 1024 * 1024);
        assert_eq!(bs.record_size, 1024);
    }

    #[test]
    fn rejects_non_ntfs() {
        let mut b = sample(8, -10);
        b[3] = b'F';
        assert!(matches!(BootSector::parse(&b), Err(Error::NotNtfs)));
    }
}
