//! The removal report.
//!
//! Every removal the cleaner makes is counted in an entry keyed by `(kind, name, reason)`, so the
//! report grows with the number of distinct removed features, not with their occurrences. An entry
//! keeps the count and the first few sample locations. It never holds a body, a URL or a removed
//! attribute or style value: `name` is the element, attribute or CSS property name only.
//!
//! A sample location is the one-based count of nodes the cleaner had visited, in document order and
//! not counting nodes inside removed subtrees, when it made the removal (for an attribute, URL or
//! CSS removal, the owning element's count). The parsed tree keeps no source offsets, so this is a
//! visit ordinal, not a byte offset. The generated fragment root is normally skipped; when it
//! carries retained attributes it is visited first (ordinal 1), before its children.
//!
//! Names are limited to ASCII letters, digits and `-_:.` and 64 bytes; anything else is recorded as
//! `(invalid)`, so a hostile name cannot smuggle text into the report or fill it alone.

use std::collections::BTreeMap;

use crate::HtmlSanitizeError;
use crate::budget::HtmlBudget;

/// What was removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum RemovalKind {
    /// An element.
    Tag,
    /// An attribute.
    Attribute,
    /// A CSS declaration inside a `style` attribute.
    Css,
    /// A URL in a link attribute.
    Url,
    /// A comment.
    Comment,
}

/// Why it was removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum RemovalReason {
    /// An element removed with its whole subtree (kill list or form control).
    ForbiddenElement,
    /// An element dropped while its children were kept (not allow-listed, `form`, or carrying `encoding`).
    UnwrappedElement,
    /// An otherwise allowed attribute discarded with its unwrapped element.
    RemovedWithElement,
    /// An attribute that is not on the allow-list.
    ForbiddenAttribute,
    /// An event-handler attribute (`on*`).
    EventHandler,
    /// A `class` attribute removed because the policy strips classes.
    StrippedClass,
    /// A `style` attribute removed because the policy strips styles.
    StrippedStyle,
    /// A link attribute whose URL scheme is not allowed; the value was blanked.
    DisallowedUrl,
    /// A CSS declaration that is dangerous in every mode.
    DangerousCss,
    /// A CSS property outside the whitelist (under `sanitize_style`).
    CssNotWhitelisted,
    /// A CSS declaration that is not `name: value`.
    MalformedCss,
    /// A CSS declaration overridden by a later one for the same property (whitelist mode).
    DuplicateCss,
    /// A conditional comment or CDATA remnant.
    ConditionalComment,
    /// A comment that would not read back as the same comment.
    UnstableComment,
}

impl RemovalKind {
    /// Stable machine-readable name for diagnostic transports.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tag => "tag",
            Self::Attribute => "attribute",
            Self::Css => "css",
            Self::Url => "url",
            Self::Comment => "comment",
        }
    }
}
impl RemovalReason {
    /// Stable machine-readable reason for diagnostic transports.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ForbiddenElement => "forbidden_element",
            Self::UnwrappedElement => "unwrapped_element",
            Self::RemovedWithElement => "removed_with_element",
            Self::ForbiddenAttribute => "forbidden_attribute",
            Self::EventHandler => "event_handler",
            Self::StrippedClass => "stripped_class",
            Self::StrippedStyle => "stripped_style",
            Self::DisallowedUrl => "disallowed_url",
            Self::DangerousCss => "dangerous_css",
            Self::CssNotWhitelisted => "css_not_whitelisted",
            Self::MalformedCss => "malformed_css",
            Self::DuplicateCss => "duplicate_css",
            Self::ConditionalComment => "conditional_comment",
            Self::UnstableComment => "unstable_comment",
        }
    }
}

/// One group of removals with the same kind, name and reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemovalEntry {
    kind: RemovalKind,
    name: String,
    reason: RemovalReason,
    count: u64,
    sample_locations: Vec<u64>,
}

impl RemovalEntry {
    /// What was removed.
    #[must_use]
    pub fn kind(&self) -> RemovalKind {
        self.kind
    }

    /// The element, attribute or CSS property name (lower case); `(invalid)` for a tag, attribute
    /// or CSS declaration without a usable name; a fixed word for comments.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Why it was removed.
    #[must_use]
    pub fn reason(&self) -> RemovalReason {
        self.reason
    }

    /// How many removals this entry stands for.
    #[must_use]
    pub fn count(&self) -> u64 {
        self.count
    }

