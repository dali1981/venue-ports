//! SPL Token and Token-2022, as far as a Solana adapter needs them: the
//! programs' ids, an owner's associated token account, and the amount a
//! token account holds. Both token programs lay a token account's mint,
//! owner and amount out the same way in its first 72 bytes.

use crate::solana::rpc::{AccountData, SolanaRpc};
use anyhow::{anyhow, bail, Result};
use solana_address::Address;

pub const SYSTEM_PROGRAM: Address = Address::from_str_const("11111111111111111111111111111111");
pub const TOKEN_PROGRAM: Address =
    Address::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
pub const TOKEN_2022_PROGRAM: Address =
    Address::from_str_const("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
pub const ASSOCIATED_TOKEN_PROGRAM: Address =
    Address::from_str_const("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
pub const MEMO_PROGRAM: Address =
    Address::from_str_const("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr");

/// `owner`'s associated token account for `mint` under `token_program`.
pub fn associated_token_account(owner: &Address, mint: &Address, token_program: &Address) -> Address {
    Address::find_program_address(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        &ASSOCIATED_TOKEN_PROGRAM,
    )
    .0
}

/// The amount a token account holds: bytes 64 to 72, little-endian.
pub fn token_account_amount(account: &AccountData) -> Result<u64> {
    let bytes = account
        .data
        .get(64..72)
        .ok_or_else(|| anyhow!("a token account of {} bytes", account.data.len()))?;
    Ok(u64::from_le_bytes(bytes.try_into().expect("eight bytes")))
}

/// The token program that owns `mint`: SPL Token or Token-2022.
pub async fn token_program_of(rpc: &SolanaRpc, mint: &Address) -> Result<Address> {
    let account = rpc
        .multiple_accounts(&[*mint])
        .await?
        .pop()
        .flatten()
        .ok_or_else(|| anyhow!("mint {mint} does not exist"))?;
    if account.owner != TOKEN_PROGRAM && account.owner != TOKEN_2022_PROGRAM {
        bail!(
            "{mint} is owned by {}, not by a token program",
            account.owner
        );
    }
    Ok(account.owner)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A known associated token account: USDC's for a fixed owner, as
    /// `spl-associated-token-account` derives it.
    #[test]
    fn derives_the_associated_token_account() {
        let owner: Address = "DzuFuBM9JeCZiDZ3NFF5DktduNKVSwdMdbeb1MJFRRov".parse().unwrap();
        let usdc: Address = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".parse().unwrap();
        assert_eq!(
            associated_token_account(&owner, &usdc, &TOKEN_PROGRAM).to_string(),
            "85BzAkMpW4bCL7w4zZDKWXsaM7mPdDdwrPpp6px7H1So"
        );
    }

    #[test]
    fn reads_a_token_accounts_amount() {
        let mut data = vec![0u8; 165];
        data[64..72].copy_from_slice(&123_456_789u64.to_le_bytes());
        let account = AccountData {
            lamports: 2_039_280,
            owner: TOKEN_PROGRAM,
            data,
        };
        assert_eq!(token_account_amount(&account).unwrap(), 123_456_789);
    }
}
