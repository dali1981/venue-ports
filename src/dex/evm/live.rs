//! `EvmLive` — signs and broadcasts a real EIP-1559 transaction, then polls
//! for the receipt (`SPEC.md` §5, first required implementation;
//! `IMPLEMENTATION_PLAN.md` Phase 5). Never exercised by an automated test
//! or a schedule — only ever a deliberate, human-triggered run against a
//! disposable, faucet-funded testnet signer (`SPEC.md` §3/§9.4).
//!
//! **Adapter convention for `RouteQuote.payload`:** same as `EvmSimulated`
//! — `router_address (20 bytes) ++ calldata`. This adapter never inspects
//! or rebuilds that calldata; it only signs and sends it, so whatever
//! encoded it is responsible for its correctness (including, e.g., any
//! `amountOutMinimum` the router itself checks).
//!
//! **`SwapRequest.deadline_unix_secs` enforcement:** `SPEC.md` §5 asks for
//! a real deadline "never omitted". Because this adapter's calldata is
//! opaque and already fully built by the caller (see above), it cannot
//! embed a deadline into a call that doesn't already carry one (Uniswap's
//! `SwapRouter02#exactInputSingle`, used by the real-network tests
//! elsewhere in this crate, has no deadline parameter at all). What this
//! adapter *can* and does enforce: `prepare()` refuses a zero/omitted
//! deadline outright, and `execute()` refuses to broadcast anything once
//! wall-clock time has passed it — a weaker guarantee than an on-chain
//! deadline check, but the strongest available without changing what
//! calldata means.
//!
//! **One nonce in flight per chain at a time** (`SPEC.md` §5): enforced by
//! `tx::Signer`'s send lock, held for the whole build-sign-broadcast-poll
//! sequence of every transaction this adapter sends — including the
//! `approve` below, which is why it and the swap that follows it never
//! race for the same nonce.
//!
//! **The venue cap, never an unlimited allowance** (`SPEC.md` §5): before
//! ever sending the swap, `ensure_allowance` checks the router's current
//! allowance and, only if it's short, sends an `approve` capped to exactly
//! `route.amount_in` — never `U256::MAX`.

use crate::dex::evm::tx::Signer;
use crate::dex::{ChainAmount, DexExecutor, Outcome, Prepared, Realised, RouteQuote, SwapRequest};
use crate::Provenance;
use alloy_consensus::TxEip1559;
use alloy_primitives::{keccak256, Address, TxKind, B256, U256};
use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

const ALLOWANCE_SELECTOR: [u8; 4] = [0xdd, 0x62, 0xed, 0x3e];
const APPROVE_SELECTOR: [u8; 4] = [0x09, 0x5e, 0xa7, 0xb3];
const ERROR_STRING_SELECTOR: [u8; 4] = [0x08, 0xc3, 0x79, 0xa0];

struct PendingLive {
    token_in: Address,
    token_out: Address,
    sender: Address,
    recipient: Address,
    amount_in: ChainAmount,
    deadline_unix_secs: u64,
    chain_id: u64,
}

/// The outcome of sending and confirming exactly one real transaction —
/// internal plumbing shared by the `approve` step and the swap itself.
/// Not `Realised`: an `approve`'s revert/timeout is a setup failure for
/// this `execute()` call, not the swap's own outcome, so only the swap's
/// `TxOutcome` gets turned into one.
enum TxOutcome {
    Success {
        block: u64,
        tx_hash: B256,
        logs: Vec<Value>,
    },
    Reverted {
        block: u64,
        tx_hash: B256,
        reason: String,
    },
    TimedOut {
        tx_hash: B256,
    },
}

pub struct EvmLive {
    rpc_url: String,
    http: reqwest::Client,
    signer: Signer,
    pending: Mutex<HashMap<B256, PendingLive>>,
    receipt_poll_interval: Duration,
    receipt_poll_timeout: Duration,
}

impl EvmLive {
    /// `signer_private_key_hex` is a raw secp256k1 private key (with or
    /// without a `0x` prefix) — a disposable, faucet-funded testnet key,
    /// never a mainnet one at this stage (`IMPLEMENTATION_PLAN.md`
    /// Blocker 2). Defaults to a 4-second poll every attempt, giving up
    /// after 3 minutes (comfortable headroom over Sepolia's ~12s blocks).
    pub fn new(rpc_url: impl Into<String>, signer_private_key_hex: &str) -> Result<Self> {
        Ok(Self {
            rpc_url: rpc_url.into(),
            http: reqwest::Client::new(),
            signer: Signer::from_private_key_hex(signer_private_key_hex)?,
            pending: Mutex::new(HashMap::new()),
            receipt_poll_interval: Duration::from_secs(4),
            receipt_poll_timeout: Duration::from_secs(180),
        })
    }

    /// Overrides the default receipt-polling cadence — used by tests to
    /// force a deterministic `Outcome::TimedOut` without actually waiting
    /// three minutes against a mock server that never answers.
    pub fn with_poll_settings(mut self, interval: Duration, timeout: Duration) -> Self {
        self.receipt_poll_interval = interval;
        self.receipt_poll_timeout = timeout;
        self
    }

