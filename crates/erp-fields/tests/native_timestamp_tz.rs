//! Bounded explicit-offset microsecond instants; existing Datetime stays naive UTC.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
use erp_fields::{DateTime, ErrorKind, TimestampTz};

const INVALID: &str =
    "invalid timestamptz: expected explicit-offset ISO timestamp with at most 6 fractional digits";

#[test]
fn explicit_offsets_round_trip_without_changing_naive_datetime() {
    let local = TimestampTz::parse("2024-03-01T01:00:00.123456+07:00").unwrap();
    let utc = TimestampTz::parse("2024-02-29T18:00:00.123456Z").unwrap();
    assert_eq!(local, utc);
    assert_eq!(local.to_string(), "2024-02-29T18:00:00.123456Z");
    assert_eq!(
        TimestampTz::from_unix_micros(local.unix_micros()).unwrap(),
        local
    );
    let before = TimestampTz::parse("1969-12-31T23:59:59.999999Z").unwrap();
    assert_eq!(before.unix_micros(), -1);
    assert_eq!(before.to_string(), "1969-12-31T23:59:59.999999Z");
    assert_eq!(
        DateTime::parse_odoo("2024-03-01 01:00:00")
            .unwrap()
            .to_string(),
        "2024-03-01 01:00:00"
    );
    for invalid in [
        "",
        "2024-03-01T01:00:00",
        "2024-03-01 01:00:00Z",
        "2024-03-01T01:00:00.1234567Z",
        "2024-03-01T01:00:00.0000000Z",
        "2024-03-01T01:00:60Z",
        "2024-03-01T24:00:00Z",
        "2024-03-01T01:00:00-00:00",
        "2024-03-01T01:00:00Z[UTC]",
        "infinity",
        "-infinity",
    ] {
        let error = TimestampTz::parse(invalid).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(error.message(), INVALID);
    }
    for invalid in [i64::MIN, i64::MAX] {
        let error = TimestampTz::from_unix_micros(invalid).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "timestamptz outside supported finite instant range"
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
        let error = result.err().expect("codec must refuse input");
        let error = error
            .downcast_ref::<erp_fields::Error>()
            .expect("typed error");
        assert_eq!(error.kind(), kind);
        assert_eq!(error.message(), message);
    }

    #[test]
    fn timestamp_tz_codec_is_distinct_from_timestamp_and_never_truncates() {
        let epoch = TimestampTz::from_unix_micros(0).unwrap();
        let local = TimestampTz::parse("2024-03-01T01:00:00.123456+07:00").unwrap();
        let mut bytes = BytesMut::new();
        epoch
            .to_sql_checked(&Type::TIMESTAMPTZ, &mut bytes)
            .unwrap();
        assert_eq!(&bytes[..], &(-946_684_800_000_000_i64).to_be_bytes());
        assert_eq!(
            TimestampTz::from_sql(&Type::TIMESTAMPTZ, &bytes).unwrap(),
            epoch
        );
        assert!(<TimestampTz as ToSql>::accepts(&Type::TIMESTAMPTZ));
        assert!(!<TimestampTz as ToSql>::accepts(&Type::TIMESTAMP));
        codec_error(
            TimestampTz::from_sql(&Type::TIMESTAMP, &[0; 8]),
            ErrorKind::Programming,
            "timestamptz SQL decoder expected timestamptz",
        );
        for length in [0, 7, 9] {
            codec_error(
                TimestampTz::from_sql(&Type::TIMESTAMPTZ, &vec![0; length]),
                ErrorKind::Validation,
                "timestamptz binary value must contain exactly 8 bytes",
            );
        }
        for infinite in [i64::MIN, i64::MAX] {
            codec_error(
                TimestampTz::from_sql(&Type::TIMESTAMPTZ, &infinite.to_be_bytes()),
                ErrorKind::Validation,
                "infinite timestamptz values are not supported",
            );
        }
        for outside in [i64::MIN + 1, i64::MAX - 1] {
            codec_error(
                TimestampTz::from_sql(&Type::TIMESTAMPTZ, &outside.to_be_bytes()),
                ErrorKind::Validation,
                "timestamptz outside supported finite instant range",
            );
        }
        let mut unchanged = BytesMut::from(&b"kept"[..]);
        codec_error(
            epoch.to_sql(&Type::TIMESTAMP, &mut unchanged),
            ErrorKind::Programming,
            "timestamptz SQL encoder expected timestamptz",
        );
        assert_eq!(&unchanged[..], b"kept");
        for values in [vec![], vec![None], vec![Some(epoch), None, Some(local)]] {
            let mut bytes = BytesMut::new();
            values
                .to_sql_checked(&Type::TIMESTAMPTZ_ARRAY, &mut bytes)
                .unwrap();
            assert_eq!(
                Vec::<Option<TimestampTz>>::from_sql(&Type::TIMESTAMPTZ_ARRAY, &bytes).unwrap(),
                values
            );
        }
        let mut empty = BytesMut::new();
        assert!(matches!(
            None::<TimestampTz>
                .to_sql_checked(&Type::TIMESTAMPTZ, &mut empty)
                .unwrap(),
            postgres_types::IsNull::Yes
        ));
        assert!(empty.is_empty());
    }
}
