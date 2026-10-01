//! Reading a perp account (`SPEC.md` §6b, `specs/V4-cex-account-reads.md`).
//!
//! A consumer holding a perp position needs three facts only the venue
//! knows: the position the venue holds, the account's margin, and the
//! funding it paid or received. Every value here is the venue's own number,
//! read at call time. Nothing is summed across calls, marked to a price the
//! crate chose, or remembered (§2): reading what a venue reports is not
//! tracking.
//!
//! Reads carry no `Provenance`: nothing is sent, so there is no "sent or
//! thrown away" to report. Which account was read, testnet or production,
//! is the adapter's `label()` and base URL.

use anyhow::Result;
use async_trait::async_trait;
use rust_decimal::Decimal;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarginMode {
    Isolated,
    Cross,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PerpPosition {
    pub symbol: String,
    /// Signed: negative is short. Zero when flat, which is an answer and not
    /// an error.
    pub qty: Decimal,
    pub entry_price: Decimal,
    pub mark_price: Decimal,
    /// `None` when flat, or when the venue reports none.
    pub liquidation_price: Option<Decimal>,
    pub margin_mode: MarginMode,
    pub leverage: u32,
    /// `Some` exactly when `margin_mode` is `Isolated`.
    pub isolated_margin: Option<Decimal>,
    /// The venue's own update time for this position, in Unix ms.
    pub as_of_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarginState {
    /// The asset these figures are in, e.g. "USDT".
    pub asset: String,
    pub margin_balance: Decimal,
    pub maint_margin: Decimal,
    pub available: Decimal,
    pub as_of_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FundingPayment {
    pub symbol: String,
    pub ts_ms: i64,
    /// Signed: positive was received, negative was paid.
    pub amount: Decimal,
    pub asset: String,
    /// The venue's id for this payment, so a caller can de-duplicate.
    pub venue_ref: u64,
}

#[async_trait]
pub trait CexAccount: Send + Sync {
    async fn position(&self, symbol: &str) -> Result<PerpPosition>;
    async fn margin(&self) -> Result<MarginState>;
    /// Every payment with `ts_ms >= since_ms`, oldest first. The adapter pages
    /// through the venue's limit itself; the result is never cut short
    /// without a word.
    async fn funding_since(&self, symbol: &str, since_ms: i64) -> Result<Vec<FundingPayment>>;
    fn label(&self) -> &'static str;
}
