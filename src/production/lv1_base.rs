//! LV1 against Base (`specs/V7-production-validation.md`, "LV1"): what a node
//! answers to a transaction it must refuse, to the requests a provider may
//! serve differently, and to reads of a pool. One test,
//! `production_lv1_base`, runs the cases for each provider in
//! `VP_BASE_RPC_URLS` with a **throwaway key** that controls nothing and holds
//! nothing; each case is a method here with a self-test against a mock node.
//!
//! | # | Request | Expected |
//! |---|---|---|
//! | GUARD | `web3_clientVersion` | not anvil (`-32601` is "not anvil") |
//! | C1 | an unfunded signer sends a zero-value self-transfer | the node refuses, and not as a revert; its wording is recorded |
//! | C2 | `transfer(…, 1)` of the quote token from the throwaway address | the estimate reverts; nothing is broadcast; the nonce is unchanged |
//! | C3 | `connect` with chain id 1 to a Base node | refused at connect, naming both ids |
//! | C4 | `eth_getBlockByNumber("pending")`, `eth_call` at `pending`, `eth_maxPriorityFeePerGas`, a log range over the provider's limit | each reply's shape is recorded; nothing is asserted of the provider |
//! | C5 | a closed port; a node that sleeps past the request timeout | an error that is not a revert (offline) |
//! | C6 | a pool's `factory()`, `stable()`, `getReserves()`, its factory's `getFee`, its code | the raw replies are recorded and decode |
//! | EXIT | the throwaway address's native and quote balances | unchanged to the unit |
//!
//! **One request at a time**, never a burst: each call waits for the last, and
//! C4 pauses between its four.
//!
//! **A provider's URL carries its key.** It is never written: the run hides
//! each URL and the path and query of each from every line, and a line names
//! the provider by its host alone.

use crate::balance::{EvmBalanceReader, EvmBalances};
use crate::evm::rpc::{decode_hex, hex_data};
use crate::evm::{BlockTag, EvmRpc, EvmSender, FeePolicy, RpcError, Signer};
use crate::production::env::Env;
use crate::production::guard;
use crate::production::results::{Expected, Params, RequestRecord, Tally, Verdict};
use crate::production::wire::Wire;
use crate::production::{CallSpec, Called, Run};
use alloy_primitives::{keccak256, Address, U256};
use alloy_sol_types::SolCall;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Base mainnet.
pub(crate) const BASE_CHAIN_ID: u64 = 8453;

/// How long a request to a node may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// How far back C4's log range goes: far enough over any provider's limit.
const LOG_RANGE_BLOCKS: u64 = 100_000;

/// `keccak256("Transfer(address,address,uint256)")`.
const TRANSFER_TOPIC: &str = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";

mod abi {
    alloy_sol_types::sol! {
        interface Erc20 {
            function transfer(address to, uint256 amount) external returns (bool);
            function balanceOf(address owner) external view returns (uint256);
        }

        interface Pool {
            function stable() external view returns (bool);
            function getReserves() external view returns (uint256 reserve0, uint256 reserve1, uint256 blockTimestampLast);
            function factory() external view returns (address);
        }

        interface Factory {
            function getFee(address pool, bool stable) external view returns (uint256);
        }
    }
}

/// The throwaway address's nonce as `(latest, pending)`, before and after C2's send.
type NonceReads = ((u64, u64), (u64, u64));

/// What the run is pointed at.
#[derive(Debug, Clone)]
pub(crate) struct Settings {
    /// `VP_BASE_RPC_URLS`, comma-separated (`VP_BASE_RPC_URL` for one).
    pub(crate) rpc_urls: Vec<String>,
    /// `VP_TOKEN_QUOTE`: the quote token (USDC on Base). C2, C4 and the exit need it.
    pub(crate) token_quote: Option<Address>,
    /// `VP_POOL`: the pool C6 reads.
    pub(crate) pool: Option<Address>,
}

impl Settings {
    pub(crate) fn from_env(env: &dyn Env) -> Result<Self> {
        let urls = env
            .var("VP_BASE_RPC_URLS")
            .or_else(|| env.var("VP_BASE_RPC_URL"))
            .ok_or_else(|| anyhow!("VP_BASE_RPC_URLS is not set and has no default"))?;
        let rpc_urls: Vec<String> = urls
            .split(',')
            .map(|url| url.trim().to_string())
            .filter(|url| !url.is_empty())
            .collect();
        if rpc_urls.is_empty() {
            bail!("VP_BASE_RPC_URLS names no node");
        }
        let address = |var: &str| {
            env.var(var)
                .map(|text| {
                    text.parse::<Address>()
                        .with_context(|| format!("{var} is {text:?}, which is not an address"))
                })
                .transpose()
        };
        Ok(Self {
            rpc_urls,
            token_quote: address("VP_TOKEN_QUOTE")?,
            pool: address("VP_POOL")?,
        })
    }
}

/// A throwaway key: derived from a hash of the time, the process id and a
/// counter, so that it controls nothing anyone could have known and holds
/// nothing. It is never written anywhere, and has no `Debug`.
pub(crate) struct Throwaway {
    key_hex: String,
    address: Address,
}

impl Throwaway {
    fn generate() -> Result<Self> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        loop {
            let mut seed = Vec::new();
            seed.extend(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |elapsed| elapsed.as_nanos())
                    .to_be_bytes(),
            );
            seed.extend(std::process::id().to_be_bytes());
            seed.extend(COUNTER.fetch_add(1, Ordering::SeqCst).to_be_bytes());
            let key_hex = hex::encode(keccak256(&seed));
            // A hash is a valid secp256k1 scalar but for odds of 2^-128; if it
            // is not, the next counter gives another.
            if let Ok(signer) = Signer::from_private_key_hex(&key_hex) {
                return Ok(Self {
                    key_hex,
                    address: signer.address(),
                });
            }
        }
    }

    fn signer(&self) -> Result<Signer> {
        Signer::from_private_key_hex(&self.key_hex)
    }
}

/// One provider.
pub(crate) struct Node {
    index: usize,
    host: String,
    rpc: EvmRpc,
}

impl Node {
    fn stem(&self, name: &str) -> String {
        format!("p{}-{name}", self.index)
    }
}

/// The host of `url` alone: a provider's path and query are its key.
fn host_of(url: &str) -> String {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .unwrap_or_else(|| "unparsable-url".to_string())
}

fn rpc_request(method: &str, params: &[(&str, String)]) -> RequestRecord {
    RequestRecord {
        method: "RPC".to_string(),
        path: method.to_string(),
        params: Params(
            params
                .iter()
                .map(|(key, value)| ((*key).to_string(), value.clone()))
                .collect(),
        ),
    }
}

/// Whether the run goes on with this provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flow {
    Go,
    Stop,
}

fn flow<T>(called: &Called<T>) -> Flow {
    if called.stops_the_run() {
        Flow::Stop
    } else {
        Flow::Go
    }
}

pub(crate) struct Lv1Base<'a> {
    run: &'a Run,
    settings: Settings,
    request_timeout: Duration,
    /// Pause between two requests to a provider that asks nothing of it but
    /// answers.
    pause: Duration,
    /// A node on this machine is accepted (the self-tests' mocks are).
    allow_local: bool,
}

impl<'a> Lv1Base<'a> {
    pub(crate) fn new(run: &'a Run, settings: Settings) -> Self {
        run.hide_urls(&settings.rpc_urls);
        Self {
            run,
            settings,
            request_timeout: REQUEST_TIMEOUT,
            pause: Duration::from_millis(250),
            allow_local: false,
        }
    }

    /// For a self-test: short waits, and a mock node on this machine is a node.
    #[cfg(test)]
    fn against_mocks(mut self) -> Self {
        self.request_timeout = Duration::from_secs(2);
        self.pause = Duration::ZERO;
        self.allow_local = true;
        self
    }

