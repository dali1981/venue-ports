//! `WhirlpoolLiquidity` — Orca's Whirlpool as a liquidity venue, over an
//! `Arc<SolanaSender>` (`SPEC.md` §5b, V5 §5, `IMPLEMENTATION_PLAN.md` Phase
//! 15). Given a fork sender it runs on a Surfpool fork and is Simulated;
//! given a signing sender it would be Live, which is not built until the
//! owner asks (the signing backend refuses).
//!
//! **The venue's deployment comes from construction**: the program id. A
//! command names the pool; its tokens, vaults and tick spacing are read from
//! the pool, and an account the program does not own is refused.
//!
//! **One transaction per command**, from the owner, who pays:
//!
//! | command | instructions |
//! | --- | --- |
//! | `Open` | each missing tick array of the range (`initialize_dynamic_tick_array`), `open_position_with_token_extensions`, `increase_liquidity_by_token_amounts_v2` |
//! | `Add` | `increase_liquidity_by_token_amounts_v2` |
//! | `Remove` | `decrease_liquidity_v2`: the principal reaches the owner at once |
//! | `Collect` | `update_fees_and_rewards` (only while the position holds liquidity: the program refuses it on an empty one, whose fees `decrease_liquidity_v2` brought up to date), `collect_fees_v2` |
//! | `Close` | `close_position_with_token_extensions`: the position's rent returns to the owner |
//!
//! `Open` mints a new position under Token-2022, its mint a key generated
//! here, which co-signs the transaction before the sender signs it. A
//! position is named by its mint ([`PositionId::bytes`]).
//!
//! **What a position cannot take is refused before sending**: the adapter
//! reads the position (and that the owner holds its token) and refuses a
//! `Remove` above its liquidity, and a `Close` while it holds liquidity or
//! owes fees or rewards.
//!
//! **Reading the outcome.** Amounts come from the program's own events
//! (`PositionOpened`, `LiquidityIncreased`, `LiquidityDecreased`, read from
//! its `Program data:` logs), checked against the owner's token balances in
//! the transaction's meta: the owner must have sent what was paid, and
//! received what was transferred, each with its Token-2022 transfer fee. A
//! `Collect` has no event, and is read from the balances, checked against
//! the pool's vaults. A mismatch or a missing event is [`LandedUnread`],
//! never a guessed number.
//!
//! **Rent** is read from the same meta's lamports ([`rent`]): what the
//! position's three accounts (the position, its mint, the owner's token
//! account for it) gain is `rent_deposited_lamports`, what the range's tick
//! arrays gain `rent_spent_lamports`, and what any of them gives back
//! `rent_returned_lamports`. A dynamic tick array gives a released tick's
//! rent back into the position, which returns it at `Close`. Every landed
//! transaction is checked: the owner's lamports moved by exactly the
//! returned rent less the fee and the rent paid.
//!
//! **Not supported**: a Token-2022 mint with a transfer hook (its extra
//! accounts are not passed, so the program refuses), and closing a position
//! minted under SPL Token rather than Token-2022 (`close_position`, which
//! this adapter does not send). Neither wraps SOL nor creates the owner's
//! token accounts (`SPEC.md` §5b's non-goals).

use crate::dex::{Outcome, Prepared, SolanaCost, SolanaTransaction, TxCost};
use crate::liquidity::{
    check_command, unix_now, Deposit, DepositGuard, LandedUnread, LiquidityCapabilities,
    LiquidityCommand, LiquidityEvent, LiquidityExecutor, LiquidityReport, LiquidityRequest,
    PositionId, TokenPair,
};
use crate::solana::rpc::{address_from_slice, TxMeta};
use crate::solana::token::{
    associated_token_account, token_account_amount, ASSOCIATED_TOKEN_PROGRAM, MEMO_PROGRAM,
    SYSTEM_PROGRAM, TOKEN_2022_PROGRAM, TOKEN_PROGRAM,
};
use crate::solana::{SolanaSender, SolanaTxOutcome};
use crate::{Network, Provenance};
use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use base64::Engine;
use borsh::BorshDeserialize;
use orca_whirlpools_client as orca;
use sha2::{Digest, Sha256};
use solana_address::Address;
use solana_hash::Hash;
use solana_instruction::Instruction;
use solana_keypair::Keypair;
use solana_message::{v0, VersionedMessage};
use solana_signature::Signature;
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Orca's Whirlpool program on mainnet.
pub const WHIRLPOOL_PROGRAM: Address = orca::WHIRLPOOL_ID;

/// The authority Orca's program requires `open_position_with_token_extensions`
/// to name (its IDL's default), even when no metadata is written.
const METADATA_UPDATE_AUTH: Address =
    Address::from_str_const("3axbTs2z5GBy6usVbNVoqEgZMng3vZvMnAoX29BFfwhr");

/// Ticks per tick array.
const TICK_ARRAY_SIZE: i32 = 88;

/// The program's tick bounds.
const MIN_TICK: i32 = -443_636;
const MAX_TICK: i32 = 443_636;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Open,
    Add,
    Remove,
    Collect,
    Close,
}

/// What a pool says of itself, as far as a command needs it.
#[derive(Debug, Clone, Copy)]
struct Pool {
    address: Address,
    tick_spacing: u16,
    mint_a: Address,
    mint_b: Address,
    vault_a: Address,
    vault_b: Address,
    program_a: Address,
    program_b: Address,
}

/// A position's accounts, and what it holds.
#[derive(Debug, Clone, Copy)]
struct Position {
    address: Address,
    mint: Address,
    /// The owner's token account for the position's mint.
    token_account: Address,
    /// The token program the position's mint is under.
    token_program: Address,
    whirlpool: Address,
    tick_lower: i32,
    tick_upper: i32,
    liquidity: u128,
    fees_owed: (u64, u64),
    rewards_owed: bool,
}

