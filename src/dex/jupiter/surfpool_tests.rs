//! Jupiter's adapters on a Surfpool fork (`IMPLEMENTATION_PLAN.md` Phase 15,
//! step 5): `JupiterSimulated` from an account the fork funded, through
//! `dex_executor_contract`, and `JupiterLive` over a fork sender. Gated on
//! `SURFPOOL_RPC_URL`, a no-op when it is unset; `JUP_API_KEY` is used when
//! set. The test fetches Jupiter's quote itself (the crate never does), as a
//! direct route through Orca's Whirlpool only (`dexes=Whirlpool`): one venue,
//! the same each run and the program phase 15's liquidity venue runs on, and
//! few accounts for the fork to read from its upstream. Unpinned, Jupiter
//! chose a proprietary AMM (Kipseli) on 1 October.
//!
//! A free public node behind the fork refuses the burst of account reads a
//! swap needs; `scripts/rpc-pacer.py` paces them (its docs say how).

use crate::dex::jupiter::{slippage_bps_for, JupiterConfig, JupiterLive, JupiterSimulated};
use crate::dex::{DexExecutor, Outcome, Payer, RouteQuote, SwapRequest, TxCost};
use crate::solana::token::{associated_token_account, token_account_amount, TOKEN_PROGRAM};
use crate::solana::{SolanaRpc, SolanaSender};
use crate::testkit::contract::{dex_executor_contract, DexContractFixture, Sends};
use crate::{Network, Provenance};
use serde_json::Value;
use solana_address::Address;

const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const WSOL: &str = "So11111111111111111111111111111111111111112";
const JUPITER_PROGRAM: &str = "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4";
/// 5 USDC in.
const AMOUNT_IN: u64 = 5_000_000;

fn surfpool() -> Option<SolanaRpc> {
    let Ok(url) = std::env::var("SURFPOOL_RPC_URL") else {
        eprintln!("skipping: SURFPOOL_RPC_URL is not set");
        return None;
    };
    Some(SolanaRpc::new(url))
}

/// One Jupiter quote, USDC to wSOL, one hop through a Whirlpool; paced to
/// the keyed rate of one call a second.
async fn quote(config: &JupiterConfig) -> Value {
    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
    let url = format!(
        "{}/quote?inputMint={USDC}&outputMint={WSOL}&amount={AMOUNT_IN}&slippageBps=50&onlyDirectRoutes=true&dexes=Whirlpool",
        config.base_url
    );
    let mut request = reqwest::Client::new().get(url);
    if let Some(key) = &config.api_key {
        request = request.header("x-api-key", key);
    }
    let quote: Value = request.send().await.unwrap().json().await.unwrap();
    assert!(
        quote.get("outAmount").is_some(),
        "Jupiter quoted nothing: {quote}"
    );
    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
    quote
}

fn route(network: Network, quote: &Value) -> RouteQuote {
    let amount = |name: &str| quote[name].as_str().unwrap().parse::<u128>().unwrap();
    RouteQuote {
        network,
        token_in: USDC.parse::<Address>().unwrap().to_bytes().to_vec(),
        token_out: WSOL.parse::<Address>().unwrap().to_bytes().to_vec(),
        amount_in: amount("inAmount"),
        expected_amount_out: amount("outAmount"),
        payload: serde_json::to_vec(quote).unwrap(),
    }
}

fn request(owner: &Address, min_amount_out: u128) -> SwapRequest {
    SwapRequest {
        sender: owner.to_bytes().to_vec(),
        recipient: owner.to_bytes().to_vec(),
        payer: Payer::Sender,
        min_amount_out,
        deadline_unix_secs: crate::liquidity::unix_now() + 300,
    }
}

/// The slippage in bps Jupiter's route instruction carries: its data ends
/// with `quoted_out_amount: u64, slippage_bps: u16, platform_fee_bps: u8`.
fn instruction_bps(prepared: &crate::dex::Prepared) -> (u64, u16) {
    let tx = &prepared.solana_transaction().unwrap().transaction;
    let jupiter: Address = JUPITER_PROGRAM.parse().unwrap();
    let keys = tx.message.static_account_keys();
    let ix = tx
        .message
        .instructions()
        .iter()
        .find(|ix| keys.get(usize::from(ix.program_id_index)) == Some(&jupiter))
        .expect("a Jupiter instruction");
    let n = ix.data.len();
    let quoted = u64::from_le_bytes(ix.data[n - 11..n - 3].try_into().unwrap());
    let bps = u16::from_le_bytes(ix.data[n - 3..n - 1].try_into().unwrap());
    (quoted, bps)
}