    /// Every provider's cases, then C5. A provider that is not a production node
    /// (a local URL, an anvil) stops the run.
    pub(crate) async fn run_all(&self) -> Result<Tally> {
        for (index, url) in self.settings.rpc_urls.iter().enumerate() {
            if !self.allow_local {
                guard::node_url(url)
                    .map_err(|refused| anyhow!("provider {index} ({}): {refused}", host_of(url)))?;
            }
            let node = Node {
                index,
                host: host_of(url),
                rpc: EvmRpc::with_request_timeout(url.clone(), self.request_timeout),
            };
            if self.provider(&node).await? == Flow::Stop {
                return Ok(self.run.tally());
            }
        }
        self.c5().await?;
        Ok(self.run.tally())
    }

    async fn provider(&self, node: &Node) -> Result<Flow> {
        let throwaway = Throwaway::generate()?;
        macro_rules! go {
            ($case:expr) => {
                if $case.await? == Flow::Stop {
                    return Ok(Flow::Stop);
                }
            };
        }
        go!(self.guard(node));
        let (flow, before) = self.balances(node, &throwaway, "before", None).await?;
        if flow == Flow::Stop {
            return Ok(Flow::Stop);
        }
        go!(self.c1(node, &throwaway));
        go!(self.c2(node, &throwaway));
        go!(self.c3(node, &throwaway));
        self.c4(node, &throwaway).await?;
        go!(self.c6(node));
        let (flow, _) = self.balances(node, &throwaway, "after", before).await?;
        Ok(flow)
    }

    fn skip(&self, node: &Node, case: &str, expected: Expected, why: &str) -> Result<Flow> {
        self.run
            .note_at(Some(&node.host), case, expected, Verdict::Skipped, why)?;
        Ok(Flow::Go)
    }

    // --- raw reads, recorded ---

    /// One JSON-RPC request, asked of the gate first and recorded after: the
    /// reply as `{"result": …}` or `{"error": …}`, the provider's own words.
    async fn rpc(&self, node: &Node, method: &str, params: Value) -> Result<Value> {
        self.run
            .permit_rpc(method, &[("params", params.to_string())])?;
        let reply = node.rpc.call(method, params).await;
        self.record(method, &reply);
        reply
    }

    /// Saves a reply (record mode): what the provider answered, not what this
    /// crate made of it.
    fn record(&self, method: &str, reply: &Result<Value>) {
        let body = match reply {
            Ok(value) => json!({ "result": value }),
            Err(err) => match err.downcast_ref::<RpcError>() {
                Some(node) => json!({"error": {"code": node.code, "message": node.message}}),
                None => return,
            },
        };
        self.run
            .gate()
            .observed("RPC", method, 200, &body.to_string());
    }

    fn eth_call_params(to: Address, data: &[u8], block: &str) -> Value {
        json!([{"to": to.to_string(), "data": hex_data(data)}, block])
    }

    // --- the guard ---

