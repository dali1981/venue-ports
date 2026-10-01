//! The Solana sender against a Surfpool fork (`IMPLEMENTATION_PLAN.md`
//! Phase 15, step 4). Gated on `SURFPOOL_RPC_URL`, a no-op when it is unset:
//!
//! ```text
//! surfpool start --ci --no-deploy --rpc-url https://api.mainnet-beta.solana.com \
//!   --port 8980 --ws-port 8981 --airdrop-amount 0
//! SURFPOOL_RPC_URL=http://127.0.0.1:8980 cargo test --lib against_surfpool -- --test-threads=1
//! ```
//!
//! Surfpool fetches mainnet accounts from its upstream as they are first
//! read, so these tests touch few accounts: one Solana process per IP fits
//! the public endpoints.

use crate::dex::SolanaTransaction;
use crate::solana::token::{associated_token_account, token_account_amount, MEMO_PROGRAM, TOKEN_PROGRAM};
use crate::solana::{SolanaRpc, SolanaSender, SolanaTxOutcome};
use crate::{Network, Provenance};
use solana_address::Address;
use solana_hash::Hash;
use solana_instruction::{AccountMeta, Instruction};
use solana_message::{v0, VersionedMessage};
use solana_transaction::versioned::VersionedTransaction;

const MAINNET_GENESIS: &str = "5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d";
const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

fn surfpool() -> Option<SolanaRpc> {
    let Ok(url) = std::env::var("SURFPOOL_RPC_URL") else {
        eprintln!("skipping: SURFPOOL_RPC_URL is not set");
        return None;
    };
    Some(SolanaRpc::new(url))
}

/// An unsigned memo transaction from the sender, valid to its blockhash's
/// last height.
async fn memo(sender: &SolanaSender, text: &str) -> SolanaTransaction {
    let (blockhash, last_valid_block_height) = sender.latest_blockhash().await.unwrap();
    let ix = Instruction {
        program_id: MEMO_PROGRAM,
        accounts: vec![AccountMeta::new_readonly(sender.pubkey(), true)],
        data: text.as_bytes().to_vec(),
    };
    let message = v0::Message::try_compile(
        &sender.pubkey(),
        &[ix],
        &[],
        Hash::new_from_array(blockhash),
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
async fn against_surfpool_a_fork_sender_signs_sends_and_reads_the_outcome() {
    let Some(rpc) = surfpool() else {
        return;
    };
    let sender = SolanaSender::fork(rpc).await.unwrap();
    let genesis: Address = MAINNET_GENESIS.parse().unwrap();
    assert_eq!(
        sender.network(),
        Network::Solana {
            genesis_hash: genesis.to_bytes()
        }
    );
    assert_eq!(sender.provenance(), Provenance::Simulated);

    let tx = memo(&sender, "venue-ports: a fork send").await;
    match sender.send_and_confirm(&tx).await.unwrap() {
        SolanaTxOutcome::Success { cost, meta, .. } => {
            assert!(cost.fee_lamports >= 5_000, "{cost:?}");
            assert!(cost.units_consumed > 0, "{cost:?}");
            assert_eq!(
                meta.lamports_delta(&sender.pubkey()),
                Some(-(cost.fee_lamports as i128)),
                "the payer paid exactly the fee"
            );
        }
        other => panic!("expected Success, got {other:?}"),
    }
    assert_eq!(sender.unresolved(), None);

    // A blockhash already past its last valid height: refused, nothing sent.
    let mut stale = memo(&sender, "venue-ports: too late").await;
    stale.last_valid_block_height = 0;
    let err = sender.send_and_confirm(&stale).await.unwrap_err();
    assert!(err.to_string().contains("expired"), "{err}");
}

#[tokio::test]
async fn against_surfpool_ensure_balance_funds_the_owners_token_account() {
    let Some(rpc) = surfpool() else {
        return;
    };
    let sender = SolanaSender::fork(rpc).await.unwrap();
    let usdc: Address = USDC.parse().unwrap();
    sender
        .ensure_balance(usdc.to_bytes(), TOKEN_PROGRAM.to_bytes(), 250_000_000)
        .await
        .unwrap();
    let account = associated_token_account(&sender.pubkey(), &usdc, &TOKEN_PROGRAM);
    let held = sender
        .rpc()
        .multiple_accounts(&[account])
        .await
        .unwrap()
        .pop()
        .flatten()
        .expect("the associated token account exists");
    assert_eq!(token_account_amount(&held).unwrap(), 250_000_000);
    // Holding enough already, it writes nothing.
    sender
        .ensure_balance(usdc.to_bytes(), TOKEN_PROGRAM.to_bytes(), 1)
        .await
        .unwrap();
    let held = sender.rpc().multiple_accounts(&[account]).await.unwrap();
    assert_eq!(
        token_account_amount(held[0].as_ref().unwrap()).unwrap(),
        250_000_000
    );
}
