//! `CexStub` — an in-process fake `CexExecutor`, same shape and
//! call-recording requirement as `DexStub` (`SPEC.md` §6).
//!
//! **Reduce-only mode.** Once a test sets a signed position for a symbol
//! with [`CexStub::set_position`], the stub behaves like a venue that holds
//! that position: it rejects any `reduce_only` order that would increase or
//! flip it, and moves the position by every fill it returns for that
//! symbol. Symbols with no position set are not checked. A reduce-only
//! order larger than the position is rejected outright: what Binance does
//! with one (reject it, or fill up to the position and expire the rest) is
//! recorded by `IMPLEMENTATION_PLAN.md` Phase 12's testnet run, and the
//! stub copies that once it is known.

use crate::cex::{CexExecutor, CexFill, OrderRequest, OrderSide, OrderStateUnknown};
use crate::Provenance;
use anyhow::{bail, Result};
use async_trait::async_trait;
use rust_decimal::Decimal;
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

/// One `execute()` call exactly as received by the stub — the request it
/// was given, `reduce_only` included — so a test can assert on what was
/// actually sent, not just on what came back.
#[derive(Debug, Clone)]
pub struct RecordedCall {
    pub request: OrderRequest,
}

/// One programmed outcome. `StateUnknown` is completed with the request's
/// own symbol when it is returned.
enum Programmed {
    Result(Result<CexFill>),
    StateUnknown {
        client_order_id: String,
        order_ref: Option<u64>,
    },
}

#[derive(Default)]
pub struct CexStub {
    programmed: Mutex<VecDeque<Programmed>>,
    calls: Mutex<Vec<RecordedCall>>,
    positions: Mutex<HashMap<String, Decimal>>,
}

impl CexStub {
    pub fn new() -> Self {
        Self::default()
    }

    /// Program the exact result the next `execute()` call returns.
    /// Consumed in the order programmed; once exhausted, `execute()` falls
    /// back to a full fill at the request's own `quantity`/`quoted_price`,
    /// so a test that doesn't care about the outcome doesn't have to
    /// program one.
    pub fn program_execute(&self, result: Result<CexFill>) {
        self.programmed
            .lock()
            .unwrap()
            .push_back(Programmed::Result(result));
    }

    /// Program a fill — full or partial, depending on `filled_qty`.
    pub fn program_fill(
        &self,
        filled_qty: Decimal,
        filled_price: Decimal,
        commission: Decimal,
        commission_asset: impl Into<String>,
    ) {
        self.program_execute(Ok(CexFill {
            filled_qty,
            filled_price,
            commission,
            commission_asset: commission_asset.into(),
            provenance: Provenance::Simulated,
            order_ref: None,
        }));
    }

    /// Program a rejection — the venue refused the order outright, and
    /// nothing filled.
    pub fn program_rejected(&self, reason: impl std::fmt::Display) {
        self.program_execute(Err(anyhow::anyhow!("order rejected: {reason}")));
    }

    /// Program an [`OrderStateUnknown`] for the next `execute()` call: the
    /// order may have filled, and the caller must find out before acting on
    /// the symbol again. Its `symbol` is the request's own.
    pub fn program_state_unknown(&self, client_order_id: impl Into<String>) {
        self.programmed
            .lock()
            .unwrap()
            .push_back(Programmed::StateUnknown {
                client_order_id: client_order_id.into(),
                order_ref: None,
            });
    }

    /// Sets the signed position (negative is short) the stub holds for
    /// `symbol`, which switches on reduce-only checking for it — see the
    /// module docs.
    pub fn set_position(&self, symbol: impl Into<String>, qty: Decimal) {
        self.positions.lock().unwrap().insert(symbol.into(), qty);
    }

    /// The position the stub holds for `symbol`, if one was set.
    pub fn position(&self, symbol: &str) -> Option<Decimal> {
        self.positions.lock().unwrap().get(symbol).copied()
    }

    /// Every call the stub has received so far, in the order received.
    pub fn calls(&self) -> Vec<RecordedCall> {
        self.calls.lock().unwrap().clone()
    }

    fn check_reduce_only(&self, req: &OrderRequest) -> Result<()> {
        if !req.reduce_only {
            return Ok(());
        }
        let Some(position) = self.position(&req.symbol) else {
            return Ok(());
        };
        let reducible = match req.side {
            OrderSide::Buy => -position,
            OrderSide::Sell => position,
        };
        if reducible <= Decimal::ZERO || req.quantity > reducible {
            bail!(
                "order rejected: reduce-only {:?} of {} {} would increase or flip the position of {position}",
                req.side,
                req.quantity,
                req.symbol
            );
        }
        Ok(())
    }

    fn apply_fill(&self, req: &OrderRequest, fill: &CexFill) {
        if let Some(position) = self.positions.lock().unwrap().get_mut(&req.symbol) {
            match req.side {
                OrderSide::Buy => *position += fill.filled_qty,
                OrderSide::Sell => *position -= fill.filled_qty,
            }
        }
    }
}

#[async_trait]
impl CexExecutor for CexStub {
    async fn execute(&self, req: &OrderRequest) -> Result<CexFill> {
        self.calls.lock().unwrap().push(RecordedCall {
            request: req.clone(),
        });
        self.check_reduce_only(req)?;

        let programmed = self.programmed.lock().unwrap().pop_front();
        let result = match programmed {
            Some(Programmed::Result(result)) => result,
            Some(Programmed::StateUnknown {
                client_order_id,
                order_ref,
            }) => Err(OrderStateUnknown {
                symbol: req.symbol.clone(),
                client_order_id,
                order_ref,
            }
            .into()),
            None => Ok(CexFill {
                filled_qty: req.quantity,
                filled_price: req.quoted_price,
                commission: Decimal::ZERO,
                commission_asset: String::new(),
                provenance: Provenance::Simulated,
                order_ref: None,
            }),
        };
        if let Ok(fill) = &result {
            self.apply_fill(req, fill);
        }
        result
    }

