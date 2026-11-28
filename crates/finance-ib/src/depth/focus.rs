use std::future::pending;

use chrono::Utc;
use rust_decimal::Decimal;
use tokio::sync::watch;

use super::{
    DepthError, DepthFrame, DepthLevel, DepthOperation, DepthPermissions, DepthRequest, DepthSide,
    DepthSource, DepthState, DepthSubscription, DepthSymbol, DepthUpdate, MAX_DEPTH_ROWS,
    MAX_DEPTH_SUBSCRIPTIONS, PINNED_DEPTH_SLOTS,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DepthFocus {
    pinned: [Option<DepthSymbol>; PINNED_DEPTH_SLOTS],
    followed: Option<DepthSymbol>,
}

impl DepthFocus {
    pub fn new(pinned: &[DepthSymbol], followed: Option<DepthSymbol>) -> Result<Self, DepthError> {
        let pinned = match pinned {
            [] => [None, None],
            [first] => [Some(*first), None],
            [first, second] if first != second => [Some(*first), Some(*second)],
            _ => return Err(DepthError::InvalidFocus),
        };
        Ok(Self { pinned, followed })
    }

    /// The page can follow a pinned symbol without acquiring another line.
    pub fn with_followed(self, followed: Option<DepthSymbol>) -> Self {
        Self { followed, ..self }
    }

    fn desired(self) -> impl Iterator<Item = DepthSymbol> {
        let followed = self
            .followed
            .filter(|symbol| !self.pinned.contains(&Some(*symbol)));
        self.pinned.into_iter().chain([followed]).flatten()
    }

    fn contains(self, symbol: DepthSymbol) -> bool {
        self.followed == Some(symbol) || self.pinned.contains(&Some(symbol))
    }
}

impl Default for DepthFocus {
    fn default() -> Self {
        Self {
            pinned: [Some(DepthSymbol::Qqq), Some(DepthSymbol::Aapl)],
            followed: None,
        }
    }
}

/// A watch value from which the depth owner reads its pinned and followed symbols.
pub trait DepthSelection: Send + Sync {
    fn selection(&self) -> DepthFocus;
}

impl DepthSelection for DepthFocus {
    fn selection(&self) -> DepthFocus {
        *self
    }
}

fn selected(focus: &watch::Receiver<impl DepthSelection>, symbol: DepthSymbol) -> bool {
    focus.has_changed().is_ok() && focus.borrow().selection().contains(symbol)
}

struct Active<S> {
    request: DepthRequest,
    subscription: S,
}

struct Line<S> {
    frame: DepthFrame,
    active: Option<Active<S>>,
    recovering: bool,
}

impl<S: DepthSubscription> Line<S> {
    fn new(symbol: DepthSymbol, permissions: DepthPermissions, requested_rows: u32) -> Self {
        Self {
            frame: DepthFrame {
                symbol,
                permissions,
                requested_rows,
                state: if permissions.is_confirmed() {
                    DepthState::Awaiting
                } else {
                    DepthState::PermissionUnconfirmed
                },
                request_id: None,
                received_at: None,
                bids: Vec::new(),
                asks: Vec::new(),
            },
            active: None,
            recovering: false,
        }
    }

    fn clear(&mut self, state: DepthState, publish: &mut impl FnMut(DepthFrame)) {
        self.frame.state = state;
        self.frame.bids.clear();
        self.frame.asks.clear();
        self.frame.received_at = None;
        publish(self.frame.clone());
        self.frame.request_id = None;
    }

    async fn subscribe(
        &mut self,
        source: &impl DepthSource<Subscription = S>,
        focus: &watch::Receiver<impl DepthSelection>,
        request: DepthRequest,
        publish: &mut impl FnMut(DepthFrame),
    ) -> Result<(), DepthError> {
        let symbol = self.frame.symbol;
        match source
            .allocate_if_selected(|| selected(focus, symbol), source.subscribe_depth(&request))
            .await
        {
            Ok(Some(subscription)) => {
                self.frame.state = DepthState::Awaiting;
                self.frame.request_id = Some(subscription.request_id());
                self.active = Some(Active {
                    request,
                    subscription,
                });
                publish(self.frame.clone());
                Ok(())
            }
            Ok(None) => Ok(()),
            Err(error @ (DepthError::Timeout | DepthError::Transport)) => {
                // A failed write may have reached the Gateway. Stop this
                // session owner rather than risk allocating another line.
                self.clear(DepthState::Failed(error), publish);
                Err(error)
            }
            Err(error) => self.fail(error, publish).await,
        }
    }

    async fn fail(
        &mut self,
        error: DepthError,
        publish: &mut impl FnMut(DepthFrame),
    ) -> Result<(), DepthError> {
        self.clear(DepthState::Failed(error), publish);
        self.cancel().await?;
        if matches!(error, DepthError::Disconnected | DepthError::CancelFailed) {
            return Err(error);
        }
        Ok(())
    }

    async fn start(
        &mut self,
        source: &impl DepthSource<Subscription = S>,
        focus: &watch::Receiver<impl DepthSelection>,
        publish: &mut impl FnMut(DepthFrame),
    ) -> Result<(), DepthError> {
        publish(self.frame.clone());
        if !self.frame.permissions.is_confirmed() {
            return Ok(());
        }
        let symbol = self.frame.symbol;
        let rows = self.frame.requested_rows;
        let request = source
            .allocate_if_selected(|| selected(focus, symbol), async {
                DepthRequest::new(symbol, source.confirm_depth(symbol).await?, rows)
            })
            .await;
        match request {
            Ok(Some(request)) => self.subscribe(source, focus, request, publish).await,
            Ok(None) => Ok(()),
            Err(error) => self.fail(error, publish).await,
        }
    }

    async fn cancel(&mut self) -> Result<(), DepthError> {
        if let Some(Active {
            mut subscription, ..
        }) = self.active.take()
        {
            cancel(&mut subscription).await?;
        }
        Ok(())
    }

    async fn reset(
        &mut self,
        source: &impl DepthSource<Subscription = S>,
        focus: &watch::Receiver<impl DepthSelection>,
        publish: &mut impl FnMut(DepthFrame),
    ) -> Result<(), DepthError> {
        self.clear(DepthState::Reset, publish);
        let Some(Active {
            request,
            mut subscription,
        }) = self.active.take()
        else {
            return Ok(());
        };
        cancel(&mut subscription).await?;
        if !selected(focus, self.frame.symbol) {
            return Ok(());
        }
        if self.recovering {
            // A second reset before any valid row is a failed recovery. Wait
            // for a new selection or session instead of reopening in a loop.
            return self.fail(DepthError::InvalidCallback, publish).await;
        }
        self.subscribe(source, focus, request, publish).await?;
        self.recovering = self.active.is_some();
        Ok(())
    }

    async fn update(
        &mut self,
        source: &impl DepthSource<Subscription = S>,
        focus: &watch::Receiver<impl DepthSelection>,
        result: Result<Option<DepthUpdate>, DepthError>,
        publish: &mut impl FnMut(DepthFrame),
    ) -> Result<(), DepthError> {
        match result {
            Ok(Some(DepthUpdate::Row {
                side,
                operation,
                position,
                level,
            })) => {
                let rows = match side {
                    DepthSide::Bid => &mut self.frame.bids,
                    DepthSide::Ask => &mut self.frame.asks,
                };
                if !apply_row(
                    rows,
                    self.frame.requested_rows as usize,
                    position as usize,
                    operation,
                    level,
                ) {
                    return self.reset(source, focus, publish).await;
                }
                self.recovering = false;
                self.frame.state = DepthState::Receiving;
                self.frame.received_at = Some(Utc::now());
                publish(self.frame.clone());
                Ok(())
            }
            Ok(Some(DepthUpdate::Reset)) | Err(DepthError::InvalidCallback) => {
                self.reset(source, focus, publish).await
            }
            Ok(None) => self.fail(DepthError::StreamEnded, publish).await,
            Err(error) => self.fail(error, publish).await,
        }
    }
}

async fn cancel(subscription: &mut impl DepthSubscription) -> Result<(), DepthError> {
    subscription.cancel().await.map_err(|error| match error {
        DepthError::Disconnected => error,
        _ => DepthError::CancelFailed,
    })
}

fn apply_row(
    rows: &mut Vec<DepthLevel>,
    max_rows: usize,
    position: usize,
    operation: DepthOperation,
    level: Option<DepthLevel>,
) -> bool {
    if position >= max_rows
        || level.as_ref().is_some_and(|level| {
            level.price.value() <= Decimal::ZERO || level.size.value() <= Decimal::ZERO
        })
    {
        return false;
    }
    match (operation, level) {
        (DepthOperation::Insert, Some(level)) if position <= rows.len() => {
            rows.insert(position, level);
            rows.truncate(max_rows);
        }
        (DepthOperation::Update, Some(level)) if position < rows.len() => rows[position] = level,
        (DepthOperation::Delete, None) if position < rows.len() => {
            rows.remove(position);
        }
        _ => return false,
    }
    true
}

async fn next_update<S: DepthSubscription>(
    line: &mut Option<Line<S>>,
) -> Result<Option<DepthUpdate>, DepthError> {
    match line.as_mut().and_then(|line| line.active.as_mut()) {
        Some(active) => active.subscription.next().await,
        None => pending().await,
    }
}

async fn reconcile<S: DepthSource>(
    source: &S,
    lines: &mut [Option<Line<S::Subscription>>; MAX_DEPTH_SUBSCRIPTIONS],
    focus: &mut watch::Receiver<impl DepthSelection>,
    permissions: DepthPermissions,
    requested_rows: u32,
    publish: &mut impl FnMut(DepthFrame),
) -> Result<(), DepthError> {
    let desired = focus.borrow_and_update().selection();
    for line in lines.iter_mut() {
        if line
            .as_ref()
            .is_some_and(|line| !desired.contains(line.frame.symbol))
        {
            let mut removed = line.take().expect("existing line");
            removed.clear(DepthState::Inactive, publish);
            removed.cancel().await?;
        }
    }
    for symbol in desired.desired() {
        if !selected(focus, symbol) {
            continue;
        }
        if let Some(line) = lines
            .iter_mut()
            .flatten()
            .find(|line| line.frame.symbol == symbol)
        {
            if line.active.is_none()
                && matches!(line.frame.state, DepthState::Reset | DepthState::Awaiting)
            {
                // The symbol can be reselected after a reset or skipped allocation.
                line.frame.state = DepthState::Awaiting;
                line.start(source, focus, publish).await?;
            }
            continue;
        }
        let slot = lines
            .iter_mut()
            .find(|line| line.is_none())
            .expect("focus bounded by slot count");
        let line = slot.insert(Line::new(symbol, permissions, requested_rows));
        line.start(source, focus, publish).await?;
    }
    Ok(())
}

enum Work {
    FocusChanged,
    Shutdown,
    Update(usize, Result<Option<DepthUpdate>, DepthError>),
}

/// Serve one verified paper session for as long as the focus sender exists.
/// Publish receives each new display state synchronously, including the empty
/// reset before any cancel/reopen. The owning scheduler must retire this loop
/// before starting a replacement session. Use sender closure for shutdown.
pub async fn run_depth<S: DepthSource>(
    source: &S,
    mut focus: watch::Receiver<impl DepthSelection>,
    permissions: DepthPermissions,
    requested_rows: u32,
    mut publish: impl FnMut(DepthFrame),
) -> Result<(), DepthError> {
    if !(1..=MAX_DEPTH_ROWS).contains(&requested_rows) {
        return Err(DepthError::InvalidRequest);
    }
    let mut lines = [None, None, None];
    let mut work = Work::FocusChanged;
    let outcome = loop {
        let result = match work {
            Work::FocusChanged if focus.has_changed().is_err() => break Ok(()),
            Work::FocusChanged => {
                reconcile(
                    source,
                    &mut lines,
                    &mut focus,
                    permissions,
                    requested_rows,
                    &mut publish,
                )
                .await
            }
            Work::Shutdown => break Ok(()),
            Work::Update(_, Err(DepthError::Disconnected)) => break Err(DepthError::Disconnected),
            Work::Update(index, result) => {
                lines[index]
                    .as_mut()
                    .expect("ready line")
                    .update(source, &focus, result, &mut publish)
                    .await
            }
        };
        if let Err(error) = result {
            break Err(error);
        }
        let [first, second, third] = &mut lines;
        work = tokio::select! {
            change = focus.changed() => if change.is_ok() { Work::FocusChanged } else { Work::Shutdown },
            event = next_update(first) => Work::Update(0, event),
            event = next_update(second) => Work::Update(1, event),
            event = next_update(third) => Work::Update(2, event),
        };
    };
    let state = outcome
        .err()
        .map_or(DepthState::Inactive, DepthState::Failed);
    for line in lines.iter_mut().flatten() {
        if line.frame.state != state {
            line.clear(state, &mut publish);
        }
    }
    let mut result = outcome;
    for line in lines.iter_mut().flatten() {
        result = result.and(line.cancel().await);
    }
    result
}

#[cfg(test)]
mod tests;