/// Everything `execute()` needs that is not in `Prepared`, stashed by
/// `prepare()` and consumed exactly once.
#[derive(Debug, Clone)]
struct PendingCommand {
    kind: Kind,
    deadline_unix_secs: u64,
    pool: Pool,
    owner: Address,
    position: Address,
    position_mint: Address,
    position_token_account: Address,
    tick_arrays: [Address; 2],
    /// What `Open` and `Add` deposit at most, per token.
    max: (u64, u64),
}

impl PendingCommand {
    fn owner_accounts(&self) -> (Address, Address) {
        (
            associated_token_account(&self.owner, &self.pool.mint_a, &self.pool.program_a),
            associated_token_account(&self.owner, &self.pool.mint_b, &self.pool.program_b),
        )
    }
}

pub struct WhirlpoolLiquidity {
    sender: Arc<SolanaSender>,
    program: Address,
    pending: Mutex<HashMap<Vec<u8>, PendingCommand>>,
}

impl WhirlpoolLiquidity {
    /// The venue over `sender`, for the Whirlpool program `program`
    /// ([`WHIRLPOOL_PROGRAM`] on mainnet).
    pub fn new(sender: Arc<SolanaSender>, program: Address) -> Self {
        Self {
            sender,
            program,
            pending: Mutex::new(HashMap::new()),
        }
    }

    pub fn sender(&self) -> &Arc<SolanaSender> {
        &self.sender
    }

    fn network(&self) -> Network {
        self.sender.network()
    }

    async fn pool(&self, address: Address) -> Result<Pool> {
        let account = self
            .sender
            .rpc()
            .multiple_accounts(&[address])
            .await?
            .pop()
            .flatten()
            .ok_or_else(|| anyhow!("pool {address} does not exist"))?;
        if account.owner != self.program
            || !account.data.starts_with(&orca::WHIRLPOOL_DISCRIMINATOR)
        {
            bail!(
                "{address} is not a pool of the Whirlpool program {}",
                self.program
            );
        }
        let pool = orca::Whirlpool::from_bytes(&account.data)
            .with_context(|| format!("decoding pool {address}"))?;
        let mints = self
            .sender
            .rpc()
            .multiple_accounts(&[pool.token_mint_a, pool.token_mint_b])
            .await?;
        let program = |i: usize| -> Result<Address> {
            let mint = mints
                .get(i)
                .cloned()
                .flatten()
                .ok_or_else(|| anyhow!("pool {address}'s mint {i} does not exist"))?;
            if mint.owner != TOKEN_PROGRAM && mint.owner != TOKEN_2022_PROGRAM {
                bail!(
                    "pool {address}'s mint {i} is owned by {}, not by a token program",
                    mint.owner
                );
            }
            Ok(mint.owner)
        };
        Ok(Pool {
            address,
            tick_spacing: pool.tick_spacing,
            mint_a: pool.token_mint_a,
            mint_b: pool.token_mint_b,
            vault_a: pool.token_vault_a,
            vault_b: pool.token_vault_b,
            program_a: program(0)?,
            program_b: program(1)?,
        })
    }

    /// The position `id` names, refused unless the owner holds it.
    async fn position(&self, id: &PositionId, owner: Address) -> Result<Position> {
        if id.network != self.network() {
            bail!(
                "position is on network {}, but this WhirlpoolLiquidity's sender is on {}",
                id.network,
                self.network()
            );
        }
        let mint =
            address_from_slice(&id.bytes).context("a Whirlpool position is its 32-byte mint")?;
        let address = position_address(&mint, &self.program)?;
        let accounts = self
            .sender
            .rpc()
            .multiple_accounts(&[address, mint])
            .await?;
        let account = accounts[0]
            .clone()
            .ok_or_else(|| anyhow!("position {mint} does not exist (no account at {address})"))?;
        if account.owner != self.program || !account.data.starts_with(&orca::POSITION_DISCRIMINATOR)
        {
            bail!(
                "{address} is not a position of the Whirlpool program {}",
                self.program
            );
        }
        let state = orca::Position::from_bytes(&account.data)
            .with_context(|| format!("decoding position {address}"))?;
        let token_program = accounts[1]
            .as_ref()
            .map(|m| m.owner)
            .ok_or_else(|| anyhow!("position mint {mint} does not exist"))?;
        let token_account = associated_token_account(&owner, &mint, &token_program);
        let held = match self
            .sender
            .rpc()
            .multiple_accounts(&[token_account])
            .await?
            .pop()
            .flatten()
        {
            Some(data) => token_account_amount(&data)?,
            None => 0,
        };
        if held != 1 {
            bail!("the owner {owner} does not hold position {mint}");
        }
        Ok(Position {
            address,
            mint,
            token_account,
            token_program,
            whirlpool: state.whirlpool,
            tick_lower: state.tick_lower_index,
            tick_upper: state.tick_upper_index,
            liquidity: state.liquidity,
            fees_owed: (state.fee_owed_a, state.fee_owed_b),
            rewards_owed: state.reward_infos.iter().any(|r| r.amount_owed > 0),
        })
    }

    fn tick_arrays(&self, pool: &Pool, lower: i32, upper: i32) -> Result<[(i32, Address); 2]> {
        let at = |tick: i32| -> Result<(i32, Address)> {
            let start = tick_array_start(tick, pool.tick_spacing);
            Ok((
                start,
                tick_array_address(&pool.address, start, &self.program)?,
            ))
        };
        Ok([at(lower)?, at(upper)?])
    }

    fn increase(&self, ctx: &PendingCommand, max: (u64, u64), band: (u128, u128)) -> Instruction {
        let (owner_a, owner_b) = ctx.owner_accounts();
        orca::IncreaseLiquidityByTokenAmountsV2 {
            whirlpool: ctx.pool.address,
            token_program_a: ctx.pool.program_a,
            token_program_b: ctx.pool.program_b,
            memo_program: MEMO_PROGRAM,
            position_authority: ctx.owner,
            position: ctx.position,
            position_token_account: ctx.position_token_account,
            token_mint_a: ctx.pool.mint_a,
            token_mint_b: ctx.pool.mint_b,
            token_owner_account_a: owner_a,
            token_owner_account_b: owner_b,
            token_vault_a: ctx.pool.vault_a,
            token_vault_b: ctx.pool.vault_b,
            tick_array_lower: ctx.tick_arrays[0],
            tick_array_upper: ctx.tick_arrays[1],
        }
        .instruction(orca::IncreaseLiquidityByTokenAmountsV2InstructionArgs {
            method: orca::IncreaseLiquidityMethod::ByTokenAmounts {
                token_max_a: max.0,
                token_max_b: max.1,
                min_sqrt_price: band.0,
                max_sqrt_price: band.1,
            },
            remaining_accounts_info: None,
        })
    }

