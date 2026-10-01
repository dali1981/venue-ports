//! `LiquidityStub` — an in-process fake `LiquidityExecutor` with no network
//! calls (`SPEC.md` §5b). Each `execute()` returns the next programmed
//! outcome: success with given values, `Reverted { reason }`, `TimedOut`, an
//! `Err`, or `LandedUnread`. It records every `prepare` and `execute` it
//! receives.
//!
//! **An action with no programmed outcome is an `Err` naming the action.**
//! Unlike `DexStub` and `CexStub`, there is no default: a fallback would
//! have to invent liquidity and amounts, and invented numbers in a test are
//! how a fake drifts away from the real thing.
//!
//! `prepare` makes the same chain-free checks as `EvmLiquidity` (owner,
//! deadline, token order, tick order, amounts), so a test against the stub
//! is refused what the real adapter would refuse.

use crate::dex::{ChainAmount, EvmCall, Outcome, Prepared};
use crate::liquidity::{
    validate, LandedUnread, LiquidityAction, LiquidityExecutor, LiquidityRealised,
    LiquidityRequest, PositionRef,
};
use crate::Provenance;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

/// One call exactly as the stub received it.
#[derive(Debug, Clone)]
pub enum LiquidityCall {
    Prepare {
        action: LiquidityAction,
        request: LiquidityRequest,
        /// `None` when `prepare` refused the action.
        prepared: Option<Prepared>,
    },
    Execute {
        prepared: Prepared,
        /// The action `prepared` came from, when this stub prepared it.
        action: Option<LiquidityAction>,
    },
}

#[derive(Default)]
pub struct LiquidityStub {
    programmed: Mutex<VecDeque<Result<LiquidityRealised>>>,
    calls: Mutex<Vec<LiquidityCall>>,
    prepared: Mutex<HashMap<Vec<u8>, LiquidityAction>>,
}

impl LiquidityStub {
    pub fn new() -> Self {
        Self::default()
    }

    /// Program the exact result the next `execute()` call returns.
    /// Consumed in the order programmed.
    pub fn program_execute(&self, result: Result<LiquidityRealised>) {
        self.programmed.lock().unwrap().push_back(result);
    }

    /// Program a success. For `Collect` and `Burn`, `liquidity_delta` is
    /// zero, and for `Burn` so are both amounts (`SPEC.md` §5b).
    pub fn program_success(
        &self,
        position: PositionRef,
        liquidity_delta: u128,
        amount0: ChainAmount,
        amount1: ChainAmount,
        at: u64,
    ) {
        self.program_execute(Ok(LiquidityRealised {
            outcome: Outcome::Success,
            position: Some(position),
            liquidity_delta: Some(liquidity_delta),
            amount0: Some(amount0),
            amount1: Some(amount1),
            at,
            provenance: Provenance::Simulated,
            tx_ref: None,
        }));
    }

    /// Program a revert with a specific reason, e.g. `"Price slippage
    /// check"` or `"Not cleared"`.
    pub fn program_reverted(&self, reason: impl Into<String>, at: u64) {
        self.program_execute(Ok(unsettled(
            Outcome::Reverted {
                reason: reason.into(),
            },
            at,
        )));
    }

    /// Program a forced timeout: the transaction's fate is unknown.
    pub fn program_timed_out(&self, at: u64) {
        self.program_execute(Ok(unsettled(Outcome::TimedOut, at)));
    }

    /// Program a plain error: nothing was sent for the action.
    pub fn program_error(&self, reason: impl std::fmt::Display) {
        self.program_execute(Err(anyhow!("{reason}")));
    }

    /// Program a [`LandedUnread`]: the action landed, but its outcome could
    /// not be read.
    pub fn program_landed_unread(&self, tx_ref: Vec<u8>, reason: impl Into<String>) {
        self.program_execute(Err(LandedUnread {
            tx_ref,
            reason: reason.into(),
        }
        .into()));
    }

    /// Every call the stub has received so far, in the order received.
    pub fn calls(&self) -> Vec<LiquidityCall> {
        self.calls.lock().unwrap().clone()
    }
}

fn unsettled(outcome: Outcome, at: u64) -> LiquidityRealised {
    LiquidityRealised {
        outcome,
        position: None,
        liquidity_delta: None,
        amount0: None,
        amount1: None,
        at,
        provenance: Provenance::Simulated,
        tx_ref: None,
    }
}

#[async_trait]
impl LiquidityExecutor for LiquidityStub {
    async fn prepare(&self, action: &LiquidityAction, req: &LiquidityRequest) -> Result<Prepared> {
        let result = validate(action, req).map(|()| {
            let mut prepared = self.prepared.lock().unwrap();
            // Distinct calldata per prepared action, so each `execute` can
            // be traced back to the action it came from.
            let calldata = format!("{}#{}", action.kind(), prepared.len()).into_bytes();
            prepared.insert(calldata.clone(), action.clone());
            Prepared::Evm(EvmCall {
                to: action.manager().clone(),
                calldata,
                value: 0,
            })
        });
        self.calls.lock().unwrap().push(LiquidityCall::Prepare {
            action: action.clone(),
            request: req.clone(),
            prepared: result.as_ref().ok().cloned(),
        });
        result
    }

