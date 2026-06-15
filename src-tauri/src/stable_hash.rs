//! Rustのバージョンに依存しない安定ハッシュ
//! DefaultHasher はバージョン間で出力が変わり得るため、永続化する識別子には使えない

/// FNV-1a 64bit
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;
    let mut hash = OFFSET;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}
