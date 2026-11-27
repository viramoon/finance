use super::{PaperMarket, check_expiry, sdk_market_contract, transport_error};
use crate::{
    historical::{OptionMinuteBar, Pacer, no_history, read_bars, request_trade_minute_page},
    market::{ConfirmedContract, ContractKind, MarketError, OptionMinuteSource},
};
use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};
use finance_market_protocol::{ClosedBar, U64Text};
use ibapi::market_data::historical;
use std::{ops::Range, path::Path};
use time::OffsetDateTime;
use tokio::time::timeout;

const MINUTE_NS: u64 = 60_000_000_000;
const MINUTES_PER_PAGE: u64 = 24 * 60;

#[async_trait]
impl OptionMinuteSource for PaperMarket {
    async fn option_trade_minutes(
        &self,
        confirmed: &ConfirmedContract,
        window: Range<U64Text>,
        expires_at: U64Text,
        data_root: &Path,
    ) -> Result<Vec<OptionMinuteBar>, MarketError> {
        self.peer
            .while_connected(async {
                if self.confirmed.lock().await.get(&confirmed.con_id) != Some(confirmed) {
                    return Err(MarketError::UnconfirmedContract);
                }
                let ContractKind::Option(option) = &confirmed.kind else {
                    return Err(MarketError::InvalidRequest);
                };
                let now = Utc::now();
                let now_ns = check_cutoff(option.expiry, expires_at, now)?;
                if window.start >= window.end || window.end.get() > now_ns {
                    return Err(MarketError::InvalidRequest);
                }

                let contract = sdk_market_contract(confirmed);
                let mut start = window.start.get().div_ceil(MINUTE_NS);
                let end = window.end.get() / MINUTE_NS;
                let mut bars = Vec::new();
                while start < end {
                    let page_end = end.min(start + MINUTES_PER_PAGE);
                    let anchor =
                        OffsetDateTime::from_unix_timestamp_nanos(i128::from(page_end * MINUTE_NS))
                            .map_err(|_| MarketError::InvalidRequest)?;
                    let pacer = Pacer::acquire(data_root).await.map_err(pacing_error)?;
                    pacer.wait().await.map_err(pacing_error)?;
                    pacer.begin().map_err(pacing_error)?;
                    let response = async {
                        check_cutoff(option.expiry, expires_at, Utc::now())?;
                        timeout(
                            self.timeout,
                            request_trade_minute_page(&self.peer, &contract, anchor),
                        )
                        .await
                        .map_err(|_| MarketError::Timeout)
                    }
                    .await;
                    pacer.finish().map_err(pacing_error)?;
                    let raw_bars = match response? {
                        Err(error) if !no_history(&error) => return Err(transport_error(error)),
                        other => read_bars(other).map_err(|_| MarketError::Transport)?,
                    };
                    let page = start * MINUTE_NS..page_end * MINUTE_NS;
                    for raw in raw_bars {
                        if let Some(bar) = option_bar(raw, &page)? {
                            bars.push(bar);
                        }
                    }
                    start = page_end;
                }
                Ok(bars)
            })
            .await
    }
}

fn pacing_error(error: crate::Error) -> MarketError {
    match error {
        crate::Error::Config(_) => MarketError::InvalidConfiguration,
        _ => MarketError::Transport,
    }
}

fn check_cutoff(
    expiry: NaiveDate,
    expires_at: U64Text,
    now: DateTime<Utc>,
) -> Result<u64, MarketError> {
    let now_ns = now
        .timestamp_nanos_opt()
        .and_then(|value| u64::try_from(value).ok())
        .ok_or(MarketError::InvalidRequest)?;
    if now_ns >= expires_at.get() {
        return Err(MarketError::ExpiredContract);
    }
    check_expiry(expiry, now)?;
    Ok(now_ns)
}

fn option_bar(
    raw: historical::Bar,
    page: &Range<u64>,
) -> Result<Option<OptionMinuteBar>, MarketError> {
    let historical::BarTimestamp::DateTime(at) = raw.date else {
        return Err(MarketError::Transport);
    };
    let start = u64::try_from(at.unix_timestamp_nanos()).map_err(|_| MarketError::Transport)?;
    if !page.contains(&start) {
        return Ok(None);
    }
    if !start.is_multiple_of(MINUTE_NS) {
        return Err(MarketError::Transport);
    }
    // The SDK already decoded binary64. Keep its shortest round-trip decimal
    // and reject values outside the protocol's exact decimal representation.
    let exact = |value: f64| {
        value
            .to_string()
            .parse()
            .map_err(|_| MarketError::Transport)
    };
    Ok(Some(OptionMinuteBar {
        close_ts: (start + MINUTE_NS).into(),
        data: ClosedBar {
            open_ts: start.into(),
            open: exact(raw.open)?,
            high: exact(raw.high)?,
            low: exact(raw.low)?,
            close: exact(raw.close)?,
            volume: exact(raw.volume)?,
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sdk_option_bar_keeps_minute_bounds_and_shortest_decimal_text() {
        let start = 1_741_363_200_000_000_000;
        let mut raw = historical::Bar {
            date: historical::BarTimestamp::DateTime(
                OffsetDateTime::from_unix_timestamp_nanos(i128::from(start)).unwrap(),
            ),
            open: 1.234_567_891,
            high: 1.4,
            low: 1.2,
            close: 1.25,
            volume: 9_007_199_254_740_992.0,
            wap: 1.25,
            count: 1,
        };
        let page = start..start + MINUTE_NS;
        let bar = option_bar(raw, &page).unwrap().unwrap();
        assert_eq!(bar.data.open_ts.get(), start);
        assert_eq!(bar.close_ts.get(), page.end);
        assert_eq!(bar.data.open.as_str(), "1.234567891");
        assert_eq!(bar.data.volume.as_str(), "9007199254740992");
        assert_eq!(option_bar(raw, &(page.end..page.end + MINUTE_NS)), Ok(None));
        raw.volume = f64::NAN;
        assert_eq!(option_bar(raw, &page), Err(MarketError::Transport));
    }

    #[test]
    fn cutoff_uses_exact_instant_on_the_expiry_date() {
        let expiry = NaiveDate::from_ymd_opt(2025, 3, 7).unwrap();
        let now = "2025-03-07T21:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let nanos = u64::try_from(now.timestamp_nanos_opt().unwrap()).unwrap();
        assert_eq!(check_cutoff(expiry, (nanos + 1).into(), now), Ok(nanos));
        assert_eq!(
            check_cutoff(expiry, nanos.into(), now),
            Err(MarketError::ExpiredContract)
        );
    }
}