    /// This `EvmLive`'s own signing address — the only address it will
    /// ever sign a `SwapRequest.sender` as.
    pub fn address(&self) -> Address {
        self.signer.address
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
            bail!(decode_revert_reason(error));
        }
        response.get("result").cloned().ok_or_else(|| {
            anyhow!("JSON-RPC response had neither `result` nor `error`: {response}")
        })
    }

    async fn eth_call(
        &self,
        to: Address,
        data: &[u8],
        from: Address,
        block: &str,
    ) -> Result<Vec<u8>> {
        let call = json!({
            "to": to.to_string(),
            "from": from.to_string(),
            "data": format!("0x{}", hex::encode(data)),
        });
        let result = self.rpc_call("eth_call", json!([call, block])).await?;
        decode_hex(
            result
                .as_str()
                .ok_or_else(|| anyhow!("eth_call result was not a hex string: {result}"))?,
        )
    }

    async fn fetch_nonce(&self, address: Address) -> Result<u64> {
        let result = self
            .rpc_call(
                "eth_getTransactionCount",
                json!([address.to_string(), "pending"]),
            )
            .await?;
        parse_hex_u64(result.as_str().ok_or_else(|| {
            anyhow!("eth_getTransactionCount result was not a hex string: {result}")
        })?)
    }

    /// `(max_priority_fee_per_gas, max_fee_per_gas)`, both in wei. The
    /// max-fee headroom (2x the current base fee, plus the priority fee)
    /// is the same heuristic ethers.js/viem use by default — not a
    /// protocol requirement, just enough slack that a fee spike across a
    /// block or two doesn't strand the transaction.
    async fn fetch_fees(&self) -> Result<(u128, u128)> {
        let block = self
            .rpc_call("eth_getBlockByNumber", json!(["latest", false]))
            .await?;
        let base_fee = parse_hex_u128(
            block
                .get("baseFeePerGas")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    anyhow!("latest block has no baseFeePerGas — is this chain EIP-1559-enabled?")
                })?,
        )?;

        let priority_fee = match self.rpc_call("eth_maxPriorityFeePerGas", json!([])).await {
            Ok(v) => parse_hex_u128(v.as_str().unwrap_or("0x0"))?,
            // Not every node implements this method; 1.5 gwei is a
            // reasonable Sepolia default when it's unavailable.
            Err(_) => 1_500_000_000,
        };
        let max_fee = base_fee.saturating_mul(2).saturating_add(priority_fee);
        Ok((priority_fee, max_fee))
    }

    async fn estimate_gas(
        &self,
        from: Address,
        to: Address,
        data: &[u8],
        value: U256,
    ) -> Result<u64> {
        let call = json!({
            "from": from.to_string(),
            "to": to.to_string(),
            "data": format!("0x{}", hex::encode(data)),
            "value": format!("0x{value:x}"),
        });
        let result = self.rpc_call("eth_estimateGas", json!([call])).await?;
        let raw =
            parse_hex_u64(result.as_str().ok_or_else(|| {
                anyhow!("eth_estimateGas result was not a hex string: {result}")
            })?)?;
        // 20% headroom: an estimate is a snapshot of current state, and
        // this transaction won't actually execute until it's mined
        // against whatever state exists by then.
        Ok(raw + raw / 5)
    }

    /// Replays the just-reverted call via `eth_call` at the block it was
    /// mined in, to recover the Solidity revert reason a receipt alone
    /// never carries. Best-effort: if the replay itself can't produce a
    /// reason (state moved, or the node disagrees), says so rather than
    /// guessing.
    async fn fetch_revert_reason(
        &self,
        from: Address,
        to: Address,
        data: &[u8],
        block: u64,
    ) -> String {
        let call = json!({
            "from": from.to_string(),
            "to": to.to_string(),
            "data": format!("0x{}", hex::encode(data)),
        });
        match self
            .rpc_call("eth_call", json!([call, format!("0x{block:x}")]))
            .await
        {
            Err(err) => err.to_string(),
            Ok(_) => {
                "transaction reverted (replaying the call at its block did not reproduce a revert)"
                    .to_string()
            }
        }
    }

    async fn broadcast(&self, raw_tx: &[u8]) -> Result<()> {
        let raw_hex = format!("0x{}", hex::encode(raw_tx));
        self.rpc_call("eth_sendRawTransaction", json!([raw_hex]))
            .await?;
        Ok(())
    }

    /// Builds, signs, broadcasts, and polls exactly one EIP-1559
    /// transaction to a terminal outcome, under the signer's send lock —
    /// see the module docs for why that lock must span this whole
    /// sequence.
    async fn send_and_confirm(
        &self,
        to: Address,
        calldata: Vec<u8>,
        value: U256,
        chain_id: u64,
    ) -> Result<TxOutcome> {
        let sender = self.signer.address;
        let calldata_for_replay = calldata.clone();
        self.signer
            .with_send_lock(move || async move {
                let nonce = self.fetch_nonce(sender).await?;
                let (priority_fee, max_fee) = self.fetch_fees().await?;
                let gas_limit = self.estimate_gas(sender, to, &calldata, value).await?;

                let tx = TxEip1559 {
                    chain_id,
                    nonce,
                    gas_limit,
                    max_fee_per_gas: max_fee,
                    max_priority_fee_per_gas: priority_fee,
                    to: TxKind::Call(to),
                    value,
                    input: calldata.into(),
                    ..Default::default()
                };
                let (raw, tx_hash) = self.signer.sign_eip1559(tx);
                self.broadcast(&raw).await?;

                let deadline = tokio::time::Instant::now() + self.receipt_poll_timeout;
                loop {
                    let receipt = self
                        .rpc_call(
                            "eth_getTransactionReceipt",
                            json!([format!("0x{}", hex::encode(tx_hash))]),
                        )
                        .await?;
                    if !receipt.is_null() {
                        let status = receipt
                            .get("status")
                            .and_then(Value::as_str)
                            .unwrap_or("0x0");
                        let block = parse_hex_u64(
                            receipt
                                .get("blockNumber")
                                .and_then(Value::as_str)
                                .unwrap_or("0x0"),
                        )?;
                        if status == "0x1" {
                            let logs = receipt
                                .get("logs")
                                .and_then(Value::as_array)
                                .cloned()
                                .unwrap_or_default();
                            return Ok(TxOutcome::Success {
                                block,
                                tx_hash,
                                logs,
                            });
                        }
                        let reason = self
                            .fetch_revert_reason(sender, to, &calldata_for_replay, block)
                            .await;
                        return Ok(TxOutcome::Reverted {
                            block,
                            tx_hash,
                            reason,
                        });
                    }
                    if tokio::time::Instant::now() >= deadline {
                        return Ok(TxOutcome::TimedOut { tx_hash });
                    }
                    tokio::time::sleep(self.receipt_poll_interval).await;
                }
            })
            .await
    }

    /// Ensures the router's allowance over `token_in` covers `amount_in`,
    /// sending a capped `approve` (never `U256::MAX`) only if it doesn't.
    /// A failure here is a setup failure for this `execute()` call, not
    /// the swap's own outcome — it becomes an `Err`, not an `Outcome`.
    async fn ensure_allowance(&self, ctx: &PendingLive, router: Address) -> Result<()> {
        let mut calldata = ALLOWANCE_SELECTOR.to_vec();
        calldata.extend_from_slice(&pad_address(ctx.sender));
        calldata.extend_from_slice(&pad_address(router));
        let data = self
            .eth_call(ctx.token_in, &calldata, ctx.sender, "latest")
            .await
            .context("checking the router's current allowance")?;
        let current = U256::from_be_slice(&data);
        if current >= U256::from(ctx.amount_in) {
            return Ok(());
        }

        let mut approve_calldata = APPROVE_SELECTOR.to_vec();
        approve_calldata.extend_from_slice(&pad_address(router));
        approve_calldata.extend_from_slice(&U256::from(ctx.amount_in).to_be_bytes::<32>());

        match self
            .send_and_confirm(ctx.token_in, approve_calldata, U256::ZERO, ctx.chain_id)
            .await?
        {
            TxOutcome::Success { .. } => Ok(()),
            TxOutcome::Reverted {
                reason, tx_hash, ..
            } => bail!(
                "approve({router}, {}) reverted (tx {tx_hash}): {reason}",
                ctx.amount_in
            ),
            TxOutcome::TimedOut { tx_hash } => bail!(
                "approve({router}, {}) timed out waiting for a receipt (tx {tx_hash}) — \
                 its nonce may still land later; check before retrying",
                ctx.amount_in
            ),
        }
    }
}

