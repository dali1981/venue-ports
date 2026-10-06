//! ERC-20 reads, calldata and storage layout, shared by every EVM adapter
//! (`SPEC.md` §5, §8): selectors, `balanceOf`/`allowance` reads, `approve`
//! calldata, `Transfer` log decoding, and the storage-slot probing that
//! lets `EvmSimulated` override a balance in `eth_call` and a fork sender
//! write one with `anvil_setStorageAt`.
//!
//! Neither storage layout is standardised, so both slots are probed for per
//! token the first time they're needed, then cached. This is the one prober:
//! every candidate base (the first [`MAX_PROBE_SLOTS`] slots, where plain and
//! inherited layouts put a mapping, and the namespace OpenZeppelin v5's
//! upgradeable ERC-20 keeps its state under, [`OZ_ERC20_NAMESPACE`]) is
//! written with a sentinel of its own in one `eth_call` state override, and
//! the sentinel the token reads back names the base. One request finds the
//! layout or rules every candidate out; a node that did not answer has said
//! nothing about the layout, and that is an error, not "no candidate".

use crate::evm::rpc::{first_word, format_u256, pad_address, BlockTag, EvmRpc, RpcLog};
use alloy_primitives::{b256, keccak256, Address, B256, U256};
use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::Mutex;

/// `keccak256("balanceOf(address)")[..4]` — a fixed, public ERC-20 ABI
/// selector, not computed at runtime.
pub const BALANCE_OF_SELECTOR: [u8; 4] = [0x70, 0xa0, 0x82, 0x31];
/// `keccak256("allowance(address,address)")[..4]`.
pub const ALLOWANCE_SELECTOR: [u8; 4] = [0xdd, 0x62, 0xed, 0x3e];
/// `keccak256("approve(address,uint256)")[..4]`.
pub const APPROVE_SELECTOR: [u8; 4] = [0x09, 0x5e, 0xa7, 0xb3];

/// How many low storage slots to try as a mapping's base before giving up
/// on a plain layout. Real contracts almost always place `balanceOf` and
/// `allowance` in one of the first few storage slots (inherited layouts a
/// few more); this is generous headroom, not a limit callers need to think
/// about. Tokens that keep their state elsewhere are the namespaces below.
pub const MAX_PROBE_SLOTS: u64 = 24;

/// Where OpenZeppelin v5's upgradeable ERC-20 (`ERC20Upgradeable`) keeps its
/// state: an ERC-7201 namespace, `erc7201:openzeppelin.storage.ERC20`, so
/// `keccak256(abi.encode(uint256(keccak256("openzeppelin.storage.ERC20")) - 1))
/// & ~bytes32(uint256(0xff))`, not a low slot. MORPHO on Base is one such
/// token. The first field of its `ERC20Storage` struct is the balances
/// mapping, so this is the balances' base.
pub const OZ_ERC20_NAMESPACE: SlotBase = SlotBase(b256!(
    "52c63247e1f47db19d5ce0460030c497f067ca4cebf71ba98eeadabe20bace00"
));

/// The allowances' base under [`OZ_ERC20_NAMESPACE`]: the second field of
/// the `ERC20Storage` struct, so the namespace plus one.
pub const OZ_ERC20_ALLOWANCES: SlotBase = SlotBase(b256!(
    "52c63247e1f47db19d5ce0460030c497f067ca4cebf71ba98eeadabe20bace01"
));

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

/// The slot a `mapping(address => …)` is declared at: a low slot index in a
/// contract's own layout (`0u64.into()`), or the 32-byte word of a
/// namespaced layout ([`OZ_ERC20_NAMESPACE`]). Both are a slot word, and a
/// mapping's entries are hashed from it the same way. It is a type of its
/// own, and not a `B256`, so that the slot of an entry (what the finders
/// return) cannot be passed where the slot of the mapping is meant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SlotBase(B256);

impl SlotBase {
    /// The slot word itself.
    pub fn word(self) -> B256 {
        self.0
    }
}

impl From<u64> for SlotBase {
    fn from(index: u64) -> Self {
        Self(B256::from(U256::from(index)))
    }
}

/// The storage slot for `mapping(address => T)[key]` declared at `base`, per
/// Solidity's standard storage layout: `keccak256(pad(key) ++ base)`. A low
/// slot index (`mapping_slot(key, 0)`) or a namespace
/// (`mapping_slot(key, OZ_ERC20_NAMESPACE)`) both work.
pub fn mapping_slot(key: Address, base: impl Into<SlotBase>) -> B256 {
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(&pad_address(key));
    buf[32..].copy_from_slice(base.into().0.as_slice());
    keccak256(buf)
}

