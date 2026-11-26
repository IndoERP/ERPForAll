// SPDX-License-Identifier: MIT OR Apache-2.0
//! Stream decoding (PDF 32000-1 §7.4): every filter and parameter is applied here or refused by
//! name.
//!
//! lopdf 0.45 reads `Filter` and `DecodeParms` only as direct objects, hands one parameter
//! dictionary to every filter of a chain, replaces a value it cannot use by a default and returns
//! the readable prefix of corrupt Flate, LZW and run-length data. This module resolves the filter
//! chain and the entry of each filter, decodes every layer itself under the caller's cap and
//! reverses the row predictors, so a stream either decodes to exactly its content or fails with
//! a named error. It never returns undecoded or shortened bytes.
use crate::error::{Result, malformed, owner, refused, unsupported};
use crate::graph::resolve;
use crate::limits::Context;
use crate::{Limit, PdfError, PdfErrorKind};
use lopdf::{Dictionary, Document, Object, Stream};
use std::borrow::Cow;

/// Row predictor of a Flate or LZW layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Predictor {
    /// Predictor 2: every sample is stored as its difference from the sample to its left.
    Tiff,
    /// Predictors 10 to 15: every row starts with the tag of the PNG filter used for it.
    Png,
}

/// Validated row geometry of a predictor.
#[derive(Debug, Clone, Copy)]
struct Rows {
    predictor: Predictor,
    /// Colour components per pixel.
    colors: usize,
    /// Bits per colour component: 1, 2, 4, 8 or 16.
    bits: usize,
    /// Samples per row: columns times colour components.
    samples: usize,
    /// Bytes per decoded row; a row is padded to a whole byte.
    row_bytes: usize,
}

/// A filter this crate decodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Codec {
    Flate,
    Lzw { early_change: bool },
    Ascii85,
    AsciiHex,
    RunLength,
}

/// One filter of a stream with the parameters that apply to it.
#[derive(Debug)]
struct Layer {
    codec: Codec,
    rows: Option<Rows>,
}

/// The size one decoded layer may reach, and the limit named when it is exceeded.
#[derive(Debug, Clone, Copy)]
struct Cap {
    max: usize,
    dimension: Limit,
}
impl Cap {
    fn next(self, len: usize, added: usize) -> Result<usize> {
        let next = len
            .checked_add(added)
            .ok_or_else(|| malformed("PDF decoded length overflow"))?;
        self.check(next)?;
        Ok(next)
    }
    fn append(self, out: &mut Vec<u8>, bytes: &[u8]) -> Result<()> {
        self.next(out.len(), bytes.len())?;
        out.extend_from_slice(bytes);
        Ok(())
    }
    fn room(self, len: usize, scratch: usize) -> Result<usize> {
        self.check(len)?;
        let remaining = self
            .max
            .checked_sub(len)
            .and_then(|n| n.checked_add(1))
            .ok_or_else(|| malformed("PDF decoded length overflow"))?;
        Ok(remaining.min(scratch))
    }
    fn check(self, len: usize) -> Result<()> {
        if len > self.max {
            return Err(PdfError::new(
                PdfErrorKind::Limit,
                format!("PDF {} limit exceeded", self.dimension.name()),
            ));
        }
        Ok(())
    }
}

/// Data that cannot be the output of the filter.
fn corrupt(filter: &str) -> PdfError {
    malformed(format!("PDF {filter} data is corrupt"))
}
/// Data that stops before the end marker its filter requires: a cut stream is never read as a
/// short one.
fn truncated(filter: &str) -> PdfError {
    malformed(format!(
        "PDF {filter} data ends before its end-of-data marker"
    ))
}

