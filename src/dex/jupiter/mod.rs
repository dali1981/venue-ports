//! Jupiter as a venue behind the DEX port (`SPEC.md` §5, Jupiter): its rules
//! live here, and none of its types cross the port.
//!
//! - **The payload** is Jupiter's quote response, verbatim: it is what
//!   `/swap` takes back, and a subset reassembled here would be a different
//!   route from the one that was priced.
//! - **The minimum.** Jupiter's program takes a slippage in bps against the
//!   quoted amount, and computes its minimum as `outAmount × (10 000 − bps)
//!   / 10 000`, rounded down. The adapter writes into the quote the largest
//!   bps whose minimum is still at or above `SwapRequest.min_amount_out`, so
//!   the on-chain minimum is the smallest one that keeps the caller's
//!   number: the literal number stays the contract, and the bps are the
//!   venue's encoding of it. A minimum above the quoted amount is refused.
//! - **The amount out** is the destination token account's balance after,
//!   less before: the recipient's associated token account for the output
//!   mint, under the mint's own token program.
//! - **The amount in** is the quote's `inAmount`, which is `route.amount_in`:
//!   an `ExactIn` route (the only mode accepted) spends all of it or fails, and
//!   slippage is a minimum on the output alone, so a swap that succeeded took
//!   the whole offer. A swap that did not succeed took none.
//!
//! [`JupiterSimulated`] runs the built transaction as a dry run;
//! [`JupiterLive`] signs and sends it through a `SolanaSender` (Simulated on
//! a Surfpool fork; Live is not built until the owner asks).

mod live;
#[cfg(test)]
mod mock_tests;
mod simulated;
#[cfg(test)]
mod surfpool_tests;

pub use live::JupiterLive;
pub use simulated::JupiterSimulated;

use crate::dex::{Payer, RouteQuote, SwapRequest};
use crate::solana::rpc::{address_from_slice, SolanaRpc};
use crate::solana::token::{associated_token_account, token_account_amount, token_program_of};
use crate::Network;
use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use serde_json::{json, Value};
use solana_address::Address;
use solana_transaction::versioned::VersionedTransaction;
use std::time::Duration;

/// Jupiter's swap API: `api.jup.ag/swap/v1`, keyed when a key is given.
#[derive(Debug, Clone)]
pub struct JupiterConfig {
    pub base_url: String,
    /// Sent as `x-api-key`. Keyless calls are slower to be answered.
    pub api_key: Option<String>,
    /// The priority fee `/swap` puts in the transaction, in lamports; `None`
    /// lets Jupiter choose (`auto`).
    pub prioritization_fee_lamports: Option<u64>,
}

impl Default for JupiterConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.jup.ag/swap/v1".to_string(),
            api_key: None,
            prioritization_fee_lamports: None,
        }
    }
}

impl JupiterConfig {
    /// The default endpoint, keyed with `JUP_API_KEY` when it is set.
    pub fn from_env() -> Self {
        Self {
            api_key: std::env::var("JUP_API_KEY").ok().filter(|k| !k.is_empty()),
            ..Self::default()
        }
    }
}

/// The largest slippage in bps whose on-chain minimum, `quoted × (10 000 −
/// bps) / 10 000` rounded down, is still at or above `min_out`. An error
/// when `min_out` is above `quoted`.
pub fn slippage_bps_for(quoted: u64, min_out: u64) -> Result<u16> {
    if min_out > quoted {
        bail!("the minimum out ({min_out}) is above Jupiter's quoted amount ({quoted})");
    }
    let minimum = |bps: u64| (u128::from(quoted) * u128::from(10_000 - bps) / 10_000) as u64;
    // The minimum falls as bps rise: search for the last bps that keeps it.
    let (mut keeps, mut breaks) = (0u64, 10_001u64);
    while breaks - keeps > 1 {
        let mid = (keeps + breaks) / 2;
        if minimum(mid) >= min_out {
            keeps = mid;
        } else {
            breaks = mid;
        }
    }
    Ok(keeps as u16)
}

/// What a quote says of itself, checked against the route that carries it.
struct Quote {
    body: Value,
    output_mint: Address,
    out_amount: u64,
}