/// The storage slot for `mapping(address => mapping(address => T))[owner][spender]`
/// declared at `base`: `keccak256(pad(spender) ++ keccak256(pad(owner) ++ base))`.
pub fn allowance_slot(owner: Address, spender: Address, base: impl Into<SlotBase>) -> B256 {
    let outer_slot = mapping_slot(owner, base);
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(&pad_address(spender));
    buf[32..].copy_from_slice(outer_slot.as_slice());
    keccak256(buf)
}

/// A distinctive value vanishingly unlikely to equal any real balance or
/// allowance, used to detect which candidate storage slot an override
/// actually landed on. Candidate `i` of a probe is written `probe_marker() +
/// i`, so what the token reads back names the candidate.
pub fn probe_marker() -> U256 {
    U256::from(0xdead_beef_dead_beef_u64)
}

/// The layouts found, per token, so each is probed once. A layout belongs to
/// the token, not to whoever held a balance or was approved when it was found.
#[derive(Default)]
pub struct SlotCache {
    balance: Mutex<HashMap<Address, SlotBase>>,
    allowance: Mutex<HashMap<Address, SlotBase>>,
}

impl SlotCache {
    /// Where `token`'s balances mapping is declared, if it has been found.
    pub fn balance_base(&self, token: Address) -> Option<SlotBase> {
        self.balance.lock().unwrap().get(&token).copied()
    }

    /// Where `token`'s allowances mapping is declared, if it has been found.
    pub fn allowance_base(&self, token: Address) -> Option<SlotBase> {
        self.allowance.lock().unwrap().get(&token).copied()
    }
}

/// Every base a token's balances mapping may be declared at: the first
/// [`MAX_PROBE_SLOTS`] slots, then OpenZeppelin's namespace.
fn balance_bases() -> Vec<SlotBase> {
    (0..MAX_PROBE_SLOTS)
        .map(SlotBase::from)
        .chain([OZ_ERC20_NAMESPACE])
        .collect()
}

/// Every base a token's allowances mapping may be declared at: the first
/// [`MAX_PROBE_SLOTS`] slots, then the allowances' place under OpenZeppelin's
/// namespace.
fn allowance_bases() -> Vec<SlotBase> {
    (0..MAX_PROBE_SLOTS)
        .map(SlotBase::from)
        .chain([OZ_ERC20_ALLOWANCES])
        .collect()
}

/// The slot of `holder`'s balance in `token`, probed once per token and its
/// layout cached thereafter.
pub async fn find_balance_slot(
    rpc: &EvmRpc,
    cache: &SlotCache,
    token: Address,
    holder: Address,
    block: BlockTag,
) -> Result<B256> {
    if let Some(base) = cache.balance_base(token) {
        return Ok(mapping_slot(holder, base));
    }
    let candidates = balance_bases()
        .into_iter()
        .map(|base| (base, mapping_slot(holder, base)))
        .collect::<Vec<_>>();
    let Some(base) = probe(rpc, token, &balance_of_calldata(holder), &candidates, block)
        .await
        .with_context(|| format!("probing {token}'s `balanceOf` storage slot"))?
    else {
        bail!(
            "could not find `balanceOf` storage slot for token {token} after probing the first \
             {MAX_PROBE_SLOTS} slots and OpenZeppelin's ERC-7201 namespace"
        );
    };
    cache.balance.lock().unwrap().insert(token, base);
    Ok(mapping_slot(holder, base))
}

/// The slot of `owner`'s allowance to `spender` in `token`, probed once per
/// token and its layout cached thereafter.
pub async fn find_allowance_slot(
    rpc: &EvmRpc,
    cache: &SlotCache,
    token: Address,
    owner: Address,
    spender: Address,
    block: BlockTag,
) -> Result<B256> {
    if let Some(base) = cache.allowance_base(token) {
        return Ok(allowance_slot(owner, spender, base));
    }
    let candidates = allowance_bases()
        .into_iter()
        .map(|base| (base, allowance_slot(owner, spender, base)))
        .collect::<Vec<_>>();
    let Some(base) = probe(
        rpc,
        token,
        &allowance_calldata(owner, spender),
        &candidates,
        block,
    )
    .await
    .with_context(|| format!("probing {token}'s `allowance` storage slot"))?
    else {
        bail!(
            "could not find `allowance` storage slot for token {token}, spender {spender} \
             after probing the first {MAX_PROBE_SLOTS} slots and OpenZeppelin's ERC-7201 \
             namespace"
        );
    };
    cache.allowance.lock().unwrap().insert(token, base);
    Ok(allowance_slot(owner, spender, base))
}