#[tokio::test]
async fn against_surfpool_jupiter_simulated_satisfies_the_dex_contract() {
    let Some(rpc) = surfpool() else {
        return;
    };
    let config = JupiterConfig::from_env();
    // An account the fork holds USDC in: Solana has no state override.
    let funded = SolanaSender::fork(rpc.clone()).await.unwrap();
    let usdc: Address = USDC.parse().unwrap();
    funded
        .ensure_balance(usdc.to_bytes(), TOKEN_PROGRAM.to_bytes(), AMOUNT_IN)
        .await
        .unwrap();
    let adapter = JupiterSimulated::connect(rpc, config.clone())
        .await
        .unwrap();
    assert_eq!(adapter.label(), "jupiter-simulated");

    let q = quote(&config).await;
    let r = route(funded.network(), &q);
    let min = r.expected_amount_out * 99 / 100;
    dex_executor_contract(
        &adapter,
        Sends::Nothing,
        DexContractFixture {
            route: r.clone(),
            request: request(&funded.pubkey(), min),
        },
    )
    .await;

    // Again, looking at the numbers.
    let q = quote(&config).await;
    let r = route(funded.network(), &q);
    let min = r.expected_amount_out * 99 / 100;
    let prepared = adapter
        .prepare(&r, &request(&funded.pubkey(), min))
        .await
        .unwrap();
    let (quoted, bps) = instruction_bps(&prepared);
    assert_eq!(u128::from(quoted), r.expected_amount_out);
    assert_eq!(bps, slippage_bps_for(quoted, min as u64).unwrap());
    let realised = adapter.execute(&prepared, None).await.unwrap();
    assert_eq!(realised.outcome, Outcome::Success, "{realised:?}");
    let out = realised.amount_out.unwrap();
    assert!(out >= min, "{out} below the minimum {min}");
    let TxCost::Solana(cost) = &realised.cost else {
        panic!("{:?}", realised.cost)
    };
    assert!(
        cost.units_consumed > 0 && cost.fee_lamports >= 5_000,
        "{cost:?}"
    );
    assert_eq!(realised.provenance, Provenance::Simulated);
    eprintln!(
        "jupiter-simulated: {AMOUNT_IN} USDC units quoted {} wSOL units, simulated {out}, \
         minimum {min} at {bps} bps, {} CU",
        r.expected_amount_out, cost.units_consumed
    );
}

#[tokio::test]
async fn against_surfpool_jupiter_live_swaps_on_the_fork() {
    let Some(rpc) = surfpool() else {
        return;
    };
    let config = JupiterConfig::from_env();
    let sender = SolanaSender::fork(rpc).await.unwrap();
    let usdc: Address = USDC.parse().unwrap();
    sender
        .ensure_balance(usdc.to_bytes(), TOKEN_PROGRAM.to_bytes(), AMOUNT_IN)
        .await
        .unwrap();
    let adapter = JupiterLive::new(sender.clone(), config.clone());
    assert_eq!(adapter.label(), "jupiter-live-fork");

    let q = quote(&config).await;
    let r = route(sender.network(), &q);
    let min = r.expected_amount_out * 99 / 100;
    let req = request(&sender.pubkey(), min);
    let prepared = adapter.prepare(&r, &req).await.unwrap();
    let realised = adapter.execute(&prepared, None).await.unwrap();
    assert_eq!(realised.outcome, Outcome::Success, "{realised:?}");
    assert_eq!(realised.provenance, Provenance::Simulated);
    assert_eq!(
        realised.tx_ref.as_ref().map(Vec::len),
        Some(64),
        "a fork send carries its signature"
    );
    let out = realised.amount_out.unwrap();
    assert!(out >= min, "{out} below the minimum {min}");
    // The fork's own state agrees with what the adapter read from the meta.
    let wsol: Address = WSOL.parse().unwrap();
    let account = associated_token_account(&sender.pubkey(), &wsol, &TOKEN_PROGRAM);
    let held = sender.rpc().multiple_accounts(&[account]).await.unwrap();
    assert_eq!(
        u128::from(token_account_amount(held[0].as_ref().unwrap()).unwrap()),
        out
    );

    // The contract suite too, from the same sender (the first swap left it
    // the USDC to spend again only if it is funded again).
    sender
        .ensure_balance(usdc.to_bytes(), TOKEN_PROGRAM.to_bytes(), AMOUNT_IN)
        .await
        .unwrap();
    let q = quote(&config).await;
    let r = route(sender.network(), &q);
    let min = r.expected_amount_out * 99 / 100;
    dex_executor_contract(
        &adapter,
        Sends::Transactions,
        DexContractFixture {
            route: r,
            request: request(&sender.pubkey(), min),
        },
    )
    .await;
    eprintln!("jupiter-live on the fork: {AMOUNT_IN} USDC units swapped for {out} wSOL units");
}
