//! Signing and nonce management for the EVM chain family (`SPEC.md` §5,
//! "The EVM sender"). `EvmSender` is the only thing in the crate that
//! assigns nonces for one wallet on one chain; every adapter that sends
//! from that wallet holds the same `Arc<EvmSender>`, so "one nonce in
//! flight per chain at a time" holds for the wallet, not just for each
//! adapter.
//!
//! **Why a lock over the whole send, not a nonce counter.** The simplest
//! way to guarantee one nonce in flight without a nonce cache that can fall
//! out of sync is to never let a second send start building its transaction
//! until the previous one has fully resolved (landed, reverted, or timed
//! out). `send_and_confirm` holds the sender's lock for exactly that whole
//! sequence, reading a fresh `eth_getTransactionCount(pending)` each time.
//!
//! **Why nothing is sent over a timeout.** After `TxOutcome::TimedOut` the
//! transaction may still be in the mempool. A next send would either queue
//! behind it or, if it was dropped, take its nonce — and the caller would
//! have treated an unknown outcome as settled. `SPEC.md` says a timed-out
//! transaction "must be resolved out of band before it is treated as
//! anything else", so the sender refuses every send until
//! [`EvmSender::resolve`] has cleared it.
//!
//! **One sender per wallet per process.** [`EvmSender::connect`] keeps a
//! process-wide registry of (address, chain_id) pairs and refuses a second
//! sender for the same pair; the entry is removed when the sender drops.
//! Nothing can enforce this across processes: run one process per wallet
//! per chain.

use crate::evm::erc20;
use crate::evm::rpc::{BlockTag, EvmRpc, Receipt, RpcError, RpcLog};
use crate::Provenance;
use alloy_consensus::{SignableTransaction, TxEip1559};
use alloy_primitives::{keccak256, Address, Signature, TxKind, B256, U256};
use anyhow::{bail, Context, Result};
use k256::ecdsa::SigningKey;
use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// One secp256k1 signing key and the address it controls. It signs; it
/// does not send — sending, and the nonce that goes with it, belong to the
/// one [`EvmSender`] built from it.
pub struct Signer {
    key: SigningKey,
    address: Address,
}

impl Signer {
    /// `hex_key` is a raw secp256k1 private key, with or without `0x`.
    pub fn from_private_key_hex(hex_key: &str) -> Result<Self> {
        let trimmed = hex_key.trim_start_matches("0x");
        let bytes = hex::decode(trimmed).context("private key must be valid hex")?;
        let key = SigningKey::from_slice(&bytes)
            .context("private key is not a valid secp256k1 scalar")?;
        let address = address_from_signing_key(&key);
        Ok(Self { key, address })
    }

    pub fn address(&self) -> Address {
        self.address
    }

    /// Signs `tx` and EIP-2718-encodes it, ready for `eth_sendRawTransaction`.
    /// Returns the raw bytes to broadcast and the hash a receipt poll
    /// should look for — computed the same way the signed transaction
    /// itself is, so it can never disagree with what actually gets sent.
    pub fn sign_eip1559(&self, tx: TxEip1559) -> (Vec<u8>, B256) {
        let sig_hash = tx.signature_hash();
        let (sig, recid) = self
            .key
            .sign_prehash_recoverable(sig_hash.as_slice())
            .expect("signing a 32-byte prehash cannot fail");
        let signature: Signature = (sig, recid).into();
        let signed = tx.into_signed(signature);
        let hash = *signed.hash();
        let mut out = Vec::new();
        signed.eip2718_encode(&mut out);
        (out, hash)
    }
}

/// The standard uncompressed-public-key-to-address derivation: drop the
/// SEC1 tag byte, `keccak256` the remaining 64 bytes, keep the low 20.
fn address_from_signing_key(key: &SigningKey) -> Address {
    let point = key.verifying_key().to_encoded_point(false);
    let hash = keccak256(&point.as_bytes()[1..]);
    Address::from_slice(&hash[12..])
}

/// How max fee and priority fee are chosen: `base_fee_multiplier` × the
/// latest base fee + the priority fee, where the priority fee is the node's
/// `eth_maxPriorityFeePerGas`, or `fallback_priority_fee_wei` when the node
/// does not implement it. The multiplier is the headroom ethers.js and viem
/// use by default — enough that a fee spike across a block or two doesn't
/// strand the transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeePolicy {
    pub base_fee_multiplier: u32,
    pub fallback_priority_fee_wei: u128,
}

