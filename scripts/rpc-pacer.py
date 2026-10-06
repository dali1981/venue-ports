#!/usr/bin/env python3
"""Pace a Surfpool fork's calls to a public Solana node.

Surfpool fetches mainnet accounts on demand from its upstream RPC, and one
transaction can need a burst of them: its accounts, its programs' data, its
lookup tables. The free public nodes answer such a burst with HTTP 429, and
Surfpool's datasource client gives up after five retries 500 ms apart. The
fork then answers `simulateTransaction` with "Internal error" and drops a
transaction sent with preflight skipped, which the sender sees as `Expired`.

This proxy sits between the fork and its upstream. It forwards every JSON-RPC
POST to the upstream one at a time, at most one call per `--interval`
seconds, and retries a 429 after a backoff (the upstream's `Retry-After` when
it gives one) until `--deadline` seconds have passed. Every other answer is
returned as the upstream gave it. It keeps nothing.

Some free nodes also price a call by its keys. Solana Vibe Station's public
node refuses a `getMultipleAccounts` of more than 3 keys with the same 429
however long the wait, and allows about one of 3 keys every 8 s, while it
answers `getAccountInfo` about twice a second (measured 1 October 2026). With
`--split-accounts`, a `getMultipleAccounts` is sent as one `getAccountInfo`
per key, with the same options, in order, and their accounts are joined into
one answer carrying the first call's context. Any other answer from one of
them (an error, a refusal) is returned as it came.

Usage:
    uv run --no-project scripts/rpc-pacer.py --upstream https://public.rpc.solanavibestation.com \
        --port 8979 --interval 0.5 --split-accounts &
    surfpool start --ci --no-deploy --rpc-url http://127.0.0.1:8979 --port 8980 --ws-port 8981 --airdrop-amount 0
    SURFPOOL_RPC_URL=http://127.0.0.1:8980 cargo test --lib against_surfpool -- --test-threads=1

Needs only Python 3's standard library.
"""

import argparse
import http.server
import json
import sys
import threading
import time
import urllib.error
import urllib.request


def parse_args():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--upstream", required=True, help="the Solana node to forward to")
    parser.add_argument("--port", type=int, default=8979, help="where the fork reaches this proxy")
    parser.add_argument("--interval", type=float, default=1.0, help="seconds between two upstream calls")
    parser.add_argument(
        "--deadline",
        type=float,
        default=25.0,
        help="seconds one call may spend retrying 429s; under Surfpool's 30 s per attempt",
    )
    parser.add_argument(
        "--split-accounts",
        action="store_true",
        help="send a getMultipleAccounts as one getAccountInfo per key",
    )
    return parser.parse_args()


class Pacer:
    """One upstream call at a time, `interval` seconds apart."""

    def __init__(self, upstream, interval, deadline):
        self.upstream = upstream
        self.interval = interval
        self.deadline = deadline
        self.lock = threading.Lock()
        self.last = 0.0
        self.calls = 0
        self.throttled = 0

    def forward(self, body, content_type):
        """The upstream's status, content type and body for one POST body,
        with one line on stderr saying what was asked and how it went."""
        started = time.monotonic()
        give_up = started + self.deadline
        backoff = self.interval
        throttled = 0
        while True:
            with self.lock:
                wait = self.last + self.interval - time.monotonic()
                if wait > 0:
                    time.sleep(wait)
                self.last = time.monotonic()
                self.calls += 1
                status, answer_type, answer, retry_after = self.call(body, content_type)
            if status != 429:
                break
            throttled += 1
            self.throttled += 1
            pause = float(retry_after) if retry_after and retry_after.isdigit() else backoff
            if time.monotonic() + pause > give_up:
                answer_type, answer = "text/plain", b"rpc-pacer: still throttled at the deadline"
                break
            time.sleep(pause)
            backoff = min(backoff * 2, 8.0)
        print(
            f"rpc-pacer: {methods(body)} -> {status}, {throttled} throttled, "
            f"{time.monotonic() - started:.1f} s",
            file=sys.stderr,
            flush=True,
        )
        return status, answer_type, answer

    def call(self, body, content_type):
        """One POST to the upstream: its status, content type, body and
        `Retry-After`."""
        request = urllib.request.Request(
            self.upstream, data=body, headers={"Content-Type": content_type}, method="POST"
        )
        try:
            with urllib.request.urlopen(request, timeout=20) as response:
                return response.status, response.headers.get("Content-Type"), response.read(), None
        except urllib.error.HTTPError as error:
            return (
                error.code,
                error.headers.get("Content-Type"),
                error.read(),
                error.headers.get("Retry-After"),
            )


def forward_split(pacer, body, content_type, split_accounts):
    """`pacer.forward`, with a `getMultipleAccounts` sent as one
    `getAccountInfo` per key when `split_accounts` is set, its accounts
    joined into one answer."""
    try:
        request = json.loads(body)
    except ValueError:
        request = None
    if (
        not split_accounts
        or not isinstance(request, dict)
        or request.get("method") != "getMultipleAccounts"
    ):
        return pacer.forward(body, content_type)
    keys, options = request["params"][0], request["params"][1:]
    joined = None
    for key in keys:
        one = dict(request, method="getAccountInfo", params=[key, *options])
        status, answer_type, answer = pacer.forward(json.dumps(one).encode(), content_type)
        try:
            reply = json.loads(answer) if status == 200 else None
        except ValueError:
            reply = None
        if reply is None or "result" not in reply:
            return status, answer_type, answer
        if joined is None:
            joined = dict(reply, result={"context": reply["result"]["context"], "value": []})
        joined["result"]["value"].append(reply["result"]["value"])
    if joined is None:
        return pacer.forward(body, content_type)
    return 200, "application/json", json.dumps(joined).encode()


def methods(body):
    """The JSON-RPC methods in a request body (a batch has several)."""
    try:
        request = json.loads(body)
    except ValueError:
        return "(not JSON)"
    calls = request if isinstance(request, list) else [request]
    return ",".join(str(c.get("method")) for c in calls if isinstance(c, dict))


def handler_for(pacer, split_accounts):
    class Handler(http.server.BaseHTTPRequestHandler):
        def do_POST(self):
            body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
            content_type = self.headers.get("Content-Type", "application/json")
            try:
                status, answer_type, answer = forward_split(pacer, body, content_type, split_accounts)
            except OSError as error:
                status, answer_type, answer = 502, "text/plain", f"rpc-pacer: {error}".encode()
            self.send_response(status)
            self.send_header("Content-Type", answer_type or "application/json")
            self.send_header("Content-Length", str(len(answer)))
            self.end_headers()
            self.wfile.write(answer)

        def log_message(self, format, *args):
            pass

    return Handler


def main():
    args = parse_args()
    pacer = Pacer(args.upstream, args.interval, args.deadline)
    server = http.server.ThreadingHTTPServer(("127.0.0.1", args.port), handler_for(pacer, args.split_accounts))
    print(
        f"rpc-pacer: 127.0.0.1:{args.port} -> {args.upstream}, one call per {args.interval} s",
        file=sys.stderr,
        flush=True,
    )
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        print(f"rpc-pacer: {pacer.calls} calls, {pacer.throttled} throttled", file=sys.stderr)


if __name__ == "__main__":
    main()
