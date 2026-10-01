//! `SolanaRpc` — the Solana family's JSON-RPC client (`SPEC.md` §5, the
//! Solana family). Every Solana adapter talks to a node through this one
//! type, as every EVM adapter does through `EvmRpc`. It is plain JSON-RPC
//! over `reqwest`: the crate does not use `solana-rpc-client`.
//!
//! The reads return what the adapters need and nothing else: a blockhash
//! and the block height it is valid to, a signature's status, a landed
//! transaction's meta (its fee, its compute units, every account's lamports
//! and token balances before and after), accounts' raw data, and a dry run.
//! A fork node's `surfnet_*` cheatcodes sit here too, for the fork sender.

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use serde_json::{json, Value};
use solana_address::Address;
use std::time::Duration;

const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// A JSON-RPC error the node returned, with its code: what was asked was
/// refused, and nothing about it was done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SolanaRpcError {
    pub code: i64,
    pub message: String,
}

impl std::fmt::Display for SolanaRpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (code {})", self.message, self.code)
    }
}

impl std::error::Error for SolanaRpcError {}

/// One account as the node holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountData {
    pub lamports: u64,
    pub owner: Address,
    pub data: Vec<u8>,
}

/// A signature's status, once the node has seen it land.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureStatus {
    pub slot: u64,
    /// `Some` when the transaction landed and failed.
    pub err: Option<Value>,
    /// `processed`, `confirmed` or `finalized`.
    pub confirmation: String,
}

/// A token balance in a transaction's meta.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenBalance {
    /// The account's index in the transaction's account keys.
    pub account_index: usize,
    pub mint: Address,
    pub owner: Option<Address>,
    pub amount: u64,
}

/// What a landed transaction's meta says: everything an adapter reads its
/// outcome and its cost from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxMeta {
    pub slot: u64,
    /// `Some` when the transaction failed.
    pub err: Option<Value>,
    pub fee_lamports: u64,
    pub units_consumed: u64,
    /// The static keys, then the lookup tables' writable and read-only
    /// keys, in the order the balances below index them.
    pub account_keys: Vec<Address>,
    pub pre_balances: Vec<u64>,
    pub post_balances: Vec<u64>,
    pub pre_token_balances: Vec<TokenBalance>,
    pub post_token_balances: Vec<TokenBalance>,
    pub log_messages: Vec<String>,
}

impl TxMeta {
    /// The lamports `account` gained (positive) or lost in the transaction.
    pub fn lamports_delta(&self, account: &Address) -> Option<i128> {
        let i = self.account_keys.iter().position(|k| k == account)?;
        Some(i128::from(*self.post_balances.get(i)?) - i128::from(*self.pre_balances.get(i)?))
    }

    /// The token amount `account` held before and after, zero where it held
    /// none (an account created or closed by the transaction).
    pub fn token_amounts(&self, account: &Address) -> (u64, u64) {
        let amount = |balances: &[TokenBalance]| {
            balances
                .iter()
                .find(|b| self.account_keys.get(b.account_index) == Some(account))
                .map_or(0, |b| b.amount)
        };
        (
            amount(&self.pre_token_balances),
            amount(&self.post_token_balances),
        )
    }
}

/// What a dry run reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Simulation {
    pub slot: u64,
    /// `Some` when the transaction would fail.
    pub err: Option<Value>,
    pub units_consumed: u64,
    pub logs: Vec<String>,
    /// The accounts asked for, as they would be after the transaction.
    pub accounts: Vec<Option<AccountData>>,
}

#[derive(Debug, Clone)]
pub struct SolanaRpc {
    url: String,
    http: reqwest::Client,
}

impl SolanaRpc {
    pub fn new(url: impl Into<String>) -> Self {
        Self::with_request_timeout(url, DEFAULT_REQUEST_TIMEOUT)
    }

