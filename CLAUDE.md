# Gossip Market

Peer-to-peer agents that gossip wants over libp2p and settle rings of two to four parties through
the NEAR Intents verifier (`intents.near`). Rust workspace. Read `README.md` first, then
`docs/SPEC.md` for the protocol and `docs/DEV-NOTES.md` for where the verifier's rules come from.

## Commands

```sh
cargo run -- sim                 # the whole market in one process; must end with SIMULATION PASSED
cargo test                       # default members: intents, chain, market, node
cargo test -p conformance        # against NEAR's own engine; slow to build, run before changing intents/ or market/ring.rs
cargo clippy --workspace --all-targets
cargo fmt --all
scripts/demo.sh                  # devnet + three nodes in tmux
```

## Layout

- `intents/` what the verifier reads and its rules in memory. Pure, no async.
- `chain/` NEAR JSON-RPC client, transaction encoding, the local devnet.
- `market/` wants, the book, ring search and sizing.
- `node/` the `gossipmarket` binary: `node.rs` is the agent's event loop.
- `conformance/` our rules against `defuse-core`, pinned by commit.
- `demo/` configs for the devnet and for mainnet. `node/src/config.rs` tests that they form a ring.

## Rules

- Anything that changes what is signed, how fees round, or how a ring is sized must pass
  `cargo test -p conformance`. The contract is the specification; our ledger only mirrors it.
- Refusal texts in `intents::Ledger` are the contract's, word for word. Do not reword them.
- No floats near amounts. Base units are `u128`, written as decimal text in JSON.
- A node believes signatures and the chain, never a peer's say-so. New message handlers check
  everything they are told.
- Comments say why, not what. Tests are named as sentences and start with a comment on what
  breaks if they fail.
- `rustfmt.toml` sets `max_width = 120`. Clippy is clean; keep it clean.
- Keys never go in git: `keys/`, `*.key` and `.env` are ignored. `devnet:<name>` keys are public
  by design and refused against anything but localhost.
- Never run anything that spends on mainnet without being asked. `preflight` and `verify` only
  read.
- The README claims only what has been run. If a claim changes, change the README in the same
  commit.
- Conventional commits (`feat:`, `fix:`, `docs:`, `test:`).