fn read_quote(route: &RouteQuote) -> Result<Quote> {
    let mut body: Value = serde_json::from_slice(&route.payload)
        .context("the route's payload is not Jupiter's quote")?;
    let text = |name: &str| -> Result<String> {
        body.get(name)
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| anyhow!("Jupiter's quote has no {name}"))
    };
    let number = |name: &str| -> Result<u64> {
        text(name)?
            .parse()
            .with_context(|| format!("Jupiter's quote's {name}"))
    };
    let input_mint = crate::solana::rpc::parse_address(&text("inputMint")?)?;
    let output_mint = crate::solana::rpc::parse_address(&text("outputMint")?)?;
    if input_mint.as_ref() != route.token_in.as_slice()
        || output_mint.as_ref() != route.token_out.as_slice()
    {
        bail!("the route's tokens are not its quote's ({input_mint} to {output_mint})");
    }
    let (in_amount, out_amount) = (number("inAmount")?, number("outAmount")?);
    if u128::from(in_amount) != route.amount_in
        || u128::from(out_amount) != route.expected_amount_out
    {
        bail!(
            "the route's amounts ({} in, {} out) are not its quote's ({in_amount}, {out_amount})",
            route.amount_in,
            route.expected_amount_out
        );
    }
    if text("swapMode")? != "ExactIn" {
        bail!("only an ExactIn quote has a literal minimum out");
    }
    // Written back below with the minimum's bps.
    body["slippageBps"] = json!(0);
    Ok(Quote {
        body,
        output_mint,
        out_amount,
    })
}

/// Where the proceeds land, and what it holds before the swap.
#[derive(Debug, Clone)]
struct Destination {
    account: Address,
    before: u64,
}

/// The pieces both adapters share: a checked request, the transaction
/// `/swap` built for it, and the account its proceeds land in.
struct Built {
    transaction: VersionedTransaction,
    last_valid_block_height: u64,
    prioritization_fee_lamports: u64,
    destination: Destination,
}

fn check_request(
    network: Network,
    route: &RouteQuote,
    req: &SwapRequest,
) -> Result<(Address, Address)> {
    if route.network != network {
        bail!(
            "the route is for network {}, but this Jupiter adapter's node is on {network}",
            route.network
        );
    }
    match req.payer {
        Payer::Sender => {}
        Payer::CalledContract => bail!(
            "a Jupiter swap is paid by the account that signs it: SwapRequest.payer must be \
             Payer::Sender, not Payer::CalledContract"
        ),
    }
    req.priority.policy_only("a Jupiter adapter")?;
    let sender = address_from_slice(&req.sender).context("SwapRequest.sender")?;
    let recipient = address_from_slice(&req.recipient).context("SwapRequest.recipient")?;
    if req.deadline_unix_secs == 0 {
        bail!("SwapRequest.deadline_unix_secs must be set");
    }
    if crate::liquidity::unix_now() >= req.deadline_unix_secs {
        bail!(
            "SwapRequest.deadline_unix_secs ({}) has already passed",
            req.deadline_unix_secs
        );
    }
    Ok((sender, recipient))
}

/// Posts the quote back to `/swap` with the minimum's bps, for `sender`,
/// landing in `recipient`'s token account.
async fn build(
    config: &JupiterConfig,
    http: &reqwest::Client,
    rpc: &SolanaRpc,
    route: &RouteQuote,
    req: &SwapRequest,
    sender: Address,
    recipient: Address,
) -> Result<Built> {
    let mut quote = read_quote(route)?;
    let min_out = u64::try_from(req.min_amount_out)
        .map_err(|_| anyhow!("a Solana amount is a u64; {} is not", req.min_amount_out))?;
    let bps = slippage_bps_for(quote.out_amount, min_out)?;
    quote.body["slippageBps"] = json!(bps);
    quote.body["otherAmountThreshold"] = json!((u128::from(quote.out_amount)
        * u128::from(10_000 - u64::from(bps))
        / 10_000)
        .to_string());

    let program = token_program_of(rpc, &quote.output_mint).await?;
    let account = associated_token_account(&recipient, &quote.output_mint, &program);
    let before = match rpc.multiple_accounts(&[account]).await?.pop().flatten() {
        Some(data) => token_account_amount(&data)?,
        None => 0,
    };

    let mut body = json!({
        "quoteResponse": quote.body,
        "userPublicKey": sender.to_string(),
        "wrapAndUnwrapSol": false,
        "dynamicComputeUnitLimit": true,
    });
    if recipient != sender {
        body["destinationTokenAccount"] = json!(account.to_string());
    }
    if let Some(fee) = config.prioritization_fee_lamports {
        body["prioritizationFeeLamports"] = json!(fee);
    }
    let mut request = http.post(format!("{}/swap", config.base_url)).json(&body);
    if let Some(key) = &config.api_key {
        request = request.header("x-api-key", key);
    }
    let answer: Value = request
        .send()
        .await
        .context("asking Jupiter's /swap")?
        .json()
        .await
        .context("reading Jupiter's /swap answer")?;
    let encoded = answer
        .get("swapTransaction")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("Jupiter's /swap built no transaction: {answer}"))?;
    let wire = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .context("Jupiter's transaction is not base64")?;
    let transaction: VersionedTransaction =
        bincode::deserialize(&wire).context("decoding Jupiter's transaction")?;
    if transaction.message.static_account_keys().first() != Some(&sender) {
        bail!("Jupiter built a transaction whose fee payer is not the sender");
    }
    Ok(Built {
        transaction,
        last_valid_block_height: answer
            .get("lastValidBlockHeight")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow!("Jupiter's /swap gave no lastValidBlockHeight"))?,
        prioritization_fee_lamports: answer
            .get("prioritizationFeeLamports")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        destination: Destination { account, before },
    })
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .expect("a reqwest client with only a timeout set always builds")
}

