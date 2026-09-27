//! `CexStub` — an in-process fake `CexExecutor`, same shape and
//! call-recording requirement as `EvmStub`. Implemented in Phase 6
//! (`IMPLEMENTATION_PLAN.md`).

use crate::cex::{CexExecutor, CexFill, OrderRequest};
use crate::Provenance;
use anyhow::Result;
use async_trait::async_trait;
use rust_decimal::Decimal;
use std::collections::VecDeque;
use std::sync::Mutex;

/// One `execute()` call exactly as received by the stub — the request it
/// was given — so a test can assert on what was actually sent, not just on
/// what came back.
#[derive(Debug, Clone)]
pub struct RecordedCall {
    pub request: OrderRequest,
}

#[derive(Default)]
pub struct CexStub {
    programmed: Mutex<VecDeque<Result<CexFill>>>,
    calls: Mutex<Vec<RecordedCall>>,
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
        self.programmed.lock().unwrap().push_back(result);
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

    /// Program a rejection — the venue refused the order outright.
    pub fn program_rejected(&self, reason: impl std::fmt::Display) {
        self.program_execute(Err(anyhow::anyhow!("order rejected: {reason}")));
    }

    /// Every call the stub has received so far, in the order received.
    pub fn calls(&self) -> Vec<RecordedCall> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl CexExecutor for CexStub {
    async fn execute(&self, req: &OrderRequest) -> Result<CexFill> {
        self.calls.lock().unwrap().push(RecordedCall {
            request: req.clone(),
        });

        if let Some(result) = self.programmed.lock().unwrap().pop_front() {
            return result;
        }
        Ok(CexFill {
            filled_qty: req.quantity,
            filled_price: req.quoted_price,
            commission: Decimal::ZERO,
            commission_asset: String::new(),
            provenance: Provenance::Simulated,
            order_ref: None,
        })
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
    }

    #[tokio::test]
    async fn records_every_call_it_receives() {
        let stub = CexStub::new();
        stub.execute(&request()).await.unwrap();

        let calls = stub.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].request.symbol, "SOLUSDT");
        assert_eq!(calls[0].request.quantity, decimal("10"));
    }
}
