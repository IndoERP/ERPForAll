//! Unsigned, nonwrapping PostgreSQL full transaction-id prerequisite.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
use erp_fields::{ErrorKind, Xid8};

#[test]
fn xid8_preserves_all_unsigned_bits_without_xid32_or_i64_coercion() {
    for value in [
        0_u64,
        1,
        u32::MAX as u64,
        u32::MAX as u64 + 1,
        i64::MAX as u64 + 1,
        u64::MAX,
    ] {
        let text = value.to_string();
        let parsed = Xid8::parse(&text).unwrap();
        assert_eq!(parsed.get(), value);
        assert_eq!(parsed.to_string(), text);
        assert_eq!(text.parse::<Xid8>().unwrap(), Xid8::from_u64(value));
    }
    assert!(Xid8::from_u64(i64::MAX as u64) < Xid8::from_u64(i64::MAX as u64 + 1));
    for invalid in [
        "",
        "+1",
        "-1",
        "01",
        " 1",
        "1 ",
        "1.0",
        "18446744073709551616",
        "١",
    ] {
        let error = Xid8::parse(invalid).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "invalid xid8: expected canonical unsigned 64-bit decimal text"
        );
    }
}

#[cfg(feature = "pg")]
mod pg {
    use super::*;
    use bytes::BytesMut;
    use postgres_types::{FromSql, ToSql, Type};

    fn codec_error<T>(
        result: Result<T, Box<dyn std::error::Error + Sync + Send>>,
        kind: ErrorKind,
        message: &str,
    ) {
        let error = result.err().expect("codec must refuse this input");
        let error = error
            .downcast_ref::<erp_fields::Error>()
            .expect("typed codec error");
        assert_eq!(error.kind(), kind);
        assert_eq!(error.message(), message);
    }

    #[test]
    fn xid8_wire_round_trips_high_bits_and_nullable_arrays() {
        let value = Xid8::from_u64(u64::MAX);
        let mut bytes = BytesMut::new();
        value.to_sql_checked(&Type::XID8, &mut bytes).unwrap();
        assert_eq!(&bytes[..], &u64::MAX.to_be_bytes());
        assert_eq!(Xid8::from_sql(&Type::XID8, &bytes).unwrap(), value);
        assert!(<Xid8 as ToSql>::accepts(&Type::XID8));
        assert!(!<Xid8 as ToSql>::accepts(&Type::INT8));
        assert!(
            value
                .to_sql_checked(&Type::INT8, &mut BytesMut::new())
                .is_err()
        );
        for ty in [Type::INT8, Type::XID, Type::TEXT] {
            codec_error(
                Xid8::from_sql(&ty, &[0; 8]),
                ErrorKind::Programming,
                "xid8 SQL decoder expected xid8",
            );
        }
        for length in [0, 7, 9] {
            codec_error(
                Xid8::from_sql(&Type::XID8, &vec![0; length]),
                ErrorKind::Validation,
                "xid8 binary value must contain exactly 8 bytes",
            );
        }
        let mut unchanged = BytesMut::from(&b"kept"[..]);
        codec_error(
            value.to_sql(&Type::INT8, &mut unchanged),
            ErrorKind::Programming,
            "xid8 SQL encoder expected xid8",
        );
        assert_eq!(&unchanged[..], b"kept");
        let mut empty = BytesMut::new();
        assert!(matches!(
            None::<Xid8>
                .to_sql_checked(&Type::XID8, &mut empty)
                .unwrap(),
            postgres_types::IsNull::Yes
        ));
        assert!(empty.is_empty());
        for values in [
            vec![],
            vec![None],
            vec![Some(value), None, Some(Xid8::from_u64(0))],
        ] {
            let mut bytes = BytesMut::new();
            values
                .to_sql_checked(&Type::XID8_ARRAY, &mut bytes)
                .unwrap();
            assert_eq!(
                Vec::<Option<Xid8>>::from_sql(&Type::XID8_ARRAY, &bytes).unwrap(),
                values
            );
        }
    }
}
