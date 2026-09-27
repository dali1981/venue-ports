//! `EvmSimulated` — runs a prepared call against live chain state via
//! `eth_call`'s state-override parameter, with no transaction ever
//! broadcast (`SPEC.md` §5, second required implementation).
//!
//! This needs no forked node: `eth_call`'s optional third parameter lets a
//! caller override specific storage slots for the duration of one call,
//! against whatever block the node already has (default: latest). Every
//! major EVM client (Geth, Erigon, Reth) and hosted RPC provider supports
//! it, including free/keyless public endpoints, so this adapter needs
//! nothing beyond an RPC URL.
//!
//! Where the simulated sender does not actually hold the input token or
//! the router's allowance, this adapter overrides just enough storage to
//! make the call possible: the sender's `balanceOf` slot, and the
//! `allowance` slot for the sender *and the router the route actually
//! calls* (never the address probing it — SPEC.md §5's explicit pitfall).
//! Neither ERC-20 storage layout is standardized, so both slots are probed
//! for per token the first time they're needed, then cached.
//!
//! **Adapter convention for `RouteQuote.payload`:** SPEC.md leaves the
//! payload's shape entirely up to the adapter that consumes it (§5). This
//! adapter expects it to be `router_address (20 bytes) ++ calldata` — the
//! simplest encoding that carries what a generic (not router-specific)
//! implementation needs. A concrete router integration is free to use a
//! different convention as long as its own adapter agrees with whatever
//! produces its `RouteQuote`s.
//!
//! **Return-value assumption:** the router call's return data is decoded
//! as a single big-endian `uint256` (`amount_out`) — the common shape for
//! a raw swap call. A router that returns something richer (e.g. an
//! array) needs its own decoding here; this is a placeholder until a real
//! router is wired in.

use crate::dex::{ChainAmount, DexExecutor, Outcome, Prepared, Realised, RouteQuote, SwapRequest};
use crate::Provenance;
use alloy_primitives::{keccak256, Address, B256, U256};
use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;

/// How many candidate storage-slot indices to probe before giving up.
/// Real contracts almost always place `balanceOf`/`allowance` in one of
/// the first few storage slots; this is generous headroom, not a limit
/// callers need to think about.
const MAX_PROBE_SLOTS: u64 = 16;

/// `keccak256("balanceOf(address)")[..4]` — a fixed, public ERC-20 ABI
/// selector, not computed at runtime.
const BALANCE_OF_SELECTOR: [u8; 4] = [0x70, 0xa0, 0x82, 0x31];
/// `keccak256("allowance(address,address)")[..4]`.
const ALLOWANCE_SELECTOR: [u8; 4] = [0xdd, 0x62, 0xed, 0x3e];
/// `keccak256("Error(string)")[..4]` — the standard Solidity
/// `require`/`revert("reason")` encoding.
const ERROR_STRING_SELECTOR: [u8; 4] = [0x08, 0xc3, 0x79, 0xa0];

/// A JSON-RPC error object, carrying an already-decoded revert reason.
/// Distinguished from a transport-level failure (a bad URL, a connection
/// drop) so `execute()` can turn only this into `Outcome::Reverted`.
#[derive(Debug)]
struct RpcRevert(String);

impl std::fmt::Display for RpcRevert {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for RpcRevert {}

/// Everything `execute()` needs that isn't in `Prepared`, stashed by
/// `prepare()` and consumed exactly once.
struct PendingSimulation {
    token_in: Address,
    sender: Address,
    amount_in: ChainAmount,
}

pub struct EvmSimulated {
    rpc_url: String,
    http: reqwest::Client,
    pending: Mutex<HashMap<B256, PendingSimulation>>,
    balance_slot_cache: Mutex<HashMap<Address, u64>>,
    allowance_slot_cache: Mutex<HashMap<(Address, Address), u64>>,
}

impl EvmSimulated {
    pub fn new(rpc_url: impl Into<String>) -> Self {
        Self {
            rpc_url: rpc_url.into(),
            http: reqwest::Client::new(),
            pending: Mutex::new(HashMap::new()),
            balance_slot_cache: Mutex::new(HashMap::new()),
            allowance_slot_cache: Mutex::new(HashMap::new()),
        }
    }