/// Decode the content of `s`: its filter chain in order, every layer at most `cap` bytes.
pub(crate) fn stream(
    doc: &Document,
    s: &Stream,
    l: &Context<'_>,
    cap: usize,
    dimension: Limit,
) -> Result<Vec<u8>> {
    if let Ok(file) = s.dict.get(b"F")
        && !matches!(resolve(doc, file, l)?, Object::Null)
    {
        // The bytes are in another file; the bytes between `stream` and `endstream` are not them.
        return Err(refused("external stream data"));
    }
    let cap = Cap {
        max: cap,
        dimension,
    };
    let names = filters(doc, s, l)?;
    let mut data = Cow::Borrowed(s.content.as_slice());
    for layer in layers(doc, s, &names, l)? {
        l.step()?;
        let decoded = match layer.codec {
            Codec::Flate => inflate(&data, cap, l)?,
            Codec::Lzw { early_change } => lzw(&data, early_change, cap, l)?,
            Codec::Ascii85 => ascii85(&data, cap, l)?,
            Codec::AsciiHex => ascii_hex(&data, cap, l)?,
            Codec::RunLength => run_length(&data, cap, l)?,
        };
        data = Cow::Owned(match layer.rows {
            Some(rows) => unpredict(decoded, rows, l)?,
            None => decoded,
        });
    }
    // A stream without filters reaches this unchecked: every decoded layer is capped above.
    cap.check(data.len())?;
    Ok(data.into_owned())
}

/// The filter chain of a stream in decoding order; names may sit behind references.
fn filters<'a>(doc: &'a Document, s: &'a Stream, l: &Context<'_>) -> Result<Vec<&'a [u8]>> {
    let wrong = || malformed("PDF stream Filter must be a name or an array of names");
    let Ok(filter) = s.dict.get(b"Filter") else {
        return Ok(Vec::new()); // A stream without Filter holds its bytes as they are.
    };
    match resolve(doc, filter, l)? {
        Object::Null => Ok(Vec::new()),
        Object::Name(name) => Ok(vec![name.as_slice()]),
        Object::Array(items) => items
            .iter()
            .map(|item| {
                l.step()?;
                match resolve(doc, item, l)? {
                    Object::Name(name) => Ok(name.as_slice()),
                    _ => Err(wrong()),
                }
            })
            .collect(),
        _ => Err(wrong()),
    }
}

fn layers(doc: &Document, s: &Stream, names: &[&[u8]], l: &Context<'_>) -> Result<Vec<Layer>> {
    let absent = Object::Null;
    let empty = Dictionary::new();
    let parameters = match s.dict.get(b"DecodeParms") {
        Ok(p) => resolve(doc, p, l)?,
        Err(_) => &absent, // No parameters: every filter takes its defaults.
    };
    let count = match parameters {
        Object::Null => names.len(),
        Object::Array(a) => a.len(),
        _ => 1,
    };
    if count != names.len() {
        return Err(malformed("PDF DecodeParms must have one entry per filter"));
    }
    let mut out = Vec::new();
    for (index, name) in names.iter().enumerate() {
        l.step()?;
        let entry = match parameters {
            Object::Null => &absent,
            Object::Array(a) => &a[index], // Count was checked against the filter list above.
            single => single,
        };
        let d = match resolve(doc, entry, l)? {
            Object::Null => &empty,
            Object::Dictionary(d) => d,
            _ => {
                return Err(malformed(
                    "PDF DecodeParms entry must be a dictionary or null",
                ));
            }
        };
        out.push(layer(doc, name, d, l)?);
    }
    Ok(out)
}

/// Parameter names are input: keep error diagnostics bounded, with escaped non-ASCII bytes.
fn parameter_refusal(key: &[u8]) -> PdfError {
    let mut name = String::new();
    for byte in key.iter().take(64) {
        name.extend(std::ascii::escape_default(*byte).map(char::from));
    }
    if key.len() > 64 {
        name.push_str("...");
    }
    refused(format!("stream decoding parameter {name}"))
}