    fn label(&self) -> &'static str {
        "cex-stub"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cex::OrderSide;
    use std::str::FromStr;

    fn decimal(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    fn request() -> OrderRequest {
        OrderRequest {
            symbol: "SOLUSDT".to_string(),
            side: OrderSide::Buy,
            quantity: decimal("10"),
            quoted_price: decimal("150"),
            reduce_only: false,
        }
    }

    fn reduce_only(side: OrderSide, quantity: &str) -> OrderRequest {
        OrderRequest {
            side,
            quantity: decimal(quantity),
            reduce_only: true,
            ..request()
        }
    }

    #[tokio::test]
    async fn default_execute_fills_in_full_at_the_quoted_price() {
        let stub = CexStub::new();
        let fill = stub.execute(&request()).await.unwrap();

        assert_eq!(fill.filled_qty, decimal("10"));
        assert_eq!(fill.filled_price, decimal("150"));
        assert_eq!(fill.provenance, Provenance::Simulated);
        assert_eq!(fill.order_ref, None);
    }

    #[tokio::test]
    async fn programmed_partial_fill_carries_a_smaller_quantity() {
        let stub = CexStub::new();
        stub.program_fill(decimal("4"), decimal("151"), decimal("0.05"), "USDT");

        let fill = stub.execute(&request()).await.unwrap();

        assert_eq!(fill.filled_qty, decimal("4"));
        assert_eq!(fill.filled_price, decimal("151"));
        assert_eq!(fill.commission, decimal("0.05"));
        assert_eq!(fill.commission_asset, "USDT");
    }

    #[tokio::test]
    async fn programmed_rejection_is_an_error() {
        let stub = CexStub::new();
        stub.program_rejected("insufficient balance");

        let err = stub.execute(&request()).await.unwrap_err();

        assert!(err.to_string().contains("insufficient balance"));
        assert!(err.downcast_ref::<OrderStateUnknown>().is_none());
    }

    #[tokio::test]
    async fn programmed_state_unknown_is_found_with_downcast_ref() {
        let stub = CexStub::new();
        stub.program_state_unknown("vp-1");

        let err = stub.execute(&request()).await.unwrap_err();
        let unknown = err
            .downcast_ref::<OrderStateUnknown>()
            .expect("an OrderStateUnknown");

        assert_eq!(unknown.symbol, "SOLUSDT");
        assert_eq!(unknown.client_order_id, "vp-1");
        assert_eq!(unknown.order_ref, None);
    }

    #[tokio::test]
    async fn records_every_call_it_receives() {
        let stub = CexStub::new();
        stub.execute(&request()).await.unwrap();
        stub.set_position("SOLUSDT", decimal("-5"));
        stub.execute(&reduce_only(OrderSide::Buy, "3"))
            .await
            .unwrap();

        let calls = stub.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].request.symbol, "SOLUSDT");
        assert_eq!(calls[0].request.quantity, decimal("10"));
        assert!(!calls[0].request.reduce_only);
        assert!(calls[1].request.reduce_only);
    }

    #[tokio::test]
    async fn reduce_only_mode_fills_a_reducing_order_and_rejects_the_rest() {
        let stub = CexStub::new();
        stub.set_position("SOLUSDT", decimal("-5"));

        // Buying 3 against a short of 5 reduces it.
        let fill = stub
            .execute(&reduce_only(OrderSide::Buy, "3"))
            .await
            .unwrap();
        assert_eq!(fill.filled_qty, decimal("3"));
        assert_eq!(stub.position("SOLUSDT"), Some(decimal("-2")));

        // Selling would grow the short.
        let err = stub
            .execute(&reduce_only(OrderSide::Sell, "1"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("reduce-only"));

        // Buying more than the short would flip it into a long.
        let err = stub
            .execute(&reduce_only(OrderSide::Buy, "8"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("reduce-only"));
        assert_eq!(stub.position("SOLUSDT"), Some(decimal("-2")));
    }

    #[tokio::test]
    async fn reduce_only_against_a_short_of_five_rejects_a_buy_of_eight() {
        let stub = CexStub::new();
        stub.set_position("SOLUSDT", decimal("-5"));

        let err = stub
            .execute(&reduce_only(OrderSide::Buy, "8"))
            .await
            .unwrap_err();

        assert!(err.to_string().contains("flip"));
        assert_eq!(stub.position("SOLUSDT"), Some(decimal("-5")));
    }

    #[tokio::test]
    async fn reduce_only_against_a_flat_position_is_rejected() {
        let stub = CexStub::new();
        stub.set_position("SOLUSDT", Decimal::ZERO);

        assert!(stub
            .execute(&reduce_only(OrderSide::Buy, "1"))
            .await
            .is_err());
        assert!(stub
            .execute(&reduce_only(OrderSide::Sell, "1"))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn a_symbol_with_no_position_set_is_not_checked() {
        let stub = CexStub::new();
        let fill = stub
            .execute(&reduce_only(OrderSide::Sell, "1"))
            .await
            .unwrap();
        assert_eq!(fill.filled_qty, decimal("1"));
        assert_eq!(stub.position("SOLUSDT"), None);
    }
}
