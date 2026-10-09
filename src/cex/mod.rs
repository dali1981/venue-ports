//! The CEX port. See `SPEC.md` §6. Types and the `CexExecutor` trait land
//! in Phase 6, `CexStub` alongside them, a venue's `Live` adapter in Phase 7
//! (`IMPLEMENTATION_PLAN.md`). Reading a perp account (`CexAccount`, §6b)
//! lives in `account`; resolving an order left unknown (`CexOrders`, §6) in
//! `orders`.

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
mod orders;
mod stub;

pub use account::{CexAccount, FundingPayment, MarginMode, MarginState, PerpPosition};
pub use account_stub::{AccountCall, AccountRead, CexAccountStub};
pub use binance::{
    BinanceConfig, BinanceLive, BinanceRest, CommissionDiscount, CommissionRates, MakerTaker,
    OrderCheck,
};
pub use binance_futures::{
    BinanceFuturesAccount, BinanceFuturesConfig, BinanceFuturesLive, BinanceFuturesRest,
};
pub use bybit::{BybitConfig, BybitLive, BybitRest};
pub use orders::{CexOrders, OrderState};
pub use stub::{CexStub, OrderStateCall, RecordedCall};

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
    /// This value as the error `execute` returns, with `why` as its message
    /// and `provenance` attached. `downcast_ref::<OrderStateUnknown>()` still
    /// finds it under them, and `why` is said once: the provenance is carried
    /// by the layer that holds `why`.
    pub(crate) fn because(
        self,
        provenance: Provenance,
        why: impl std::fmt::Display,
    ) -> anyhow::Error {
        let message = format!("{why:#}");
        anyhow::Error::new(self).context(ErrorProvenance {
            provenance,
            message,
        })
    }
}

/// Which provenance an `Err` from [`CexExecutor::execute`] came from: the one
/// a fill from that executor would carry. `Landed` for an executor that sends
/// to a venue (a refusal before anything was sent included, since it is the
/// refusal of an order that would have been), `Simulated` for a stub or a
/// paper model. An [`OrderStateUnknown`] carries it too, so a caller records
/// where the unresolved order is without being told by whoever built the
/// executor.
///
/// It is a layer of the error's chain, found with `downcast_ref` like
/// `OrderStateUnknown`, or read with [`provenance_of`], and added with
/// [`with_provenance`]. It leaves the error as it was: its `Display` is the
/// message of the layer it was added over, so `to_string()` of the error is
/// the same with or without it, and whatever else is in the chain
/// (`OrderStateUnknown` included) is still found by `downcast_ref`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorProvenance {
    pub provenance: Provenance,
    message: String,
}

impl std::fmt::Display for ErrorProvenance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ErrorProvenance {}

/// `err` with `provenance` attached, for an executor to return from
/// `execute`: every adapter in this crate does, and a consumer's paper model
/// does the same with `Provenance::Simulated`. An error that already carries
/// one keeps it, since whoever attached it first knew the order best.
///
/// `to_string()` of the result is `err`'s. `{:#}` of one tagged after the fact
/// says the top-level message twice (once as the tag, once as the layer it
/// was added over); an [`OrderStateUnknown`] built by this crate does not.
pub fn with_provenance(err: anyhow::Error, provenance: Provenance) -> anyhow::Error {
    if provenance_of(&err).is_some() {
        return err;
    }
    let message = err.to_string();
    err.context(ErrorProvenance {
        provenance,
        message,
    })
}

/// The provenance an `Err` from [`CexExecutor::execute`] carries, or `None`
/// if the executor that returned it did not attach one.
pub fn provenance_of(err: &anyhow::Error) -> Option<Provenance> {
    err.downcast_ref::<ErrorProvenance>()
        .map(|tag| tag.provenance)
}

/// The venue read the request and refused it: it did not act on it. A caller
/// that must act differently on `-2010` (no balance), `-2015` (key, IP or
/// permission) and `-1021` (clock) reads `code`, instead of searching the
/// error's text for it.
///
/// It is a layer of the error's chain, found with [`refusal_of`] (or
/// `downcast_ref`), added wherever a Binance refusal becomes an
/// `anyhow::Error`: the order path, `test_order`, `order_state`, the account
/// read, the clock read and the futures client, which shares the spot client.
/// The error's `Display` is as it was: `to_string()` is the same with or
/// without it, and whatever else is in the chain (`OrderStateUnknown` included)
/// is still found by `downcast_ref`. A call whose answer was lost, or that was
/// never sent, is not a refusal: the venue may have acted on the first, and
/// says nothing of the second.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VenueRefusal {
    /// The HTTP status of the answer.
    pub status: u16,
    /// The venue's own code, when the body carried one (Binance: `-1013`,
    /// `-2010`, `-2015`, `-1021`, …).
    pub code: Option<i64>,
    /// The venue's own message, or its body when it was not JSON.
    pub msg: String,
}

