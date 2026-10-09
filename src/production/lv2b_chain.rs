//! LV2b against Base (`specs/V7-production-validation.md`, "LV2b"): one router
//! swap at a time, the port's reading of each equal to the chain's own record.
//! One test, `production_lv2b_chain`, makes sixty swaps of about `VP_SWAP_USD`
//! through an Aerodrome-style router, alternating direction, at exponential
//! intervals (mean 90 s), at the node's suggested priority fee, through a shared
//! [`EvmSender`] and [`EvmLive`]: a swap of token in for token out, then of what it
//! delivered back, and so on.
//!
//! Before each swap the case reads the router's quote at `pending` and approves
//! exactly the amount (as `EvmLive` does); it sends the swap; and it checks what
//! `Realised` says against **an independent decoding of the raw
//! `eth_getTransactionReceipt` JSON** (`chain_checks`), to the unit:
//!
//! | # | Check |
//! |---|---|
//! | X1 | `amount_out` is the output token's `Transfer`s to the recipient |
//! | X2 | `amount_in` is the input token's `Transfer`s out of the payer, less any back |
//! | X3 | `cost` is the receipt's `gasUsed`, `effectiveGasPrice` and `l1Fee` |
//! | X4 | the signer's native balance fell by the fee between the block before and the block: the first measurement of a real L1 fee |
//! | X5 | the signer's token balances changed by the two amounts |
//! | X6 | `at` is the inclusion block, `tx_ref` the hash, the nonce advanced by one |
//!
//! What is recorded and not asserted is on each swap's line, in `references`
//! (`chain_checks::swap_references`): the quote it was sent against, what is needed
//! to run the same swap again at another block, when it left, was first seen and
//! was sealed (the receipt polled every 50 ms, the block number as often), where it
//! landed, and what it cost.
//!
//! **Stops:** the loss over `VP_SPEND_CAP_USD` (the wallet valued at the pool's own
//! price and `VP_NATIVE_USD`, through the ledger, which also refuses a swap that
//! could pass the cap); any EXACT failure; more than 10 % of the swaps reverting; a
//! landing five or more blocks after the send; a swap that timed out.
//!
//! **The signer's key** is read once from `VP_SIGNER_KEY_HEX`, removed from the
//! process environment as it is read, held only by the signer, and never logged or
//! written. No keystore reader is added.
//!
//! **The router's ABI is from memory, not from a verified contract**, because the
//! environment this was written in has no block explorer: the calldata is built in
//! one small function ([`swap_calldata`]) so that a check against the verified ABI
//! changes one place (`specs/V7-questions.md`).

use crate::balance::{EvmBalanceReader, EvmBalances};
use crate::dex::evm::EvmLive;
use crate::dex::{DexExecutor, Outcome, Payer, PriorityBid, Realised, RouteQuote, SwapRequest};
use crate::evm::rpc::{hex_data, RpcEvent, RpcTap};
use crate::evm::{BlockTag, EvmRpc, EvmSender, FeePolicy, PollSettings, RpcError, Signer};
use crate::production::chain_checks::{
    swap_references, x1_amount_out, x2_amount_in, x3_cost, x4_native_change, x5_token_changes,
    x6_landing, Quote, SwapFacts,
};
use crate::production::env::{positive_decimal, required, Env};
use crate::production::gate::now_ns;
use crate::production::guard;
use crate::production::lv1_base::{host_of, rpc_request, BASE_CHAIN_ID};
use crate::production::results::{Expected, Tally, Verdict};
use crate::production::wire::Wire;
use crate::production::{CallSpec, Grade, Run};
use crate::Network;
use alloy_primitives::{keccak256, Address, B256, U256};
use alloy_sol_types::SolCall;
use anyhow::{anyhow, bail, Context, Result};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The case id of the swaps and of the reads around them.
const CASE: &str = "LV2b";
/// How many swaps a run makes.
pub(crate) const SWAPS: usize = 60;
/// The mean of the exponential interval between swaps.
const MEAN_INTERVAL: Duration = Duration::from_secs(90);
/// The polls of the receipt and of the block number, as often.
const POLL: Duration = Duration::from_millis(50);
/// `amountOutMin` is the quote less this many basis points.
const SLIPPAGE_BPS: u64 = 30;
/// How long ahead of now a swap's deadline is.
const DEADLINE_SECS: u64 = 60;
/// A landing this many blocks after the send ends the run.
const LATE_BLOCKS: u64 = 5;
/// How long a transaction is waited for before it is a time-out.
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(120);
/// How long after the receipt the block number is waited on to reach the swap's
/// block, for its sealed time.
const SEAL_TIMEOUT: Duration = Duration::from_secs(15);

mod abi {
    // An Aerodrome (Solidly-fork) router. See the module docs on where this comes from.
    alloy_sol_types::sol! {
        struct Route {
            address from;
            address to;
            bool stable;
            address factory;
        }

        interface Router {
            function getAmountsOut(uint256 amountIn, Route[] routes) external view returns (uint256[] amounts);
            function swapExactTokensForTokens(uint256 amountIn, uint256 amountOutMin, Route[] routes, address to, uint256 deadline) external returns (uint256[] amounts);
        }

        interface Pool {
            function stable() external view returns (bool);
            function factory() external view returns (address);
        }

        interface Token {
            function decimals() external view returns (uint8);
        }
    }
}
use abi::{Pool, Route, Router, Token};

/// What the run is pointed at, from the environment.
#[derive(Debug, Clone)]
pub(crate) struct Settings {
    /// `VP_BASE_RPC_URL`. A provider's URL carries its key: it is never written.
    pub(crate) rpc_url: String,
    /// `VP_ROUTER`, `VP_POOL`: the router swapped through and the pool it routes
    /// through, which the route's `stable` flag and factory are read from.
    pub(crate) router: Address,
    pub(crate) pool: Address,
    /// `VP_TOKEN_IN` (USD-pegged: one unit of it is a dollar) and `VP_TOKEN_OUT`.
    pub(crate) token_in: Address,
    pub(crate) token_out: Address,
    /// `VP_SWAP_USD`, default 10: about what one swap of the input token is worth.
    pub(crate) swap_usd: Decimal,
    /// `VP_NATIVE_USD`: what a unit of the chain's native token is worth, to value
    /// the gas the wallet spends.
    pub(crate) native_usd: Decimal,
    /// How many swaps; sixty, unless a test asks for fewer.
    pub(crate) swaps: usize,
    /// The mean of the exponential interval between swaps.
    pub(crate) mean_interval: Duration,
    /// How often the receipt and the block number are polled.
    pub(crate) poll: Duration,
}