fn layer(doc: &Document, filter: &[u8], d: &Dictionary, l: &Context<'_>) -> Result<Layer> {
    let lzw = match filter {
        b"FlateDecode" => false,
        b"LZWDecode" => true,
        b"ASCII85Decode" | b"ASCIIHexDecode" | b"RunLengthDecode" => {
            // These filters define no parameter.
            if let Some((key, _)) = d.iter().next() {
                return Err(parameter_refusal(key));
            }
            return Ok(Layer {
                codec: match filter {
                    b"ASCII85Decode" => Codec::Ascii85,
                    b"ASCIIHexDecode" => Codec::AsciiHex,
                    _ => Codec::RunLength,
                },
                rows: None,
            });
        }
        // Filters that stay out, each by its own name: the image codecs decode to samples, which
        // no caller of this module reads; Crypt belongs to encrypted documents, which are refused
        // as a whole; Brotli is outside the supported PDF 1.7 profile.
        b"DCTDecode" => return Err(refused("stream filter DCTDecode")),
        b"CCITTFaxDecode" => return Err(refused("stream filter CCITTFaxDecode")),
        b"JBIG2Decode" => return Err(refused("stream filter JBIG2Decode")),
        b"JPXDecode" => return Err(refused("stream filter JPXDecode")),
        b"Crypt" => return Err(unsupported("stream filter Crypt", owner::ENCRYPT)),
        b"BrotliDecode" => return Err(refused("stream filter BrotliDecode")),
        _ => return Err(malformed("PDF stream filter name is not defined")),
    };
    let mut predictor = 1;
    let mut colors = 1;
    let mut bits = 8;
    let mut columns = 1;
    let mut early_change = 1;
    for (key, value) in d.iter() {
        l.step()?;
        let name = match key.as_slice() {
            b"Predictor" => "Predictor",
            b"Colors" => "Colors",
            b"BitsPerComponent" => "BitsPerComponent",
            b"Columns" => "Columns",
            b"EarlyChange" if lzw => "EarlyChange",
            // A parameter this crate does not apply is named, never ignored.
            _ => return Err(parameter_refusal(key)),
        };
        let v = resolve(doc, value, l)?.as_i64().map_err(|e| {
            malformed(format!("PDF DecodeParms {name} must be an integer")).source(e)
        })?;
        let valid = match name {
            "Predictor" => v == 1 || v == 2 || (10..=15).contains(&v),
            "BitsPerComponent" => matches!(v, 1 | 2 | 4 | 8 | 16),
            "EarlyChange" => v == 0 || v == 1,
            _ => v >= 1,
        };
        if !valid {
            return Err(malformed(format!(
                "PDF DecodeParms {name} has the undefined value {v}"
            )));
        }
        match name {
            "Predictor" => predictor = v,
            "Colors" => colors = v,
            "BitsPerComponent" => bits = v,
            "Columns" => columns = v,
            _ => early_change = v,
        }
    }
    let rows = if predictor == 1 {
        None
    } else {
        // Conversion failure joins the checked arithmetic below and becomes a named error.
        let size = |v: i64| usize::try_from(v).ok();
        let (colors, bits) = (size(colors), size(bits));
        let samples = colors
            .zip(size(columns))
            .and_then(|(colors, columns)| colors.checked_mul(columns));
        let row_bytes = samples
            .zip(bits)
            .and_then(|(samples, bits)| samples.checked_mul(bits))
            .map(|row_bits| row_bits.div_ceil(8));
        let (Some(colors), Some(bits), Some(samples), Some(row_bytes)) =
            (colors, bits, samples, row_bytes)
        else {
            return Err(malformed("PDF DecodeParms row size overflow"));
        };
        Some(Rows {
            predictor: if predictor == 2 {
                Predictor::Tiff
            } else {
                Predictor::Png
            },
            colors,
            bits,
            samples,
            row_bytes,
        })
    };
    Ok(Layer {
        codec: if lzw {
            Codec::Lzw {
                early_change: early_change == 1,
            }
        } else {
            Codec::Flate
        },
        rows,
    })
}

