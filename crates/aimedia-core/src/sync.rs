use std::collections::VecDeque;

use thiserror::Error;

use crate::time::Timestamp;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SyncError {
    #[error("timeline capacity must be greater than zero")]
    ZeroCapacity,
    #[error("timestamp moved backwards from {previous_ms}ms to {current_ms}ms")]
    NonMonotonic { previous_ms: i64, current_ms: i64 },
}

#[derive(Debug, Clone)]
pub struct Timed<T> {
    pub timestamp: Timestamp,
    pub value: T,
}

/// A fixed-capacity, timestamp-ordered buffer. Old samples are evicted deterministically.
#[derive(Debug, Clone)]
pub struct BoundedTimeline<T> {
    capacity: usize,
    items: VecDeque<Timed<T>>,
}

impl<T> BoundedTimeline<T> {
    pub fn new(capacity: usize) -> Result<Self, SyncError> {
        if capacity == 0 {
            return Err(SyncError::ZeroCapacity);
        }
        Ok(Self {
            capacity,
            items: VecDeque::with_capacity(capacity),
        })
    }

    pub fn push(&mut self, timestamp: Timestamp, value: T) -> Result<Option<Timed<T>>, SyncError> {
        if let Some(last) = self.items.back() {
            if timestamp < last.timestamp {
                return Err(SyncError::NonMonotonic {
                    previous_ms: last.timestamp.as_millis(),
                    current_ms: timestamp.as_millis(),
                });
            }
        }

        let evicted = (self.items.len() == self.capacity)
            .then(|| self.items.pop_front())
            .flatten();
        self.items.push_back(Timed { timestamp, value });
        Ok(evicted)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }

    #[must_use]
    pub fn closest(&self, target: Timestamp, max_skew_ms: u64) -> Option<&Timed<T>> {
        self.items
            .iter()
            .min_by_key(|item| item.timestamp.abs_diff_ms(target))
            .filter(|item| item.timestamp.abs_diff_ms(target) <= max_skew_ms)
    }

    /// Removes samples strictly older than the given program timestamp.
    pub fn discard_before(&mut self, timestamp: Timestamp) -> usize {
        let original = self.items.len();
        while self
            .items
            .front()
            .is_some_and(|item| item.timestamp < timestamp)
        {
            self.items.pop_front();
        }
        original - self.items.len()
    }
}

#[derive(Debug)]
pub struct AlignedPair<'a, A, B> {
    pub master: &'a Timed<A>,
    pub secondary: &'a Timed<B>,
    pub skew_ms: u64,
}

#[must_use]
pub fn align_pair<'a, A, B>(
    master: &'a Timed<A>,
    secondary: &'a BoundedTimeline<B>,
    max_skew_ms: u64,
) -> Option<AlignedPair<'a, A, B>> {
    let secondary = secondary.closest(master.timestamp, max_skew_ms)?;
    Some(AlignedPair {
        master,
        secondary,
        skew_ms: master.timestamp.abs_diff_ms(secondary.timestamp),
    })
}
