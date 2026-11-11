//! Encoding admission shares the caller's construction allowance before output copies.
use crate::{
    DescriptionBudget, DescriptionReservation, LimitKind, ViewError, ViewLimits, ViewResult,
};
use serde::Serialize;
use std::io::{self, Write};

/// Bounded encoding component of the description builder. This grants no caller authority.
#[derive(Debug)]
pub struct DescriptionEncoder {
    budget: DescriptionBudget,
    maximum: usize,
}
impl DescriptionEncoder {
    /// Use one request's shared construction allowance and validated output ceiling.
    pub fn new(budget: &DescriptionBudget, limits: &ViewLimits) -> Self {
        Self {
            budget: budget.clone(),
            maximum: limits.get(LimitKind::DescriptionEncodedBytes),
        }
    }
    /// Encode borrowed data; retained bytes keep their reservation until dropped.
    ///
    /// Buffer growth reserves the complete replacement while retaining the old buffer, then
    /// releases the old reservation after copying and dropping that storage.
    /// Serialization failure poisons this request's allowance. Schema/authorization admission
    /// is still required: a transport buffer alone is not a published description payload.
    pub fn encode<T: Serialize + ?Sized>(&self, value: &T) -> ViewResult<DescriptionBuffer> {
        let mut writer = BufferWriter {
            budget: self.budget.clone(),
            maximum: self.maximum,
            bytes: Vec::new(),
            reservation: self.budget.reserve(0)?,
            error: None,
        };
        let result = serde_json::to_writer(&mut writer, value);
        if let Some(error) = writer.error.take() {
            return Err(error);
        }
        if result.is_err() {
            self.budget.poison();
            return Err(ViewError::InvalidDescription(
                "Description serialization failed".to_owned(),
            ));
        }
        self.budget.check()?;
        Ok(DescriptionBuffer {
            bytes: writer.bytes,
            _reservation: writer.reservation,
        })
    }
}

/// Retained JSON transport bytes. No raw constructor, Clone or authority claim is provided.
pub struct DescriptionBuffer {
    bytes: Vec<u8>,
    _reservation: DescriptionReservation,
}
impl DescriptionBuffer {
    /// Borrow bounded bytes; the buffer remains charged while this borrow can exist.
    pub fn as_json_bytes(&self) -> &[u8] {
        &self.bytes
    }
}
impl std::fmt::Debug for DescriptionBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DescriptionBuffer")
            .field("encoded_bytes", &self.bytes.len())
            .finish_non_exhaustive()
    }
}

struct BufferWriter {
    budget: DescriptionBudget,
    maximum: usize,
    bytes: Vec<u8>,
    reservation: DescriptionReservation,
    error: Option<ViewError>,
}
impl BufferWriter {
    fn refuse(&mut self, error: ViewError) -> io::Error {
        self.budget.poison();
        self.error.get_or_insert(error);
        io::Error::other("description encoding admission refused")
    }
}
impl Write for BufferWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.error.is_some() {
            return Err(io::Error::other(
                "description encoding admission remains refused",
            ));
        }
        if let Err(error) = self.budget.check() {
            return Err(self.refuse(error));
        }
        let Some(total) = self
            .bytes
            .len()
            .checked_add(bytes.len())
            .filter(|total| *total <= self.maximum)
        else {
            return Err(self.refuse(ViewError::LimitExceeded {
                limit: LimitKind::DescriptionEncodedBytes,
                max: self.maximum,
                actual: self.bytes.len().checked_add(bytes.len()),
            }));
        };
        if total > self.bytes.capacity() {
            let Some(doubled) = self.bytes.capacity().checked_mul(2) else {
                return Err(self.refuse(ViewError::InvalidDescription(
                    "Description output capacity overflow".to_owned(),
                )));
            };
            let affordable = self.budget.available();
            // Grow amortized, but never reserve spare capacity beyond the shared allowance.
            // The atomic reservation is authoritative; this availability read is only a hint.
            let capacity = total
                .max(doubled)
                .min(self.maximum)
                .min(affordable)
                .max(total);
            // A moving realloc can temporarily own both buffers. Make that lifetime explicit:
            // charge a complete replacement, allocate/copy, drop old storage, then release it.
            if let Err(error) = self.reservation.grow(capacity) {
                return Err(self.refuse(error));
            }
            let mut replacement = Vec::new();
            if replacement.try_reserve_exact(capacity).is_err() {
                return Err(self.refuse(ViewError::InvalidDescription(
                    "Description output allocation failed".to_owned(),
                )));
            }
            // Allocators may report more capacity than requested. Admit any such spare
            // capacity before it can hold output; failure drops the buffer immediately.
            if replacement.capacity() > capacity {
                if replacement.capacity() > self.maximum {
                    return Err(self.refuse(ViewError::LimitExceeded {
                        limit: LimitKind::DescriptionEncodedBytes,
                        max: self.maximum,
                        actual: Some(replacement.capacity()),
                    }));
                }
                if let Err(error) = self.reservation.grow(replacement.capacity() - capacity) {
                    return Err(self.refuse(error));
                }
            }
            replacement.extend_from_slice(&self.bytes);
            let old = std::mem::replace(&mut self.bytes, replacement);
            drop(old);
            if let Err(error) = self.reservation.shrink_to(self.bytes.capacity()) {
                return Err(self.refuse(error));
            }
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.budget.check().map_err(|error| self.refuse(error))
    }
}

#[cfg(test)]
mod growth_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, reason = "test assertions")]
    use super::*;

    #[test]
    fn replacement_buffer_is_admitted_while_the_old_buffer_is_retained() {
        let budget = DescriptionBudget::with_limit(10).unwrap();
        let mut writer = BufferWriter {
            budget: budget.clone(),
            maximum: 100,
            bytes: Vec::new(),
            reservation: budget.reserve(0).unwrap(),
            error: None,
        };
        writer.write_all(b"abcd").unwrap();
        writer
            .write_all(b"efg")
            .expect_err("old plus replacement must fit before copying");
        let error = writer.error.unwrap();
        assert_eq!(
            error,
            ViewError::LimitExceeded {
                limit: LimitKind::DescriptionBuildBytes,
                max: 10,
                actual: Some(11),
            }
        );
        assert_eq!(
            error.to_string(),
            "simultaneously retained description bytes limit exceeded: maximum 10, reached 11"
        );
        assert!(budget.check().is_err());
    }

    #[test]
    fn replacement_copies_bytes_and_releases_only_old_storage() {
        let budget = DescriptionBudget::with_limit(14).unwrap();
        let mut writer = BufferWriter {
            budget: budget.clone(),
            maximum: 100,
            bytes: Vec::new(),
            reservation: budget.reserve(0).unwrap(),
            error: None,
        };
        writer.write_all(b"abcd").unwrap();
        writer.write_all(b"efg").unwrap();
        assert_eq!(writer.bytes, b"abcdefg");
        let held = budget
            .reserve(6)
            .expect("only replacement capacity eight remains charged");
        drop(held);
        drop(writer);
        budget.reserve(14).expect("all encoder storage released");
    }
}