impl Default for FeePolicy {
    /// Tuned for Ethereum Sepolia: 2 × base fee, and 1.5 gwei when the node
    /// has no `eth_maxPriorityFeePerGas`. Set it per chain.
    fn default() -> Self {
        Self {
            base_fee_multiplier: 2,
            fallback_priority_fee_wei: 1_500_000_000,
        }
    }
}

/// How often a sent transaction's receipt is polled for, and for how long
/// before the send ends [`TxOutcome::TimedOut`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PollSettings {
    pub interval: Duration,
    pub timeout: Duration,
}

impl Default for PollSettings {
    /// A poll every 4 seconds, giving up after 3 minutes — comfortable
    /// headroom over Sepolia's ~12 s blocks.
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(4),
            timeout: Duration::from_secs(180),
        }
    }
}

/// The terminal outcome of sending exactly one transaction. Not `Realised`:
/// an adapter decides what a given transaction's outcome means (an
/// `approve`'s revert is a setup failure, a swap's revert is the caller's
/// outcome).
#[derive(Debug, Clone)]
pub enum TxOutcome {
    Success {
        block: u64,
        tx_hash: B256,
        logs: Vec<RpcLog>,
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

enum Backend {
    Signing(Signer),
}

/// A send that ended `TimedOut`, kept until [`EvmSender::resolve`] clears
/// it. Holds what `resolve` needs to tell a late landing from a drop, and
/// to replay a late revert for its reason.
#[derive(Debug, Clone)]
struct Unresolved {
    tx_hash: B256,
    nonce: u64,
    to: Address,
    calldata: Vec<u8>,
    value: U256,
}

/// The only thing in the crate that assigns nonces for one wallet on one
/// chain. Every adapter that sends from that wallet holds the same `Arc`.
/// See the module docs for the rules it enforces.
pub struct EvmSender {
    rpc: EvmRpc,
    backend: Backend,
    address: Address,
    chain_id: u64,
    fees: FeePolicy,
    poll: Mutex<PollSettings>,
    send_lock: tokio::sync::Mutex<()>,
    unresolved: Mutex<Option<Unresolved>>,
    registered: bool,
}

fn registry() -> &'static Mutex<HashSet<(Address, u64)>> {
    static REGISTRY: OnceLock<Mutex<HashSet<(Address, u64)>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashSet::new()))
}

impl EvmSender {
    /// Checks `eth_chainId` against `chain_id`. Returns an error if this
    /// process already holds an `EvmSender` for the same (address, chain_id).
    pub async fn connect(
        rpc: EvmRpc,
        signer: Signer,
        chain_id: u64,
        fees: FeePolicy,
    ) -> Result<Arc<Self>> {
        let node_chain_id = rpc
            .chain_id()
            .await
            .context("reading the node's eth_chainId")?;
        if node_chain_id != chain_id {
            bail!(
                "the node at {} is on chain {node_chain_id}, not the expected chain {chain_id}",
                rpc.url()
            );
        }
        let address = signer.address();
        if !registry().lock().unwrap().insert((address, chain_id)) {
            bail!(
                "this process already holds an EvmSender for {address} on chain {chain_id} — \
                 share that one (it is an Arc) instead of connecting a second, which would race \
                 it for nonces"
            );
        }
        Ok(Arc::new(Self {
            rpc,
            backend: Backend::Signing(signer),
            address,
            chain_id,
            fees,
            poll: Mutex::new(PollSettings::default()),
            send_lock: tokio::sync::Mutex::new(()),
            unresolved: Mutex::new(None),
            registered: true,
        }))
    }

    pub fn address(&self) -> Address {
        self.address
    }

    pub fn chain_id(&self) -> u64 {
        self.chain_id
    }

    pub fn rpc(&self) -> &EvmRpc {
        &self.rpc
    }

    /// `Landed` for a signing sender: what it sends is real.
    pub fn provenance(&self) -> Provenance {
        match self.backend {
            Backend::Signing(_) => Provenance::Landed,
        }
    }

    /// Changes how receipts are polled for — for a slow chain, or for a
    /// test that forces a `TimedOut` without waiting three minutes.
    pub fn set_poll_settings(&self, poll: PollSettings) {
        *self.poll.lock().unwrap() = poll;
    }

    /// The hash of a send that ended `TimedOut` and is not yet resolved.
    pub fn unresolved(&self) -> Option<B256> {
        self.unresolved.lock().unwrap().as_ref().map(|u| u.tx_hash)
    }

