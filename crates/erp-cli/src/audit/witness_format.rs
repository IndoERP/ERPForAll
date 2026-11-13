// SPDX-License-Identifier: MIT OR Apache-2.0
//! Closed wire representation, kept out of the core witness contract.
use erp_orm::audit::seal::{Anchor, SealIdentity, WitnessError};
use serde::{Deserialize, Serialize};

pub(super) const MAX_RECORD_BYTES: usize = 1024;
pub(super) const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
pub(super) const MAX_RECORDS: u32 = 65_536;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Record<'a> {
    incarnation: &'a str,
    epoch: u64,
    batch_no: u64,
    seal_hash: &'a str,
    format_version: u16,
}
#[derive(Serialize)]
struct Output {
    incarnation: String,
    epoch: u64,
    batch_no: u64,
    seal_hash: String,
    format_version: u16,
}
fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(DIGITS[usize::from(byte >> 4)]));
        output.push(char::from(DIGITS[usize::from(byte & 15)]));
    }
    output
}
fn nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}
fn decode_hex<const N: usize>(bytes: &[u8]) -> Option<[u8; N]> {
    if bytes.len() != N * 2 {
        return None;
    }
    let mut result = [0; N];
    for (out, pair) in result.iter_mut().zip(bytes.as_chunks::<2>().0) {
        *out = (nibble(pair[0])? << 4) | nibble(pair[1])?;
    }
    Some(result)
}
fn uuid(text: &str) -> Option<[u8; 16]> {
    let bytes = text.as_bytes();
    if bytes.len() != 36 {
        return None;
    }
    let mut compact = [0; 32];
    let mut index = 0;
    for (position, byte) in bytes.iter().enumerate() {
        if matches!(position, 8 | 13 | 18 | 23) {
            if *byte != b'-' {
                return None;
            }
        } else {
            compact[index] = *byte;
            index += 1;
        }
    }
    decode_hex(&compact)
}
pub(super) fn decode(line: &[u8], number: u32) -> Result<Anchor, WitnessError> {
    let malformed = || WitnessError::MalformedRecord { line: number };
    let record: Record<'_> = serde_json::from_slice(line).map_err(|_| malformed())?;
    let incarnation = uuid(record.incarnation).ok_or_else(malformed)?;
    let hash = decode_hex(record.seal_hash.as_bytes()).ok_or_else(malformed)?;
    let identity = SealIdentity::new(incarnation, record.epoch, record.batch_no)
        .map_err(|_| WitnessError::InvalidIdentity)?;
    Anchor::new(identity, hash, record.format_version)
}
pub(super) fn encode(anchor: &Anchor) -> Result<Vec<u8>, WitnessError> {
    let identity = anchor.identity();
    let compact = hex(identity.incarnation());
    let incarnation = format!(
        "{}-{}-{}-{}-{}",
        &compact[..8],
        &compact[8..12],
        &compact[12..16],
        &compact[16..20],
        &compact[20..]
    );
    let mut bytes = serde_json::to_vec(&Output {
        incarnation,
        epoch: identity.epoch(),
        batch_no: identity.batch_no(),
        seal_hash: hex(anchor.seal_hash()),
        format_version: anchor.format_version(),
    })
    .map_err(|_| WitnessError::Limit("encoded record"))?;
    bytes.push(b'\n');
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(WitnessError::Limit("record bytes"));
    }
    Ok(bytes)
}