    /// A v0 transaction of `instructions` paid by the owner, signed by
    /// `co_signer` (a new position's mint) and left for the sender to sign.
    async fn transaction(
        &self,
        instructions: &[Instruction],
        co_signer: Option<&Keypair>,
    ) -> Result<SolanaTransaction> {
        let (blockhash, last_valid_block_height) = self.sender.latest_blockhash().await?;
        let message = v0::Message::try_compile(
            &self.sender.pubkey(),
            instructions,
            &[],
            Hash::new_from_array(blockhash),
        )
        .context("compiling the transaction")?;
        let message = VersionedMessage::V0(message);
        let required = usize::from(message.header().num_required_signatures);
        let mut signatures = vec![Signature::default(); required];
        if let Some(key) = co_signer {
            let slot = message.static_account_keys()[..required]
                .iter()
                .position(|k| *k == key.pubkey())
                .ok_or_else(|| {
                    anyhow!("the transaction needs no signature from {}", key.pubkey())
                })?;
            signatures[slot] = key.sign_message(&message.serialize());
        }
        Ok(SolanaTransaction {
            transaction: VersionedTransaction {
                signatures,
                message,
            },
            last_valid_block_height,
        })
    }

    fn tx_ref(&self, signature: [u8; 64]) -> Option<Vec<u8>> {
        (self.sender.provenance() == Provenance::Landed).then(|| signature.to_vec())
    }

    fn unsettled(
        &self,
        outcome: Outcome,
        cost: SolanaCost,
        at: u64,
        signature: [u8; 64],
    ) -> LiquidityReport {
        LiquidityReport {
            outcome,
            event: None,
            cost: TxCost::Solana(cost),
            at,
            provenance: self.sender.provenance(),
            tx_ref: self.tx_ref(signature),
        }
    }
}

/// Reads a successful command's event from its meta, or explains why it
/// cannot (which the caller turns into `LandedUnread`).
fn read_event(
    network: Network,
    ctx: &PendingCommand,
    meta: &TxMeta,
) -> std::result::Result<LiquidityEvent, String> {
    let events = events(&meta.log_messages);
    let (owner_a, owner_b) = ctx.owner_accounts();
    // What the owner's token accounts lost (sent) and gained (received).
    let delta = |account: &Address| {
        let (before, after) = meta.token_amounts(account);
        (before.saturating_sub(after), after.saturating_sub(before))
    };
    let ((sent_a, received_a), (sent_b, received_b)) = (delta(&owner_a), delta(&owner_b));
    let mine = |e: &Event| e.whirlpool == ctx.pool.address && e.position == ctx.position;
    match ctx.kind {
        Kind::Open | Kind::Add => {
            if ctx.kind == Kind::Open
                && !events
                    .iter()
                    .any(|e| e.kind == EventKind::PositionOpened && mine(e))
            {
                return Err("no PositionOpened event for the new position".into());
            }
            let e = events
                .iter()
                .find(|e| e.kind == EventKind::LiquidityIncreased && mine(e))
                .ok_or("no LiquidityIncreased event for the position")?;
            for (name, amount, fee, sent) in [
                ("token A", e.amounts.0, e.transfer_fees.0, sent_a),
                ("token B", e.amounts.1, e.transfer_fees.1, sent_b),
            ] {
                if amount.saturating_add(fee) != sent {
                    return Err(format!(
                        "the program reports {amount} of {name} deposited with a transfer fee of \
                         {fee}, but the owner sent {sent} per the transaction's token balances"
                    ));
                }
            }
            let paid = TokenPair::new(u128::from(sent_a), u128::from(sent_b));
            Ok(if ctx.kind == Kind::Open {
                LiquidityEvent::Opened {
                    position: PositionId {
                        network,
                        bytes: ctx.position_mint.to_bytes().to_vec(),
                    },
                    liquidity: e.liquidity,
                    paid,
                }
            } else {
                LiquidityEvent::Added {
                    liquidity: e.liquidity,
                    paid,
                }
            })
        }
        Kind::Remove => {
            let e = events
                .iter()
                .find(|e| e.kind == EventKind::LiquidityDecreased && mine(e))
                .ok_or("no LiquidityDecreased event for the position")?;
            for (name, amount, fee, received) in [
                ("token A", e.amounts.0, e.transfer_fees.0, received_a),
                ("token B", e.amounts.1, e.transfer_fees.1, received_b),
            ] {
                if amount.saturating_sub(fee) != received {
                    return Err(format!(
                        "the program reports {amount} of {name} withdrawn with a transfer fee of \
                         {fee}, but the owner received {received} per the transaction's token \
                         balances"
                    ));
                }
            }
            Ok(LiquidityEvent::Removed {
                liquidity: e.liquidity,
                released: TokenPair::new(u128::from(e.amounts.0), u128::from(e.amounts.1)),
                transferred: TokenPair::new(u128::from(received_a), u128::from(received_b)),
            })
        }
        Kind::Collect => {
            // No event: what the owner received is what the vaults gave,
            // less any Token-2022 fee withheld on the way.
            for (name, vault, received) in [
                ("token A", ctx.pool.vault_a, received_a),
                ("token B", ctx.pool.vault_b, received_b),
            ] {
                let (before, after) = meta.token_amounts(&vault);
                let given = before.saturating_sub(after);
                if received > given {
                    return Err(format!(
                        "the owner received {received} of {name} but the pool's vault gave {given}"
                    ));
                }
            }
            Ok(LiquidityEvent::Collected {
                transferred: TokenPair::new(u128::from(received_a), u128::from(received_b)),
            })
        }
        Kind::Close => {
            let left = meta
                .account_keys
                .iter()
                .position(|k| *k == ctx.position)
                .and_then(|i| meta.post_balances.get(i))
                .ok_or("the position's account is not in the transaction")?;
            if *left != 0 {
                return Err(format!(
                    "the position's account still holds {left} lamports"
                ));
            }
            Ok(LiquidityEvent::Closed)
        }
    }
}

