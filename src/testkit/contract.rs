//! §7's shared contract-test suite. Every `DexExecutor`/`CexExecutor`
//! implementation must pass the corresponding function here — run against
//! `Stub` unconditionally, against `Simulated` before any `Live` change
//! ships and on a recurring schedule, and against `Live` only as a
//! deliberate, human-triggered run (`SPEC.md` §7's table).

use crate::dex::{DexExecutor, Outcome, RouteQuote, SwapRequest};
use crate::Provenance;

/// Inputs for one run of [`dex_executor_contract`] against a given
/// executor.
#[derive(Debug, Clone)]
pub struct ContractFixture {
    pub route: RouteQuote,
    pub request: SwapRequest,
}

/// Shape assertions every `DexExecutor` implementation must satisfy,
/// regardless of mode (`SPEC.md` §7).
pub async fn dex_executor_contract(executor: &dyn DexExecutor, fixture: ContractFixture) {
    let prepared = executor
        .prepare(&fixture.route, &fixture.request)
        .await
        .unwrap();
    let realised = executor.execute(&prepared, None).await.unwrap();

    assert_eq!(
        realised.amount_out.is_some(),
        matches!(realised.outcome, Outcome::Success)
    );
    assert_eq!(
        realised.tx_ref.is_some(),
        realised.provenance == Provenance::Landed
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dex::{Prepared, Realised};
    use anyhow::Result;
    use async_trait::async_trait;

    /// A throwaway mock proving `dex_executor_contract` compiles and holds
    /// against *some* implementation. Delete this once `EvmStub` (Phase 3)
    /// exists to run the suite against for real.
    struct ThrowawayMock;

    #[async_trait]
    impl DexExecutor for ThrowawayMock {
        async fn prepare(&self, route: &RouteQuote, req: &SwapRequest) -> Result<Prepared> {
            Ok(Prepared {
                to: route.token_out.clone(),
                calldata: Vec::new(),
                value: req.min_amount_out,
            })
        }

        async fn execute(&self, prepared: &Prepared, _at: Option<u64>) -> Result<Realised> {
            Ok(Realised {
                amount_out: Some(prepared.value),
                outcome: Outcome::Success,
                at: 0,
                provenance: Provenance::Simulated,
                tx_ref: None,
            })
        }

        fn label(&self) -> &'static str {
            "throwaway-mock"
        }
    }

    fn fixture() -> ContractFixture {
        ContractFixture {
            route: RouteQuote {
                chain_id: 1,
                token_in: vec![1],
                token_out: vec![2],
                amount_in: 100,
                expected_amount_out: 90,
                payload: Vec::new(),
            },
            request: SwapRequest {
                sender: vec![3],
                recipient: vec![4],
                min_amount_out: 90,
                deadline_unix_secs: 0,
            },
        }
    }

    #[tokio::test]
    async fn throwaway_mock_satisfies_the_contract() {
        dex_executor_contract(&ThrowawayMock, fixture()).await;
    }
}