    async fn execute(&self, prepared: &Prepared) -> Result<LiquidityRealised> {
        let action = self
            .prepared
            .lock()
            .unwrap()
            .get(&prepared.evm_call().map(|call| call.calldata.clone()).unwrap_or_default())
            .cloned();
        self.calls.lock().unwrap().push(LiquidityCall::Execute {
            prepared: prepared.clone(),
            action: action.clone(),
        });
        match self.programmed.lock().unwrap().pop_front() {
            Some(result) => result,
            None => Err(anyhow!(
                "LiquidityStub has no programmed outcome for {} — program one; the stub never \
                 invents liquidity or amounts",
                action
                    .map(|a| format!("{a:?}"))
                    .unwrap_or_else(|| "a Prepared value it did not prepare".to_string())
            )),
        }
    }

    fn label(&self) -> &'static str {
        "liquidity-stub"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::liquidity::{unix_now, PoolKey, RangeSpec};

    fn position() -> PositionRef {
        PositionRef {
            chain_id: 1,
            manager: vec![0x11; 20],
            id: vec![0; 32],
        }
    }

    fn mint() -> LiquidityAction {
        LiquidityAction::Mint {
            range: RangeSpec {
                chain_id: 1,
                manager: vec![0x11; 20],
                token0: vec![0xA0; 20],
                token1: vec![0xB0; 20],
                pool_key: PoolKey::Fee(3_000),
                tick_lower: -60,
                tick_upper: 60,
            },
            amount0_desired: 100,
            amount1_desired: 100,
            amount0_min: 0,
            amount1_min: 0,
        }
    }

    fn request() -> LiquidityRequest {
        LiquidityRequest {
            owner: vec![0xCC; 20],
            deadline_unix_secs: unix_now() + 600,
        }
    }

    #[tokio::test]
    async fn an_action_with_no_programmed_outcome_is_an_error_naming_it() {
        let stub = LiquidityStub::new();
        let prepared = stub.prepare(&mint(), &request()).await.unwrap();

        let err = stub.execute(&prepared).await.unwrap_err();

        assert!(err.to_string().contains("Mint"));
        assert!(err.to_string().contains("no programmed outcome"));
    }

    #[tokio::test]
    async fn returns_each_programmed_outcome_in_order() {
        let stub = LiquidityStub::new();
        stub.program_success(position(), 1_000, 40, 60, 7);
        stub.program_reverted("Price slippage check", 8);
        stub.program_timed_out(9);
        stub.program_landed_unread(vec![0xAB; 32], "no IncreaseLiquidity event");
        stub.program_error("approve reverted");

        let prepared = stub.prepare(&mint(), &request()).await.unwrap();
        let success = stub.execute(&prepared).await.unwrap();
        assert!(matches!(success.outcome, Outcome::Success));
        assert_eq!(success.liquidity_delta, Some(1_000));
        assert_eq!(
            (success.amount0, success.amount1, success.at),
            (Some(40), Some(60), 7)
        );

        let reverted = stub.execute(&prepared).await.unwrap();
        match reverted.outcome {
            Outcome::Reverted { reason } => assert_eq!(reason, "Price slippage check"),
            other => panic!("expected Reverted, got {other:?}"),
        }
        assert_eq!(reverted.position, None);
        assert_eq!(reverted.amount0, None);

        let timed_out = stub.execute(&prepared).await.unwrap();
        assert!(matches!(timed_out.outcome, Outcome::TimedOut));
        assert_eq!(timed_out.liquidity_delta, None);

        let unread = stub.execute(&prepared).await.unwrap_err();
        assert_eq!(
            unread
                .downcast_ref::<LandedUnread>()
                .map(|u| u.tx_ref.clone()),
            Some(vec![0xAB; 32])
        );

        let plain = stub.execute(&prepared).await.unwrap_err();
        assert!(plain.downcast_ref::<LandedUnread>().is_none());
    }

    #[tokio::test]
    async fn records_every_prepare_and_execute() {
        let stub = LiquidityStub::new();
        stub.program_success(position(), 1, 1, 1, 1);
        let prepared = stub.prepare(&mint(), &request()).await.unwrap();
        stub.execute(&prepared).await.unwrap();
        let refused = LiquidityRequest {
            deadline_unix_secs: 0,
            ..request()
        };
        assert!(stub.prepare(&mint(), &refused).await.is_err());

        let calls = stub.calls();
        assert_eq!(calls.len(), 3);
        assert!(matches!(
            &calls[0],
            LiquidityCall::Prepare { prepared: Some(p), .. } if p == &prepared
        ));
        assert!(matches!(
            &calls[1],
            LiquidityCall::Execute {
                action: Some(LiquidityAction::Mint { .. }),
                ..
            }
        ));
        assert!(matches!(
            &calls[2],
            LiquidityCall::Prepare { prepared: None, .. }
        ));
    }
}