    /// As [`SolanaRpc::new`], with a different limit on how long one
    /// request may take.
    pub fn with_request_timeout(url: impl Into<String>, timeout: Duration) -> Self {
        Self {
            url: url.into(),
            http: reqwest::Client::builder()
                .timeout(timeout)
                .build()
                .expect("a reqwest client with only a timeout set always builds"),
        }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// One JSON-RPC call. A node's error is a [`SolanaRpcError`] inside the
    /// `anyhow::Error`; a transport failure is a plain error.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let response: Value = self
            .http
            .post(&self.url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("{method} to {}", self.url))?
            .json()
            .await
            .with_context(|| format!("decoding {method}'s answer"))?;
        if let Some(error) = response.get("error") {
            return Err(SolanaRpcError {
                code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
                message: error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("no message")
                    .to_string(),
            }
            .into());
        }
        response
            .get("result")
            .cloned()
            .ok_or_else(|| anyhow!("{method} answered with neither a result nor an error"))
    }

    pub async fn genesis_hash(&self) -> Result<[u8; 32]> {
        let hash = self.call("getGenesisHash", json!([])).await?;
        decode_32(hash.as_str().ok_or_else(|| anyhow!("getGenesisHash: {hash}"))?)
    }

    /// A blockhash at `confirmed`, and the last block height it is valid at.
    pub async fn latest_blockhash(&self) -> Result<([u8; 32], u64)> {
        let result = self
            .call("getLatestBlockhash", json!([{ "commitment": "confirmed" }]))
            .await?;
        let value = &result["value"];
        let hash = value["blockhash"]
            .as_str()
            .ok_or_else(|| anyhow!("getLatestBlockhash: {result}"))?;
        let last = value["lastValidBlockHeight"]
            .as_u64()
            .ok_or_else(|| anyhow!("getLatestBlockhash: {result}"))?;
        Ok((decode_32(hash)?, last))
    }

    /// The block height at `confirmed`.
    pub async fn block_height(&self) -> Result<u64> {
        let height = self
            .call("getBlockHeight", json!([{ "commitment": "confirmed" }]))
            .await?;
        height
            .as_u64()
            .ok_or_else(|| anyhow!("getBlockHeight: {height}"))
    }

    /// The current slot at `confirmed`.
    pub async fn slot(&self) -> Result<u64> {
        let slot = self.call("getSlot", json!([{ "commitment": "confirmed" }])).await?;
        slot.as_u64().ok_or_else(|| anyhow!("getSlot: {slot}"))
    }

