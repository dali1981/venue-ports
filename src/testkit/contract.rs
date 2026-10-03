//! §7's shared contract-test suites. Every `DexExecutor`/`CexExecutor`/
//! `LiquidityExecutor` implementation must pass the corresponding function
//! here — run against
//! `Stub` unconditionally, against `Simulated` before any `Live` change
//! ships and on a recurring schedule, and against `Live` only as a
//! deliberate, human-triggered run (`SPEC.md` §7's table).

use crate::cex::{CexAccount, MarginMode};
use crate::cex::{CexExecutor, CexFill, OrderRequest, OrderStateUnknown};
use crate::dex::{DexExecutor, Outcome, RouteQuote, SwapRequest};
use crate::liquidity::{
    Deposit, DepositGuard, DepositGuardKind, LiquidityCommand, LiquidityEvent, LiquidityExecutor,
    LiquidityReport, LiquidityRequest, PositionId, Range, TokenPair,
};
use crate::Provenance;
use std::collections::HashSet;

/// Whether the executor under test sends a transaction for each command,
/// which decides whether its reports carry a `tx_ref` (`SPEC.md` §7). The
/// report alone cannot say: a send to a fork and a throwaway run are both
/// `Simulated`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sends {
    /// A throwaway run (an `eth_call`, `simulateTransaction`), a stub, or a
    /// consumer's paper model. No report carries a `tx_ref`.
    Nothing,
    /// A transaction, to the chain (`Landed`) or to a fork (`Simulated`).
    /// Every report carries its hash or signature.
    Transactions,
}

/// The `tx_ref` shape rule: `Some` exactly when a transaction was sent, so
/// always on a `Landed` report.
fn assert_tx_ref_shape(
    tx_ref: &Option<Vec<u8>>,
    provenance: Provenance,
    sends: Sends,
    report: &dyn std::fmt::Debug,
) {
    assert_eq!(
        tx_ref.is_some(),
        sends == Sends::Transactions,
        "tx_ref against {sends:?}: {report:?}"
    );
    if provenance == Provenance::Landed {
        assert_eq!(
            sends,
            Sends::Transactions,
            "Landed with nothing sent: {report:?}"
        );
    }
}

/// Inputs for one run of [`dex_executor_contract`] against a given
/// executor.
#[derive(Debug, Clone)]
pub struct DexContractFixture {
    pub route: RouteQuote,
    pub request: SwapRequest,
}

/// Shape assertions every `DexExecutor` implementation must satisfy,
/// regardless of mode (`SPEC.md` §7). `sends` is what the executor does with
/// the swap.
pub async fn dex_executor_contract(
    executor: &dyn DexExecutor,
    sends: Sends,
    fixture: DexContractFixture,
) {
    let prepared = executor
        .prepare(&fixture.route, &fixture.request)
        .await
        .unwrap();
    let realised = executor.execute(&prepared, None).await.unwrap();

    assert_eq!(
        realised.amount_out.is_some(),
        matches!(realised.outcome, Outcome::Success)
    );
    assert_tx_ref_shape(&realised.tx_ref, realised.provenance, sends, &realised);
}

/// Inputs for one run of [`liquidity_executor_contract`]: the range to open,
/// who acts, the deposit maxima for the open and for the add that follows
/// it, and the sqrt-price band (Q64.64) a venue that enforces one is given.
#[derive(Debug, Clone)]
pub struct LiquidityContractFixture {
    pub range: Range,
    pub request: LiquidityRequest,
    pub open: TokenPair,
    pub add: TokenPair,
    pub sqrt_price_band_x64: (u128, u128),
}

/// The shape rules every liquidity report must satisfy (`SPEC.md` §5b):
/// `event` is `Some` exactly when the outcome is `Success`, and `tx_ref` is
/// `Some` exactly when a transaction was sent, which `sends` says.
pub fn assert_liquidity_shape(report: &LiquidityReport, sends: Sends) {
    let success = matches!(report.outcome, Outcome::Success);
    assert_eq!(report.event.is_some(), success, "{report:?}");
    assert_tx_ref_shape(&report.tx_ref, report.provenance, sends, report);
}

