//! `EvmLiquidity` over a fork sender against a real position manager on an
//! anvil node (`IMPLEMENTATION_PLAN.md` Phases 13 and 15): the contract
//! suite, `SPEC.md` §9.2's bar of 100 lives reconciled to the wei against
//! the chain's own state, and one injected failure per `Outcome` variant.
//!
//! Gated on `EVM_ANVIL_RPC_URL` and the `LIQUIDITY_FORK_*` variables, a
//! no-op when they are unset:
//!
//! | variable | meaning |
//! | --- | --- |
//! | `LIQUIDITY_FORK_MANAGER` | the position manager |
//! | `LIQUIDITY_FORK_TOKEN0`, `LIQUIDITY_FORK_TOKEN1` | the pool's tokens, in its own order |
//! | `LIQUIDITY_FORK_POOL_KEY` | `fee:<hundredths of a bip>` or `tick_spacing:<n>` |
//! | `LIQUIDITY_FORK_TICK_SPACING` | only for a fee tier outside 100/500/3000/10000 |
//! | `LIQUIDITY_FORK_AMOUNT0`, `LIQUIDITY_FORK_AMOUNT1` | base desired amounts (default 10^18) |
//! | `LIQUIDITY_FORK_SWAP_ROUTER` | optional: a Uniswap v3 `SwapRouter`, to move the price and earn fees mid-life |
//! | `LIQUIDITY_FORK_LIVES` | how many lives the §9.2 run drives (default 100) |
//!
//! Against an anvil fork of a real chain these name the deployed contracts.
//! `scripts/anvil-uniswap-v3.py` deploys Uniswap's published v3 bytecode
//! onto a plain anvil node and prints them, for when no fork RPC is at hand.

use crate::dex::evm::EvmLive;
use crate::dex::{DexExecutor, Outcome, Payer, RouteQuote, SwapRequest};
use crate::evm::erc20;
use crate::evm::rpc::{BlockTag, EvmRpc};
use crate::evm::tx::tests::anvil;
use crate::evm::{EvmSender, PollSettings};
use crate::liquidity::uniswap_v3::abi::manager;
use crate::liquidity::{
    unix_now, Deposit, DepositGuard, EvmLiquidity, LiquidityCommand, LiquidityEvent,
    LiquidityExecutor, LiquidityReport, LiquidityRequest, ManagerAbi, PositionId, Range, TokenPair,
};
use crate::testkit::contract::{
    assert_liquidity_shape, liquidity_executor_contract, LiquidityContractFixture,
};
use crate::{Network, Provenance};
use alloy_primitives::aliases::{U160, U24};
use alloy_primitives::{keccak256, Address, U256};
use alloy_sol_types::SolCall;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

alloy_sol_types::sol! {
    /// Uniswap v3 `SwapRouter` (v1), used only to move the price between
    /// liquidity actions.
    struct ExactInputSingleParams {
        address tokenIn;
        address tokenOut;
        uint24 fee;
        address recipient;
        uint256 deadline;
        uint256 amountIn;
        uint256 amountOutMinimum;
        uint160 sqrtPriceLimitX96;
    }
    function exactInputSingle(ExactInputSingleParams calldata params)
        external payable returns (uint256 amountOut);
}

/// The pool's key, from `LIQUIDITY_FORK_POOL_KEY`: what finds the pool in
/// its factory, and which manager ABI the fork speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PoolKey {
    Fee(u32),
    TickSpacing(i32),
}

struct Fork {
    rpc: EvmRpc,
    chain_id: u64,
    manager: Address,
    token0: Address,
    token1: Address,
    pool_key: PoolKey,
    spacing: i32,
    pool: Address,
    amounts: (u128, u128),
    swap_router: Option<Address>,
}

fn env_address(name: &str) -> Option<Address> {
    let value = std::env::var(name).ok()?;
    Some(
        value
            .parse()
            .unwrap_or_else(|_| panic!("{name} is not an address: {value}")),
    )
}