    /// Sends a signed transaction's wire bytes; its first signature back.
    /// `skip_preflight` sends it even when a dry run says it fails, so a
    /// fork run observes the failure as an outcome.
    pub async fn send_transaction(&self, wire: &[u8], skip_preflight: bool) -> Result<String> {
        let encoded = base64::engine::general_purpose::STANDARD.encode(wire);
        let signature = self
            .call(
                "sendTransaction",
                json!([encoded, {
                    "encoding": "base64",
                    "skipPreflight": skip_preflight,
                    "preflightCommitment": "confirmed",
                    "maxRetries": 0,
                }]),
            )
            .await?;
        signature
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| anyhow!("sendTransaction: {signature}"))
    }

    /// The statuses of `signatures`, `None` for one the node has not seen.
    pub async fn signature_statuses(&self, signatures: &[String]) -> Result<Vec<Option<SignatureStatus>>> {
        let result = self
            .call(
                "getSignatureStatuses",
                json!([signatures, { "searchTransactionHistory": true }]),
            )
            .await?;
        let values = result["value"]
            .as_array()
            .ok_or_else(|| anyhow!("getSignatureStatuses: {result}"))?;
        values
            .iter()
            .map(|v| {
                if v.is_null() {
                    return Ok(None);
                }
                Ok(Some(SignatureStatus {
                    slot: v["slot"]
                        .as_u64()
                        .ok_or_else(|| anyhow!("a status with no slot: {v}"))?,
                    err: v.get("err").filter(|e| !e.is_null()).cloned(),
                    confirmation: v["confirmationStatus"]
                        .as_str()
                        .unwrap_or("processed")
                        .to_string(),
                }))
            })
            .collect()
    }

    /// A landed transaction's meta, `None` while the node has none.
    pub async fn transaction(&self, signature: &str) -> Result<Option<TxMeta>> {
        let tx = self
            .call(
                "getTransaction",
                json!([signature, {
                    "encoding": "json",
                    "commitment": "confirmed",
                    "maxSupportedTransactionVersion": 0,
                }]),
            )
            .await?;
        if tx.is_null() {
            return Ok(None);
        }
        let meta = &tx["meta"];
        let addresses = |v: &Value| -> Result<Vec<Address>> {
            v.as_array()
                .map(|keys| {
                    keys.iter()
                        .map(|k| parse_address(k.as_str().unwrap_or_default()))
                        .collect()
                })
                .unwrap_or_else(|| Ok(Vec::new()))
        };
        let mut account_keys = addresses(&tx["transaction"]["message"]["accountKeys"])?;
        account_keys.extend(addresses(&meta["loadedAddresses"]["writable"])?);
        account_keys.extend(addresses(&meta["loadedAddresses"]["readonly"])?);
        let u64s = |v: &Value| -> Vec<u64> {
            v.as_array()
                .map(|a| a.iter().filter_map(Value::as_u64).collect())
                .unwrap_or_default()
        };
        let token_balances = |v: &Value| -> Result<Vec<TokenBalance>> {
            v.as_array()
                .map(|a| a.iter().map(token_balance).collect())
                .unwrap_or_else(|| Ok(Vec::new()))
        };
        Ok(Some(TxMeta {
            slot: tx["slot"].as_u64().unwrap_or(0),
            err: meta.get("err").filter(|e| !e.is_null()).cloned(),
            fee_lamports: meta["fee"].as_u64().unwrap_or(0),
            units_consumed: meta["computeUnitsConsumed"].as_u64().unwrap_or(0),
            account_keys,
            pre_balances: u64s(&meta["preBalances"]),
            post_balances: u64s(&meta["postBalances"]),
            pre_token_balances: token_balances(&meta["preTokenBalances"])?,
            post_token_balances: token_balances(&meta["postTokenBalances"])?,
            log_messages: meta["logMessages"]
                .as_array()
                .map(|a| a.iter().filter_map(|l| l.as_str().map(str::to_string)).collect())
                .unwrap_or_default(),
        }))
    }

    /// `accounts` at `confirmed`, `None` for one that does not exist.
    pub async fn multiple_accounts(&self, accounts: &[Address]) -> Result<Vec<Option<AccountData>>> {
        let keys: Vec<String> = accounts.iter().map(|a| a.to_string()).collect();
        let mut out = Vec::with_capacity(keys.len());
        // The node's limit is 100 keys a call.
        for chunk in keys.chunks(100) {
            let result = self
                .call(
                    "getMultipleAccounts",
                    json!([chunk, { "encoding": "base64", "commitment": "confirmed" }]),
                )
                .await?;
            let values = result["value"]
                .as_array()
                .ok_or_else(|| anyhow!("getMultipleAccounts: {result}"))?;
            for v in values {
                out.push(account_data(v)?);
            }
        }
        Ok(out)
    }

    /// A dry run of a transaction's wire bytes. `replace_recent_blockhash`
    /// runs it against the node's newest blockhash; `sig_verify: false`
    /// runs an unsigned one. `accounts` are returned as they would be after.
    pub async fn simulate_transaction(
        &self,
        wire: &[u8],
        sig_verify: bool,
        replace_recent_blockhash: bool,
        accounts: &[Address],
    ) -> Result<Simulation> {
        let encoded = base64::engine::general_purpose::STANDARD.encode(wire);
        let keys: Vec<String> = accounts.iter().map(|a| a.to_string()).collect();
        let result = self
            .call(
                "simulateTransaction",
                json!([encoded, {
                    "encoding": "base64",
                    "sigVerify": sig_verify,
                    "replaceRecentBlockhash": replace_recent_blockhash,
                    "commitment": "confirmed",
                    "accounts": { "encoding": "base64", "addresses": keys },
                }]),
            )
            .await?;
        let value = &result["value"];
        let accounts = value["accounts"]
            .as_array()
            .map(|a| a.iter().map(account_data).collect::<Result<Vec<_>>>())
            .transpose()?
            .unwrap_or_default();
        Ok(Simulation {
            slot: result["context"]["slot"].as_u64().unwrap_or(0),
            err: value.get("err").filter(|e| !e.is_null()).cloned(),
            units_consumed: value["unitsConsumed"].as_u64().unwrap_or(0),
            logs: value["logs"]
                .as_array()
                .map(|a| a.iter().filter_map(|l| l.as_str().map(str::to_string)).collect())
                .unwrap_or_default(),
            accounts,
        })
    }

    /// Whether the node is a Surfpool fork: it answers `surfnet_getSurfnetInfo`,
    /// which a real cluster does not serve.
    pub async fn is_surfnet(&self) -> bool {
        self.call("surfnet_getSurfnetInfo", json!([])).await.is_ok()
    }

    /// Surfpool's cheatcode: sets `account`'s lamports. Only a fork serves it.
    pub async fn surfnet_set_lamports(&self, account: &Address, lamports: u64) -> Result<()> {
        self.call(
            "surfnet_setAccount",
            json!([account.to_string(), { "lamports": lamports }]),
        )
        .await
        .map(|_| ())
    }

    /// Surfpool's cheatcode: creates `owner`'s associated token account for
    /// `mint` under `token_program` if it is missing, and sets its amount.
    /// Only a fork serves it.
    pub async fn surfnet_set_token_account(
        &self,
        owner: &Address,
        mint: &Address,
        token_program: &Address,
        amount: u64,
    ) -> Result<()> {
        self.call(
            "surfnet_setTokenAccount",
            json!([
                owner.to_string(),
                mint.to_string(),
                { "amount": amount },
                token_program.to_string(),
            ]),
        )
        .await
        .map(|_| ())
    }
}

