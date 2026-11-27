//! CRC32C (Castagnoli, polynomial 0x1EDC6F41, reflected 0x82F63B78) — the
//! checksum of every durable block in LatticeDB (SPEC-03). Table-driven,
//! allocation-free, deterministic across platforms.

const POLY: u32 = 0x82F63B78;

fn table() -> &'static [u32; 256] {
    use std::sync::OnceLock;
    static TABLE: OnceLock<[u32; 256]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, slot) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { POLY ^ (c >> 1) } else { c >> 1 };
            }
            *slot = c;
        }
        t
    })
}

/// CRC32C of `data`, seeded with `init` (chaining across chunks).
pub fn crc32c(init: u32, data: &[u8]) -> u32 {
    let t = table();
    let mut c = init ^ 0xFFFF_FFFF;
    for &b in data {
        c = t[((c ^ u32::from(b)) & 0xFF) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}

pub fn crc32c_of(data: &[u8]) -> u32 {
    crc32c(0, data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        // Standard CRC-32C check values.
        assert_eq!(crc32c_of(b""), 0x0000_0000);
        assert_eq!(crc32c_of(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c_of(b"Hello LatticeDB"), crc32c_of(b"Hello LatticeDB"));
    }

    #[test]
    fn chaining_matches_single_pass() {
        let data = b"determinism is the replication contract";
        let whole = crc32c_of(data);
        let split = crc32c(crc32c(0, &data[..7]), &data[7..]);
        assert_eq!(whole, split);
    }

    #[test]
    fn different_bytes_different_crc() {
        assert_ne!(crc32c_of(b"txn-1"), crc32c_of(b"txn-2"));
    }
}
