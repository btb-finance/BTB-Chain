//! Minimal hex helpers (no dependency).

pub fn encode(b: &[u8]) -> String {
    const D: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push(D[(x >> 4) as usize] as char);
        s.push(D[(x & 15) as usize] as char);
    }
    s
}

pub fn decode(s: &str) -> Result<Vec<u8>, String> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    if !s.len().is_multiple_of(2) {
        return Err("odd hex length".into());
    }
    fn nib(c: u8) -> Result<u8, String> {
        match c {
            b'0'..=b'9' => Ok(c - b'0'),
            b'a'..=b'f' => Ok(c - b'a' + 10),
            b'A'..=b'F' => Ok(c - b'A' + 10),
            _ => Err(format!("bad hex digit {:?}", c as char)),
        }
    }
    s.as_bytes()
        .chunks(2)
        .map(|p| Ok(nib(p[0])? << 4 | nib(p[1])?))
        .collect()
}

pub fn decode32(s: &str) -> Result<[u8; 32], String> {
    decode(s)?
        .try_into()
        .map_err(|_| "expected 32 bytes (64 hex digits)".to_string())
}
