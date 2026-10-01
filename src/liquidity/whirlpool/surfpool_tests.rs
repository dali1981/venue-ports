//! `WhirlpoolLiquidity` on a Surfpool fork of mainnet (`IMPLEMENTATION_PLAN.md`
//! Phase 15, step 6): V5 §8's sequence through `liquidity_executor_contract`,
//! on Orca's USDC/USDT Whirlpool (two SPL tokens, so no wrapped SOL), from a
//! fork sender the fork funds. Gated on `SURFPOOL_RPC_URL`, a no-op when it
//! is unset; `scripts/rpc-pacer.py` says how to run the fork on a free node.

use crate::dex::TxCost;
use crate::liquidity::{
    Deposit, DepositGuard, LiquidityCommand, LiquidityEvent, LiquidityExecutor, LiquidityRequest,
    Range, TokenPair, WhirlpoolLiquidity, WHIRLPOOL_PROGRAM,
};
use crate::solana::{SolanaRpc, SolanaSender};
use crate::testkit::contract::{liquidity_executor_contract, LiquidityContractFixture};
use solana_address::Address;

/// Orca's USDC/USDT Whirlpool: tick spacing 1, token A USDC, token B USDT.
const POOL: &str = "4fuUiYxTQ6QCrdSq9ouBYcTM7bqSwYTSyLueGZLTy4T4";

fn surfpool() -> Option<SolanaRpc> {
    let Ok(url) = std::env::var("SURFPOOL_RPC_URL") else {
        eprintln!("skipping: SURFPOOL_RPC_URL is not set");
        return None;
    };
    Some(SolanaRpc::new(url))
}

/// The pool's current tick and sqrt price (Q64.64), read from its account.
async fn pool_now(rpc: &SolanaRpc) -> (i32, u128) {
    let pool: Address = POOL.parse().unwrap();
    let account = rpc.multiple_accounts(&[pool]).await.unwrap().pop().flatten().unwrap();
    let state = orca_whirlpools_client::Whirlpool::from_bytes(&account.data).unwrap();
    (state.tick_current_index, state.sqrt_price)
}

fn request(sender: &SolanaSender) -> LiquidityRequest {
    LiquidityRequest {
        owner: sender.address().to_vec(),
        deadline_unix_secs: crate::liquidity::unix_now() + 600,
    }
}

/// The range eight ticks either side of the pool's tick, and a band 1 %
/// either side of its price (0.5 % in sqrt price).
async fn fixture(rpc: &SolanaRpc, sender: &SolanaSender) -> LiquidityContractFixture {
    let (tick, sqrt_price) = pool_now(rpc).await;
    LiquidityContractFixture {
        range: Range {
            network: sender.network(),
            pool: POOL.parse::<Address>().unwrap().to_bytes().to_vec(),
            tick_lower: tick - 8,
            tick_upper: tick + 8,
        },
        request: request(sender),
        open: TokenPair::new(10_000_000, 10_000_000),
        add: TokenPair::new(5_000_000, 5_000_000),
        sqrt_price_band_x64: (sqrt_price / 1_000 * 995, sqrt_price / 1_000 * 1_005),
    }
}

#[tokio::test]
async fn against_surfpool_whirlpool_satisfies_the_liquidity_contract() {
    let Some(rpc) = surfpool() else {
        return;
    };
    let sender = SolanaSender::fork(rpc.clone()).await.unwrap();
    let venue = WhirlpoolLiquidity::new(sender.clone(), WHIRLPOOL_PROGRAM);
    assert_eq!(venue.label(), "whirlpool-liquidity-fork");
    let fixture = fixture(&rpc, &sender).await;
    liquidity_executor_contract(&venue, fixture).await;
}

