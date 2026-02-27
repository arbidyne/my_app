//! Order lifecycle state machine.
//!
//! Models the full IBKR order lifecycle as a finite state machine so invalid
//! transitions are caught at the boundary rather than propagating downstream.
//! The [`OrderStateMachine`] validates every transition, handles race conditions
//! (e.g. fills arriving while a cancel is pending), and logs state changes via
//! `tracing`.

use ibapi::orders::{OrderStatus as IbOrderStatus, PlaceOrder as IbPlaceOrder};
use tracing::{debug, error, info, warn};

/// Every state an order can occupy. Terminal states ([`Filled`], [`Cancelled`],
/// [`Rejected`]) reject all further events.
#[derive(Clone, Debug, PartialEq)]
pub enum OrderState {
    Unsent,
    PendingAck,
    Working,
    PartiallyFilled,
    AmendPending,
    CancelPending,
    Filled,
    Cancelled,
    Rejected { reason: String },
}

impl OrderState {
    /// Wire-format name sent to the frontend.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Unsent => "Unsent",
            Self::PendingAck => "PendingAck",
            Self::Working => "Working",
            Self::PartiallyFilled => "PartiallyFilled",
            Self::AmendPending => "AmendPending",
            Self::CancelPending => "CancelPending",
            Self::Filled => "Filled",
            Self::Cancelled => "Cancelled",
            Self::Rejected { .. } => "Rejected",
        }
    }

    fn is_terminal(&self) -> bool {
        matches!(self, Self::Filled | Self::Cancelled | Self::Rejected { .. })
    }
}

/// Events that drive state transitions, either from local actions (Submit,
/// CancelRequested) or mapped from IBKR callbacks.
#[derive(Clone, Debug)]
pub enum OrderEvent {
    Submit,
    AckValid,
    AckReject { reason: String },
    Fill {
        filled: f64,
        remaining: f64,
        avg_fill_price: f64,
    },
    CancelRequested,
    CancelConfirmed,
    CancelRejected,
    AmendRequested,
    AmendAccepted {
        filled: f64,
        remaining: f64,
        avg_fill_price: f64,
    },
    AmendRejected,
    /// IBKR's "PendingCancel" status, which may arrive from TWS-initiated cancels.
    PreCancel,
}

/// Tracks one order's lifecycle.
pub struct OrderStateMachine {
    pub order_id: i32,
    pub state: OrderState,
    /// Saved before entering CancelPending/AmendPending so we can revert on rejection.
    pre_pending_state: Option<OrderState>,
    pub filled: f64,
    pub remaining: f64,
    pub avg_fill_price: f64,
}

impl OrderStateMachine {
    pub fn new(order_id: i32, quantity: f64) -> Self {
        Self {
            order_id,
            state: OrderState::Unsent,
            pre_pending_state: None,
            filled: 0.0,
            remaining: quantity,
            avg_fill_price: 0.0,
        }
    }

    /// Apply an event, returning `Ok(())` on a valid transition or `Err` with
    /// a description when the transition is invalid. Invalid transitions are
    /// logged but never panic — callers can choose to ignore the error.
    pub fn apply(&mut self, event: OrderEvent) -> Result<(), String> {
        if self.state.is_terminal() {
            let msg = format!(
                "Order #{}: rejecting {:?} — already in terminal state {}",
                self.order_id,
                event,
                self.state.as_str()
            );
            error!("{msg}");
            return Err(msg);
        }

        match event {
            OrderEvent::Submit => self.on_submit(),
            OrderEvent::AckValid => self.on_ack_valid(),
            OrderEvent::AckReject { reason } => self.on_ack_reject(reason),
            OrderEvent::Fill {
                filled,
                remaining,
                avg_fill_price,
            } => self.on_fill(filled, remaining, avg_fill_price),
            OrderEvent::CancelRequested => self.on_cancel_requested(),
            OrderEvent::CancelConfirmed => self.on_cancel_confirmed(),
            OrderEvent::CancelRejected => self.on_cancel_rejected(),
            OrderEvent::AmendRequested => self.on_amend_requested(),
            OrderEvent::AmendAccepted {
                filled,
                remaining,
                avg_fill_price,
            } => self.on_amend_accepted(filled, remaining, avg_fill_price),
            OrderEvent::AmendRejected => self.on_amend_rejected(),
            OrderEvent::PreCancel => self.on_pre_cancel(),
        }
    }

    fn on_submit(&mut self) -> Result<(), String> {
        match self.state {
            OrderState::Unsent => {
                self.transition(OrderState::PendingAck);
                Ok(())
            }
            _ => self.invalid("Submit"),
        }
    }