    async fn rpc_call(&self, method: &str, params: Value) -> Result<Value> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params,
        });
        let response: Value = self
            .http
            .post(&self.rpc_url)
            .json(&body)
            .send()
            .await
            .context("sending JSON-RPC request")?
            .json()
            .await
            .context("decoding JSON-RPC response body")?;

        if let Some(error) = response.get("error") {
            return Err(RpcRevert(decode_revert_reason(error)).into());
        }
        response.get("result").cloned().ok_or_else(|| {
            anyhow!("JSON-RPC response had neither `result` nor `error`: {response}")
        })
    }

    /// `from` matters whenever the call itself depends on `msg.sender` —
    /// a real router's swap reads it to decide whose balance/allowance to
    /// pull from (`TransferHelper`'s `"STF"` revert is exactly what a real
    /// Sepolia run of this adapter hit before `from` was wired through:
    /// omitting it left the call's `msg.sender` at the zero address, so
    /// the state overrides below — written against the *route's* sender —
    /// were never the address the router actually checked). It's
    /// irrelevant for a pure view call like `balanceOf`/`allowance`, so
    /// the slot-probing call sites pass `None`.
    async fn eth_call(
        &self,
        to: Address,
        data: &[u8],
        block: u64,
        overrides: Value,
        from: Option<Address>,
    ) -> Result<Vec<u8>> {
        let mut call = json!({ "to": to.to_string(), "data": format!("0x{}", hex::encode(data)) });
        if let Some(from) = from {
            call["from"] = json!(from.to_string());
        }
        let result = self
            .rpc_call("eth_call", json!([call, format!("0x{block:x}"), overrides]))
            .await?;
        let hex_str = result
            .as_str()
            .ok_or_else(|| anyhow!("eth_call result was not a hex string: {result}"))?;
        decode_hex(hex_str)
    }

    async fn latest_block_number(&self) -> Result<u64> {
        let result = self.rpc_call("eth_blockNumber", json!([])).await?;
        let hex_str = result
            .as_str()
            .ok_or_else(|| anyhow!("eth_blockNumber result was not a hex string: {result}"))?;
        u64::from_str_radix(hex_str.trim_start_matches("0x"), 16)
            .with_context(|| format!("could not parse block number {hex_str}"))
    }

    /// The storage slot for `sender`'s balance in `token`'s `balanceOf`
    /// mapping, probed once per token and cached thereafter.
    async fn find_balance_slot(&self, token: Address, sender: Address, block: u64) -> Result<u64> {
        if let Some(&index) = self.balance_slot_cache.lock().unwrap().get(&token) {
            return Ok(index);
        }
        let marker = probe_marker();
        let mut calldata = Vec::with_capacity(36);
        calldata.extend_from_slice(&BALANCE_OF_SELECTOR);
        calldata.extend_from_slice(&pad_address(sender));

        for index in 0..MAX_PROBE_SLOTS {
            let slot = mapping_slot(sender, index);
            let overrides = json!({
                token.to_string(): { "stateDiff": { slot.to_string(): format_u256(marker) } }
            });
            let data = self
                .eth_call(token, &calldata, block, overrides, None)
                .await?;
            if U256::from_be_slice(&data) == marker {
                self.balance_slot_cache.lock().unwrap().insert(token, index);
                return Ok(index);
            }
        }
        bail!("could not find `balanceOf` storage slot for token {token} after probing {MAX_PROBE_SLOTS} candidates");
    }

    /// The storage slot for `allowance[sender][spender]` in `token`,
    /// probed once per (token, spender) pair and cached thereafter.
    async fn find_allowance_slot(
        &self,
        token: Address,
        sender: Address,
        spender: Address,
        block: u64,
    ) -> Result<u64> {
        if let Some(&index) = self
            .allowance_slot_cache
            .lock()
            .unwrap()
            .get(&(token, spender))
        {
            return Ok(index);
        }
        let marker = probe_marker();
        let mut calldata = Vec::with_capacity(68);
        calldata.extend_from_slice(&ALLOWANCE_SELECTOR);
        calldata.extend_from_slice(&pad_address(sender));
        calldata.extend_from_slice(&pad_address(spender));

        for index in 0..MAX_PROBE_SLOTS {
            let slot = allowance_slot(sender, spender, index);
            let overrides = json!({
                token.to_string(): { "stateDiff": { slot.to_string(): format_u256(marker) } }
            });
            let data = self
                .eth_call(token, &calldata, block, overrides, None)
                .await?;
            if U256::from_be_slice(&data) == marker {
                self.allowance_slot_cache
                    .lock()
                    .unwrap()
                    .insert((token, spender), index);
                return Ok(index);
            }
        }
        bail!(
            "could not find `allowance` storage slot for token {token}, spender {spender} \
             after probing {MAX_PROBE_SLOTS} candidates"
        );
    }
}

