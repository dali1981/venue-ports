//! ERC-20 reads, calldata and storage layout, shared by every EVM adapter
//! (`SPEC.md` §5, §8): selectors, `balanceOf`/`allowance` reads, `approve`
//! calldata, `Transfer` log decoding, and the storage-slot probing that
//! lets `EvmSimulated` override a balance in `eth_call` and a fork sender
//! write one with `anvil_setStorageAt`.
//!
//! Neither storage layout is standardised, so both slots are probed for per
//! token the first time they're needed, then cached: a distinctive marker is
//! written into each candidate slot with an `eth_call` state override, and
//! the candidate whose read-back returns the marker is the one.

use crate::evm::rpc::{first_word, format_u256, pad_address, BlockTag, EvmRpc, RpcLog};
use alloy_primitives::{keccak256, Address, B256, U256};
use anyhow::{bail, Result};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Mutex;

/// `keccak256("balanceOf(address)")[..4]` — a fixed, public ERC-20 ABI
/// selector, not computed at runtime.
pub const BALANCE_OF_SELECTOR: [u8; 4] = [0x70, 0xa0, 0x82, 0x31];
/// `keccak256("allowance(address,address)")[..4]`.
pub const ALLOWANCE_SELECTOR: [u8; 4] = [0xdd, 0x62, 0xed, 0x3e];
/// `keccak256("approve(address,uint256)")[..4]`.
pub const APPROVE_SELECTOR: [u8; 4] = [0x09, 0x5e, 0xa7, 0xb3];

/// How many candidate storage-slot indices to probe before giving up.
/// Real contracts almost always place `balanceOf`/`allowance` in one of
/// the first few storage slots; this is generous headroom, not a limit
/// callers need to think about.
pub const MAX_PROBE_SLOTS: u64 = 16;

/// `keccak256("Transfer(address,address,uint256)")`, computed rather than
/// hardcoded. ERC-721's `Transfer` has the same signature, and so the same
/// topic; the two are told apart by whether the third argument is indexed.
pub fn transfer_topic() -> B256 {
    keccak256(b"Transfer(address,address,uint256)")
}

pub fn balance_of_calldata(holder: Address) -> Vec<u8> {
    let mut data = BALANCE_OF_SELECTOR.to_vec();
    data.extend_from_slice(&pad_address(holder));
    data
}

pub fn allowance_calldata(owner: Address, spender: Address) -> Vec<u8> {
    let mut data = ALLOWANCE_SELECTOR.to_vec();
    data.extend_from_slice(&pad_address(owner));
    data.extend_from_slice(&pad_address(spender));
    data
}

pub fn approve_calldata(spender: Address, amount: U256) -> Vec<u8> {
    let mut data = APPROVE_SELECTOR.to_vec();
    data.extend_from_slice(&pad_address(spender));
    data.extend_from_slice(&amount.to_be_bytes::<32>());
    data
}

pub async fn balance_of(
    rpc: &EvmRpc,
    token: Address,
    holder: Address,
    block: BlockTag,
) -> Result<U256> {
    let data = rpc
        .eth_call(token, &balance_of_calldata(holder), None, block, None)
        .await?;
    first_word(&data)
}

pub async fn allowance(
    rpc: &EvmRpc,
    token: Address,
    owner: Address,
    spender: Address,
    block: BlockTag,
) -> Result<U256> {
    let data = rpc
        .eth_call(
            token,
            &allowance_calldata(owner, spender),
            None,
            block,
            None,
        )
        .await?;
    first_word(&data)
}

/// One ERC-20 `Transfer` event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transfer {
    pub token: Address,
    pub from: Address,
    pub to: Address,
    pub value: U256,
}

/// Every ERC-20 `Transfer` in `logs`, in order. An ERC-721 `Transfer` (same
/// topic, but four topics because its token id is indexed) is skipped.
pub fn transfers(logs: &[RpcLog]) -> Vec<Transfer> {
    let topic = transfer_topic();
    logs.iter()
        .filter(|log| log.topics.len() == 3 && log.topics[0] == topic && log.data.len() >= 32)
        .map(|log| Transfer {
            token: log.address,
            from: Address::from_slice(&log.topics[1][12..]),
            to: Address::from_slice(&log.topics[2][12..]),
            value: U256::from_be_slice(&log.data[..32]),
        })
        .collect()
}

/// The storage slot for `mapping(address => T)[key]` at slot index
/// `index`, per Solidity's standard storage layout: `keccak256(pad(key) ++
/// pad(index))`.
pub fn mapping_slot(key: Address, index: u64) -> B256 {
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(&pad_address(key));
    buf[32..].copy_from_slice(&U256::from(index).to_be_bytes::<32>());
    keccak256(buf)
}

/// The storage slot for `mapping(address => mapping(address => T))[owner][spender]`
/// at slot index `index`: `keccak256(pad(spender) ++ keccak256(pad(owner) ++ pad(index)))`.
pub fn allowance_slot(owner: Address, spender: Address, index: u64) -> B256 {
    let outer_slot = mapping_slot(owner, index);
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(&pad_address(spender));
    buf[32..].copy_from_slice(outer_slot.as_slice());
    keccak256(buf)
}

/// A distinctive value vanishingly unlikely to equal any real balance or
/// allowance, used to detect which candidate storage slot an override
/// actually landed on.
pub fn probe_marker() -> U256 {
    U256::from(0xdead_beef_dead_beef_u64)
}

