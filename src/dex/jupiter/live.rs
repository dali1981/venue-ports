//! `JupiterLive` — a Jupiter route signed and sent through a shared
//! `SolanaSender` (`SPEC.md` §5, Jupiter). Over a fork sender it runs on a
//! Surfpool fork and is Simulated; over a signing sender it would be Live,
//! which is not built until the owner asks (the signing backend refuses).
//!
//! The transaction `/swap` built carries a blockhash from Jupiter's own
//! node. The adapter stamps it with a blockhash from its sender's node
//! instead, and takes that blockhash's last valid height: on a real cluster
//! the two are the same chain, and on a fork only the sender's node's
//! blockhash is one the fork knows. The amount out is read from the landed
//! transaction's meta: the destination token account's balance after, less
//! before.

use crate::dex::jupiter::{build, check_request, http_client, message_key, Destination, JupiterConfig};
use crate::dex::{
    DexExecutor, Outcome, Prepared, Realised, RouteQuote, SolanaCost, SolanaTransaction, SwapRequest,
    TxCost,
};
use crate::solana::{SolanaSender, SolanaTxOutcome};
use crate::Provenance;
use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use solana_hash::Hash;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub struct JupiterLive {
    sender: Arc<SolanaSender>,
    config: JupiterConfig,
    http: reqwest::Client,
    pending: Mutex<HashMap<Vec<u8>, Destination>>,
}

impl JupiterLive {
    pub fn new(sender: Arc<SolanaSender>, config: JupiterConfig) -> Self {
        Self {
            sender,
            config,
            http: http_client(),
            pending: Mutex::new(HashMap::new()),
        }
    }

    pub fn sender(&self) -> &Arc<SolanaSender> {
        &self.sender
    }

    fn tx_ref(&self, signature: [u8; 64]) -> Option<Vec<u8>> {
        (self.sender.provenance() == Provenance::Landed).then(|| signature.to_vec())
    }

    fn unsettled(&self, outcome: Outcome, cost: SolanaCost, at: u64, signature: [u8; 64]) -> Realised {
        Realised {
            amount_out: None,
            outcome,
            cost: TxCost::Solana(cost),
            at,
            provenance: self.sender.provenance(),
            tx_ref: self.tx_ref(signature),
        }
    }
}

#[async_trait]
impl DexExecutor for JupiterLive {
    async fn prepare(&self, route: &RouteQuote, req: &SwapRequest) -> Result<Prepared> {
        let (sender, recipient) = check_request(self.sender.network(), route, req)?;
        if sender != self.sender.pubkey() {
            bail!(
                "SwapRequest.sender ({sender}) is not this JupiterLive's sending address ({}) — it \
                 can only send as its own sender",
                self.sender.pubkey()
            );
        }
        let mut built = build(
            &self.config,
            &self.http,
            self.sender.rpc(),
            route,
            req,
            sender,
            recipient,
        )
        .await?;
        let (blockhash, last_valid_block_height) = self.sender.latest_blockhash().await?;
        built
            .transaction
            .message
            .set_recent_blockhash(Hash::new_from_array(blockhash));
        built.transaction.signatures.clear();
        self.pending
            .lock()
            .unwrap()
            .insert(message_key(&built.transaction), built.destination);
        Ok(Prepared::Solana(SolanaTransaction {
            transaction: built.transaction,
            last_valid_block_height,
        }))
    }

    /// `at` is ignored: a sent transaction only ever runs now.
    async fn execute(&self, prepared: &Prepared, _at: Option<u64>) -> Result<Realised> {
        let tx = prepared.solana_transaction()?;
        let destination = self
            .pending
            .lock()
            .unwrap()
            .remove(&message_key(&tx.transaction))
            .ok_or_else(|| {
                anyhow!(
                    "execute() called with a Prepared value this JupiterLive instance did not \
                     produce, or already consumed"
                )
            })?;
        match self.sender.send_and_confirm(tx).await? {
            SolanaTxOutcome::Success {
                slot,
                signature,
                cost,
                meta,
            } => {
                if !meta.account_keys.contains(&destination.account) {
                    bail!(
                        "the swap landed ({}) but its destination token account {} is not in the \
                         transaction",
                        solana_signature::Signature::from(signature),
                        destination.account
                    );
                }
                // An account the swap created held nothing before it, and is
                // in no pre-balance.
                let (before, after) = meta.token_amounts(&destination.account);
                Ok(Realised {
                    amount_out: Some(u128::from(after.saturating_sub(before))),
                    outcome: Outcome::Success,
                    cost: TxCost::Solana(cost),
                    at: slot,
                    provenance: self.sender.provenance(),
                    tx_ref: self.tx_ref(signature),
                })
            }
            SolanaTxOutcome::Failed {
                slot,
                signature,
                reason,
                cost,
            } => Ok(self.unsettled(Outcome::Reverted { reason }, cost, slot, signature)),
            SolanaTxOutcome::TimedOut { signature } => {
                let at = self.sender.rpc().slot().await.unwrap_or(0);
                Ok(self.unsettled(Outcome::TimedOut, SolanaCost::default(), at, signature))
            }
            SolanaTxOutcome::Expired { signature } => {
                let at = self.sender.rpc().slot().await.unwrap_or(0);
                Ok(self.unsettled(Outcome::Expired, SolanaCost::default(), at, signature))
            }
        }
    }

    fn label(&self) -> &'static str {
        match self.sender.provenance() {
            Provenance::Landed => "jupiter-live",
            Provenance::Simulated => "jupiter-live-fork",
        }
    }
}
