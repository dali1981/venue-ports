//! `SolanaSender` — the only thing in the crate that signs for one wallet on
//! one Solana cluster (`SPEC.md` §5, the Solana family). Every Solana
//! adapter that sends from that wallet holds the same `Arc`, as every EVM
//! adapter does an `EvmSender`.
//!
//! **Two backends, one built.** The signing backend (a key from the
//! environment, sending to a real cluster) is **refused**: it returns an
//! error until the owner asks for Live. The fork backend sends to a Surfpool
//! node, a throwaway copy of the chain, with a key generated at run time and
//! never written anywhere. It refuses any node that does not answer
//! `surfnet_getSurfnetInfo`, as the EVM fork backend refuses a node that is
//! not anvil: it sends transactions, and must never send them anywhere real.
//!
//! **A send's outcome.** The sender signs its own slot of the transaction
//! (an adapter may have signed others, such as a new position's mint), sends
//! it, and polls its signature until the outcome is known: it landed
//! (`Success` or `Failed`, each with its meta's fee and compute units), its
//! blockhash expired with no status for it (`Expired`: it can never land), or
//! polling gave up first (`TimedOut`: unknown, so every later send is refused
//! until `resolve` settles it). On a fork, preflight is skipped, so a
//! transaction that fails lands and is observed as `Failed` with its reason.

use crate::dex::{SolanaCost, SolanaTransaction};
use crate::solana::rpc::{SolanaRpc, SolanaRpcError, TxMeta};
use crate::solana::token::associated_token_account;
use crate::{Network, Provenance};
use anyhow::{anyhow, bail, Context, Result};
use solana_address::Address;
use solana_keypair::Keypair;
use solana_signature::Signature;
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// SOL a fork sender's key starts with, for fees and rent: 100 SOL.
const FORK_LAMPORTS: u64 = 100_000_000_000;

/// The terminal outcome of sending exactly one transaction. Not `Realised`:
/// an adapter decides what a given transaction's outcome means.
#[derive(Debug, Clone)]
pub enum SolanaTxOutcome {
    Success {
        slot: u64,
        signature: [u8; 64],
        cost: SolanaCost,
        meta: TxMeta,
    },
    /// The transaction landed and failed: its fee is spent, its effects are
    /// not.
    Failed {
        slot: u64,
        signature: [u8; 64],
        reason: String,
        cost: SolanaCost,
    },
    /// Sent, not seen, and its blockhash not yet expired when polling gave
    /// up.
    TimedOut { signature: [u8; 64] },
    /// Its blockhash expired with no status for its signature: it can never
    /// land.
    Expired { signature: [u8; 64] },
}

/// How often a signature is polled, and for how long before a send ends
/// `TimedOut`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SolanaPollSettings {
    pub interval: Duration,
    pub timeout: Duration,
}

impl Default for SolanaPollSettings {
    /// A poll every 400 ms, a slot; giving up after 90 s, past the ~60 s a
    /// blockhash lives, so a send that never lands ends `Expired`, which is
    /// known, rather than `TimedOut`.
    fn default() -> Self {
        Self {
            interval: Duration::from_millis(400),
            timeout: Duration::from_secs(90),
        }
    }
}

#[derive(Debug, Clone)]
struct Unresolved {
    signature: [u8; 64],
    last_valid_block_height: u64,
}

pub struct SolanaSender {
    rpc: SolanaRpc,
    keypair: Keypair,
    network: Network,
    poll: Mutex<SolanaPollSettings>,
    send_lock: tokio::sync::Mutex<()>,
    unresolved: Mutex<Option<Unresolved>>,
}

impl SolanaSender {
    /// A signing sender, with a key from the environment. **Refused**: it
    /// returns an error until the owner asks for Live.
    pub async fn signing(rpc: SolanaRpc, network: Network) -> Result<Arc<Self>> {
        bail!(
            "the Solana signing backend is not built: Live on {network} (node {}) waits until the \
             owner asks for it. Use SolanaSender::fork on a Surfpool node",
            rpc.url()
        )
    }

    /// A sender on a Surfpool fork. It refuses any node that does not answer
    /// a `surfnet_*` method, generates its key at run time and never writes
    /// it, and funds it with SOL for fees and rent. Its network is the one
    /// the fork reports, as an anvil fork reports its chain's id.
    pub async fn fork(rpc: SolanaRpc) -> Result<Arc<Self>> {
        if !rpc.is_surfnet().await {
            bail!(
                "refusing {}: it does not answer surfnet_getSurfnetInfo, so it is not a Surfpool \
                 fork — the fork backend sends transactions and must never send them anywhere real",
                rpc.url()
            );
        }
        let genesis_hash = rpc
            .genesis_hash()
            .await
            .context("reading the fork's genesis hash")?;
        let keypair = Keypair::new();
        rpc.surfnet_set_lamports(&keypair.pubkey(), FORK_LAMPORTS)
            .await
            .context("funding the fork sender's key")?;
        Ok(Arc::new(Self {
            rpc,
            keypair,
            network: Network::Solana { genesis_hash },
            poll: Mutex::new(SolanaPollSettings::default()),
            send_lock: tokio::sync::Mutex::new(()),
            unresolved: Mutex::new(None),
        }))
    }

