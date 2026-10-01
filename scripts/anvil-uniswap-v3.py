#!/usr/bin/env python3
"""Deploy a Uniswap v3 pool, its NonfungiblePositionManager and SwapRouter
onto a local anvil node, for the fork-mode liquidity tests.

The fork-mode tests (`IMPLEMENTATION_PLAN.md` Phase 13) run `EvmLiquidity`
over a fork sender against whatever manager `LIQUIDITY_FORK_MANAGER` names.
Pointed at an anvil fork of a real chain, that is the deployed manager. This
script is for when no fork RPC is at hand: it deploys Uniswap's own published
bytecode (the `@uniswap/v3-core` and `@uniswap/v3-periphery` npm packages,
plus the test ERC-20 from `@uniswap/v2-core`) onto a plain anvil node, creates
and initialises one 0.3 % pool at price 1, and prints the environment the
tests read. Nothing is compiled here and no bytecode is kept in this
repository: the packages are downloaded from the npm registry when the script
runs, into a cache directory.

Usage:
    anvil --disable-code-size-limit &
    eval "$(scripts/anvil-uniswap-v3.py)"
    cargo test -- against_anvil

Needs only Python 3's standard library, and network access to
registry.npmjs.org on the first run.
"""

import argparse
import io
import json
import os
import sys
import tarfile
import tempfile
import time
import urllib.request

PACKAGES = {
    "v3-core": "https://registry.npmjs.org/@uniswap/v3-core/-/v3-core-1.0.1.tgz",
    "v3-periphery": "https://registry.npmjs.org/@uniswap/v3-periphery/-/v3-periphery-1.4.4.tgz",
    "v2-core": "https://registry.npmjs.org/@uniswap/v2-core/-/v2-core-1.0.1.tgz",
}
ARTIFACTS = {
    "factory": ("v3-core", "package/artifacts/contracts/UniswapV3Factory.sol/UniswapV3Factory.json"),
    "pool": ("v3-core", "package/artifacts/contracts/UniswapV3Pool.sol/UniswapV3Pool.json"),
    "manager": (
        "v3-periphery",
        "package/artifacts/contracts/NonfungiblePositionManager.sol/NonfungiblePositionManager.json",
    ),
    "router": ("v3-periphery", "package/artifacts/contracts/SwapRouter.sol/SwapRouter.json"),
    "erc20": ("v2-core", "package/build/ERC20.json"),
}
# The pool init code hash the published NonfungiblePositionManager computes
# pool addresses with. If the published pool bytecode hashes differently,
# every mint's callback check fails.
POOL_INIT_CODE_HASH = "0xe34f199b19b2b4f47f68442619d555527d244f78a3297ea89325f843f87b8b54"
FEE = 3000
PRICE_ONE_SQRT_X96 = 2**96
# NonfungiblePositionManager and SwapRouter store a WETH9 address and a
# token-descriptor address, and only call them for native ETH and for
# tokenURI. The liquidity port never uses either, so any non-zero address
# will do.
UNUSED = "0x000000000000000000000000000000000000dEaD"


def rpc(url, method, params):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    req = urllib.request.Request(url, body, {"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=60) as resp:
        reply = json.load(resp)
    if "error" in reply:
        raise RuntimeError(f"{method}: {reply['error']}")
    return reply["result"]


def selector(url, signature):
    return rpc(url, "web3_sha3", ["0x" + signature.encode().hex()])[:10]


def word(value):
    if isinstance(value, str):
        return value.lower().removeprefix("0x").rjust(64, "0")
    return format(value, "064x")


def bytecode(cache, name):
    package, path = ARTIFACTS[name]
    with open(os.path.join(cache, package, path)) as f:
        code = json.load(f)["bytecode"]
    return code if code.startswith("0x") else "0x" + code


def fetch(cache):
    for package, url in PACKAGES.items():
        target = os.path.join(cache, package)
        if os.path.isdir(target):
            continue
        with urllib.request.urlopen(url, timeout=120) as resp:
            data = resp.read()
        with tarfile.open(fileobj=io.BytesIO(data)) as tar:
            tar.extractall(target, filter="data")


def send(url, sender, to, data):
    tx = {"from": sender, "data": data}
    if to is not None:
        tx["to"] = to
    tx_hash = rpc(url, "eth_sendTransaction", [tx])
    for _ in range(600):
        receipt = rpc(url, "eth_getTransactionReceipt", [tx_hash])
        if receipt is not None:
            if receipt["status"] != "0x1":
                raise RuntimeError(f"transaction {tx_hash} reverted")
            return receipt
        time.sleep(0.1)
    raise RuntimeError(f"no receipt for {tx_hash} — is automine on?")


def deploy(url, sender, code, *args):
    return send(url, sender, None, code + "".join(word(a) for a in args))["contractAddress"]


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--rpc", default="http://127.0.0.1:8545", help="the anvil node's URL")
    parser.add_argument(
        "--cache",
        default=os.path.join(tempfile.gettempdir(), "venue-ports-uniswap-artifacts"),
        help="where the npm packages are unpacked",
    )
    args = parser.parse_args()
    url = args.rpc

    version = rpc(url, "web3_clientVersion", [])
    if not version.startswith("anvil"):
        sys.exit(f"{url} reports {version!r}: this script only deploys to an anvil node")

    fetch(args.cache)
    pool_hash = rpc(url, "web3_sha3", [bytecode(args.cache, "pool")])
    if pool_hash != POOL_INIT_CODE_HASH:
        sys.exit(f"the published pool bytecode hashes to {pool_hash}, not {POOL_INIT_CODE_HASH}")

    deployer = rpc(url, "eth_accounts", [])[0]
    erc20 = bytecode(args.cache, "erc20")
    tokens = sorted(
        (deploy(url, deployer, erc20, 10**30) for _ in range(2)), key=lambda a: int(a, 16)
    )
    factory = deploy(url, deployer, bytecode(args.cache, "factory"))
    send(
        url,
        deployer,
        factory,
        selector(url, "createPool(address,address,uint24)") + word(tokens[0]) + word(tokens[1]) + word(FEE),
    )
    pool = "0x" + rpc(
        url,
        "eth_call",
        [
            {
                "to": factory,
                "data": selector(url, "getPool(address,address,uint24)")
                + word(tokens[0])
                + word(tokens[1])
                + word(FEE),
            },
            "latest",
        ],
    )[-40:]
    send(url, deployer, pool, selector(url, "initialize(uint160)") + word(PRICE_ONE_SQRT_X96))
    manager = deploy(url, deployer, bytecode(args.cache, "manager"), factory, UNUSED, UNUSED)
    router = deploy(url, deployer, bytecode(args.cache, "router"), factory, UNUSED)

    for name, value in [
        ("EVM_ANVIL_RPC_URL", url),
        ("LIQUIDITY_FORK_MANAGER", manager),
        ("LIQUIDITY_FORK_TOKEN0", tokens[0]),
        ("LIQUIDITY_FORK_TOKEN1", tokens[1]),
        ("LIQUIDITY_FORK_POOL_KEY", f"fee:{FEE}"),
        ("LIQUIDITY_FORK_SWAP_ROUTER", router),
    ]:
        print(f"export {name}={value}")


if __name__ == "__main__":
    main()
