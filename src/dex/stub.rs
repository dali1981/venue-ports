//! `DexStub` — an in-process fake `DexExecutor` with no network calls at all
//! (`SPEC.md` §5, the Stub row). It was `EvmStub`, renamed: it was never
//! EVM-specific. A test programs the exact `Realised` (or error) a given
//! `execute()` call returns — including a specific revert reason, a forced
//! `TimedOut` or an `Expired` — and can assert on every call the stub
//! actually received.
//!
//! **Chain-family-agnostic.** A route on an EVM network is prepared as an
//! `EvmCall`, one on Solana as an unsigned `SolanaTransaction` for the
//! sender, so the stub hands back what a real adapter of that family would.
//! An outcome programmed without a cost gets the family's cost of nothing
//! ([`TxCost::none_for`]); [`DexStub::program_execute`] sets one exactly.

use crate::dex::{
    ChainAmount, DexExecutor, EvmCall, Outcome, Prepared, Realised, RouteQuote, SwapRequest, TxCost,
};
use crate::{Network, Provenance};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use std::collections::VecDeque;
use std::sync::Mutex;

/// One `prepare`/`execute` pair exactly as received by the stub — the
/// route, the request, and the `Prepared` value built from them — so a
/// test can assert on what was actually sent, not just on what came back.
#[derive(Debug, Clone)]
pub struct RecordedCall {
    pub route: RouteQuote,
    pub request: SwapRequest,
    pub prepared: Prepared,
}

enum Programmed {
    /// A whole result, its cost included.
    Exact(Result<Realised>),
    /// An outcome whose cost is filled in at `execute`, from the family of
    /// the `Prepared` it runs.
    Shaped {
        amount_out: Option<ChainAmount>,
        outcome: Outcome,
        at: u64,
    },
}

#[derive(Default)]
pub struct DexStub {
    programmed: Mutex<VecDeque<Programmed>>,
    calls: Mutex<Vec<RecordedCall>>,
}

impl DexStub {
    pub fn new() -> Self {
        Self::default()
    }

    /// Program the exact result the next `execute()` call returns, its cost
    /// included. Consumed in the order programmed; once exhausted,
    /// `execute()` falls back to a plain success at the request's
    /// `min_amount_out`, so a test that doesn't care about the outcome
    /// doesn't have to program one.
    pub fn program_execute(&self, result: Result<Realised>) {
        self.programmed
            .lock()
            .unwrap()
            .push_back(Programmed::Exact(result));
    }

    fn program_shaped(&self, amount_out: Option<ChainAmount>, outcome: Outcome, at: u64) {
        self.programmed
            .lock()
            .unwrap()
            .push_back(Programmed::Shaped {
                amount_out,
                outcome,
                at,
            });
    }

    /// Program a successful swap.
    pub fn program_success(&self, amount_out: ChainAmount, at: u64) {
        self.program_shaped(Some(amount_out), Outcome::Success, at);
    }

    /// Program a revert with a specific reason.
    pub fn program_reverted(&self, reason: impl Into<String>, at: u64) {
        self.program_shaped(
            None,
            Outcome::Reverted {
                reason: reason.into(),
            },
            at,
        );
    }

    /// Program a forced timeout — the outcome a real adapter would report
    /// when a transaction never reaches a terminal state in time.
    pub fn program_timed_out(&self, at: u64) {
        self.program_shaped(None, Outcome::TimedOut, at);
    }

    /// Program an expiry — a Solana transaction whose blockhash expired
    /// with no status for its signature, so it can never land.
    pub fn program_expired(&self, at: u64) {
        self.program_shaped(None, Outcome::Expired, at);
    }

    /// Every call the stub has received so far, in the order received.
    pub fn calls(&self) -> Vec<RecordedCall> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl DexExecutor for DexStub {
    async fn prepare(&self, route: &RouteQuote, req: &SwapRequest) -> Result<Prepared> {
        let mut calls = self.calls.lock().unwrap();
        let prepared = match route.network {
            Network::Evm { .. } => Prepared::Evm(EvmCall {
                to: route.token_out.clone(),
                calldata: route.payload.clone(),
                value: req.min_amount_out,
            }),
            Network::Solana { .. } => {
                Prepared::offline(route.network, &req.sender, calls.len() as u64)
            }
        };
        calls.push(RecordedCall {
            route: route.clone(),
            request: req.clone(),
            prepared: prepared.clone(),
        });
        Ok(prepared)
    }

    async fn execute(&self, prepared: &Prepared, at: Option<u64>) -> Result<Realised> {
        let shaped = |amount_out, outcome, at| Realised {
            amount_out,
            outcome,
            cost: TxCost::none_for(prepared),
            at,
            provenance: Provenance::Simulated,
            tx_ref: None,
        };
        match self.programmed.lock().unwrap().pop_front() {
            Some(Programmed::Exact(result)) => return result,
            Some(Programmed::Shaped {
                amount_out,
                outcome,
                at,
            }) => return Ok(shaped(amount_out, outcome, at)),
            None => {}
        }
        let min_amount_out = self
            .calls
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|call| &call.prepared == prepared)
            .map(|call| call.request.min_amount_out)
            .ok_or_else(|| {
                anyhow!("DexStub has no programmed outcome for a Prepared value it did not prepare")
            })?;
        Ok(shaped(
            Some(min_amount_out),
            Outcome::Success,
            at.unwrap_or(0),
        ))
    }

