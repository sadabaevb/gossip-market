# Development notes

What was read, what was checked against what, and the decisions behind the code. For whoever
picks this up next, including us.

## Where the verifier's rules come from

Read from the source of [near/intents](https://github.com/near/intents) at commit
`fce0936b1699e0591204ce152a78afc1c1338b35`, the same commit `conformance` builds:

| Rule | File |
|---|---|
| `token_diff`: the fee is taken from each amount given, the batch must net per token | `contracts/defuse/core/src/intents/token_diff.rs` |
| The fee is `ceil(amount × pips / 1 000 000)` | `crates/primitives/fees/src/lib.rs` |
| Versioned nonces: magic `5628f6c6`, version, salt, deadline; plain nonces "will be prohibited" | `contracts/defuse/core/src/nonce/versioned.rs` |
| NEP-413 payloads and what is hashed | `contracts/defuse/core/src/payload/` |
| The order of checks and the invariant | `contracts/defuse/core/src/engine/` |
| The wording of refusals | `contracts/defuse/core/src/error.rs` |
| A deposit's `msg` is the receiving account, or JSON if it starts with `{` | `contracts/defuse/src/tokens/mod.rs` |
| `ft_withdraw`: the amount leaves the signer's balance outside the netting, no fee; zero is "invalid intent" | `contracts/defuse/core/src/intents/tokens.rs`, `contracts/defuse/core/src/engine/state/cached.rs` |
| A failed `ft_transfer` after a withdraw puts the amount back | `contracts/defuse/src/contract/tokens/nep141/withdraw.rs` |

`intents::Ledger` re-implements the subset agents use: `token_diff` and plain `ft_withdraw` in
NEP-413 envelopes signed by implicit accounts. It is a few hundred lines and exists so that the simulation, the devnet and the
tests need no chain. `conformance` is what keeps it from drifting: every case runs through both
and must get the same answer, including the text of a refusal.

Not implemented in the ledger: other intent kinds, `ft_withdraw` with `msg` or `storage_deposit`,
other signing standards, named accounts with registered keys, NFTs beyond the fee rule, salt
rotation.

## Checked against mainnet on 12 November 2026

Read-only, through a public RPC node, at block 224190010:

- `intents.near` `fee` returned 1 (one pip) and `current_salt` returned `252812b3`.
- The four `preflight` probes got the expected answers: a well-formed leg from an unfunded key
  under a versioned nonce and under a plain nonce both reached "insufficient balance or
  overflow"; a wrong salt got "invalid salt"; an altered leg got "invalid signature".
- A failed view call comes back as `result.error` holding the panic's debug text. The devnet
  answers in the same shape, so `chain::rpc` parses one format.

## Funded on mainnet, 12 and 13 November 2026

- A ring of three settled in one `execute_intents` sent by one of the makers:
  `CX1a9fxurEHkKm5TTyAHUnjd6muqqcYy61mhpv9AY1k4`. `verify` reads it back as three valid
  signatures that net to zero after the 1-pip fee. It burnt 18.98 Tgas, 0.0011 NEAR.
- The first attempts were turned away before inclusion with `NotEnoughBalance`: every
  transaction attaches 300 Tgas, and NEAR reserves it at a pessimistic price, 0.3009 NEAR, while
  the agents held 0.1. `chain::rpc` reads that as "no answer", so the nodes retried and spent
  nothing. The submitter settled once it held 0.31.
- `withdraw` took 0.05 USDC out to an ordinary account, which received exactly 50000 units:
  `C9ediagwiPguKwYjdKZAbFbaTdLzrjXV7v81voU4QTna`, 33.36 Tgas over six receipts.

The encoding in `chain/src/tx.rs` was pinned to a vector produced by near-api-js 4.0.4 before
this; these are the first transactions from it to run on mainnet.

## Decisions

- **Rust workspace, five crates.** `intents` is pure and has no async, so the rules can be tested
  and reused without a runtime. `conformance` is kept out of `default-members` because it pulls
  and builds NEAR's contract crates; everything else builds from crates.io alone.
- **No NEAR SDK.** Transactions are borsh-encoded by hand (there is only one kind: a single
  function call) and RPC is plain JSON over `reqwest`. The NEAR crates would have been most of the
  build, for one action.
- **One key for everything.** Implicit accounts need no registration in the verifier, and an
  ed25519 libp2p identity carries its public key in the peer id, so a want's `maker` is enough to
  find, verify and pay someone.
- **Rings up to four.** Each maker is one more signature to wait for and one more way to fall
  through. Three is what a pair matcher cannot do; four is headroom.
- **No proposer's cut.** Sizing gives every surplus to the receiver. A proposer that kept the
  spread would have a reason to race, and the turn-taking would stop working.
- **Versioned nonces by default.** They are what the contract is moving to, and they give each
  leg an expiry the contract enforces twice.
- **Withdraw by intent, not by the contract's `ft_withdraw` method.** The method is simpler (the
  caller is the owner) but cannot be tried first. A signed intent is dry-run by `simulate_intents`
  for free before any gas is spent, and goes through the engine `conformance` runs.
- **The proposer shares the bundle before submitting.** Otherwise a proposer that died after
  collecting signatures would lock everyone until the deadline for nothing.
- **Outcomes come from `is_nonce_used`.** It is a free view call, it cannot be forged by a peer,
  and it is right whoever submitted.
- **The devnet speaks NEAR JSON-RPC.** One client for both. The alternative, a trait with a mock
  behind it, would have left the real client untested until mainnet.
- **A terminal, not a web page.** Three terminals with nothing between them is the argument.
- **MIT, edition 2021, `max_width = 120`,** tests named as sentences with a comment on what breaks
  if they fail: the conventions of NearBy.

## Known gaps

- Every transaction attaches the maximum 300 Tgas, so an agent needs about 0.31 NEAR free to
  submit or withdraw, though a ring of three burns 19 Tgas and a withdraw 33. Less would do; how
  much a ring of four burns has not been measured.
- No command to send an agent's NEAR for gas back off its account.
- No NAT traversal: nodes need a reachable address or a shared local network.
- No language model in the loop: wants come from a config file or standard input.
- The ring search is bounded (20,000 steps per starting want, 64 rings) rather than clever. It is
  enough for hundreds of wants, not for a busy book.
- `proc-macro-error2`, a transitive dependency, prints a future-incompatibility note on build.
  It is harmless today.
