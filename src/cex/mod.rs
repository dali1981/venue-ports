//! The CEX port. See `SPEC.md` §6. Types and the `CexExecutor` trait land
//! in Phase 6, `CexStub` alongside them, a venue's `Live` adapter in Phase 7
//! (`IMPLEMENTATION_PLAN.md`). Reading a perp account (`CexAccount`, §6b)
//! lives in `account`.

use crate::Provenance;
use anyhow::Result;
use async_trait::async_trait;
use rust_decimal::Decimal;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

mod account;
mod account_stub;
mod binance;
mod binance_futures;
mod bybit;
mod stub;

pub use account::{CexAccount, FundingPayment, MarginMode, MarginState, PerpPosition};
pub use account_stub::{AccountCall, AccountRead, CexAccountStub};
pub use binance::{BinanceConfig, BinanceLive, BinanceRest};
pub use binance_futures::{
    BinanceFuturesAccount, BinanceFuturesConfig, BinanceFuturesLive, BinanceFuturesRest,
};
pub use bybit::{BybitConfig, BybitLive, BybitRest};
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

impl OrderStateUnknown {
    /// This value as the error `execute` returns, with `why` as context.
    /// `downcast_ref::<OrderStateUnknown>()` still finds it under the
    /// context.
    pub(crate) fn because(self, why: impl std::fmt::Display) -> anyhow::Error {
        let why = format!("{why:#}");
        anyhow::Error::new(self).context(why)
    }
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
    ///
    /// An `Err` means nothing filled, unless it is an [`OrderStateUnknown`].
    /// After a venue accepts an order, a lost response, a failed status
    /// query, or a fill whose details cannot be read all become
    /// `OrderStateUnknown`, never a plain error.
    async fn execute(&self, req: &OrderRequest) -> Result<CexFill>;

    fn label(&self) -> &'static str;
}

/// Every wait a live CEX adapter makes, in one place so that a test can make
/// them short. The defaults suit a real venue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CexTimings {
    /// How long one HTTP request may take, response body included, before
    /// its answer counts as lost. A lost order request is followed by a
    /// status query, never resent blind.
    pub request_timeout: Duration,
    /// How long a signed request stays valid after its timestamp (Binance's
    /// `recvWindow`). Once it has passed for a lost order request, the
    /// venue can no longer accept that request, so "no such order" becomes
    /// conclusive.
    pub recv_window: Duration,
    /// How often the venue's clock is read again (Binance).
    pub clock_refresh: Duration,
    /// The pause between two status queries for an order that has not
    /// settled, or between two reads of trade lines that have not all
    /// appeared.
    pub poll_interval: Duration,
    /// How long to keep asking about an order the venue accepted, or may
    /// have, before giving up with [`OrderStateUnknown`].
    pub poll_timeout: Duration,
    /// How long an order's trade lines, which carry its commission, may lag
    /// its fill before the adapter gives up with [`OrderStateUnknown`]
    /// (Binance).
    pub trades_timeout: Duration,
}

impl Default for CexTimings {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(10),
            // Binance's own default `recvWindow`.
            recv_window: Duration::from_millis(5_000),
            clock_refresh: Duration::from_secs(10 * 60),
            poll_interval: Duration::from_millis(200),
            poll_timeout: Duration::from_secs(5),
            trades_timeout: Duration::from_secs(2),
        }
    }
}

/// A client order id unique to this call, which the venue can be asked
/// about when the placing call's answer is lost: milliseconds since the
/// epoch, this process's id and a per-process counter, in base 36 behind a
/// `vp-` prefix. At most 34 characters of `[0-9a-z-]` until the year 2059,
/// inside what Binance (`newClientOrderId`) and Bybit (`orderLinkId`) accept:
/// 36 characters each.
pub(crate) fn new_client_order_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64);
    format!(
        "vp-{}-{}-{}",
        base36(millis),
        base36(u64::from(std::process::id())),
        base36(count)
    )
}

fn base36(mut n: u64) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    loop {
        out.push(DIGITS[(n % 36) as usize]);
        n /= 36;
        if n == 0 {
            break;
        }
    }
    out.reverse();
    String::from_utf8(out).expect("base-36 digits are ASCII")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn client_order_ids_are_unique_and_fit_every_venue() {
        let ids: Vec<String> = (0..10_000).map(|_| new_client_order_id()).collect();
        assert_eq!(ids.iter().collect::<HashSet<_>>().len(), ids.len());
        for id in &ids {
            assert!(id.len() <= 36, "{id} is {} characters", id.len());
            assert!(id
                .bytes()
                .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase() || b == b'-'));
        }
    }

    #[test]
    fn the_longest_id_before_2059_still_fits() {
        // 36^8 ms after the epoch is in 2059; before then the time part is
        // at most 8 characters. The process id is a `u32` everywhere.
        let longest = format!(
            "vp-{}-{}-{}",
            base36(36u64.pow(8) - 1),
            base36(u64::from(u32::MAX)),
            base36(u64::MAX)
        );
        assert!(longest.len() <= 36, "{longest} is {}", longest.len());
    }

    #[test]
    fn base36_round_trips() {
        assert_eq!(base36(0), "0");
        assert_eq!(base36(35), "z");
        assert_eq!(base36(36), "10");
        assert_eq!(
            u64::from_str_radix(&base36(u64::MAX), 36).unwrap(),
            u64::MAX
        );
    }

    #[test]
    fn state_unknown_with_context_is_still_found_by_downcast() {
        let err = OrderStateUnknown {
            symbol: "SOLUSDT".to_string(),
            client_order_id: "vp-1".to_string(),
            order_ref: Some(7),
        }
        .because("the status query failed");
        assert_eq!(err.to_string(), "the status query failed");
        assert_eq!(
            err.downcast_ref::<OrderStateUnknown>().unwrap().order_ref,
            Some(7)
        );
    }
}
