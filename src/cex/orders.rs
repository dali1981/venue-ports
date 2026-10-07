//! Resolving an order left unknown (`SPEC.md` §6, `specs/V6-balances-and-resolution.md`).
//!
//! An [`OrderStateUnknown`](crate::cex::OrderStateUnknown) names an order the venue may have filled. The
//! adapter that returned it gave up reading it, honestly; [`CexOrders`] asks
//! the venue again, by the client order id the adapter itself gave the order.
//! No caller chooses an id, so a resolver cannot turn a resend into a first
//! send.

use crate::cex::CexFill;
use anyhow::Result;
use async_trait::async_trait;

/// What the venue says became of an order, asked once.
#[derive(Debug, Clone)]
pub enum OrderState {
    /// Ended `FILLED`: the whole fill, as `execute` would have returned it.
    Filled(CexFill),
    /// Ended with part of it filled and the rest not (`EXPIRED`, `CANCELED`,
    /// `EXPIRED_IN_MATCH`: a market order that ran out of book, for example).
    /// What `execute` returns as a partial `CexFill`.
    PartlyFilled(CexFill),
    /// Ended with nothing filled: the venue accepted the order, and it ended in
    /// `status`. Nothing is exposed.
    Rejected { order_ref: u64, status: String },
    /// The venue has no such order, and can no longer accept the request that
    /// placed it: it never accepted the order, and nothing filled.
    NotFound,
    /// Not settled: still working, or the venue does not know it yet and could
    /// still accept the request that placed it (a lost answer's `recvWindow`
    /// has not passed). Ask again.
    Open,
}

/// Asking a venue what became of an order it may have filled.
///
/// It has no `label()`, which the adapters that implement it already have from
/// [`CexExecutor`](crate::cex::CexExecutor): a second one would make
/// `adapter.label()` ambiguous wherever both traits are in scope.
#[async_trait]
pub trait CexOrders: Send + Sync {
    /// The state of the order this adapter placed under `client_order_id` (the
    /// id an [`OrderStateUnknown`](crate::cex::OrderStateUnknown) names) on `symbol`.
    ///
    /// One read, with no waiting: the caller sets the schedule. An `Err` is a
    /// read that failed (the status query, or the trade lines of a fill, could
    /// not be read): ask again. It is never [`OrderState::NotFound`], which is
    /// said only once it is conclusive.
    async fn order_state(&self, symbol: &str, client_order_id: &str) -> Result<OrderState>;
}
