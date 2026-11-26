use serde::Serialize;

pub const SOURCE: &str = "ibkr";
pub const PRODUCT: &str = "nasdaq-totalview";

/// Operator confirmations from the paper user's market data entitlement review.
/// Depth callbacks do not identify the individual account products.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct DepthPermissions {
    pub equity_l1_confirmed: bool,
    pub totalview_confirmed: bool,
    pub eds_confirmed: bool,
}

impl DepthPermissions {
    pub fn is_confirmed(self) -> bool {
        self.equity_l1_confirmed && self.totalview_confirmed && self.eds_confirmed
    }
}