impl Settings {
    /// Reads the settings, and the signer's key, once: `VP_SIGNER_KEY_HEX` is
    /// taken out of the environment as it is read.
    pub(crate) fn from_env(env: &dyn Env) -> Result<(Self, Signer)> {
        let address = |var: &str| -> Result<Address> {
            let text = required(env, var)?;
            text.parse()
                .with_context(|| format!("{var} is {text:?}, which is not an address"))
        };
        let settings = Self {
            rpc_url: required(env, "VP_BASE_RPC_URL")?,
            router: address("VP_ROUTER")?,
            pool: address("VP_POOL")?,
            token_in: address("VP_TOKEN_IN")?,
            token_out: address("VP_TOKEN_OUT")?,
            swap_usd: positive_decimal(env, "VP_SWAP_USD", Some(Decimal::from(10)))?,
            native_usd: positive_decimal(env, "VP_NATIVE_USD", None)?,
            swaps: SWAPS,
            mean_interval: MEAN_INTERVAL,
            poll: POLL,
        };
        let key = env
            .take("VP_SIGNER_KEY_HEX")
            .ok_or_else(|| anyhow!("VP_SIGNER_KEY_HEX is not set and has no default"))?;
        // The error never says the key.
        let signer = Signer::from_private_key_hex(&key)
            .map_err(|_| anyhow!("VP_SIGNER_KEY_HEX is not a secp256k1 private key in hex"))?;
        Ok((settings, signer))
    }
}

// --- the calldata: the one place the router's ABI is written ---------------------

/// The route through the pool: `from` to `to`, `stable` and `factory` as the pool
/// says.
fn route(from: Address, to: Address, stable: bool, factory: Address) -> Route {
    Route {
        from,
        to,
        stable,
        factory,
    }
}

/// `getAmountsOut(amountIn, routes)`.
fn quote_calldata(amount_in: U256, routes: Vec<Route>) -> Vec<u8> {
    Router::getAmountsOutCall {
        amountIn: amount_in,
        routes,
    }
    .abi_encode()
}

/// `swapExactTokensForTokens(amountIn, amountOutMin, routes, to, deadline)`.
pub(crate) fn swap_calldata(
    amount_in: U256,
    amount_out_min: U256,
    routes: Vec<Route>,
    to: Address,
    deadline: u64,
) -> Vec<u8> {
    Router::swapExactTokensForTokensCall {
        amountIn: amount_in,
        amountOutMin: amount_out_min,
        routes,
        to,
        deadline: U256::from(deadline),
    }
    .abi_encode()
}

/// The last of the amounts a `getAmountsOut` returns: what the route delivers.
fn amount_out_of(returned: &[u8]) -> Result<U256> {
    let amounts = Router::getAmountsOutCall::abi_decode_returns(returned)
        .context("decoding getAmountsOut's reply")?;
    amounts
        .last()
        .copied()
        .ok_or_else(|| anyhow!("getAmountsOut returned no amounts"))
}

// --- pacing --------------------------------------------------------------------

/// The intervals between swaps: exponential with a mean, from a generator seeded
/// from the time and the process (the same seed gives the same intervals, which is
/// how the self-tests pin it). Not a source of money or of randomness anyone
/// depends on: it spaces swaps out.
pub(crate) struct Pacer {
    state: u64,
    mean_secs: f64,
}

impl Pacer {
    pub(crate) fn with_seed(mean: Duration, seed: u64) -> Self {
        Self {
            // xorshift cannot hold zero.
            state: seed | 1,
            mean_secs: mean.as_secs_f64(),
        }
    }

    pub(crate) fn seeded(mean: Duration) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let mut material = Vec::new();
        material.extend(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos())
                .to_be_bytes(),
        );
        material.extend(std::process::id().to_be_bytes());
        material.extend(COUNTER.fetch_add(1, Ordering::SeqCst).to_be_bytes());
        let hash = keccak256(&material);
        Self::with_seed(
            mean,
            u64::from_be_bytes(hash[..8].try_into().expect("eight bytes")),
        )
    }

    fn next_u64(&mut self) -> u64 {
        // xorshift64*.
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// The next interval: `-mean * ln(u)` for `u` uniform in (0, 1], never more than
    /// ten means.
    pub(crate) fn next(&mut self) -> Duration {
        let u = ((self.next_u64() >> 11) as f64 + 1.0) / (1u64 << 53) as f64;
        let secs = (-self.mean_secs * u.ln()).clamp(0.0, 10.0 * self.mean_secs);
        Duration::from_secs_f64(secs)
    }
}

// --- the evidence of a swap ------------------------------------------------------

/// What the chain says of a landed swap, read after it: the raw receipt, the
/// block's length, the signer's balances at the block before and at the block, and
/// its nonce.
#[derive(Debug, Clone)]
struct Evidence {
    receipt: Value,
    block_tx_count: Option<u64>,
    native: (u128, u128),
    token_from: (u128, u128),
    token_to: (u128, u128),
    nonce_after: u64,
}

/// A swap that was sent, with what was measured around it.
struct Landing {
    realised: Realised,
    tx_hash: Option<B256>,
    sent_ns: Option<u128>,
    first_seen_ns: Option<u128>,
    sealed_ns: Option<u128>,
    /// The evidence, or why it could not be read.
    evidence: std::result::Result<Evidence, String>,
}

/// The blocks the node reported while a swap was in flight, polled as often as the
/// receipt is.
struct Sampler {
    stop: Arc<AtomicBool>,
    samples: Arc<Mutex<Vec<(u128, u64)>>>,
    handle: tokio::task::JoinHandle<()>,
}

impl Sampler {
    fn start(rpc: EvmRpc, poll: Duration) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let samples = Arc::new(Mutex::new(Vec::new()));
        let (flag, kept) = (Arc::clone(&stop), Arc::clone(&samples));
        let handle = tokio::spawn(async move {
            while !flag.load(Ordering::SeqCst) {
                if let Ok(block) = rpc.block_number().await {
                    kept.lock().unwrap().push((now_ns(), block));
                }
                tokio::time::sleep(poll).await;
            }
        });
        Self {
            stop,
            samples,
            handle,
        }
    }

    async fn finish(self) -> Vec<(u128, u64)> {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.handle.await;
        let samples = self.samples.lock().unwrap().clone();
        samples
    }
}

/// What the tap on the node's client saw.
#[derive(Default)]
struct Taps {
    /// When a transaction was about to be broadcast, in order.
    broadcasts: Mutex<Vec<u128>>,
    /// The first time a receipt poll found each transaction.
    first_seen: Mutex<HashMap<B256, u128>>,
}

