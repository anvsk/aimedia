use std::cmp::Ordering;

use serde::{Deserialize, Deserializer, Serialize, de};
use thiserror::Error;

/// A media timestamp represented without floating-point rounding.
#[derive(Debug, Clone, Copy, Serialize, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Timestamp {
    pub ticks: i64,
    pub timescale: u32,
}

#[derive(Debug, Clone, Copy, Error, PartialEq, Eq)]
#[error("timestamp timescale must be greater than zero")]
pub struct InvalidTimescale;

impl Timestamp {
    pub const MPEG_TS_TIMESCALE: u32 = 90_000;

    #[must_use]
    pub const fn new(ticks: i64, timescale: u32) -> Self {
        assert!(
            timescale > 0,
            "timestamp timescale must be greater than zero"
        );
        Self { ticks, timescale }
    }

    pub const fn try_new(ticks: i64, timescale: u32) -> Result<Self, InvalidTimescale> {
        if timescale == 0 {
            Err(InvalidTimescale)
        } else {
            Ok(Self { ticks, timescale })
        }
    }

    #[must_use]
    pub const fn from_millis(millis: i64) -> Self {
        Self::new(millis, 1_000)
    }

    #[must_use]
    pub fn as_nanos(self) -> i128 {
        i128::from(self.ticks) * 1_000_000_000_i128 / i128::from(self.timescale)
    }

    #[must_use]
    pub fn as_millis(self) -> i64 {
        let millis = self.as_nanos() / 1_000_000;
        millis.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
    }

    #[must_use]
    pub fn with_offset_ms(self, offset_ms: i64) -> Self {
        let delta = i128::from(offset_ms) * i128::from(self.timescale) / 1_000;
        let ticks = i128::from(self.ticks) + delta;
        Self {
            ticks: ticks.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64,
            timescale: self.timescale,
        }
    }

    #[must_use]
    pub fn abs_diff_ms(self, other: Self) -> u64 {
        let difference = (self.as_nanos() - other.as_nanos()).unsigned_abs() / 1_000_000;
        difference.min(u128::from(u64::MAX)) as u64
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct TimestampWire {
            ticks: i64,
            timescale: u32,
        }

        let wire = TimestampWire::deserialize(deserializer)?;
        Timestamp::try_new(wire.ticks, wire.timescale).map_err(de::Error::custom)
    }
}

impl PartialEq for Timestamp {
    fn eq(&self, other: &Self) -> bool {
        self.as_nanos() == other.as_nanos()
    }
}

impl PartialOrd for Timestamp {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Timestamp {
    fn cmp(&self, other: &Self) -> Ordering {
        self.as_nanos().cmp(&other.as_nanos())
    }
}

/// Converts input timestamps to a monotonic program timeline.
#[derive(Debug, Clone)]
pub struct TimelineMapper {
    source_origin: Option<Timestamp>,
    program_origin: Timestamp,
    offset_ms: i64,
}

impl TimelineMapper {
    #[must_use]
    pub const fn new(program_origin: Timestamp, offset_ms: i64) -> Self {
        Self {
            source_origin: None,
            program_origin,
            offset_ms,
        }
    }

    #[must_use]
    pub fn map(&mut self, source: Timestamp) -> Timestamp {
        let source_origin = *self.source_origin.get_or_insert(source);
        let elapsed_ns = source.as_nanos() - source_origin.as_nanos();
        let offset_ns = i128::from(self.offset_ms) * 1_000_000;
        let program_ns = self.program_origin.as_nanos() + elapsed_ns + offset_ns;
        let ticks = program_ns * i128::from(Timestamp::MPEG_TS_TIMESCALE) / 1_000_000_000;
        Timestamp::new(
            ticks.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64,
            Timestamp::MPEG_TS_TIMESCALE,
        )
    }

    pub fn reset(&mut self) {
        self.source_origin = None;
    }
}
