use std::{
    collections::{HashMap, HashSet},
    error::Error,
    fmt,
    time::Duration,
};

use chrono::NaiveDate;
use finance_ib::market::{ConfirmedContract, ContractKind, ContractQuery};
use finance_market_protocol::DecimalText;
use rust_decimal::Decimal;

pub(super) const DEFAULT_PACING_COOLDOWN: Duration = Duration::from_secs(15);

/// Allocation for this scheduler within a shared IBKR user's L1 allowance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BudgetConfig {
    pub account_lines: usize,
    pub stock_lines: usize,
    pub hot_lines: usize,
    pub rotation_lines: usize,
    pub reserve_lines: usize,
    pub request_interval: Duration,
    pub rotation_dwell: Duration,
    pub reconnect_initial: Duration,
    pub reconnect_max: Duration,
    pub pacing_cooldown: Duration,
    pub coverage_retention: Duration,
    pub chain_refresh: Duration,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            account_lines: 100,
            stock_lines: 9,
            hot_lines: 16,
            rotation_lines: 45,
            reserve_lines: 30,
            request_interval: Duration::from_millis(250),
            rotation_dwell: Duration::from_secs(20),
            reconnect_initial: Duration::from_secs(2),
            reconnect_max: Duration::from_secs(60),
            pacing_cooldown: DEFAULT_PACING_COOLDOWN,
            coverage_retention: Duration::from_secs(3_600),
            chain_refresh: Duration::from_secs(900),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BudgetConfigError {
    InvalidAllocation,
    InvalidDuration(&'static str),
    ReconnectOrder,
}

impl fmt::Display for BudgetConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidAllocation => f.write_str(
                "L1 allocation and reserve exceed the account allowance or no lines are usable",
            ),
            Self::InvalidDuration(field) => write!(f, "invalid duration for {field}"),
            Self::ReconnectOrder => {
                f.write_str("maximum reconnect delay is below the initial delay")
            }
        }
    }
}

impl Error for BudgetConfigError {}

impl BudgetConfig {
    pub fn validate(&self) -> Result<(), BudgetConfigError> {
        let allocation = self
            .stock_lines
            .checked_add(self.hot_lines)
            .and_then(|sum| sum.checked_add(self.rotation_lines));
        if !allocation.is_some_and(|sum| {
            sum > 0
                && self
                    .account_lines
                    .checked_sub(self.reserve_lines)
                    .is_some_and(|available| sum <= available)
        }) {
            return Err(BudgetConfigError::InvalidAllocation);
        }

        // Tokio timers and UTC coverage arithmetic both need representable durations.
        let now = std::time::Instant::now();
        for (field, value) in [
            ("request_interval", self.request_interval),
            ("rotation_dwell", self.rotation_dwell),
            ("reconnect_initial", self.reconnect_initial),
            ("reconnect_max", self.reconnect_max),
            ("pacing_cooldown", self.pacing_cooldown),
            ("coverage_retention", self.coverage_retention),
            ("chain_refresh", self.chain_refresh),
        ] {
            if value.is_zero() || now.checked_add(value).is_none() {
                return Err(BudgetConfigError::InvalidDuration(field));
            }
        }
        if chrono::TimeDelta::from_std(self.coverage_retention).is_err() {
            return Err(BudgetConfigError::InvalidDuration("coverage_retention"));
        }
        if self.reconnect_max < self.reconnect_initial {
            return Err(BudgetConfigError::ReconnectOrder);
        }
        Ok(())
    }
}

pub fn watchlist() -> Vec<ContractQuery> {
    [
        ("QQQ", "NASDAQ"),
        ("SPY", "ARCA"),
        ("AAPL", "NASDAQ"),
        ("MSFT", "NASDAQ"),
        ("NVDA", "NASDAQ"),
        ("AMZN", "NASDAQ"),
        ("GOOGL", "NASDAQ"),
        ("META", "NASDAQ"),
        ("TSLA", "NASDAQ"),
    ]
    .into_iter()
    .map(|(symbol, primary_exchange)| ContractQuery::Equity {
        symbol: symbol.into(),
        primary_exchange: primary_exchange.into(),
    })
    .collect()
}

/// Rank eligible contracts for a sweep. The scheduler tracks turns across priority changes.
pub(super) fn rank_options(
    options: impl IntoIterator<Item = ConfirmedContract>,
    focus: Option<i32>,
    spots: &HashMap<i32, DecimalText>,
    today: NaiveDate,
) -> Vec<ConfirmedContract> {
    let mut ranked: Vec<_> = options
        .into_iter()
        .filter_map(|contract| {
            let ContractKind::Option(option) = &contract.kind else {
                return None;
            };
            let strike = option.strike.value();
            if option.expiry < today || strike <= Decimal::ZERO {
                return None;
            }
            let distance = spots
                .get(&option.underlying_con_id)
                .map(DecimalText::value)
                .filter(|spot| *spot > Decimal::ZERO)
                .map(|spot| exact_distance(strike, spot));
            let rank = (
                focus != Some(option.underlying_con_id),
                option.expiry,
                distance.is_none(),
                distance,
                contract.con_id,
            );
            Some((rank, contract))
        })
        .collect();
    ranked.sort_unstable_by_key(|(rank, _)| *rank);
    let mut seen = HashSet::new();
    ranked
        .into_iter()
        .filter_map(|(_, contract)| seen.insert(contract.con_id).then_some(contract))
        .collect()
}