fn selector(signature: &str) -> Vec<u8> {
    keccak256(signature.as_bytes())[..4].to_vec()
}

fn word(value: U256) -> [u8; 32] {
    value.to_be_bytes::<32>()
}

/// The fork named by the environment, and the guard that keeps every
/// anvil test in this process from running alongside this one.
async fn fork() -> Option<(Fork, tokio::sync::MutexGuard<'static, ()>)> {
    let (rpc, guard) = anvil().await?;
    let (Some(manager), Some(token0), Some(token1), Ok(pool_key)) = (
        env_address("LIQUIDITY_FORK_MANAGER"),
        env_address("LIQUIDITY_FORK_TOKEN0"),
        env_address("LIQUIDITY_FORK_TOKEN1"),
        std::env::var("LIQUIDITY_FORK_POOL_KEY"),
    ) else {
        eprintln!("skipping: LIQUIDITY_FORK_MANAGER/TOKEN0/TOKEN1/POOL_KEY are not all set");
        return None;
    };
    let (pool_key, spacing) = match pool_key.split_once(':') {
        Some(("fee", fee)) => {
            let fee: u32 = fee.parse().expect("LIQUIDITY_FORK_POOL_KEY fee");
            let spacing = match (fee, std::env::var("LIQUIDITY_FORK_TICK_SPACING")) {
                (_, Ok(spacing)) => spacing.parse().expect("LIQUIDITY_FORK_TICK_SPACING"),
                (100, _) => 1,
                (500, _) => 10,
                (3_000, _) => 60,
                (10_000, _) => 200,
                (other, _) => panic!("set LIQUIDITY_FORK_TICK_SPACING for fee tier {other}"),
            };
            (PoolKey::Fee(fee), spacing)
        }
        Some(("tick_spacing", spacing)) => {
            let spacing: i32 = spacing
                .parse()
                .expect("LIQUIDITY_FORK_POOL_KEY tick spacing");
            (PoolKey::TickSpacing(spacing), spacing)
        }
        _ => panic!("LIQUIDITY_FORK_POOL_KEY must be fee:<n> or tick_spacing:<n>, not {pool_key}"),
    };
    let amount = |name: &str| {
        std::env::var(name)
            .map(|v| v.parse().expect(name))
            .unwrap_or(1_000_000_000_000_000_000u128)
    };

    // The manager must be a deployed contract that answers `factory()`.
    assert!(
        !rpc.code(manager).await.unwrap().is_empty(),
        "LIQUIDITY_FORK_MANAGER {manager} has no code"
    );
    let factory_return = rpc
        .eth_call(
            manager,
            &manager::factoryCall {}.abi_encode(),
            None,
            BlockTag::Latest,
            None,
        )
        .await
        .expect("the manager should answer factory()");
    let factory = Address::from_slice(&factory_return[12..32]);

    let mut get_pool = match pool_key {
        PoolKey::Fee(_) => selector("getPool(address,address,uint24)"),
        PoolKey::TickSpacing(_) => selector("getPool(address,address,int24)"),
    };
    get_pool.extend_from_slice(&crate::evm::rpc::pad_address(token0));
    get_pool.extend_from_slice(&crate::evm::rpc::pad_address(token1));
    let key_word = match pool_key {
        PoolKey::Fee(fee) => word(U256::from(fee)),
        PoolKey::TickSpacing(spacing) => {
            let mut w = [if spacing < 0 { 0xff } else { 0 }; 32];
            w[28..].copy_from_slice(&spacing.to_be_bytes());
            w
        }
    };
    get_pool.extend_from_slice(&key_word);
    let pool_return = rpc
        .eth_call(factory, &get_pool, None, BlockTag::Latest, None)
        .await
        .expect("the factory should answer getPool");
    let pool = Address::from_slice(&pool_return[12..32]);
    assert!(!pool.is_zero(), "the factory has no pool for this key");

    let chain_id = rpc.chain_id().await.unwrap();
    Some((
        Fork {
            rpc,
            chain_id,
            manager,
            token0,
            token1,
            pool_key,
            spacing,
            pool,
            amounts: (
                amount("LIQUIDITY_FORK_AMOUNT0"),
                amount("LIQUIDITY_FORK_AMOUNT1"),
            ),
            swap_router: env_address("LIQUIDITY_FORK_SWAP_ROUTER"),
        },
        guard,
    ))
}