    /// Build, sign, broadcast and poll one transaction to a terminal
    /// outcome, holding the send lock throughout. Returns an error without
    /// sending if an earlier send timed out and has not been resolved.
    ///
    /// An `Err` means nothing was sent, with one narrow exception that is
    /// never an `Err`: a broadcast whose response was lost may still have
    /// reached the node, so it is polled for like any other, and ends
    /// `TimedOut` (and unresolved) if no receipt appears.
    pub async fn send_and_confirm(
        &self,
        to: Address,
        calldata: Vec<u8>,
        value: U256,
    ) -> Result<TxOutcome> {
        let _guard = self.send_lock.lock().await;
        if let Some(hash) = self.unresolved() {
            bail!(
                "transaction {hash} from {} timed out and is unresolved — call resolve() until \
                 it clears before sending anything else",
                self.address
            );
        }

        let (tx_hash, nonce) = match &self.backend {
            Backend::Signing(signer) => {
                self.sign_and_broadcast(signer, to, &calldata, value)
                    .await?
            }
        };

        let poll = *self.poll.lock().unwrap();
        match self.poll_receipt(tx_hash, poll).await {
            Some(receipt) => Ok(self
                .terminal_outcome(tx_hash, receipt, to, &calldata, value)
                .await),
            None => {
                *self.unresolved.lock().unwrap() = Some(Unresolved {
                    tx_hash,
                    nonce,
                    to,
                    calldata,
                    value,
                });
                Ok(TxOutcome::TimedOut { tx_hash })
            }
        }
    }

    /// If the sender's allowance to `spender` is below `amount`, approve
    /// exactly `amount`. Never `U256::MAX`. A revert or timeout here is an
    /// `Err`, a setup failure, and not the caller's outcome.
    pub async fn ensure_allowance(
        &self,
        token: Address,
        spender: Address,
        amount: U256,
    ) -> Result<()> {
        let current = erc20::allowance(&self.rpc, token, self.address, spender, BlockTag::Latest)
            .await
            .with_context(|| format!("reading {token}'s allowance to {spender}"))?;
        if current >= amount {
            return Ok(());
        }
        match self
            .send_and_confirm(token, erc20::approve_calldata(spender, amount), U256::ZERO)
            .await?
        {
            TxOutcome::Success { .. } => Ok(()),
            TxOutcome::Reverted {
                reason, tx_hash, ..
            } => bail!("approve({spender}, {amount}) on {token} reverted (tx {tx_hash}): {reason}"),
            TxOutcome::TimedOut { tx_hash } => bail!(
                "approve({spender}, {amount}) on {token} timed out waiting for a receipt \
                 (tx {tx_hash}) — it may still land; resolve() it before sending again"
            ),
        }
    }

    /// Polls the unresolved hash once. Returns its terminal outcome, which
    /// clears it. Returns `None` if it is still pending. Once the node no
    /// longer knows the hash and the account's `latest` nonce has moved past
    /// it, it was replaced or dropped: that is reported as an error naming
    /// the hash, and the hash is cleared.
    ///
    /// A hash the node does not know whose nonce is still unused stays
    /// unresolved: another node may yet broadcast it. A caller who has
    /// established out of band that it will never land drops every `Arc` to
    /// this sender and connects a new one.
    pub async fn resolve(&self) -> Result<Option<TxOutcome>> {
        let _guard = self.send_lock.lock().await;
        let Some(pending) = self.unresolved.lock().unwrap().clone() else {
            return Ok(None);
        };

        if let Some(receipt) = self.rpc.transaction_receipt(pending.tx_hash).await? {
            let outcome = self
                .terminal_outcome(
                    pending.tx_hash,
                    receipt,
                    pending.to,
                    &pending.calldata,
                    pending.value,
                )
                .await;
            *self.unresolved.lock().unwrap() = None;
            return Ok(Some(outcome));
        }
        if self.rpc.knows_transaction(pending.tx_hash).await? {
            return Ok(None);
        }
        let latest = self
            .rpc
            .transaction_count(self.address, BlockTag::Latest)
            .await?;
        if latest > pending.nonce {
            *self.unresolved.lock().unwrap() = None;
            bail!(
                "transaction {} is no longer known to the node and nonce {} of {} has been used: \
                 it was replaced or dropped, and did not land",
                pending.tx_hash,
                pending.nonce,
                self.address
            );
        }
        Ok(None)
    }