#[async_trait]
impl DexExecutor for EvmLive {
    async fn prepare(&self, route: &RouteQuote, req: &SwapRequest) -> Result<Prepared> {
        if route.payload.len() < 20 {
            bail!(
                "route.payload must be at least 20 bytes (router address + calldata), got {}",
                route.payload.len()
            );
        }
        let (router_bytes, calldata) = route.payload.split_at(20);
        let token_in = address_from_slice(&route.token_in).context("route.token_in")?;
        let token_out = address_from_slice(&route.token_out).context("route.token_out")?;
        let sender = address_from_slice(&req.sender).context("req.sender")?;
        let recipient = address_from_slice(&req.recipient).context("req.recipient")?;

        if sender.is_zero() {
            bail!("SwapRequest.sender must not be the zero address for EvmLive");
        }
        if recipient.is_zero() {
            bail!("SwapRequest.recipient must not be the zero address for EvmLive");
        }
        if req.deadline_unix_secs == 0 {
            bail!(
                "SwapRequest.deadline_unix_secs must be set for EvmLive — see the module docs \
                 for how it's enforced given this adapter's opaque-calldata convention"
            );
        }
        if sender != self.signer.address {
            bail!(
                "SwapRequest.sender ({sender}) does not match this EvmLive's signing address \
                 ({}) — it can only ever sign for its own key",
                self.signer.address
            );
        }

        let prepared = Prepared {
            to: router_bytes.to_vec(),
            calldata: calldata.to_vec(),
            value: 0,
        };
        self.pending.lock().unwrap().insert(
            pending_key(&prepared),
            PendingLive {
                token_in,
                token_out,
                sender,
                recipient,
                amount_in: route.amount_in,
                deadline_unix_secs: req.deadline_unix_secs,
                chain_id: route.chain_id,
            },
        );
        Ok(prepared)
    }

