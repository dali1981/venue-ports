//! §7's shared contract-test suite. Every `DexExecutor`/`CexExecutor`
//! implementation must pass the corresponding function here — run against
//! `Stub` unconditionally, against `Simulated` before any `Live` change
//! ships and on a recurring schedule, and against `Live` only as a
//! deliberate, human-triggered run (`SPEC.md` §7's table).

use crate::cex::{CexExecutor, OrderRequest};
use crate::dex::{DexExecutor, Outcome, RouteQuote, SwapRequest};
use crate::Provenance;

/// Inputs for one run of [`dex_executor_contract`] against a given
/// executor.
#[derive(Debug, Clone)]
pub struct DexContractFixture {
    pub route: RouteQuote,
    pub request: SwapRequest,
}

/// Shape assertions every `DexExecutor` implementation must satisfy,
/// regardless of mode (`SPEC.md` §7).
pub async fn dex_executor_contract(executor: &dyn DexExecutor, fixture: DexContractFixture) {
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

/// Inputs for one run of [`cex_executor_contract`] against a given
/// executor.
#[derive(Debug, Clone)]
pub struct CexContractFixture {
    pub request: OrderRequest,
}

/// Shape assertions every `CexExecutor` implementation must satisfy,
/// regardless of mode (`SPEC.md` §7).
pub async fn cex_executor_contract(executor: &dyn CexExecutor, fixture: CexContractFixture) {
    let fill = executor.execute(&fixture.request).await.unwrap();

    assert_eq!(
        fill.order_ref.is_some(),
        fill.provenance == Provenance::Landed
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cex::{CexStub, OrderSide};
    use crate::dex::evm::EvmStub;
    use std::str::FromStr;

    fn dex_fixture() -> DexContractFixture {
        DexContractFixture {
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

    fn cex_fixture() -> CexContractFixture {
        CexContractFixture {
            request: OrderRequest {
                symbol: "SOLUSDT".to_string(),
                side: OrderSide::Buy,
                quantity: rust_decimal::Decimal::from_str("10").unwrap(),
                quoted_price: rust_decimal::Decimal::from_str("150").unwrap(),
            },
        }
    }

    #[tokio::test]
    async fn evm_stub_satisfies_the_contract() {
        dex_executor_contract(&EvmStub::new(), dex_fixture()).await;
    }

    #[tokio::test]
    async fn cex_stub_satisfies_the_contract() {
        cex_executor_contract(&CexStub::new(), cex_fixture()).await;
    }
}
