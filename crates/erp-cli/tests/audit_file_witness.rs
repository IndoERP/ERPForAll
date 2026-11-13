// SPDX-License-Identifier: MIT OR Apache-2.0
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
use erp_cli::audit::FileWitness;
use erp_orm::audit::seal::{Anchor, AnchorRange, AuditWitness, SealIdentity, WitnessError};
use std::fs;

fn anchor(incarnation: u8, epoch: u64, batch: u64, hash: u8) -> Anchor {
    Anchor::new(
        SealIdentity::new([incarnation; 16], epoch, batch).unwrap(),
        [hash; 32],
        1,
    )
    .unwrap()
}
fn record() -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("witness.jsonl");
    FileWitness::new(&path)
        .unwrap()
        .publish(&anchor(1, 1, 1, 9))
        .unwrap();
    fs::read(path).unwrap()
}

#[test]
fn inventory_is_independent_and_sorted_by_identity() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("witness.jsonl");
    let witness = FileWitness::new(&path).unwrap();
    assert!(matches!(
        witness.inventory(),
        Err(WitnessError::Storage { .. })
    ));
    assert!(!path.exists());
    let entries = [
        anchor(2, 1, 1, 3),
        anchor(1, 2, 3, 4),
        anchor(1, 1, 2, 5),
        anchor(1, 1, 1, 6),
    ];
    for entry in &entries {
        witness.publish(entry).unwrap();
    }
    let actual = FileWitness::new(&path).unwrap().inventory().unwrap();
    assert_eq!(actual, [entries[3], entries[2], entries[1], entries[0]]);
    assert_eq!(
        witness
            .fetch([1; 16], 1, AnchorRange::new(2, 9).unwrap())
            .unwrap(),
        [entries[2]]
    );
    let bytes = fs::read(&path).unwrap();
    let line: serde_json::Value =
        serde_json::from_slice(bytes.split(|b| *b == b'\n').next().unwrap()).unwrap();
    assert_eq!(line.as_object().unwrap().len(), 5);
    assert_eq!(line["incarnation"], "02020202-0202-0202-0202-020202020202");
}

#[test]
fn duplicate_retry_preserves_bytes_and_conflicts_refuse() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("witness.jsonl");
    let witness = FileWitness::new(&path).unwrap();
    let entry = anchor(1, 1, 1, 9);
    witness.publish(&entry).unwrap();
    let bytes = fs::read(&path).unwrap();
    witness.publish(&entry).unwrap();
    assert_eq!(fs::read(&path).unwrap(), bytes);
    let error = witness.publish(&anchor(1, 1, 1, 8)).unwrap_err();
    assert_eq!(error, WitnessError::Conflict);
    assert_eq!(
        error.to_string(),
        "audit witness: conflicting anchor identity"
    );
    assert_eq!(fs::read(&path).unwrap(), bytes);
    fs::write(&path, [bytes.as_slice(), bytes.as_slice()].concat()).unwrap();
    assert_eq!(witness.inventory().unwrap(), [entry]);
    let conflicting = String::from_utf8(bytes.clone())
        .unwrap()
        .replace(&"09".repeat(32), &"08".repeat(32));
    fs::write(&path, [bytes.as_slice(), conflicting.as_bytes()].concat()).unwrap();
    assert_eq!(witness.inventory().unwrap_err(), WitnessError::Conflict);
}