/// Prepares and executes one command, asserts the shape rules, and requires
/// `Success`: the sequence only succeeds. Returns its event.
async fn succeed(
    executor: &dyn LiquidityExecutor,
    sends: Sends,
    cmd: LiquidityCommand,
    request: &LiquidityRequest,
) -> LiquidityEvent {
    let kind = cmd.kind();
    let prepared = executor
        .prepare(&cmd, request)
        .await
        .unwrap_or_else(|err| panic!("prepare({kind}) on {} failed: {err:#}", executor.label()));
    let report = executor
        .execute(&prepared)
        .await
        .unwrap_or_else(|err| panic!("execute({kind}) on {} failed: {err:#}", executor.label()));
    assert_liquidity_shape(&report, sends);
    assert!(
        matches!(report.outcome, Outcome::Success),
        "{kind} on {} did not succeed: {report:?}",
        executor.label()
    );
    report.event.expect("a success carries its event")
}

/// A command the position cannot take is an `Err` from `prepare`, with
/// nothing sent.
async fn refused(
    executor: &dyn LiquidityExecutor,
    cmd: LiquidityCommand,
    request: &LiquidityRequest,
) {
    let kind = cmd.kind();
    assert!(
        executor.prepare(&cmd, request).await.is_err(),
        "{kind} should have been refused before sending on {}",
        executor.label()
    );
}