    async fn sign_and_broadcast(
        &self,
        signer: &Signer,
        to: Address,
        calldata: &[u8],
        value: U256,
    ) -> Result<(B256, u64)> {
        let nonce = self
            .rpc
            .transaction_count(self.address, BlockTag::Pending)
            .await?;
        let (priority_fee, max_fee) = self.fees().await?;
        let estimate = self
            .rpc
            .estimate_gas(self.address, to, calldata, value)
            .await?;
        let tx = TxEip1559 {
            chain_id: self.chain_id,
            nonce,
            // 20% headroom: an estimate is a snapshot of current state, and
            // the transaction executes against whatever state exists when
            // it is mined.
            gas_limit: estimate + estimate / 5,
            max_fee_per_gas: max_fee,
            max_priority_fee_per_gas: priority_fee,
            to: TxKind::Call(to),
            value,
            input: calldata.to_vec().into(),
            ..Default::default()
        };
        let (raw, tx_hash) = signer.sign_eip1559(tx);
        if let Err(err) = self.rpc.send_raw_transaction(&raw).await {
            // The node answered and refused it: nothing was sent. Anything
            // else (a dropped connection, a timeout) may have happened after
            // the node accepted it, so it is polled for like a sent one.
            if err.downcast_ref::<RpcError>().is_some() {
                return Err(err.context(format!("broadcasting transaction {tx_hash}")));
            }
        }
        Ok((tx_hash, nonce))
    }

    /// `(max_priority_fee_per_gas, max_fee_per_gas)`, in wei, per the
    /// sender's [`FeePolicy`].
    async fn fees(&self) -> Result<(u128, u128)> {
        let base_fee = self.rpc.base_fee().await?;
        let priority_fee = self
            .rpc
            .max_priority_fee()
            .await
            .unwrap_or(self.fees.fallback_priority_fee_wei);
        let max_fee = base_fee
            .saturating_mul(u128::from(self.fees.base_fee_multiplier))
            .saturating_add(priority_fee);
        Ok((priority_fee, max_fee))
    }

    /// Polls until a receipt appears or `poll.timeout` elapses. A failed
    /// poll is retried rather than returned: the transaction is already
    /// out, so a transient RPC error must not end the send as an error.
    async fn poll_receipt(&self, tx_hash: B256, poll: PollSettings) -> Option<Receipt> {
        let deadline = tokio::time::Instant::now() + poll.timeout;
        loop {
            if let Ok(Some(receipt)) = self.rpc.transaction_receipt(tx_hash).await {
                return Some(receipt);
            }
            if tokio::time::Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(poll.interval).await;
        }
    }

    async fn terminal_outcome(
        &self,
        tx_hash: B256,
        receipt: Receipt,
        to: Address,
        calldata: &[u8],
        value: U256,
    ) -> TxOutcome {
        if receipt.success {
            return TxOutcome::Success {
                block: receipt.block,
                tx_hash,
                logs: receipt.logs,
            };
        }
        let reason = self
            .rpc
            .revert_reason(self.address, to, calldata, value, receipt.block)
            .await;
        TxOutcome::Reverted {
            block: receipt.block,
            tx_hash,
            reason,
        }
    }
}

impl Drop for EvmSender {
    fn drop(&mut self) {
        if self.registered {
            registry()
                .lock()
                .unwrap()
                .remove(&(self.address, self.chain_id));
        }
    }
}

