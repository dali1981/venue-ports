//! Signing and nonce management shared within the EVM adapter family
//! (`IMPLEMENTATION_PLAN.md` Phase 5). Only `EvmLive` needs either of
//! these: `EvmSimulated` never signs anything (`eth_call` needs no
//! signature), and `EvmStub` never touches the network at all.

use alloy_consensus::{SignableTransaction, TxEip1559};
use alloy_primitives::{keccak256, Address, Signature, B256};
use anyhow::{Context, Result};
use k256::ecdsa::SigningKey;
use tokio::sync::Mutex;

/// One signing key, plus the lock that serializes every send made with
/// it. `SPEC.md` §5 requires "one nonce in flight per chain at a time" —
/// the simplest way to guarantee that without a nonce cache that can fall
/// out of sync is to never allow a second send to even start building its
/// transaction until the previous one has fully resolved (landed,
/// reverted, or timed out). `EvmLive::send_and_confirm` holds this lock
/// for exactly that whole sequence, fetching a fresh `eth_getTransactionCount`
/// each time rather than tracking a counter.
pub struct Signer {
    key: SigningKey,
    pub address: Address,
    send_lock: Mutex<()>,
}

impl Signer {
    pub fn from_private_key_hex(hex_key: &str) -> Result<Self> {
        let trimmed = hex_key.trim_start_matches("0x");
        let bytes = hex::decode(trimmed).context("private key must be valid hex")?;
        let key = SigningKey::from_slice(&bytes)
            .context("private key is not a valid secp256k1 scalar")?;
        let address = address_from_signing_key(&key);
        Ok(Self {
            key,
            address,
            send_lock: Mutex::new(()),
        })
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

    /// Runs `f` while holding this signer's send lock — see the struct
    /// docs for why the lock must span the whole send, not just signing.
    pub async fn with_send_lock<F, Fut, T>(&self, f: F) -> T
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        let _guard = self.send_lock.lock().await;
        f().await
    }
}

/// The standard uncompressed-public-key-to-address derivation: drop the
/// SEC1 tag byte, `keccak256` the remaining 64 bytes, keep the low 20.
fn address_from_signing_key(key: &SigningKey) -> Address {
    let point = key.verifying_key().to_encoded_point(false);
    let hash = keccak256(&point.as_bytes()[1..]);
    Address::from_slice(&hash[12..])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pure key-derivation math, no network — gated on the same env vars
    /// `EvmLive`'s real-network tests use so a real (disposable,
    /// testnet-only) wallet never has to be hardcoded here to prove this
    /// crate's address derivation agrees with whatever tool generated it.
    /// A no-op for every contributor and CI run that hasn't opted in.
    #[test]
    fn derives_the_address_a_real_wallet_reports_for_its_own_key() {
        let (Ok(key_hex), Ok(expected_addr)) = (
            std::env::var("EVM_LIVE_SIGNER_KEY"),
            std::env::var("EVM_LIVE_SENDER_ADDRESS"),
        ) else {
            eprintln!(
                "skipping: EVM_LIVE_SIGNER_KEY / EVM_LIVE_SENDER_ADDRESS not set"
            );
            return;
        };

        let signer = Signer::from_private_key_hex(&key_hex).expect("valid private key");
        let expected: Address = expected_addr
            .parse()
            .expect("EVM_LIVE_SENDER_ADDRESS must be a valid EVM address");
        assert_eq!(signer.address, expected);
    }
}
