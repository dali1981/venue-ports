//! §7's shared contract-test suites. Every `DexExecutor`/`CexExecutor`/
//! `LiquidityExecutor` implementation must pass the corresponding function
//! here — run against
//! `Stub` unconditionally, against `Simulated` before any `Live` change
//! ships and on a recurring schedule, and against `Live` only as a
//! deliberate, human-triggered run (`SPEC.md` §7's table).

use crate::cex::{CexExecutor, OrderRequest, OrderStateUnknown};
use crate::dex::{ChainAmount, DexExecutor, Outcome, RouteQuote, SwapRequest};
use crate::liquidity::{
    LiquidityAction, LiquidityExecutor, LiquidityRealised, LiquidityRequest, PositionRef, RangeSpec,
};
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

/// Inputs for one run of [`liquidity_executor_contract`]: the range to
/// mint in, who acts, and the desired `(amount0, amount1)` for the mint and
/// for the increase that follows it.
#[derive(Debug, Clone)]
pub struct LiquidityContractFixture {
    pub range: RangeSpec,
    pub request: LiquidityRequest,
    pub mint: (ChainAmount, ChainAmount),
    pub increase: (ChainAmount, ChainAmount),
}

/// The shape rules every `LiquidityExecutor` outcome must satisfy
/// (`SPEC.md` §5b): the position, liquidity delta and both amounts are all
/// `Some` exactly when the outcome is `Success`, and `tx_ref` is `Some`
/// exactly when the provenance is `Landed`.
pub fn assert_liquidity_shape(realised: &LiquidityRealised) {
    let success = matches!(realised.outcome, Outcome::Success);
    assert_eq!(realised.position.is_some(), success, "{realised:?}");
    assert_eq!(realised.liquidity_delta.is_some(), success, "{realised:?}");
    assert_eq!(realised.amount0.is_some(), success, "{realised:?}");
    assert_eq!(realised.amount1.is_some(), success, "{realised:?}");
    assert_eq!(
        realised.tx_ref.is_some(),
        realised.provenance == Provenance::Landed,
        "{realised:?}"
    );
}

/// Prepares and executes one action, asserts the shape rules, and requires
/// `Success`: the contract suite drives a life that only succeeds.
async fn succeed(
    executor: &dyn LiquidityExecutor,
    action: LiquidityAction,
    request: &LiquidityRequest,
) -> LiquidityRealised {
    let kind = action.kind();
    let prepared = executor
        .prepare(&action, request)
        .await
        .unwrap_or_else(|err| panic!("prepare({kind}) failed: {err:#}"));
    let realised = executor
        .execute(&prepared)
        .await
        .unwrap_or_else(|err| panic!("execute({kind}) failed: {err:#}"));
    assert_liquidity_shape(&realised);
    assert!(
        matches!(realised.outcome, Outcome::Success),
        "{kind} did not succeed: {realised:?}"
    );
    realised
}

/// One whole position life against any `LiquidityExecutor` (`SPEC.md` §7):
/// mint, increase, decrease all of it, collect everything (`u128::MAX`
/// caps), burn — with the shape rules at every step, and across the steps:
///
/// - mint gives a position and liquidity above zero, and every later step
///   returns that same position;
/// - decrease removes exactly the sum of what mint and increase added;
/// - collect returns, per token, at least what decrease credited (the
///   principal plus fees of zero or more);
/// - collect and burn change no liquidity, and burn moves no tokens.
pub async fn liquidity_executor_contract(
    executor: &dyn LiquidityExecutor,
    fixture: LiquidityContractFixture,
) {
    let request = &fixture.request;
    let mint = succeed(
        executor,
        LiquidityAction::Mint {
            range: fixture.range.clone(),
            amount0_desired: fixture.mint.0,
            amount1_desired: fixture.mint.1,
            amount0_min: 0,
            amount1_min: 0,
        },
        request,
    )
    .await;
    let position: PositionRef = mint.position.clone().unwrap();
    let minted = mint.liquidity_delta.unwrap();
    assert!(minted > 0, "mint added no liquidity: {mint:?}");

    let increase = succeed(
        executor,
        LiquidityAction::Increase {
            position: position.clone(),
            amount0_desired: fixture.increase.0,
            amount1_desired: fixture.increase.1,
            amount0_min: 0,
            amount1_min: 0,
        },
        request,
    )
    .await;
    assert_eq!(increase.position.as_ref(), Some(&position));
    let total = minted + increase.liquidity_delta.unwrap();

    let decrease = succeed(
        executor,
        LiquidityAction::Decrease {
            position: position.clone(),
            liquidity: total,
            amount0_min: 0,
            amount1_min: 0,
        },
        request,
    )
    .await;
    assert_eq!(decrease.position.as_ref(), Some(&position));
    assert_eq!(decrease.liquidity_delta, Some(total));

    let collect = succeed(
        executor,
        LiquidityAction::Collect {
            position: position.clone(),
            amount0_max: u128::MAX,
            amount1_max: u128::MAX,
        },
        request,
    )
    .await;
    assert_eq!(collect.position.as_ref(), Some(&position));
    assert_eq!(collect.liquidity_delta, Some(0));
    assert!(collect.amount0.unwrap() >= decrease.amount0.unwrap());
    assert!(collect.amount1.unwrap() >= decrease.amount1.unwrap());

    let burn = succeed(
        executor,
        LiquidityAction::Burn {
            position: position.clone(),
        },
        request,
    )
    .await;
    assert_eq!(burn.position.as_ref(), Some(&position));
    assert_eq!(
        (burn.liquidity_delta, burn.amount0, burn.amount1),
        (Some(0), Some(0), Some(0))
    );
}

