use serde::{Deserialize, Serialize};
use std::sync::LazyLock;
use utoipa::ToSchema;

/// The earliest usable point within or after the identified session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Availability {
    SessionOpen,
    SessionClose,
    AfterSessionClose,
    NextSessionClose,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Universe<'a> {
    pub id: &'a str,
    pub calendar: &'a str,
    pub currency: &'a str,
    pub asset: &'a str,
    pub point_in_time: bool,
    pub available_at: Availability,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Bars<'a> {
    pub id: &'a str,
    pub calendar: &'a str,
    pub currency: &'a str,
    pub asset: &'a str,
    pub timeframe: &'a str,
    pub price_basis: &'a str,
    pub available_at: Availability,
}

/// Trusted metadata supplied by finance, separate from the uploaded bundle.
/// Catalog entries are keyed by opaque IDs. No reference is fetched or opened.
pub struct Catalog<'a> {
    pub calendars: &'a [&'a str],
    pub universes: &'a [Universe<'a>],
    pub bars: &'a [Bars<'a>],
}

#[derive(Deserialize)]
struct HistoricalResearchId<'a> {
    id: &'a str,
}

pub(super) fn is_historical_research(id: &str) -> bool {
    static MOMENTUM: LazyLock<HistoricalResearchId<'static>> = LazyLock::new(|| {
        serde_json::from_str(include_str!(
            "../../../../strategies/registry/factor-momentum-v1.json"
        ))
        .expect("the historical factor-momentum registry has a built-in strategy ID")
    });
    id == MOMENTUM.id
}

const REVIEWED_BARS: Bars<'static> = Bars {
    id: "example.reviewed-us-daily@1.0.0",
    calendar: "XNYS",
    currency: "USD",
    asset: "cash_equity",
    timeframe: "1d",
    price_basis: "raw_split_free_reviewed",
    available_at: Availability::SessionClose,
};

/// Fixture catalog used by the standalone CLI and examples.
pub fn example_catalog() -> Catalog<'static> {
    Catalog {
        calendars: &["XNYS"],
        universes: &[Universe {
            id: "example.us-cash-equities@1.0.0",
            calendar: "XNYS",
            currency: "USD",
            asset: "cash_equity",
            point_in_time: true,
            available_at: Availability::SessionOpen,
        }],
        bars: &[
            REVIEWED_BARS,
            Bars {
                id: "example.delayed-us-daily@1.0.0",
                available_at: Availability::AfterSessionClose,
                ..REVIEWED_BARS
            },
        ],
    }
}
