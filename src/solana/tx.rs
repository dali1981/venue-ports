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
//!
//! **A fork never runs a transaction against an account newer than its
//! clock.** Surfpool fetches an account from its upstream the first time a
//! transaction needs it, so the account carries the chain's time at that
//! moment, while the fork's clock runs from its own start, slot by slot, and
//! is about a second behind the chain's (measured 1 October 2026). A program
//! that orders the two refuses: a Whirlpool's last reward update is then
//! after the fork's now ("Timestamp should be greater than the last updated
//! timestamp"). So the fork sender first loads the transaction's accounts
//! with a dry run, and sends once the fork's clock has passed the second the
//! dry run ended in. Surfpool's `surfnet_timeTravel` cannot do this instead:
//! in 1.6.0 it writes the slot's index within the epoch into the `Clock`
//! sysvar's absolute slot (so every lookup table's entries read as not yet
//! active), and the clock falls back behind at the next slot.

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
/// `TimedOut`. A fork send waits for its fork's clock (below) on the same
/// terms.
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
    pub async fn ensure_balance(
        &self,
        mint: [u8; 32],
        token_program: [u8; 32],
        amount: u64,
    ) -> Result<()> {
        let (mint, program) = (
            Address::new_from_array(mint),
            Address::new_from_array(token_program),
        );
        let account = associated_token_account(&self.pubkey(), &mint, &program);
        let held = match self
            .rpc
            .multiple_accounts(&[account])
            .await?
            .pop()
            .flatten()
        {
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
    /// is unresolved, a signature is missing, the fork could not load the
    /// transaction's accounts or its clock did not pass them, the blockhash
    /// had already expired, or the node refused the transaction.
    pub async fn send_and_confirm(&self, tx: &SolanaTransaction) -> Result<SolanaTxOutcome> {
        let _guard = self.send_lock.lock().await;
        if let Some(signature) = self.unresolved() {
            bail!(
                "an earlier send ({}) timed out and is unresolved: resolve it before sending again",
                Signature::from(signature)
            );
        }
        let signed = self.sign(&tx.transaction)?;
        let signature: [u8; 64] = signed.signatures[0].into();
        let wire = bincode::serialize(&signed).context("encoding the transaction")?;
        self.load_on_fork(&wire).await?;
        let height = self.rpc.block_height().await?;
        if height > tx.last_valid_block_height {
            bail!(
                "the transaction's blockhash expired at block height {} (now {height}): nothing sent",
                tx.last_valid_block_height
            );
        }
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

    /// Loads `wire`'s accounts into the fork with a dry run, then waits until
    /// the fork's clock has passed the second the dry run ended in: an `Err`,
    /// with nothing sent, when the fork cannot load them or its clock does
    /// not get there (the module's docs say why). What the dry run says of
    /// the transaction is not read: a failure lands, and is read, when it is
    /// sent.
    async fn load_on_fork(&self, wire: &[u8]) -> Result<()> {
        self.rpc
            .simulate_transaction(wire, false, false, &[])
            .await
            .context("loading the transaction's accounts into the fork: nothing sent")?;
        let loaded_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .context("the wall clock is before 1970")?
            .as_secs();
        let loaded_at = i64::try_from(loaded_at).context("the wall clock is past 2^63 s")?;
        let poll = *self.poll.lock().unwrap();
        let give_up = tokio::time::Instant::now() + poll.timeout;
        loop {
            let clock = self.rpc.clock_unix_timestamp().await?;
            if clock > loaded_at {
                return Ok(());
            }
            if tokio::time::Instant::now() >= give_up {
                bail!(
                    "the fork's clock ({clock}) did not pass the time its accounts were loaded \
                     ({loaded_at}) within {:?}: nothing sent",
                    poll.timeout
                );
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
            .or_else(|| {
                line.strip_prefix("Program log: Error: ")
                    .map(str::to_string)
            })
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
    /// transaction and never shows a status for it. Its clock is far in the
    /// future.
    async fn unseeing_node(heights: Vec<u64>) -> wiremock::MockServer {
        mock_node(heights, vec![i64::MAX / 2]).await
    }

    /// As `unseeing_node`, with the clock reading `clocks[i]` on the i-th
    /// read (the last one after that).
    async fn mock_node(heights: Vec<u64>, clocks: Vec<i64>) -> wiremock::MockServer {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let clock_reads = Arc::new(AtomicUsize::new(0));
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
                    // The Clock sysvar, its unix_timestamp from `clocks`.
                    "getMultipleAccounts" => {
                        use base64::Engine;
                        let i = clock_reads.fetch_add(1, Ordering::SeqCst);
                        let mut clock = vec![0u8; 40];
                        let unix_timestamp = clocks[i.min(clocks.len() - 1)];
                        clock[32..40].copy_from_slice(&unix_timestamp.to_le_bytes());
                        let data = base64::engine::general_purpose::STANDARD.encode(clock);
                        json!({ "context": { "slot": 1 }, "value": [{
                            "lamports": 1_169_280,
                            "owner": "Sysvar1111111111111111111111111111111111111",
                            "data": [data, "base64"],
                            "executable": false,
                            "rentEpoch": 0,
                        }] })
                    }
                    "simulateTransaction" => json!({
                        "context": { "slot": 1 },
                        "value": { "err": null, "logs": [], "unitsConsumed": 150 },
                    }),
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
        let sender = SolanaSender::fork(SolanaRpc::new(server.uri()))
            .await
            .unwrap();
        sender.set_poll_settings(SolanaPollSettings {
            interval: Duration::from_millis(10),
            timeout: Duration::from_secs(5),
        });
        let tx = memo_from(&sender).await;
        let outcome = sender.send_and_confirm(&tx).await.unwrap();
        assert!(
            matches!(outcome, SolanaTxOutcome::Expired { .. }),
            "{outcome:?}"
        );
        assert_eq!(
            sender.unresolved(),
            None,
            "an expiry is known, so it blocks nothing"
        );
    }

    #[tokio::test]
    async fn a_fork_send_loads_its_accounts_and_waits_for_the_forks_clock_to_pass_them() {
        // The fork's clock reads 1970 twice, then the far future.
        let server = mock_node(vec![10, 100], vec![1, 1, i64::MAX / 2]).await;
        let sender = SolanaSender::fork(SolanaRpc::new(server.uri()))
            .await
            .unwrap();
        sender.set_poll_settings(SolanaPollSettings {
            interval: Duration::from_millis(10),
            timeout: Duration::from_secs(5),
        });
        let tx = memo_from(&sender).await;
        sender.send_and_confirm(&tx).await.unwrap();
        let methods: Vec<String> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| {
                let body: serde_json::Value = r.body_json().unwrap();
                body["method"].as_str().unwrap().to_string()
            })
            .collect();
        let at = |m: &str| {
            methods
                .iter()
                .position(|x| x == m)
                .unwrap_or_else(|| panic!("no {m}"))
        };
        let clock_reads = methods
            .iter()
            .filter(|m| *m == "getMultipleAccounts")
            .count();
        assert!(
            at("simulateTransaction") < at("getMultipleAccounts"),
            "{methods:?}"
        );
        assert_eq!(clock_reads, 3, "read until it passed: {methods:?}");
        let last_clock_read = methods
            .iter()
            .rposition(|m| m == "getMultipleAccounts")
            .unwrap();
        assert!(at("sendTransaction") > last_clock_read, "{methods:?}");
    }

    #[tokio::test]
    async fn a_fork_whose_clock_never_passes_its_accounts_sends_nothing() {
        let server = mock_node(vec![10], vec![1]).await;
        let sender = SolanaSender::fork(SolanaRpc::new(server.uri()))
            .await
            .unwrap();
        sender.set_poll_settings(SolanaPollSettings {
            interval: Duration::from_millis(10),
            timeout: Duration::from_millis(100),
        });
        let tx = memo_from(&sender).await;
        let err = sender.send_and_confirm(&tx).await.unwrap_err();
        assert!(err.to_string().contains("nothing sent"), "{err}");
        let sent =
            server.received_requests().await.unwrap().iter().any(|r| {
                r.body_json::<serde_json::Value>().unwrap()["method"] == "sendTransaction"
            });
        assert!(!sent);
        assert_eq!(sender.unresolved(), None);
    }

    #[tokio::test]
    async fn a_timed_out_send_blocks_the_next_until_resolved() {
        let server = unseeing_node(vec![10]).await;
        let sender = SolanaSender::fork(SolanaRpc::new(server.uri()))
            .await
            .unwrap();
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
        assert!(
            sender.resolve().await.unwrap().is_none(),
            "it may still land"
        );
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