// Positive DecimalText values fit in two fixed-point parts. Keep them separate
// so subtracting numbers of different scales cannot round a ranking distance.
fn exact_distance(left: Decimal, right: Decimal) -> (u128, u128) {
    fn parts(value: Decimal) -> (u128, u128) {
        let places = 10_u128.pow(value.scale());
        let mantissa = value.mantissa().unsigned_abs();
        let integer = mantissa / places;
        let fraction = (mantissa % places) * (10_u128.pow(Decimal::MAX_SCALE) / places);
        (integer, fraction)
    }

    let (high, low) = if left >= right {
        (parts(left), parts(right))
    } else {
        (parts(right), parts(left))
    };
    let (whole, fraction) = (high.0 - low.0, high.1.checked_sub(low.1));
    match fraction {
        Some(fraction) => (whole, fraction),
        None => (
            whole - 1,
            10_u128.pow(Decimal::MAX_SCALE) - (low.1 - high.1),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use finance_ib::market::{OptionContract, OptionRight};

    #[test]
    fn default_budget_and_watchlist_match_the_nine_stock_allocation() {
        let config = BudgetConfig::default();
        assert_eq!(config.account_lines, 100);
        assert_eq!(config.stock_lines, 9);
        assert_eq!(config.hot_lines, 16);
        assert_eq!(config.rotation_lines, 45);
        assert_eq!(config.reserve_lines, 30);
        assert!(config.validate().is_ok());
        let watchlist = watchlist();
        assert_eq!(watchlist.len(), config.stock_lines);
        assert_eq!(
            watchlist[1],
            ContractQuery::Equity {
                symbol: "SPY".into(),
                primary_exchange: "ARCA".into(),
            }
        );
    }

    #[test]
    fn validates_custom_limits_without_spending_the_reserve() {
        let config = BudgetConfig {
            account_lines: 20,
            stock_lines: 2,
            hot_lines: 3,
            rotation_lines: 4,
            reserve_lines: 10,
            ..BudgetConfig::default()
        };
        assert!(config.validate().is_ok());
        let mut invalid = config.clone();
        invalid.rotation_lines = 6;
        assert_eq!(
            invalid.validate(),
            Err(BudgetConfigError::InvalidAllocation)
        );
        invalid.rotation_lines = usize::MAX;
        assert_eq!(
            invalid.validate(),
            Err(BudgetConfigError::InvalidAllocation)
        );
        let mut invalid = config.clone();
        invalid.chain_refresh = Duration::ZERO;
        assert_eq!(
            invalid.validate(),
            Err(BudgetConfigError::InvalidDuration("chain_refresh"))
        );
        let mut invalid = config;
        invalid.reconnect_initial = invalid.reconnect_max + Duration::from_secs(1);
        assert_eq!(invalid.validate(), Err(BudgetConfigError::ReconnectOrder));
    }

    fn decimal(value: &str) -> DecimalText {
        value.to_owned().try_into().unwrap()
    }

    fn option(id: i32, underlying: i32, expiry: NaiveDate, strike: &str) -> ConfirmedContract {
        ConfirmedContract {
            con_id: id,
            symbol: "SYNTH".into(),
            primary_exchange: None,
            kind: ContractKind::Option(OptionContract {
                expiry,
                strike: decimal(strike),
                right: OptionRight::Call,
                multiplier: decimal("100"),
                underlying_con_id: underlying,
            }),
            liquid_hours: Vec::new(),
        }
    }

    #[test]
    fn option_sweep_prioritizes_focus_expiry_and_exact_distance_then_id() {
        let today = NaiveDate::from_ymd_opt(2031, 1, 1).unwrap();
        let next = today.succ_opt().unwrap();
        let expired = today.pred_opt().unwrap();
        let spots = HashMap::from([(1, decimal("100")), (2, decimal("110"))]);
        let duplicate = option(8, 1, today, "100.02");
        let contracts = [
            option(3, 2, today, "110"),
            option(2, 1, next, "100"),
            duplicate.clone(),
            option(9, 1, today, "100.01"),
            option(1, 1, expired, "100"),
            duplicate,
            option(7, 1, today, "99.98"),
            option(5, 3, today, "100"),
        ];
        let ids = rank_options(contracts, Some(1), &spots, today)
            .into_iter()
            .map(|contract| contract.con_id)
            .collect::<Vec<_>>();
        assert_eq!(ids, [9, 7, 8, 2, 3, 5]);
    }

    #[test]
    fn option_sweep_preserves_distance_order_across_disparate_decimal_scales() {
        let today = NaiveDate::from_ymd_opt(2031, 1, 1).unwrap();
        let spots = HashMap::from([(1, decimal("9999999999999999999999999999"))]);
        let contracts = [
            option(1, 1, today, "0.000000000000000000000000001"),
            option(2, 1, today, "0.000000000000000000000000002"),
        ];
        let ids = rank_options(contracts, None, &spots, today)
            .into_iter()
            .map(|contract| contract.con_id)
            .collect::<Vec<_>>();
        assert_eq!(ids, [2, 1]);
    }
}