/// The key a built transaction is stashed under between `prepare` and
/// `execute`: its message's bytes.
fn message_key(transaction: &VersionedTransaction) -> Vec<u8> {
    transaction.message.serialize()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dex::PriorityBid;

    #[test]
    fn a_contract_payer_is_refused_by_name() {
        let network = Network::Solana {
            genesis_hash: [7; 32],
        };
        let route = RouteQuote {
            network,
            token_in: vec![1; 32],
            token_out: vec![2; 32],
            amount_in: 1_000,
            expected_amount_out: 900,
            payload: Vec::new(),
        };
        let req = SwapRequest {
            sender: vec![3; 32],
            recipient: vec![3; 32],
            payer: Payer::CalledContract,
            min_amount_out: 900,
            deadline_unix_secs: u64::MAX,
            priority: PriorityBid::Policy,
        };
        let err = check_request(network, &route, &req).unwrap_err();
        assert!(err.to_string().contains("Payer::CalledContract"), "{err}");
        let paid_by_sender = SwapRequest {
            payer: Payer::Sender,
            ..req
        };
        assert!(check_request(network, &route, &paid_by_sender).is_ok());
        // Jupiter's `/swap` puts its own priority fee in the transaction: no
        // bid above it is sent, so one is refused rather than dropped.
        let bidding = SwapRequest {
            priority: PriorityBid::AbovePolicyLamports(5_000),
            ..paid_by_sender
        };
        let err = check_request(network, &route, &bidding).unwrap_err();
        assert!(err.to_string().contains("fee policy only"), "{err}");
    }

    #[test]
    fn the_bps_keep_the_on_chain_minimum_at_or_above_the_literal_one() {
        // 1 000 000 quoted: 50 bps leaves exactly 995 000.
        assert_eq!(slippage_bps_for(1_000_000, 995_000).unwrap(), 50);
        // One unit more and 50 bps would undershoot it: 49 is the most.
        assert_eq!(slippage_bps_for(1_000_000, 995_001).unwrap(), 49);
        // The minimum equal to the quote allows nothing.
        assert_eq!(slippage_bps_for(1_000_000, 1_000_000).unwrap(), 0);
        // A minimum of zero allows everything.
        assert_eq!(slippage_bps_for(1_000_000, 0).unwrap(), 10_000);
        // Rounding toward safety on a small quote: 1 177 574 × 9 950 / 10 000
        // = 1 171 686.13 → 1 171 686, below 1 171 687; so 49 bps, not 50.
        assert_eq!(slippage_bps_for(1_177_574, 1_171_687).unwrap(), 49);
        for (quoted, min) in [
            (1_177_574u64, 1_171_687u64),
            (7, 3),
            (u64::MAX, u64::MAX / 3),
        ] {
            let bps = u64::from(slippage_bps_for(quoted, min).unwrap());
            let at = |b: u64| (u128::from(quoted) * u128::from(10_000 - b) / 10_000) as u64;
            assert!(
                at(bps) >= min && (bps == 10_000 || at(bps + 1) < min),
                "{quoted} {min} {bps}"
            );
        }
        assert!(slippage_bps_for(10, 11).is_err());
    }
}