impl std::fmt::Display for VenueRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.code {
            Some(code) => write!(
                f,
                "refused (HTTP {}, code {code}): {}",
                self.status, self.msg
            ),
            None => write!(f, "refused (HTTP {}): {}", self.status, self.msg),
        }
    }
}

impl std::error::Error for VenueRefusal {}

impl VenueRefusal {
    /// This refusal as the error an order path returns, with `why` as its
    /// message and `provenance` attached, as [`OrderStateUnknown::because`] does
    /// for an unknown order: `to_string()` is `why`, and [`refusal_of`] finds
    /// the refusal under it. `why` names the order and says what the refusal
    /// meant, and ends with the refusal's own text, so `{:#}` of the result
    /// says that text once more.
    pub(crate) fn because(
        self,
        provenance: Provenance,
        why: impl std::fmt::Display,
    ) -> anyhow::Error {
        let message = format!("{why:#}");
        anyhow::Error::new(self).context(ErrorProvenance {
            provenance,
            message,
        })
    }
}

/// The venue's refusal in `err`'s chain, if the error is one: the request was
/// read and turned down, and the venue did not act on it. `None` for every
/// other error, a lost answer and an unsent request included.
pub fn refusal_of(err: &anyhow::Error) -> Option<&VenueRefusal> {
    err.downcast_ref::<VenueRefusal>()
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
    /// The id this crate gave the order (`newClientOrderId`, `orderLinkId`),
    /// which the venue can be asked about it by. Set if and only if
    /// `provenance == Provenance::Landed`: a stub or a paper model sends
    /// nothing, so it names nothing.
    pub client_order_id: Option<String>,
    /// The venue's own time for the order, in ms since the epoch: Binance
    /// spot's `transactTime` (its `updateTime` when the order was found by a
    /// status query), Binance futures' `updateTime`, Bybit's `updatedTime`.
    /// `None` when nothing was sent, or when the venue's answer carried none.
    pub venue_time_ms: Option<u64>,
    /// The order's trades, as the venue listed them. Empty when nothing was
    /// sent, and on a venue whose adapter does not read them (Bybit). When
    /// listed, their quantities add up to `filled_qty`.
    pub trades: Vec<CexTrade>,
}