    /// The provider is not an anvil. A node with no `web3_clientVersion` is not
    /// one (anvil has it); any other refusal of it says nothing, and the run
    /// does not go on to send to a node it could not read.
    pub(crate) async fn guard(&self, node: &Node) -> Result<Flow> {
        let stem = node.stem("30-client-version");
        let called = self
            .run
            .call_with(
                CallSpec::new("GUARD", Expected::answer())
                    .host(&node.host)
                    .record_as(&stem)
                    .request(rpc_request("web3_clientVersion", &[])),
                || async { self.rpc(node, "web3_clientVersion", json!([])).await },
                |_| None,
                |result| match result {
                    Ok(version) => match version.as_str() {
                        Some(version) => {
                            guard::node_version(version).map_err(|refused| refused.to_string())
                        }
                        None => Err(format!(
                            "web3_clientVersion answered {version}, not a string"
                        )),
                    },
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
        Ok(match called.verdict {
            Verdict::Fail | Verdict::Halted => Flow::Stop,
            _ => flow(&called),
        })
    }

    // --- the exit check's two reads ---

    /// The throwaway address's native and quote-token balances, read as
    /// `when` (`before` or `after` the cases). `before` is what the first read
    /// found: the second must find the same, to the unit.
    async fn balances(
        &self,
        node: &Node,
        throwaway: &Throwaway,
        when: &str,
        before: Option<(u128, u128)>,
    ) -> Result<(Flow, Option<(u128, u128)>)> {
        let case = "EXIT";
        let Some(token) = self.settings.token_quote else {
            return Ok((
                self.skip(node, case, Expected::ok(), "VP_TOKEN_QUOTE is not set")?,
                None,
            ));
        };
        let reader = EvmBalances::new(node.rpc.clone());
        let address = throwaway.address;
        let stem = node.stem(&format!("40-throwaway-balances-{when}"));
        let called = self
            .run
            .call_with(
                CallSpec::new(case, Expected::ok())
                    .host(&node.host)
                    .record_as(&stem)
                    .request(rpc_request(
                        "balances",
                        &[("holder", address.to_string()), ("when", when.to_string())],
                    )),
                || async {
                    self.run
                        .permit_rpc("balances", &[("holder", address.to_string())])?;
                    let native = reader.native(address, BlockTag::Latest).await?;
                    let quote = reader.token(token, address, BlockTag::Latest).await?;
                    Ok((native, quote))
                },
                |_| None,
                |result| match (result, before) {
                    (Ok(read), Some(before)) if *read != before => Err(format!(
                        "the throwaway address's balances changed during the cases: (native, \
                         quote) {before:?} became {read:?}"
                    )),
                    _ => Ok(()),
                },
            )
            .await?;
        let read = called.result.as_ref().ok().copied();
        Ok((flow(&called), read))
    }

    // --- C1: an unfunded signer sends ---

    /// C1: the throwaway signer sends a zero-value transfer to itself. It holds
    /// nothing, so the node refuses it (`insufficient funds` in one wording or
    /// another, which is recorded), and the refusal is not a revert.
    pub(crate) async fn c1(&self, node: &Node, throwaway: &Throwaway) -> Result<Flow> {
        let address = throwaway.address;
        let params = [("from", address.to_string()), ("value", "0".to_string())];
        let stem = node.stem("31-unfunded-send");
        let connected = AtomicBool::new(false);
        let called = self
            .run
            .call_with(
                CallSpec::new("C1", Expected::refused(&[]))
                    .host(&node.host)
                    .record_as(&stem)
                    .request(rpc_request("eth_sendRawTransaction", &params)),
                || async {
                    self.run.permit_rpc("eth_sendRawTransaction", &params)?;
                    let sender = EvmSender::connect(
                        node.rpc.clone(),
                        throwaway.signer()?,
                        BASE_CHAIN_ID,
                        FeePolicy::default(),
                    )
                    .await?;
                    connected.store(true, Ordering::SeqCst);
                    let sent = sender
                        .send_and_confirm(address, Vec::new(), U256::ZERO)
                        .await;
                    if let Err(err) = &sent {
                        self.record_refusal("eth_sendRawTransaction", err);
                    }
                    sent
                },
                |_| None,
                |result| match result {
                    _ if !connected.load(Ordering::SeqCst) => Err(
                        "the sender could not connect to the node, so nothing was sent".to_string(),
                    ),
                    Err(err) => match err.downcast_ref::<RpcError>() {
                        Some(refusal) if !refusal.is_revert() => Ok(()),
                        Some(refusal) => Err(format!(
                            "the node's refusal reads as a revert: {}",
                            refusal.message
                        )),
                        None => Err(format!("the error is not a node's refusal: {err:#}")),
                    },
                    Ok(_) => Err(
                        "the node mined a transaction from an address that holds nothing"
                            .to_string(),
                    ),
                },
            )
            .await?;
        Ok(flow(&called))
    }

    /// Saves a node's refusal found inside an error (record mode).
    fn record_refusal(&self, method: &str, err: &anyhow::Error) {
        if let Some(node) = err.downcast_ref::<RpcError>() {
            let body = json!({"error": {"code": node.code, "message": node.message}});
            self.run
                .gate()
                .observed("RPC", method, 200, &body.to_string());
        }
    }

    // --- C2: a call whose estimate reverts ---

    /// C2: `transfer(throwaway, 1)` of the quote token from the throwaway
    /// address, which holds none. The node's estimate reverts, the sender
    /// refuses to broadcast it, and the address's nonce is where it was.
    pub(crate) async fn c2(&self, node: &Node, throwaway: &Throwaway) -> Result<Flow> {
        let expected = Expected::refused(&[]);
        let Some(token) = self.settings.token_quote else {
            return self.skip(node, "C2", expected, "VP_TOKEN_QUOTE is not set");
        };
        let address = throwaway.address;
        let calldata = abi::Erc20::transferCall {
            to: address,
            amount: U256::from(1),
        }
        .abi_encode();
        let params = [("from", address.to_string()), ("to", token.to_string())];
        let stem = node.stem("32-estimate-reverts");
        let nonces: std::sync::Mutex<Option<NonceReads>> = std::sync::Mutex::new(None);
        let called = self
            .run
            .call_with(
                CallSpec::new("C2", expected)
                    .host(&node.host)
                    .record_as(&stem)
                    .request(rpc_request("eth_estimateGas", &params)),
                || async {
                    self.run.permit_rpc("eth_estimateGas", &params)?;
                    let sender = EvmSender::connect(
                        node.rpc.clone(),
                        throwaway.signer()?,
                        BASE_CHAIN_ID,
                        FeePolicy::default(),
                    )
                    .await?;
                    let nonce = || async {
                        Ok::<_, anyhow::Error>((
                            node.rpc
                                .transaction_count(address, BlockTag::Latest)
                                .await?,
                            node.rpc
                                .transaction_count(address, BlockTag::Pending)
                                .await?,
                        ))
                    };
                    let before = nonce().await?;
                    let sent = sender
                        .send_and_confirm(token, calldata.clone(), U256::ZERO)
                        .await;
                    let after = nonce().await?;
                    *nonces.lock().unwrap() = Some((before, after));
                    if let Err(err) = &sent {
                        self.record_refusal("eth_estimateGas", err);
                    }
                    sent
                },
                |_| None,
                |result| {
                    match result {
                        Err(err) => match err.downcast_ref::<RpcError>() {
                            Some(refusal) if refusal.is_revert() => {}
                            Some(refusal) => {
                                return Err(format!(
                                    "the node refused the estimate, but not as a revert: {}",
                                    refusal.message
                                ))
                            }
                            None => {
                                return Err(format!("the error is not a node's answer: {err:#}"))
                            }
                        },
                        Ok(_) => return Err("the transfer was sent and mined".to_string()),
                    }
                    match *nonces.lock().unwrap() {
                        Some((before, after)) if before == after => Ok(()),
                        Some((before, after)) => Err(format!(
                            "the throwaway address's nonce (latest, pending) moved from {before:?} \
                             to {after:?}: something was broadcast"
                        )),
                        None => Err("the nonce was not read".to_string()),
                    }
                },
            )
            .await?;
        Ok(flow(&called))
    }

    // --- C3: the wrong chain ---

    /// C3: `connect` with chain id 1 to a Base node is refused, and the refusal
    /// names both ids.
    pub(crate) async fn c3(&self, node: &Node, throwaway: &Throwaway) -> Result<Flow> {
        let stem = node.stem("33-wrong-chain-id");
        let params = [("expected_chain_id", "1".to_string())];
        let called = self
            .run
            .call_with(
                CallSpec::new("C3", Expected::not_sent())
                    .host(&node.host)
                    .record_as(&stem)
                    .request(rpc_request("eth_chainId", &params)),
                || async {
                    self.run.permit_rpc("eth_chainId", &params)?;
                    EvmSender::connect(
                        node.rpc.clone(),
                        throwaway.signer()?,
                        1,
                        FeePolicy::default(),
                    )
                    .await
                    .map(|_| ())
                },
                |_| None,
                |result| match result {
                    Err(err)
                        if format!("{err:#}").contains(&format!(
                            "is on chain {BASE_CHAIN_ID}, not the expected chain 1"
                        )) =>
                    {
                        Ok(())
                    }
                    Err(err) => Err(format!(
                        "the refusal does not name chain {BASE_CHAIN_ID} and chain 1: {err:#}"
                    )),
                    Ok(()) => Err("connect accepted chain id 1 on this node".to_string()),
                },
            )
            .await?;
        Ok(flow(&called))
    }

    // --- C4: what the provider serves ---

    /// C4: four requests whose answers differ from provider to provider, one at
    /// a time. Each reply is recorded; none is judged.
    pub(crate) async fn c4(&self, node: &Node, throwaway: &Throwaway) -> Result<Flow> {
        let expected = Expected::answer;
        let address = throwaway.address;

        let stem = node.stem("34-pending-block");
        let called = self
            .run
            .call(
                CallSpec::new("C4", expected())
                    .host(&node.host)
                    .record_as(&stem)
                    .request(rpc_request(
                        "eth_getBlockByNumber",
                        &[("block", "pending".to_string())],
                    )),
                || async {
                    self.rpc(node, "eth_getBlockByNumber", json!(["pending", false]))
                        .await
                },
            )
            .await?;
        if called.stops_the_run() {
            return Ok(Flow::Stop);
        }
        tokio::time::sleep(self.pause).await;

        match self.settings.token_quote {
            Some(token) => {
                let data = abi::Erc20::balanceOfCall { owner: address }.abi_encode();
                let stem = node.stem("35-eth-call-pending");
                let called = self
                    .run
                    .call(
                        CallSpec::new("C4", expected())
                            .host(&node.host)
                            .record_as(&stem)
                            .request(rpc_request(
                                "eth_call",
                                &[("to", token.to_string()), ("block", "pending".to_string())],
                            )),
                        || async {
                            self.rpc(
                                node,
                                "eth_call",
                                Self::eth_call_params(token, &data, "pending"),
                            )
                            .await
                        },
                    )
                    .await?;
                if called.stops_the_run() {
                    return Ok(Flow::Stop);
                }
                tokio::time::sleep(self.pause).await;
            }
            None => {
                self.skip(
                    node,
                    "C4",
                    expected(),
                    "VP_TOKEN_QUOTE is not set: no eth_call at pending",
                )?;
            }
        }

        let stem = node.stem("36-max-priority-fee");
        let called = self
            .run
            .call(
                CallSpec::new("C4", expected())
                    .host(&node.host)
                    .record_as(&stem)
                    .request(rpc_request("eth_maxPriorityFeePerGas", &[])),
                || async { self.rpc(node, "eth_maxPriorityFeePerGas", json!([])).await },
            )
            .await?;
        if called.stops_the_run() {
            return Ok(Flow::Stop);
        }
        tokio::time::sleep(self.pause).await;

        // A log range over any provider's limit, filtered to the throwaway
        // address as the sender of a quote-token transfer, so that a provider
        // that answers it answers with nothing.
        let Some(token) = self.settings.token_quote else {
            return self.skip(
                node,
                "C4",
                expected(),
                "VP_TOKEN_QUOTE is not set: no log range",
            );
        };
        // The log range is counted back from the latest block, which is read as
        // its own step so that its reply and the log reply are recorded apart.
        let stem = node.stem("37-latest-block");
        let latest = self
            .run
            .call(
                CallSpec::new("C4", expected())
                    .host(&node.host)
                    .record_as(&stem)
                    .request(rpc_request("eth_blockNumber", &[])),
                || async {
                    let latest = self.rpc(node, "eth_blockNumber", json!([])).await?;
                    crate::evm::rpc::parse_hex_u64(latest.as_str().ok_or_else(|| {
                        anyhow!("eth_blockNumber answered {latest}, not a hex string")
                    })?)
                },
            )
            .await?;
        if latest.stops_the_run() {
            return Ok(Flow::Stop);
        }
        let Ok(latest) = latest.result else {
            return self.skip(
                node,
                "C4",
                expected(),
                "the latest block was not read, so no log range",
            );
        };
        tokio::time::sleep(self.pause).await;

        let stem = node.stem("37-log-range");
        let from_topic = format!("0x{}", hex::encode(crate::evm::rpc::pad_address(address)));
        let from_block = latest.saturating_sub(LOG_RANGE_BLOCKS);
        let called = self
            .run
            .call(
                CallSpec::new("C4", expected())
                    .host(&node.host)
                    .record_as(&stem)
                    .request(rpc_request(
                        "eth_getLogs",
                        &[
                            ("blocks", LOG_RANGE_BLOCKS.to_string()),
                            ("address", token.to_string()),
                        ],
                    )),
                || async {
                    self.rpc(
                        node,
                        "eth_getLogs",
                        json!([{
                            "fromBlock": format!("0x{from_block:x}"),
                            "toBlock": "latest",
                            "address": token.to_string(),
                            "topics": [TRANSFER_TOPIC, from_topic]
                        }]),
                    )
                    .await
                },
            )
            .await?;
        Ok(flow(&called))
    }

    // --- C5: a dead node ---

    /// C5: a closed port and a node that sleeps past the request timeout. Each
    /// is an error that is not the node's answer, and so not a revert. Both are
    /// on this machine, so this case needs no network.
    pub(crate) async fn c5(&self) -> Result<Flow> {
        let closed = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0")
                .context("finding a port to leave closed")?;
            format!("http://{}", listener.local_addr()?)
            // The listener is dropped here: nothing listens on the port.
        };
        let slow = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::any())
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_delay(self.request_timeout + Duration::from_secs(2)),
            )
            .mount(&slow)
            .await;
        let flow_closed = self.c5_against("a closed port", &closed).await?;
        let flow_slow = self
            .c5_against("a node that sleeps past the timeout", &slow.uri())
            .await?;
        Ok(if flow_closed == Flow::Stop || flow_slow == Flow::Stop {
            Flow::Stop
        } else {
            Flow::Go
        })
    }

    /// C5 against one endpoint. The request timeout is the run's, and the
    /// endpoint answers nothing within it.
    pub(crate) async fn c5_against(&self, what: &str, url: &str) -> Result<Flow> {
        let rpc =
            EvmRpc::with_request_timeout(url, self.request_timeout.min(Duration::from_secs(2)));
        let called = self
            .run
            .call_with(
                CallSpec::new("C5", Expected::not_sent())
                    .host("localhost")
                    .request(rpc_request(
                        "eth_blockNumber",
                        &[("endpoint", what.to_string())],
                    )),
                || async {
                    self.run
                        .permit_rpc("eth_blockNumber", &[("endpoint", what.to_string())])?;
                    rpc.block_number().await
                },
                |_| None,
                |result| match result {
                    Err(err) => match err.downcast_ref::<RpcError>() {
                        None => Ok(()),
                        Some(answer) => Err(format!(
                            "{what}: the error is a node's answer ({}), not a dead node",
                            answer.message
                        )),
                    },
                    Ok(number) => Err(format!("{what}: answered with block {number}")),
                },
            )
            .await?;
        Ok(flow(&called))
    }

    // --- C6: reads from a pool ---

    /// C6: the views of a Solidly-style pool and its factory, and the hash of the
    /// pool's code. The raw replies are recorded and each must decode.
    pub(crate) async fn c6(&self, node: &Node) -> Result<Flow> {
        let Some(pool) = self.settings.pool else {
            return self.skip(node, "C6", Expected::ok(), "VP_POOL is not set");
        };
        let factory_call = abi::Pool::factoryCall {}.abi_encode();
        let factory = self
            .view(node, "C6", "38-pool-factory", pool, &factory_call, |raw| {
                abi::Pool::factoryCall::abi_decode_returns(raw).map_err(|e| e.to_string())
            })
            .await?;
        let stable_call = abi::Pool::stableCall {}.abi_encode();
        let stable = self
            .view(node, "C6", "39-pool-stable", pool, &stable_call, |raw| {
                abi::Pool::stableCall::abi_decode_returns(raw).map_err(|e| e.to_string())
            })
            .await?;
        let reserves_call = abi::Pool::getReservesCall {}.abi_encode();
        self.view(
            node,
            "C6",
            "41-pool-reserves",
            pool,
            &reserves_call,
            |raw| {
                let reserves = abi::Pool::getReservesCall::abi_decode_returns(raw)
                    .map_err(|e| e.to_string())?;
                Ok((
                    reserves.reserve0,
                    reserves.reserve1,
                    reserves.blockTimestampLast,
                ))
            },
        )
        .await?;
        match (factory.1, stable.1) {
            (Some(factory), Some(stable)) => {
                let fee_call = abi::Factory::getFeeCall { pool, stable }.abi_encode();
                self.view(node, "C6", "42-factory-fee", factory, &fee_call, |raw| {
                    abi::Factory::getFeeCall::abi_decode_returns(raw).map_err(|e| e.to_string())
                })
                .await?;
            }
            _ => {
                self.skip(
                    node,
                    "C6",
                    Expected::ok(),
                    "the pool's factory or stable flag was not read, so the factory's fee was not asked",
                )?;
            }
        }
        let stem = node.stem("43-pool-code");
        let called = self
            .run
            .call_with(
                CallSpec::new("C6", Expected::ok())
                    .host(&node.host)
                    .record_as(&stem)
                    .request(rpc_request("eth_getCode", &[("address", pool.to_string())])),
                || async {
                    let code = self
                        .rpc(node, "eth_getCode", json!([pool.to_string(), "latest"]))
                        .await?;
                    let bytes = decode_hex(code.as_str().ok_or_else(|| {
                        anyhow!("eth_getCode answered {code}, not a hex string")
                    })?)?;
                    Ok(keccak256(&bytes))
                },
                |result| {
                    result.as_ref().ok().map(
                        |hash| json!({"pool": pool.to_string(), "code_hash": hash.to_string()}),
                    )
                },
                |result| match result {
                    Ok(hash) if *hash == keccak256([]) => {
                        Err(format!("{pool} has no code on this chain"))
                    }
                    _ => Ok(()),
                },
            )
            .await?;
        Ok(flow(&called))
    }

    /// One `eth_call` view at `latest`, its raw reply recorded and decoded by
    /// `decode`. The decoded value is returned with the call.
    async fn view<T>(
        &self,
        node: &Node,
        case: &str,
        name: &str,
        to: Address,
        data: &[u8],
        decode: impl FnOnce(&[u8]) -> std::result::Result<T, String>,
    ) -> Result<(Flow, Option<T>)> {
        let stem = node.stem(name);
        let selector = format!("0x{}", hex::encode(&data[..4]));
        let called =
            self.run
                .call(
                    CallSpec::new(case, Expected::ok())
                        .host(&node.host)
                        .record_as(&stem)
                        .request(rpc_request(
                            "eth_call",
                            &[("to", to.to_string()), ("selector", selector)],
                        )),
                    || async {
                        let reply = self
                            .rpc(node, "eth_call", Self::eth_call_params(to, data, "latest"))
                            .await?;
                        let raw = decode_hex(reply.as_str().ok_or_else(|| {
                            anyhow!("eth_call answered {reply}, not a hex string")
                        })?)?;
                        decode(&raw).map_err(|why| {
                            anyhow!("the reply {} does not decode: {why}", hex_data(&raw))
                        })
                    },
                )
                .await?;
        Ok((flow(&called), called.result.ok()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evm::rpc::encode_error_string;
    use crate::production::env::{MapEnv, ProcessEnv};
    use crate::production::gate::Echo;
    use crate::production::testing::{MockNode, Setup};
    use alloy_primitives::B256;
    use wiremock::MockServer;

    // Addresses made up for the mocks. No real token or pool is named here: a
    // run is told which to use.
    const TOKEN: Address = Address::new([0x11; 20]);
    const POOL: Address = Address::new([0x22; 20]);
    const FACTORY: Address = Address::new([0x33; 20]);

    /// A provider's key, in the path of its URL.
    const KEY: &str = "SECRETKEY99";

    fn word(value: u64) -> Value {
        json!(format!("0x{:064x}", value))
    }

    fn words(values: &[u64]) -> Value {
        json!(format!(
            "0x{}",
            values
                .iter()
                .map(|v| format!("{v:064x}"))
                .collect::<String>()
        ))
    }

    fn address_word(address: Address) -> Value {
        json!(format!("0x{:0>64}", hex::encode(address.as_slice())))
    }

    fn revert_data(reason: &str) -> Value {
        json!(hex_data(&encode_error_string(reason)))
    }

    /// A Base node as it answers these cases: it refuses an unfunded signer's
    /// broadcast, reverts the estimate of a transfer of the quote token from an
    /// address that holds none, serves a pool, and has no balance for anyone.
    fn base_node() -> MockNode {
        let token = TOKEN.to_string();
        MockNode::chain(BASE_CHAIN_ID)
            .error(
                "eth_sendRawTransaction",
                -32000,
                "insufficient funds for gas * price + value: address 0xabc have 0 want 1000",
                None,
            )
            .error_when(
                "eth_estimateGas",
                move |params| {
                    params[0]["to"]
                        .as_str()
                        .is_some_and(|to| to.eq_ignore_ascii_case(&token))
                },
                3,
                "execution reverted: ERC20: transfer amount exceeds balance",
                Some(revert_data("ERC20: transfer amount exceeds balance")),
            )
            .result("eth_getBalance", json!("0x0"))
            .eth_call("0x70a08231", word(0))
            .eth_call("0xc45a0155", address_word(FACTORY))
            .eth_call("0x22be3de1", word(1))
            .eth_call("0x0902f1ac", words(&[5_000_000, 7_000_000, 1_700_000_000]))
            .eth_call("0xcc56b2c5", word(30))
            .result(
                "eth_getCode",
                json!("0x608060405234801561001057600080fd5b50"),
            )
            .result("eth_getLogs", json!([]))
            .result(
                "eth_getBlockByNumber",
                json!({"number": "0x65", "baseFeePerGas": "0x3b9aca00"}),
            )
    }

    /// A run against one mock provider, whose URL carries a key in its path.
    struct Lab {
        server: MockServer,
        node: MockNode,
        setup: Setup,
        url: String,
    }

    impl Lab {
        async fn new(name: &str, node: MockNode) -> Self {
            let server = node.start().await;
            let url = format!("{}/v2/{KEY}", server.uri());
            Self {
                setup: Setup::new(name, &server),
                server,
                node,
                url,
            }
        }

        fn with(mut self, key: &str, value: &str) -> Self {
            self.setup = self.setup.with(key, value);
            self
        }

        fn settings(&self) -> Settings {
            Settings {
                rpc_urls: vec![self.url.clone()],
                token_quote: Some(TOKEN),
                pool: Some(POOL),
            }
        }

        fn provider(&self) -> Node {
            Node {
                index: 0,
                host: host_of(&self.url),
                rpc: EvmRpc::with_request_timeout(self.url.clone(), Duration::from_secs(2)),
            }
        }

        fn lines(&self) -> Vec<Value> {
            self.setup.lines()
        }

        fn text(&self) -> String {
            std::fs::read_to_string(self.setup.out.join("results.jsonl")).unwrap()
        }
    }

    fn base(run: &Run, settings: Settings) -> Lv1Base<'_> {
        Lv1Base::new(run, settings).against_mocks()
    }

    fn last(lab: &Lab) -> Value {
        lab.lines().pop().expect("the case wrote a line")
    }

    fn verdict(lab: &Lab) -> String {
        last(lab)["verdict"].as_str().unwrap().to_string()
    }

    fn reason(lab: &Lab) -> String {
        last(lab)["reason"].as_str().unwrap().to_string()
    }

    // --- the selectors this module speaks ---

    #[test]
    fn the_calls_are_the_selectors_the_pool_and_token_answer_to() {
        assert_eq!(abi::Erc20::transferCall::SELECTOR, [0xa9, 0x05, 0x9c, 0xbb]);
        assert_eq!(
            abi::Erc20::balanceOfCall::SELECTOR,
            [0x70, 0xa0, 0x82, 0x31]
        );
        assert_eq!(abi::Pool::factoryCall::SELECTOR, [0xc4, 0x5a, 0x01, 0x55]);
        assert_eq!(abi::Pool::stableCall::SELECTOR, [0x22, 0xbe, 0x3d, 0xe1]);
        assert_eq!(
            abi::Pool::getReservesCall::SELECTOR,
            [0x09, 0x02, 0xf1, 0xac]
        );
        assert_eq!(abi::Factory::getFeeCall::SELECTOR, [0xcc, 0x56, 0xb2, 0xc5]);
        assert_eq!(
            keccak256("Transfer(address,address,uint256)").to_string(),
            TRANSFER_TOPIC
        );
    }

    // --- the throwaway key ---

    #[test]
    fn a_throwaway_key_is_new_each_time_and_signs_for_its_address() {
        let (a, b) = (
            Throwaway::generate().unwrap(),
            Throwaway::generate().unwrap(),
        );
        assert_ne!(a.key_hex, b.key_hex);
        assert_ne!(a.address, b.address);
        assert_eq!(a.key_hex.len(), 64);
        assert_eq!(a.signer().unwrap().address(), a.address);
    }

    // --- GUARD ---

    #[tokio::test]
    async fn the_guard_passes_a_node_that_is_not_an_anvil_and_one_that_has_no_version() {
        for (name, node) in [
            ("reth", base_node()),
            (
                "no-method",
                base_node().error(
                    "web3_clientVersion",
                    -32601,
                    "the method does not exist",
                    None,
                ),
            ),
        ] {
            let lab = Lab::new(name, node).await;
            let run = lab.setup.run(&lab.server).unwrap();
            let flow = base(&run, lab.settings())
                .guard(&lab.provider())
                .await
                .unwrap();
            assert_eq!((verdict(&lab).as_str(), flow), ("pass", Flow::Go), "{name}");
            lab.setup.clean_up();
        }
    }

    #[tokio::test]
    async fn the_guard_stops_the_run_at_an_anvil_and_at_a_node_it_could_not_ask() {
        let anvil = base_node().result("web3_clientVersion", json!("anvil/v1.3.0"));
        let rate_limited =
            base_node().error("web3_clientVersion", -32005, "rate limit exceeded", None);
        for (name, node, expect) in [
            ("anvil", anvil, "anvil"),
            ("limited", rate_limited, "cannot say it is not an anvil"),
        ] {
            let lab = Lab::new(name, node).await;
            let run = lab.setup.run(&lab.server).unwrap();
            let flow = base(&run, lab.settings())
                .guard(&lab.provider())
                .await
                .unwrap();
            assert_eq!(
                (verdict(&lab).as_str(), flow),
                ("fail", Flow::Stop),
                "{name}"
            );
            assert!(reason(&lab).contains(expect), "{name}: {}", reason(&lab));
            lab.setup.clean_up();
        }
    }

    // --- C1 ---

    #[tokio::test]
    async fn c1_an_unfunded_signer_is_refused_and_the_wording_is_recorded() {
        let lab = Lab::new("c1", base_node()).await;
        let run = lab.setup.run(&lab.server).unwrap();
        let throwaway = Throwaway::generate().unwrap();

        let flow = base(&run, lab.settings())
            .c1(&lab.provider(), &throwaway)
            .await
            .unwrap();

        assert_eq!(
            (verdict(&lab).as_str(), flow),
            ("pass", Flow::Go),
            "{}",
            reason(&lab)
        );
        let line = last(&lab);
        assert_eq!(line["outcome"]["kind"], "refused");
        assert_eq!(line["outcome"]["code"], -32000);
        assert!(line["outcome"]["msg"]
            .as_str()
            .unwrap()
            .contains("insufficient funds"));
        assert_eq!(lab.node.times_asked("eth_sendRawTransaction"), 1);
        assert_eq!(line["request"]["path"], "eth_sendRawTransaction");
        assert_eq!(line["host"], "127.0.0.1");
        lab.setup.clean_up();
    }

    /// A node that refuses at the estimate, before anything is signed, has said
    /// no all the same.
    #[tokio::test]
    async fn c1_passes_when_the_node_refuses_at_the_estimate() {
        let node = base_node().error(
            "eth_estimateGas",
            -32000,
            "insufficient funds for transfer",
            None,
        );
        let lab = Lab::new("c1-estimate", node).await;
        let run = lab.setup.run(&lab.server).unwrap();

        base(&run, lab.settings())
            .c1(&lab.provider(), &Throwaway::generate().unwrap())
            .await
            .unwrap();

        assert_eq!(verdict(&lab), "pass", "{}", reason(&lab));
        assert_eq!(lab.node.times_asked("eth_sendRawTransaction"), 0);
        lab.setup.clean_up();
    }

    #[tokio::test]
    async fn c1_fails_when_the_refusal_reads_as_a_revert() {
        let node = base_node().error(
            "eth_sendRawTransaction",
            3,
            "execution reverted",
            Some(revert_data("nope")),
        );
        let lab = Lab::new("c1-revert", node).await;
        let run = lab.setup.run(&lab.server).unwrap();

        base(&run, lab.settings())
            .c1(&lab.provider(), &Throwaway::generate().unwrap())
            .await
            .unwrap();

        assert_eq!(verdict(&lab), "fail");
        assert!(
            reason(&lab).contains("reads as a revert"),
            "{}",
            reason(&lab)
        );
        lab.setup.clean_up();
    }

    /// A node that cannot be connected to has refused nothing: it is not a pass.
    #[tokio::test]
    async fn c1_fails_when_the_sender_could_not_connect() {
        let node = base_node().error("eth_chainId", -32005, "rate limit exceeded", None);
        let lab = Lab::new("c1-connect", node).await;
        let run = lab.setup.run(&lab.server).unwrap();

        base(&run, lab.settings())
            .c1(&lab.provider(), &Throwaway::generate().unwrap())
            .await
            .unwrap();

        assert_eq!(verdict(&lab), "fail");
        assert!(
            reason(&lab).contains("could not connect"),
            "{}",
            reason(&lab)
        );
        assert_eq!(lab.node.times_asked("eth_sendRawTransaction"), 0);
        lab.setup.clean_up();
    }

    // --- C2 ---

    #[tokio::test]
    async fn c2_a_transfer_that_reverts_at_the_estimate_is_not_broadcast() {
        let lab = Lab::new("c2", base_node()).await;
        let run = lab.setup.run(&lab.server).unwrap();

        let flow = base(&run, lab.settings())
            .c2(&lab.provider(), &Throwaway::generate().unwrap())
            .await
            .unwrap();

        assert_eq!(
            (verdict(&lab).as_str(), flow),
            ("pass", Flow::Go),
            "{}",
            reason(&lab)
        );
        assert_eq!(
            lab.node.times_asked("eth_sendRawTransaction"),
            0,
            "nothing broadcast"
        );
        assert_eq!(lab.node.times_asked("eth_estimateGas"), 1);
        // It asked for a transfer of one unit of the token.
        let asked = lab.node.asked();
        let (_, params) = asked.iter().find(|(m, _)| m == "eth_estimateGas").unwrap();
        assert!(params[0]["data"]
            .as_str()
            .unwrap()
            .starts_with("0xa9059cbb"));
        lab.setup.clean_up();
    }

    #[tokio::test]
    async fn c2_fails_when_the_refusal_is_not_a_revert_and_when_the_nonce_moved() {
        // A rate limit is a node failing to run the call, which says nothing of it.
        let limited = base_node().error_when(
            "eth_estimateGas",
            |_| true,
            -32005,
            "rate limit exceeded",
            None,
        );
        let lab = Lab::new("c2-limit", limited).await;
        let run = lab.setup.run(&lab.server).unwrap();
        base(&run, lab.settings())
            .c2(&lab.provider(), &Throwaway::generate().unwrap())
            .await
            .unwrap();
        assert_eq!(verdict(&lab), "fail");
        assert!(reason(&lab).contains("not as a revert"), "{}", reason(&lab));
        lab.setup.clean_up();

        // The nonce is read four times around the send (latest and pending, before
        // and after) and once by the sender; the fifth and later reads find it moved.
        let moved = base_node().sequence(
            "eth_getTransactionCount",
            ["0x0", "0x0", "0x0", "0x1", "0x1"]
                .map(|n| json!(n))
                .to_vec(),
        );
        let lab = Lab::new("c2-nonce", moved).await;
        let run = lab.setup.run(&lab.server).unwrap();
        base(&run, lab.settings())
            .c2(&lab.provider(), &Throwaway::generate().unwrap())
            .await
            .unwrap();
        assert_eq!(verdict(&lab), "fail");
        assert!(reason(&lab).contains("nonce"), "{}", reason(&lab));
        lab.setup.clean_up();
    }

    #[tokio::test]
    async fn c2_is_skipped_without_the_quote_token() {
        let lab = Lab::new("c2-skip", base_node()).await;
        let run = lab.setup.run(&lab.server).unwrap();
        let mut settings = lab.settings();
        settings.token_quote = None;

        base(&run, settings)
            .c2(&lab.provider(), &Throwaway::generate().unwrap())
            .await
            .unwrap();

        assert_eq!(verdict(&lab), "skipped");
        assert!(reason(&lab).contains("VP_TOKEN_QUOTE"));
        assert!(lab.node.asked().is_empty());
        lab.setup.clean_up();
    }

    // --- C3 ---

    #[tokio::test]
    async fn c3_the_wrong_chain_id_is_refused_at_connect_naming_both() {
        let lab = Lab::new("c3", base_node()).await;
        let run = lab.setup.run(&lab.server).unwrap();

        base(&run, lab.settings())
            .c3(&lab.provider(), &Throwaway::generate().unwrap())
            .await
            .unwrap();

        assert_eq!(verdict(&lab), "pass", "{}", reason(&lab));
        let line = last(&lab);
        let said = line["outcome"]["msg"].as_str().unwrap();
        assert!(
            said.contains("8453") && said.contains("expected chain 1"),
            "{said}"
        );
        lab.setup.clean_up();
    }

    /// A node that is on chain 1 accepts the connect: it is not a Base node, and
    /// the case says so.
    #[tokio::test]
    async fn c3_fails_when_the_node_is_on_the_chain_it_was_connected_as() {
        let lab = Lab::new(
            "c3-mainnet",
            base_node().result("eth_chainId", json!("0x1")),
        )
        .await;
        let run = lab.setup.run(&lab.server).unwrap();

        base(&run, lab.settings())
            .c3(&lab.provider(), &Throwaway::generate().unwrap())
            .await
            .unwrap();

        assert_eq!(verdict(&lab), "fail");
        lab.setup.clean_up();
    }

    // --- C4 ---

    #[tokio::test]
    async fn c4_four_requests_one_at_a_time_each_recorded_and_none_judged() {
        let record = Setup::new("probe", &MockServer::start().await)
            .record
            .display()
            .to_string();
        let lab = Lab::new("c4", base_node())
            .await
            .with("VP_RECORD_DIR", &record);
        let run = lab.setup.run(&lab.server).unwrap();

        let flow = base(&run, lab.settings())
            .c4(&lab.provider(), &Throwaway::generate().unwrap())
            .await
            .unwrap();

        assert_eq!(flow, Flow::Go);
        let lines = lab.lines();
        assert_eq!(lines.len(), 5, "the latest block is a step of its own");
        assert!(lines.iter().all(|line| line["verdict"] == "pass"));
        let methods: Vec<String> = lab
            .node
            .asked()
            .into_iter()
            .map(|(method, _)| method)
            .collect();
        assert_eq!(
            methods,
            [
                "eth_getBlockByNumber",
                "eth_call",
                "eth_maxPriorityFeePerGas",
                "eth_blockNumber",
                "eth_getLogs"
            ],
            "one at a time, in order"
        );
        let asked = lab.node.asked();
        assert_eq!(asked[0].1, json!(["pending", false]));
        assert_eq!(asked[1].1[1], "pending");
        // The log range is over any provider's limit and filtered so that an
        // answer is nothing.
        let filter = &asked[4].1[0];
        assert_eq!(
            filter["fromBlock"], "0x0",
            "100 blocks of history on a mock chain"
        );
        assert_eq!(filter["topics"][0], TRANSFER_TOPIC);
        assert_eq!(filter["address"], TOKEN.to_string());
        // Each reply is saved as the provider said it.
        for name in [
            "34-pending-block",
            "35-eth-call-pending",
            "36-max-priority-fee",
            "37-latest-block",
            "37-log-range",
        ] {
            let saved = std::fs::read_to_string(format!("{record}/p0-{name}.json")).unwrap();
            assert!(saved.starts_with("{\"result\":"), "{name}: {saved}");
        }
        lab.setup.clean_up();
        let _ = std::fs::remove_dir_all(std::path::Path::new(&record).parent().unwrap());
    }

    /// A provider that refuses the log range, or has no pending block, has
    /// answered; its words are the record.
    #[tokio::test]
    async fn c4_records_a_providers_refusals_and_passes_them() {
        let record = Setup::new("probe", &MockServer::start().await)
            .record
            .display()
            .to_string();
        let node = base_node()
            .error(
                "eth_getBlockByNumber",
                -32601,
                "the method does not exist",
                None,
            )
            .error(
                "eth_getLogs",
                -32005,
                "query exceeds max block range 10000",
                None,
            );
        let lab = Lab::new("c4-refused", node)
            .await
            .with("VP_RECORD_DIR", &record);
        let run = lab.setup.run(&lab.server).unwrap();

        base(&run, lab.settings())
            .c4(&lab.provider(), &Throwaway::generate().unwrap())
            .await
            .unwrap();

        let lines = lab.lines();
        assert!(
            lines.iter().all(|line| line["verdict"] == "pass"),
            "{lines:#?}"
        );
        assert_eq!(lines[0]["outcome"]["kind"], "refused");
        assert_eq!(lines[4]["outcome"]["code"], -32005);
        let saved = std::fs::read_to_string(format!("{record}/p0-37-log-range.json")).unwrap();
        assert_eq!(
            saved,
            r#"{"error":{"code":-32005,"message":"query exceeds max block range 10000"}}"#
        );
        lab.setup.clean_up();
        let _ = std::fs::remove_dir_all(std::path::Path::new(&record).parent().unwrap());
    }

    /// A provider that cannot be reached has answered nothing: not a pass.
    #[tokio::test]
    async fn c4_is_unmapped_when_the_provider_does_not_answer() {
        let lab = Lab::new("c4-down", base_node()).await;
        let run = lab.setup.run(&lab.server).unwrap();
        let closed = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}/v2/{KEY}", listener.local_addr().unwrap())
        };
        let node = Node {
            index: 0,
            host: "closed".to_string(),
            rpc: EvmRpc::with_request_timeout(closed.clone(), Duration::from_millis(500)),
        };
        let mut settings = lab.settings();
        settings.rpc_urls = vec![closed];

        base(&run, settings)
            .c4(&node, &Throwaway::generate().unwrap())
            .await
            .unwrap();

        let lines = lab.lines();
        assert_eq!(lines[0]["verdict"], "unmapped");
        assert_eq!(
            last(&lab)["verdict"],
            "skipped",
            "no latest block, so no log range"
        );
        // The error names the URL, and the line does not.
        assert!(!lab.text().contains(KEY), "{}", lab.text());
        assert!(lab.text().contains("[provider]"));
        lab.setup.clean_up();
    }

    // --- C5 ---

    #[tokio::test]
    async fn c5_a_closed_port_and_a_node_that_sleeps_are_errors_and_not_a_nodes_answer() {
        let lab = Lab::new("c5", base_node()).await;
        let run = lab.setup.run(&lab.server).unwrap();
        let mut case = base(&run, lab.settings());
        case.request_timeout = Duration::from_millis(400);

        let flow = case.c5().await.unwrap();

        assert_eq!(flow, Flow::Go);
        let lines = lab.lines();
        assert_eq!(lines.len(), 2);
        assert!(
            lines.iter().all(|line| line["verdict"] == "pass"),
            "{lines:#?}"
        );
        assert!(lines
            .iter()
            .all(|line| line["outcome"]["kind"] == "not_sent"));
        assert!(lab.node.asked().is_empty(), "C5 asks no provider");
        lab.setup.clean_up();
    }

    /// A node that answers is not a dead one.
    #[tokio::test]
    async fn c5_fails_against_a_node_that_answers() {
        let lab = Lab::new("c5-alive", base_node()).await;
        let run = lab.setup.run(&lab.server).unwrap();

        base(&run, lab.settings())
            .c5_against("a live node", &lab.server.uri())
            .await
            .unwrap();

        assert_eq!(verdict(&lab), "fail");
        assert!(reason(&lab).contains("went through"), "{}", reason(&lab));
        lab.setup.clean_up();
    }

    // --- C6 ---

    #[tokio::test]
    async fn c6_the_pools_views_decode_and_the_code_is_hashed() {
        let lab = Lab::new("c6", base_node()).await;
        let run = lab.setup.run(&lab.server).unwrap();

        let flow = base(&run, lab.settings())
            .c6(&lab.provider())
            .await
            .unwrap();

        assert_eq!(flow, Flow::Go);
        let lines = lab.lines();
        assert_eq!(lines.len(), 5, "factory, stable, reserves, fee, code");
        assert!(
            lines.iter().all(|line| line["verdict"] == "pass"),
            "{lines:#?}"
        );
        // The fee is asked of the factory the pool named, for the pool and its flag.
        let asked = lab.node.asked();
        let fee = asked
            .iter()
            .find(|(m, p)| {
                m == "eth_call" && p[0]["data"].as_str().unwrap().starts_with("0xcc56b2c5")
            })
            .unwrap();
        assert!(fee.1[0]["to"]
            .as_str()
            .unwrap()
            .eq_ignore_ascii_case(&FACTORY.to_string()));
        assert!(fee.1[0]["data"].as_str().unwrap().ends_with("01"), "stable");
        // The code is recorded by its hash, not its bytes.
        let references = &lines[4]["references"];
        assert_eq!(
            references["code_hash"],
            keccak256(hex::decode("608060405234801561001057600080fd5b50").unwrap()).to_string()
        );
        lab.setup.clean_up();
    }

    #[tokio::test]
    async fn c6_fails_when_a_reply_does_not_decode_or_the_pool_has_no_code() {
        let broken = base_node().eth_call("0x22be3de1", json!("0x"));
        let lab = Lab::new("c6-decode", broken).await;
        let run = lab.setup.run(&lab.server).unwrap();
        base(&run, lab.settings())
            .c6(&lab.provider())
            .await
            .unwrap();
        let lines = lab.lines();
        assert_eq!(lines[1]["verdict"], "fail", "stable() answered nothing");
        assert!(lines[1]["reason"]
            .as_str()
            .unwrap()
            .contains("expected an answer"));
        // Without the flag the factory's fee is not asked, and says why.
        assert_eq!(lines[2]["verdict"], "pass");
        assert_eq!(lines[3]["verdict"], "skipped");
        lab.setup.clean_up();

        let empty = base_node().result("eth_getCode", json!("0x"));
        let lab = Lab::new("c6-nocode", empty).await;
        let run = lab.setup.run(&lab.server).unwrap();
        base(&run, lab.settings())
            .c6(&lab.provider())
            .await
            .unwrap();
        assert_eq!(verdict(&lab), "fail");
        assert!(reason(&lab).contains("no code"), "{}", reason(&lab));
        lab.setup.clean_up();
    }

    #[tokio::test]
    async fn c6_is_skipped_without_a_pool() {
        let lab = Lab::new("c6-skip", base_node()).await;
        let run = lab.setup.run(&lab.server).unwrap();
        let mut settings = lab.settings();
        settings.pool = None;

        base(&run, settings).c6(&lab.provider()).await.unwrap();

        assert_eq!(verdict(&lab), "skipped");
        assert!(lab.node.asked().is_empty());
        lab.setup.clean_up();
    }

    // --- the exit ---

    #[tokio::test]
    async fn the_exit_passes_when_the_throwaway_address_holds_what_it_held() {
        let lab = Lab::new("exit", base_node()).await;
        let run = lab.setup.run(&lab.server).unwrap();
        let case = base(&run, lab.settings());
        let throwaway = Throwaway::generate().unwrap();

        let (_, before) = case
            .balances(&lab.provider(), &throwaway, "before", None)
            .await
            .unwrap();
        let (flow, _) = case
            .balances(&lab.provider(), &throwaway, "after", before)
            .await
            .unwrap();

        assert_eq!((verdict(&lab).as_str(), flow), ("pass", Flow::Go));
        assert_eq!(before, Some((0, 0)));
        lab.setup.clean_up();
    }

    #[tokio::test]
    async fn the_exit_fails_when_the_address_gained_a_single_unit() {
        let node = base_node().sequence("eth_getBalance", vec![json!("0x0"), json!("0x1")]);
        let lab = Lab::new("exit-gain", node).await;
        let run = lab.setup.run(&lab.server).unwrap();
        let case = base(&run, lab.settings());
        let throwaway = Throwaway::generate().unwrap();

        let (_, before) = case
            .balances(&lab.provider(), &throwaway, "before", None)
            .await
            .unwrap();
        let (flow, _) = case
            .balances(&lab.provider(), &throwaway, "after", before)
            .await
            .unwrap();

        assert_eq!((verdict(&lab).as_str(), flow), ("fail", Flow::Go));
        assert!(
            reason(&lab).contains("(0, 0) became (1, 0)"),
            "{}",
            reason(&lab)
        );
        lab.setup.clean_up();
    }

    // --- the whole of it ---

    /// Two providers, each given the cases; nothing fails; no provider key, and
    /// no throwaway key, is in the lines.
    #[tokio::test]
    async fn the_whole_of_lv1_base_is_clean_for_every_provider() {
        let first = Lab::new("whole", base_node()).await;
        let second_server = base_node().start().await;
        let run = first.setup.run(&first.server).unwrap();
        let mut settings = first.settings();
        settings
            .rpc_urls
            .push(format!("{}/v2/{KEY}-second", second_server.uri()));
        let mut case = base(&run, settings);
        case.request_timeout = Duration::from_millis(400);

        let tally = case.run_all().await.unwrap();

        assert_eq!(
            tally.fail + tally.unmapped + tally.halted,
            0,
            "{}",
            first.text()
        );
        assert!(run.finish().is_ok());
        let lines = first.lines();
        let mut per_case: Vec<(String, String)> = lines
            .iter()
            .map(|line| {
                (
                    line["case"].as_str().unwrap().to_string(),
                    line["host"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        per_case.dedup();
        for case in ["GUARD", "EXIT", "C1", "C2", "C3", "C4", "C6"] {
            assert_eq!(
                lines.iter().filter(|line| line["case"] == case).count() % 2,
                0,
                "{case} ran for each of the two providers"
            );
        }
        assert_eq!(
            lines.iter().filter(|line| line["case"] == "C5").count(),
            2,
            "C5 once"
        );
        let text = first.text();
        assert!(!text.contains(KEY), "a provider's key is in a line");
        assert!(!text.contains("v2/"), "a provider's path is in a line");
        first.setup.clean_up();
    }

    #[tokio::test]
    async fn a_node_on_this_machine_is_not_a_production_node() {
        let lab = Lab::new("local", base_node()).await;
        let run = lab.setup.run(&lab.server).unwrap();
        // Not `against_mocks`: the guard is on.
        let case = Lv1Base::new(&run, lab.settings());

        let err = case.run_all().await.unwrap_err().to_string();

        assert!(err.contains("not on this machine"), "{err}");
        assert!(lab.node.asked().is_empty(), "nothing was asked of it");
        lab.setup.clean_up();
    }

    /// A dry run asks no provider anything, and every line is skipped.
    #[tokio::test]
    async fn a_dry_run_asks_no_provider_anything() {
        let lab = Lab::new("dry", base_node()).await.with("VP_DRY_RUN", "1");
        let run = lab.setup.run(&lab.server).unwrap();
        let mut case = base(&run, lab.settings());
        case.request_timeout = Duration::from_millis(400);

        case.run_all().await.unwrap();

        assert!(lab.node.asked().is_empty(), "{:?}", lab.node.asked());
        let lines = lab.lines();
        let c5: Vec<_> = lines.iter().filter(|line| line["case"] == "C5").collect();
        assert!(
            lines.iter().all(|line| line["verdict"] == "skipped"),
            "{lines:#?}"
        );
        assert_eq!(c5.len(), 2, "C5 is held back too: it is a request");
        let printed = run.gate().printed();
        assert!(printed
            .iter()
            .any(|line| line.starts_with("DRY RUN RPC eth_sendRawTransaction")));
        assert!(printed
            .iter()
            .any(|line| line.starts_with("DRY RUN RPC eth_estimateGas")));
        assert!(run.finish().is_ok());
        lab.setup.clean_up();
    }

    #[test]
    fn the_settings_read_the_providers_the_token_and_the_pool() {
        let env = MapEnv::new(&[
            (
                "VP_BASE_RPC_URLS",
                "https://a.example/k1, https://b.example/k2 ,",
            ),
            ("VP_TOKEN_QUOTE", &TOKEN.to_string()),
        ]);
        let settings = Settings::from_env(&env).unwrap();
        assert_eq!(
            settings.rpc_urls,
            ["https://a.example/k1", "https://b.example/k2"]
        );
        assert_eq!((settings.token_quote, settings.pool), (Some(TOKEN), None));

        let single = MapEnv::new(&[("VP_BASE_RPC_URL", "https://c.example/k3")]);
        assert_eq!(
            Settings::from_env(&single).unwrap().rpc_urls,
            ["https://c.example/k3"]
        );

        assert!(Settings::from_env(&MapEnv::new(&[])).is_err());
        let not_an_address = MapEnv::new(&[
            ("VP_BASE_RPC_URL", "https://c.example"),
            ("VP_POOL", "0x12"),
        ]);
        let err = format!("{:#}", Settings::from_env(&not_an_address).unwrap_err());
        assert!(
            err.contains("VP_POOL") && err.contains("not an address"),
            "{err}"
        );
    }

    #[test]
    fn a_provider_is_named_by_its_host_alone() {
        assert_eq!(
            host_of("https://base-mainnet.example.com/v2/KEY?x=1"),
            "base-mainnet.example.com"
        );
        assert_eq!(host_of("not a url"), "unparsable-url");
        let _ = B256::ZERO;
    }

    // --- the test a person runs ---

    /// LV1 against Base, once for each provider:
    ///
    /// ```text
    /// VP_DRY_RUN=1 cargo test --lib production_lv1_base -- --ignored --nocapture
    /// cargo test --lib production_lv1_base -- --ignored --nocapture
    /// ```
    ///
    /// Needs `VP_BASE_RPC_URLS` (comma-separated), `VP_SPEND_CAP_USD`,
    /// `VP_HALT_FILE` and `VP_OUT`; `VP_TOKEN_QUOTE` and `VP_POOL` for the cases
    /// that use them. The key is generated for the run and holds nothing.
    #[tokio::test]
    #[ignore = "runs against Base mainnet nodes: see specs/V7-production-validation.md"]
    async fn production_lv1_base() {
        let env = ProcessEnv;
        let run =
            Run::start(&env, "base", "base-mainnet", Echo::Stdout).expect("the run's settings");
        let settings = Settings::from_env(&env).expect("the case settings");
        Lv1Base::new(&run, settings)
            .run_all()
            .await
            .expect("the run");
        run.finish().expect("LV1 on Base is clean");
    }
}
