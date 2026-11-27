use super::{
    ConfirmedContract, ContractKind, ContractQuery, MarketError, PaperMarket, bounded_request,
    check_expiry, confirmed_contract, contract_matches, option_details, option_market_date,
    positive_decimal,
};
use crate::market::{OptionChainSource, OptionSeries};
use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};
use finance_market_protocol::DecimalText;
use ibapi::contracts::{Contract, ContractDetails, OptionChain, SecurityType};
use rust_decimal::Decimal;
use std::collections::{BTreeMap, BTreeSet};

#[async_trait]
impl OptionChainSource for PaperMarket {
    async fn option_series(
        &self,
        equity: &ConfirmedContract,
    ) -> Result<Vec<OptionSeries>, MarketError> {
        self.peer
            .while_connected(async {
                self.require_confirmed_equity(equity).await?;
                let rows = bounded_request(
                    self.timeout,
                    self.peer.client.option_chain_complete(
                        &equity.symbol,
                        "",
                        SecurityType::Stock,
                        equity.con_id,
                    ),
                )
                .await?;
                Ok(discovered_series(
                    equity.con_id,
                    rows,
                    option_market_date(Utc::now())?,
                ))
            })
            .await
    }

    async fn confirm_series(
        &self,
        equity: &ConfirmedContract,
        series: &OptionSeries,
    ) -> Result<Vec<ConfirmedContract>, MarketError> {
        self.peer
            .while_connected(async {
                self.require_confirmed_equity(equity).await?;
                let query = series_query(equity, series, Utc::now())?;
                let details =
                    bounded_request(self.timeout, self.peer.client.contract_details(&query))
                        .await?;
                let contracts = confirmed_series(equity, series, details, Utc::now());
                let mut cache = self.confirmed.lock().await;
                cache.extend(contracts.iter().map(|item| (item.con_id, item.clone())));
                Ok(contracts)
            })
            .await
    }
}

impl PaperMarket {
    async fn require_confirmed_equity(
        &self,
        equity: &ConfirmedContract,
    ) -> Result<(), MarketError> {
        if equity.kind != ContractKind::Equity {
            return Err(MarketError::InvalidRequest);
        }
        if self.confirmed.lock().await.get(&equity.con_id) != Some(equity) {
            return Err(MarketError::UnconfirmedContract);
        }
        Ok(())
    }
}

fn discovered_series(con_id: i32, rows: Vec<OptionChain>, today: NaiveDate) -> Vec<OptionSeries> {
    let mut series =
        BTreeMap::<(NaiveDate, String, Decimal), BTreeMap<Decimal, DecimalText>>::new();
    for row in rows {
        if row.exchange != "SMART" || row.underlying_contract_id != con_id {
            continue;
        }
        let Ok(multiplier) = row.multiplier.parse::<DecimalText>() else {
            continue;
        };
        if multiplier.value() <= Decimal::ZERO || row.trading_class.trim().is_empty() {
            continue;
        }
        let strikes: BTreeMap<_, _> = row
            .strikes
            .into_iter()
            .filter_map(positive_decimal)
            .map(|strike| (strike.value(), strike))
            .collect();
        if strikes.is_empty() {
            continue;
        }
        for expiry in row.expirations {
            let Ok(expiry) = NaiveDate::parse_from_str(&expiry, "%Y%m%d") else {
                continue;
            };
            if expiry >= today {
                series
                    .entry((
                        expiry,
                        row.trading_class.clone(),
                        multiplier.value().normalize(),
                    ))
                    .or_default()
                    .extend(strikes.clone());
            }
        }
    }
    series
        .into_iter()
        .map(
            |((expiry, trading_class, multiplier), strikes)| OptionSeries {
                expiry,
                trading_class,
                multiplier: multiplier
                    .to_string()
                    .parse()
                    .expect("normalized multiplier came from validated DecimalText"),
                strikes: strikes.into_values().collect(),
            },
        )
        .collect()
}

fn series_query(
    equity: &ConfirmedContract,
    series: &OptionSeries,
    now: DateTime<Utc>,
) -> Result<Contract, MarketError> {
    check_expiry(series.expiry, now)?;
    let expiry = series.expiry.format("%Y%m%d").to_string();
    if expiry.len() != 8
        || series.trading_class.trim().is_empty()
        || series.multiplier.value() <= Decimal::ZERO
        || series.strikes.is_empty()
    {
        return Err(MarketError::InvalidRequest);
    }
    // A single expiry, class and multiplier bounds the contract-details
    // request. The SDK omits the default strike and the absent right.
    Ok(Contract {
        symbol: equity.symbol.as_str().into(),
        security_type: SecurityType::Option,
        last_trade_date_or_contract_month: expiry,
        trading_class: series.trading_class.clone(),
        multiplier: series.multiplier.as_str().into(),
        exchange: "SMART".into(),
        currency: "USD".into(),
        ..Contract::default()
    })
}

fn confirmed_series(
    equity: &ConfirmedContract,
    series: &OptionSeries,
    details: Vec<ContractDetails>,
    now: DateTime<Utc>,
) -> Vec<ConfirmedContract> {
    let strikes: BTreeSet<_> = series.strikes.iter().map(DecimalText::value).collect();
    let mut contracts = BTreeMap::<i32, Option<ConfirmedContract>>::new();
    for row in details {
        let id = row.contract.contract_id;
        if row.under_contract_id != equity.con_id
            || row.contract.trading_class != series.trading_class
            || !contract_matches(&ContractQuery::Option { con_id: id }, &row)
        {
            continue;
        }
        let Ok(option) = option_details(&row, now) else {
            continue;
        };
        if option.expiry != series.expiry
            || option.multiplier.value() != series.multiplier.value()
            || !strikes.contains(&option.strike.value())
        {
            continue;
        }
        let item = confirmed_contract(row, ContractKind::Option(option));
        match contracts.entry(id) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(Some(item));
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                if entry.get().as_ref() != Some(&item) {
                    entry.insert(None);
                }
            }
        }
    }
    contracts.into_values().flatten().collect()
}

#[cfg(test)]
#[path = "chain_tests.rs"]
mod tests;