/// One trade an order made, as the venue listed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CexTrade {
    /// The venue's id for the trade. On Binance it is the id the public
    /// trade stream gives the same trade (`t`), so the fill can be found in
    /// a recording of that stream. `None` only if the venue's line carried
    /// none.
    pub trade_id: Option<u64>,
    pub price: Decimal,
    pub qty: Decimal,
    pub commission: Decimal,
    pub commission_asset: String,
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
    ///
    /// Every `Err` carries the provenance of the order it is the failure of
    /// ([`provenance_of`]): an implementation attaches it with
    /// [`with_provenance`], and a caller reads it there rather than being
    /// handed the executor's.
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

    fn unknown() -> OrderStateUnknown {
        OrderStateUnknown {
            symbol: "SOLUSDT".to_string(),
            client_order_id: "vp-1".to_string(),
            order_ref: Some(7),
        }
    }

    fn refusal(code: Option<i64>) -> VenueRefusal {
        VenueRefusal {
            status: 400,
            code,
            msg: "Account has insufficient balance for requested action.".to_string(),
        }
    }

    /// The text `ApiError::Refused` printed before the refusal was a type.
    #[test]
    fn a_refusal_says_what_a_refused_call_always_said() {
        assert_eq!(
            refusal(Some(-2010)).to_string(),
            "refused (HTTP 400, code -2010): Account has insufficient balance for requested action."
        );
        assert_eq!(
            VenueRefusal {
                status: 403,
                code: None,
                msg: "forbidden".to_string(),
            }
            .to_string(),
            "refused (HTTP 403): forbidden"
        );
    }

    #[test]
    fn a_refusal_is_found_by_type_under_whatever_was_added_to_the_error() {
        let bare: anyhow::Error = refusal(Some(-2015)).into();
        assert_eq!(refusal_of(&bare), Some(&refusal(Some(-2015))));

        let explained = bare.context("reading GET /api/v3/account");
        assert_eq!(explained.to_string(), "reading GET /api/v3/account");
        assert_eq!(refusal_of(&explained).unwrap().code, Some(-2015));

        let tagged = with_provenance(explained, Provenance::Landed);
        assert_eq!(refusal_of(&tagged).unwrap().code, Some(-2015));
        assert_eq!(provenance_of(&tagged), Some(Provenance::Landed));
    }

    /// Only a refusal is one: not a plain error, and not an order whose state is
    /// unknown, which a venue may have acted on.
    #[test]
    fn nothing_but_a_refusal_is_a_refusal() {
        assert!(refusal_of(&anyhow::anyhow!("refused (HTTP 400, code -2010): no")).is_none());
        let unknown_state = unknown().because(Provenance::Landed, "the status query failed");
        assert!(refusal_of(&unknown_state).is_none());
    }

    /// An order path says why in its own words, the provenance rides on that
    /// layer, and the refusal is under both.
    #[test]
    fn a_refused_order_keeps_its_message_its_provenance_and_the_refusal() {
        let why = format!(
            "BUY 1 SOLUSDT (vp-1) was not placed, nothing filled: {}",
            refusal(Some(-2010))
        );
        let err = refusal(Some(-2010)).because(Provenance::Landed, &why);

        assert_eq!(err.to_string(), why);
        assert_eq!(provenance_of(&err), Some(Provenance::Landed));
        assert_eq!(refusal_of(&err).unwrap().code, Some(-2010));
        assert!(err.downcast_ref::<OrderStateUnknown>().is_none());
        // With provenance already on it, `with_provenance` leaves it as it is.
        let again = with_provenance(err, Provenance::Simulated);
        assert_eq!(provenance_of(&again), Some(Provenance::Landed));
        assert_eq!(again.to_string(), why);
    }

    #[test]
    fn state_unknown_with_context_is_still_found_by_downcast() {
        let err = unknown().because(Provenance::Landed, "the status query failed");
        assert_eq!(err.to_string(), "the status query failed");
        assert_eq!(
            err.downcast_ref::<OrderStateUnknown>().unwrap().order_ref,
            Some(7)
        );
    }

    /// The provenance rides on the layer that holds the message, so the
    /// error reads as it did before it carried one.
    #[test]
    fn state_unknown_carries_its_provenance_without_saying_its_message_twice() {
        let err = unknown().because(Provenance::Landed, "the status query failed");
        assert_eq!(provenance_of(&err), Some(Provenance::Landed));
        assert_eq!(
            format!("{err:#}"),
            "the status query failed: the state of order vp-1 on SOLUSDT is unknown: \
             it may have filled in part, in full or not at all (venue order id 7)"
        );
    }

    /// An order state unknown built by a struct literal, as a consumer's
    /// tests build one, is still an `OrderStateUnknown` once it is tagged,
    /// and says what it said.
    #[test]
    fn a_tagged_error_keeps_its_message_and_the_typed_error_under_it() {
        let bare: anyhow::Error = unknown().into();
        let said = bare.to_string();
        assert_eq!(provenance_of(&bare), None);

        let tagged = with_provenance(bare, Provenance::Simulated);
        assert_eq!(tagged.to_string(), said);
        assert_eq!(provenance_of(&tagged), Some(Provenance::Simulated));
        assert_eq!(tagged.downcast_ref::<OrderStateUnknown>(), Some(&unknown()));

        let plain = with_provenance(anyhow::anyhow!("order rejected: no"), Provenance::Landed);
        assert_eq!(plain.to_string(), "order rejected: no");
        assert_eq!(provenance_of(&plain), Some(Provenance::Landed));
        // The cost of tagging after the fact, which `with_provenance` says.
        assert_eq!(
            format!("{plain:#}"),
            "order rejected: no: order rejected: no"
        );
        assert!(plain.downcast_ref::<OrderStateUnknown>().is_none());
    }

    #[test]
    fn the_provenance_survives_context_added_above_it() {
        let err = with_provenance(anyhow::anyhow!("refused"), Provenance::Landed)
            .context("placing the hedge");
        assert_eq!(err.to_string(), "placing the hedge");
        assert_eq!(provenance_of(&err), Some(Provenance::Landed));

        let err = unknown()
            .because(Provenance::Simulated, "lost")
            .context("placing the hedge");
        assert_eq!(provenance_of(&err), Some(Provenance::Simulated));
        assert!(err.downcast_ref::<OrderStateUnknown>().is_some());
    }

    /// Whoever tagged an error first knew the order best.
    #[test]
    fn an_error_that_carries_a_provenance_keeps_it() {
        let landed = unknown().because(Provenance::Landed, "lost");
        let retagged = with_provenance(landed, Provenance::Simulated);
        assert_eq!(provenance_of(&retagged), Some(Provenance::Landed));
        assert_eq!(retagged.to_string(), "lost");
    }
}
