use std::{
    collections::HashMap,
    error::Error,
    fmt,
    future::Future,
    sync::{Arc, RwLock},
};

use async_trait::async_trait;
use finance_ib::depth::{
    DEFAULT_DEPTH_ROWS, DepthError, DepthFocus, DepthFrame, DepthPermissions, DepthRequest,
    DepthSelection, DepthSource, DepthSubscription, DepthSymbol, DepthUpdate, run_depth,
};
use finance_ib::market::ConfirmedContract;
use tokio::sync::watch;

use super::{SubscriptionDemand, pacing::RequestPacing};

/// Latest display frame for each symbol in the current session.
#[derive(Clone, Default)]
pub struct DepthFrames(Arc<RwLock<HashMap<DepthSymbol, DepthFrame>>>);

impl DepthFrames {
    pub fn publish(&self, frame: DepthFrame) {
        self.0
            .write()
            .expect("depth frames")
            .insert(frame.symbol, frame);
    }

    pub fn latest(&self, symbol: DepthSymbol) -> Option<DepthFrame> {
        self.0.read().expect("depth frames").get(&symbol).cloned()
    }

    pub(super) fn clear(&self) {
        self.0.write().expect("depth frames").clear();
    }
}

impl DepthSelection for SubscriptionDemand {
    fn selection(&self) -> DepthFocus {
        DepthFocus::default().with_followed(self.depth_focus())
    }
}

pub(super) async fn drive_depth(
    session: &impl DepthSource,
    pacing: &RequestPacing,
    demand: watch::Receiver<SubscriptionDemand>,
    permissions: DepthPermissions,
    frames: &DepthFrames,
) -> Result<(), DepthError> {
    let source = PacedDepth {
        inner: session,
        pacing,
    };
    run_depth(&source, demand, permissions, DEFAULT_DEPTH_ROWS, |frame| {
        frames.publish(frame)
    })
    .await
}

/// Apply the owner's write budget to F9c, including resets and checked cancellation.
struct PacedDepth<'a, T> {
    inner: T,
    pacing: &'a RequestPacing,
}

impl RequestPacing {
    fn depth_result<T>(&self, result: Result<T, DepthError>) -> Result<T, DepthError> {
        if matches!(result, Err(DepthError::Pacing)) {
            self.note_pacing();
        }
        result
    }
}

#[async_trait]
impl<'a, S: DepthSource> DepthSource for PacedDepth<'a, &S> {
    type Subscription = PacedDepth<'a, S::Subscription>;

    async fn allocate_if_selected<T: Send>(
        &self,
        selected: impl FnOnce() -> bool + Send,
        request: impl Future<Output = Result<T, DepthError>> + Send,
    ) -> Result<Option<T>, DepthError> {
        let result = self.pacing.slot().await.run_if(selected, request).await;
        self.pacing.depth_result(result.transpose())
    }

    async fn confirm_depth(&self, symbol: DepthSymbol) -> Result<ConfirmedContract, DepthError> {
        self.inner.confirm_depth(symbol).await
    }

    async fn subscribe_depth(
        &self,
        request: &DepthRequest,
    ) -> Result<Self::Subscription, DepthError> {
        let inner = self.inner.subscribe_depth(request).await?;
        Ok(PacedDepth {
            inner,
            pacing: self.pacing,
        })
    }
}

#[async_trait]
impl<S: DepthSubscription> DepthSubscription for PacedDepth<'_, S> {
    fn request_id(&self) -> i32 {
        self.inner.request_id()
    }

    async fn next(&mut self) -> Result<Option<DepthUpdate>, DepthError> {
        self.pacing.depth_result(self.inner.next().await)
    }

    async fn cancel(&mut self) -> Result<(), DepthError> {
        let (result, _) = self.pacing.slot().await.run(self.inner.cancel()).await;
        self.pacing.depth_result(result)
    }
}

/// Independent depth quota. The depth adapter supplies eligible equity conIds
/// and applies the resulting allocation to its venue subscriptions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DepthBudget {
    pub pinned_slots: usize,
    pub page_slot: bool,
}

impl Default for DepthBudget {
    fn default() -> Self {
        Self {
            pinned_slots: 2,
            page_slot: true,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DepthAllocation {
    pub pinned: Vec<i32>,
    /// Additional depth slot reserved for the page, when needed.
    pub focus: Option<i32>,
}

impl DepthAllocation {
    pub fn active(&self) -> impl Iterator<Item = i32> + '_ {
        self.pinned.iter().copied().chain(self.focus)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DepthBudgetError {
    InvalidContractId,
    TooManyPinned,
}

impl fmt::Display for DepthBudgetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidContractId => "depth allocation requires positive equity conIds",
            Self::TooManyPinned => "pinned depth contracts exceed the configured depth quota",
        })
    }
}

impl Error for DepthBudgetError {}

impl DepthBudget {
    pub fn allocate(
        &self,
        pinned: &[i32],
        focus: Option<i32>,
    ) -> Result<DepthAllocation, DepthBudgetError> {
        if pinned.iter().any(|con_id| *con_id <= 0) || focus.is_some_and(|con_id| con_id <= 0) {
            return Err(DepthBudgetError::InvalidContractId);
        }
        let mut allocated = Vec::new();
        for &con_id in pinned {
            if allocated.contains(&con_id) {
                continue;
            }
            if allocated.len() == self.pinned_slots {
                return Err(DepthBudgetError::TooManyPinned);
            }
            allocated.push(con_id);
        }
        let focus = focus.filter(|con_id| self.page_slot && !allocated.contains(con_id));
        Ok(DepthAllocation {
            pinned: allocated,
            focus,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_depth_allocation_keeps_two_pins_and_moves_only_the_page_slot() {
        let budget = DepthBudget::default();
        let on_third = budget.allocate(&[11, 22], Some(33)).unwrap();
        let on_fourth = budget.allocate(&[11, 22], Some(44)).unwrap();
        assert_eq!(on_third.active().collect::<Vec<_>>(), [11, 22, 33]);
        assert_eq!(on_fourth.active().collect::<Vec<_>>(), [11, 22, 44]);
        assert_eq!(on_third.pinned, on_fourth.pinned);

        let on_pin = budget.allocate(&[11, 22, 11], Some(11)).unwrap();
        assert_eq!(on_pin.active().collect::<Vec<_>>(), [11, 22]);
        assert_eq!(on_pin.focus, None);
    }

    #[test]
    fn depth_quota_is_configurable_and_rejects_overcommitment() {
        let budget = DepthBudget {
            pinned_slots: 1,
            page_slot: false,
        };
        assert_eq!(budget.allocate(&[11], Some(22)).unwrap().focus, None);
        assert_eq!(
            budget.allocate(&[11, 22], None),
            Err(DepthBudgetError::TooManyPinned)
        );
        assert_eq!(
            budget.allocate(&[], Some(0)),
            Err(DepthBudgetError::InvalidContractId)
        );
    }
}