fn account_data(v: &Value) -> Result<Option<AccountData>> {
    if v.is_null() {
        return Ok(None);
    }
    let data = v["data"]
        .as_array()
        .and_then(|d| d.first())
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("an account with no base64 data: {v}"))?;
    Ok(Some(AccountData {
        lamports: v["lamports"]
            .as_u64()
            .ok_or_else(|| anyhow!("an account with no lamports: {v}"))?,
        owner: parse_address(v["owner"].as_str().unwrap_or_default())?,
        data: base64::engine::general_purpose::STANDARD
            .decode(data)
            .context("an account's base64 data")?,
    }))
}

fn token_balance(v: &Value) -> Result<TokenBalance> {
    Ok(TokenBalance {
        account_index: v["accountIndex"]
            .as_u64()
            .ok_or_else(|| anyhow!("a token balance with no account index: {v}"))?
            as usize,
        mint: parse_address(v["mint"].as_str().unwrap_or_default())?,
        owner: v["owner"].as_str().map(parse_address).transpose()?,
        amount: v["uiTokenAmount"]["amount"]
            .as_str()
            .and_then(|a| a.parse().ok())
            .ok_or_else(|| anyhow!("a token balance with no amount: {v}"))?,
    })
}

/// A base58 address.
pub fn parse_address(text: &str) -> Result<Address> {
    text.parse()
        .map_err(|e| anyhow!("{text:?} is not a base58 address: {e:?}"))
}

/// A base58 32-byte value (a hash or an address).
fn decode_32(text: &str) -> Result<[u8; 32]> {
    let address = parse_address(text)?;
    Ok(address.to_bytes())
}

/// `bytes` as an address, refused unless it is 32 bytes long.
pub fn address_from_slice(bytes: &[u8]) -> Result<Address> {
    let array = <[u8; 32]>::try_from(bytes)
        .map_err(|_| anyhow!("a Solana address is 32 bytes, got {}", bytes.len()))?;
    if array == [0; 32] {
        bail!("the all-zero key is not an address anything can own");
    }
    Ok(Address::new_from_array(array))
}