    pub fn address(&self) -> [u8; 32] {
        self.keypair.pubkey().to_bytes()
    }

    pub fn pubkey(&self) -> Address {
        self.keypair.pubkey()
    }

    pub fn network(&self) -> Network {
        self.network
    }

    /// `Simulated`: the only backend built is the fork's. A signing sender
    /// would be `Landed`.
    pub fn provenance(&self) -> Provenance {
        Provenance::Simulated
    }

    pub fn rpc(&self) -> &SolanaRpc {
        &self.rpc
    }

    pub fn set_poll_settings(&self, poll: SolanaPollSettings) {
        *self.poll.lock().unwrap() = poll;
    }

    /// A fresh blockhash and the block height it stays valid to, for an
    /// adapter building a `SolanaTransaction`.
    pub async fn latest_blockhash(&self) -> Result<([u8; 32], u64)> {
        self.rpc.latest_blockhash().await
    }

    /// The signature of a send that ended `TimedOut` and is not yet
    /// resolved.
    pub fn unresolved(&self) -> Option<[u8; 64]> {
        self.unresolved
            .lock()
            .unwrap()
            .as_ref()
            .map(|u| u.signature)
    }

    /// Polls the unresolved signature once. Its terminal outcome clears it:
    /// it landed, or its blockhash expired with no status. `None` while it
    /// may still land.
    pub async fn resolve(&self) -> Result<Option<SolanaTxOutcome>> {
        let Some(pending) = self.unresolved.lock().unwrap().clone() else {
            return Ok(None);
        };
        let outcome = self.check(&pending).await?;
        if outcome.is_some() {
            *self.unresolved.lock().unwrap() = None;
        }
        Ok(outcome)
    }

    /// Checks that the owner's associated token account for `mint` holds at
    /// least `amount`. A fork sender writes the amount with Surfpool's
    /// cheatcode when it holds less, creating the account if it is missing,
    /// as `EvmSender::ensure_balance` writes a storage slot on anvil.
    pub async fn ensure_balance(&self, mint: [u8; 32], token_program: [u8; 32], amount: u64) -> Result<()> {
        let (mint, program) = (
            Address::new_from_array(mint),
            Address::new_from_array(token_program),
        );
        let account = associated_token_account(&self.pubkey(), &mint, &program);
        let held = match self.rpc.multiple_accounts(&[account]).await?.pop().flatten() {
            Some(data) => crate::solana::token::token_account_amount(&data)?,
            None => 0,
        };
        if held >= amount {
            return Ok(());
        }
        self.rpc
            .surfnet_set_token_account(&self.pubkey(), &mint, &program, amount)
            .await
            .with_context(|| format!("funding {amount} of {mint} on the fork"))
    }

    /// Signs the sender's own slot of `tx`, refusing one that needs a
    /// signature nobody has given.
    fn sign(&self, tx: &VersionedTransaction) -> Result<VersionedTransaction> {
        let header = tx.message.header();
        let required = usize::from(header.num_required_signatures);
        let keys = tx.message.static_account_keys();
        let me = self.pubkey();
        let mine = keys[..required.min(keys.len())]
            .iter()
            .position(|k| *k == me)
            .ok_or_else(|| anyhow!("the transaction needs no signature from this sender ({me})"))?;
        let mut signed = tx.clone();
        signed.signatures.resize(required, Signature::default());
        signed.signatures[mine] = self.keypair.sign_message(&tx.message.serialize());
        if let Some(i) = signed
            .signatures
            .iter()
            .position(|s| *s == Signature::default())
        {
            bail!(
                "the transaction needs a signature from {} that nobody has given",
                keys[i]
            );
        }
        signed
            .verify_and_hash_message()
            .map_err(|e| anyhow!("the signed transaction does not verify: {e:?}"))?;
        Ok(signed)
    }