    /// `at` is ignored: a live adapter only ever acts *now* — there is no
    /// "re-run this at a historical block" for a real broadcast the way
    /// there is for `EvmSimulated`'s throwaway `eth_call`.
    async fn execute(&self, prepared: &Prepared, _at: Option<u64>) -> Result<Realised> {
        let ctx = self
            .pending
            .lock()
            .unwrap()
            .remove(&pending_key(prepared))
            .ok_or_else(|| {
                anyhow!(
                    "execute() called with a Prepared value this EvmLive instance did not \
                     produce, or already consumed"
                )
            })?;
        let router = address_from_slice(&prepared.to).context("prepared.to")?;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        if now >= ctx.deadline_unix_secs {
            bail!(
                "SwapRequest.deadline_unix_secs ({}) has already passed (now {now}) — \
                 refusing to broadcast",
                ctx.deadline_unix_secs
            );
        }

        self.ensure_allowance(&ctx, router).await?;

        match self
            .send_and_confirm(router, prepared.calldata.clone(), U256::ZERO, ctx.chain_id)
            .await?
        {
            TxOutcome::Success {
                block,
                tx_hash,
                logs,
            } => {
                let amount_out = decode_transfer_amount(&logs, ctx.token_out, ctx.recipient)
                    .ok_or_else(|| {
                        anyhow!(
                            "swap landed (tx {tx_hash}) but no ERC-20 Transfer of the output \
                             token to the recipient was found in its logs"
                        )
                    })?;
                Ok(Realised {
                    amount_out: Some(amount_out),
                    outcome: Outcome::Success,
                    at: block,
                    provenance: Provenance::Landed,
                    tx_ref: Some(tx_hash.as_slice().to_vec()),
                })
            }
            TxOutcome::Reverted {
                block,
                tx_hash,
                reason,
            } => Ok(Realised {
                amount_out: None,
                outcome: Outcome::Reverted { reason },
                at: block,
                provenance: Provenance::Landed,
                tx_ref: Some(tx_hash.as_slice().to_vec()),
            }),
            TxOutcome::TimedOut { tx_hash } => {
                let at = self
                    .rpc_call("eth_blockNumber", json!([]))
                    .await
                    .ok()
                    .and_then(|v| v.as_str().and_then(|s| parse_hex_u64(s).ok()))
                    .unwrap_or(0);
                Ok(Realised {
                    amount_out: None,
                    outcome: Outcome::TimedOut,
                    at,
                    provenance: Provenance::Landed,
                    tx_ref: Some(tx_hash.as_slice().to_vec()),
                })
            }
        }
    }