impl Fork {
    async fn sender(&self, owner: Address) -> Arc<EvmSender> {
        EvmSender::fork(self.rpc.clone(), owner, self.chain_id)
            .await
            .unwrap()
    }

    fn request(&self, owner: Address) -> LiquidityRequest {
        LiquidityRequest {
            owner: owner.as_slice().to_vec(),
            deadline_unix_secs: unix_now() + 3_600,
        }
    }

    fn liquidity(&self, sender: Arc<EvmSender>) -> EvmLiquidity {
        let abi = match self.pool_key {
            PoolKey::Fee(_) => ManagerAbi::UniswapV3,
            PoolKey::TickSpacing(_) => ManagerAbi::Slipstream,
        };
        EvmLiquidity::new(sender, self.manager, abi)
    }

    fn range(&self, tick_lower: i32, tick_upper: i32) -> Range {
        Range {
            network: Network::evm(self.chain_id),
            pool: self.pool.as_slice().to_vec(),
            tick_lower,
            tick_upper,
        }
    }

    fn open(&self, tick_lower: i32, tick_upper: i32, max: (u128, u128)) -> LiquidityCommand {
        LiquidityCommand::Open {
            range: self.range(tick_lower, tick_upper),
            deposit: deposit(max, (0, 0)),
        }
    }

    /// The pool's current tick, rounded down to its spacing.
    async fn aligned_tick(&self) -> i32 {
        let slot0 = self
            .rpc
            .eth_call(
                self.pool,
                &selector("slot0()"),
                None,
                BlockTag::Latest,
                None,
            )
            .await
            .expect("the pool should answer slot0()");
        let tick = i32::from_be_bytes(slot0[60..64].try_into().unwrap());
        tick.div_euclid(self.spacing) * self.spacing
    }

    async fn balance(&self, token: Address, owner: Address) -> U256 {
        erc20::balance_of(&self.rpc, token, owner, BlockTag::Latest)
            .await
            .unwrap()
    }

    /// `positions(id)`'s liquidity (word 7) and tokens owed (words 10, 11).
    async fn position_state(&self, position: &PositionId) -> (u128, u128, u128) {
        let id = U256::from_be_slice(&position.bytes);
        let data = self
            .rpc
            .eth_call(
                self.manager,
                &manager::positionsCall { tokenId: id }.abi_encode(),
                None,
                BlockTag::Latest,
                None,
            )
            .await
            .unwrap();
        let word = |i: usize| -> u128 {
            U256::from_be_slice(&data[i * 32..(i + 1) * 32])
                .try_into()
                .unwrap()
        };
        (word(7), word(10), word(11))
    }

