use std::time::Duration;

use async_trait::async_trait;
use finance_collector::{
    market_ingress::{recording_policy, wal_root},
    market_wal::WalReader,
};
use finance_ib::{
    depth::{
        DEFAULT_DEPTH_ROWS, DepthError, DepthFocus, DepthLevel, DepthOperation, DepthPermissions,
        DepthRequest, DepthSide, DepthSource, DepthState, DepthSubscription, DepthSymbol,
        DepthUpdate,
        permission::{PRODUCT, SOURCE},
        run_depth,
    },
    market::{ConfirmedContract, ContractKind},
};
use tokio::sync::watch;

struct SyntheticDepth;

#[async_trait]
impl DepthSource for SyntheticDepth {
    type Subscription = SyntheticLine;

    async fn confirm_depth(&self, symbol: DepthSymbol) -> Result<ConfirmedContract, DepthError> {
        Ok(ConfirmedContract {
            con_id: 9,
            symbol: symbol.as_str().into(),
            primary_exchange: Some("NASDAQ".into()),
            kind: ContractKind::Equity,
            liquid_hours: Vec::new(),
        })
    }

    async fn subscribe_depth(
        &self,
        request: &DepthRequest,
    ) -> Result<Self::Subscription, DepthError> {
        assert_eq!(request.contract().symbol, "QQQ");
        assert_eq!(request.rows(), DEFAULT_DEPTH_ROWS);
        // A fixed synthetic row. The size exceeds JS's exact integer range.
        Ok(SyntheticLine(Some(DepthUpdate::Row {
            side: DepthSide::Bid,
            operation: DepthOperation::Insert,
            position: 0,
            level: Some(DepthLevel {
                price: "123.4567".parse().unwrap(),
                size: "9007199254740993.125".parse().unwrap(),
            }),
        })))
    }
}

struct SyntheticLine(Option<DepthUpdate>);

#[async_trait]
impl DepthSubscription for SyntheticLine {
    fn request_id(&self) -> i32 {
        12
    }

    async fn next(&mut self) -> Result<Option<DepthUpdate>, DepthError> {
        match self.0.take() {
            Some(update) => Ok(Some(update)),
            None => std::future::pending().await,
        }
    }

    async fn cancel(&mut self) -> Result<(), DepthError> {
        Ok(())
    }
}

#[tokio::test]
async fn totalview_displays_exact_rows_without_creating_wal() {
    let temp = tempfile::tempdir().unwrap();
    let (sender, receiver) = watch::channel(DepthFocus::new(&[], Some(DepthSymbol::Qqq)).unwrap());
    let mut sender = Some(sender);
    let permissions = DepthPermissions {
        equity_l1_confirmed: true,
        totalview_confirmed: true,
        eds_confirmed: true,
    };
    let display = run_depth(
        &SyntheticDepth,
        receiver,
        permissions,
        DEFAULT_DEPTH_ROWS,
        |frame| {
            if frame.state == DepthState::Receiving {
                assert_eq!(frame.venue(), "NASDAQ");
                assert_eq!(frame.requested_rows, DEFAULT_DEPTH_ROWS);
                assert_eq!(frame.returned_rows(), 1);
                assert!(frame.asks.is_empty());
                assert_eq!(frame.bids[0].size.as_str(), "9007199254740993.125");
                drop(sender.take().expect("only one synthetic row"));
            }
        },
    );
    tokio::time::timeout(Duration::from_secs(3), display)
        .await
        .expect("synthetic display timed out")
        .unwrap();
    assert!(sender.is_none(), "display never received the synthetic row");
    assert!(
        recording_policy(
            temp.path(),
            &SOURCE.parse().unwrap(),
            &PRODUCT.parse().unwrap()
        )
        .unwrap()
        .is_none()
    );
    assert!(
        WalReader::products(&wal_root(temp.path()))
            .unwrap()
            .is_empty()
    );
    assert_eq!(temp.path().read_dir().unwrap().count(), 0);
}