    fn label(&self) -> &'static str {
        "evm-live"
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

fn decode_hex(s: &str) -> Result<Vec<u8>> {
    hex::decode(s.trim_start_matches("0x")).context("decoding hex string")
}

fn parse_hex_u64(s: &str) -> Result<u64> {
    u64::from_str_radix(s.trim_start_matches("0x"), 16)
        .with_context(|| format!("could not parse hex u64 {s}"))
}

fn parse_hex_u128(s: &str) -> Result<u128> {
    u128::from_str_radix(s.trim_start_matches("0x"), 16)
        .with_context(|| format!("could not parse hex u128 {s}"))
}

/// `keccak256("Transfer(address,address,uint256)")`, computed rather than
/// hardcoded — a 32-byte constant is exactly the kind of thing worth not
/// trusting to memory when a one-line computation is just as cheap.
fn transfer_topic0() -> B256 {
    keccak256(b"Transfer(address,address,uint256)")
}

/// Scans a receipt's logs for an ERC-20 `Transfer` of `token` to
/// `recipient`, returning its `value`. A router swap may emit several
/// `Transfer`s (intermediate hops, fee transfers); filtering by both the
/// token and the recipient is what picks out the one that's actually this
/// swap's output, not an incidental one along the way.
fn decode_transfer_amount(
    logs: &[Value],
    token: Address,
    recipient: Address,
) -> Option<ChainAmount> {
    let topic0_hex = format!("0x{}", hex::encode(transfer_topic0()));
    for log in logs {
        let log_address: Address = log.get("address")?.as_str()?.parse().ok()?;
        if log_address != token {
            continue;
        }
        let topics = log.get("topics")?.as_array()?;
        if topics.len() < 3 || topics[0].as_str()?.to_lowercase() != topic0_hex {
            continue;
        }
        let to_bytes = decode_hex(topics[2].as_str()?).ok()?;
        if to_bytes.len() != 32 || Address::from_slice(&to_bytes[12..]) != recipient {
            continue;
        }
        let data_bytes = decode_hex(log.get("data")?.as_str()?).ok()?;
        return U256::from_be_slice(&data_bytes).try_into().ok();
    }
    None
}

/// Best-effort decode of a Solidity revert reason from a JSON-RPC error's
/// `data` field: the standard `Error(string)` `require`/`revert` encoding
/// when present, the node's own error message otherwise. Identical to
/// `EvmSimulated`'s version of this — duplicated rather than shared, same
/// as every other per-file helper in this module family.
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

/// A key correlating a `Prepared` value with the context `prepare()`
/// stashed for it — identical technique to `EvmSimulated`'s.
fn pending_key(prepared: &Prepared) -> B256 {
    let mut buf = Vec::with_capacity(prepared.to.len() + prepared.calldata.len() + 16);
    buf.extend_from_slice(&prepared.to);
    buf.extend_from_slice(&prepared.calldata);
    buf.extend_from_slice(&prepared.value.to_be_bytes());
    keccak256(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_partial_json, body_string_contains, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Any valid nonzero secp256k1 scalar — these tests only need *a*
    /// working signer, never a specific address, so nothing is gained by
    /// pinning a memorized test vector here.
    fn test_key() -> String {
        "11".repeat(32)
    }

    fn route(
        router: Address,
        token_in: Address,
        token_out: Address,
        calldata: &[u8],
    ) -> RouteQuote {
        let mut payload = router.as_slice().to_vec();
        payload.extend_from_slice(calldata);
        RouteQuote {
            chain_id: 11_155_111,
            token_in: token_in.as_slice().to_vec(),
            token_out: token_out.as_slice().to_vec(),
            amount_in: 1_000,
            expected_amount_out: 900,
            payload,
        }
    }

    fn request(sender: Address, recipient: Address) -> SwapRequest {
        SwapRequest {
            sender: sender.as_slice().to_vec(),
            recipient: recipient.as_slice().to_vec(),
            min_amount_out: 900,
            deadline_unix_secs: 9_999_999_999,
        }
    }

    #[tokio::test]
    async fn prepare_rejects_the_zero_address_as_sender_or_recipient() {
        let live = EvmLive::new("http://127.0.0.1:0", &test_key()).unwrap();
        let router = Address::from([0x11; 20]);
        let token = Address::from([0xAA; 20]);
        let route = route(router, token, token, &[0xCA, 0xFE]);

        let zero_sender = SwapRequest {
            sender: Address::ZERO.as_slice().to_vec(),
            ..request(live.address(), Address::from([0xDD; 20]))
        };
        assert!(live.prepare(&route, &zero_sender).await.is_err());

        let zero_recipient = SwapRequest {
            recipient: Address::ZERO.as_slice().to_vec(),
            ..request(live.address(), Address::from([0xDD; 20]))
        };
        assert!(live.prepare(&route, &zero_recipient).await.is_err());
    }

    #[tokio::test]
    async fn prepare_rejects_an_omitted_deadline() {
        let live = EvmLive::new("http://127.0.0.1:0", &test_key()).unwrap();
        let router = Address::from([0x11; 20]);
        let token = Address::from([0xAA; 20]);
        let route = route(router, token, token, &[0xCA, 0xFE]);
        let req = SwapRequest {
            deadline_unix_secs: 0,
            ..request(live.address(), Address::from([0xDD; 20]))
        };
        let err = live.prepare(&route, &req).await.unwrap_err();
        assert!(err.to_string().contains("deadline_unix_secs"));
    }

    #[tokio::test]
    async fn prepare_rejects_a_sender_that_is_not_this_signers_own_address() {
        let live = EvmLive::new("http://127.0.0.1:0", &test_key()).unwrap();
        let router = Address::from([0x11; 20]);
        let token = Address::from([0xAA; 20]);
        let route = route(router, token, token, &[0xCA, 0xFE]);
        let req = request(Address::from([0x99; 20]), Address::from([0xDD; 20]));
        let err = live.prepare(&route, &req).await.unwrap_err();
        assert!(err.to_string().contains("does not match"));
    }

    /// Mounts the RPC methods every `send_and_confirm` call needs
    /// regardless of which transaction (approve or swap) it's sending —
    /// nonce, fee data, gas estimation, and broadcast. None of these care
    /// how many times they're called.
    async fn mount_send_plumbing(server: &MockServer) {
        Mock::given(method("POST"))
            .and(body_partial_json(
                json!({"method": "eth_getTransactionCount"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0", "id": 1, "result": "0x5",
            })))
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "eth_getBlockByNumber"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0", "id": 1, "result": { "baseFeePerGas": "0x3b9aca00" },
            })))
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(body_partial_json(
                json!({"method": "eth_maxPriorityFeePerGas"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0", "id": 1, "result": "0x59682f00",
            })))
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "eth_estimateGas"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0", "id": 1, "result": "0x5208",
            })))
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(body_partial_json(
                json!({"method": "eth_sendRawTransaction"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0", "id": 1, "result": "0xdeadbeef",
            })))
            .mount(server)
            .await;
    }

    /// Mounts an `eth_call` handler that reports a sufficient allowance
    /// already in place, so `ensure_allowance` never sends an `approve` —
    /// isolating a test to exactly one `send_and_confirm` (the swap).
    /// Scoped to calls whose data carries the `allowance` selector: two
    /// equal-priority mocks that both matched the same request would
    /// resolve to whichever was registered first (`wiremock`'s
    /// `mock_set.rs` sorts by priority, stable, then takes the first
    /// match), so leaving this unscoped would silently swallow the
    /// revert-reason replay call in `execute_reports_the_solidity_revert_reason`
    /// too if it happened to be mounted first there.
    async fn mount_sufficient_allowance(server: &MockServer) {
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "eth_call"})))
            .and(body_string_contains(hex::encode(ALLOWANCE_SELECTOR)))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0", "id": 1,
                "result": format!("0x{}", hex::encode(U256::MAX.to_be_bytes::<32>())),
            })))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn execute_approves_then_swaps_and_decodes_the_output_transfer() {
        let server = MockServer::start().await;
        let router = Address::from([0x11; 20]);
        let token_in = Address::from([0xAA; 20]);
        let token_out = Address::from([0xBB; 20]);
        let recipient = Address::from([0xDD; 20]);
        let calldata = vec![0xCA, 0xFE, 0xBA, 0xBE];

        mount_send_plumbing(&server).await;

        // Allowance check reports zero (forcing an `approve`) for any
        // `eth_call` whose data starts with the `allowance` selector;
        // anything else (there's no revert-reason replay on this happy
        // path) gets an empty-but-valid response.
        let allowance_selector_hex = hex::encode(ALLOWANCE_SELECTOR);
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "eth_call"})))
            .respond_with(move |req: &wiremock::Request| {
                let body: Value = req.body_json().unwrap();
                let data = body["params"][0]["data"].as_str().unwrap_or("");
                let result = if data.starts_with(&format!("0x{allowance_selector_hex}")) {
                    format!("0x{}", hex::encode(U256::ZERO.to_be_bytes::<32>()))
                } else {
                    "0x".to_string()
                };
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "jsonrpc": "2.0", "id": 1, "result": result }))
            })
            .mount(&server)
            .await;

        let transfer_log = json!({
            "address": token_out.to_string(),
            "topics": [
                format!("0x{}", hex::encode(transfer_topic0())),
                format!("0x{}", hex::encode(pad_address(router))),
                format!("0x{}", hex::encode(pad_address(recipient))),
            ],
            "data": format!("0x{}", hex::encode(U256::from(4_200u64).to_be_bytes::<32>())),
        });
        Mock::given(method("POST"))
            .and(body_partial_json(
                json!({"method": "eth_getTransactionReceipt"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": { "status": "0x1", "blockNumber": "0x2a", "logs": [transfer_log] },
            })))
            .mount(&server)
            .await;

        let live = EvmLive::new(server.uri(), &test_key()).unwrap();
        let sender = live.address();
        let route = route(router, token_in, token_out, &calldata);
        let req = request(sender, recipient);

        let prepared = live.prepare(&route, &req).await.unwrap();
        let realised = live.execute(&prepared, None).await.unwrap();

        assert_eq!(realised.amount_out, Some(4_200));
        assert!(matches!(realised.outcome, Outcome::Success));
        assert_eq!(realised.provenance, Provenance::Landed);
        assert!(realised.tx_ref.is_some());
        assert_eq!(realised.at, 42);
    }

    #[tokio::test]
    async fn execute_reports_the_solidity_revert_reason() {
        let server = MockServer::start().await;
        let router = Address::from([0x11; 20]);
        let token = Address::from([0xAA; 20]);
        let recipient = Address::from([0xDD; 20]);

        mount_send_plumbing(&server).await;
        mount_sufficient_allowance(&server).await;

        // Error(string) ABI-encoded "insufficient liquidity" — served both
        // as the (unused, since allowance is sufficient) revert-reason
        // path and would also be what a real node returns.
        let reason = "insufficient liquidity";
        let mut error_data = ERROR_STRING_SELECTOR.to_vec();
        error_data.extend_from_slice(&U256::from(32u64).to_be_bytes::<32>());
        error_data.extend_from_slice(&U256::from(reason.len() as u64).to_be_bytes::<32>());
        let mut padded_reason = reason.as_bytes().to_vec();
        padded_reason.resize(padded_reason.len().div_ceil(32) * 32, 0);
        error_data.extend_from_slice(&padded_reason);
        let error_data_hex = format!("0x{}", hex::encode(&error_data));

        Mock::given(method("POST"))
            .and(body_partial_json(
                json!({"method": "eth_getTransactionReceipt"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0", "id": 1,
                "result": { "status": "0x0", "blockNumber": "0x2a", "logs": [] },
            })))
            .mount(&server)
            .await;

        // The revert-reason replay: any `eth_call` that isn't the
        // allowance probe (already mounted above, and matched first by
        // wiremock's most-recently-mounted-wins order) hits this instead.
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "eth_call"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0", "id": 1,
                "error": { "code": 3, "message": "execution reverted", "data": error_data_hex },
            })))
            .mount(&server)
            .await;

        let live = EvmLive::new(server.uri(), &test_key()).unwrap();
        let sender = live.address();
        let route = route(router, token, token, &[0xCA, 0xFE]);
        let req = request(sender, recipient);

        let prepared = live.prepare(&route, &req).await.unwrap();
        let realised = live.execute(&prepared, None).await.unwrap();

        assert_eq!(realised.amount_out, None);
        assert_eq!(realised.provenance, Provenance::Landed);
        assert!(realised.tx_ref.is_some());
        match realised.outcome {
            Outcome::Reverted { reason: got } => assert_eq!(got, reason),
            other => panic!("expected Reverted, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn execute_times_out_when_no_receipt_ever_arrives() {
        let server = MockServer::start().await;
        let router = Address::from([0x11; 20]);
        let token = Address::from([0xAA; 20]);
        let recipient = Address::from([0xDD; 20]);

        mount_send_plumbing(&server).await;
        mount_sufficient_allowance(&server).await;

        Mock::given(method("POST"))
            .and(body_partial_json(
                json!({"method": "eth_getTransactionReceipt"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0", "id": 1, "result": null,
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "eth_blockNumber"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0", "id": 1, "result": "0x2a",
            })))
            .mount(&server)
            .await;

        let live = EvmLive::new(server.uri(), &test_key())
            .unwrap()
            .with_poll_settings(Duration::from_millis(5), Duration::from_millis(20));
        let sender = live.address();
        let route = route(router, token, token, &[0xCA, 0xFE]);
        let req = request(sender, recipient);

        let prepared = live.prepare(&route, &req).await.unwrap();
        let realised = live.execute(&prepared, None).await.unwrap();

        assert_eq!(realised.amount_out, None);
        assert_eq!(realised.provenance, Provenance::Landed);
        assert!(realised.tx_ref.is_some());
        assert!(matches!(realised.outcome, Outcome::TimedOut));
    }

    /// Real Sepolia, all three legs SPEC.md §3 describes for a DEX port,
    /// same pool, same input size: **quoted** (Uniswap's own `QuoterV2` —
    /// independent of this crate), **simulated** (`EvmSimulated`'s
    /// `eth_call` + state overrides), and **executed** (`EvmLive`, this
    /// module — a real signed swap). No-ops when the real-network env vars
    /// aren't set, same as every other real-Sepolia test in this crate.
    ///
    /// The signer's wallet holds only Sepolia ETH going in, so step 0
    /// wraps a small amount into WETH first (`WETH9.deposit()`) — outside
    /// `DexExecutor` entirely, since funding an address is explicitly not
    /// this crate's job (`SPEC.md` §2). That wrap, and the swap's own
    /// `approve`, both go out through `EvmLive`'s own send pipeline
    /// (`send_and_confirm`), not a reimplementation of it.
    #[tokio::test]
    async fn real_sepolia_quote_vs_simulated_vs_executed() {
        let (Ok(rpc_url), Ok(signer_key)) = (
            std::env::var("EVM_LIVE_RPC_URL"),
            std::env::var("EVM_LIVE_SIGNER_KEY"),
        ) else {
            eprintln!("skipping: EVM_LIVE_RPC_URL / EVM_LIVE_SIGNER_KEY not set");
            return;
        };

        let weth: Address = "0xfFf9976782d46CC05630D1f6eBAb18b2324d6B14"
            .parse()
            .unwrap();
        let usdc: Address = "0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238"
            .parse()
            .unwrap();
        let router: Address = "0x3bFA4769FB09eefC5a80d6E87c3B9C650f7Ae48E"
            .parse()
            .unwrap();
        let quoter: Address = "0xEd1f6473345F45b75F8179591dd5bA1888cf2FB3"
            .parse()
            .unwrap();
        const FEE: u64 = 3_000; // 0.3%
        const AMOUNT_IN: u128 = 1_000_000_000_000_000; // 0.001 WETH
        const CHAIN_ID: u64 = 11_155_111;

        let live = EvmLive::new(rpc_url.clone(), &signer_key).unwrap();
        let sender = live.address();

        // Step 0: make sure there's real WETH to swap.
        let mut balance_of = vec![0x70, 0xa0, 0x82, 0x31];
        balance_of.extend_from_slice(&pad_address(sender));
        let weth_balance = U256::from_be_slice(
            &live
                .eth_call(weth, &balance_of, sender, "latest")
                .await
                .expect("checking WETH balance"),
        );
        if weth_balance < U256::from(AMOUNT_IN) {
            let deposit_calldata = vec![0xd0, 0xe3, 0x0d, 0xb0]; // WETH9.deposit()
            match live
                .send_and_confirm(
                    weth,
                    deposit_calldata,
                    U256::from(AMOUNT_IN) * U256::from(4u64),
                    CHAIN_ID,
                )
                .await
                .expect("sending the wrap transaction")
            {
                TxOutcome::Success { tx_hash, .. } => {
                    eprintln!("wrapped ETH -> WETH: tx 0x{}", hex::encode(tx_hash))
                }
                TxOutcome::Reverted { reason, .. } => panic!("WETH deposit() reverted: {reason}"),
                TxOutcome::TimedOut { tx_hash } => {
                    panic!("WETH deposit() timed out (tx 0x{})", hex::encode(tx_hash))
                }
            }
        }

        // Leg 1: quoted — Uniswap's own QuoterV2, not this crate.
        let mut quote_calldata = vec![0xc6, 0xa5, 0x02, 0x6a];
        quote_calldata.extend_from_slice(&pad_address(weth));
        quote_calldata.extend_from_slice(&pad_address(usdc));
        quote_calldata.extend_from_slice(&U256::from(AMOUNT_IN).to_be_bytes::<32>());
        quote_calldata.extend_from_slice(&U256::from(FEE).to_be_bytes::<32>());
        quote_calldata.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());
        let quote_result = live
            .rpc_call(
                "eth_call",
                json!([
                    { "to": quoter.to_string(), "data": format!("0x{}", hex::encode(&quote_calldata)) },
                    "latest",
                ]),
            )
            .await
            .expect("quoter call should succeed");
        let quote_bytes = decode_hex(quote_result.as_str().unwrap()).unwrap();
        let quoted_amount_out: u128 = U256::from_be_slice(&quote_bytes[0..32]).try_into().unwrap();

        // The shared swap calldata both Simulated and Executed run —
        // amountOutMinimum left at 0, same as this crate's other
        // exploratory real-network tests, to avoid a slippage revert on a
        // thin real pool.
        let mut swap_calldata = vec![0x04, 0xe4, 0x5a, 0xaf]; // exactInputSingle
        swap_calldata.extend_from_slice(&pad_address(weth));
        swap_calldata.extend_from_slice(&pad_address(usdc));
        swap_calldata.extend_from_slice(&U256::from(FEE).to_be_bytes::<32>());
        swap_calldata.extend_from_slice(&pad_address(sender));
        swap_calldata.extend_from_slice(&U256::from(AMOUNT_IN).to_be_bytes::<32>());
        swap_calldata.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());
        swap_calldata.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());
        let mut payload = router.as_slice().to_vec();
        payload.extend_from_slice(&swap_calldata);

        let route = RouteQuote {
            chain_id: CHAIN_ID,
            token_in: weth.as_slice().to_vec(),
            token_out: usdc.as_slice().to_vec(),
            amount_in: AMOUNT_IN,
            expected_amount_out: quoted_amount_out,
            payload,
        };

        // Leg 2: simulated — this crate's EvmSimulated, eth_call + state
        // overrides, nothing broadcast.
        let simulated = crate::dex::evm::EvmSimulated::new(rpc_url.clone());
        let sim_request = SwapRequest {
            sender: sender.as_slice().to_vec(),
            recipient: sender.as_slice().to_vec(),
            min_amount_out: 0,
            deadline_unix_secs: 0,
        };
        let sim_prepared = simulated.prepare(&route, &sim_request).await.unwrap();
        let sim_realised = simulated.execute(&sim_prepared, None).await.unwrap();

        // Leg 3: executed — this crate's EvmLive, a real signed swap
        // through the real router.
        let deadline = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 600;
        let live_request = SwapRequest {
            sender: sender.as_slice().to_vec(),
            recipient: sender.as_slice().to_vec(),
            min_amount_out: 0,
            deadline_unix_secs: deadline,
        };
        let live_prepared = live.prepare(&route, &live_request).await.unwrap();
        let live_realised = live.execute(&live_prepared, None).await.unwrap();

        eprintln!(
            "\nWETH->USDC, amountIn = {AMOUNT_IN} wei WETH (0.001 WETH)\n\
             {:>10} | {:>18} wei USDC\n\
             {:>10} | {:?} (outcome {:?})\n\
             {:>10} | {:?} (outcome {:?}, tx {})\n",
            "quoted",
            quoted_amount_out,
            "simulated",
            sim_realised.amount_out,
            sim_realised.outcome,
            "executed",
            live_realised.amount_out,
            live_realised.outcome,
            live_realised
                .tx_ref
                .as_ref()
                .map(|h| format!("0x{}", hex::encode(h)))
                .unwrap_or_default(),
        );

        assert!(matches!(sim_realised.outcome, Outcome::Success));
        assert!(matches!(live_realised.outcome, Outcome::Success));
        assert_eq!(live_realised.provenance, Provenance::Landed);
        assert!(live_realised.tx_ref.is_some());
    }
}