    /// A swap through `SwapRouter`, run by `EvmLive` over the same fork
    /// sender the liquidity adapter uses.
    async fn swap(&self, live: &EvmLive, owner: Address, zero_for_one: bool, amount_in: u128) {
        let (Some(router), PoolKey::Fee(fee)) = (self.swap_router, self.pool_key) else {
            return;
        };
        let (token_in, token_out) = if zero_for_one {
            (self.token0, self.token1)
        } else {
            (self.token1, self.token0)
        };
        live.sender()
            .ensure_balance(token_in, live.address(), U256::from(amount_in))
            .await
            .unwrap();
        let deadline = unix_now() + 3_600;
        let calldata = exactInputSingleCall {
            params: ExactInputSingleParams {
                tokenIn: token_in,
                tokenOut: token_out,
                fee: U24::from(fee),
                recipient: owner,
                deadline: U256::from(deadline),
                amountIn: U256::from(amount_in),
                amountOutMinimum: U256::ZERO,
                sqrtPriceLimitX96: U160::ZERO,
            },
        }
        .abi_encode();
        let mut payload = router.as_slice().to_vec();
        payload.extend_from_slice(&calldata);
        let route = RouteQuote {
            network: Network::evm(self.chain_id),
            token_in: token_in.as_slice().to_vec(),
            token_out: token_out.as_slice().to_vec(),
            amount_in,
            expected_amount_out: 0,
            payload,
        };
        let request = SwapRequest {
            sender: owner.as_slice().to_vec(),
            recipient: owner.as_slice().to_vec(),
            payer: Payer::Sender,
            min_amount_out: 0,
            deadline_unix_secs: deadline,
        };
        let prepared = live.prepare(&route, &request).await.unwrap();
        let realised = live.execute(&prepared, None).await.unwrap();
        assert!(matches!(realised.outcome, Outcome::Success), "{realised:?}");
        assert!(realised.amount_out.unwrap() > 0);
        assert_eq!(realised.provenance, Provenance::Simulated);
        assert_eq!(realised.tx_ref, None);
    }
}

fn deposit(max: (u128, u128), min: (u128, u128)) -> Deposit {
    Deposit {
        max: TokenPair::new(max.0, max.1),
        guard: DepositGuard::MinAmounts(TokenPair::new(min.0, min.1)),
    }
}

async fn run(
    liquidity: &EvmLiquidity,
    cmd: LiquidityCommand,
    request: &LiquidityRequest,
) -> LiquidityReport {
    let prepared = liquidity.prepare(&cmd, request).await.unwrap();
    let report = liquidity.execute(&prepared).await.unwrap();
    assert_liquidity_shape(&report);
    assert_eq!(report.provenance, Provenance::Simulated);
    report
}

async fn succeed(
    liquidity: &EvmLiquidity,
    cmd: LiquidityCommand,
    request: &LiquidityRequest,
) -> LiquidityEvent {
    let report = run(liquidity, cmd, request).await;
    assert!(matches!(report.outcome, Outcome::Success), "{report:?}");
    report.event.unwrap()
}

#[tokio::test]
async fn against_anvil_evm_liquidity_satisfies_the_contract() {
    let Some((fork, _anvil)) = fork().await else {
        return;
    };
    let owner = Address::repeat_byte(0x51);
    let liquidity = fork.liquidity(fork.sender(owner).await);
    assert_eq!(liquidity.label(), "evm-liquidity-fork");
    let tick = fork.aligned_tick().await;

    liquidity_executor_contract(
        &liquidity,
        LiquidityContractFixture {
            range: fork.range(tick - 10 * fork.spacing, tick + 10 * fork.spacing),
            request: fork.request(owner),
            open: TokenPair::new(fork.amounts.0, fork.amounts.1),
            add: TokenPair::new(fork.amounts.0 / 2, fork.amounts.1 / 2),
            sqrt_price_band_x64: (0, u128::MAX),
        },
    )
    .await;
}

