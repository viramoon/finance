use std::{error::Error, sync::Arc};

use finance_collector::line_budget::{BudgetConfig, LineScheduler, watchlist};
use finance_ib::{ConnectionManager, IbConfig, IbTransport, depth::DepthPermissions};
use tokio::sync::watch;

use crate::config::SubscriptionOwnerConfig;

use super::{DepthFrames, Licensed, MarketReadSources};

/// Runtime owner of the API's optional read-only paper subscriptions.
pub struct PaperSubscriptionOwner {
    scheduler: LineScheduler,
    manager: ConnectionManager<IbTransport>,
}

impl PaperSubscriptionOwner {
    pub fn attach(
        sources: &mut MarketReadSources,
        config: Option<SubscriptionOwnerConfig>,
        load_ib: impl FnOnce() -> finance_ib::Result<IbConfig>,
    ) -> Result<Option<Self>, Box<dyn Error>> {
        let Some(config) = config else {
            return Ok(None);
        };
        if !config.equity_l1_confirmed {
            return Err("subscription owner requires confirmed paper equity L1 permissions".into());
        }
        let mut ib = load_ib()?;
        if [
            ib.probe_client_id,
            ib.collector_client_id,
            ib.trader_client_id,
        ]
        .contains(&config.client_id)
        {
            return Err("subscription owner requires a dedicated IB client ID".into());
        }
        ib.probe_client_id = config.client_id;
        let manager = ConnectionManager::new(ib, IbTransport)?;
        let scheduler = LineScheduler::new(BudgetConfig::default(), watchlist())?;
        let depth = config.nasdaq_totalview_license.map(|license| {
            let frames = DepthFrames::default();
            sources.depth = Some(Licensed::new(frames.clone(), license));
            (
                frames,
                DepthPermissions {
                    equity_l1_confirmed: true,
                    totalview_confirmed: true,
                    eds_confirmed: true,
                },
            )
        });
        let (scheduler, demand) = scheduler.with_subscription_demand(depth);
        sources.option_lines = Some(Licensed::new(
            Arc::new(scheduler.handle()),
            config.opra_license,
        ));
        sources.subscription_demand = Some(demand);
        Ok(Some(Self { scheduler, manager }))
    }

    pub async fn run(
        self,
        stop: watch::Receiver<bool>,
    ) -> Result<(), finance_ib::market::MarketError> {
        self.scheduler.run_paper(&self.manager, stop).await
    }
}
