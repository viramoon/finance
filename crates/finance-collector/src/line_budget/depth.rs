use std::{error::Error, fmt};

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