/// Inputs for one run of [`cex_executor_contract`] against a given
/// executor.
#[derive(Debug, Clone)]
pub struct CexContractFixture {
    pub request: OrderRequest,
}

/// Shape assertions every `CexExecutor` implementation must satisfy,
/// regardless of mode (`SPEC.md` §7). Runs the fixture's order with
/// `reduce_only: false`, whatever the fixture says, so it means the same
/// thing on a spot venue and a perp one.
pub async fn cex_executor_contract(executor: &dyn CexExecutor, fixture: CexContractFixture) {
    let request = OrderRequest {
        reduce_only: false,
        ..fixture.request
    };
    let fill = executor.execute(&request).await.unwrap();

    assert_eq!(
        fill.order_ref.is_some(),
        fill.provenance == Provenance::Landed
    );
}

/// Every spot `CexExecutor` must refuse `reduce_only: true` with a plain
/// error — nothing filled, so never an [`OrderStateUnknown`] — rather than
/// send an order it cannot guard (`SPEC.md` §6). That nothing reached the
/// venue is the caller's to assert, e.g. with a mock server that expects no
/// request.
pub async fn cex_spot_rejects_reduce_only(executor: &dyn CexExecutor, fixture: CexContractFixture) {
    let request = OrderRequest {
        reduce_only: true,
        ..fixture.request
    };
    let err = executor
        .execute(&request)
        .await
        .expect_err("a spot venue must reject reduce_only: true");

    assert!(
        err.downcast_ref::<OrderStateUnknown>().is_none(),
        "a refusal before sending is not an unknown order state: {err}"
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
                reduce_only: false,
            },
        }
    }

    #[tokio::test]
    async fn evm_stub_satisfies_the_contract() {
        dex_executor_contract(&EvmStub::new(), dex_fixture()).await;
    }

    /// The stub, programmed with one consistent life: the suite asserts
    /// the shape and the relations between steps, so the programmed numbers
    /// must agree with each other the way a real manager's would.
    #[tokio::test]
    async fn liquidity_stub_satisfies_the_contract() {
        use crate::liquidity::{unix_now, LiquidityStub, PoolKey};

        let range = RangeSpec {
            chain_id: 1,
            manager: vec![0x11; 20],
            token0: vec![0xA0; 20],
            token1: vec![0xB0; 20],
            pool_key: PoolKey::Fee(3_000),
            tick_lower: -600,
            tick_upper: 600,
        };
        let position = PositionRef {
            chain_id: 1,
            manager: range.manager.clone(),
            id: {
                let mut id = vec![0; 32];
                id[31] = 7;
                id
            },
        };
        let stub = LiquidityStub::new();
        stub.program_success(position.clone(), 1_000, 50, 60, 10);
        stub.program_success(position.clone(), 500, 25, 30, 11);
        stub.program_success(position.clone(), 1_500, 74, 89, 12);
        stub.program_success(position.clone(), 0, 75, 90, 13);
        stub.program_success(position, 0, 0, 0, 14);

        liquidity_executor_contract(
            &stub,
            LiquidityContractFixture {
                range,
                request: LiquidityRequest {
                    owner: vec![0xCC; 20],
                    deadline_unix_secs: unix_now() + 600,
                },
                mint: (50, 60),
                increase: (25, 30),
            },
        )
        .await;
        assert_eq!(stub.calls().len(), 10);
    }

    #[tokio::test]
    async fn cex_stub_satisfies_the_contract() {
        cex_executor_contract(&CexStub::new(), cex_fixture()).await;
    }
}