    fn on_ack_valid(&mut self) -> Result<(), String> {
        match self.state {
            OrderState::PendingAck => {
                self.transition(OrderState::Working);
                Ok(())
            }
            _ => self.invalid("AckValid"),
        }
    }

    fn on_ack_reject(&mut self, reason: String) -> Result<(), String> {
        match self.state {
            OrderState::PendingAck | OrderState::Working | OrderState::PartiallyFilled => {
                self.transition(OrderState::Rejected { reason });
                Ok(())
            }
            _ => self.invalid("AckReject"),
        }
    }

    fn on_fill(
        &mut self,
        filled: f64,
        remaining: f64,
        avg_fill_price: f64,
    ) -> Result<(), String> {
        // Duplicate fill detection: if the new cumulative filled qty is not greater
        // than what we already have, skip silently.
        if filled <= self.filled && filled > 0.0 {
            debug!(
                "Order #{}: skipping duplicate fill (filled={filled}, already={})",
                self.order_id, self.filled
            );
            return Ok(());
        }

        match self.state {
            // Fill during PendingAck: treat as implicit ack + fill.
            OrderState::PendingAck => {
                self.update_fills(filled, remaining, avg_fill_price);
                if remaining <= 0.0 {
                    self.transition(OrderState::Filled);
                } else {
                    self.transition(OrderState::PartiallyFilled);
                }
                Ok(())
            }
            OrderState::Working | OrderState::PartiallyFilled => {
                self.update_fills(filled, remaining, avg_fill_price);
                if remaining <= 0.0 {
                    self.transition(OrderState::Filled);
                } else {
                    self.transition(OrderState::PartiallyFilled);
                }
                Ok(())
            }
            // Fill during CancelPending: update fills but stay CancelPending
            // unless fully filled.
            OrderState::CancelPending => {
                self.update_fills(filled, remaining, avg_fill_price);
                if remaining <= 0.0 {
                    self.transition(OrderState::Filled);
                }
                // else stay CancelPending
                Ok(())
            }
            // Fill during AmendPending: same pattern.
            OrderState::AmendPending => {
                self.update_fills(filled, remaining, avg_fill_price);
                if remaining <= 0.0 {
                    self.transition(OrderState::Filled);
                }
                // else stay AmendPending
                Ok(())
            }
            _ => self.invalid("Fill"),
        }
    }

    fn on_cancel_requested(&mut self) -> Result<(), String> {
        match self.state {
            OrderState::Working | OrderState::PartiallyFilled => {
                self.pre_pending_state = Some(self.state.clone());
                self.transition(OrderState::CancelPending);
                Ok(())
            }
            OrderState::PendingAck => {
                self.pre_pending_state = Some(self.state.clone());
                self.transition(OrderState::CancelPending);
                Ok(())
            }
            _ => self.invalid("CancelRequested"),
        }
    }

    fn on_cancel_confirmed(&mut self) -> Result<(), String> {
        match self.state {
            OrderState::CancelPending | OrderState::Working | OrderState::PartiallyFilled | OrderState::PendingAck => {
                self.transition(OrderState::Cancelled);
                Ok(())
            }
            _ => self.invalid("CancelConfirmed"),
        }
    }

    fn on_cancel_rejected(&mut self) -> Result<(), String> {
        match self.state {
            OrderState::CancelPending => {
                if let Some(prev) = self.pre_pending_state.take() {
                    self.transition(prev);
                } else {
                    self.transition(OrderState::Working);
                }
                Ok(())
            }
            _ => self.invalid("CancelRejected"),
        }
    }

    fn on_amend_requested(&mut self) -> Result<(), String> {
        match self.state {
            OrderState::Working | OrderState::PartiallyFilled => {
                self.pre_pending_state = Some(self.state.clone());
                self.transition(OrderState::AmendPending);
                Ok(())
            }
            _ => self.invalid("AmendRequested"),
        }
    }

    fn on_amend_accepted(
        &mut self,
        filled: f64,
        remaining: f64,
        avg_fill_price: f64,
    ) -> Result<(), String> {
        match self.state {
            OrderState::AmendPending => {
                self.update_fills(filled, remaining, avg_fill_price);
                if remaining <= 0.0 {
                    self.transition(OrderState::Filled);
                } else if filled > 0.0 {
                    self.transition(OrderState::PartiallyFilled);
                } else {
                    self.transition(OrderState::Working);
                }
                Ok(())
            }
            _ => self.invalid("AmendAccepted"),
        }
    }

