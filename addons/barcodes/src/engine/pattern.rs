// SPDX-License-Identifier: MIT OR Apache-2.0
use super::{BarcodeError, Result};
use regex::{Regex, RegexBuilder};

#[derive(Debug)]
pub(super) struct Pattern {
    regex: Regex,
    decimals: Option<usize>,
}
impl Pattern {
    pub(super) fn compile(source: &str, compiled_budget: usize) -> Result<Self> {
        if source.len() > 1024 {
            return Err(BarcodeError::new("limit", "Pattern exceeds 1024 bytes."));
        }
        let mut chars = source.chars();
        let mut translated = String::new();
        let mut decimals = None;
        while let Some(c) = chars.next() {
            match c {
                '\\' => {
                    translated.push(c);
                    translated.push(chars.next().ok_or_else(|| {
                        BarcodeError::new(
                            "pattern",
                            "Pattern is not supported by the bounded regex engine.",
                        )
                    })?);
                }
                '{' => {
                    if decimals.is_some() {
                        return Err(BarcodeError::new(
                            "pattern",
                            "A pattern may contain only one numeric token.",
                        ));
                    }
                    let mut token = String::new();
                    let mut closed = false;
                    for c in chars.by_ref() {
                        if c == '}' {
                            closed = true;
                            break;
                        }
                        token.push(c);
                    }
                    let whole = token.bytes().take_while(|b| *b == b'N').count();
                    if !closed
                        || token.is_empty()
                        || !token.as_bytes()[whole..].iter().all(|b| *b == b'D')
                    {
                        return Err(BarcodeError::new(
                            "pattern",
                            "Numeric pattern must contain N digits followed by D digits.",
                        ));
                    }
                    if token.len() > 18 {
                        return Err(BarcodeError::new(
                            "pattern",
                            "Numeric pattern exceeds 18 digits.",
                        ));
                    }
                    decimals = Some(token.len() - whole);
                    translated.push_str(&format!("(?P<erp_numeric>[0-9]{{{}}})", token.len()));
                }
                '}' => {
                    return Err(BarcodeError::new(
                        "pattern",
                        "Numeric pattern must contain N digits followed by D digits.",
                    ));
                }
                _ => translated.push(c), // Other syntax is validated by RegexBuilder, never discarded.
            }
        }
        let regex = RegexBuilder::new(&format!(r"\A(?:{translated})"))
            .nest_limit(32)
            .size_limit(compiled_budget)
            .dfa_size_limit(16 * 1024)
            .build()
            .map_err(|error| {
                BarcodeError::new(
                    "pattern",
                    "Pattern is not supported by the bounded regex engine.",
                )
                .with_source(error)
            })?;
        // The reserved capture must be ours, not injected by a custom expression.
        let numeric_count = regex
            .capture_names()
            .flatten()
            .filter(|name| *name == "erp_numeric")
            .count();
        if numeric_count != usize::from(decimals.is_some()) {
            return Err(BarcodeError::new(
                "pattern",
                "Reserved numeric capture name.",
            ));
        }
        Ok(Self { regex, decimals })
    }
    pub(super) fn apply(&self, code: &str) -> Result<Option<(String, String)>> {
        let Some(captures) = self.regex.captures(code) else {
            return Ok(None);
        };
        let Some(scale) = self.decimals else {
            return Ok(Some((code.into(), "0".into())));
        };
        // An optional branch may match without consuming the numeric token: no numeric value then.
        let Some(numeric) = captures.name("erp_numeric") else {
            return Ok(Some((code.into(), "0".into())));
        };
        let digits = numeric.as_str();
        let coefficient: u64 = digits
            .parse()
            .map_err(|_| BarcodeError::new("pattern", "Invalid numeric content."))?;
        let divisor = 10u64
            .checked_pow(scale as u32)
            .ok_or_else(|| BarcodeError::new("limit", "Numeric scale overflow."))?;
        let value = if scale == 0 {
            coefficient.to_string()
        } else {
            let decimal = format!(
                "{}.{:0width$}",
                coefficient / divisor,
                coefficient % divisor,
                width = scale
            );
            decimal
                .trim_end_matches('0')
                .trim_end_matches('.')
                .to_owned()
        };
        let mut base = code.to_owned();
        base.replace_range(numeric.range(), &"0".repeat(digits.len()));
        Ok(Some((base, value)))
    }
}
