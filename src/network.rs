/// The network a command is for (`SPEC.md` §4). An EIP-155 id means nothing
/// on Solana, so a command names its network instead. An adapter belongs to
/// one network and refuses a command for any other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Network {
    Evm {
        chain_id: u64,
    },
    /// Identified by its genesis hash, which the adapter checks against its
    /// node at construction.
    Solana {
        genesis_hash: [u8; 32],
    },
}

impl Network {
    pub fn evm(chain_id: u64) -> Self {
        Network::Evm { chain_id }
    }

    /// The EIP-155 chain id, on an EVM network.
    pub fn chain_id(&self) -> Option<u64> {
        match self {
            Network::Evm { chain_id } => Some(*chain_id),
            Network::Solana { .. } => None,
        }
    }

    /// The length of an address on this network: 20 bytes on EVM, 32 on
    /// Solana.
    pub fn address_len(&self) -> usize {
        match self {
            Network::Evm { .. } => 20,
            Network::Solana { .. } => 32,
        }
    }
}

/// The network's CAIP-2 id: `eip155:<chain id>`, or `solana:` and the first
/// 32 characters of the genesis hash in base58.
impl std::fmt::Display for Network {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Network::Evm { chain_id } => write!(f, "eip155:{chain_id}"),
            Network::Solana { genesis_hash } => {
                let base58 = solana_hash::Hash::new_from_array(*genesis_hash).to_string();
                write!(f, "solana:{}", &base58[..base58.len().min(32)])
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn displays_as_caip2() {
        assert_eq!(Network::evm(8453).to_string(), "eip155:8453");
        let mainnet: solana_hash::Hash = "5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d"
            .parse()
            .unwrap();
        let solana = Network::Solana {
            genesis_hash: mainnet.to_bytes(),
        };
        assert_eq!(
            solana.to_string(),
            "solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp"
        );
        assert_eq!(solana.chain_id(), None);
        assert_eq!(solana.address_len(), 32);
    }
}
