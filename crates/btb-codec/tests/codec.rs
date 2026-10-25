use btb_codec::*;

fn rt<T: Encode + Decode + PartialEq + core::fmt::Debug>(v: T) {
    let b = v.encode();
    assert_eq!(T::decode(&b).unwrap(), v);
}

#[test]
fn roundtrips() {
    rt(true);
    rt(false);
    rt(0u8);
    rt(u64::MAX);
    rt(i8::MIN);
    rt(i64::MIN);
    rt(u128::MAX);
    rt(i128::MIN);
    rt(String::from("héllo ✓"));
    rt(Some(7u32));
    rt(None::<u32>);
    rt(vec![1u16, 2, 3]);
    rt(vec![String::from("a"), String::new()]);
    rt([1u8, 2, 3, 4]);
    rt([[1u16, 2], [3, 4]]);
    rt((1u8, String::from("x"), Some(true)));
    rt(Address([9; 32]));
    rt(Some(vec![Some(1i32), None]));
}

#[test]
fn exact_vectors() {
    assert_eq!(true.encode(), [1]);
    assert_eq!(0x0102u16.encode(), [2, 1]);
    assert_eq!(0x01020304u32.encode(), [4, 3, 2, 1]);
    assert_eq!((-2i8).encode(), [0xFE]);
    assert_eq!((-1i32).encode(), [0xFF; 4]);
    assert_eq!(
        1u128.encode(),
        [1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
    );
    assert_eq!("hi".encode(), [2, 0, 0, 0, b'h', b'i']);
    assert_eq!(String::new().encode(), [0, 0, 0, 0]);
    assert_eq!(Some(5u8).encode(), [1, 5]);
    assert_eq!(None::<u8>.encode(), [0]);
    assert_eq!(vec![1u16, 2].encode(), [2, 0, 0, 0, 1, 0, 2, 0]);
    assert_eq!(vec![7u8, 8].encode(), [2, 0, 0, 0, 7, 8]);
    assert_eq!([1u16, 2].encode(), [1, 0, 2, 0]);
    assert_eq!((1u8, true).encode(), [1, 1]);
    assert_eq!(Address([0xAB; 32]).encode(), [0xAB; 32]);
}

#[test]
fn rejections() {
    assert_eq!(bool::decode(&[2]), Err(DecodeError::InvalidBool(2)));
    assert_eq!(
        Option::<u8>::decode(&[2, 0]),
        Err(DecodeError::InvalidOptionTag(2))
    );
    assert_eq!(u8::decode(&[1, 2]), Err(DecodeError::TrailingBytes));
    assert_eq!(u32::decode(&[1, 2, 3]), Err(DecodeError::UnexpectedEof));
    assert_eq!(u8::decode(&[]), Err(DecodeError::UnexpectedEof));
    assert_eq!(
        String::decode(&[1, 0, 0, 0, 0xFF]),
        Err(DecodeError::InvalidUtf8)
    );
    // overlong / surrogate UTF-8
    assert_eq!(
        String::decode(&[2, 0, 0, 0, 0xC0, 0x80]),
        Err(DecodeError::InvalidUtf8)
    );
    assert_eq!(
        String::decode(&[5, 0, 0, 0, b'a']),
        Err(DecodeError::UnexpectedEof)
    );
    assert_eq!(Address::decode(&[0; 31]), Err(DecodeError::UnexpectedEof));
    assert_eq!(<[u8; 3]>::decode(&[1, 2]), Err(DecodeError::UnexpectedEof));
}

#[test]
fn length_limits() {
    let huge = [0xFF, 0xFF, 0xFF, 0xFF];
    assert!(matches!(
        Vec::<u8>::decode(&huge),
        Err(DecodeError::LengthTooLarge { .. })
    ));
    assert!(matches!(
        String::decode(&huge),
        Err(DecodeError::LengthTooLarge { .. })
    ));
    // within the default max but input is short: EOF, no giant allocation
    assert_eq!(
        Vec::<u64>::decode(&[0, 0, 0, 1]),
        Err(DecodeError::UnexpectedEof)
    );
    let enc = vec![1u8, 2, 3].encode();
    assert!(matches!(
        Vec::<u8>::decode_with_max(&enc, 2),
        Err(DecodeError::LengthTooLarge { len: 3, max: 2 })
    ));
    assert!(Vec::<u8>::decode_with_max(&enc, 3).is_ok());
}

#[test]
fn fuzz_never_panics() {
    let mut s: u64 = 0x9E3779B97F4A7C15;
    let mut next = || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    for _ in 0..20000 {
        let n = (next() % 48) as usize;
        let buf: Vec<u8> = (0..n).map(|_| next() as u8).collect();
        let _ = bool::decode(&buf);
        let _ = u128::decode(&buf);
        let _ = String::decode_with_max(&buf, 64);
        let _ = Vec::<String>::decode_with_max(&buf, 64);
        let _ = Option::<Vec<u16>>::decode_with_max(&buf, 64);
        let _ = <(u8, Option<String>, [u16; 3], Address)>::decode_with_max(&buf, 64);
        // anything that decodes must re-encode to the same bytes (canonical)
        if let Ok(v) = Vec::<Option<String>>::decode_with_max(&buf, 64) {
            assert_eq!(v.encode(), buf);
        }
    }
}

#[test]
fn unit_is_zero_bytes() {
    use btb_codec::{Decode, Encode};
    assert!(().encode().is_empty());
    assert_eq!(<()>::decode(&[]), Ok(()));
    assert_eq!(
        <()>::decode(&[0]),
        Err(btb_codec::DecodeError::TrailingBytes)
    );
}
