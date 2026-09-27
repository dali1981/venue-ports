//! `EvmStub` — an in-process fake `DexExecutor` with no network calls at
//! all (`SPEC.md` §5, third required implementation). A test programs the
//! exact `Realised` (or error) a given `execute()` call returns — including
//! a specific revert reason or a forced `TimedOut` — and can assert on
//! every call the stub actually received.

use crate::dex::{ChainAmount, DexExecutor, Outcome, Prepared, Realised, RouteQuote, SwapRequest};
use crate::Provenance;
use anyhow::Result;
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

#[derive(Default)]
pub struct EvmStub {
    programmed: Mutex<VecDeque<Result<Realised>>>,
    calls: Mutex<Vec<RecordedCall>>,
}

impl EvmStub {
    pub fn new() -> Self {
        Self::default()
    }

    /// Program the exact result the next `execute()` call returns.
    /// Consumed in the order programmed; once exhausted, `execute()` falls
    /// back to a plain success at the request's `min_amount_out`, so a
    /// test that doesn't care about the outcome doesn't have to program
    /// one.
    pub fn program_execute(&self, result: Result<Realised>) {
        self.programmed.lock().unwrap().push_back(result);
    }

    /// Program a successful fill.
    pub fn program_success(&self, amount_out: ChainAmount, at: u64) {
        self.program_execute(Ok(Realised {
            amount_out: Some(amount_out),
            outcome: Outcome::Success,
            at,
            provenance: Provenance::Simulated,
            tx_ref: None,
        }));
    }

    /// Program a revert with a specific reason.
    pub fn program_reverted(&self, reason: impl Into<String>, at: u64) {
        self.program_execute(Ok(Realised {
            amount_out: None,
            outcome: Outcome::Reverted {
                reason: reason.into(),
            },
            at,
            provenance: Provenance::Simulated,
            tx_ref: None,
        }));
    }

    /// Program a forced timeout — the outcome a real adapter would report
    /// when a transaction never reaches a terminal state in time.
    pub fn program_timed_out(&self, at: u64) {
        self.program_execute(Ok(Realised {
            amount_out: None,
            outcome: Outcome::TimedOut,
            at,
            provenance: Provenance::Simulated,
            tx_ref: None,
        }));
    }

    /// Every call the stub has received so far, in the order received.
    pub fn calls(&self) -> Vec<RecordedCall> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl DexExecutor for EvmStub {
    async fn prepare(&self, route: &RouteQuote, req: &SwapRequest) -> Result<Prepared> {
        let prepared = Prepared {
            to: route.token_out.clone(),
            calldata: route.payload.clone(),
            value: req.min_amount_out,
        };
        self.calls.lock().unwrap().push(RecordedCall {
            route: route.clone(),
            request: req.clone(),
            prepared: prepared.clone(),
        });
        Ok(prepared)
    }

    async fn execute(&self, prepared: &Prepared, at: Option<u64>) -> Result<Realised> {
        if let Some(result) = self.programmed.lock().unwrap().pop_front() {
            return result;
        }
        Ok(Realised {
            amount_out: Some(prepared.value),
            outcome: Outcome::Success,
            at: at.unwrap_or(0),
            provenance: Provenance::Simulated,
            tx_ref: None,
        })
    }

    fn label(&self) -> &'static str {
        "evm-stub"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route() -> RouteQuote {
        RouteQuote {
            chain_id: 1,
            token_in: vec![1],
            token_out: vec![2],
            amount_in: 100,
            expected_amount_out: 90,
            payload: vec![0xde, 0xad],
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
        let stub = EvmStub::new();
        let prepared = stub.prepare(&route(), &request()).await.unwrap();
        let realised = stub.execute(&prepared, None).await.unwrap();

        assert!(matches!(realised.outcome, Outcome::Success));
        assert_eq!(realised.amount_out, Some(90));
        assert_eq!(realised.provenance, Provenance::Simulated);
        assert_eq!(realised.tx_ref, None);
    }

    #[tokio::test]
    async fn programmed_revert_carries_its_reason() {
        let stub = EvmStub::new();
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
    async fn programmed_timeout_is_forced_on_demand() {
        let stub = EvmStub::new();
        stub.program_timed_out(7);

        let prepared = stub.prepare(&route(), &request()).await.unwrap();
        let realised = stub.execute(&prepared, None).await.unwrap();

        assert_eq!(realised.amount_out, None);
        assert_eq!(realised.at, 7);
        assert!(matches!(realised.outcome, Outcome::TimedOut));
    }

    #[tokio::test]
    async fn records_every_call_it_receives() {
        let stub = EvmStub::new();
        let prepared = stub.prepare(&route(), &request()).await.unwrap();
        stub.execute(&prepared, None).await.unwrap();

        let calls = stub.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].route.chain_id, 1);
        assert_eq!(calls[0].request.min_amount_out, 90);
        assert_eq!(calls[0].prepared.value, 90);
    }
}