impl Taps {
    fn tap(self: &Arc<Self>) -> RpcTap {
        let taps = Arc::clone(self);
        RpcTap::new(move |event| match event {
            RpcEvent::Broadcast => taps.broadcasts.lock().unwrap().push(now_ns()),
            RpcEvent::Receipt { hash, found: true } => {
                taps.first_seen
                    .lock()
                    .unwrap()
                    .entry(hash)
                    .or_insert_with(now_ns);
            }
            RpcEvent::Receipt { found: false, .. } => {}
        })
    }
}

/// What the run reads once and holds.
struct Ctx {
    rpc: EvmRpc,
    sender: Arc<EvmSender>,
    live: EvmLive,
    balances: EvmBalances,
    signer: Address,
    decimals_in: u32,
    decimals_out: u32,
    stable: bool,
    factory: Address,
    taps: Arc<Taps>,
    host: String,
}

/// What the run has done so far.
struct Progress {
    attempted: usize,
    reverts: usize,
    /// The output token the wallet holds from these swaps.
    held: U256,
    /// The wallet's value in USD after the last swap.
    value: Decimal,
}

/// Whether the run goes on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    Go,
    Stop,
}

pub(crate) struct Lv2bChain<'a> {
    run: &'a Run,
    settings: Settings,
    request_timeout: Duration,
    confirm_timeout: Duration,
    /// A node on this machine is accepted (the self-tests' mocks are).
    allow_local: bool,
}

fn units(usd: Decimal, decimals: u32) -> Result<U256> {
    let scaled = usd
        .checked_mul(Decimal::from(10u64.pow(decimals.min(18))))
        .ok_or_else(|| anyhow!("{usd} USD in a token of {decimals} decimals overflows"))?;
    Ok(U256::from(scaled.trunc().to_u128().ok_or_else(|| {
        anyhow!("{usd} USD in a token of {decimals} decimals does not fit a u128")
    })?))
}

fn decimal_of(amount: U256, decimals: u32) -> Result<Decimal> {
    let units = u128::try_from(amount)
        .ok()
        .and_then(|units| i128::try_from(units).ok())
        .ok_or_else(|| anyhow!("{amount} does not fit the arithmetic that values it"))?;
    Decimal::try_from_i128_with_scale(units, decimals)
        .map_err(|err| anyhow!("{amount} at {decimals} decimals cannot be valued: {err}"))
}

impl<'a> Lv2bChain<'a> {
    pub(crate) fn new(run: &'a Run, settings: Settings) -> Self {
        run.hide_urls(std::slice::from_ref(&settings.rpc_url));
        Self {
            run,
            settings,
            request_timeout: Duration::from_secs(30),
            confirm_timeout: CONFIRM_TIMEOUT,
            allow_local: false,
        }
    }

    /// For a self-test: short waits, and a mock node on this machine is a node.
    /// Unused until the mock-chain self-tests are written (`specs/V7-handover.md` §10).
    #[allow(dead_code)]
    fn against_mocks(mut self) -> Self {
        self.request_timeout = Duration::from_secs(2);
        self.confirm_timeout = Duration::from_secs(10);
        self.allow_local = true;
        self
    }

    // --- reads, gated and recorded ---

    /// An `eth_call` of `data` to `to` at `block`, asked of the gate first.
    async fn eth_call(
        &self,
        ctx: &Ctx,
        to: Address,
        data: &[u8],
        block: BlockTag,
    ) -> Result<Vec<u8>> {
        self.run.permit_rpc("eth_call", &[("to", to.to_string())])?;
        ctx.rpc.eth_call(to, data, None, block, None).await
    }

    /// One JSON-RPC request, asked of the gate first and recorded after: the reply
    /// as `{"result": …}` or `{"error": …}`, the node's own words.
    async fn raw(&self, ctx: &Ctx, method: &str, params: Value) -> Result<Value> {
        self.run
            .permit_rpc(method, &[("params", params.to_string())])?;
        let reply = ctx.rpc.call(method, params).await;
        let body = match &reply {
            Ok(value) => Some(json!({ "result": value })),
            Err(err) => err
                .downcast_ref::<RpcError>()
                .map(|node| json!({"error": {"code": node.code, "message": node.message}})),
        };
        if let Some(body) = body {
            self.run
                .gate()
                .observed("RPC", method, 200, &body.to_string());
        }
        reply
    }

    fn routes(&self, ctx: &Ctx, from: Address, to: Address) -> Vec<Route> {
        vec![route(from, to, ctx.stable, ctx.factory)]
    }

    /// What `amount_in` of `from` would buy of `to`, from the router, at `block`.
    async fn quote_at(
        &self,
        ctx: &Ctx,
        from: Address,
        to: Address,
        amount_in: U256,
        block: BlockTag,
    ) -> Result<U256> {
        let data = quote_calldata(amount_in, self.routes(ctx, from, to));
        amount_out_of(
            &self
                .eth_call(ctx, self.settings.router, &data, block)
                .await?,
        )
    }

    // --- the run ---

    /// The swaps, in order. A node that is not a production one, or a setup that
    /// fails, ends the run before anything is sent.
    pub(crate) async fn run_all(&self, signer: Signer) -> Result<Tally> {
        let url = self.settings.rpc_url.clone();
        let host = host_of(&url);
        if !self.allow_local {
            guard::node_url(&url).map_err(|refused| anyhow!("{host}: {refused}"))?;
        }
        let Some(ctx) = self.setup(&url, &host, signer).await? else {
            return Ok(self.run.tally());
        };
        let Some(start) = self.wallet_value(&ctx, "wallet-start").await? else {
            return Ok(self.run.tally());
        };
        let mut progress = Progress {
            attempted: 0,
            reverts: 0,
            held: U256::ZERO,
            value: start,
        };
        let mut pacer = Pacer::seeded(self.settings.mean_interval);
        for swap in 1..=self.settings.swaps {
            if swap > 1 && !self.settings.mean_interval.is_zero() {
                tokio::time::sleep(pacer.next()).await;
            }
            if self.swap(&ctx, &mut progress, swap).await? == Flow::Stop {
                self.exit(start, &progress, false)?;
                return Ok(self.run.tally());
            }
        }
        self.exit(start, &progress, true)?;
        Ok(self.run.tally())
    }