#[async_trait]
impl DexExecutor for EvmSimulated {
    async fn prepare(&self, route: &RouteQuote, req: &SwapRequest) -> Result<Prepared> {
        if route.payload.len() < 20 {
            bail!(
                "route.payload must be at least 20 bytes (router address + calldata), got {}",
                route.payload.len()
            );
        }
        let (router_bytes, calldata) = route.payload.split_at(20);
        let token_in = address_from_slice(&route.token_in).context("route.token_in")?;
        let sender = address_from_slice(&req.sender).context("req.sender")?;

        let prepared = Prepared {
            to: router_bytes.to_vec(),
            calldata: calldata.to_vec(),
            value: 0,
        };

        self.pending.lock().unwrap().insert(
            pending_key(&prepared),
            PendingSimulation {
                token_in,
                sender,
                amount_in: route.amount_in,
            },
        );
        Ok(prepared)
    }

    async fn execute(&self, prepared: &Prepared, at: Option<u64>) -> Result<Realised> {
        let ctx = self
            .pending
            .lock()
            .unwrap()
            .remove(&pending_key(prepared))
            .ok_or_else(|| {
                anyhow!(
                    "execute() called with a Prepared value this EvmSimulated instance did not \
                     produce, or already consumed"
                )
            })?;
        let router = address_from_slice(&prepared.to).context("prepared.to")?;

        let block = match at {
            Some(block) => block,
            None => self.latest_block_number().await?,
        };

        let balance_index = self
            .find_balance_slot(ctx.token_in, ctx.sender, block)
            .await?;
        let allowance_index = self
            .find_allowance_slot(ctx.token_in, ctx.sender, router, block)
            .await?;
        let balance_slot = mapping_slot(ctx.sender, balance_index);
        let allowance_slot_key = allowance_slot(ctx.sender, router, allowance_index);
        let amount_in = format_u256(U256::from(ctx.amount_in));

        let overrides = json!({
            ctx.token_in.to_string(): {
                "stateDiff": {
                    balance_slot.to_string(): amount_in,
                    allowance_slot_key.to_string(): amount_in,
                }
            }
        });

        match self
            .eth_call(
                router,
                &prepared.calldata,
                block,
                overrides,
                Some(ctx.sender),
            )
            .await
        {
            Ok(data) => {
                let amount_out: u128 = U256::from_be_slice(&data)
                    .try_into()
                    .context("amount_out overflowed u128")?;
                Ok(Realised {
                    amount_out: Some(amount_out),
                    outcome: Outcome::Success,
                    at: block,
                    provenance: Provenance::Simulated,
                    tx_ref: None,
                })
            }
            Err(err) => match err.downcast::<RpcRevert>() {
                Ok(revert) => Ok(Realised {
                    amount_out: None,
                    outcome: Outcome::Reverted { reason: revert.0 },
                    at: block,
                    provenance: Provenance::Simulated,
                    tx_ref: None,
                }),
                Err(transport_err) => Err(transport_err),
            },
        }
    }

    fn label(&self) -> &'static str {
        "evm-simulated"
    }
}

fn address_from_slice(bytes: &[u8]) -> Result<Address> {
    let array: [u8; 20] = bytes
        .try_into()
        .map_err(|_| anyhow!("expected a 20-byte EVM address, got {} bytes", bytes.len()))?;
    Ok(Address::from(array))
}

fn pad_address(address: Address) -> [u8; 32] {
    let mut buf = [0u8; 32];
    buf[12..32].copy_from_slice(address.as_slice());
    buf
}

/// The storage slot for `mapping(address => T)[key]` at slot index
/// `index`, per Solidity's standard storage layout: `keccak256(pad(key) ++
/// pad(index))`.
fn mapping_slot(key: Address, index: u64) -> B256 {
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(&pad_address(key));
    buf[32..].copy_from_slice(&U256::from(index).to_be_bytes::<32>());
    keccak256(buf)
}

