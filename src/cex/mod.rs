//! The CEX port. See `SPEC.md` §6. Types and the `CexExecutor` trait land
//! in Phase 6, `CexStub` alongside them, a venue's `Live` adapter in Phase 7
//! (`IMPLEMENTATION_PLAN.md`).

use crate::Provenance;
use anyhow::Result;
use async_trait::async_trait;
use rust_decimal::Decimal;

mod stub;

pub use stub::{CexStub, RecordedCall};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderSide {
    Buy,
    Sell,
}

#[derive(Debug, Clone)]
pub struct OrderRequest {
    /// The venue's own symbol spelling (e.g. `"SOLUSDT"`) — this crate does
    /// not maintain a symbol mapping.
    pub symbol: String,
    pub side: OrderSide,
    pub quantity: Decimal,
    /// The price this order was decided against. Used by a simulated or
    /// stub implementation to price the resulting fill; a live
    /// implementation may log it for comparison but must never use it to
    /// build the order itself — a market order carries no price, and
    /// treating this as one would be a fabricated quote, not a real order.
    pub quoted_price: Decimal,
}

#[derive(Debug, Clone)]
pub struct CexFill {
    pub filled_qty: Decimal,
    pub filled_price: Decimal,
    pub commission: Decimal,
    pub commission_asset: String,
    pub provenance: Provenance,
    /// Set if and only if `provenance == Provenance::Landed`.
    pub order_ref: Option<u64>,
}

#[async_trait]
pub trait CexExecutor: Send + Sync {
    /// Places (or simulates) the order and returns only once its outcome
    /// is known. Many venues answer a market order synchronously in the
    /// same call that placed it — read that response correctly rather
    /// than polling unnecessarily; fall back to a status query only when
    /// the placing call's own response is ambiguous (e.g. the connection
    /// was lost after the order was sent but before the response arrived).
    async fn execute(&self, req: &OrderRequest) -> Result<CexFill>;

    fn label(&self) -> &'static str;
}