    /// Node ordinals of the first removals, in encounter order, at most the call's
    /// `report_samples` limit.
    #[must_use]
    pub fn sample_locations(&self) -> &[u64] {
        &self.sample_locations
    }
}

/// The aggregated removals of one sanitising call, in order of first encounter.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemovalReport {
    entries: Vec<RemovalEntry>,
}

/// Fixed accounting cost of one entry, on top of its name bytes.
const ENTRY_BYTES: u64 = 64;
/// Accounting cost of one sample location.
const SAMPLE_BYTES: u64 = 8;
/// Estimated size of one index map node; charged at insert and counted in `retained_bytes`.
const INDEX_NODE_BYTES: usize = 48;

impl RemovalReport {
    /// The entries, in order of first encounter.
    #[must_use]
    pub fn entries(&self) -> &[RemovalEntry] {
        &self.entries
    }

    /// Whether nothing was removed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The number of removals across all entries.
    #[must_use]
    pub fn total_count(&self) -> u64 {
        self.entries
            .iter()
            .fold(0u64, |sum, e| sum.saturating_add(e.count))
    }

    /// Bytes the report really holds, from the allocated capacities (used for the retained-byte
    /// account of the reparse pass).
    pub fn retained_bytes(&self) -> usize {
        self.entries.iter().fold(
            self.entries.capacity() * size_of::<RemovalEntry>(),
            |sum, e| {
                sum.saturating_add(e.name.capacity())
                    .saturating_add(e.sample_locations.capacity() * size_of::<u64>())
                    // The index key copy and a map node.
                    .saturating_add(e.name.len())
                    .saturating_add(INDEX_NODE_BYTES)
            },
        )
    }

    /// The size charged against the report byte limit: a fixed cost per entry plus its name and
    /// eight bytes per sample location.
    #[must_use]
    pub fn byte_size(&self) -> u64 {
        self.entries.iter().fold(0u64, |sum, e| {
            sum.saturating_add(entry_cost(&e.name))
                .saturating_add(SAMPLE_BYTES.saturating_mul(to_u64(e.sample_locations.len())))
        })
    }
}

/// `name` if it is a plain identifier of at most 64 bytes, else `(invalid)`.
fn usable_name(name: &str) -> &str {
    let plain = !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b':' | b'.'));
    if plain { name } else { "(invalid)" }
}

fn to_u64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

fn entry_cost(name: &str) -> u64 {
    ENTRY_BYTES.saturating_add(to_u64(name.len()))
}

/// Collects removals during the first pass. Every new entry, name copy and sample is checked
/// against the report limit and charged to the working-byte budget before it is allocated.
pub(crate) struct ReportBuilder {
    entries: Vec<RemovalEntry>,
    index: BTreeMap<(RemovalKind, RemovalReason), BTreeMap<Box<str>, usize>>,
    bytes: u64,
    limit_bytes: u64,
    samples: usize,
}

impl ReportBuilder {
    pub(crate) fn new(limit_bytes: u64, samples: u64) -> ReportBuilder {
        ReportBuilder {
            entries: Vec::new(),
            index: BTreeMap::new(),
            bytes: 0,
            limit_bytes,
            samples: usize::try_from(samples).unwrap_or(usize::MAX),
        }
    }

    fn admit(&mut self, cost: u64) -> Result<(), HtmlSanitizeError> {
        let total = self.bytes.saturating_add(cost);
        if total > self.limit_bytes {
            return Err(HtmlSanitizeError::ReportTooLarge {
                limit: self.limit_bytes,
            });
        }
        self.bytes = total;
        Ok(())
    }

    /// Count one removal of `name` at node ordinal `location`.
    pub(crate) fn record(
        &mut self,
        kind: RemovalKind,
        name: &str,
        reason: RemovalReason,
        location: u64,
        budget: &HtmlBudget,
    ) -> Result<(), HtmlSanitizeError> {
        budget.charge_work(1)?;
        let name = usable_name(name);
        let found = self
            .index
            .get(&(kind, reason))
            .and_then(|names| names.get(name))
            .copied();
        let slot = match found {
            Some(slot) => slot,
            None => {
                self.admit(entry_cost(name))?;
                // The name is held twice (entry and index key) plus one index node.
                budget.charge_bytes(
                    name.len()
                        .saturating_mul(2)
                        .saturating_add(INDEX_NODE_BYTES),
                )?;
                budget.reserve(&mut self.entries, 1)?;
                let slot = self.entries.len();
                self.entries.push(RemovalEntry {
                    kind,
                    name: name.to_owned(),
                    reason,
                    count: 0,
                    sample_locations: Vec::new(),
                });
                self.index
                    .entry((kind, reason))
                    .or_default()
                    .insert(Box::from(name), slot);
                slot
            }
        };
        let wants_sample = self
            .entries
            .get(slot)
            .is_some_and(|e| e.sample_locations.len() < self.samples);
        if wants_sample {
            self.admit(SAMPLE_BYTES)?;
        }
        let entry = self
            .entries
            .get_mut(slot)
            .ok_or(HtmlSanitizeError::Internal {
                reason: "report index names no entry",
            })?;
        // Saturation is unreachable: the work limit (at most 50M units) bounds the occurrences.
        entry.count = entry.count.saturating_add(1);
        if wants_sample {
            budget.reserve(&mut entry.sample_locations, 1)?;
            entry.sample_locations.push(location);
        }
        Ok(())
    }