    /// Sign, send, and poll the signature until its outcome is known or its
    /// blockhash expires. An `Err` means nothing was sent: an earlier send
    /// is unresolved, the blockhash had already expired, a signature is
    /// missing, or the node refused the transaction.
    pub async fn send_and_confirm(&self, tx: &SolanaTransaction) -> Result<SolanaTxOutcome> {
        let _guard = self.send_lock.lock().await;
        if let Some(signature) = self.unresolved() {
            bail!(
                "an earlier send ({}) timed out and is unresolved: resolve it before sending again",
                Signature::from(signature)
            );
        }
        let height = self.rpc.block_height().await?;
        if height > tx.last_valid_block_height {
            bail!(
                "the transaction's blockhash expired at block height {} (now {height}): nothing sent",
                tx.last_valid_block_height
            );
        }
        let signed = self.sign(&tx.transaction)?;
        let signature: [u8; 64] = signed.signatures[0].into();
        let wire = bincode::serialize(&signed).context("encoding the transaction")?;
        match self.rpc.send_transaction(&wire, true).await {
            Ok(_) => {}
            // The node answered and refused it: nothing landed.
            Err(e) if e.downcast_ref::<SolanaRpcError>().is_some() => {
                return Err(e.context("the node refused the transaction"));
            }
            // A lost answer: it may be out, so it is polled for like any other.
            Err(_) => {}
        }

        let pending = Unresolved {
            signature,
            last_valid_block_height: tx.last_valid_block_height,
        };
        let poll = *self.poll.lock().unwrap();
        let deadline = tokio::time::Instant::now() + poll.timeout;
        loop {
            // A failed poll is retried: the transaction is already out.
            if let Ok(Some(outcome)) = self.check(&pending).await {
                return Ok(outcome);
            }
            if tokio::time::Instant::now() >= deadline {
                *self.unresolved.lock().unwrap() = Some(pending);
                return Ok(SolanaTxOutcome::TimedOut { signature });
            }
            tokio::time::sleep(poll.interval).await;
        }
    }

    /// One look at a sent transaction: its outcome once it landed (with its
    /// meta) or once its blockhash expired unseen, `None` while it may still
    /// land.
    async fn check(&self, pending: &Unresolved) -> Result<Option<SolanaTxOutcome>> {
        let encoded = Signature::from(pending.signature).to_string();
        let status = self
            .rpc
            .signature_statuses(std::slice::from_ref(&encoded))
            .await?
            .pop()
            .flatten();
        match status {
            Some(status) if status.confirmation != "processed" => {
                let Some(meta) = self.rpc.transaction(&encoded).await? else {
                    return Ok(None);
                };
                let cost = SolanaCost {
                    fee_lamports: meta.fee_lamports,
                    units_consumed: meta.units_consumed,
                    ..SolanaCost::default()
                };
                Ok(Some(match &meta.err {
                    None => SolanaTxOutcome::Success {
                        slot: meta.slot,
                        signature: pending.signature,
                        cost,
                        meta,
                    },
                    Some(err) => SolanaTxOutcome::Failed {
                        slot: meta.slot,
                        signature: pending.signature,
                        reason: failure_reason(err, &meta.log_messages),
                        cost,
                    },
                }))
            }
            Some(_) => Ok(None),
            None => {
                if self.rpc.block_height().await? > pending.last_valid_block_height {
                    // Unseen past its last valid height: it can never land.
                    // One more look first, in case it landed at the edge.
                    let again = self
                        .rpc
                        .signature_statuses(std::slice::from_ref(&encoded))
                        .await?
                        .pop()
                        .flatten();
                    if again.is_none() {
                        return Ok(Some(SolanaTxOutcome::Expired {
                            signature: pending.signature,
                        }));
                    }
                }
                Ok(None)
            }
        }
    }
}