/// Inflate zlib through its checked end marker without a recovery decoder.
fn inflate(input: &[u8], cap: Cap, l: &Context<'_>) -> Result<Vec<u8>> {
    let mut decoder = flate2::Decompress::new(true);
    let mut out = Vec::new();
    let mut block = [0u8; 4096]; // Scratch size, not an admission-policy limit.
    let mut rest = input;
    loop {
        l.step()?;
        let room = cap.room(out.len(), block.len())?;
        let (read, written) = (decoder.total_in(), decoder.total_out());
        let status = decoder
            .decompress(rest, &mut block[..room], flate2::FlushDecompress::None)
            .map_err(|e| corrupt("FlateDecode").source(e))?;
        let count = |after: u64, before: u64| {
            after
                .checked_sub(before)
                .and_then(|n| usize::try_from(n).ok())
                .ok_or_else(|| corrupt("FlateDecode"))
        };
        let consumed = count(decoder.total_in(), read)?;
        let produced = count(decoder.total_out(), written)?;
        rest = rest.get(consumed..).ok_or_else(|| corrupt("FlateDecode"))?;
        let bytes = block
            .get(..produced)
            .ok_or_else(|| corrupt("FlateDecode"))?;
        cap.append(&mut out, bytes)?;
        if matches!(status, flate2::Status::StreamEnd) {
            return Ok(out);
        }
        if consumed == 0 && produced == 0 {
            return Err(truncated("FlateDecode"));
        }
    }
}

/// LZW with most-significant-bit-first codes and the selected width switch.
fn lzw(input: &[u8], early_change: bool, cap: Cap, l: &Context<'_>) -> Result<Vec<u8>> {
    use weezl::{BitOrder, LzwStatus, decode::Decoder};
    let mut decoder = if early_change {
        Decoder::with_tiff_size_switch(BitOrder::Msb, 8)
    } else {
        Decoder::new(BitOrder::Msb, 8)
    };
    let mut out = Vec::new();
    let mut block = [0u8; 4096];
    let mut rest = input;
    loop {
        l.step()?;
        let room = cap.room(out.len(), block.len())?;
        let step = decoder.decode_bytes(rest, &mut block[..room]);
        rest = rest
            .get(step.consumed_in..)
            .ok_or_else(|| corrupt("LZWDecode"))?;
        let bytes = block
            .get(..step.consumed_out)
            .ok_or_else(|| corrupt("LZWDecode"))?;
        cap.append(&mut out, bytes)?;
        match step.status {
            Ok(LzwStatus::Done) => return Ok(out),
            Ok(LzwStatus::Ok) if step.consumed_in > 0 || step.consumed_out > 0 => {}
            Ok(LzwStatus::Ok | LzwStatus::NoProgress) => return Err(truncated("LZWDecode")),
            Err(e) => return Err(corrupt("LZWDecode").source(e)),
        }
    }
}

/// Run-length data: a length byte below 128 copies the next `length + 1` bytes, one above 128
/// repeats the next byte `257 - length` times, 128 ends the data.
fn run_length(input: &[u8], cap: Cap, l: &Context<'_>) -> Result<Vec<u8>> {
    let cut = || truncated("RunLengthDecode");
    let mut out = Vec::new();
    let mut rest = input;
    loop {
        l.step()?;
        let (&length, tail) = rest.split_first().ok_or_else(cut)?;
        match length {
            128 => return Ok(out),
            0..=127 => {
                let (literal, tail) = tail
                    .split_at_checked(usize::from(length) + 1)
                    .ok_or_else(cut)?;
                cap.append(&mut out, literal)?;
                rest = tail;
            }
            _ => {
                let (&byte, tail) = tail.split_first().ok_or_else(cut)?;
                let next = cap.next(out.len(), 257 - usize::from(length))?;
                out.resize(next, byte);
                rest = tail;
            }
        }
        cap.check(out.len())?;
    }
}

/// White space inside the text encodings (PDF 32000-1 table 1).
fn white(c: u8) -> bool {
    matches!(c, 0 | 9 | 10 | 12 | 13 | 32)
}