/// A signer with a key no other test in this process uses, so tests that
/// each connect a sender never collide in the registry.
#[cfg(test)]
pub(crate) fn unique_test_signer() -> Signer {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let mut key = [0x11u8; 32];
    key[24..].copy_from_slice(&NEXT.fetch_add(1, Ordering::Relaxed).to_be_bytes());
    Signer::from_private_key_hex(&hex::encode(key)).expect("a valid secp256k1 scalar")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::evm::rpc::{encode_error_string, hex_data};
    use alloy_consensus::Transaction;
    use alloy_eips::eip2718::Decodable2718;
    use serde_json::{json, Value};
    use std::sync::atomic::{AtomicU64, Ordering};
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    pub(crate) const SEPOLIA: u64 = 11_155_111;

    fn ok(result: Value) -> ResponseTemplate {
        ResponseTemplate::new(200)
            .set_body_json(json!({ "jsonrpc": "2.0", "id": 1, "result": result }))
    }

    /// Mounts every RPC method a signing send needs apart from receipts:
    /// chain id, nonce, fees, gas and broadcast. None of these care how many
    /// times they're called.
    pub(crate) async fn mount_send_plumbing(server: &MockServer) {
        for (rpc_method, result) in [
            ("eth_chainId", json!(format!("0x{SEPOLIA:x}"))),
            ("eth_getTransactionCount", json!("0x5")),
            (
                "eth_getBlockByNumber",
                json!({ "baseFeePerGas": "0x3b9aca00" }),
            ),
            ("eth_maxPriorityFeePerGas", json!("0x59682f00")),
            ("eth_estimateGas", json!("0x5208")),
            ("eth_sendRawTransaction", json!("0xdeadbeef")),
            ("eth_blockNumber", json!("0x2a")),
        ] {
            Mock::given(method("POST"))
                .and(body_partial_json(json!({ "method": rpc_method })))
                .respond_with(ok(result))
                .mount(server)
                .await;
        }
    }

    pub(crate) async fn mount_receipt(server: &MockServer, receipt: Value) {
        Mock::given(method("POST"))
            .and(body_partial_json(
                json!({"method": "eth_getTransactionReceipt"}),
            ))
            .respond_with(ok(receipt))
            .mount(server)
            .await;
    }

    pub(crate) async fn connect(server: &MockServer) -> Arc<EvmSender> {
        EvmSender::connect(
            EvmRpc::new(server.uri()),
            unique_test_signer(),
            SEPOLIA,
            FeePolicy::default(),
        )
        .await
        .unwrap()
    }

    fn fast_poll() -> PollSettings {
        PollSettings {
            interval: Duration::from_millis(5),
            timeout: Duration::from_millis(30),
        }
    }

    /// Pure key-derivation math, no network — gated on the same env vars
    /// the real-network `EvmLive` test uses so a real (disposable,
    /// testnet-only) wallet never has to be hardcoded here to prove this
    /// crate's address derivation agrees with whatever tool generated it.
    /// A no-op for every contributor and CI run that hasn't opted in.
    #[test]
    fn derives_the_address_a_real_wallet_reports_for_its_own_key() {
        let (Ok(key_hex), Ok(expected_addr)) = (
            std::env::var("EVM_LIVE_SIGNER_KEY"),
            std::env::var("EVM_LIVE_SENDER_ADDRESS"),
        ) else {
            eprintln!("skipping: EVM_LIVE_SIGNER_KEY / EVM_LIVE_SENDER_ADDRESS not set");
            return;
        };

        let signer = Signer::from_private_key_hex(&key_hex).expect("valid private key");
        let expected: Address = expected_addr
            .parse()
            .expect("EVM_LIVE_SENDER_ADDRESS must be a valid EVM address");
        assert_eq!(signer.address(), expected);
    }

    #[tokio::test]
    async fn a_second_connect_for_the_same_wallet_and_chain_is_refused() {
        let server = MockServer::start().await;
        mount_send_plumbing(&server).await;
        let key = hex::encode([0x42u8; 32]);
        let signer = || Signer::from_private_key_hex(&key).unwrap();

        let first = EvmSender::connect(
            EvmRpc::new(server.uri()),
            signer(),
            SEPOLIA,
            FeePolicy::default(),
        )
        .await
        .unwrap();
        let err = EvmSender::connect(
            EvmRpc::new(server.uri()),
            signer(),
            SEPOLIA,
            FeePolicy::default(),
        )
        .await
        .err()
        .expect("a second sender for the same wallet and chain");
        assert!(err.to_string().contains("already holds an EvmSender"));

        // The same wallet on another chain is another nonce sequence.
        let other_chain = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "eth_chainId"})))
            .respond_with(ok(json!("0x1")))
            .mount(&other_chain)
            .await;
        let mainnet = EvmSender::connect(
            EvmRpc::new(other_chain.uri()),
            signer(),
            1,
            FeePolicy::default(),
        )
        .await
        .unwrap();
        assert_eq!(mainnet.address(), first.address());

        // Dropping the sender frees its slot.
        drop(first);
        EvmSender::connect(
            EvmRpc::new(server.uri()),
            signer(),
            SEPOLIA,
            FeePolicy::default(),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn connect_refuses_a_node_on_another_chain() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "eth_chainId"})))
            .respond_with(ok(json!("0x1")))
            .mount(&server)
            .await;

        let err = EvmSender::connect(
            EvmRpc::new(server.uri()),
            unique_test_signer(),
            SEPOLIA,
            FeePolicy::default(),
        )
        .await
        .err()
        .unwrap();
        assert!(err.to_string().contains("not the expected chain"));
    }

    /// Two tasks sharing one sender, sending at the same time. The mock
    /// node's `pending` nonce is the number of transactions it has been
    /// sent, so it only hands out distinct nonces if the sends never
    /// overlap.
    #[tokio::test]
    async fn concurrent_sends_through_one_sender_get_consecutive_nonces() {
        let server = MockServer::start().await;
        let sent = Arc::new(Mutex::new(Vec::<u64>::new()));
        let count = Arc::new(AtomicU64::new(0));

        let count_for_nonce = count.clone();
        Mock::given(method("POST"))
            .and(body_partial_json(
                json!({"method": "eth_getTransactionCount"}),
            ))
            .respond_with(move |_: &wiremock::Request| {
                ok(json!(format!(
                    "0x{:x}",
                    count_for_nonce.load(Ordering::SeqCst)
                )))
            })
            .mount(&server)
            .await;
        let (sent_for_broadcast, count_for_broadcast) = (sent.clone(), count.clone());
        Mock::given(method("POST"))
            .and(body_partial_json(
                json!({"method": "eth_sendRawTransaction"}),
            ))
            .respond_with(move |req: &wiremock::Request| {
                let body: Value = req.body_json().unwrap();
                let raw = hex::decode(body["params"][0].as_str().unwrap().trim_start_matches("0x"))
                    .unwrap();
                let tx = alloy_consensus::TxEnvelope::decode_2718(&mut raw.as_slice()).unwrap();
                sent_for_broadcast.lock().unwrap().push(tx.nonce());
                count_for_broadcast.fetch_add(1, Ordering::SeqCst);
                ok(json!(format!("0x{}", hex::encode(tx.tx_hash()))))
            })
            .mount(&server)
            .await;
        mount_send_plumbing(&server).await;
        mount_receipt(
            &server,
            json!({ "status": "0x1", "blockNumber": "0x2a", "logs": [] }),
        )
        .await;

        let sender = connect(&server).await;
        let (a, b) = (sender.clone(), sender.clone());
        let to = Address::from([0x11; 20]);
        let (first, second) = tokio::join!(
            tokio::spawn(async move { a.send_and_confirm(to, vec![1], U256::ZERO).await }),
            tokio::spawn(async move { b.send_and_confirm(to, vec![2], U256::ZERO).await }),
        );
        assert!(matches!(first.unwrap().unwrap(), TxOutcome::Success { .. }));
        assert!(matches!(
            second.unwrap().unwrap(),
            TxOutcome::Success { .. }
        ));

        let mut nonces = sent.lock().unwrap().clone();
        nonces.sort();
        assert_eq!(nonces, vec![0, 1]);
    }

    #[tokio::test]
    async fn a_timed_out_send_blocks_the_next_until_resolve_sees_its_receipt() {
        let server = MockServer::start().await;
        mount_send_plumbing(&server).await;
        // No receipt yet, and the node still knows the transaction.
        let receipt_mock = Mock::given(method("POST"))
            .and(body_partial_json(
                json!({"method": "eth_getTransactionReceipt"}),
            ))
            .respond_with(ok(Value::Null))
            .mount_as_scoped(&server)
            .await;
        Mock::given(method("POST"))
            .and(body_partial_json(
                json!({"method": "eth_getTransactionByHash"}),
            ))
            .respond_with(ok(json!({ "hash": "0x01" })))
            .mount(&server)
            .await;

        let sender = connect(&server).await;
        sender.set_poll_settings(fast_poll());
        let to = Address::from([0x11; 20]);

        let TxOutcome::TimedOut { tx_hash } = sender
            .send_and_confirm(to, vec![1], U256::ZERO)
            .await
            .unwrap()
        else {
            panic!("expected TimedOut");
        };
        assert_eq!(sender.unresolved(), Some(tx_hash));

        let err = sender
            .send_and_confirm(to, vec![2], U256::ZERO)
            .await
            .unwrap_err();
        assert!(err.to_string().contains(&tx_hash.to_string()));

        // Still pending: resolve reports nothing, and the hash stays.
        assert!(sender.resolve().await.unwrap().is_none());
        assert_eq!(sender.unresolved(), Some(tx_hash));

        // The receipt appears: resolve returns it and clears the hash.
        drop(receipt_mock);
        mount_receipt(
            &server,
            json!({ "status": "0x1", "blockNumber": "0x2b", "logs": [] }),
        )
        .await;
        match sender.resolve().await.unwrap() {
            Some(TxOutcome::Success {
                block, tx_hash: h, ..
            }) => {
                assert_eq!(block, 0x2b);
                assert_eq!(h, tx_hash);
            }
            other => panic!("expected a resolved Success, got {other:?}"),
        }
        assert_eq!(sender.unresolved(), None);
        assert!(sender
            .send_and_confirm(to, vec![3], U256::ZERO)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn resolve_reports_a_dropped_transaction_whose_nonce_was_used() {
        let server = MockServer::start().await;
        mount_send_plumbing(&server).await; // pending and latest nonce: 5
        mount_receipt(&server, Value::Null).await;
        let known = Mock::given(method("POST"))
            .and(body_partial_json(
                json!({"method": "eth_getTransactionByHash"}),
            ))
            .respond_with(ok(json!({ "hash": "0x01" })))
            .mount_as_scoped(&server)
            .await;

        let sender = connect(&server).await;
        sender.set_poll_settings(fast_poll());
        let TxOutcome::TimedOut { tx_hash } = sender
            .send_and_confirm(Address::from([0x11; 20]), vec![1], U256::ZERO)
            .await
            .unwrap()
        else {
            panic!("expected TimedOut");
        };

        // The node forgets it, but nonce 5 is not used yet: still unresolved.
        drop(known);
        Mock::given(method("POST"))
            .and(body_partial_json(
                json!({"method": "eth_getTransactionByHash"}),
            ))
            .respond_with(ok(Value::Null))
            .mount(&server)
            .await;
        assert!(sender.resolve().await.unwrap().is_none());
        assert_eq!(sender.unresolved(), Some(tx_hash));

        // Something else took nonce 5: it was replaced or dropped.
        Mock::given(method("POST"))
            .and(body_partial_json(
                json!({"method": "eth_getTransactionCount", "params": [sender.address().to_string(), "latest"]}),
            ))
            .respond_with(ok(json!("0x6")))
            .with_priority(1)
            .mount(&server)
            .await;
        let err = sender.resolve().await.unwrap_err();
        assert!(err.to_string().contains(&tx_hash.to_string()));
        assert!(err.to_string().contains("replaced or dropped"));
        assert_eq!(sender.unresolved(), None);
    }

    #[tokio::test]
    async fn a_reverted_send_carries_the_replayed_reason() {
        let server = MockServer::start().await;
        mount_send_plumbing(&server).await;
        mount_receipt(
            &server,
            json!({ "status": "0x0", "blockNumber": "0x2a", "logs": [] }),
        )
        .await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "eth_call"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0", "id": 1,
                "error": { "code": 3, "message": "execution reverted",
                           "data": hex_data(&encode_error_string("Not cleared")) },
            })))
            .mount(&server)
            .await;

        let sender = connect(&server).await;
        match sender
            .send_and_confirm(Address::from([0x11; 20]), vec![1], U256::ZERO)
            .await
            .unwrap()
        {
            TxOutcome::Reverted { reason, block, .. } => {
                assert_eq!(reason, "Not cleared");
                assert_eq!(block, 42);
            }
            other => panic!("expected Reverted, got {other:?}"),
        }
        assert_eq!(sender.unresolved(), None);
    }

    #[tokio::test]
    async fn a_broadcast_the_node_refuses_is_an_error_and_leaves_nothing_unresolved() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "eth_sendRawTransaction"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0", "id": 1,
                "error": { "code": -32000, "message": "insufficient funds for gas * price + value" },
            })))
            .mount(&server)
            .await;
        mount_send_plumbing(&server).await;

        let sender = connect(&server).await;
        let err = sender
            .send_and_confirm(Address::from([0x11; 20]), vec![1], U256::ZERO)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("insufficient funds"));
        assert_eq!(sender.unresolved(), None);
    }

    #[tokio::test]
    async fn ensure_allowance_approves_exactly_the_amount_when_short() {
        let server = MockServer::start().await;
        mount_send_plumbing(&server).await;
        mount_receipt(
            &server,
            json!({ "status": "0x1", "blockNumber": "0x2a", "logs": [] }),
        )
        .await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "eth_call"})))
            .respond_with(ok(json!(crate::evm::rpc::format_u256(U256::from(10u64)))))
            .mount(&server)
            .await;
        let approvals = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let recorded = approvals.clone();
        Mock::given(method("POST"))
            .and(body_partial_json(
                json!({"method": "eth_sendRawTransaction"}),
            ))
            .respond_with(move |req: &wiremock::Request| {
                let body: Value = req.body_json().unwrap();
                let raw = hex::decode(body["params"][0].as_str().unwrap().trim_start_matches("0x"))
                    .unwrap();
                let tx = alloy_consensus::TxEnvelope::decode_2718(&mut raw.as_slice()).unwrap();
                recorded.lock().unwrap().push(tx.input().to_vec());
                ok(json!("0x01"))
            })
            .with_priority(1)
            .mount(&server)
            .await;

        let sender = connect(&server).await;
        let (token, spender) = (Address::from([0xAA; 20]), Address::from([0x11; 20]));

        // 10 already approved covers 10: nothing is sent.
        sender
            .ensure_allowance(token, spender, U256::from(10u64))
            .await
            .unwrap();
        assert!(approvals.lock().unwrap().is_empty());

        // 11 does not: exactly 11 is approved, never U256::MAX.
        sender
            .ensure_allowance(token, spender, U256::from(11u64))
            .await
            .unwrap();
        assert_eq!(
            approvals.lock().unwrap().clone(),
            vec![erc20::approve_calldata(spender, U256::from(11u64))]
        );
    }

    /// The URL of a local anvil node, when a contributor has one running,
    /// for tests that need a real node rather than a mock. They change its
    /// state (they send transactions, mine blocks, toggle automine), so the
    /// node is checked to be anvil before anything is sent. A no-op when
    /// `EVM_ANVIL_RPC_URL` is unset.
    pub(crate) async fn anvil() -> Option<EvmRpc> {
        let Ok(url) = std::env::var("EVM_ANVIL_RPC_URL") else {
            eprintln!("skipping: EVM_ANVIL_RPC_URL is not set");
            return None;
        };
        let rpc = EvmRpc::new(url);
        let version = rpc.client_version().await.expect("web3_clientVersion");
        assert!(
            version.starts_with("anvil"),
            "EVM_ANVIL_RPC_URL must point at an anvil node, not {version}"
        );
        Some(rpc)
    }

    /// Anvil's first well-known dev account, funded on every anvil node and
    /// fork. Public, so worthless anywhere else.
    const ANVIL_DEV_KEY_0: &str =
        "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

    /// The unresolved-timeout rule against a real node: with automine off a
    /// signed transaction sits in the mempool, so the send times out; the
    /// next send is refused; `resolve` reports it pending until a block is
    /// mined, then returns its receipt and clears it.
    #[tokio::test]
    async fn against_anvil_a_timed_out_send_is_resolved_once_mined() {
        let Some(rpc) = anvil().await else { return };
        let chain_id = rpc.chain_id().await.unwrap();
        let sender = EvmSender::connect(
            rpc.clone(),
            Signer::from_private_key_hex(ANVIL_DEV_KEY_0).unwrap(),
            chain_id,
            FeePolicy::default(),
        )
        .await
        .unwrap();
        let to = Address::from([0x77; 20]);

        let TxOutcome::Success { .. } = sender
            .send_and_confirm(to, Vec::new(), U256::from(1u64))
            .await
            .unwrap()
        else {
            panic!("a plain transfer on anvil should succeed");
        };

        rpc.call("evm_setAutomine", json!([false])).await.unwrap();
        sender.set_poll_settings(PollSettings {
            interval: Duration::from_millis(50),
            timeout: Duration::from_millis(300),
        });
        let outcome = sender
            .send_and_confirm(to, Vec::new(), U256::from(2u64))
            .await;
        let blocked = sender
            .send_and_confirm(to, Vec::new(), U256::from(3u64))
            .await;
        let pending = sender.resolve().await;
        rpc.call("evm_mine", json!([])).await.unwrap();
        rpc.call("evm_setAutomine", json!([true])).await.unwrap();

        let TxOutcome::TimedOut { tx_hash } = outcome.unwrap() else {
            panic!("with automine off, the send should time out");
        };
        assert!(blocked
            .unwrap_err()
            .to_string()
            .contains(&tx_hash.to_string()));
        assert!(pending.unwrap().is_none());
        match sender.resolve().await.unwrap() {
            Some(TxOutcome::Success { tx_hash: h, .. }) => assert_eq!(h, tx_hash),
            other => panic!("expected the mined transaction's receipt, got {other:?}"),
        }
        assert_eq!(sender.unresolved(), None);
    }
}