/// A failed transaction's reason: the program's own message when its logs
/// carry one, else the runtime's error.
fn failure_reason(err: &serde_json::Value, logs: &[String]) -> String {
    let message = logs.iter().rev().find_map(|line| {
        line.split_once("Error Message: ")
            .map(|(_, m)| m.trim_end_matches('.').to_string())
            .or_else(|| line.strip_prefix("Program log: Error: ").map(str::to_string))
    });
    match message {
        Some(message) => message,
        None => err.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_failure_reads_the_programs_own_message_first() {
        let logs = vec![
            "Program whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc invoke [1]".to_string(),
            "Program log: AnchorError occurred. Error Code: TokenMaxExceeded. Error Number: 6017. \
             Error Message: Exceeded token max."
                .to_string(),
        ];
        let err = json!({"InstructionError": [2, {"Custom": 6017}]});
        assert_eq!(failure_reason(&err, &logs), "Exceeded token max");
        assert_eq!(
            failure_reason(&err, &[]),
            r#"{"InstructionError":[2,{"Custom":6017}]}"#
        );
    }

    #[tokio::test]
    async fn the_signing_backend_is_refused() {
        let err = SolanaSender::signing(
            SolanaRpc::new("http://127.0.0.1:1"),
            Network::Solana {
                genesis_hash: [1; 32],
            },
        )
        .await
        .err()
        .expect("refused");
        assert!(err.to_string().contains("not built"), "{err}");
    }

    /// A mock Surfpool whose block height is `heights[i]` on the i-th
    /// `getBlockHeight` (the last one after that), which accepts every
    /// transaction and never shows a status for it.
    async fn unseeing_node(heights: Vec<u64>) -> wiremock::MockServer {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        let calls = Arc::new(AtomicUsize::new(0));
        Mock::given(method("POST"))
            .respond_with(move |req: &wiremock::Request| {
                let body: serde_json::Value = req.body_json().unwrap();
                let result = match body["method"].as_str().unwrap() {
                    "surfnet_getSurfnetInfo" => json!({}),
                    "getGenesisHash" => json!("5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d"),
                    "surfnet_setAccount" => json!(null),
                    "getLatestBlockhash" => json!({
                        "context": { "slot": 1 },
                        "value": { "blockhash": "11111111111111111111111111111111", "lastValidBlockHeight": 50 },
                    }),
                    "getBlockHeight" => {
                        let i = calls.fetch_add(1, Ordering::SeqCst);
                        json!(heights[i.min(heights.len() - 1)])
                    }
                    "sendTransaction" => json!("sig"),
                    "getSignatureStatuses" => json!({ "context": { "slot": 1 }, "value": [null] }),
                    other => panic!("unexpected {other}"),
                };
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "jsonrpc": "2.0", "id": 1, "result": result }))
            })
            .mount(&server)
            .await;
        server
    }

    async fn memo_from(sender: &SolanaSender) -> SolanaTransaction {
        use solana_instruction::{AccountMeta, Instruction};
        use solana_message::{v0, VersionedMessage};
        let (blockhash, last_valid_block_height) = sender.latest_blockhash().await.unwrap();
        let ix = Instruction {
            program_id: crate::solana::token::MEMO_PROGRAM,
            accounts: vec![AccountMeta::new_readonly(sender.pubkey(), true)],
            data: b"x".to_vec(),
        };
        let message = v0::Message::try_compile(
            &sender.pubkey(),
            &[ix],
            &[],
            solana_hash::Hash::new_from_array(blockhash),
        )
        .unwrap();
        SolanaTransaction {
            transaction: VersionedTransaction {
                signatures: Vec::new(),
                message: VersionedMessage::V0(message),
            },
            last_valid_block_height,
        }
    }

    #[tokio::test]
    async fn a_send_unseen_past_its_last_valid_height_is_expired() {
        let server = unseeing_node(vec![10, 100]).await;
        let sender = SolanaSender::fork(SolanaRpc::new(server.uri())).await.unwrap();
        sender.set_poll_settings(SolanaPollSettings {
            interval: Duration::from_millis(10),
            timeout: Duration::from_secs(5),
        });
        let tx = memo_from(&sender).await;
        let outcome = sender.send_and_confirm(&tx).await.unwrap();
        assert!(matches!(outcome, SolanaTxOutcome::Expired { .. }), "{outcome:?}");
        assert_eq!(sender.unresolved(), None, "an expiry is known, so it blocks nothing");
    }

    #[tokio::test]
    async fn a_timed_out_send_blocks_the_next_until_resolved() {
        let server = unseeing_node(vec![10]).await;
        let sender = SolanaSender::fork(SolanaRpc::new(server.uri())).await.unwrap();
        sender.set_poll_settings(SolanaPollSettings {
            interval: Duration::from_millis(10),
            timeout: Duration::from_millis(100),
        });
        let tx = memo_from(&sender).await;
        let outcome = sender.send_and_confirm(&tx).await.unwrap();
        let SolanaTxOutcome::TimedOut { signature } = outcome else {
            panic!("expected TimedOut, got {outcome:?}");
        };
        assert_eq!(sender.unresolved(), Some(signature));
        let err = sender.send_and_confirm(&tx).await.unwrap_err();
        assert!(err.to_string().contains("unresolved"), "{err}");
        assert!(sender.resolve().await.unwrap().is_none(), "it may still land");
        assert_eq!(sender.unresolved(), Some(signature));
    }

    #[tokio::test]
    async fn the_fork_backend_refuses_a_node_that_is_not_surfpool() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0", "id": 1,
                "error": { "code": -32601, "message": "Method not found" },
            })))
            .mount(&server)
            .await;
        let err = SolanaSender::fork(SolanaRpc::new(server.uri()))
            .await
            .err()
            .expect("refused");
        assert!(err.to_string().contains("not a Surfpool"), "{err}");
    }
}