    fn on_amend_rejected(&mut self) -> Result<(), String> {
        match self.state {
            OrderState::AmendPending => {
                if let Some(prev) = self.pre_pending_state.take() {
                    self.transition(prev);
                } else {
                    self.transition(OrderState::Working);
                }
                Ok(())
            }
            _ => self.invalid("AmendRejected"),
        }
    }

    fn on_pre_cancel(&mut self) -> Result<(), String> {
        match self.state {
            OrderState::Working | OrderState::PartiallyFilled | OrderState::PendingAck => {
                self.pre_pending_state = Some(self.state.clone());
                self.transition(OrderState::CancelPending);
                Ok(())
            }
            // Already in CancelPending — harmless duplicate.
            OrderState::CancelPending => Ok(()),
            _ => self.invalid("PreCancel"),
        }
    }

    fn transition(&mut self, new_state: OrderState) {
        info!(
            "Order #{}: {} → {}",
            self.order_id,
            self.state.as_str(),
            new_state.as_str()
        );
        self.state = new_state;
    }

    fn update_fills(&mut self, filled: f64, remaining: f64, avg_fill_price: f64) {
        self.filled = filled;
        self.remaining = remaining;
        self.avg_fill_price = avg_fill_price;
    }

    fn invalid(&self, event_name: &str) -> Result<(), String> {
        let msg = format!(
            "Order #{}: invalid event {event_name} in state {}",
            self.order_id,
            self.state.as_str()
        );
        warn!("{msg}");
        Err(msg)
    }
}

// ---------------------------------------------------------------------------
// IBKR event mapper
// ---------------------------------------------------------------------------

/// Maps an IBKR [`PlaceOrder`] callback into an [`OrderEvent`], or `None` for
/// events that are logged separately (commissions, open-order echoes, notices).
pub fn map_ibkr_to_event(event: &IbPlaceOrder) -> Option<OrderEvent> {
    match event {
        IbPlaceOrder::OrderStatus(IbOrderStatus {
            status,
            filled,
            remaining,
            average_fill_price,
            ..
        }) => map_status(status, *filled, *remaining, *average_fill_price),
        IbPlaceOrder::ExecutionData(exec) => Some(OrderEvent::Fill {
            filled: exec.execution.cumulative_quantity,
            remaining: 0.0, // IBKR doesn't include remaining in executions; OrderStatus carries it.
            avg_fill_price: exec.execution.average_price,
        }),
        // Logged separately by the monitoring task.
        IbPlaceOrder::CommissionReport(_)
        | IbPlaceOrder::OpenOrder(_)
        | IbPlaceOrder::Message(_) => None,
    }
}