/// `SPEC.md` §9.2 on a fork: lives with distinct ranges and amounts, every
/// step reconciled to the wei against the chain's own state — the owner's
/// `balanceOf` before and after, and `positions(id)` — rather than against
/// the events the adapter decoded. Every fifth range lies wholly above the
/// price, so one token's amounts are zero; every third life swaps through
/// `EvmLive` on the same sender between increase and decrease, so the
/// position earns fees and collect returns more than decrease credited.
#[tokio::test]
async fn against_anvil_hundred_lives_reconcile_to_the_wei() {
    let Some((fork, _anvil)) = fork().await else {
        return;
    };
    let lives: u32 = std::env::var("LIQUIDITY_FORK_LIVES")
        .map(|v| v.parse().expect("LIQUIDITY_FORK_LIVES"))
        .unwrap_or(100);
    let owner = Address::repeat_byte(0x52);
    let sender = fork.sender(owner).await;
    let liquidity = fork.liquidity(sender.clone());
    let live = EvmLive::new(sender);
    let request = fork.request(owner);
    let (base0, base1) = fork.amounts;
    let mut swapped_lives = 0;

    for i in 0..lives {
        let k = i as i32;
        let s = fork.spacing;
        let tick = fork.aligned_tick().await;
        // `tick` is the price's tick rounded down to the spacing, so the
        // price lies in [tick, tick + s): a range from below `tick` to at
        // least `tick + 2s` holds it with room on both sides, and one from
        // `tick + s` up lies wholly above it.
        let (lower, upper) = if k % 5 == 4 {
            // Wholly above the price: only token0 goes in.
            (tick + s * (1 + k % 7), tick + s * (8 + k % 11))
        } else {
            (
                tick - s * (1 + k % 10 + 10 * (k / 100)),
                tick + s * (2 + (k / 10) % 10),
            )
        };
        let spread = |base: u128, n: u128| base / 7 * (1 + (i as u128 * n) % 17) + i as u128;
        let mint_amounts = (spread(base0, 3), spread(base1, 5));
        let increase_amounts = (mint_amounts.0 / 3 + 1, mint_amounts.1 / 3 + 1);
        let before = |token| fork.balance(token, owner);

        // Open: the owner pays exactly what the manager reports.
        let (b0, b1) = (before(fork.token0).await, before(fork.token1).await);
        let LiquidityEvent::Opened {
            position,
            liquidity: minted,
            paid: mint,
        } = succeed(&liquidity, fork.open(lower, upper, mint_amounts), &request).await
        else {
            panic!("life {i}: Open did not answer Opened");
        };
        // The adapter topped the balance up before paying, so reconcile
        // against the balance it was given: pre-mint balance plus any
        // top-up is at least the desired amount, and what left is exact.
        let paid0 = b0.max(U256::from(mint_amounts.0)) - fork.balance(fork.token0, owner).await;
        let paid1 = b1.max(U256::from(mint_amounts.1)) - fork.balance(fork.token1, owner).await;
        assert_eq!(paid0, U256::from(mint.token0), "life {i}: mint token0");
        assert_eq!(paid1, U256::from(mint.token1), "life {i}: mint token1");
        assert!(minted > 0, "life {i}: mint added no liquidity");
        assert_eq!(fork.position_state(&position).await.0, minted, "life {i}");
        if k % 5 == 4 {
            assert_eq!(
                mint.token1, 0,
                "life {i}: a range above the price takes no token1"
            );
        }

        // Add.
        let (b0, b1) = (before(fork.token0).await, before(fork.token1).await);
        let LiquidityEvent::Added {
            liquidity: added,
            paid: increase,
        } = succeed(
            &liquidity,
            LiquidityCommand::Add {
                position: position.clone(),
                deposit: deposit(increase_amounts, (0, 0)),
            },
            &request,
        )
        .await
        else {
            panic!("life {i}: Add did not answer Added");
        };
        let paid0 = b0.max(U256::from(increase_amounts.0)) - fork.balance(fork.token0, owner).await;
        let paid1 = b1.max(U256::from(increase_amounts.1)) - fork.balance(fork.token1, owner).await;
        assert_eq!(paid0, U256::from(increase.token0), "life {i}: add token0");
        assert_eq!(paid1, U256::from(increase.token1), "life {i}: add token1");
        let total = minted + added;
        assert_eq!(fork.position_state(&position).await.0, total, "life {i}");

        // A swap across the position, so it earns fees.
        let swapped = k % 3 == 0 && fork.swap_router.is_some() && k % 5 != 4;
        if swapped {
            swapped_lives += 1;
            let zero_for_one = (k / 3) % 2 == 0;
            // Small against the position, so the price stays inside its
            // range and every swapped token pays the position a fee.
            let amount_in = if zero_for_one {
                mint.token0 / 50 + 1
            } else {
                mint.token1 / 50 + 1
            };
            fork.swap(&live, owner, zero_for_one, amount_in).await;
        }

        // Remove: nothing moves; the position owes exactly what it
        // released, plus any fees it earned.
        let (b0, b1) = (before(fork.token0).await, before(fork.token1).await);
        let LiquidityEvent::Removed {
            liquidity: removed,
            released,
            transferred,
        } = succeed(
            &liquidity,
            LiquidityCommand::Remove {
                position: position.clone(),
                liquidity: total,
                min_out: TokenPair::default(),
            },
            &request,
        )
        .await
        else {
            panic!("life {i}: Remove did not answer Removed");
        };
        assert_eq!(removed, total, "life {i}");
        assert_eq!(transferred, TokenPair::default(), "life {i}");
        assert_eq!(fork.balance(fork.token0, owner).await, b0, "life {i}");
        assert_eq!(fork.balance(fork.token1, owner).await, b1, "life {i}");
        let (left, owed0, owed1) = fork.position_state(&position).await;
        assert_eq!(left, 0, "life {i}");
        assert!(owed0 >= released.token0, "life {i}");
        assert!(owed1 >= released.token1, "life {i}");

        // Collect: the owner receives exactly what is reported, which is
        // exactly what was owed.
        let LiquidityEvent::Collected {
            transferred: collect,
        } = succeed(
            &liquidity,
            LiquidityCommand::Collect {
                position: position.clone(),
            },
            &request,
        )
        .await
        else {
            panic!("life {i}: Collect did not answer Collected");
        };
        assert_eq!(
            fork.balance(fork.token0, owner).await - b0,
            U256::from(collect.token0),
            "life {i}: collect token0"
        );
        assert_eq!(
            fork.balance(fork.token1, owner).await - b1,
            U256::from(collect.token1),
            "life {i}: collect token1"
        );
        assert_eq!((collect.token0, collect.token1), (owed0, owed1));
        assert_eq!(fork.position_state(&position).await, (0, 0, 0), "life {i}");
        if swapped {
            assert!(
                collect.token0 > released.token0 || collect.token1 > released.token1,
                "life {i}: the swap earned the position no fees"
            );
        }

        // Close: nothing moves, and the token is gone.
        let (b0, b1) = (before(fork.token0).await, before(fork.token1).await);
        let closed = succeed(
            &liquidity,
            LiquidityCommand::Close {
                position: position.clone(),
            },
            &request,
        )
        .await;
        assert_eq!(closed, LiquidityEvent::Closed, "life {i}");
        assert_eq!(fork.balance(fork.token0, owner).await, b0, "life {i}");
        assert_eq!(fork.balance(fork.token1, owner).await, b1, "life {i}");
        let owner_of = fork
            .rpc
            .eth_call(
                fork.manager,
                &manager::ownerOfCall {
                    tokenId: U256::from_be_slice(&position.bytes),
                }
                .abi_encode(),
                None,
                BlockTag::Latest,
                None,
            )
            .await;
        assert!(
            owner_of.is_err(),
            "life {i}: the burnt position still has an owner"
        );
    }
    eprintln!("{lives} lives reconciled to the wei ({swapped_lives} with a swap mid-life)");
}