    pub(crate) fn finish(self) -> RemovalReport {
        RemovalReport {
            entries: self.entries,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Limits;

    #[test]
    fn collector_allocation_refusals_are_typed() {
        for refused_call in [2, 3, 4] {
            let limits = Limits::ceiling();
            let budget = HtmlBudget::new(&limits);
            budget.refuse_call(refused_call);
            let mut builder = ReportBuilder::new(limits.report_bytes(), limits.report_samples());
            let err = builder
                .record(
                    RemovalKind::Tag,
                    "script",
                    RemovalReason::ForbiddenElement,
                    1,
                    &budget,
                )
                .expect_err("report allocation refused");
            assert_eq!(budget.calls(), refused_call);
            assert_eq!(
                err,
                HtmlSanitizeError::MemoryLimit {
                    limit: limits.working_bytes()
                }
            );
            assert_eq!(
                err.to_string(),
                format!(
                    "sanitising exceeds the working memory limit of {} bytes",
                    limits.working_bytes()
                )
            );
            assert_eq!(budget.failure(), Some(err));
            // No caller may finish a builder after record returned Err; the pipeline uses `?`.
        }
    }

    #[test]
    fn collector_repeated_entry_work_refusal_is_typed() {
        let limits = Limits::ceiling().with_work_units(1).expect("limit");
        let budget = HtmlBudget::new(&limits);
        let mut builder = ReportBuilder::new(limits.report_bytes(), 1);
        builder
            .record(
                RemovalKind::Tag,
                "script",
                RemovalReason::ForbiddenElement,
                1,
                &budget,
            )
            .expect("first record fits");
        let err = builder
            .record(
                RemovalKind::Tag,
                "script",
                RemovalReason::ForbiddenElement,
                2,
                &budget,
            )
            .expect_err("second record exceeds work");
        assert_eq!(err, HtmlSanitizeError::WorkLimit { limit: 1 });
        assert_eq!(
            err.to_string(),
            "sanitising exceeds the work limit of 1 units"
        );
        assert_eq!(builder.entries[0].count(), 1);
        assert_eq!(builder.entries[0].sample_locations(), &[1]);
    }

    #[test]
    fn collector_sample_growth_memory_boundary() {
        let limits = Limits::ceiling();
        let budget = HtmlBudget::new(&limits);
        let mut builder = ReportBuilder::new(limits.report_bytes(), limits.report_samples());
        for n in 1..=5 {
            builder
                .record(
                    RemovalKind::Tag,
                    "script",
                    RemovalReason::ForbiddenElement,
                    n,
                    &budget,
                )
                .expect("record fits");
        }
        let used = budget.peak_bytes();
        for at in [used - 1, used] {
            let limits = Limits::ceiling().with_working_bytes(at).expect("limit");
            let budget = HtmlBudget::new(&limits);
            let mut builder = ReportBuilder::new(limits.report_bytes(), limits.report_samples());
            for n in 1..=4 {
                builder
                    .record(
                        RemovalKind::Tag,
                        "script",
                        RemovalReason::ForbiddenElement,
                        n,
                        &budget,
                    )
                    .expect("first four fit");
            }
            let result = builder.record(
                RemovalKind::Tag,
                "script",
                RemovalReason::ForbiddenElement,
                5,
                &budget,
            );
            if at == used {
                result.expect("exact limit fits");
                assert_eq!(builder.finish().total_count(), 5);
            } else {
                let err = result.expect_err("sample growth refused");
                assert_eq!(err, HtmlSanitizeError::MemoryLimit { limit: at });
                assert_eq!(
                    err.to_string(),
                    format!("sanitising exceeds the working memory limit of {at} bytes")
                );
            }
        }
    }
}