/// Slot indices already found, per token (and per spender, for
/// allowances), so each is probed once.
#[derive(Default)]
pub struct SlotCache {
    balance: Mutex<HashMap<Address, u64>>,
    allowance: Mutex<HashMap<(Address, Address), u64>>,
}

impl SlotCache {
    pub fn balance_index(&self, token: Address) -> Option<u64> {
        self.balance.lock().unwrap().get(&token).copied()
    }

    pub fn allowance_index(&self, token: Address, spender: Address) -> Option<u64> {
        self.allowance
            .lock()
            .unwrap()
            .get(&(token, spender))
            .copied()
    }
}

/// The slot index of `token`'s `balanceOf` mapping, probed once per token
/// and cached thereafter.
pub async fn find_balance_slot(
    rpc: &EvmRpc,
    cache: &SlotCache,
    token: Address,
    holder: Address,
    block: BlockTag,
) -> Result<u64> {
    if let Some(index) = cache.balance_index(token) {
        return Ok(index);
    }
    let calldata = balance_of_calldata(holder);
    for index in 0..MAX_PROBE_SLOTS {
        let slot = mapping_slot(holder, index);
        if probe(rpc, token, &calldata, slot, block).await? {
            cache.balance.lock().unwrap().insert(token, index);
            return Ok(index);
        }
    }
    bail!("could not find `balanceOf` storage slot for token {token} after probing {MAX_PROBE_SLOTS} candidates");
}

/// The slot index of `token`'s `allowance` mapping, probed once per
/// (token, spender) pair and cached thereafter.
pub async fn find_allowance_slot(
    rpc: &EvmRpc,
    cache: &SlotCache,
    token: Address,
    owner: Address,
    spender: Address,
    block: BlockTag,
) -> Result<u64> {
    if let Some(index) = cache.allowance_index(token, spender) {
        return Ok(index);
    }
    let calldata = allowance_calldata(owner, spender);
    for index in 0..MAX_PROBE_SLOTS {
        let slot = allowance_slot(owner, spender, index);
        if probe(rpc, token, &calldata, slot, block).await? {
            cache
                .allowance
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

/// Whether writing the marker into `slot` makes `calldata`'s view call on
/// `token` return the marker.
async fn probe(
    rpc: &EvmRpc,
    token: Address,
    calldata: &[u8],
    slot: B256,
    block: BlockTag,
) -> Result<bool> {
    let overrides = json!({
        token.to_string(): { "stateDiff": { slot.to_string(): format_u256(probe_marker()) } }
    });
    let data = rpc
        .eth_call(token, calldata, None, block, Some(&overrides))
        .await?;
    Ok(first_word(&data).ok() == Some(probe_marker()))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::Value;
    use wiremock::matchers::{body_partial_json, body_string_contains, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Mounts a mock that answers `eth_call` requests whose calldata
    /// starts with `selector` — scoped to that selector in the matcher
    /// itself, not just the response logic, so probes for two different
    /// mappings mounted on the same server never shadow each other. It
    /// returns the marker value the moment the request's overrides touch
    /// `expected_slot`, and zero otherwise — standing in for "this is the
    /// one real storage slot that actually backs the mapping".
    pub(crate) async fn mount_slot_probe(
        server: &MockServer,
        selector: [u8; 4],
        expected_slot: B256,
    ) {
        Mock::given(method("POST"))
            .and(body_partial_json(json!({ "method": "eth_call" })))
            .and(body_string_contains(hex::encode(selector)))
            .respond_with(move |req: &wiremock::Request| {
                let body: Value = req.body_json().unwrap();
                let params = body["params"].as_array().unwrap();
                let touches_expected = params
                    .get(2)
                    .and_then(|overrides| overrides.as_object())
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
        let holder = Address::from([0xCC; 20]);
        mount_slot_probe(&server, BALANCE_OF_SELECTOR, mapping_slot(holder, 0)).await;

        let rpc = EvmRpc::new(server.uri());
        let cache = SlotCache::default();
        let index = find_balance_slot(&rpc, &cache, token, holder, BlockTag::Number(1))
            .await
            .expect("slot found");
        assert_eq!(index, 0);

        // Cached: a second call must not need another probe request. If it
        // did, wiremock would still answer (the mount has no upper bound),
        // so assert the cache directly instead of relying on request counts.
        assert_eq!(cache.balance_index(token), Some(0));
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

        let rpc = EvmRpc::new(server.uri());
        let err = find_balance_slot(
            &rpc,
            &SlotCache::default(),
            Address::from([0xAA; 20]),
            Address::from([0xCC; 20]),
            BlockTag::Number(1),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("could not find"));
    }

    #[test]
    fn transfers_skips_an_erc721_transfer_with_the_same_topic() {
        let token = Address::from([0xAA; 20]);
        let from = Address::from([0x01; 20]);
        let to = Address::from([0x02; 20]);
        let erc20 = RpcLog {
            address: token,
            topics: vec![
                transfer_topic(),
                B256::from(pad_address(from)),
                B256::from(pad_address(to)),
            ],
            data: U256::from(5u64).to_be_bytes::<32>().to_vec(),
        };
        let erc721 = RpcLog {
            topics: vec![
                transfer_topic(),
                B256::from(pad_address(from)),
                B256::from(pad_address(to)),
                B256::from(U256::from(7u64)),
            ],
            data: Vec::new(),
            ..erc20.clone()
        };

        assert_eq!(
            transfers(&[erc721, erc20]),
            vec![Transfer {
                token,
                from,
                to,
                value: U256::from(5u64)
            }]
        );
    }
}