/// The storage slot for `mapping(address => mapping(address => T))[outer][inner]`
/// at slot index `index`: `keccak256(pad(inner) ++ keccak256(pad(outer) ++ pad(index)))`.
fn allowance_slot(outer: Address, inner: Address, index: u64) -> B256 {
    let outer_slot = mapping_slot(outer, index);
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(&pad_address(inner));
    buf[32..].copy_from_slice(outer_slot.as_slice());
    keccak256(buf)
}

/// Formats a `U256` as 32 bytes of big-endian hex `DATA` — the shape a
/// node expects for a `stateDiff` storage value and returns for a
/// `uint256` return value. Unlike a JSON-RPC `QUANTITY` (which drops
/// leading zeros, so zero is legally `"0x0"`), `DATA` here must stay a
/// fixed 32 bytes or a node can reject or misinterpret it.
fn format_u256(value: U256) -> String {
    format!("0x{}", hex::encode(value.to_be_bytes::<32>()))
}

/// A distinctive value vanishingly unlikely to equal any real balance or
/// allowance, used to detect which candidate storage slot an override
/// actually landed on.
fn probe_marker() -> U256 {
    U256::from(0xdead_beef_dead_beef_u64)
}

fn decode_hex(s: &str) -> Result<Vec<u8>> {
    hex::decode(s.trim_start_matches("0x")).context("decoding hex string")
}

/// A key correlating a `Prepared` value with the context `prepare()`
/// stashed for it, so `execute()` can recover what it needs without the
/// trait signature carrying it directly.
fn pending_key(prepared: &Prepared) -> B256 {
    let mut buf = Vec::with_capacity(prepared.to.len() + prepared.calldata.len() + 16);
    buf.extend_from_slice(&prepared.to);
    buf.extend_from_slice(&prepared.calldata);
    buf.extend_from_slice(&prepared.value.to_be_bytes());
    keccak256(buf)
}

/// Best-effort decode of a Solidity revert reason from a JSON-RPC error's
/// `data` field: the standard `Error(string)` `require`/`revert` encoding
/// when present, the node's own error message otherwise.
fn decode_revert_reason(error: &Value) -> String {
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("execution reverted");
    let data = error
        .get("data")
        .and_then(Value::as_str)
        .or_else(|| error.pointer("/data/data").and_then(Value::as_str));

    if let Some(data) = data {
        if let Ok(bytes) = decode_hex(data) {
            if bytes.len() >= 4 && bytes[..4] == ERROR_STRING_SELECTOR {
                if let Some(reason) = decode_abi_string(&bytes[4..]) {
                    return reason;
                }
            }
        }
    }
    message.to_string()
}