fn map_status(
    status: &str,
    filled: f64,
    remaining: f64,
    avg_fill_price: f64,
) -> Option<OrderEvent> {
    match status {
        // We set PendingAck locally on submit — these IBKR echoes are redundant.
        "PendingSubmit" | "ApiPending" => None,

        "PreSubmitted" | "Submitted" => {
            if filled > 0.0 {
                Some(OrderEvent::Fill {
                    filled,
                    remaining,
                    avg_fill_price,
                })
            } else {
                Some(OrderEvent::AckValid)
            }
        }

        "Filled" => Some(OrderEvent::Fill {
            filled,
            remaining,
            avg_fill_price,
        }),

        "Cancelled" | "ApiCancelled" => Some(OrderEvent::CancelConfirmed),

        "Inactive" => Some(OrderEvent::AckReject {
            reason: "Inactive".to_string(),
        }),

        "PendingCancel" => Some(OrderEvent::PreCancel),

        other => {
            warn!("Unmapped IBKR status: {other}");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sm(order_id: i32, qty: f64) -> OrderStateMachine {
        OrderStateMachine::new(order_id, qty)
    }

    // Helper: advance to Working state.
    fn working(order_id: i32, qty: f64) -> OrderStateMachine {
        let mut m = sm(order_id, qty);
        m.apply(OrderEvent::Submit).unwrap();
        m.apply(OrderEvent::AckValid).unwrap();
        m
    }

    #[test]
    fn happy_path_unsent_to_filled() {
        let mut m = sm(1, 100.0);
        assert_eq!(m.state.as_str(), "Unsent");

        m.apply(OrderEvent::Submit).unwrap();
        assert_eq!(m.state.as_str(), "PendingAck");

        m.apply(OrderEvent::AckValid).unwrap();
        assert_eq!(m.state.as_str(), "Working");

        m.apply(OrderEvent::Fill {
            filled: 50.0,
            remaining: 50.0,
            avg_fill_price: 10.0,
        })
        .unwrap();
        assert_eq!(m.state.as_str(), "PartiallyFilled");
        assert_eq!(m.filled, 50.0);

        m.apply(OrderEvent::Fill {
            filled: 100.0,
            remaining: 0.0,
            avg_fill_price: 10.05,
        })
        .unwrap();
        assert_eq!(m.state.as_str(), "Filled");
        assert_eq!(m.filled, 100.0);
        assert_eq!(m.avg_fill_price, 10.05);
    }

    #[test]
    fn immediate_fill() {
        let mut m = sm(2, 50.0);
        m.apply(OrderEvent::Submit).unwrap();
        // Fill arrives before AckValid (market order race).
        m.apply(OrderEvent::Fill {
            filled: 50.0,
            remaining: 0.0,
            avg_fill_price: 20.0,
        })
        .unwrap();
        assert_eq!(m.state.as_str(), "Filled");
    }

    #[test]
    fn rejection() {
        let mut m = sm(3, 100.0);
        m.apply(OrderEvent::Submit).unwrap();
        m.apply(OrderEvent::AckReject {
            reason: "Insufficient margin".to_string(),
        })
        .unwrap();
        assert_eq!(m.state, OrderState::Rejected { reason: "Insufficient margin".to_string() });
    }

    #[test]
    fn cancel_flow() {
        let mut m = working(4, 100.0);
        m.apply(OrderEvent::CancelRequested).unwrap();
        assert_eq!(m.state.as_str(), "CancelPending");
        m.apply(OrderEvent::CancelConfirmed).unwrap();
        assert_eq!(m.state.as_str(), "Cancelled");
    }

    #[test]
    fn cancel_rejected_reverts() {
        let mut m = working(5, 100.0);
        m.apply(OrderEvent::CancelRequested).unwrap();
        assert_eq!(m.state.as_str(), "CancelPending");
        m.apply(OrderEvent::CancelRejected).unwrap();
        assert_eq!(m.state.as_str(), "Working");
    }

    #[test]
    fn fill_during_cancel() {
        let mut m = working(6, 100.0);
        m.apply(OrderEvent::CancelRequested).unwrap();

        // Partial fill while cancel is pending.
        m.apply(OrderEvent::Fill {
            filled: 30.0,
            remaining: 70.0,
            avg_fill_price: 15.0,
        })
        .unwrap();
        assert_eq!(m.state.as_str(), "CancelPending");
        assert_eq!(m.filled, 30.0);

        // Cancel confirms — now cancelled with partial fill.
        m.apply(OrderEvent::CancelConfirmed).unwrap();
        assert_eq!(m.state.as_str(), "Cancelled");
    }

    #[test]
    fn fill_completes_during_cancel() {
        let mut m = working(7, 100.0);
        m.apply(OrderEvent::CancelRequested).unwrap();

        // Full fill arrives before cancel confirms — fill wins.
        m.apply(OrderEvent::Fill {
            filled: 100.0,
            remaining: 0.0,
            avg_fill_price: 15.0,
        })
        .unwrap();
        assert_eq!(m.state.as_str(), "Filled");
    }

    #[test]
    fn amend_flow() {
        let mut m = working(8, 100.0);
        m.apply(OrderEvent::AmendRequested).unwrap();
        assert_eq!(m.state.as_str(), "AmendPending");
        m.apply(OrderEvent::AmendAccepted {
            filled: 0.0,
            remaining: 100.0,
            avg_fill_price: 0.0,
        })
        .unwrap();
        assert_eq!(m.state.as_str(), "Working");
    }

    #[test]
    fn amend_rejected_reverts() {
        let mut m = working(9, 100.0);
        // Partially fill first.
        m.apply(OrderEvent::Fill {
            filled: 20.0,
            remaining: 80.0,
            avg_fill_price: 10.0,
        })
        .unwrap();
        assert_eq!(m.state.as_str(), "PartiallyFilled");

        m.apply(OrderEvent::AmendRequested).unwrap();
        assert_eq!(m.state.as_str(), "AmendPending");

        m.apply(OrderEvent::AmendRejected).unwrap();
        assert_eq!(m.state.as_str(), "PartiallyFilled");
    }

    #[test]
    fn terminal_state_rejects_events() {
        let mut m = working(10, 100.0);
        m.apply(OrderEvent::Fill {
            filled: 100.0,
            remaining: 0.0,
            avg_fill_price: 10.0,
        })
        .unwrap();
        assert_eq!(m.state.as_str(), "Filled");

        // All further events should be rejected.
        assert!(m.apply(OrderEvent::AckValid).is_err());
        assert!(m
            .apply(OrderEvent::Fill {
                filled: 100.0,
                remaining: 0.0,
                avg_fill_price: 10.0,
            })
            .is_err());
        assert!(m.apply(OrderEvent::CancelRequested).is_err());
    }

    #[test]
    fn duplicate_fill_skipped() {
        let mut m = working(11, 100.0);
        m.apply(OrderEvent::Fill {
            filled: 50.0,
            remaining: 50.0,
            avg_fill_price: 10.0,
        })
        .unwrap();
        assert_eq!(m.state.as_str(), "PartiallyFilled");
        assert_eq!(m.filled, 50.0);

        // Same fill again — should be silently skipped.
        m.apply(OrderEvent::Fill {
            filled: 50.0,
            remaining: 50.0,
            avg_fill_price: 10.0,
        })
        .unwrap();
        assert_eq!(m.state.as_str(), "PartiallyFilled");
        assert_eq!(m.filled, 50.0);
    }

    #[test]
    fn pre_cancel_from_tws() {
        let mut m = working(12, 100.0);
        m.apply(OrderEvent::PreCancel).unwrap();
        assert_eq!(m.state.as_str(), "CancelPending");

        // Duplicate PreCancel is harmless.
        m.apply(OrderEvent::PreCancel).unwrap();
        assert_eq!(m.state.as_str(), "CancelPending");

        m.apply(OrderEvent::CancelConfirmed).unwrap();
        assert_eq!(m.state.as_str(), "Cancelled");
    }

    #[test]
    fn map_ibkr_pending_submit_returns_none() {
        let status = IbOrderStatus {
            order_id: 1,
            status: "PendingSubmit".to_string(),
            filled: 0.0,
            remaining: 100.0,
            average_fill_price: 0.0,
            perm_id: 0,
            parent_id: 0,
            last_fill_price: 0.0,
            client_id: 0,
            why_held: String::new(),
            market_cap_price: 0.0,
        };
        let event = IbPlaceOrder::OrderStatus(status);
        assert!(map_ibkr_to_event(&event).is_none());
    }

    #[test]
    fn map_ibkr_submitted_no_fill() {
        let status = IbOrderStatus {
            order_id: 1,
            status: "Submitted".to_string(),
            filled: 0.0,
            remaining: 100.0,
            average_fill_price: 0.0,
            perm_id: 0,
            parent_id: 0,
            last_fill_price: 0.0,
            client_id: 0,
            why_held: String::new(),
            market_cap_price: 0.0,
        };
        let event = IbPlaceOrder::OrderStatus(status);
        let mapped = map_ibkr_to_event(&event).unwrap();
        assert!(matches!(mapped, OrderEvent::AckValid));
    }

    #[test]
    fn map_ibkr_submitted_with_fill() {
        let status = IbOrderStatus {
            order_id: 1,
            status: "Submitted".to_string(),
            filled: 50.0,
            remaining: 50.0,
            average_fill_price: 10.0,
            perm_id: 0,
            parent_id: 0,
            last_fill_price: 0.0,
            client_id: 0,
            why_held: String::new(),
            market_cap_price: 0.0,
        };
        let event = IbPlaceOrder::OrderStatus(status);
        let mapped = map_ibkr_to_event(&event).unwrap();
        assert!(matches!(mapped, OrderEvent::Fill { .. }));
    }

    #[test]
    fn map_ibkr_cancelled() {
        let status = IbOrderStatus {
            order_id: 1,
            status: "Cancelled".to_string(),
            filled: 0.0,
            remaining: 100.0,
            average_fill_price: 0.0,
            perm_id: 0,
            parent_id: 0,
            last_fill_price: 0.0,
            client_id: 0,
            why_held: String::new(),
            market_cap_price: 0.0,
        };
        let event = IbPlaceOrder::OrderStatus(status);
        let mapped = map_ibkr_to_event(&event).unwrap();
        assert!(matches!(mapped, OrderEvent::CancelConfirmed));
    }

    #[test]
    fn map_ibkr_inactive() {
        let status = IbOrderStatus {
            order_id: 1,
            status: "Inactive".to_string(),
            filled: 0.0,
            remaining: 100.0,
            average_fill_price: 0.0,
            perm_id: 0,
            parent_id: 0,
            last_fill_price: 0.0,
            client_id: 0,
            why_held: String::new(),
            market_cap_price: 0.0,
        };
        let event = IbPlaceOrder::OrderStatus(status);
        let mapped = map_ibkr_to_event(&event).unwrap();
        assert!(matches!(mapped, OrderEvent::AckReject { .. }));
    }
}
