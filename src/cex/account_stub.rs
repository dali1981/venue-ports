//! `CexAccountStub` — an in-process fake `CexAccount` (`SPEC.md` §6b):
//! programmable values and errors, and a record of every call, the same
//! shape as `CexStub`.
//!
//! It keeps to the port's contract the way a venue adapter must, so code
//! built on it meets the same answers a real account gives:
//! `funding_since` returns only payments at or after `since_ms`, oldest
//! first, one per `venue_ref`. It never invents a value: a position or
//! margin that was never set is an error, not a made-up flat account.

use crate::cex::account::{CexAccount, FundingPayment, MarginState, PerpPosition};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Mutex;

/// One call exactly as the stub received it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountCall {
    Position { symbol: String },
    Margin,
    FundingSince { symbol: String, since_ms: i64 },
}

/// Which read a programmed error is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AccountRead {
    Position,
    Margin,
    Funding,
}

#[derive(Default)]
pub struct CexAccountStub {
    positions: Mutex<HashMap<String, PerpPosition>>,
    margin: Mutex<Option<MarginState>>,
    /// Keyed by `venue_ref`, which identifies one payment.
    funding: Mutex<BTreeMap<u64, FundingPayment>>,
    errors: Mutex<HashMap<AccountRead, VecDeque<anyhow::Error>>>,
    calls: Mutex<Vec<AccountCall>>,
}

impl CexAccountStub {
    pub fn new() -> Self {
        Self::default()
    }

    /// The position `position(symbol)` returns for `position.symbol`, until
    /// replaced.
    pub fn set_position(&self, position: PerpPosition) {
        self.positions
            .lock()
            .unwrap()
            .insert(position.symbol.clone(), position);
    }

    /// The margin `margin()` returns, until replaced.
    pub fn set_margin(&self, margin: MarginState) {
        *self.margin.lock().unwrap() = Some(margin);
    }

    /// Adds a funding payment. One with the `venue_ref` of an earlier one
    /// replaces it, as the venue's id says it is the same payment.
    pub fn push_funding(&self, payment: FundingPayment) {
        self.funding
            .lock()
            .unwrap()
            .insert(payment.venue_ref, payment);
    }

    /// The next `read` fails with `error` instead of answering. Errors
    /// programmed for one read are consumed in the order programmed.
    pub fn program_error(&self, read: AccountRead, error: anyhow::Error) {
        self.errors
            .lock()
            .unwrap()
            .entry(read)
            .or_default()
            .push_back(error);
    }

    /// Every call the stub has received so far, in the order received.
    pub fn calls(&self) -> Vec<AccountCall> {
        self.calls.lock().unwrap().clone()
    }

    fn receive(&self, call: AccountCall, read: AccountRead) -> Result<()> {
        self.calls.lock().unwrap().push(call);
        match self
            .errors
            .lock()
            .unwrap()
            .get_mut(&read)
            .and_then(VecDeque::pop_front)
        {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

#[async_trait]
impl CexAccount for CexAccountStub {
    async fn position(&self, symbol: &str) -> Result<PerpPosition> {
        self.receive(
            AccountCall::Position {
                symbol: symbol.to_string(),
            },
            AccountRead::Position,
        )?;
        self.positions
            .lock()
            .unwrap()
            .get(symbol)
            .cloned()
            .ok_or_else(|| anyhow!("no position was set for {symbol} on this stub"))
    }

    async fn margin(&self) -> Result<MarginState> {
        self.receive(AccountCall::Margin, AccountRead::Margin)?;
        self.margin
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| anyhow!("no margin was set on this stub"))
    }

    async fn funding_since(&self, symbol: &str, since_ms: i64) -> Result<Vec<FundingPayment>> {
        self.receive(
            AccountCall::FundingSince {
                symbol: symbol.to_string(),
                since_ms,
            },
            AccountRead::Funding,
        )?;
        let mut payments: Vec<FundingPayment> = self
            .funding
            .lock()
            .unwrap()
            .values()
            .filter(|payment| payment.symbol == symbol && payment.ts_ms >= since_ms)
            .cloned()
            .collect();
        payments.sort_by_key(|payment| (payment.ts_ms, payment.venue_ref));
        Ok(payments)
    }

    fn label(&self) -> &'static str {
        "cex-account-stub"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cex::account::MarginMode;
    use rust_decimal::Decimal;

    fn payment(venue_ref: u64, ts_ms: i64, symbol: &str) -> FundingPayment {
        FundingPayment {
            symbol: symbol.to_string(),
            ts_ms,
            amount: Decimal::new(-12, 2),
            asset: "USDT".to_string(),
            venue_ref,
        }
    }

    #[tokio::test]
    async fn returns_what_was_set_and_records_every_call() {
        let stub = CexAccountStub::new();
        let position = PerpPosition {
            symbol: "BTCUSDT".to_string(),
            qty: Decimal::new(-5, 3),
            entry_price: Decimal::from(60_000),
            mark_price: Decimal::from(60_100),
            liquidation_price: Some(Decimal::from(90_000)),
            margin_mode: MarginMode::Cross,
            leverage: 5,
            isolated_margin: None,
            as_of_ms: 1,
        };
        stub.set_position(position.clone());

        assert_eq!(stub.position("BTCUSDT").await.unwrap(), position);
        assert!(stub.margin().await.is_err(), "nothing set is an error");
        stub.funding_since("BTCUSDT", 5).await.unwrap();

        assert_eq!(
            stub.calls(),
            [
                AccountCall::Position {
                    symbol: "BTCUSDT".to_string()
                },
                AccountCall::Margin,
                AccountCall::FundingSince {
                    symbol: "BTCUSDT".to_string(),
                    since_ms: 5
                },
            ]
        );
    }

    #[tokio::test]
    async fn programmed_errors_are_consumed_in_order_for_their_read_only() {
        let stub = CexAccountStub::new();
        stub.program_error(AccountRead::Funding, anyhow!("first"));
        stub.program_error(AccountRead::Funding, anyhow!("second"));

        assert!(stub.margin().await.is_err()); // unset, not programmed
        let first = stub.funding_since("BTCUSDT", 0).await.unwrap_err();
        let second = stub.funding_since("BTCUSDT", 0).await.unwrap_err();
        assert_eq!(first.to_string(), "first");
        assert_eq!(second.to_string(), "second");
        assert!(stub.funding_since("BTCUSDT", 0).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn funding_is_filtered_sorted_and_one_per_venue_ref() {
        let stub = CexAccountStub::new();
        stub.push_funding(payment(3, 300, "BTCUSDT"));
        stub.push_funding(payment(1, 100, "BTCUSDT"));
        stub.push_funding(payment(2, 200, "ETHUSDT"));
        stub.push_funding(payment(4, 50, "BTCUSDT"));
        stub.push_funding(payment(1, 150, "BTCUSDT")); // replaces ref 1

        let funding = stub.funding_since("BTCUSDT", 100).await.unwrap();

        let seen: Vec<(u64, i64)> = funding.iter().map(|p| (p.venue_ref, p.ts_ms)).collect();
        assert_eq!(seen, [(1, 150), (3, 300)]);
    }
}