/// Decodes a lone ABI-encoded `string` (the standard `Error(string)`
/// payload shape): a 32-byte offset, a 32-byte length, then the UTF-8
/// bytes.
fn decode_abi_string(bytes: &[u8]) -> Option<String> {
    if bytes.len() < 64 {
        return None;
    }
    let len: usize = U256::from_be_slice(&bytes[32..64]).try_into().ok()?;
    let start: usize = 64;
    let end = start.checked_add(len)?;
    bytes
        .get(start..end)
        .map(|s| String::from_utf8_lossy(s).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value as Json;
    use wiremock::matchers::{body_partial_json, body_string_contains, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn route(payload: Vec<u8>) -> RouteQuote {
        RouteQuote {
            chain_id: 1,
            token_in: vec![0xAA; 20],
            token_out: vec![0xBB; 20],
            amount_in: 1_000,
            expected_amount_out: 900,
            payload,
        }
    }

    fn request() -> SwapRequest {
        SwapRequest {
            sender: vec![0xCC; 20],
            recipient: vec![0xDD; 20],
            min_amount_out: 900,
            deadline_unix_secs: 0,
        }
    }

    fn payload_for(router: [u8; 20], calldata: &[u8]) -> Vec<u8> {
        let mut payload = router.to_vec();
        payload.extend_from_slice(calldata);
        payload
    }

    /// Mounts a mock that answers `eth_call` requests whose calldata
    /// starts with `selector` — scoped to that selector in the matcher
    /// itself, not just the response logic, so probes for two different
    /// mappings mounted on the same server never shadow each other. It
    /// returns the marker value the moment the request's overrides touch
    /// `expected_slot`, and zero otherwise — standing in for "this is the
    /// one real storage slot that actually backs the mapping".
    async fn mount_slot_probe(server: &MockServer, selector: [u8; 4], expected_slot: B256) {
        Mock::given(method("POST"))
            .and(body_partial_json(json!({ "method": "eth_call" })))
            .and(body_string_contains(hex::encode(selector)))
            .respond_with(move |req: &wiremock::Request| {
                let body: Json = req.body_json().unwrap();
                let params = body["params"].as_array().unwrap();
                let overrides = &params[2];
                let touches_expected = overrides
                    .as_object()
                    .and_then(|tokens| tokens.values().next())
                    .and_then(|token_overrides| token_overrides.get("stateDiff"))
                    .and_then(|diff| diff.as_object())
                    .map(|diff| diff.contains_key(&expected_slot.to_string()))
                    .unwrap_or(false);
                let value = if touches_expected {
                    format_u256(probe_marker())
                } else {
                    format_u256(U256::ZERO)
                };
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "jsonrpc": "2.0", "id": 1, "result": value }))
            })
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn finds_balance_slot_at_index_zero_and_caches_it() {
        let server = MockServer::start().await;
        let token = Address::from([0xAA; 20]);
        let sender = Address::from([0xCC; 20]);
        mount_slot_probe(&server, BALANCE_OF_SELECTOR, mapping_slot(sender, 0)).await;

        let adapter = EvmSimulated::new(server.uri());
        let index = adapter
            .find_balance_slot(token, sender, 1)
            .await
            .expect("slot found");
        assert_eq!(index, 0);

        // Cached: a second call must not need another probe request. If it
        // did, wiremock would still answer (the mount has no upper bound),
        // so assert the cache directly instead of relying on request counts.
        assert_eq!(
            adapter.balance_slot_cache.lock().unwrap().get(&token),
            Some(&0)
        );
    }

    #[tokio::test]
    async fn errors_when_no_candidate_slot_matches() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0", "id": 1, "result": format_u256(U256::ZERO),
            })))
            .mount(&server)
            .await;

        let adapter = EvmSimulated::new(server.uri());
        let err = adapter
            .find_balance_slot(Address::from([0xAA; 20]), Address::from([0xCC; 20]), 1)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("could not find"));
    }

    #[tokio::test]
    async fn execute_overrides_both_slots_and_decodes_amount_out() {
        let server = MockServer::start().await;
        let router = [0x11; 20];
        let calldata = vec![0x01, 0x02, 0x03, 0x04];
        let token = Address::from([0xAA; 20]);
        let sender = Address::from([0xCC; 20]);
        let router_addr = Address::from(router);

        mount_slot_probe(&server, BALANCE_OF_SELECTOR, mapping_slot(sender, 0)).await;
        mount_slot_probe(
            &server,
            ALLOWANCE_SELECTOR,
            allowance_slot(sender, router_addr, 0),
        )
        .await;

        // The final swap call itself: anything whose calldata is exactly
        // `calldata` (not a balanceOf/allowance probe) returns amount_out.
        let expected_calldata = calldata.clone();
        Mock::given(method("POST"))
            .and(body_partial_json(json!({ "method": "eth_call" })))
            .respond_with(move |req: &wiremock::Request| {
                let body: Json = req.body_json().unwrap();
                let params = body["params"].as_array().unwrap();
                let data = params[0]["data"].as_str().unwrap();
                let data_bytes = decode_hex(data).unwrap();
                if data_bytes == expected_calldata {
                    let amount_out = format_u256(U256::from(777u64));
                    ResponseTemplate::new(200)
                        .set_body_json(json!({ "jsonrpc": "2.0", "id": 1, "result": amount_out }))
                } else {
                    ResponseTemplate::new(200).set_body_json(json!({
                        "jsonrpc": "2.0", "id": 1, "result": format_u256(U256::ZERO),
                    }))
                }
            })
            .mount(&server)
            .await;

        let adapter = EvmSimulated::new(server.uri());
        let route = route(payload_for(router, &calldata));
        let request = request();

        let prepared = adapter.prepare(&route, &request).await.unwrap();
        assert_eq!(prepared.to, router.to_vec());
        assert_eq!(prepared.calldata, calldata);

        let realised = adapter.execute(&prepared, Some(42)).await.unwrap();
        assert_eq!(realised.amount_out, Some(777));
        assert!(matches!(realised.outcome, Outcome::Success));
        assert_eq!(realised.provenance, Provenance::Simulated);
        assert_eq!(realised.tx_ref, None);
        assert_eq!(realised.at, 42);

        // The allowance override must have gone to the router the route
        // actually calls (SPEC.md §5's explicit pitfall), not some other
        // address.
        let cached_spender = adapter
            .allowance_slot_cache
            .lock()
            .unwrap()
            .keys()
            .next()
            .copied();
        assert_eq!(cached_spender, Some((token, router_addr)));
    }

    #[tokio::test]
    async fn execute_reports_the_solidity_revert_reason() {
        let server = MockServer::start().await;
        let router = [0x11; 20];
        let calldata = vec![0xff; 4];
        let sender = Address::from([0xCC; 20]);
        let router_addr = Address::from(router);

        mount_slot_probe(&server, BALANCE_OF_SELECTOR, mapping_slot(sender, 0)).await;
        mount_slot_probe(
            &server,
            ALLOWANCE_SELECTOR,
            allowance_slot(sender, router_addr, 0),
        )
        .await;

        // Error(string) ABI-encoded "insufficient liquidity".
        let reason = "insufficient liquidity";
        let mut error_data = ERROR_STRING_SELECTOR.to_vec();
        error_data.extend_from_slice(&U256::from(32u64).to_be_bytes::<32>());
        error_data.extend_from_slice(&U256::from(reason.len() as u64).to_be_bytes::<32>());
        let mut padded_reason = reason.as_bytes().to_vec();
        padded_reason.resize(padded_reason.len().div_ceil(32) * 32, 0);
        error_data.extend_from_slice(&padded_reason);

        let expected_calldata = calldata.clone();
        let error_data_hex = format!("0x{}", hex::encode(&error_data));
        Mock::given(method("POST"))
            .and(body_partial_json(json!({ "method": "eth_call" })))
            .respond_with(move |req: &wiremock::Request| {
                let body: Json = req.body_json().unwrap();
                let params = body["params"].as_array().unwrap();
                let data = params[0]["data"].as_str().unwrap();
                let data_bytes = decode_hex(data).unwrap();
                if data_bytes == expected_calldata {
                    ResponseTemplate::new(200).set_body_json(json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "error": { "code": 3, "message": "execution reverted", "data": error_data_hex },
                    }))
                } else {
                    ResponseTemplate::new(200).set_body_json(json!({
                        "jsonrpc": "2.0", "id": 1, "result": format_u256(U256::ZERO),
                    }))
                }
            })
            .mount(&server)
            .await;

        let adapter = EvmSimulated::new(server.uri());
        let route = route(payload_for(router, &calldata));
        let request = request();

        let prepared = adapter.prepare(&route, &request).await.unwrap();
        let realised = adapter.execute(&prepared, Some(1)).await.unwrap();

        assert_eq!(realised.amount_out, None);
        match realised.outcome {
            Outcome::Reverted { reason: got } => assert_eq!(got, reason),
            other => panic!("expected Reverted, got {other:?}"),
        }
    }

    /// Everything above proves this adapter's logic against a mocked
    /// JSON-RPC server. This test proves the one part a mock can't: that
    /// the slot-probing state-override technique actually works against a
    /// *real* node's real storage layout — SPEC.md §9.2's real-RPC bar,
    /// worked toward incrementally rather than all at once.
    ///
    /// No-ops (does not fail) when `EVM_LIVE_RPC_URL` is unset, so it's
    /// silent for every contributor and CI run that hasn't opted in —
    /// exactly the "network-dependent job gated on a secret being present"
    /// `IMPLEMENTATION_PLAN.md` Phase 4 asks for. Read-only (`eth_call`,
    /// no transaction), so it needs no signer and spends nothing: probing
    /// `balanceOf`/`allowance` for an arbitrary address is safe against
    /// any real ERC-20.
    #[tokio::test]
    async fn against_real_sepolia_usdc_finds_real_balance_and_allowance_slots() {
        let Ok(rpc_url) = std::env::var("EVM_LIVE_RPC_URL") else {
            eprintln!("skipping: EVM_LIVE_RPC_URL is not set");
            return;
        };

        // Circle's official Sepolia USDC (a real, verified-deployed proxy
        // contract) and Uniswap V3's SwapRouter02 on Sepolia (used here
        // only as an arbitrary allowance spender, not actually called).
        let usdc: Address = "0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238"
            .parse()
            .unwrap();
        let router: Address = "0x3bFA4769FB09eefC5a80d6E87c3B9C650f7Ae48E"
            .parse()
            .unwrap();
        // An arbitrary address — this probe never needs it to hold a real
        // balance or have granted a real allowance.
        let sender: Address = "0x000000000000000000000000000000000000dEaD"
            .parse()
            .unwrap();

        let adapter = EvmSimulated::new(rpc_url);
        let block = adapter
            .latest_block_number()
            .await
            .expect("real Sepolia RPC should answer eth_blockNumber");

        let balance_index = adapter
            .find_balance_slot(usdc, sender, block)
            .await
            .expect("balanceOf storage slot should be probeable on real USDC");
        let allowance_index = adapter
            .find_allowance_slot(usdc, sender, router, block)
            .await
            .expect("allowance storage slot should be probeable on real USDC");

        eprintln!(
            "real Sepolia USDC @ block {block}: balanceOf slot index {balance_index}, \
             allowance slot index {allowance_index}"
        );
    }

    /// Exploratory — not a permanent fixture — one real swap through a
    /// real, currently-liquid Uniswap V3 pool (USDC/WETH, 0.3% fee,
    /// verified on GeckoTerminal) via SwapRouter02's real
    /// `exactInputSingle`, gated on `EVM_LIVE_RPC_URL` the same way.
    #[tokio::test]
    async fn against_real_sepolia_executes_a_real_swap_via_swaprouter02() {
        let Ok(rpc_url) = std::env::var("EVM_LIVE_RPC_URL") else {
            eprintln!("skipping: EVM_LIVE_RPC_URL is not set");
            return;
        };

        let usdc: Address = "0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238"
            .parse()
            .unwrap();
        let weth: Address = "0xfFf9976782d46CC05630D1f6eBAb18b2324d6B14"
            .parse()
            .unwrap();
        let router: Address = "0x3bFA4769FB09eefC5a80d6E87c3B9C650f7Ae48E"
            .parse()
            .unwrap();
        let sender: Address = "0x000000000000000000000000000000000000dEaD"
            .parse()
            .unwrap();

        // SwapRouter02#exactInputSingle((address,address,uint24,address,uint256,uint256,uint160)) — 0x04e45aaf.
        let mut calldata = vec![0x04, 0xe4, 0x5a, 0xaf];
        calldata.extend_from_slice(&pad_address(usdc));
        calldata.extend_from_slice(&pad_address(weth));
        calldata.extend_from_slice(&U256::from(3_000u64).to_be_bytes::<32>()); // fee: 0.3%
        calldata.extend_from_slice(&pad_address(sender)); // recipient
        calldata.extend_from_slice(&U256::from(1_000_000u64).to_be_bytes::<32>()); // amountIn: 1 USDC
        calldata.extend_from_slice(&U256::ZERO.to_be_bytes::<32>()); // amountOutMinimum
        calldata.extend_from_slice(&U256::ZERO.to_be_bytes::<32>()); // sqrtPriceLimitX96

        let mut payload = router.as_slice().to_vec();
        payload.extend_from_slice(&calldata);

        let route = RouteQuote {
            chain_id: 11_155_111,
            token_in: usdc.as_slice().to_vec(),
            token_out: weth.as_slice().to_vec(),
            amount_in: 1_000_000,
            expected_amount_out: 0,
            payload,
        };
        let request = SwapRequest {
            sender: sender.as_slice().to_vec(),
            recipient: sender.as_slice().to_vec(),
            min_amount_out: 0,
            deadline_unix_secs: 0,
        };

        let adapter = EvmSimulated::new(rpc_url);
        let prepared = adapter.prepare(&route, &request).await.unwrap();
        let realised = adapter.execute(&prepared, None).await.unwrap();
        eprintln!("real swap result: {realised:?}");
    }
}
