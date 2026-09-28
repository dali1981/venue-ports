//! The CEX port. See `SPEC.md` §6. Types and the `CexExecutor` trait land
//! in Phase 6, `CexStub` alongside them, a venue's `Live` adapter in Phase 7
//! (`IMPLEMENTATION_PLAN.md`).

use crate::Provenance;
use anyhow::Result;
use async_trait::async_trait;
use rust_decimal::Decimal;

mod binance;
mod bybit;
mod stub;

pub use binance::{BinanceConfig, BinanceLive};
pub use bybit::{BybitConfig, BybitLive};
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
    /// The venue must refuse any part of this order that would increase or
    /// flip the position instead of reducing it. A venue with no positions
    /// (spot) must reject `true` with an error before sending, never ignore
    /// it: an ignored reduce-only is an unguarded order.
    pub reduce_only: bool,
}

/// Returned (inside `anyhow::Error`, found with `downcast_ref`) when an
/// order may have reached the venue but its outcome could not be read: the
/// placing call's response was lost, and the status query that should
/// follow it also failed. The venue may have filled some, all or none of
/// it. The caller must find out before it acts on this symbol again, for
/// example by reading its position.
///
/// This is the one exception to the port's error rule: **an `Err` from
/// [`CexExecutor::execute`] means nothing filled, unless it is an
/// `OrderStateUnknown`.**
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderStateUnknown {
    pub symbol: String,
    /// The id this crate gave the order, which the venue can be asked about.
    pub client_order_id: String,
    /// `Some` if the venue acknowledged the order before contact was lost.
    pub order_ref: Option<u64>,
}

impl std::fmt::Display for OrderStateUnknown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the state of order {} on {} is unknown: it may have filled in part, in full or not at all",
            self.client_order_id, self.symbol
        )?;
        if let Some(order_ref) = self.order_ref {
            write!(f, " (venue order id {order_ref})")?;
        }
        Ok(())
    }
}

impl std::error::Error for OrderStateUnknown {}

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
    ///
    /// An `Err` means nothing filled, unless it is an [`OrderStateUnknown`].
    /// After a venue accepts an order, a lost response, a failed status
    /// query, or a fill whose details cannot be read all become
    /// `OrderStateUnknown`, never a plain error.
    async fn execute(&self, req: &OrderRequest) -> Result<CexFill>;

    fn label(&self) -> &'static str;
}