/// The rent a transaction moved, `(deposited, spent, returned)`, read from
/// its meta's lamports, account by account: what the position's accounts
/// gain is deposited, what the range's tick arrays gain is spent, and what
/// any of them loses is returned.
///
/// A dynamic tick array gives a tick's rent back when the tick is released
/// (the last liquidity on it removed), into the position's account, which
/// hands it to the owner at `Close`: so a `Remove` can show the same amount
/// returned by a tick array and deposited into the position, and `Close`
/// can return more than `Open` deposited. Over a position's life,
/// `deposited + spent − returned` is exactly the rent the owner is out of
/// pocket; per transaction, the owner's lamports move by `returned − fee −
/// deposited − spent` ([`check_lamports`]).
fn rent(ctx: &PendingCommand, meta: &TxMeta) -> (u64, u64, u64) {
    let (mut deposited, mut spent, mut returned) = (0u64, 0u64, 0u64);
    let mut seen = Vec::new();
    let position_accounts = [ctx.position, ctx.position_mint, ctx.position_token_account];
    for (account, is_tick_array) in position_accounts
        .iter()
        .map(|a| (a, false))
        .chain(ctx.tick_arrays.iter().map(|a| (a, true)))
    {
        if seen.contains(account) {
            continue;
        }
        seen.push(*account);
        let Some(delta) = meta.lamports_delta(account) else {
            continue;
        };
        let amount = u64::try_from(delta.unsigned_abs()).unwrap_or(u64::MAX);
        match (delta > 0, is_tick_array) {
            (true, false) => deposited += amount,
            (true, true) => spent += amount,
            (false, _) => returned += amount,
        }
    }
    (deposited, spent, returned)
}

/// The owner paid the fee and the rent, and received what was returned,
/// and nothing else moved its lamports: otherwise an account this adapter
/// does not know of took or gave rent, and the figures are not to be trusted.
fn check_lamports(
    ctx: &PendingCommand,
    meta: &TxMeta,
    rent: (u64, u64, u64),
) -> std::result::Result<(), String> {
    let (deposited, spent, returned) = rent;
    let expected = i128::from(returned)
        - i128::from(meta.fee_lamports)
        - i128::from(deposited)
        - i128::from(spent);
    let moved = meta
        .lamports_delta(&ctx.owner)
        .ok_or("the owner is not in the transaction")?;
    if moved != expected {
        return Err(format!(
            "the owner's lamports moved by {moved}, but the fee ({}) and the rent deposited ({deposited}), \
             spent ({spent}) and returned ({returned}) account for {expected}",
            meta.fee_lamports
        ));
    }
    Ok(())
}

/// The start of the tick array holding `tick`.
fn tick_array_start(tick: i32, tick_spacing: u16) -> i32 {
    let span = TICK_ARRAY_SIZE * i32::from(tick_spacing);
    tick.div_euclid(span) * span
}

fn position_address(mint: &Address, program: &Address) -> Result<Address> {
    orca::get_position_address(mint, Some(*program))
        .map(|(address, _)| address)
        .map_err(|e| anyhow!("deriving the position address of {mint}: {e:?}"))
}

fn tick_array_address(pool: &Address, start: i32, program: &Address) -> Result<Address> {
    orca::get_tick_array_address(pool, start, Some(*program))
        .map(|(address, _)| address)
        .map_err(|e| anyhow!("deriving the tick array at {start} of {pool}: {e:?}"))
}

fn amount_u64(name: &str, v: u128) -> Result<u64> {
    u64::try_from(v).map_err(|_| anyhow!("{name} {v} is above a Solana token amount's u64"))
}