/// Hexadecimal digit pairs up to `>`; a last single digit is the high half of its byte.
fn ascii_hex(input: &[u8], cap: Cap, l: &Context<'_>) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut high: Option<u8> = None;
    for (index, &c) in input.iter().enumerate() {
        if index.is_multiple_of(4096) {
            l.step()?;
        }
        let digit = match c {
            b'>' => {
                if let Some(h) = high {
                    cap.append(&mut out, &[h << 4])?;
                }
                cap.check(out.len())?;
                return Ok(out);
            }
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            c if white(c) => continue,
            _ => return Err(corrupt("ASCIIHexDecode")),
        };
        match high.take() {
            Some(h) => {
                cap.append(&mut out, &[(h << 4) | digit])?;
                cap.check(out.len())?;
            }
            None => high = Some(digit),
        }
    }
    Err(truncated("ASCIIHexDecode"))
}

/// Base-85 groups up to `~>`: five digits `!` to `u` hold four bytes, `z` alone holds four zero
/// bytes, a last group of `n` digits holds `n - 1` bytes.
fn ascii85(input: &[u8], cap: Cap, l: &Context<'_>) -> Result<Vec<u8>> {
    let bad = || corrupt("ASCII85Decode");
    // The value of a group; one above the 32-bit range is not the encoding of any bytes.
    let word = |group: [u8; 5]| {
        group
            .iter()
            .try_fold(0u32, |v, d| v.checked_mul(85)?.checked_add(u32::from(*d)))
            .ok_or_else(bad)
    };
    let mut out = Vec::new();
    let mut group = [0u8; 5];
    let mut n = 0;
    let mut bytes = input.iter().copied().enumerate();
    while let Some((index, c)) = bytes.next() {
        if index.is_multiple_of(4096) {
            l.step()?;
        }
        match c {
            b'~' => {
                match bytes.next().map(|(_, c)| c) {
                    Some(b'>') => {}
                    None => return Err(truncated("ASCII85Decode")),
                    Some(_) => return Err(bad()),
                }
                if n == 1 {
                    return Err(bad()); // One digit cannot hold a byte.
                }
                if n > 1 {
                    // The encoder cut the digits of the zero-padded bytes: the highest digit
                    // fills them up again.
                    group[n..].fill(84);
                    cap.append(&mut out, &word(group)?.to_be_bytes()[..n - 1])?;
                }
                cap.check(out.len())?;
                return Ok(out);
            }
            b'z' if n == 0 => cap.append(&mut out, &[0; 4])?,
            b'!'..=b'u' => {
                group[n] = c - b'!';
                n += 1;
                if n == 5 {
                    cap.append(&mut out, &word(group)?.to_be_bytes())?;
                    n = 0;
                }
            }
            c if white(c) => continue,
            _ => return Err(bad()),
        }
        cap.check(out.len())?;
    }
    Err(truncated("ASCII85Decode"))
}

/// Reverse a row predictor. Data that ends inside a row is refused, never cut short.
fn unpredict(data: Vec<u8>, rows: Rows, l: &Context<'_>) -> Result<Vec<u8>> {
    let partial = || malformed("PDF predictor data ends inside a row");
    match rows.predictor {
        Predictor::Tiff => {
            let mut data = data;
            if !data.len().is_multiple_of(rows.row_bytes) {
                return Err(partial());
            }
            for row in data.chunks_exact_mut(rows.row_bytes) {
                l.step()?;
                tiff_row(row, rows);
            }
            Ok(data)
        }
        Predictor::Png => {
            let encoded = rows
                .row_bytes
                .checked_add(1)
                .ok_or_else(|| malformed("PDF DecodeParms row size overflow"))?;
            if !data.len().is_multiple_of(encoded) {
                return Err(partial());
            }
            // Bytes per complete pixel, at least one: the distance to the "left" byte.
            let step = (rows.colors * rows.bits).div_ceil(8);
            let mut out = Vec::with_capacity(data.len() / encoded * rows.row_bytes);
            for row in data.chunks_exact(encoded) {
                l.step()?;
                let start = out.len();
                let tag = row[0];
                if tag > 4 {
                    return Err(malformed(format!(
                        "PDF predictor row has the undefined filter type {tag}"
                    )));
                }
                for (i, raw) in row[1..].iter().enumerate() {
                    let left = if i >= step { out[start + i - step] } else { 0 };
                    let (up, corner) = if start >= rows.row_bytes {
                        let above = start - rows.row_bytes + i;
                        (out[above], if i >= step { out[above - step] } else { 0 })
                    } else {
                        (0, 0)
                    };
                    let predicted = match tag {
                        0 => 0,
                        1 => left,
                        2 => up,
                        3 => ((u16::from(left) + u16::from(up)) / 2) as u8,
                        _ => paeth(left, up, corner),
                    };
                    out.push(raw.wrapping_add(predicted));
                }
            }
            Ok(out)
        }
    }
}

