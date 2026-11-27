use crate::indicators::Bar;
use chrono::{DateTime, NaiveDate, TimeDelta, Utc};
use finance_market_protocol::{CalendarId, DecimalText, DelayTier, InstrumentId, Token};
use serde::{Deserialize, Deserializer, Serialize, de::IntoDeserializer};
use std::{
    fs::File,
    io::{self, Read},
    path::PathBuf,
};
use utoipa::ToSchema;

const MAX_BYTES: u64 = 32 * 1024 * 1024;
pub(super) const MAX_CLOSED_BARS: usize = 50_000;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BarSeriesKey {
    pub instrument_id: InstrumentId,
    pub source: Token,
    pub product: Token,
    pub timeframe: Token,
    pub adjustment: Token,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Deserialize, Serialize, ToSchema)]
pub enum DisplayPolicy {
    #[serde(rename = "authorized_display")]
    AuthorizedDisplay,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub enum ProjectionVersion {
    #[serde(rename = "1.0")]
    V1,
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BarProvenance {
    pub license: Token,
    pub delay_tier: DelayTier,
    pub calendar_id: CalendarId,
    /// Market timezone label assigned by the publisher. Query times are UTC RFC3339.
    pub market_timezone: String,
    #[serde(deserialize_with = "text_tag")]
    pub display: DisplayPolicy,
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectedBar {
    pub open_ts: DateTime<Utc>,
    pub end_ts: DateTime<Utc>,
    /// Availability of this bar revision, independent of the derived chart values.
    pub first_available_ts: DateTime<Utc>,
    #[schema(value_type = String, format = Date)]
    pub trading_day: NaiveDate,
    pub open: DecimalText,
    pub high: DecimalText,
    pub low: DecimalText,
    pub close: DecimalText,
    pub volume: DecimalText,
}

impl ProjectedBar {
    fn valid(&self, as_of: DateTime<Utc>, previous: Option<&Self>, provisional: bool) -> bool {
        let available_after = if provisional {
            self.open_ts
        } else {
            self.end_ts
        };
        self.indicator_bar().validate_structure().is_ok()
            && available_after <= self.first_available_ts
            && self.first_available_ts <= as_of
            && previous
                .is_none_or(|bar| bar.end_ts <= self.open_ts && bar.trading_day <= self.trading_day)
    }

    fn within_utc_day(&self) -> bool {
        self.open_ts.date_naive() == self.trading_day
            && self
                .end_ts
                .checked_sub_signed(TimeDelta::nanoseconds(1))
                .is_some_and(|last| last.date_naive() == self.trading_day)
    }

    pub(super) fn indicator_bar(&self) -> Bar {
        Bar {
            open_ts: self.open_ts,
            end_ts: self.end_ts,
            trading_day: self.trading_day,
            open: self.open.value(),
            high: self.high.value(),
            low: self.low.value(),
            close: self.close.value(),
            volume: self.volume.value(),
        }
    }
}

/// Latest publisher-approved view of a single series. Corrections are already applied.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BarSnapshot {
    #[serde(deserialize_with = "text_tag")]
    pub schema_version: ProjectionVersion,
    pub series: BarSeriesKey,
    pub provenance: BarProvenance,
    /// Must advance on each new publication to distinguish independently read pages.
    pub as_of: DateTime<Utc>,
    /// All retained observations from the publisher's fixed indicator replay origin.
    pub history_complete: bool,
    pub closed: Vec<ProjectedBar>,
    pub provisional: Option<ProjectedBar>,
}

fn text_tag<'de, D: Deserializer<'de>, T: Deserialize<'de>>(input: D) -> Result<T, D::Error> {
    T::deserialize(String::deserialize(input)?.into_deserializer())
}

impl BarSnapshot {
    pub(super) fn valid(&self, key: &BarSeriesKey) -> bool {
        let continuous = self.provenance.calendar_id.as_str() == "24X7";
        if self.series != *key
            || self.closed.len() > MAX_CLOSED_BARS
            || !valid_timezone(&self.provenance.market_timezone)
            || (continuous && self.provenance.market_timezone != "UTC")
        {
            return false;
        }
        let mut previous = None;
        for (bar, provisional) in self
            .closed
            .iter()
            .map(|bar| (bar, false))
            .chain(self.provisional.iter().map(|bar| (bar, true)))
        {
            if !bar.valid(self.as_of, previous, provisional)
                || (continuous && !bar.within_utc_day())
            {
                return false;
            }
            previous = Some(bar);
        }
        true
    }
}

fn valid_timezone(label: &str) -> bool {
    label == "UTC"
        || (label.len() <= 128
            && label.contains('/')
            && label.split('/').all(|segment| {
                !segment.is_empty()
                    && segment.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'+')
                    })
            }))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BarsReadError {
    Missing,
    Unreadable,
    Invalid,
}

/// Read boundary for publishers of authorized, revision-resolved chart data.
/// Called on a blocking worker. Implementations must bound the work and supply a
/// complete prefix when setting `history_complete`.
pub trait BarsSource: Send + Sync + 'static {
    fn snapshot(&self, series: &BarSeriesKey) -> Result<BarSnapshot, BarsReadError>;
}

pub struct LocalBarsSource {
    data_root: PathBuf,
}

impl LocalBarsSource {
    pub fn new(data_root: PathBuf) -> Self {
        Self { data_root }
    }
}

impl BarsSource for LocalBarsSource {
    fn snapshot(&self, series: &BarSeriesKey) -> Result<BarSnapshot, BarsReadError> {
        // Snapshots are published with atomic replacement inside a trusted local
        // data root. Refuse path aliases below that root and non-regular files.
        let root = self.data_root.canonicalize().map_err(io_error)?;
        let path = root
            .join("finance-state/bars/v1")
            .join(series.source.as_str())
            .join(series.product.as_str())
            .join(series.instrument_id.as_str())
            .join(series.adjustment.as_str())
            .join(format!("{}.json", series.timeframe));
        if path.canonicalize().map_err(io_error)? != path {
            return Err(BarsReadError::Invalid);
        }
        let metadata = path.metadata().map_err(io_error)?;
        if !metadata.is_file() || metadata.len() > MAX_BYTES {
            return Err(BarsReadError::Invalid);
        }
        let mut bytes = Vec::new();
        File::open(path)
            .map_err(io_error)?
            .take(MAX_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(io_error)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(BarsReadError::Invalid);
        }
        serde_json::from_slice(&bytes).map_err(|_| BarsReadError::Invalid)
    }
}

fn io_error(error: io::Error) -> BarsReadError {
    if error.kind() == io::ErrorKind::NotFound {
        BarsReadError::Missing
    } else {
        BarsReadError::Unreadable
    }
}