    /// The node is not an anvil, the sender connects on Base, and the token and
    /// pool facts are read. `None` ends the run, with the line that says why.
    async fn setup(&self, url: &str, host: &str, signer: Signer) -> Result<Option<Ctx>> {
        let taps = Arc::new(Taps::default());
        let rpc = EvmRpc::with_request_timeout(url.to_string(), self.request_timeout)
            .with_tap(taps.tap());
        // The node is not an anvil. A node with no `web3_clientVersion` is not one.
        let guard_call = self
            .run
            .call_with(
                CallSpec::new("GUARD", Expected::answer())
                    .host(host)
                    .record_as("50-client-version")
                    .request(rpc_request("web3_clientVersion", &[])),
                || async { self.raw_with(&rpc, "web3_clientVersion", json!([])).await },
                |_| None,
                |result| match result {
                    Ok(version) => version
                        .as_str()
                        .ok_or_else(|| {
                            format!("web3_clientVersion answered {version}, not a string")
                        })
                        .and_then(|version| {
                            guard::node_version(version).map_err(|refused| refused.to_string())
                        }),
                    Err(err) => match err.downcast_ref::<RpcError>() {
                        Some(node) if node.code == Some(-32601) => Ok(()),
                        Some(node) => Err(format!(
                            "the node refused web3_clientVersion ({:?}: {}), so the run cannot say \
                             it is not an anvil",
                            node.code, node.message
                        )),
                        None => Ok(()),
                    },
                },
            )
            .await?;
        if guard_call.stops_the_run() || matches!(guard_call.verdict, Verdict::Fail) {
            return Ok(None);
        }

        // The sender connects on Base: the chain id is checked.
        let connect_params = [("chain", BASE_CHAIN_ID.to_string())];
        let signer_address = signer.address();
        let connected = self
            .run
            .call(
                self.spec(host, "51-connect", "eth_chainId", &connect_params),
                || async {
                    self.run.permit_rpc("eth_chainId", &connect_params)?;
                    EvmSender::connect(rpc.clone(), signer, BASE_CHAIN_ID, FeePolicy::default())
                        .await
                },
            )
            .await?;
        let Ok(sender) = connected.result else {
            if matches!(connected.verdict, Verdict::Skipped) {
                self.run.computed(
                    CASE,
                    Verdict::Skipped,
                    "a dry run makes no read, so no swap can be sized",
                    None,
                )?;
            }
            return Ok(None);
        };
        sender.set_poll_settings(PollSettings {
            interval: self.settings.poll,
            timeout: self.confirm_timeout,
        });
        let live = EvmLive::new(Arc::clone(&sender));
        let balances = EvmBalances::new(rpc.clone());
        let mut ctx = Ctx {
            rpc,
            sender,
            live,
            balances,
            signer: signer_address,
            decimals_in: 0,
            decimals_out: 0,
            stable: false,
            factory: Address::ZERO,
            taps,
            host: host.to_string(),
        };

        // The facts a swap is built from: the tokens' decimals, and the pool's.
        let reads = self
            .run
            .call(
                self.spec(
                    host,
                    "52-pool-and-tokens",
                    "eth_call",
                    &[("pool", self.settings.pool.to_string())],
                ),
                || async {
                    let pool = self.settings.pool;
                    let stable = Pool::stableCall::abi_decode_returns(
                        &self
                            .eth_call(
                                &ctx,
                                pool,
                                &Pool::stableCall {}.abi_encode(),
                                BlockTag::Latest,
                            )
                            .await?,
                    )
                    .context("decoding the pool's stable()")?;
                    let factory = Pool::factoryCall::abi_decode_returns(
                        &self
                            .eth_call(
                                &ctx,
                                pool,
                                &Pool::factoryCall {}.abi_encode(),
                                BlockTag::Latest,
                            )
                            .await?,
                    )
                    .context("decoding the pool's factory()")?;
                    Ok((
                        self.decimals_of(&ctx, self.settings.token_in).await?,
                        self.decimals_of(&ctx, self.settings.token_out).await?,
                        stable,
                        factory,
                    ))
                },
            )
            .await?;
        let Ok((decimals_in, decimals_out, stable, factory)) = reads.result else {
            return Ok(None);
        };
        ctx.decimals_in = decimals_in;
        ctx.decimals_out = decimals_out;
        ctx.stable = stable;
        ctx.factory = factory;
        Ok(Some(ctx))
    }

    /// A token's `decimals()`.
    async fn decimals_of(&self, ctx: &Ctx, token: Address) -> Result<u32> {
        let data = self
            .eth_call(
                ctx,
                token,
                &Token::decimalsCall {}.abi_encode(),
                BlockTag::Latest,
            )
            .await?;
        Token::decimalsCall::abi_decode_returns(&data)
            .map(u32::from)
            .with_context(|| format!("decoding {token}'s decimals"))
    }