/// The PNG Paeth choice: the neighbour closest to `left + up - corner`, ties in that order.
fn paeth(left: u8, up: u8, corner: u8) -> u8 {
    let estimate = i16::from(left) + i16::from(up) - i16::from(corner);
    let distance = |v: u8| (estimate - i16::from(v)).abs();
    let (dl, du, dc) = (distance(left), distance(up), distance(corner));
    if dl <= du && dl <= dc {
        left
    } else if du <= dc {
        up
    } else {
        corner
    }
}

/// Add every sample to the sample of the same colour component to its left, modulo its width.
/// Padding bits after the last sample of a row are left as they are.
fn tiff_row(row: &mut [u8], rows: Rows) {
    for k in rows.colors..rows.samples {
        let v = sample(row, k, rows.bits).wrapping_add(sample(row, k - rows.colors, rows.bits));
        set_sample(row, k, rows.bits, v);
    }
}

fn sample(row: &[u8], k: usize, bits: usize) -> u16 {
    if bits == 16 {
        return u16::from_be_bytes([row[2 * k], row[2 * k + 1]]);
    }
    let offset = k * bits;
    let shift = 8 - bits - offset % 8;
    u16::from(row[offset / 8] >> shift) & ((1 << bits) - 1)
}

fn set_sample(row: &mut [u8], k: usize, bits: usize, v: u16) {
    if bits == 16 {
        row[2 * k..2 * k + 2].copy_from_slice(&v.to_be_bytes());
        return;
    }
    let offset = k * bits;
    let shift = 8 - bits - offset % 8;
    let mask = ((1u16 << bits) - 1) as u8;
    let byte = &mut row[offset / 8];
    *byte = (*byte & !(mask << shift)) | (((v as u8) & mask) << shift);
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]
    use super::*;
    use crate::PdfLimits;

    fn rows(predictor: Predictor, colors: usize, bits: usize, columns: usize) -> Rows {
        Rows {
            predictor,
            colors,
            bits,
            samples: colors * columns,
            row_bytes: (colors * bits * columns).div_ceil(8),
        }
    }
    fn decoded(data: &[u8], rows: Rows) -> Vec<u8> {
        let limits = PdfLimits::default();
        unpredict(data.to_vec(), rows, &Context::new(&limits)).unwrap()
    }

    /// SPEC-PDF-050 (partial: the five PNG row filters on values worked out by hand)
    #[test]
    fn png_rows_are_reversed_per_filter_type() {
        let encoded = [
            1, 10, 1, 1, 1, // Sub: each byte adds the byte to its left.
            2, 1, 1, 1, 1, // Up: each byte adds the byte above.
            3, 5, 0, 0, 0, // Average: adds the floor of the mean of left and above.
            4, 1, 1, 1, 1, // Paeth: adds the nearest of left, above and upper left.
            0, 7, 8, 9, 10, // None.
        ];
        assert_eq!(
            decoded(&encoded, rows(Predictor::Png, 1, 8, 4)),
            [
                10, 11, 12, 13, //
                11, 12, 13, 14, //
                10, 11, 12, 13, //
                11, 12, 13, 14, //
                7, 8, 9, 10,
            ]
        );
        // Two bytes per pixel: "left" is the same byte of the pixel before.
        assert_eq!(
            decoded(&[1, 1, 2, 3, 4], rows(Predictor::Png, 2, 8, 2)),
            [1, 2, 4, 6]
        );
    }

    /// SPEC-PDF-050 (partial: horizontal differencing for every sample width, by hand)
    #[test]
    fn tiff_rows_add_the_sample_to_the_left() {
        let tiff = |colors, bits, columns| rows(Predictor::Tiff, colors, bits, columns);
        assert_eq!(
            decoded(&[1, 2, 1, 1, 1, 1], tiff(2, 8, 3)),
            [1, 2, 2, 3, 3, 4]
        );
        assert_eq!(decoded(&[250, 10], tiff(1, 8, 2)), [250, 4]);
        assert_eq!(decoded(&[0, 255, 0, 1], tiff(1, 16, 2)), [0, 255, 1, 0]);
        // Three 4-bit samples 1, 1, 15 and four padding bits that stay as they are.
        assert_eq!(decoded(&[0x11, 0xFA], tiff(1, 4, 3)), [0x12, 0x1A]);
        assert_eq!(decoded(&[0b0101_1110], tiff(1, 2, 4)), [0b0110_0111]);
        assert_eq!(decoded(&[0b1100_0001], tiff(1, 1, 8)), [0b1000_0001]);
    }

    fn check_error(
        doc: &Document,
        s: &Stream,
        limits: &PdfLimits,
        kind: PdfErrorKind,
        message: &str,
    ) {
        let error = stream(doc, s, &Context::new(limits), 64, Limit::DecodedBytes).unwrap_err();
        assert_eq!(error.kind(), kind);
        assert_eq!(error.to_string(), message);
    }

    /// SPEC-PDF-051 (partial: parameter shape, indirect filter resolution, cycles and named exclusions)
    #[test]
    fn references_shapes_and_refusals_are_explicit() {
        use lopdf::dictionary;
        let limits = PdfLimits::default();
        let mut doc = Document::with_version("1.7");
        let name = doc.add_object(Object::Name(b"ASCIIHexDecode".to_vec()));
        let filters = doc.add_object(vec![Object::Reference(name)]);
        let params = doc.add_object(vec![Object::Null]);
        let encoded = Stream::new(
            dictionary! {"Filter"=>filters,"DecodeParms"=>params},
            b"4142>".to_vec(),
        );
        assert_eq!(
            stream(
                &doc,
                &encoded,
                &Context::new(&limits),
                2,
                Limit::DecodedBytes
            )
            .unwrap(),
            b"AB"
        );
        for filter in [Object::Integer(1), Object::Array(vec![Object::Null])] {
            let s = Stream::new(dictionary! {"Filter"=>filter}, vec![]);
            check_error(
                &doc,
                &s,
                &limits,
                PdfErrorKind::Malformed,
                "PDF stream Filter must be a name or an array of names",
            );
        }
        for name in [
            "DCTDecode",
            "CCITTFaxDecode",
            "JBIG2Decode",
            "JPXDecode",
            "BrotliDecode",
        ] {
            let s = Stream::new(dictionary! {"Filter"=>name}, vec![]);
            check_error(
                &doc,
                &s,
                &limits,
                PdfErrorKind::Unsupported,
                &format!("PDF feature stream filter {name} is refused by design"),
            );
        }
        check_error(
            &doc,
            &Stream::new(dictionary! {"Filter"=>"Crypt"}, vec![]),
            &limits,
            PdfErrorKind::Unsupported,
            "PDF feature stream filter Crypt is unsupported (a later release)",
        );
        check_error(
            &doc,
            &Stream::new(dictionary! {"Filter"=>"Undefined"}, vec![]),
            &limits,
            PdfErrorKind::Malformed,
            "PDF stream filter name is not defined",
        );
        check_error(
            &doc,
            &Stream::new(
                dictionary! {"F"=>Object::string_literal("external")},
                vec![],
            ),
            &limits,
            PdfErrorKind::Unsupported,
            "PDF feature external stream data is refused by design",
        );
        let cycle = doc.new_object_id();
        doc.objects.insert(cycle, Object::Reference(cycle));
        check_error(
            &doc,
            &Stream::new(dictionary! {"Filter"=>cycle}, vec![]),
            &limits,
            PdfErrorKind::Malformed,
            "PDF cyclic object reference",
        );
        check_error(
            &doc,
            &Stream::new(dictionary! {"Filter"=>Object::Reference((999,0))}, vec![]),
            &limits,
            PdfErrorKind::Malformed,
            "PDF missing object 999 0",
        );
        check_error(
            &doc,
            &Stream::new(
                dictionary! {"Filter"=>"FlateDecode", "DecodeParms"=>cycle},
                vec![],
            ),
            &limits,
            PdfErrorKind::Malformed,
            "PDF cyclic object reference",
        );
    }

    /// SPEC-PDF-053 (partial: bounded filter work and reference depth; no output-sized allocation for huge rows)
    #[test]
    fn filter_traversal_and_geometry_obey_admission() {
        use lopdf::dictionary;
        let mut doc = Document::with_version("1.7");
        let limits = PdfLimits::default();
        let name = doc.add_object(Object::Name(b"ASCIIHexDecode".to_vec()));
        let first = doc.add_object(Object::Reference(name));
        let second = doc.add_object(Object::Reference(first));
        let deep = Stream::new(dictionary! {"Filter"=>second}, b">".to_vec());
        check_error(
            &doc,
            &deep,
            &limits.clone().with(Limit::Depth, 2).unwrap(),
            PdfErrorKind::Limit,
            "PDF limit depth exceeded: 3 > 2",
        );
        let work = Stream::new(
            dictionary! {"Filter"=>vec![Object::Name(b"ASCIIHexDecode".to_vec()); 2]},
            vec![],
        );
        check_error(
            &doc,
            &work,
            &limits.clone().with(Limit::WorkUnits, 1).unwrap(),
            PdfErrorKind::Limit,
            "PDF limit work_units exceeded: 2 > 1",
        );
        let s = Stream::new(
            dictionary! {"Filter"=>"FlateDecode", "DecodeParms"=>dictionary!{
            "Predictor"=>2,"Columns"=>i64::MAX,"Colors"=>2,"BitsPerComponent"=>16}},
            vec![],
        );
        check_error(
            &doc,
            &s,
            &limits,
            PdfErrorKind::Malformed,
            "PDF DecodeParms row size overflow",
        );
    }

    /// SPEC-PDF-052 (partial: malformed LZW code and truncated later chain layer)
    #[test]
    fn bad_codes_and_later_empty_layers_never_succeed() {
        use lopdf::dictionary;
        let doc = Document::with_version("1.7");
        let limits = PdfLimits::default();
        check_error(
            &doc,
            &Stream::new(dictionary! {"Filter"=>"LZWDecode"}, vec![0x80, 0x7f, 0xc0]),
            &limits,
            PdfErrorKind::Malformed,
            "PDF LZWDecode data is corrupt",
        );
        check_error(
            &doc,
            &Stream::new(dictionary! {"Filter"=>"ASCII85Decode"}, b"~".to_vec()),
            &limits,
            PdfErrorKind::Malformed,
            "PDF ASCII85Decode data ends before its end-of-data marker",
        );
        let s = Stream::new(
            dictionary! {"Filter"=>vec![Object::Name(b"ASCIIHexDecode".to_vec()),Object::Name(b"LZWDecode".to_vec())]},
            b">".to_vec(),
        );
        check_error(
            &doc,
            &s,
            &limits,
            PdfErrorKind::Malformed,
            "PDF LZWDecode data ends before its end-of-data marker",
        );
    }
}