/// One sequence against any liquidity venue (`SPEC.md` §7, V5 §8), with no
/// swap during it:
///
/// 1. `Open`, then `Add`, then `Remove` of all the liquidity, then
///    `Collect`, then `Close` — answered `Opened`, `Added`, `Removed`,
///    `Collected`, `Closed` on every venue.
/// 2. `Close` while the position holds liquidity, and `Remove` above what it
///    holds, are each refused before sending.
/// 3. The accounting: `Removed.released` is what `Opened` and `Added` paid,
///    less at most one unit per token per deposit (each venue rounds a
///    deposit up and a withdrawal down); everything `Removed` and
///    `Collected` transferred is at least what was released; and the
///    liquidity `Removed` takes out is what `Opened` and `Added` put in.
/// 4. The shape rules on every report, `tx_ref` by `sends`: what the
///    executor does with each command.
///
/// The guard is built from the venue's capabilities, the one branch a
/// caller makes: no minimum on a `MinAmounts` venue, the fixture's band on a
/// `SqrtPriceBand` one.
pub async fn liquidity_executor_contract(
    executor: &dyn LiquidityExecutor,
    sends: Sends,
    fixture: LiquidityContractFixture,
) {
    let request = &fixture.request;
    let guard = match executor.capabilities().deposit_guard {
        DepositGuardKind::MinAmounts => DepositGuard::MinAmounts(TokenPair::default()),
        DepositGuardKind::SqrtPriceBand => DepositGuard::SqrtPriceBand {
            min_sqrt_price_x64: fixture.sqrt_price_band_x64.0,
            max_sqrt_price_x64: fixture.sqrt_price_band_x64.1,
        },
    };
    let deposit = |max| Deposit {
        max,
        guard: guard.clone(),
    };

    let LiquidityEvent::Opened {
        position,
        liquidity: opened,
        paid: paid_open,
    } = succeed(
        executor,
        sends,
        LiquidityCommand::Open {
            range: fixture.range.clone(),
            deposit: deposit(fixture.open),
        },
        request,
    )
    .await
    else {
        panic!("Open on {} did not answer Opened", executor.label());
    };
    assert!(
        opened > 0,
        "Open added no liquidity on {}",
        executor.label()
    );
    let position: PositionId = position;
    refused(
        executor,
        LiquidityCommand::Close {
            position: position.clone(),
        },
        request,
    )
    .await;

    let LiquidityEvent::Added {
        liquidity: added,
        paid: paid_add,
    } = succeed(
        executor,
        sends,
        LiquidityCommand::Add {
            position: position.clone(),
            deposit: deposit(fixture.add),
        },
        request,
    )
    .await
    else {
        panic!("Add on {} did not answer Added", executor.label());
    };
    let total = opened + added;
    refused(
        executor,
        LiquidityCommand::Remove {
            position: position.clone(),
            liquidity: total + 1,
            min_out: TokenPair::default(),
        },
        request,
    )
    .await;

    let LiquidityEvent::Removed {
        liquidity: removed,
        released,
        transferred: transferred_remove,
    } = succeed(
        executor,
        sends,
        LiquidityCommand::Remove {
            position: position.clone(),
            liquidity: total,
            min_out: TokenPair::default(),
        },
        request,
    )
    .await
    else {
        panic!("Remove on {} did not answer Removed", executor.label());
    };
    assert_eq!(
        removed,
        total,
        "liquidity is conserved on {}",
        executor.label()
    );

    let LiquidityEvent::Collected {
        transferred: transferred_collect,
    } = succeed(
        executor,
        sends,
        LiquidityCommand::Collect {
            position: position.clone(),
        },
        request,
    )
    .await
    else {
        panic!("Collect on {} did not answer Collected", executor.label());
    };

    let closed = succeed(
        executor,
        sends,
        LiquidityCommand::Close { position },
        request,
    )
    .await;
    assert_eq!(closed, LiquidityEvent::Closed, "on {}", executor.label());

    // Two deposits, each rounded up, and one withdrawal rounded down.
    let paid = paid_open.saturating_add(paid_add);
    for (name, paid, released) in [
        ("token0", paid.token0, released.token0),
        ("token1", paid.token1, released.token1),
    ] {
        assert!(
            released <= paid && paid - released <= 2,
            "{name} on {}: paid {paid}, released {released}",
            executor.label()
        );
    }
    let transferred = transferred_remove.saturating_add(transferred_collect);
    assert!(
        transferred.token0 >= released.token0 && transferred.token1 >= released.token1,
        "on {}: transferred {transferred:?}, released {released:?}",
        executor.label()
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
    assert_fill_shape(&fill, executor.label());
}

/// The shape rules of a `CexFill` (`SPEC.md` §6): what a landed fill names
/// and a simulated one does not, and that listed trades add up to the fill.
pub fn assert_fill_shape(fill: &CexFill, label: &str) {
    let landed = fill.provenance == Provenance::Landed;
    assert_eq!(fill.order_ref.is_some(), landed, "on {label}: order_ref");
    assert_eq!(
        fill.client_order_id.is_some(),
        landed,
        "on {label}: client_order_id"
    );
    if !landed {
        assert!(
            fill.venue_time_ms.is_none() && fill.trades.is_empty(),
            "on {label}: nothing was sent, so no venue time and no trades: {fill:?}"
        );
    }
    if !fill.trades.is_empty() {
        let listed: rust_decimal::Decimal = fill.trades.iter().map(|trade| trade.qty).sum();
        assert_eq!(
            listed, fill.filled_qty,
            "on {label}: the trades add up to the fill"
        );
    }
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

/// Inputs for one run of [`cex_account_contract`] against a given account.
#[derive(Debug, Clone)]
pub struct CexAccountContractFixture {
    pub symbol: String,
    /// Where `funding_since` starts.
    pub since_ms: i64,
}

/// Shape assertions every `CexAccount` implementation must satisfy,
/// whatever the account holds (`SPEC.md` §7): the isolated margin is there
/// exactly when the position is isolated; a flat position has no
/// liquidation price; funding is sorted by time, none before `since_ms`,
/// no `venue_ref` twice; and no asset is ever empty.
pub async fn cex_account_contract(account: &dyn CexAccount, fixture: CexAccountContractFixture) {
    let position = account.position(&fixture.symbol).await.unwrap();
    assert_eq!(
        position.isolated_margin.is_some(),
        position.margin_mode == MarginMode::Isolated,
        "isolated_margin must be set exactly when the position is isolated: {position:?}"
    );
    if position.qty.is_zero() {
        assert!(
            position.liquidation_price.is_none(),
            "a flat position has no liquidation price: {position:?}"
        );
    }

    let margin = account.margin().await.unwrap();
    assert!(!margin.asset.is_empty(), "margin with no asset: {margin:?}");

    let funding = account
        .funding_since(&fixture.symbol, fixture.since_ms)
        .await
        .unwrap();
    assert!(
        funding
            .windows(2)
            .all(|pair| pair[0].ts_ms <= pair[1].ts_ms),
        "funding must be oldest first"
    );
    assert!(
        funding
            .iter()
            .all(|payment| payment.ts_ms >= fixture.since_ms),
        "funding from before since_ms"
    );
    let refs: HashSet<u64> = funding.iter().map(|payment| payment.venue_ref).collect();
    assert_eq!(refs.len(), funding.len(), "a venue_ref appears twice");
    assert!(
        funding.iter().all(|payment| !payment.asset.is_empty()),
        "a funding payment with no asset"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cex::{CexStub, OrderSide};
    use crate::dex::{DexStub, Payer};
    use crate::Network;
    use std::str::FromStr;

    fn dex_fixture() -> DexContractFixture {
        DexContractFixture {
            route: RouteQuote {
                network: Network::evm(1),
                token_in: vec![1],
                token_out: vec![2],
                amount_in: 100,
                expected_amount_out: 90,
                payload: Vec::new(),
            },
            request: SwapRequest {
                sender: vec![3],
                recipient: vec![4],
                payer: Payer::Sender,
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
    async fn dex_stub_satisfies_the_contract() {
        dex_executor_contract(&DexStub::new(), Sends::Nothing, dex_fixture()).await;
    }

    /// The stub, programmed with one consistent life: the suite asserts the
    /// events and the relations between them, so the programmed numbers must
    /// agree with each other the way a real venue's would.
    #[tokio::test]
    async fn liquidity_stub_satisfies_the_contract() {
        use crate::liquidity::{unix_now, LiquidityCapabilities, LiquidityStub};

        for capabilities in [
            LiquidityCapabilities::UNISWAP_V3,
            LiquidityCapabilities::WHIRLPOOL,
        ] {
            let position = PositionId {
                network: Network::evm(1),
                bytes: vec![7; 32],
            };
            let released = TokenPair::new(74, 89);
            // On a venue whose Remove transfers, the principal leaves at once
            // and Collect pays only the fees; otherwise Collect pays both.
            let (on_remove, on_collect) = if capabilities.remove_transfers {
                (released, TokenPair::default())
            } else {
                (TokenPair::default(), released)
            };
            let stub = LiquidityStub::new(capabilities);
            stub.program_event(
                LiquidityEvent::Opened {
                    position: position.clone(),
                    liquidity: 1_000,
                    paid: TokenPair::new(50, 60),
                },
                10,
            );
            stub.program_event(
                LiquidityEvent::Added {
                    liquidity: 500,
                    paid: TokenPair::new(25, 30),
                },
                11,
            );
            stub.program_event(
                LiquidityEvent::Removed {
                    liquidity: 1_500,
                    released,
                    transferred: on_remove,
                },
                12,
            );
            stub.program_event(
                LiquidityEvent::Collected {
                    transferred: on_collect,
                },
                13,
            );
            stub.program_event(LiquidityEvent::Closed, 14);

            liquidity_executor_contract(
                &stub,
                Sends::Nothing,
                LiquidityContractFixture {
                    range: Range {
                        network: Network::evm(1),
                        pool: vec![0x22; 20],
                        tick_lower: -600,
                        tick_upper: 600,
                    },
                    request: LiquidityRequest {
                        owner: vec![0xCC; 20],
                        deadline_unix_secs: unix_now() + 600,
                    },
                    open: TokenPair::new(50, 60),
                    add: TokenPair::new(25, 30),
                    sqrt_price_band_x64: (1 << 63, 1 << 65),
                },
            )
            .await;
            // Five commands prepared and run, two refused.
            assert_eq!(stub.calls().len(), 12);
        }
    }

    #[tokio::test]
    async fn cex_stub_satisfies_the_contract() {
        cex_executor_contract(&CexStub::new(), cex_fixture()).await;
    }

    /// Runs against a flat cross position and an isolated short, with
    /// funding pushed out of order and partly before `since_ms`, which the
    /// stub must filter and sort as a venue adapter does.
    #[tokio::test]
    async fn cex_account_stub_satisfies_the_contract() {
        use crate::cex::{CexAccountStub, FundingPayment, MarginState, PerpPosition};
        use rust_decimal::Decimal;

        let flat = PerpPosition {
            symbol: "BTCUSDT".to_string(),
            qty: Decimal::ZERO,
            entry_price: Decimal::ZERO,
            mark_price: Decimal::from(60_000),
            liquidation_price: None,
            margin_mode: MarginMode::Cross,
            leverage: 20,
            isolated_margin: None,
            as_of_ms: 1_000,
        };
        let isolated_short = PerpPosition {
            qty: Decimal::new(-5, 3),
            entry_price: Decimal::from(60_000),
            liquidation_price: Some(Decimal::from(95_000)),
            margin_mode: MarginMode::Isolated,
            leverage: 5,
            isolated_margin: Some(Decimal::from(60)),
            ..flat.clone()
        };

        for position in [flat, isolated_short] {
            let stub = CexAccountStub::new();
            stub.set_position(position);
            stub.set_margin(MarginState {
                asset: "USDT".to_string(),
                margin_balance: Decimal::from(1_000),
                maint_margin: Decimal::from(2),
                available: Decimal::from(900),
                as_of_ms: 1_000,
            });
            for (venue_ref, ts_ms) in [(3, 300), (1, 100), (2, 200), (0, 50)] {
                stub.push_funding(FundingPayment {
                    symbol: "BTCUSDT".to_string(),
                    ts_ms,
                    amount: Decimal::new(-12, 2),
                    asset: "USDT".to_string(),
                    venue_ref,
                });
            }

            cex_account_contract(
                &stub,
                CexAccountContractFixture {
                    symbol: "BTCUSDT".to_string(),
                    since_ms: 100,
                },
            )
            .await;
        }
    }
}
