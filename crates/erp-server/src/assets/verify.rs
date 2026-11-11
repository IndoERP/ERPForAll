//! Verified compressed forms; bounded output and strict RFC Brotli window admission.
use super::{PublicationError, PublicationErrorKind, refused};
use std::io::Read;
fn corrupt(detail: &str) -> PublicationError {
    refused(PublicationErrorKind::Representation, detail)
}
pub(super) fn encoded(gzip: &[u8], brotli: &[u8], identity: &[u8]) -> Result<(), PublicationError> {
    if gzip.len() > 17 * 1024 * 1024 || brotli.len() > 17 * 1024 * 1024 {
        return Err(corrupt("encoded asset exceeds 17 MiB"));
    }
    verify_gzip(gzip, identity)?;
    verify_brotli(brotli, identity)
}
fn decoded_window(
    identity: &[u8],
    offset: usize,
    window: &[u8],
) -> Result<usize, PublicationError> {
    let end = offset
        .checked_add(window.len())
        .ok_or_else(|| corrupt("blob compressed bytes mismatch"))?;
    if identity.get(offset..end) != Some(window) {
        return Err(corrupt("blob compressed bytes mismatch"));
    }
    Ok(end)
}
fn verify_gzip(encoded: &[u8], identity: &[u8]) -> Result<(), PublicationError> {
    // BufRead preserves the exact unconsumed suffix of this one member, unlike read::GzDecoder.
    let mut decoder = flate2::bufread::GzDecoder::new(encoded);
    let mut output = [0u8; 8192];
    let mut offset = 0usize;
    loop {
        let produced = decoder
            .read(&mut output)
            .map_err(|_| corrupt("invalid gzip representation"))?;
        if produced == 0 {
            break;
        }
        let window = output
            .get(..produced)
            .ok_or_else(|| corrupt("invalid gzip output"))?;
        offset = decoded_window(identity, offset, window)?;
    }
    if offset != identity.len() {
        return Err(corrupt("blob compressed bytes mismatch"));
    }
    if !decoder.get_ref().is_empty() {
        return Err(corrupt("gzip trailing data"));
    }
    Ok(())
}
fn verify_brotli(encoded: &[u8], identity: &[u8]) -> Result<(), PublicationError> {
    // RFC-sized windows only: the large-window extension is not admitted (up to 1 GiB state).
    // The strict decoder's maximum RFC window is 16 MiB, bounded independently of output length.
    let mut decoder = brotli::BrotliState::new_strict(
        brotli::HeapAlloc::<u8>::default(),
        brotli::HeapAlloc::<u32>::default(),
        brotli::HeapAlloc::<brotli::HuffmanCode>::default(),
    );
    let mut available_in = encoded.len();
    let mut input_offset = 0usize;
    let mut total_out = 0usize;
    let mut compared = 0usize;
    let mut output = [0u8; 8192];
    loop {
        let before_input = input_offset;
        let mut available_out = output.len();
        let mut output_offset = 0usize;
        let result = brotli::BrotliDecompressStream(
            &mut available_in,
            &mut input_offset,
            encoded,
            &mut available_out,
            &mut output_offset,
            &mut output,
            &mut total_out,
            &mut decoder,
        );
        let window = output
            .get(..output_offset)
            .ok_or_else(|| corrupt("invalid Brotli output"))?;
        compared = decoded_window(identity, compared, window)?;
        match result {
            brotli::BrotliResult::ResultSuccess => {
                if compared != identity.len() {
                    return Err(corrupt("blob compressed bytes mismatch"));
                }
                if input_offset != encoded.len() || available_in != 0 {
                    return Err(corrupt("Brotli trailing data"));
                }
                return Ok(());
            }
            brotli::BrotliResult::NeedsMoreOutput => {
                if before_input == input_offset && output_offset == 0 {
                    return Err(corrupt("Brotli decoder made no progress"));
                }
            }
            brotli::BrotliResult::NeedsMoreInput | brotli::BrotliResult::ResultFailure => {
                return Err(corrupt("invalid Brotli representation"));
            }
        }
    }
}