#[test]
fn invalid_records_refuse_without_mutation_or_scope_bypass() {
    let valid = String::from_utf8(record()).unwrap();
    let malformed = [
        "\n".to_owned(),
        valid.trim_end().to_owned(),
        valid.replace("\"epoch\":1", "\"epoch\":1,\"epoch\":1"),
        valid.replace("\"epoch\":1", "\"extra-secret\":1,\"epoch\":1"),
        valid.replace("\"epoch\":1", "\"epoch\":\"1\""),
        valid.replace("\"epoch\":1", "\"epoch\":0"),
        valid.replace("\"epoch\":1", "\"epoch\":18446744073709551615"),
        valid.replace("\"epoch\":1,", ""),
        valid.replace("\"format_version\":1", "\"format_version\":2"),
        valid.replace(&"09".repeat(32), "bad-hash"),
        valid.replace(
            "01010101-0101-0101-0101-010101010101",
            "00000000-0000-0000-0000-000000000000",
        ),
        valid.replace("\"epoch\":1", "\"epoch\":[]"),
        format!("{}{{}}\n", valid.trim_end()),
        format!("{}\n", " ".repeat(1024)),
    ];
    for (index, input) in malformed.into_iter().enumerate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("witness.jsonl");
        fs::write(&path, input.as_bytes()).unwrap();
        let witness = FileWitness::new(&path).unwrap();
        let error = witness.inventory().unwrap_err();
        let (expected, message) = match index {
            1 => (
                WitnessError::TornRecord { line: 1 },
                "audit witness: torn record at line 1; operator reconciliation required",
            ),
            5 | 6 | 10 => (
                WitnessError::InvalidIdentity,
                "audit witness: invalid anchor identity",
            ),
            8 => (
                WitnessError::UnsupportedVersion,
                "audit witness: unsupported format version",
            ),
            13 => (
                WitnessError::Limit("record bytes"),
                "audit witness: record bytes limit exceeded",
            ),
            _ => (
                WitnessError::MalformedRecord { line: 1 },
                "audit witness: malformed record at line 1",
            ),
        };
        assert_eq!(error, expected, "fixture {index}");
        assert_eq!(error.to_string(), message);
        assert!(!error.to_string().contains("extra-secret"));
        assert_eq!(
            witness
                .fetch([2; 16], 2, AnchorRange::new(1, 1).unwrap())
                .unwrap_err(),
            expected
        );
        assert_eq!(witness.publish(&anchor(2, 2, 2, 1)).unwrap_err(), expected);
        assert_eq!(fs::read(&path).unwrap(), input.as_bytes());
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("witness.jsonl");
    fs::write(&path, [0xff, b'\n']).unwrap();
    assert_eq!(
        FileWitness::new(&path).unwrap().inventory().unwrap_err(),
        WitnessError::MalformedRecord { line: 1 }
    );
    fs::write(&path, valid.replace('\n', "\r\n")).unwrap();
    assert_eq!(
        FileWitness::new(&path).unwrap().inventory().unwrap(),
        [anchor(1, 1, 1, 9)]
    );
    fs::write(&path, []).unwrap();
    assert!(
        FileWitness::new(&path)
            .unwrap()
            .inventory()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn refuses_links_nonregular_files_and_held_lock() {
    use rustix::fs::{FlockOperation, flock};
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("witness.jsonl");
    fs::write(&path, []).unwrap();
    let link = dir.path().join("link");
    symlink(&path, &link).unwrap();
    assert!(FileWitness::new(&link).unwrap().inventory().is_err());
    let hard = dir.path().join("hard");
    fs::hard_link(&path, &hard).unwrap();
    assert_eq!(
        FileWitness::new(&path).unwrap().inventory().unwrap_err(),
        WitnessError::InvalidFile
    );
    fs::remove_file(hard).unwrap();
    assert!(FileWitness::new(dir.path()).unwrap().inventory().is_err());
    let file = fs::File::open(&path).unwrap();
    flock(&file, FlockOperation::NonBlockingLockExclusive).unwrap();
    let witness = FileWitness::new(&path).unwrap();
    assert_eq!(witness.inventory().unwrap_err(), WitnessError::Busy);
    assert_eq!(
        witness.publish(&anchor(1, 1, 1, 9)).unwrap_err(),
        WitnessError::Busy
    );
    drop(file);
    witness.publish(&anchor(1, 1, 1, 9)).unwrap();
}

#[test]
fn bounded_inventory_and_append_admission_preserve_evidence() {
    let valid = record();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("witness.jsonl");
    let witness = FileWitness::new(&path).unwrap();
    let mut padded = valid[..valid.len() - 1].to_vec();
    padded.resize(1023, b' ');
    padded.push(b'\n');
    fs::write(&path, &padded).unwrap();
    assert_eq!(witness.inventory().unwrap(), [anchor(1, 1, 1, 9)]);
    let mut too_long = padded.clone();
    too_long.insert(0, b' ');
    fs::write(&path, &too_long).unwrap();
    let error = witness.inventory().unwrap_err();
    assert_eq!(error, WitnessError::Limit("record bytes"));
    assert_eq!(
        error.to_string(),
        "audit witness: record bytes limit exceeded"
    );
    fs::write(&path, valid.repeat(65_536)).unwrap();
    assert_eq!(witness.inventory().unwrap().len(), 1);
    let before = fs::read(&path).unwrap();
    assert_eq!(
        witness.publish(&anchor(1, 1, 2, 8)).unwrap_err(),
        WitnessError::Limit("physical records")
    );
    witness.publish(&anchor(1, 1, 1, 9)).unwrap();
    assert_eq!(fs::read(&path).unwrap(), before);
    fs::write(&path, valid.repeat(65_537)).unwrap();
    assert_eq!(
        witness.inventory().unwrap_err(),
        WitnessError::Limit("physical records")
    );
    fs::write(&path, padded.repeat(16_384)).unwrap();
    assert_eq!(witness.inventory().unwrap().len(), 1);
    assert_eq!(
        witness.publish(&anchor(1, 1, 2, 8)).unwrap_err(),
        WitnessError::Limit("file bytes")
    );
    let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.set_len(16 * 1024 * 1024 + 1).unwrap();
    assert_eq!(
        witness.inventory().unwrap_err(),
        WitnessError::Limit("file bytes")
    );
}
