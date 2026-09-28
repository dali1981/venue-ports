//! `sol!` definitions for the position managers `EvmLiquidity` speaks to
//! (`SPEC.md` §5b). Every call takes struct arguments with `int24` and
//! `uint24` fields, so none of it is hand-encoded.
//!
//! Uniswap v3's `NonfungiblePositionManager` is the reference;
//! PancakeSwap v3's is ABI-identical. Aerodrome and Velodrome Slipstream's
//! differ only in `mint`: `tickSpacing` (an `int24`) takes the place of
//! `fee`, and a trailing `sqrtPriceX96` creates the pool when non-zero —
//! this crate always sends zero, because it never creates a pool. Their
//! `increaseLiquidity`, `decreaseLiquidity`, `collect`, `burn`, `ownerOf`
//! and events are Uniswap v3's. That is from their published source, and
//! has not yet been run against a Slipstream deployment
//! (`IMPLEMENTATION_PLAN.md` Phase 13).

/// The calls and events every supported manager shares.
pub mod manager {
    alloy_sol_types::sol! {
        struct IncreaseLiquidityParams {
            uint256 tokenId;
            uint256 amount0Desired;
            uint256 amount1Desired;
            uint256 amount0Min;
            uint256 amount1Min;
            uint256 deadline;
        }

        struct DecreaseLiquidityParams {
            uint256 tokenId;
            uint128 liquidity;
            uint256 amount0Min;
            uint256 amount1Min;
            uint256 deadline;
        }

        struct CollectParams {
            uint256 tokenId;
            address recipient;
            uint128 amount0Max;
            uint128 amount1Max;
        }

        function increaseLiquidity(IncreaseLiquidityParams calldata params)
            external payable returns (uint128 liquidity, uint256 amount0, uint256 amount1);
        function decreaseLiquidity(DecreaseLiquidityParams calldata params)
            external payable returns (uint256 amount0, uint256 amount1);
        function collect(CollectParams calldata params)
            external payable returns (uint256 amount0, uint256 amount1);
        function burn(uint256 tokenId) external payable;
        function ownerOf(uint256 tokenId) external view returns (address owner);
        function factory() external view returns (address);
        /// Returns `(nonce, operator, token0, token1, fee or tickSpacing,
        /// tickLower, tickUpper, liquidity, …)`. Its fifth field is a
        /// `uint24` on Uniswap v3 and an `int24` on Slipstream, so the
        /// return is read by word rather than declared here.
        function positions(uint256 tokenId) external view;

        event IncreaseLiquidity(uint256 indexed tokenId, uint128 liquidity, uint256 amount0, uint256 amount1);
        event DecreaseLiquidity(uint256 indexed tokenId, uint128 liquidity, uint256 amount0, uint256 amount1);
        event Collect(uint256 indexed tokenId, address recipient, uint256 amount0, uint256 amount1);
        /// ERC-721's `Transfer`: the same signature, and so the same topic,
        /// as ERC-20's, but with the token id indexed (four topics).
        event Transfer(address indexed from, address indexed to, uint256 indexed tokenId);
    }
}

/// `mint` on Uniswap v3's manager and its ABI-identical forks.
pub mod uniswap_v3 {
    alloy_sol_types::sol! {
        struct MintParams {
            address token0;
            address token1;
            uint24 fee;
            int24 tickLower;
            int24 tickUpper;
            uint256 amount0Desired;
            uint256 amount1Desired;
            uint256 amount0Min;
            uint256 amount1Min;
            address recipient;
            uint256 deadline;
        }

        function mint(MintParams calldata params)
            external payable returns (uint256 tokenId, uint128 liquidity, uint256 amount0, uint256 amount1);
    }
}

/// `mint` on Aerodrome and Velodrome Slipstream's manager.
pub mod slipstream {
    alloy_sol_types::sol! {
        struct MintParams {
            address token0;
            address token1;
            int24 tickSpacing;
            int24 tickLower;
            int24 tickUpper;
            uint256 amount0Desired;
            uint256 amount1Desired;
            uint256 amount0Min;
            uint256 amount1Min;
            address recipient;
            uint256 deadline;
            uint160 sqrtPriceX96;
        }

        function mint(MintParams calldata params)
            external payable returns (uint256 tokenId, uint128 liquidity, uint256 amount0, uint256 amount1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_sol_types::{SolCall, SolEvent};

    /// Uniswap v3's selectors, as its verified `NonfungiblePositionManager`
    /// exposes them — a mistyped field in a struct above changes its
    /// selector, and this catches it.
    #[test]
    fn uniswap_v3_selectors_match_the_deployed_manager() {
        assert_eq!(uniswap_v3::mintCall::SELECTOR, [0x88, 0x31, 0x64, 0x56]);
        assert_eq!(
            manager::increaseLiquidityCall::SELECTOR,
            [0x21, 0x9f, 0x5d, 0x17]
        );
        assert_eq!(
            manager::decreaseLiquidityCall::SELECTOR,
            [0x0c, 0x49, 0xcc, 0xbe]
        );
        assert_eq!(manager::collectCall::SELECTOR, [0xfc, 0x6f, 0x78, 0x65]);
        assert_eq!(manager::burnCall::SELECTOR, [0x42, 0x96, 0x6c, 0x68]);
        assert_eq!(manager::ownerOfCall::SELECTOR, [0x63, 0x52, 0x21, 0x1e]);
        assert_eq!(manager::positionsCall::SELECTOR, [0x99, 0xfb, 0xab, 0x88]);
    }

    #[test]
    fn slipstream_mint_takes_tick_spacing_and_a_trailing_sqrt_price() {
        assert_eq!(
            slipstream::mintCall::SIGNATURE,
            "mint((address,address,int24,int24,int24,uint256,uint256,uint256,uint256,address,uint256,uint160))"
        );
    }

    #[test]
    fn erc721_transfer_shares_the_erc20_topic() {
        assert_eq!(
            manager::Transfer::SIGNATURE_HASH,
            crate::evm::erc20::transfer_topic()
        );
    }
}