/// One life, its costs read: `Open` deposits the position's rent, `Close`
/// returns exactly that, and every report carries its fee.
#[tokio::test]
async fn against_surfpool_whirlpool_rent_is_deposited_on_open_and_returned_on_close() {
    let Some(rpc) = surfpool() else {
        return;
    };
    let sender = SolanaSender::fork(rpc.clone()).await.unwrap();
    let venue = WhirlpoolLiquidity::new(sender.clone(), WHIRLPOOL_PROGRAM);
    let f = fixture(&rpc, &sender).await;
    let guard = DepositGuard::SqrtPriceBand {
        min_sqrt_price_x64: f.sqrt_price_band_x64.0,
        max_sqrt_price_x64: f.sqrt_price_band_x64.1,
    };
    let run = |cmd: LiquidityCommand| {
        let (venue, request) = (&venue, &f.request);
        async move {
            let prepared = venue.prepare(&cmd, request).await.unwrap();
            let report = venue.execute(&prepared).await.unwrap();
            let TxCost::Solana(cost) = report.cost else {
                panic!("{:?}", report.cost)
            };
            assert!(cost.fee_lamports >= 5_000 && cost.units_consumed > 0, "{cost:?}");
            (report.event.expect("a success"), cost)
        }
    };
    let (opened, open_cost) = run(LiquidityCommand::Open {
        range: f.range.clone(),
        deposit: Deposit { max: f.open, guard },
    })
    .await;
    let LiquidityEvent::Opened { position, liquidity, paid } = opened else {
        panic!("{opened:?}")
    };
    assert!(open_cost.rent_deposited_lamports > 0, "{open_cost:?}");
    let (removed, _) = run(LiquidityCommand::Remove {
        position: position.clone(),
        liquidity,
        min_out: TokenPair::default(),
    })
    .await;
    let (collected, _) = run(LiquidityCommand::Collect { position: position.clone() }).await;
    let (closed, close_cost) = run(LiquidityCommand::Close { position }).await;
    assert_eq!(closed, LiquidityEvent::Closed);
    assert_eq!(
        close_cost.rent_returned_lamports, open_cost.rent_deposited_lamports,
        "Close returns what Open deposited"
    );
    eprintln!(
        "whirlpool on the fork: opened L {liquidity} for {paid:?}; {removed:?}; {collected:?}; \
         open {open_cost:?}; close {close_cost:?}"
    );
}

/// A range whose tick array does not exist yet: `Open` creates it and spends
/// its rent. The dynamic array gives the two ticks' rent back into the
/// position when `Remove` releases them, and `Close` returns it with the
/// deposit, so over the life the owner is out of pocket exactly what the
/// array still holds. The range lies above the price, so only token A goes
/// in.
#[tokio::test]
async fn against_surfpool_whirlpool_open_creates_a_missing_tick_array_and_spends_its_rent() {
    let Some(rpc) = surfpool() else {
        return;
    };
    let sender = SolanaSender::fork(rpc.clone()).await.unwrap();
    let venue = WhirlpoolLiquidity::new(sender.clone(), WHIRLPOOL_PROGRAM);
    let f = fixture(&rpc, &sender).await;
    let pool: Address = POOL.parse().unwrap();
    // The first array of 88 ticks, 20 arrays up and on, that nobody created.
    let (tick, _) = pool_now(&rpc).await;
    let mut start = tick.div_euclid(88) * 88 + 20 * 88;
    loop {
        let (array, _) =
            orca_whirlpools_client::get_tick_array_address(&pool, start, Some(WHIRLPOOL_PROGRAM)).unwrap();
        if rpc.multiple_accounts(&[array]).await.unwrap()[0].is_none() {
            break;
        }
        start += 88;
    }
    let guard = DepositGuard::SqrtPriceBand {
        min_sqrt_price_x64: f.sqrt_price_band_x64.0,
        max_sqrt_price_x64: f.sqrt_price_band_x64.1,
    };
    let open = LiquidityCommand::Open {
        range: Range {
            tick_lower: start + 8,
            tick_upper: start + 16,
            ..f.range.clone()
        },
        deposit: Deposit {
            max: TokenPair::new(1_000_000, 1_000_000),
            guard,
        },
    };
    let report = venue.execute(&venue.prepare(&open, &f.request).await.unwrap()).await.unwrap();
    let Some(LiquidityEvent::Opened { position, liquidity, paid }) = report.event.clone() else {
        panic!("{report:?}")
    };
    let TxCost::Solana(open_cost) = report.cost else {
        panic!("{:?}", report.cost)
    };
    assert!(open_cost.rent_spent_lamports > 0, "the new tick array's rent: {open_cost:?}");
    assert!(paid.token0 > 0 && paid.token1 == 0, "above the price, only token A: {paid:?}");
    let mut costs = vec![open_cost];
    for cmd in [
        LiquidityCommand::Remove {
            position: position.clone(),
            liquidity,
            min_out: TokenPair::default(),
        },
        LiquidityCommand::Collect { position: position.clone() },
        LiquidityCommand::Close { position },
    ] {
        let report = venue.execute(&venue.prepare(&cmd, &f.request).await.unwrap()).await.unwrap();
        let TxCost::Solana(cost) = report.cost else {
            panic!("{:?}", report.cost)
        };
        costs.push(cost);
    }
    // Out of pocket over the life: exactly what the tick array the Open
    // created still holds (the ticks' rent came back through the position).
    let (array, _) =
        orca_whirlpools_client::get_tick_array_address(&pool, start, Some(WHIRLPOOL_PROGRAM)).unwrap();
    let held = rpc.multiple_accounts(&[array]).await.unwrap()[0].clone().unwrap().lamports;
    let net: i128 = costs
        .iter()
        .map(|c| {
            i128::from(c.rent_deposited_lamports) + i128::from(c.rent_spent_lamports)
                - i128::from(c.rent_returned_lamports)
        })
        .sum();
    eprintln!("tick array at {start} created, holding {held}: {costs:?}");
    assert_eq!(net, i128::from(held), "{costs:?}");
}
