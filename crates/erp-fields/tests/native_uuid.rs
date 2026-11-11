//! Purpose-neutral native UUID prerequisite; no ORM field or UI claim.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
use erp_fields::{ErrorKind, Uuid};

#[test]
fn native_uuid_is_lossless_canonical_and_does_not_impose_correlation_rules() {
    for value in [
        "00000000-0000-0000-0000-000000000000",
        "01234567-89ab-cdef-0123-456789abcdef",
        "ffffffff-ffff-ffff-ffff-ffffffffffff",
        "018f0000-0000-7000-8000-000000000001",
    ] {
        let parsed = Uuid::parse(value).unwrap();
        assert_eq!(parsed.to_string(), value);
        assert_eq!(Uuid::from_bytes(*parsed.as_bytes()), parsed);
        assert_eq!(value.parse::<Uuid>().unwrap(), parsed);
    }
    assert!(Uuid::NIL.is_nil());
    assert!(
        !Uuid::parse("018f0000-0000-7000-8000-000000000001")
            .unwrap()
            .is_nil()
    );
    for invalid in [
        "",
        "01234567-89AB-CDEF-0123-456789ABCDEF",
        "0123456789abcdef0123456789abcdef",
        "{01234567-89ab-cdef-0123-456789abcdef}",
        "01234567-89ab-cdef-0123-456789abcdeg",
        "01234567-89ab-cdef-0123-456789abcdef ",
        "é1234567-89ab-cdef-0123-456789abcdef",
    ] {
        let error = Uuid::parse(invalid).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "invalid UUID: expected canonical lowercase 8-4-4-4-12 hexadecimal text"
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
    fn uuid_wire_uses_native_type_and_round_trips_nullable_arrays() {
        let first = Uuid::parse("01234567-89ab-cdef-0123-456789abcdef").unwrap();
        let mut bytes = BytesMut::new();
        first.to_sql_checked(&Type::UUID, &mut bytes).unwrap();
        assert_eq!(&bytes[..], first.as_bytes());
        assert_eq!(Uuid::from_sql(&Type::UUID, &bytes).unwrap(), first);
        assert!(<Uuid as ToSql>::accepts(&Type::UUID));
        assert!(!<Uuid as ToSql>::accepts(&Type::TEXT));
        assert!(
            first
                .to_sql_checked(&Type::TEXT, &mut BytesMut::new())
                .is_err()
        );
        for length in [0, 15, 17] {
            codec_error(
                Uuid::from_sql(&Type::UUID, &vec![0; length]),
                ErrorKind::Validation,
                "UUID binary value must contain exactly 16 bytes",
            );
        }
        codec_error(
            Uuid::from_sql(&Type::TEXT, &[0; 16]),
            ErrorKind::Programming,
            "UUID SQL decoder expected uuid",
        );
        let mut unchanged = BytesMut::from(&b"kept"[..]);
        codec_error(
            first.to_sql(&Type::TEXT, &mut unchanged),
            ErrorKind::Programming,
            "UUID SQL encoder expected uuid",
        );
        assert_eq!(&unchanged[..], b"kept");
        let mut empty = BytesMut::new();
        assert!(matches!(
            None::<Uuid>
                .to_sql_checked(&Type::UUID, &mut empty)
                .unwrap(),
            postgres_types::IsNull::Yes
        ));
        assert!(empty.is_empty());
        for values in [vec![], vec![None], vec![Some(first), None, Some(Uuid::NIL)]] {
            let mut bytes = BytesMut::new();
            values
                .to_sql_checked(&Type::UUID_ARRAY, &mut bytes)
                .unwrap();
            let restored = Vec::<Option<Uuid>>::from_sql(&Type::UUID_ARRAY, &bytes).unwrap();
            assert_eq!(restored, values);
        }
        let values = vec![Some(first)];
        assert!(
            values
                .to_sql_checked(&Type::INT8_ARRAY, &mut BytesMut::new())
                .is_err()
        );
    }
}
