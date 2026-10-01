//! `JupiterSimulated` — a Jupiter route as a dry run (`SPEC.md` §5,
//! Jupiter): `/swap` builds the transaction, and `simulateTransaction` runs
//! it with `sigVerify: false` and `replaceRecentBlockhash: true` against the
//! node's current state. Nothing is signed and nothing is sent.
//!
//! **Solana has no state override.** `eth_call` can grant an EVM sender the
//! input balance and the router's allowance; `simulateTransaction` can
//! inject nothing. So the sender must be an account that really holds the
//! input token on the node simulated against — a real wallet on mainnet, or
//! one a Surfpool fork funded — and a swap from an account that holds
//! nothing fails for want of funds, which is a property of the account and
//! not of the route.

use crate::dex::jupiter::{build, check_request, http_client, message_key, Destination, JupiterConfig};
use crate::dex::{
    DexExecutor, Outcome, Prepared, Realised, RouteQuote, SolanaCost, SolanaTransaction, SwapRequest,
    TxCost,
};
use crate::solana::rpc::SolanaRpc;
use crate::solana::token::token_account_amount;
use crate::{Network, Provenance};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Mutex;

/// The base fee per signature, in lamports.
const LAMPORTS_PER_SIGNATURE: u64 = 5_000;

#[derive(Debug, Clone)]
struct Pending {
    destination: Destination,
    /// What the transaction would pay: the signatures' base fee and the
    /// priority fee `/swap` put in it.
    fee_lamports: u64,
}

pub struct JupiterSimulated {
    rpc: SolanaRpc,
    network: Network,
    config: JupiterConfig,
    http: reqwest::Client,
    pending: Mutex<HashMap<Vec<u8>, Pending>>,
}

impl JupiterSimulated {
    /// A dry-run adapter over the node `rpc` names, which it asks for its
    /// genesis hash: a route for any other network is refused.
    pub async fn connect(rpc: SolanaRpc, config: JupiterConfig) -> Result<Self> {
        let genesis_hash = rpc
            .genesis_hash()
            .await
            .context("reading the node's genesis hash")?;
        Ok(Self {
            rpc,
            network: Network::Solana { genesis_hash },
            config,
            http: http_client(),
            pending: Mutex::new(HashMap::new()),
        })
    }
}

#[async_trait]
impl DexExecutor for JupiterSimulated {
    async fn prepare(&self, route: &RouteQuote, req: &SwapRequest) -> Result<Prepared> {
        let (sender, recipient) = check_request(self.network, route, req)?;
        let built = build(&self.config, &self.http, &self.rpc, route, req, sender, recipient).await?;
        let signatures = u64::from(built.transaction.message.header().num_required_signatures);
        self.pending.lock().unwrap().insert(
            message_key(&built.transaction),
            Pending {
                destination: built.destination,
                fee_lamports: signatures * LAMPORTS_PER_SIGNATURE + built.prioritization_fee_lamports,
            },
        );
        Ok(Prepared::Solana(SolanaTransaction {
            transaction: built.transaction,
            last_valid_block_height: built.last_valid_block_height,
        }))
    }

    /// `at` is ignored: `simulateTransaction` runs against the node's
    /// current state, and Solana has no historical one to ask for.
    async fn execute(&self, prepared: &Prepared, _at: Option<u64>) -> Result<Realised> {
        let tx = prepared.solana_transaction()?;
        let ctx = self
            .pending
            .lock()
            .unwrap()
            .remove(&message_key(&tx.transaction))
            .ok_or_else(|| {
                anyhow!(
                    "execute() called with a Prepared value this JupiterSimulated instance did not \
                     produce, or already consumed"
                )
            })?;
        let wire = bincode::serialize(&tx.transaction).context("encoding the transaction")?;
        let simulation = self
            .rpc
            .simulate_transaction(&wire, false, true, &[ctx.destination.account])
            .await?;
        let cost = TxCost::Solana(SolanaCost {
            fee_lamports: ctx.fee_lamports,
            units_consumed: simulation.units_consumed,
            ..SolanaCost::default()
        });
        if let Some(err) = simulation.err {
            return Ok(Realised {
                amount_out: None,
                outcome: Outcome::Reverted {
                    reason: err.to_string(),
                },
                cost,
                at: simulation.slot,
                provenance: Provenance::Simulated,
                tx_ref: None,
            });
        }
        let after = simulation
            .accounts
            .first()
            .cloned()
            .flatten()
            .ok_or_else(|| anyhow!("the simulation returned no destination account"))?;
        let after = token_account_amount(&after)?;
        Ok(Realised {
            amount_out: Some(u128::from(after.saturating_sub(ctx.destination.before))),
            outcome: Outcome::Success,
            cost,
            at: simulation.slot,
            provenance: Provenance::Simulated,
            tx_ref: None,
        })
    }

    fn label(&self) -> &'static str {
        "jupiter-simulated"
    }
}
