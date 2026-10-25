//! Cross-language check for payment-channel vouchers (`sdk-ts/src/voucher.ts`).
//!
//! `sdk-ts/test/fixtures/voucher.json` holds vouchers that `btb-voucher` signed. Ed25519 is
//! deterministic, so the TypeScript side must reproduce the digest, the signature and the wire
//! bytes exactly, and must verify the Rust signatures. Regenerate with
//! `BTB_UPDATE_FIXTURES=1 cargo test -p btb-tests --test ts_voucher_fixture`.

use btb_voucher::Voucher;
use ed25519_dalek::SigningKey;
use std::path::PathBuf;

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn fixture() -> String {
    let seed = [7u8; 32];
    let key = SigningKey::from_bytes(&seed);
    let cases = [
        ("basic", 7u64, [0xabu8; 32], 1_000u64, 1u64),
        ("zero_amount", 7, [0x01; 32], 0, 0),
        ("large", u64::MAX, [0xff; 32], u64::MAX, u64::MAX),
        ("other_chain", 8, [0xab; 32], 1_000, 1),
    ];
    let mut out = format!(
        "{{\n  \"seed\": \"{}\",\n  \"public_key\": \"{}\",\n  \"vouchers\": [\n",
        hex(&seed),
        hex(&key.verifying_key().to_bytes())
    );
    for (i, (name, chain_id, channel_id, amount, nonce)) in cases.iter().enumerate() {
        let v = Voucher {
            chain_id: *chain_id,
            channel_id: *channel_id,
            amount: *amount,
            nonce: *nonce,
        };
        let sv = v.sign(&key);
        assert!(sv.verify(&key.verifying_key().to_bytes()));
        let sep = if i + 1 == cases.len() { "" } else { "," };
        out.push_str(&format!(
            "    {{\"name\": \"{name}\", \"chain_id\": \"{chain_id}\", \"channel_id\": \"{}\", \"amount\": \"{amount}\", \"nonce\": \"{nonce}\", \"digest\": \"{}\", \"signature\": \"{}\", \"bytes\": \"{}\"}}{sep}\n",
            hex(channel_id),
            hex(&v.digest()),
            hex(&sv.signature),
            hex(&sv.to_bytes()),
        ));
    }
    out.push_str("  ]\n}\n");
    out
}

#[test]
fn ts_voucher_fixture_is_current() {
    let json = fixture();
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../sdk-ts/test/fixtures/voucher.json");
    if std::env::var_os("BTB_UPDATE_FIXTURES").is_some() {
        std::fs::write(&path, &json).unwrap();
    }
    let on_disk = std::fs::read_to_string(&path).unwrap_or_default();
    assert_eq!(
        on_disk, json,
        "sdk-ts/test/fixtures/voucher.json is stale; run with BTB_UPDATE_FIXTURES=1"
    );
}