    fn label(&self) -> &'static str {
        "dex-stub"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dex::{EvmCost, SolanaCost};
    use solana_address::Address;

    fn route() -> RouteQuote {
        RouteQuote {
            network: Network::evm(1),
            token_in: vec![1],
            token_out: vec![2],
            amount_in: 100,
            expected_amount_out: 90,
            payload: vec![0xde, 0xad],
        }
    }

    fn solana_route() -> RouteQuote {
        RouteQuote {
            network: Network::Solana {
                genesis_hash: [5; 32],
            },
            ..route()
        }
    }

    fn request() -> SwapRequest {
        SwapRequest {
            sender: vec![3],
            recipient: vec![4],
            min_amount_out: 90,
            deadline_unix_secs: 0,
        }
    }

    #[tokio::test]
    async fn default_execute_succeeds_at_the_requested_minimum() {
        let stub = DexStub::new();
        let prepared = stub.prepare(&route(), &request()).await.unwrap();
        let realised = stub.execute(&prepared, None).await.unwrap();

        assert!(matches!(realised.outcome, Outcome::Success));
        assert_eq!(realised.amount_out, Some(90));
        assert_eq!(realised.cost, TxCost::Evm(EvmCost::default()));
        assert_eq!(realised.provenance, Provenance::Simulated);
        assert_eq!(realised.tx_ref, None);
    }

    #[tokio::test]
    async fn a_solana_route_is_prepared_as_an_unsigned_transaction_for_the_sender() {
        let stub = DexStub::new();
        let sender = vec![9u8; 32];
        let req = SwapRequest {
            sender: sender.clone(),
            ..request()
        };
        let first = stub.prepare(&solana_route(), &req).await.unwrap();
        let second = stub
            .prepare(
                &solana_route(),
                &SwapRequest {
                    min_amount_out: 80,
                    ..req.clone()
                },
            )
            .await
            .unwrap();
        assert_ne!(first, second);
        let tx = first.solana_transaction().unwrap();
        assert!(tx.transaction.signatures.is_empty());
        assert_eq!(
            tx.transaction.message.static_account_keys(),
            &[Address::new_from_array([9; 32])]
        );

        let realised = stub.execute(&first, None).await.unwrap();
        assert_eq!(realised.amount_out, Some(90));
        assert_eq!(realised.cost, TxCost::Solana(SolanaCost::default()));
        let realised = stub.execute(&second, None).await.unwrap();
        assert_eq!(realised.amount_out, Some(80));
    }

    #[tokio::test]
    async fn programmed_revert_carries_its_reason() {
        let stub = DexStub::new();
        stub.program_reverted("insufficient liquidity", 42);

        let prepared = stub.prepare(&route(), &request()).await.unwrap();
        let realised = stub.execute(&prepared, None).await.unwrap();

        assert_eq!(realised.amount_out, None);
        assert_eq!(realised.at, 42);
        match realised.outcome {
            Outcome::Reverted { reason } => assert_eq!(reason, "insufficient liquidity"),
            other => panic!("expected Reverted, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn programmed_timeout_and_expiry_are_forced_on_demand() {
        let stub = DexStub::new();
        stub.program_timed_out(7);
        stub.program_expired(8);

        let prepared = stub.prepare(&solana_route(), &request()).await.unwrap();
        let realised = stub.execute(&prepared, None).await.unwrap();
        assert_eq!(realised.amount_out, None);
        assert_eq!(realised.at, 7);
        assert!(matches!(realised.outcome, Outcome::TimedOut));

        let realised = stub.execute(&prepared, None).await.unwrap();
        assert_eq!(realised.amount_out, None);
        assert_eq!(realised.at, 8);
        assert_eq!(realised.outcome, Outcome::Expired);
    }

    #[tokio::test]
    async fn a_prepared_value_it_did_not_prepare_is_an_error() {
        let stub = DexStub::new();
        let foreign = Prepared::Evm(EvmCall {
            to: vec![7],
            calldata: Vec::new(),
            value: 1,
        });
        assert!(stub.execute(&foreign, None).await.is_err());
    }

    #[tokio::test]
    async fn records_every_call_it_receives() {
        let stub = DexStub::new();
        let prepared = stub.prepare(&route(), &request()).await.unwrap();
        stub.execute(&prepared, None).await.unwrap();

        let calls = stub.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].route.network, Network::evm(1));
        assert_eq!(calls[0].request.min_amount_out, 90);
        assert_eq!(calls[0].prepared.evm_call().unwrap().value, 90);
    }
}