/// Which candidate base `token` keeps the mapping `calldata` reads at, in one
/// `eth_call`: every candidate's slot is written with its own sentinel,
/// `probe_marker() + i`, and the value the view call reads back names the
/// slot it read. A value that is none of them means no candidate is the
/// mapping (`None`). A node that did not answer is an error: it has said
/// nothing about the layout.
async fn probe(
    rpc: &EvmRpc,
    token: Address,
    calldata: &[u8],
    candidates: &[(SlotBase, B256)],
    block: BlockTag,
) -> Result<Option<SlotBase>> {
    let diff: Map<String, Value> = candidates
        .iter()
        .enumerate()
        .map(|(i, (_, slot))| {
            (
                slot.to_string(),
                json!(format_u256(probe_marker() + U256::from(i))),
            )
        })
        .collect();
    let overrides = json!({ token.to_string(): { "stateDiff": diff } });
    let data = rpc
        .eth_call(token, calldata, None, block, Some(&overrides))
        .await?;
    let Ok(read) = first_word(&data) else {
        return Ok(None);
    };
    Ok(read
        .checked_sub(probe_marker())
        .and_then(|i| usize::try_from(i).ok())
        .and_then(|i| candidates.get(i))
        .map(|(base, _)| *base))
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
    /// stands in for a token whose mapping entry is at `expected_slot`: it
    /// reads back whatever the request's overrides wrote there, and zero
    /// when they wrote nothing there.
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
                let value = params
                    .get(2)
                    .and_then(|overrides| overrides.as_object())
                    .and_then(|tokens| tokens.values().next())
                    .and_then(|token_overrides| token_overrides.get("stateDiff"))
                    .and_then(|diff| diff.get(expected_slot.to_string()))
                    .and_then(|written| written.as_str())
                    .map_or_else(|| format_u256(U256::ZERO), str::to_string);
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "jsonrpc": "2.0", "id": 1, "result": value }))
            })
            .mount(server)
            .await;
    }

    const HOLDER: [u8; 20] = [0x11; 20];
    const SPENDER: [u8; 20] = [0x22; 20];

    /// Every request of `method` the mock node received, as its JSON body.
    async fn requests(server: &MockServer, method: &str) -> Vec<Value> {
        server
            .received_requests()
            .await
            .expect("the mock records its requests")
            .iter()
            .map(|r| r.body_json::<Value>().unwrap())
            .filter(|body| body["method"] == method)
            .collect()
    }

    /// Where the namespace is, from the ERC-7201 formula itself (EIP-7201:
    /// `keccak256(abi.encode(uint256(keccak256(id)) - 1)) & ~bytes32(uint256(0xff))`)
    /// and not from a constant: the constant this crate carries must be what
    /// OpenZeppelin's own definition gives.
    #[test]
    fn the_namespace_is_what_erc_7201_derives_from_openzeppelins_id() {
        let id = U256::from_be_bytes(keccak256(b"openzeppelin.storage.ERC20").0);
        let derived = U256::from_be_bytes(keccak256((id - U256::from(1u64)).to_be_bytes::<32>()).0)
            & !U256::from(0xffu64);
        assert_eq!(OZ_ERC20_NAMESPACE.word(), B256::from(derived));
        assert_eq!(
            OZ_ERC20_ALLOWANCES.word(),
            B256::from(derived + U256::from(1u64)),
            "the allowances are the struct's second field, one slot on"
        );
    }

    /// Known answers from OpenZeppelin v5.6.1 and v5.7.0's `ERC20Upgradeable`,
    /// compiled with solc 0.8.28 and run in an EVM (`mint(HOLDER, 1000)`, then
    /// `approve(SPENDER, 77)` from HOLDER): the storage it wrote, which is the
    /// only evidence of its layout that does not come from reading the source.
    /// The balance is under the namespace, the allowance one slot on, hashed
    /// in the same order as a low-slot mapping.
    #[test]
    fn the_slots_openzeppelin_v5_writes_are_where_the_namespace_puts_them() {
        let holder = Address::from(HOLDER);
        let spender = Address::from(SPENDER);
        assert_eq!(
            mapping_slot(holder, OZ_ERC20_NAMESPACE).to_string(),
            "0x1d71aecb7d0688f097f24a3c9e2db1a4bcfddc6f627e76835baf8a6a2195e460",
            "balanceOf(HOLDER) = 1000"
        );
        assert_eq!(
            allowance_slot(holder, spender, OZ_ERC20_ALLOWANCES).to_string(),
            "0x22ea253d555bf20d0b35d31c85822e9ae96ff9f6e7f65035c9f47cb12e91e925",
            "allowance(HOLDER, SPENDER) = 77"
        );
        // The same arguments the other way round are another entry.
        assert_ne!(
            allowance_slot(spender, holder, OZ_ERC20_ALLOWANCES).to_string(),
            "0x22ea253d555bf20d0b35d31c85822e9ae96ff9f6e7f65035c9f47cb12e91e925"
        );
        // A low slot index is the same thing as its own word.
        assert_eq!(
            mapping_slot(holder, 3),
            mapping_slot(holder, SlotBase::from(3u64))
        );
    }

    #[tokio::test]
    async fn finds_balance_slot_at_index_zero_and_caches_it() {
        let server = MockServer::start().await;
        let token = Address::from([0xAA; 20]);
        let holder = Address::from([0xCC; 20]);
        mount_slot_probe(&server, BALANCE_OF_SELECTOR, mapping_slot(holder, 0)).await;

        let rpc = EvmRpc::new(server.uri());
        let cache = SlotCache::default();
        let slot = find_balance_slot(&rpc, &cache, token, holder, BlockTag::Number(1))
            .await
            .expect("slot found");
        assert_eq!(slot, mapping_slot(holder, 0));
        assert_eq!(cache.balance_base(token), Some(SlotBase::from(0u64)));

        // Cached: the layout belongs to the token, so another holder's slot
        // needs no further probe.
        let other = Address::from([0xDD; 20]);
        assert_eq!(
            find_balance_slot(&rpc, &cache, token, other, BlockTag::Number(1))
                .await
                .unwrap(),
            mapping_slot(other, 0)
        );
        assert_eq!(requests(&server, "eth_call").await.len(), 1);
    }

    /// One `eth_call` finds the layout, however many candidates it weighs:
    /// every candidate's slot is written, each with a sentinel of its own, at
    /// the block given.
    #[tokio::test]
    async fn one_call_weighs_every_candidate_with_a_sentinel_of_its_own() {
        let server = MockServer::start().await;
        let token = Address::from([0xAA; 20]);
        let holder = Address::from(HOLDER);
        mount_slot_probe(&server, BALANCE_OF_SELECTOR, mapping_slot(holder, 7)).await;

        let slot = find_balance_slot(
            &EvmRpc::new(server.uri()),
            &SlotCache::default(),
            token,
            holder,
            BlockTag::Pending,
        )
        .await
        .unwrap();
        assert_eq!(slot, mapping_slot(holder, 7));

        let calls = requests(&server, "eth_call").await;
        assert_eq!(calls.len(), 1, "one request, not one per candidate");
        assert_eq!(calls[0]["params"][1], "pending");
        let diff = calls[0]["params"][2][token.to_string()]["stateDiff"]
            .as_object()
            .unwrap();
        assert_eq!(
            diff.len() as u64,
            MAX_PROBE_SLOTS + 1,
            "the low slots and the namespace"
        );
        let sentinels: std::collections::HashSet<_> =
            diff.values().map(|v| v.as_str().unwrap()).collect();
        assert_eq!(sentinels.len(), diff.len(), "no two candidates share one");
        assert_eq!(
            diff[&mapping_slot(holder, OZ_ERC20_NAMESPACE).to_string()],
            format_u256(probe_marker() + U256::from(MAX_PROBE_SLOTS)),
            "the namespace is the last candidate"
        );
    }

    /// The last of the low slots is found, and a token that keeps its
    /// balances in one past them is not.
    #[tokio::test]
    async fn the_last_low_slot_is_found_and_the_one_after_it_is_not() {
        let token = Address::from([0xAA; 20]);
        let holder = Address::from(HOLDER);

        let server = MockServer::start().await;
        let last = MAX_PROBE_SLOTS - 1;
        mount_slot_probe(&server, BALANCE_OF_SELECTOR, mapping_slot(holder, last)).await;
        let found = find_balance_slot(
            &EvmRpc::new(server.uri()),
            &SlotCache::default(),
            token,
            holder,
            BlockTag::Latest,
        )
        .await
        .unwrap();
        assert_eq!(found, mapping_slot(holder, last));

        let server = MockServer::start().await;
        mount_slot_probe(
            &server,
            BALANCE_OF_SELECTOR,
            mapping_slot(holder, MAX_PROBE_SLOTS),
        )
        .await;
        let err = find_balance_slot(
            &EvmRpc::new(server.uri()),
            &SlotCache::default(),
            token,
            holder,
            BlockTag::Latest,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("could not find"), "{err}");
    }

    /// MORPHO on Base: OpenZeppelin v5's upgradeable ERC-20 keeps balances
    /// under the ERC-7201 namespace, which no low slot is.
    #[tokio::test]
    async fn finds_a_balance_kept_under_openzeppelins_namespace() {
        let server = MockServer::start().await;
        let token = Address::from([0xAA; 20]);
        let holder = Address::from(HOLDER);
        mount_slot_probe(
            &server,
            BALANCE_OF_SELECTOR,
            mapping_slot(holder, OZ_ERC20_NAMESPACE),
        )
        .await;

        let cache = SlotCache::default();
        let slot = find_balance_slot(
            &EvmRpc::new(server.uri()),
            &cache,
            token,
            holder,
            BlockTag::Latest,
        )
        .await
        .unwrap();
        assert_eq!(slot, mapping_slot(holder, OZ_ERC20_NAMESPACE));
        assert_eq!(cache.balance_base(token), Some(OZ_ERC20_NAMESPACE));
    }

    /// The allowance is found the same way: in a low slot, or one on from the
    /// namespace, which is where the balance's own namespace is not.
    #[tokio::test]
    async fn finds_an_allowance_in_a_low_slot_or_under_openzeppelins_namespace() {
        let token = Address::from([0xAA; 20]);
        let (owner, spender) = (Address::from(HOLDER), Address::from(SPENDER));

        let server = MockServer::start().await;
        mount_slot_probe(
            &server,
            ALLOWANCE_SELECTOR,
            allowance_slot(owner, spender, OZ_ERC20_ALLOWANCES),
        )
        .await;
        let cache = SlotCache::default();
        let slot = find_allowance_slot(
            &EvmRpc::new(server.uri()),
            &cache,
            token,
            owner,
            spender,
            BlockTag::Latest,
        )
        .await
        .unwrap();
        assert_eq!(slot, allowance_slot(owner, spender, OZ_ERC20_ALLOWANCES));
        assert_eq!(cache.allowance_base(token), Some(OZ_ERC20_ALLOWANCES));
        let calls = requests(&server, "eth_call").await;
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0]["params"][2][token.to_string()]["stateDiff"]
                .as_object()
                .unwrap()
                .len() as u64,
            MAX_PROBE_SLOTS + 1
        );

        // The layout is the token's, so another spender is not probed again.
        let other = Address::from([0x33; 20]);
        assert_eq!(
            find_allowance_slot(
                &EvmRpc::new(server.uri()),
                &cache,
                token,
                owner,
                other,
                BlockTag::Latest,
            )
            .await
            .unwrap(),
            allowance_slot(owner, other, OZ_ERC20_ALLOWANCES)
        );
        assert_eq!(requests(&server, "eth_call").await.len(), 1);

        let server = MockServer::start().await;
        mount_slot_probe(
            &server,
            ALLOWANCE_SELECTOR,
            allowance_slot(owner, spender, 1),
        )
        .await;
        let slot = find_allowance_slot(
            &EvmRpc::new(server.uri()),
            &SlotCache::default(),
            token,
            owner,
            spender,
            BlockTag::Latest,
        )
        .await
        .unwrap();
        assert_eq!(slot, allowance_slot(owner, spender, 1));
    }

    /// What the token reads back that is none of the sentinels (a real
    /// balance, or a view that is not a mapping read) rules every candidate
    /// out, and says so.
    #[tokio::test]
    async fn a_read_that_is_no_sentinel_finds_no_layout() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0", "id": 1, "result": format_u256(U256::from(1_000_000u64)),
            })))
            .mount(&server)
            .await;
        let err = find_allowance_slot(
            &EvmRpc::new(server.uri()),
            &SlotCache::default(),
            Address::from([0xAA; 20]),
            Address::from(HOLDER),
            Address::from(SPENDER),
            BlockTag::Latest,
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("could not find `allowance`")
                && err.to_string().contains("ERC-7201"),
            "{err}"
        );
    }

    /// A node that did not answer has said nothing about the layout: an
    /// error that names the probe, never "no candidate", and nothing cached.
    #[tokio::test]
    async fn a_node_that_does_not_answer_the_probe_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500).set_body_string("upstream down"))
            .mount(&server)
            .await;
        let token = Address::from([0xAA; 20]);
        let cache = SlotCache::default();
        let err = find_balance_slot(
            &EvmRpc::new(server.uri()),
            &cache,
            token,
            Address::from(HOLDER),
            BlockTag::Latest,
        )
        .await
        .unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains("probing") && !text.contains("could not find"),
            "{text}"
        );
        assert_eq!(cache.balance_base(token), None);
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
