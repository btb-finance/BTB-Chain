use btb_voucher::*;
use ed25519_dalek::SigningKey;

fn key() -> SigningKey {
    SigningKey::from_bytes(&[5; 32])
}
fn pk() -> [u8; 32] {
    key().verifying_key().to_bytes()
}

#[test]
fn roundtrip_and_tamper() {
    let sv = Voucher {
        chain_id: 7,
        channel_id: [1; 32],
        amount: 10,
        nonce: 1,
    }
    .sign(&key());
    assert!(sv.verify(&pk()));
    assert_eq!(SignedVoucher::from_bytes(&sv.to_bytes()), Some(sv));
    assert_eq!(SignedVoucher::from_bytes(&sv.to_bytes()[1..]), None);
    let mut t = sv;
    t.voucher.amount += 1;
    assert!(!t.verify(&pk()));
    let mut t = sv;
    t.voucher.chain_id = 8;
    assert!(!t.verify(&pk()));
    let mut t = sv;
    t.voucher.channel_id[0] ^= 1;
    assert!(!t.verify(&pk()));
    let mut t = sv;
    t.signature[0] ^= 1;
    assert!(!t.verify(&pk()));
    assert!(!sv.verify(&SigningKey::from_bytes(&[6; 32]).verifying_key().to_bytes()));
}

#[test]
fn spender_and_ledger() {
    let mut s = Spender::new(7, [1; 32], 100);
    let mut l = Ledger::new(7, [1; 32], pk(), 100);
    let a = s.pay(30, &key()).unwrap();
    let b = s.pay(20, &key()).unwrap();
    assert_eq!(l.accept(&a), Ok(30));
    assert_eq!(l.accept(&b), Ok(20));
    assert_eq!(l.accept(&a), Err(VoucherError::NotIncreasing));
    assert_eq!(l.accept(&b), Err(VoucherError::NotIncreasing));
    assert_eq!(l.earned(), 50);
    assert_eq!(s.pay(0, &key()), Err(VoucherError::BadAmount));
    assert_eq!(s.pay(51, &key()), Err(VoucherError::ExceedsDeposit));
    assert_eq!(s.spent(), 50);
    s.add_deposit(10).unwrap();
    assert_eq!(s.remaining(), 60);
    // Same amount, higher nonce is still a replay of the amount; higher amount with a stale nonce too.
    let forged = Voucher {
        chain_id: 7,
        channel_id: [1; 32],
        amount: 60,
        nonce: 1,
    }
    .sign(&key());
    assert_eq!(l.accept(&forged), Err(VoucherError::StaleNonce));
    let over = Voucher {
        chain_id: 7,
        channel_id: [1; 32],
        amount: 101,
        nonce: 9,
    }
    .sign(&key());
    assert_eq!(l.accept(&over), Err(VoucherError::ExceedsDeposit));
    let other_chain = Voucher {
        chain_id: 8,
        channel_id: [1; 32],
        amount: 60,
        nonce: 9,
    }
    .sign(&key());
    assert_eq!(l.accept(&other_chain), Err(VoucherError::WrongChain));
    let other_ch = Voucher {
        chain_id: 7,
        channel_id: [2; 32],
        amount: 60,
        nonce: 9,
    }
    .sign(&key());
    assert_eq!(l.accept(&other_ch), Err(VoucherError::WrongChannel));
    let stranger = Voucher {
        chain_id: 7,
        channel_id: [1; 32],
        amount: 60,
        nonce: 9,
    }
    .sign(&SigningKey::from_bytes(&[9; 32]));
    assert_eq!(l.accept(&stranger), Err(VoucherError::BadSignature));
    assert_eq!(l.best().unwrap().voucher.amount, 50);
}