/// One injected failure per `Outcome` variant, each ending in the §5b
/// shape: an open whose minimum the range cannot meet, and a `TimedOut`
/// forced by turning automine off — which then blocks the sender until
/// `resolve` sees the open land. A close with liquidity left is refused
/// before sending, so no block is mined for it (V5: it used to revert with
/// "Not cleared").
#[tokio::test]
async fn against_anvil_injected_failures_end_in_the_right_shape() {
    let Some((fork, _anvil)) = fork().await else {
        return;
    };
    let owner = Address::repeat_byte(0x53);
    let sender = fork.sender(owner).await;
    let liquidity = fork.liquidity(sender.clone());
    let request = fork.request(owner);
    let (a0, a1) = fork.amounts;
    let tick = fork.aligned_tick().await;
    let s = fork.spacing;

    // A range wholly above the price takes no token1, so asking for at
    // least 1 of it must fail the manager's slippage check.
    let slippage = run(
        &liquidity,
        LiquidityCommand::Open {
            range: fork.range(tick + 2 * s, tick + 10 * s),
            deposit: deposit((a0, a1), (0, 1)),
        },
        &request,
    )
    .await;
    match &slippage.outcome {
        Outcome::Reverted { reason } => assert_eq!(reason, "Price slippage check"),
        other => panic!("expected a slippage revert, got {other:?}"),
    }

    // Closing a position that still holds liquidity: refused before
    // sending, so no block is mined.
    let LiquidityEvent::Opened { position, .. } = succeed(
        &liquidity,
        fork.open(tick - 5 * s, tick + 5 * s, (a0, a1)),
        &request,
    )
    .await
    else {
        panic!("Open did not answer Opened");
    };
    let block = fork.rpc.block_number().await.unwrap();
    let err = liquidity
        .prepare(
            &LiquidityCommand::Close {
                position: position.clone(),
            },
            &request,
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("holds liquidity"), "{err}");
    assert_eq!(fork.rpc.block_number().await.unwrap(), block);

    // A mint that never gets mined. Balances and approvals are put in place
    // first, so the only transaction left in flight is the mint itself.
    for (token, amount) in [(fork.token0, a0), (fork.token1, a1)] {
        sender
            .ensure_balance(token, sender.address(), U256::from(amount))
            .await
            .unwrap();
        sender
            .ensure_allowance(token, fork.manager, U256::from(amount))
            .await
            .unwrap();
    }
    let prepared = liquidity
        .prepare(&fork.open(tick - 3 * s, tick + 3 * s, (a0, a1)), &request)
        .await
        .unwrap();
    // A second, different command, to show the sender refuses it while the
    // first is unresolved.
    let next = liquidity
        .prepare(&fork.open(tick - 4 * s, tick + 4 * s, (a0, a1)), &request)
        .await
        .unwrap();
    sender.set_poll_settings(PollSettings {
        interval: Duration::from_millis(50),
        timeout: Duration::from_millis(400),
    });
    fork.rpc
        .call("evm_setAutomine", json!([false]))
        .await
        .unwrap();
    let timed_out = liquidity.execute(&prepared).await;
    let blocked = liquidity.execute(&next).await;
    let still_pending = sender.resolve().await;
    fork.rpc.call("evm_mine", json!([])).await.unwrap();
    fork.rpc
        .call("evm_setAutomine", json!([true]))
        .await
        .unwrap();

    let timed_out = timed_out.unwrap();
    assert_liquidity_shape(&timed_out);
    assert!(matches!(timed_out.outcome, Outcome::TimedOut));
    assert!(blocked.unwrap_err().to_string().contains("unresolved"));
    assert!(still_pending.unwrap().is_none());
    match sender.resolve().await.unwrap() {
        Some(crate::evm::TxOutcome::Success { .. }) => {}
        other => panic!("the mint should have landed once mined, got {other:?}"),
    }
    assert_eq!(sender.unresolved(), None);

    // Resolved, the sender sends again: a Collect on a position that owes
    // nothing is a success that transfers zero.
    let again = succeed(&liquidity, LiquidityCommand::Collect { position }, &request).await;
    assert_eq!(
        again,
        LiquidityEvent::Collected {
            transferred: TokenPair::default()
        }
    );
}