    /// What a setup call says about itself.
    fn spec<'s>(
        &self,
        host: &'s str,
        stem: &'s str,
        method: &str,
        params: &[(&str, String)],
    ) -> CallSpec<'s> {
        CallSpec::new(CASE, Expected::ok())
            .host(host)
            .record_as(stem)
            .request(rpc_request(method, params))
    }

    /// As [`Lv2bChain::raw`], for a client the context is not built on yet.
    async fn raw_with(&self, rpc: &EvmRpc, method: &str, params: Value) -> Result<Value> {
        self.run
            .permit_rpc(method, &[("params", params.to_string())])?;
        let reply = rpc.call(method, params).await;
        let body = match &reply {
            Ok(value) => Some(json!({ "result": value })),
            Err(err) => err
                .downcast_ref::<RpcError>()
                .map(|node| json!({"error": {"code": node.code, "message": node.message}})),
        };
        if let Some(body) = body {
            self.run
                .gate()
                .observed("RPC", method, 200, &body.to_string());
        }
        reply
    }

    // --- the wallet, in dollars ---

    /// The wallet's value in USD: its native balance at `VP_NATIVE_USD`, its input
    /// token at a dollar a unit, and its output token at the pool's own price (what
    /// the router would give for all of it). `None` ends the run: a dry run, or a
    /// read that failed, which the line says.
    async fn wallet_value(&self, ctx: &Ctx, stem: &str) -> Result<Option<Decimal>> {
        let called = self
            .run
            .call(
                CallSpec::new(CASE, Expected::ok())
                    .host(&ctx.host)
                    .record_as(stem)
                    .request(rpc_request(
                        "balances",
                        &[("holder", ctx.signer.to_string())],
                    )),
                || async {
                    self.run
                        .permit_rpc("balances", &[("holder", ctx.signer.to_string())])?;
                    let native = ctx.balances.native(ctx.signer, BlockTag::Latest).await?;
                    let token_in = ctx
                        .balances
                        .token(self.settings.token_in, ctx.signer, BlockTag::Latest)
                        .await?;
                    let token_out = ctx
                        .balances
                        .token(self.settings.token_out, ctx.signer, BlockTag::Latest)
                        .await?;
                    let eth = decimal_of(U256::from(native), 18)?
                        .checked_mul(self.settings.native_usd)
                        .ok_or_else(|| anyhow!("the native balance in USD overflows"))?;
                    let stable = decimal_of(U256::from(token_in), ctx.decimals_in)?;
                    let liquidation = if token_out == 0 {
                        Decimal::ZERO
                    } else {
                        let bought = self
                            .quote_at(
                                ctx,
                                self.settings.token_out,
                                self.settings.token_in,
                                U256::from(token_out),
                                BlockTag::Latest,
                            )
                            .await?;
                        decimal_of(bought, ctx.decimals_in)?
                    };
                    eth.checked_add(stable)
                        .and_then(|sum| sum.checked_add(liquidation))
                        .ok_or_else(|| anyhow!("the wallet's value overflows"))
                },
            )
            .await?;
        if called.stops_the_run() {
            return Ok(None);
        }
        if let Err(err) = &called.result {
            // The ledger cannot value what it cannot read: the run stops.
            self.run
                .ledger()
                .stop(format!("the wallet could not be valued: {err:#}"));
        }
        Ok(called.result.ok())
    }

    // --- one swap ---

    async fn swap(&self, ctx: &Ctx, progress: &mut Progress, number: usize) -> Result<Flow> {
        let (token_in, token_out) = (self.settings.token_in, self.settings.token_out);
        let buy = progress.held.is_zero();
        let (from, to) = if buy {
            (token_in, token_out)
        } else {
            (token_out, token_in)
        };
        let amount_in = if buy {
            units(self.settings.swap_usd, ctx.decimals_in)?
        } else {
            progress.held
        };
        let name = |what: &str| format!("{number:03}-{what}");
        let routes = self.routes(ctx, from, to);

        // The quote, at `pending`, and the block it was read at.
        let quote_params = [("amountIn", amount_in.to_string())];
        let quoted = self
            .run
            .call(
                CallSpec::new(CASE, Expected::ok())
                    .host(&ctx.host)
                    .record_as(&name("quote"))
                    .request(rpc_request("eth_call", &quote_params)),
                || async {
                    self.run.permit_rpc("eth_call", &quote_params)?;
                    let block = ctx.rpc.block_number().await?;
                    let local_ns = now_ns();
                    let data = quote_calldata(amount_in, routes.clone());
                    let returned = ctx
                        .rpc
                        .eth_call(self.settings.router, &data, None, BlockTag::Pending, None)
                        .await?;
                    let amount_out = amount_out_of(&returned)?;
                    self.run.gate().observed(
                        "RPC",
                        "eth_call",
                        200,
                        &json!({"result": hex_data(&returned)}).to_string(),
                    );
                    Ok(Quote {
                        amount_out,
                        block,
                        local_ns,
                    })
                },
            )
            .await?;
        let Ok(quote) = quoted.result else {
            return Ok(Flow::Stop);
        };
        let amount_out_min = quote
            .amount_out
            .checked_mul(U256::from(10_000 - SLIPPAGE_BPS))
            .ok_or_else(|| anyhow!("the quote times the slippage overflows"))?
            / U256::from(10_000);

        // The ledger's say-so, in dollars.
        let notional = if buy {
            self.settings.swap_usd
        } else {
            decimal_of(quote.amount_out, ctx.decimals_in)?
        };
        if let Err(stop) = self.run.ledger().authorise(notional) {
            self.run
                .note(CASE, Expected::ok(), Verdict::Halted, &stop.to_string())?;
            return Ok(Flow::Stop);
        }

        // Approve exactly the amount (a no-op where it is already allowed).
        let approve_params = [
            ("token", from.to_string()),
            ("amount", amount_in.to_string()),
        ];
        let approved = self
            .run
            .call(
                CallSpec::new(CASE, Expected::ok())
                    .host(&ctx.host)
                    .record_as(&name("approve"))
                    .request(rpc_request("eth_sendRawTransaction", &approve_params)),
                || async {
                    self.run
                        .permit_rpc("eth_sendRawTransaction", &approve_params)?;
                    ctx.sender
                        .ensure_allowance(from, self.settings.router, amount_in)
                        .await
                },
            )
            .await?;
        if approved.result.is_err() {
            return Ok(Flow::Stop);
        }

        // The swap.
        let nonce_before = ctx
            .rpc
            .transaction_count(ctx.signer, BlockTag::Latest)
            .await?;
        let block_at_send = ctx.rpc.block_number().await?;
        let deadline = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs())
            + DEADLINE_SECS;
        let calldata = swap_calldata(amount_in, amount_out_min, routes, ctx.signer, deadline);
        let mut payload = self.settings.router.to_vec();
        payload.extend_from_slice(&calldata);
        let route_quote = RouteQuote {
            network: Network::evm(BASE_CHAIN_ID),
            token_in: from.to_vec(),
            token_out: to.to_vec(),
            amount_in: u128::try_from(amount_in).context("the amount in does not fit a u128")?,
            expected_amount_out: u128::try_from(quote.amount_out).unwrap_or(u128::MAX),
            payload,
        };
        let request = SwapRequest {
            sender: ctx.signer.to_vec(),
            recipient: ctx.signer.to_vec(),
            payer: Payer::Sender,
            min_amount_out: u128::try_from(amount_out_min).unwrap_or(u128::MAX),
            deadline_unix_secs: deadline,
            priority: PriorityBid::Policy,
        };
        let prepared = ctx.live.prepare(&route_quote, &request).await?;

        let swap_params = [
            ("router", self.settings.router.to_string()),
            ("amountIn", amount_in.to_string()),
        ];
        let called = self
            .run
            .call_graded(
                CallSpec::new(CASE, Expected::ok())
                    .host(&ctx.host)
                    .record_as(&name("swap"))
                    .request(rpc_request("eth_sendRawTransaction", &swap_params)),
                || async {
                    self.run
                        .permit_rpc("eth_sendRawTransaction", &swap_params)?;
                    self.send_and_gather(ctx, &prepared, from, to, nonce_before)
                        .await
                },
                |result| {
                    let facts = match result {
                        Ok(landing) => SwapFacts {
                            quote: Some(&quote),
                            calldata: &calldata,
                            router: self.settings.router,
                            token_in: from,
                            token_out: to,
                            amount_in,
                            sent_ns: landing.sent_ns,
                            tx_hash: landing.tx_hash,
                            first_seen_ns: landing.first_seen_ns,
                            sealed_ns: landing.sealed_ns,
                            realised: Some(&landing.realised),
                            receipt: landing.evidence.as_ref().ok().map(|e| &e.receipt),
                            block_tx_count: landing
                                .evidence
                                .as_ref()
                                .ok()
                                .and_then(|e| e.block_tx_count),
                        },
                        Err(_) => SwapFacts {
                            quote: Some(&quote),
                            calldata: &calldata,
                            router: self.settings.router,
                            token_in: from,
                            token_out: to,
                            amount_in,
                            sent_ns: None,
                            tx_hash: None,
                            first_seen_ns: None,
                            sealed_ns: None,
                            realised: None,
                            receipt: None,
                            block_tx_count: None,
                        },
                    };
                    Some(swap_references(&facts))
                },
                |_| Grade::Pass,
            )
            .await?;
        let Ok(landing) = called.result else {
            return Ok(Flow::Stop);
        };
        progress.attempted += 1;

        match &landing.realised.outcome {
            Outcome::TimedOut => {
                self.run.computed(
                    CASE,
                    Verdict::Fail,
                    &format!(
                        "swap {number} timed out waiting for a receipt (tx {:?}): it may still \
                         land, and the sender holds it as unresolved",
                        landing.tx_hash
                    ),
                    None,
                )?;
                return Ok(Flow::Stop);
            }
            Outcome::Expired => {
                self.run.computed(
                    CASE,
                    Verdict::Fail,
                    "an EVM swap reported itself expired",
                    None,
                )?;
                return Ok(Flow::Stop);
            }
            Outcome::Reverted { .. } => progress.reverts += 1,
            Outcome::Success => {}
        }

        // The exact checks, all six, on what was gathered.
        let mut stop = !self.checks(&landing, from, to, ctx.signer, nonce_before)?;

        // A landing five or more blocks after the send.
        let late = landing.realised.at.saturating_sub(block_at_send);
        if late >= LATE_BLOCKS {
            self.run.computed(
                CASE,
                Verdict::Fail,
                &format!(
                    "swap {number} landed in block {}, {late} blocks after the send at block \
                     {block_at_send}",
                    landing.realised.at
                ),
                None,
            )?;
            stop = true;
        }

        // What the swap changed.
        match (&landing.realised.outcome, landing.realised.amount_out) {
            (Outcome::Success, Some(out)) if buy => {
                progress.held = U256::from(out);
            }
            (Outcome::Success, _) if !buy => {
                let taken = landing.realised.amount_in.unwrap_or(0);
                progress.held = progress.held.saturating_sub(U256::from(taken));
            }
            _ => {}
        }
        if progress.reverts * 10 > progress.attempted {
            self.run.computed(
                CASE,
                Verdict::Fail,
                &format!(
                    "{} of {} swaps reverted: more than 10 %",
                    progress.reverts, progress.attempted
                ),
                None,
            )?;
            stop = true;
        }

        // The loss, in dollars.
        match self.wallet_value(ctx, &name("wallet")).await? {
            Some(value) => {
                let spent = progress.value - value;
                progress.value = value;
                if let Err(stop_reason) = self.run.ledger().record_loss(spent) {
                    self.run.note(
                        CASE,
                        Expected::ok(),
                        Verdict::Halted,
                        &stop_reason.to_string(),
                    )?;
                    stop = true;
                }
                match self.run.ledger().loss() {
                    Ok(loss) if loss > self.run.ledger().spend_cap() => {
                        self.run.note(
                            CASE,
                            Expected::ok(),
                            Verdict::Halted,
                            &format!(
                                "the run has lost {loss} USD, over its cap of {} USD",
                                self.run.ledger().spend_cap()
                            ),
                        )?;
                        stop = true;
                    }
                    Err(reason) => {
                        self.run.note(
                            CASE,
                            Expected::ok(),
                            Verdict::Halted,
                            &reason.to_string(),
                        )?;
                        stop = true;
                    }
                    Ok(_) => {}
                }
            }
            None => stop = true,
        }
        Ok(if stop { Flow::Stop } else { Flow::Go })
    }

    /// Sends the swap and gathers what the checks and the references need: when it
    /// left and was seen and sealed, the raw receipt, the block's length, the
    /// signer's balances either side of the block, and its nonce.
    async fn send_and_gather(
        &self,
        ctx: &Ctx,
        prepared: &crate::dex::Prepared,
        from: Address,
        to: Address,
        _nonce_before: u64,
    ) -> Result<Landing> {
        let broadcasts_before = ctx.taps.broadcasts.lock().unwrap().len();
        let sampler = Sampler::start(ctx.rpc.clone(), self.settings.poll);
        let realised = ctx.live.execute(prepared, None).await;
        let samples = sampler.finish().await;
        let realised = realised?;

        let sent_ns = ctx
            .taps
            .broadcasts
            .lock()
            .unwrap()
            .get(broadcasts_before)
            .copied();
        let tx_hash = realised
            .tx_ref
            .as_deref()
            .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
            .map(B256::from);
        let first_seen_ns =
            tx_hash.and_then(|hash| ctx.taps.first_seen.lock().unwrap().get(&hash).copied());
        let landed = matches!(
            realised.outcome,
            Outcome::Success | Outcome::Reverted { .. }
        );
        let sealed_ns = if landed {
            self.sealed_ns(ctx, realised.at, &samples).await
        } else {
            None
        };
        let evidence = match (landed, tx_hash) {
            (true, Some(hash)) => self
                .evidence(ctx, hash, realised.at, from, to)
                .await
                .map_err(|err| format!("{err:#}")),
            _ => Err("the swap did not land, so there is no receipt to read".to_string()),
        };
        Ok(Landing {
            realised,
            tx_hash,
            sent_ns,
            first_seen_ns,
            sealed_ns,
            evidence,
        })
    }

    /// The first time the node's block number was at or past `block`: in the
    /// samples taken while the swap was in flight, else by polling on.
    async fn sealed_ns(&self, ctx: &Ctx, block: u64, samples: &[(u128, u64)]) -> Option<u128> {
        if let Some((at, _)) = samples.iter().find(|(_, seen)| *seen >= block) {
            return Some(*at);
        }
        let deadline = tokio::time::Instant::now() + SEAL_TIMEOUT;
        while tokio::time::Instant::now() < deadline {
            if let Ok(seen) = ctx.rpc.block_number().await {
                if seen >= block {
                    return Some(now_ns());
                }
            }
            tokio::time::sleep(self.settings.poll).await;
        }
        None
    }

    async fn evidence(
        &self,
        ctx: &Ctx,
        hash: B256,
        block: u64,
        from: Address,
        to: Address,
    ) -> Result<Evidence> {
        let receipt = self
            .raw(ctx, "eth_getTransactionReceipt", json!([hash.to_string()]))
            .await?;
        if receipt.is_null() {
            bail!("the node has no receipt for {hash} now");
        }
        let block_tx_count = self
            .run
            .permit_rpc("eth_getBlockTransactionCountByNumber", &[])
            .and(
                ctx.rpc
                    .block_transaction_count(BlockTag::Number(block))
                    .await,
            )
            .ok();
        let before = block
            .checked_sub(1)
            .ok_or_else(|| anyhow!("a swap in block 0 has no block before it"))?;
        let (at_before, at_block) = (BlockTag::Number(before), BlockTag::Number(block));
        self.run
            .permit_rpc("balances", &[("block", block.to_string())])?;
        let native = (
            ctx.balances.native(ctx.signer, at_before).await?,
            ctx.balances.native(ctx.signer, at_block).await?,
        );
        let token_from = (
            ctx.balances.token(from, ctx.signer, at_before).await?,
            ctx.balances.token(from, ctx.signer, at_block).await?,
        );
        let token_to = (
            ctx.balances.token(to, ctx.signer, at_before).await?,
            ctx.balances.token(to, ctx.signer, at_block).await?,
        );
        let nonce_after = ctx
            .rpc
            .transaction_count(ctx.signer, BlockTag::Latest)
            .await?;
        Ok(Evidence {
            receipt,
            block_tx_count,
            native,
            token_from,
            token_to,
            nonce_after,
        })
    }

    /// X1 to X6 on a landed swap, one line each; whether all passed (or were
    /// skipped with a reason).
    fn checks(
        &self,
        landing: &Landing,
        from: Address,
        to: Address,
        signer: Address,
        nonce_before: u64,
    ) -> Result<bool> {
        let line = |case: &str, checked: std::result::Result<(), String>| -> Result<bool> {
            match checked {
                Ok(()) => {
                    self.run.computed(case, Verdict::Pass, "", None)?;
                    Ok(true)
                }
                Err(why) => {
                    self.run.computed(case, Verdict::Fail, &why, None)?;
                    Ok(false)
                }
            }
        };
        let evidence = match &landing.evidence {
            Ok(evidence) => evidence,
            Err(why) => {
                let mut all = true;
                for case in ["X1", "X2", "X3", "X4", "X5", "X6"] {
                    all &= line(case, Err(format!("no evidence to check against: {why}")))?;
                }
                return Ok(all);
            }
        };
        let (realised, receipt) = (&landing.realised, &evidence.receipt);
        let reverted = matches!(realised.outcome, Outcome::Reverted { .. });
        let mut all = true;
        if reverted {
            // A swap that reverted has no amounts; it moved no tokens.
            for case in ["X1", "X2"] {
                self.run.computed(
                    case,
                    Verdict::Skipped,
                    "the swap reverted: it has no amounts to compare",
                    None,
                )?;
            }
        } else {
            all &= line("X1", x1_amount_out(realised, receipt, to, signer))?;
            all &= line("X2", x2_amount_in(realised, receipt, from, signer))?;
        }
        all &= line("X3", x3_cost(realised, receipt))?;
        all &= line(
            "X4",
            x4_native_change(receipt, evidence.native.0, evidence.native.1),
        )?;
        let (taken, delivered) = if reverted {
            (0, 0)
        } else {
            (
                realised.amount_in.unwrap_or(0),
                realised.amount_out.unwrap_or(0),
            )
        };
        all &= line(
            "X5",
            x5_token_changes(
                taken,
                delivered,
                (
                    U256::from(evidence.token_from.0),
                    U256::from(evidence.token_from.1),
                ),
                (
                    U256::from(evidence.token_to.0),
                    U256::from(evidence.token_to.1),
                ),
            ),
        )?;
        all &= line(
            "X6",
            x6_landing(realised, receipt, nonce_before, evidence.nonce_after),
        )?;
        Ok(all)
    }

    // --- the exit ---

    fn exit(&self, start: Decimal, progress: &Progress, finished: bool) -> Result<()> {
        let loss = self
            .run
            .ledger()
            .loss()
            .map_or_else(|stop| stop.to_string(), |loss| format!("{loss} USD"));
        let held = if progress.held.is_zero() {
            String::new()
        } else {
            format!(
                "; the wallet still holds {} of the output token",
                progress.held
            )
        };
        let reason = format!(
            "{} of {} swaps made{}; {} reverted; the wallet went from {start} to {} USD; the \
             ledger counts a loss of {loss}{held}",
            progress.attempted,
            self.settings.swaps,
            if finished { "" } else { " (the run stopped)" },
            progress.reverts,
            progress.value,
        );
        self.run.computed(
            "EXIT",
            if finished {
                Verdict::Pass
            } else {
                Verdict::Skipped
            },
            &reason,
            None,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::production::env::{MapEnv, ProcessEnv};
    use crate::production::gate::Echo;
    use crate::production::lv1_base::Throwaway;
    use std::str::FromStr;

    fn d(text: &str) -> Decimal {
        Decimal::from_str(text).unwrap()
    }

    // --- pacing ---

    #[test]
    fn the_same_seed_gives_the_same_intervals_and_another_seed_another() {
        let mean = Duration::from_secs(90);
        let draw = |seed| {
            let mut pacer = Pacer::with_seed(mean, seed);
            (0..5).map(|_| pacer.next()).collect::<Vec<_>>()
        };
        assert_eq!(draw(7), draw(7));
        assert_ne!(draw(7), draw(8));
    }

    /// Exponential with a mean of 90 s: the mean of many draws is near it, none is
    /// negative, and none is over ten means.
    #[test]
    fn the_intervals_are_exponential_with_the_mean_and_capped_at_ten_means() {
        let mean = Duration::from_secs(90);
        let mut pacer = Pacer::with_seed(mean, 0x9E37_79B9_7F4A_7C15);
        let draws: Vec<f64> = (0..20_000).map(|_| pacer.next().as_secs_f64()).collect();
        let average = draws.iter().sum::<f64>() / draws.len() as f64;
        assert!((average - 90.0).abs() < 3.0, "the mean was {average}");
        assert!(draws.iter().all(|secs| (0.0..=900.0).contains(secs)));
        // Exponential: about 63 % are under one mean.
        let under = draws.iter().filter(|secs| **secs < 90.0).count() as f64 / draws.len() as f64;
        assert!((under - 0.632).abs() < 0.02, "{under} under one mean");
        assert_eq!(Pacer::with_seed(Duration::ZERO, 1).next(), Duration::ZERO);
    }

    // --- the settings and the key ---

    fn vars() -> Vec<(&'static str, String)> {
        let key = Throwaway::generate().unwrap();
        vec![
            (
                "VP_BASE_RPC_URL",
                "https://base.example/KEY-IN-PATH".to_string(),
            ),
            ("VP_ROUTER", Address::new([0x12; 20]).to_string()),
            ("VP_POOL", Address::new([0x34; 20]).to_string()),
            ("VP_TOKEN_IN", Address::new([0xa1; 20]).to_string()),
            ("VP_TOKEN_OUT", Address::new([0xb2; 20]).to_string()),
            ("VP_NATIVE_USD", "3000".to_string()),
            ("VP_SIGNER_KEY_HEX", key.key_hex().to_string()),
        ]
    }

    fn env(vars: &[(&str, String)]) -> MapEnv {
        let pairs: Vec<(&str, &str)> = vars.iter().map(|(k, v)| (*k, v.as_str())).collect();
        MapEnv::new(&pairs)
    }

    #[test]
    fn the_key_is_read_once_and_is_gone_from_the_environment_after() {
        let vars = vars();
        let env = env(&vars);
        let (settings, signer) = Settings::from_env(&env).unwrap();
        assert!(env.var("VP_SIGNER_KEY_HEX").is_none(), "taken, not copied");
        assert_eq!(settings.swaps, 60);
        assert_eq!(
            (settings.swap_usd, settings.native_usd),
            (d("10"), d("3000"))
        );
        assert_eq!(settings.mean_interval, Duration::from_secs(90));
        assert_eq!(settings.poll, Duration::from_millis(50));
        assert!(!signer.address().is_zero());
        // Nothing the settings print holds the key.
        let key = vars
            .iter()
            .find(|(k, _)| *k == "VP_SIGNER_KEY_HEX")
            .unwrap()
            .1
            .clone();
        assert!(!format!("{settings:?}").contains(&key));
    }

    #[test]
    fn a_missing_variable_or_a_bad_key_is_an_error_that_names_it_and_never_the_key() {
        for missing in [
            "VP_BASE_RPC_URL",
            "VP_ROUTER",
            "VP_POOL",
            "VP_TOKEN_IN",
            "VP_TOKEN_OUT",
            "VP_NATIVE_USD",
            "VP_SIGNER_KEY_HEX",
        ] {
            let vars: Vec<_> = vars().into_iter().filter(|(k, _)| *k != missing).collect();
            let err = Settings::from_env(&env(&vars)).err().expect("refused");
            assert!(format!("{err:#}").contains(missing), "{missing}: {err:#}");
        }
        let mut bad = vars();
        bad.retain(|(k, _)| *k != "VP_SIGNER_KEY_HEX");
        bad.push(("VP_SIGNER_KEY_HEX", "not-a-key-0123".to_string()));
        let err = format!("{:#}", Settings::from_env(&env(&bad)).err().unwrap());
        assert!(
            err.contains("VP_SIGNER_KEY_HEX") && !err.contains("not-a-key-0123"),
            "{err}"
        );
        let mut not_address = vars();
        not_address.retain(|(k, _)| *k != "VP_ROUTER");
        not_address.push(("VP_ROUTER", "0xrouter".to_string()));
        assert!(format!(
            "{:#}",
            Settings::from_env(&env(&not_address)).err().unwrap()
        )
        .contains("VP_ROUTER"));
    }

    // --- the calldata ---

    #[test]
    fn the_calldata_starts_with_the_selectors_of_the_two_router_functions() {
        let routes = vec![route(
            Address::new([1; 20]),
            Address::new([2; 20]),
            false,
            Address::new([3; 20]),
        )];
        let swap = swap_calldata(
            U256::from(1),
            U256::from(2),
            routes.clone(),
            Address::new([4; 20]),
            99,
        );
        assert_eq!(
            swap[..4],
            keccak256(
                b"swapExactTokensForTokens(uint256,uint256,(address,address,bool,address)[],address,uint256)"
            )[..4]
        );
        let quote = quote_calldata(U256::from(1), routes);
        assert_eq!(
            quote[..4],
            keccak256(b"getAmountsOut(uint256,(address,address,bool,address)[])")[..4]
        );
    }

    #[test]
    fn units_scale_dollars_to_the_tokens_decimals_and_a_reply_decodes_to_its_last_amount() {
        assert_eq!(units(d("10"), 6).unwrap(), U256::from(10_000_000u64));
        assert_eq!(
            units(d("0.5"), 18).unwrap(),
            U256::from(500_000_000_000_000_000u128)
        );
        let reply =
            Router::getAmountsOutCall::abi_encode_returns(&vec![U256::from(5), U256::from(9)]);
        assert_eq!(amount_out_of(&reply).unwrap(), U256::from(9));
        assert!(amount_out_of(&[]).is_err());
        assert_eq!(decimal_of(U256::from(1_500_000u64), 6).unwrap(), d("1.5"));
    }

    // --- the test a person runs ---

    /// LV2b against Base mainnet:
    ///
    /// ```text
    /// VP_DRY_RUN=1 cargo test --lib production_lv2b_chain -- --ignored --nocapture
    /// cargo test --lib production_lv2b_chain -- --ignored --nocapture
    /// ```
    ///
    /// Needs `VP_BASE_RPC_URL`, `VP_SIGNER_KEY_HEX` (a funded throwaway wallet, read
    /// once), `VP_ROUTER`, `VP_POOL`, `VP_TOKEN_IN` (USD-pegged), `VP_TOKEN_OUT`,
    /// `VP_NATIVE_USD`, `VP_SPEND_CAP_USD`, `VP_HALT_FILE` and `VP_OUT`; optionally
    /// `VP_SWAP_USD`, `VP_ORDER_CAP_USD` and `VP_RECORD_DIR`.
    #[tokio::test]
    #[ignore = "sends real swaps on Base mainnet with a real key: see specs/V7-production-validation.md"]
    async fn production_lv2b_chain() {
        let env = ProcessEnv;
        // The run reads the key's value first, so that no line can hold it.
        let run =
            Run::start(&env, "base", "base-mainnet", Echo::Stdout).expect("the run's settings");
        let (settings, signer) = Settings::from_env(&env).expect("the case settings");
        Lv2bChain::new(&run, settings)
            .run_all(signer)
            .await
            .expect("the run");
        run.finish().expect("LV2b is clean");
    }
}