/// A deposit's sqrt-price band. `check_command` has already refused any
/// guard but `SqrtPriceBand`.
fn band(deposit: &Deposit) -> (u128, u128) {
    match deposit.guard {
        DepositGuard::SqrtPriceBand {
            min_sqrt_price_x64,
            max_sqrt_price_x64,
        } => (min_sqrt_price_x64, max_sqrt_price_x64),
        DepositGuard::MinAmounts(_) => {
            unreachable!("check_command refuses minimum amounts on a sqrt-price band venue")
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventKind {
    PositionOpened,
    LiquidityIncreased,
    LiquidityDecreased,
}

/// One of the program's events, as far as a command reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Event {
    kind: EventKind,
    whirlpool: Address,
    position: Address,
    liquidity: u128,
    amounts: (u64, u64),
    transfer_fees: (u64, u64),
}

/// An Anchor event's discriminator: the first eight bytes of
/// `sha256("event:<name>")`.
fn event_discriminator(name: &str) -> [u8; 8] {
    let digest = Sha256::digest(format!("event:{name}").as_bytes());
    digest[..8].try_into().expect("eight bytes")
}

/// The program's events in a transaction's logs (`Program data:` lines);
/// any other line, or an event of another kind, is skipped.
fn events(logs: &[String]) -> Vec<Event> {
    let opened = event_discriminator("PositionOpened");
    let increased = event_discriminator("LiquidityIncreased");
    let decreased = event_discriminator("LiquidityDecreased");
    logs.iter()
        .filter_map(|line| line.strip_prefix("Program data: "))
        .filter_map(|data| base64::engine::general_purpose::STANDARD.decode(data).ok())
        .filter_map(|bytes| {
            let (tag, body) = bytes.split_at_checked(8)?;
            if tag == opened {
                let e = orca::PositionOpened::try_from_slice(body).ok()?;
                Some(Event {
                    kind: EventKind::PositionOpened,
                    whirlpool: e.whirlpool,
                    position: e.position,
                    liquidity: 0,
                    amounts: (0, 0),
                    transfer_fees: (0, 0),
                })
            } else if tag == increased {
                let e = orca::LiquidityIncreased::try_from_slice(body).ok()?;
                Some(Event {
                    kind: EventKind::LiquidityIncreased,
                    whirlpool: e.whirlpool,
                    position: e.position,
                    liquidity: e.liquidity,
                    amounts: (e.token_a_amount, e.token_b_amount),
                    transfer_fees: (e.token_a_transfer_fee, e.token_b_transfer_fee),
                })
            } else if tag == decreased {
                let e = orca::LiquidityDecreased::try_from_slice(body).ok()?;
                Some(Event {
                    kind: EventKind::LiquidityDecreased,
                    whirlpool: e.whirlpool,
                    position: e.position,
                    liquidity: e.liquidity,
                    amounts: (e.token_a_amount, e.token_b_amount),
                    transfer_fees: (e.token_a_transfer_fee, e.token_b_transfer_fee),
                })
            } else {
                None
            }
        })
        .collect()
}

#[async_trait]
impl LiquidityExecutor for WhirlpoolLiquidity {
    fn capabilities(&self) -> LiquidityCapabilities {
        LiquidityCapabilities::WHIRLPOOL
    }

    async fn prepare(&self, cmd: &LiquidityCommand, req: &LiquidityRequest) -> Result<Prepared> {
        check_command(cmd, req, &self.capabilities())?;
        let owner = address_from_slice(&req.owner).context("LiquidityRequest.owner")?;
        if owner != self.sender.pubkey() {
            bail!(
                "LiquidityRequest.owner ({owner}) is not this WhirlpoolLiquidity's sending address \
                 ({}) — it can only act as its own sender",
                self.sender.pubkey()
            );
        }
        if cmd.network() != self.network() {
            bail!(
                "the command is for network {}, but this WhirlpoolLiquidity's sender is on {}",
                cmd.network(),
                self.network()
            );
        }
        let pending =
            |kind, pool, position: &Position, arrays: [(i32, Address); 2], max| PendingCommand {
                kind,
                deadline_unix_secs: req.deadline_unix_secs,
                pool,
                owner,
                position: position.address,
                position_mint: position.mint,
                position_token_account: position.token_account,
                tick_arrays: [arrays[0].1, arrays[1].1],
                max,
            };

        let (instructions, co_signer, ctx) = match cmd {
            LiquidityCommand::Open { range, deposit } => {
                let pool = self
                    .pool(address_from_slice(&range.pool).context("Range.pool")?)
                    .await?;
                let spacing = i32::from(pool.tick_spacing);
                for (name, tick) in [
                    ("tick_lower", range.tick_lower),
                    ("tick_upper", range.tick_upper),
                ] {
                    if tick % spacing != 0 || !(MIN_TICK..=MAX_TICK).contains(&tick) {
                        bail!(
                            "{name} {tick} is not a tick of pool {}: a multiple of its spacing ({spacing}) \
                             within [{MIN_TICK}, {MAX_TICK}]",
                            pool.address
                        );
                    }
                }
                let max = (
                    amount_u64("the deposit's token0 maximum", deposit.max.token0)?,
                    amount_u64("the deposit's token1 maximum", deposit.max.token1)?,
                );
                let mint = Keypair::new();
                let position = Position {
                    address: position_address(&mint.pubkey(), &self.program)?,
                    mint: mint.pubkey(),
                    token_account: associated_token_account(
                        &owner,
                        &mint.pubkey(),
                        &TOKEN_2022_PROGRAM,
                    ),
                    token_program: TOKEN_2022_PROGRAM,
                    whirlpool: pool.address,
                    tick_lower: range.tick_lower,
                    tick_upper: range.tick_upper,
                    liquidity: 0,
                    fees_owed: (0, 0),
                    rewards_owed: false,
                };
                let arrays = self.tick_arrays(&pool, range.tick_lower, range.tick_upper)?;
                let ctx = pending(Kind::Open, pool, &position, arrays, max);

                let mut instructions = Vec::new();
                let existing = self
                    .sender
                    .rpc()
                    .multiple_accounts(&[arrays[0].1, arrays[1].1])
                    .await?;
                for (i, (start, address)) in arrays.iter().enumerate() {
                    if existing[i].is_none() && (i == 0 || arrays[1].1 != arrays[0].1) {
                        instructions.push(
                            orca::InitializeDynamicTickArray {
                                whirlpool: pool.address,
                                funder: owner,
                                tick_array: *address,
                                system_program: SYSTEM_PROGRAM,
                            }
                            .instruction(
                                orca::InitializeDynamicTickArrayInstructionArgs {
                                    start_tick_index: *start,
                                    idempotent: true,
                                },
                            ),
                        );
                    }
                }
                instructions.push(
                    orca::OpenPositionWithTokenExtensions {
                        funder: owner,
                        owner,
                        position: position.address,
                        position_mint: position.mint,
                        position_token_account: position.token_account,
                        whirlpool: pool.address,
                        token2022_program: TOKEN_2022_PROGRAM,
                        system_program: SYSTEM_PROGRAM,
                        associated_token_program: ASSOCIATED_TOKEN_PROGRAM,
                        metadata_update_auth: METADATA_UPDATE_AUTH,
                    }
                    .instruction(
                        orca::OpenPositionWithTokenExtensionsInstructionArgs {
                            tick_lower_index: range.tick_lower,
                            tick_upper_index: range.tick_upper,
                            with_token_metadata_extension: false,
                        },
                    ),
                );
                instructions.push(self.increase(&ctx, max, band(deposit)));
                (instructions, Some(mint), ctx)
            }
            LiquidityCommand::Add { position, deposit } => {
                let position = self.position(position, owner).await?;
                let pool = self.pool(position.whirlpool).await?;
                let max = (
                    amount_u64("the deposit's token0 maximum", deposit.max.token0)?,
                    amount_u64("the deposit's token1 maximum", deposit.max.token1)?,
                );
                let arrays = self.tick_arrays(&pool, position.tick_lower, position.tick_upper)?;
                let ctx = pending(Kind::Add, pool, &position, arrays, max);
                (vec![self.increase(&ctx, max, band(deposit))], None, ctx)
            }
            LiquidityCommand::Remove {
                position,
                liquidity,
                min_out,
            } => {
                let position = self.position(position, owner).await?;
                if *liquidity > position.liquidity {
                    bail!(
                        "a Remove of {liquidity} is above position {}'s liquidity, {}",
                        position.mint,
                        position.liquidity
                    );
                }
                let pool = self.pool(position.whirlpool).await?;
                let arrays = self.tick_arrays(&pool, position.tick_lower, position.tick_upper)?;
                let ctx = pending(Kind::Remove, pool, &position, arrays, (0, 0));
                let (owner_a, owner_b) = ctx.owner_accounts();
                let instruction = orca::DecreaseLiquidityV2 {
                    whirlpool: pool.address,
                    token_program_a: pool.program_a,
                    token_program_b: pool.program_b,
                    memo_program: MEMO_PROGRAM,
                    position_authority: owner,
                    position: position.address,
                    position_token_account: position.token_account,
                    token_mint_a: pool.mint_a,
                    token_mint_b: pool.mint_b,
                    token_owner_account_a: owner_a,
                    token_owner_account_b: owner_b,
                    token_vault_a: pool.vault_a,
                    token_vault_b: pool.vault_b,
                    tick_array_lower: ctx.tick_arrays[0],
                    tick_array_upper: ctx.tick_arrays[1],
                }
                .instruction(orca::DecreaseLiquidityV2InstructionArgs {
                    liquidity_amount: *liquidity,
                    token_min_a: amount_u64("the minimum of token0", min_out.token0)?,
                    token_min_b: amount_u64("the minimum of token1", min_out.token1)?,
                    remaining_accounts_info: None,
                });
                (vec![instruction], None, ctx)
            }
            LiquidityCommand::Collect { position } => {
                let position = self.position(position, owner).await?;
                let pool = self.pool(position.whirlpool).await?;
                let arrays = self.tick_arrays(&pool, position.tick_lower, position.tick_upper)?;
                let ctx = pending(Kind::Collect, pool, &position, arrays, (0, 0));
                let (owner_a, owner_b) = ctx.owner_accounts();
                let mut instructions = Vec::new();
                if position.liquidity > 0 {
                    instructions.push(
                        orca::UpdateFeesAndRewards {
                            whirlpool: pool.address,
                            position: position.address,
                            tick_array_lower: ctx.tick_arrays[0],
                            tick_array_upper: ctx.tick_arrays[1],
                        }
                        .instruction(),
                    );
                }
                instructions.push(
                    orca::CollectFeesV2 {
                        whirlpool: pool.address,
                        position_authority: owner,
                        position: position.address,
                        position_token_account: position.token_account,
                        token_mint_a: pool.mint_a,
                        token_mint_b: pool.mint_b,
                        token_owner_account_a: owner_a,
                        token_vault_a: pool.vault_a,
                        token_owner_account_b: owner_b,
                        token_vault_b: pool.vault_b,
                        token_program_a: pool.program_a,
                        token_program_b: pool.program_b,
                        memo_program: MEMO_PROGRAM,
                    }
                    .instruction(orca::CollectFeesV2InstructionArgs {
                        remaining_accounts_info: None,
                    }),
                );
                (instructions, None, ctx)
            }
            LiquidityCommand::Close { position } => {
                let position = self.position(position, owner).await?;
                if position.liquidity > 0 || position.fees_owed != (0, 0) || position.rewards_owed {
                    bail!(
                        "a Close on position {}, which holds liquidity ({}) or owes fees ({}, {}) or \
                         rewards: remove and collect first",
                        position.mint,
                        position.liquidity,
                        position.fees_owed.0,
                        position.fees_owed.1
                    );
                }
                if position.token_program != TOKEN_2022_PROGRAM {
                    bail!(
                        "position {} is minted under {}, and only a Token-2022 position is closed here",
                        position.mint,
                        position.token_program
                    );
                }
                let pool = self.pool(position.whirlpool).await?;
                let arrays = self.tick_arrays(&pool, position.tick_lower, position.tick_upper)?;
                let ctx = pending(Kind::Close, pool, &position, arrays, (0, 0));
                let instruction = orca::ClosePositionWithTokenExtensions {
                    position_authority: owner,
                    receiver: owner,
                    position: position.address,
                    position_mint: position.mint,
                    position_token_account: position.token_account,
                    token2022_program: TOKEN_2022_PROGRAM,
                }
                .instruction();
                (vec![instruction], None, ctx)
            }
        };

        let tx = self.transaction(&instructions, co_signer.as_ref()).await?;
        self.pending
            .lock()
            .unwrap()
            .insert(tx.transaction.message.serialize(), ctx);
        Ok(Prepared::Solana(tx))
    }

    async fn execute(&self, prepared: &Prepared) -> Result<LiquidityReport> {
        let tx = prepared.solana_transaction()?;
        let ctx = self
            .pending
            .lock()
            .unwrap()
            .remove(&tx.transaction.message.serialize())
            .ok_or_else(|| {
                anyhow!(
                    "execute() called with a Prepared value this WhirlpoolLiquidity instance did not \
                     produce, or already consumed"
                )
            })?;
        let now = unix_now();
        if now >= ctx.deadline_unix_secs {
            bail!(
                "LiquidityRequest.deadline_unix_secs ({}) has already passed (now {now}) — refusing \
                 to send",
                ctx.deadline_unix_secs
            );
        }
        if matches!(ctx.kind, Kind::Open | Kind::Add) {
            for (mint, program, max) in [
                (ctx.pool.mint_a, ctx.pool.program_a, ctx.max.0),
                (ctx.pool.mint_b, ctx.pool.program_b, ctx.max.1),
            ] {
                if max > 0 {
                    self.sender
                        .ensure_balance(mint.to_bytes(), program.to_bytes(), max)
                        .await?;
                }
            }
        }

        match self.sender.send_and_confirm(tx).await? {
            SolanaTxOutcome::Success {
                slot,
                signature,
                cost,
                meta,
            } => {
                let unread = |reason| LandedUnread {
                    tx_ref: signature.to_vec(),
                    reason,
                };
                let event = read_event(self.network(), &ctx, &meta).map_err(unread)?;
                let (deposited, spent, returned) = rent(&ctx, &meta);
                check_lamports(&ctx, &meta, (deposited, spent, returned)).map_err(unread)?;
                Ok(LiquidityReport {
                    outcome: Outcome::Success,
                    event: Some(event),
                    cost: TxCost::Solana(SolanaCost {
                        rent_deposited_lamports: deposited,
                        rent_spent_lamports: spent,
                        rent_returned_lamports: returned,
                        ..cost
                    }),
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
            Provenance::Landed => "whirlpool-liquidity-live",
            Provenance::Simulated => "whirlpool-liquidity-fork",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::solana::rpc::TokenBalance;

    const NETWORK: Network = Network::Solana {
        genesis_hash: [1; 32],
    };

    #[test]
    fn a_tick_array_starts_at_the_multiple_of_its_span_at_or_below_the_tick() {
        // Spacing 1: arrays of 88 ticks.
        assert_eq!(tick_array_start(4, 1), 0);
        assert_eq!(tick_array_start(87, 1), 0);
        assert_eq!(tick_array_start(88, 1), 88);
        assert_eq!(tick_array_start(-1, 1), -88);
        assert_eq!(tick_array_start(-88, 1), -88);
        assert_eq!(tick_array_start(-89, 1), -176);
        // Spacing 64: arrays of 5,632 ticks.
        assert_eq!(tick_array_start(-17_000, 64), -22_528);
    }

    #[test]
    fn the_event_discriminator_is_the_one_the_program_logs() {
        // The first eight bytes of a `Traded` event a Whirlpool swap logged
        // on mainnet, 1 October 2026.
        assert_eq!(
            event_discriminator("Traded"),
            [0xe1, 0xca, 0x49, 0xaf, 0x93, 0x2b, 0xa0, 0x96]
        );
    }

    fn logged(name: &str, body: impl borsh::BorshSerialize) -> String {
        let mut bytes = event_discriminator(name).to_vec();
        bytes.extend(borsh::to_vec(&body).unwrap());
        format!(
            "Program data: {}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        )
    }

    #[test]
    fn events_are_read_from_program_data_lines_and_nothing_else() {
        let (pool, position) = (Address::new_unique(), Address::new_unique());
        let logs = vec![
            "Program whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc invoke [1]".to_string(),
            logged(
                "LiquidityIncreased",
                orca::LiquidityIncreased {
                    whirlpool: pool,
                    position,
                    tick_lower_index: -8,
                    tick_upper_index: 16,
                    liquidity: 123,
                    token_a_amount: 10,
                    token_b_amount: 20,
                    token_a_transfer_fee: 0,
                    token_b_transfer_fee: 1,
                },
            ),
            "Program data: not base64!".to_string(),
            logged("Traded", 7u8),
        ];
        assert_eq!(
            events(&logs),
            vec![Event {
                kind: EventKind::LiquidityIncreased,
                whirlpool: pool,
                position,
                liquidity: 123,
                amounts: (10, 20),
                transfer_fees: (0, 1),
            }]
        );
    }

    fn pending(kind: Kind) -> PendingCommand {
        let pool = Pool {
            address: Address::new_unique(),
            tick_spacing: 1,
            mint_a: Address::new_unique(),
            mint_b: Address::new_unique(),
            vault_a: Address::new_unique(),
            vault_b: Address::new_unique(),
            program_a: TOKEN_PROGRAM,
            program_b: TOKEN_PROGRAM,
        };
        PendingCommand {
            kind,
            deadline_unix_secs: unix_now() + 60,
            pool,
            owner: Address::new_unique(),
            position: Address::new_unique(),
            position_mint: Address::new_unique(),
            position_token_account: Address::new_unique(),
            tick_arrays: [Address::new_unique(), Address::new_unique()],
            max: (0, 0),
        }
    }

    /// A meta whose accounts are `keys`, with these lamports and these
    /// token amounts before and after.
    fn meta(
        keys: Vec<Address>,
        lamports: Vec<(u64, u64)>,
        tokens: Vec<(usize, u64, u64)>,
        logs: Vec<String>,
    ) -> TxMeta {
        let balance = |i: usize, amount: u64| TokenBalance {
            account_index: i,
            mint: Address::new_unique(),
            owner: None,
            amount,
        };
        TxMeta {
            slot: 1,
            err: None,
            fee_lamports: 10_000,
            units_consumed: 1,
            account_keys: keys,
            pre_balances: lamports.iter().map(|l| l.0).collect(),
            post_balances: lamports.iter().map(|l| l.1).collect(),
            pre_token_balances: tokens.iter().map(|t| balance(t.0, t.1)).collect(),
            post_token_balances: tokens.iter().map(|t| balance(t.0, t.2)).collect(),
            log_messages: logs,
        }
    }

    #[test]
    fn an_open_is_read_from_its_events_and_checked_against_what_the_owner_sent() {
        let ctx = pending(Kind::Open);
        let (owner_a, owner_b) = ctx.owner_accounts();
        let opened = logged(
            "PositionOpened",
            orca::PositionOpened {
                whirlpool: ctx.pool.address,
                position: ctx.position,
                tick_lower_index: -8,
                tick_upper_index: 16,
            },
        );
        let increased = |a: u64| {
            logged(
                "LiquidityIncreased",
                orca::LiquidityIncreased {
                    whirlpool: ctx.pool.address,
                    position: ctx.position,
                    tick_lower_index: -8,
                    tick_upper_index: 16,
                    liquidity: 5_000,
                    token_a_amount: a,
                    token_b_amount: 7,
                    token_a_transfer_fee: 0,
                    token_b_transfer_fee: 0,
                },
            )
        };
        let keys = vec![
            ctx.owner,
            owner_a,
            owner_b,
            ctx.position,
            ctx.position_mint,
            ctx.position_token_account,
            ctx.tick_arrays[0],
            ctx.tick_arrays[1],
        ];
        // The owner pays the fee (10,000), the deposit and the new tick array.
        let lamports = vec![
            (20_000_000, 11_595_760),
            (1, 1),
            (1, 1),
            (0, 2_394_240),
            (0, 3_000_000),
            (0, 2_000_000),
            (100, 100),
            (0, 1_000_000),
        ];
        let tokens = vec![(1, 100, 90), (2, 50, 43)];
        let m = meta(
            keys.clone(),
            lamports.clone(),
            tokens.clone(),
            vec![opened.clone(), increased(10)],
        );
        let liquidity = read_event(NETWORK, &ctx, &m).unwrap();
        assert_eq!(
            liquidity,
            LiquidityEvent::Opened {
                position: PositionId {
                    network: NETWORK,
                    bytes: ctx.position_mint.to_bytes().to_vec(),
                },
                liquidity: 5_000,
                paid: TokenPair::new(10, 7),
            }
        );
        // Position accounts gained a deposit; one tick array was created.
        assert_eq!(
            rent(&ctx, &m),
            (2_394_240 + 3_000_000 + 2_000_000, 1_000_000, 0)
        );
        check_lamports(&ctx, &m, rent(&ctx, &m)).unwrap();
        // A lamport the figures do not account for.
        let mut off = m.clone();
        off.post_balances[0] -= 1;
        assert!(check_lamports(&ctx, &off, rent(&ctx, &off))
            .unwrap_err()
            .contains("moved by"));

        // The program says 11 went in, the owner sent 10: not a number to trust.
        let m = meta(
            keys.clone(),
            lamports.clone(),
            tokens.clone(),
            vec![opened, increased(11)],
        );
        assert!(read_event(NETWORK, &ctx, &m)
            .unwrap_err()
            .contains("sent 10"));
        // No PositionOpened: the new position is not shown to exist.
        let m = meta(keys, lamports, tokens, vec![increased(10)]);
        assert!(read_event(NETWORK, &ctx, &m)
            .unwrap_err()
            .contains("PositionOpened"));
    }

    #[test]
    fn a_remove_transfers_at_once_and_a_close_returns_the_deposit() {
        let ctx = pending(Kind::Remove);
        let (owner_a, owner_b) = ctx.owner_accounts();
        let decreased = logged(
            "LiquidityDecreased",
            orca::LiquidityDecreased {
                whirlpool: ctx.pool.address,
                position: ctx.position,
                tick_lower_index: -8,
                tick_upper_index: 16,
                liquidity: 5_000,
                token_a_amount: 9,
                token_b_amount: 6,
                token_a_transfer_fee: 0,
                token_b_transfer_fee: 0,
            },
        );
        let m = meta(
            vec![ctx.owner, owner_a, owner_b],
            vec![(1, 1); 3],
            vec![(1, 90, 99), (2, 43, 49)],
            vec![decreased],
        );
        assert_eq!(
            read_event(NETWORK, &ctx, &m).unwrap(),
            LiquidityEvent::Removed {
                liquidity: 5_000,
                released: TokenPair::new(9, 6),
                transferred: TokenPair::new(9, 6),
            }
        );

        // A released tick's rent moves from its dynamic tick array into the
        // position: returned by one, deposited into the other, and the owner
        // pays only the fee.
        let keys = vec![ctx.owner, ctx.position, ctx.tick_arrays[0]];
        let m = meta(
            keys,
            vec![
                (20_000, 10_000),
                (2_394_240, 3_953_280),
                (3_480_000, 1_920_960),
            ],
            vec![],
            vec![],
        );
        assert_eq!(rent(&ctx, &m), (1_559_040, 0, 1_559_040));
        check_lamports(&ctx, &m, rent(&ctx, &m)).unwrap();

        let ctx = PendingCommand {
            kind: Kind::Close,
            ..ctx
        };
        let keys = vec![
            ctx.owner,
            ctx.position,
            ctx.position_mint,
            ctx.position_token_account,
        ];
        let m = meta(
            keys,
            vec![
                (1, 7_384_241),
                (2_394_240, 0),
                (3_000_000, 0),
                (2_000_000, 0),
            ],
            vec![],
            vec![],
        );
        assert_eq!(
            read_event(NETWORK, &ctx, &m).unwrap(),
            LiquidityEvent::Closed
        );
        assert_eq!(rent(&ctx, &m), (0, 0, 7_394_240));
        check_lamports(&ctx, &m, rent(&ctx, &m)).unwrap();
    }

    #[test]
    fn a_collect_is_what_the_owner_received_from_the_vaults() {
        let ctx = pending(Kind::Collect);
        let (owner_a, owner_b) = ctx.owner_accounts();
        let keys = vec![
            ctx.owner,
            owner_a,
            owner_b,
            ctx.pool.vault_a,
            ctx.pool.vault_b,
        ];
        let m = meta(
            keys.clone(),
            vec![(1, 1); 5],
            vec![(1, 0, 3), (2, 0, 0), (3, 100, 97), (4, 50, 50)],
            vec![],
        );
        assert_eq!(
            read_event(NETWORK, &ctx, &m).unwrap(),
            LiquidityEvent::Collected {
                transferred: TokenPair::new(3, 0),
            }
        );
        // The owner received more than the vault gave.
        let m = meta(
            keys,
            vec![(1, 1); 5],
            vec![(1, 0, 3), (2, 0, 0), (3, 100, 98), (4, 50, 50)],
            vec![],
        );
        assert!(read_event(NETWORK, &ctx, &m).is_err());
    }
}
