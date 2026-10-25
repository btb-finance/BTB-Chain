//! External addresses derived from group keys.
//!
//! * Bitcoin Taproot (BIP-341/BIP-350): `bech32m(hrp, witness v1, output key)`.
//! * Solana: base58 of the 32-byte Ed25519 public key.

const BECH32_CHARSET: &[u8; 32] = b"qpzry9x8gf2tvdw0s3jn54khce6mua7l";
const BECH32M_CONST: u32 = 0x2bc830a3;

fn polymod(values: &[u8]) -> u32 {
    const GEN: [u32; 5] = [0x3b6a57b2, 0x26508e6d, 0x1ea119fa, 0x3d4233dd, 0x2a1462b3];
    let mut chk = 1u32;
    for v in values {
        let top = chk >> 25;
        chk = ((chk & 0x1ff_ffff) << 5) ^ u32::from(*v);
        for (i, g) in GEN.iter().enumerate() {
            if (top >> i) & 1 == 1 {
                chk ^= g;
            }
        }
    }
    chk
}

fn hrp_expand(hrp: &str) -> Vec<u8> {
    let mut v: Vec<u8> = hrp.bytes().map(|b| b >> 5).collect();
    v.push(0);
    v.extend(hrp.bytes().map(|b| b & 31));
    v
}

fn convert_bits(data: &[u8], from: u32, to: u32) -> Vec<u8> {
    let (mut acc, mut bits) = (0u32, 0u32);
    let mut out = Vec::new();
    for &b in data {
        acc = (acc << from) | u32::from(b);
        bits += from;
        while bits >= to {
            bits -= to;
            out.push(((acc >> bits) & ((1 << to) - 1)) as u8);
        }
    }
    if bits > 0 {
        out.push(((acc << (to - bits)) & ((1 << to) - 1)) as u8);
    }
    out
}

/// The Taproot address (witness version 1) of an x-only output key, for `hrp` `bc`, `tb` or
/// `bcrt`.
pub fn taproot_address(hrp: &str, output_key: &[u8; 32]) -> String {
    let mut data = vec![1u8]; // witness version
    data.extend(convert_bits(output_key, 8, 5));
    let mut values = hrp_expand(hrp);
    values.extend_from_slice(&data);
    values.extend_from_slice(&[0; 6]);
    let pm = polymod(&values) ^ BECH32M_CONST;
    let checksum: Vec<u8> = (0..6).map(|i| ((pm >> (5 * (5 - i))) & 31) as u8).collect();
    let mut s = String::from(hrp);
    s.push('1');
    for d in data.iter().chain(checksum.iter()) {
        s.push(BECH32_CHARSET[*d as usize] as char);
    }
    s
}

/// The output key of a Taproot address (`None`: not a valid bech32m witness-v1 address of 32
/// bytes for this `hrp`).
pub fn parse_taproot_address(hrp: &str, addr: &str) -> Option<[u8; 32]> {
    let addr = addr.to_ascii_lowercase();
    let rest = addr.strip_prefix(hrp)?.strip_prefix('1')?;
    let data: Vec<u8> = rest
        .bytes()
        .map(|c| BECH32_CHARSET.iter().position(|x| *x == c).map(|p| p as u8))
        .collect::<Option<_>>()?;
    if data.len() < 7 {
        return None;
    }
    let mut values = hrp_expand(hrp);
    values.extend_from_slice(&data);
    if polymod(&values) != BECH32M_CONST || data[0] != 1 {
        return None;
    }
    let payload = &data[1..data.len() - 6];
    // 5-bit groups to bytes, with zero padding only
    let (mut acc, mut bits) = (0u32, 0u32);
    let mut out = Vec::new();
    for &d in payload {
        acc = (acc << 5) | u32::from(d);
        bits += 5;
        while bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    if bits >= 5 || (acc << (8 - bits)) & 0xff != 0 {
        return None;
    }
    out.try_into().ok()
}

/// The scriptPubKey of a Taproot output: `OP_1 <32 bytes>`.
pub fn taproot_script_pubkey(output_key: &[u8; 32]) -> Vec<u8> {
    let mut s = vec![0x51, 0x20];
    s.extend_from_slice(output_key);
    s
}

const B58: &[u8; 58] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

pub fn base58_encode(bytes: &[u8]) -> String {
    let zeros = bytes.iter().take_while(|b| **b == 0).count();
    let mut digits: Vec<u8> = Vec::new();
    for &b in bytes {
        let mut carry = u32::from(b);
        for d in digits.iter_mut() {
            carry += u32::from(*d) << 8;
            *d = (carry % 58) as u8;
            carry /= 58;
        }
        while carry > 0 {
            digits.push((carry % 58) as u8);
            carry /= 58;
        }
    }
    let mut s = "1".repeat(zeros);
    s.extend(digits.iter().rev().map(|d| B58[*d as usize] as char));
    s
}

pub fn base58_decode(s: &str) -> Option<Vec<u8>> {
    let zeros = s.bytes().take_while(|b| *b == b'1').count();
    let mut bytes: Vec<u8> = Vec::new();
    for c in s.bytes() {
        let mut carry = B58.iter().position(|x| *x == c)? as u32;
        for b in bytes.iter_mut() {
            carry += u32::from(*b) * 58;
            *b = (carry & 0xff) as u8;
            carry >>= 8;
        }
        while carry > 0 {
            bytes.push((carry & 0xff) as u8);
            carry >>= 8;
        }
    }
    let mut out = vec![0u8; zeros];
    out.extend(bytes.iter().rev());
    Some(out)
}

/// The Solana address (account public key) of an Ed25519 group key.
pub fn solana_address(public_key: &[u8; 32]) -> String {
    base58_encode(public_key)
}

pub fn parse_solana_address(addr: &str) -> Option<[u8; 32]> {
    base58_decode(addr)?.try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len() / 2)
            .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn bip350_taproot_vector() {
        // BIP-350: bc1p0xlxvlhemja6c4dqv22uapctqupfhlxm9h8z3k2e72q4k9hcz7vqzk5jj0
        let key: [u8; 32] =
            unhex("79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798")
                .try_into()
                .unwrap();
        let a = taproot_address("bc", &key);
        assert_eq!(
            a,
            "bc1p0xlxvlhemja6c4dqv22uapctqupfhlxm9h8z3k2e72q4k9hcz7vqzk5jj0"
        );
        assert_eq!(parse_taproot_address("bc", &a), Some(key));
        // wrong hrp, damaged checksum, uppercase is fine
        assert_eq!(parse_taproot_address("tb", &a), None);
        let mut bad = a.clone();
        bad.pop();
        bad.push('q');
        assert_eq!(parse_taproot_address("bc", &bad), None);
        assert_eq!(parse_taproot_address("bc", &a.to_uppercase()), Some(key));
        assert_eq!(taproot_script_pubkey(&key)[..2], [0x51, 0x20]);
    }

    #[test]
    fn solana_base58() {
        assert_eq!(solana_address(&[0u8; 32]), "1".repeat(32));
        let key = [0xABu8; 32];
        let a = solana_address(&key);
        assert_eq!(parse_solana_address(&a), Some(key));
        // the System Program id
        assert_eq!(
            parse_solana_address("11111111111111111111111111111111"),
            Some([0u8; 32])
        );
        assert_eq!(parse_solana_address("0OIl"), None);
        assert_eq!(base58_encode(&[0, 0, 1]), "112");
    }
}
