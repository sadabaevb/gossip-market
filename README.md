# Gossip Market

Agents that find each other over libp2p, gossip what they want, and settle N-party trades through
the NEAR Intents verifier, with no relay, matcher or coordinator between them.

Alice has USDC and wants compute credits. Bob has credits and wants a dataset. Carol has the
dataset and wants USDC. No two of them can trade. Here the three find each other, each signs its
own leg, and one `execute_intents` call settles all three legs or none. Then the node the others
first dialed is switched off, and the market carries on.

Built for NEAR HACKATHON, November 2026. Tracks: AI Swarms, Agent Economy.

## Try it

You need a recent stable Rust (built and tested with 1.97) and nothing else: no account, no
funds, and no network once the crates are downloaded.

```sh
cargo run -- sim
```

In a few seconds this starts a local verifier and three agents on your machine, and prints what
each one does. You should see, in order:

1. Bob and Carol dial only Alice, and end up connected to each other.
2. A ring of three settles in one transaction. The transaction is then read back and checked from
   scratch: three valid signatures, every token netting to zero.
3. Alice exits. Bob and Carol open new wants and settle a trade between the two of them.
4. `SIMULATION PASSED`.

Then the tests:

```sh
cargo test                  # 82 tests, under half a minute after the first build
cargo test -p conformance   # 7 tests against the verifier's own engine; fetches near/intents, takes a few minutes the first time
```

### A hundred agents

```sh
cargo run --release -- sim --agents 100
```

A hundred agents in one process, each told the addresses of two or three others, with wants over
five tokens everyone prices alike, so that rings compete for the same wants. Planted among them
are rings of two, three and four, pairs of rings that need the same want, and agents with several
wants; ten agents are set to quit partway, some in the middle of a ring. Every planted ring is
checked to close before the agents start. It prints each settlement as it happens and a summary
at the end. It passes when something settled, no ring is left among the live agents' wants and
none is locked, every transaction reads back valid, every balance adds up, and every bundle that
went out to a live maker settled or had its transaction refused by the verifier. `--seed` gives
another market.

On 14 November 2026, on an Apple M2 Pro doing other work at the time, seeds 1 to 5 each passed:
62 to 79 rings settled, the last of them 48 to 73 seconds after the last agent started. Of the
357 rings in those five runs, 70 had three or four makers: one trade in five that no pair of
agents could have made. The run
keeps four to five cores busy; with fewer it takes longer, and the market has ten minutes to
clear. `sim --agents 30` is a smaller market of the same kind: two runs took 3 and 13 seconds.
Run one at a time; two at once run out of local ports. `docs/DEV-NOTES.md` has the numbers, the
earlier runs on an Apple M4 Max, and what the runs found and fixed.

### Watch it

```sh
cargo run --release -- sim --agents 100 --web
```

It prints an address on localhost and waits a few seconds for you to open it. The page shows the
hundred agents as dots in a circle. Each ring is drawn between its makers as it settles: blue for
two makers, gold for three, pink for four. Beside them it counts the rings, the share of them that
no pair of agents could have settled, the agents that quit, and at the end the checks of the run.
You should see a few dots turn to dashed grey in the first seconds, lines accumulate for a minute
or so, and `SIMULATION PASSED` in green. The page stays up while it is open.

Without building anything, open `docs/watch/index.html` in a browser: it plays back a run of a
hundred agents recorded on 14 November 2026, 74 rings, 19 of them of three makers.
`--record docs/watch/run.js` records another.

### Take a seat yourself

```sh
cargo run --release -- seat
```

It prints an address on localhost. The page is the rehearsal of `mcp`, described next, with a
person where the AI agent sits: you are Alice with 50 USDC, Bob gives credits for data and Carol
data for USDC, so no two of the three can trade. Press "Place want" with the amounts it suggests.
Within a few seconds three arrows close a ring between the three of you and you hold 103.039
CREDITS; "Verify it against the verifier" reads the transaction back and ends with `VERIFIED`. The
column on the right shows each call the page made and the node's answer, word for word as an AI
agent reads them. Ask for 200 CREDITS instead and nothing settles: after 20 seconds the node says
so, and the want can be cancelled.

The seat is the rehearsal only. A page on localhost answers whoever on the machine asks, which is
no way to hold a real account.

### Hand the market to an AI agent

```sh
cargo build --release
claude mcp add gossipmarket -- "$PWD/target/release/gossipmarket" mcp
```

`gossipmarket mcp` serves the market as eight tools over the Model Context Protocol, on standard
input and output: read the book, place and cancel a want, wait for a settlement, check it against
the verifier. The agent decides what to want; the node finds the ring and settles it. With no
config it is a rehearsal like `sim`, with nothing to set up: a local verifier, and Bob and Carol
with wants that no two of the three can trade on. The agent is Alice, holds 50 USDC and has no
wants yet. Ask it to buy compute credits. Opened in this directory, Claude Code offers the same
server from `.mcp.json`; any other client starts `gossipmarket mcp` the way it starts any stdio
server.

By hand, without an agent:

```sh
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list_open_wants","arguments":{}}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"place_want","arguments":{"give":"10 USDC","take":"95 CREDITS"}}}' \
  '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"wait_for_settlement","arguments":{}}}' \
  | cargo run -q -- mcp
```

The first answer lists Bob's and Carol's wants. The third starts with `SETTLED ring … of 3 makers,
with bob … and carol …: you gave 10 USDC and got 103.039 CREDITS`, and names the transaction that
`verify_settlement` reads back.

`--config demo/mainnet/alice.toml` serves a real node instead of the rehearsal. Both generations
of the protocol are answered: revision 2026-07-28 request by request, and the earlier ones after
their `initialize`. What was run on 14 November 2026: the tools over standard input as above, and
the replies checked against the protocol's published JSON schemas for 2026-07-28 and 2025-11-25.
No AI client had been connected to it when this was written.

### On screen, as separate processes

```sh
scripts/demo.sh             # needs tmux
```

A local verifier on top, Alice and Bob below it with wants that cannot meet. Press Enter in the
third pane to start Carol and watch the ring close. Type into any pane:

```
want 50 CREDITS for 2 DATA
cancel 2
quit
```

`docs/DEMO.md` has the full walkthrough and what to say over it.

### Against mainnet, for free

```sh
cargo run -- preflight --config demo/mainnet/alice.toml
```

A throwaway key that holds nothing signs a leg exactly as a node would, and `intents.near` is
asked for a dry run. The contract answers "insufficient balance", which means it accepted the
signature, the account's key, the nonce and the deadline, and stopped only at the money. More
probes check that a plain nonce is still read, that a stale salt and an altered leg are refused,
and that a withdraw is read. Nothing is sent or spent.

### On mainnet, with cents

`docs/DEMO.md` walks through it: make three keys, fund them with NEAR for gas (0.35 each: a
transaction reserves 0.3 and burns about 0.001) and a few cents of USDC, USDT and wrapped NEAR,
start three nodes, and check the settlement with

```sh
cargo run -- verify <tx hash> --sender <account> --config demo/mainnet/alice.toml
```

which reads the transaction from a public RPC node and recomputes everything. An ordinary node
forgets a transaction after about two days: for an older one add
`--rpc https://archival-rpc.mainnet.fastnear.com`. What is left comes back out with

```sh
cargo run -- withdraw USDC --to you.near --config demo/mainnet/alice.toml
```

which the contract dry-runs for free before the agent sends it.

## What is on NEAR, and what is not

| | Where it happens |
|---|---|
| Discovery | libp2p: mDNS on a local network, Kademlia beyond it |
| Wants | signed by the maker, gossiped, relayed by everyone |
| Agreeing on a ring | directly between the makers, request and response |
| Settlement | one `execute_intents` on `intents.near`, sent by whichever maker gets there first |
| Custody | the verifier contract. No node ever holds anyone else's tokens or keys |

The verifier is the only thing the agents share. It checks every signature and that the batch nets
to zero per token after its fee, then moves all balances or none. That rule is what makes a
coordinator unnecessary: a leg is safe to sign and hand to a stranger, because the only thing
anyone can do with it is give the signer what it asked for.

One ed25519 key is the agent's libp2p peer id, its NEAR implicit account, its signer inside the
verifier and the payer of gas. There is nothing to register anywhere.

## How a ring settles

```
 alice            bob              carol            intents.near
   | want (signed) | want (signed)   | want (signed)      |
   |<---- gossip: every node hears every want ---->|      |
   |               |                 |                    |
   |      each finds the same ring; the lowest account proposes
   |<-- propose ---|---- propose --->|                    |
   |--- signed --->|<--- signed -----|                    |
   |               |-- simulate_intents (free) ---------->|
   |<-- bundle ----|---- bundle ---->|   (all three legs, to every maker)
   |               |-- execute_intents ------------------>|  all legs or none
   |               |                 |                    |
   |-- is my nonce spent? (each maker asks the chain itself) ->|
```

- Every node searches the wants it has heard for the rings it is in, so every maker of a ring
  finds that ring, sized and named the same way. The makers of a ring take turns proposing it,
  lowest account first, a couple of seconds apart, so usually exactly one does.
- A maker signs only if the ring nets at the fee it reads from the contract itself, and its leg is
  within the want it last announced: no more given, no worse a price.
- Before submitting, the proposer hands every maker the complete bundle. If the proposer goes
  quiet, any maker submits it. A ring needs one honest maker with gas, not an honest proposer.
- Nobody takes a settlement on anyone's word. The maker that submitted has its transaction's
  result; every other maker asks the chain whether its own nonce was spent.
- A ring has no middleman to keep a spread. Whatever a maker gives beyond what the next one asked
  goes to the receiver.

`docs/SPEC.md` has the message formats, the sizing arithmetic and the timings.

## What is in the repo

| Crate | What it is |
|---|---|
| `intents` | What the verifier reads: NEP-413 signed `token_diff` and `ft_withdraw` intents, nonces, the fee rounding, and the verifier's rules over balances in memory |
| `chain` | The NEAR JSON-RPC calls a node makes, transaction encoding, and a local devnet that answers the same calls |
| `market` | Signed wants, the book, and the ring search and sizing |
| `node` | The `gossipmarket` binary: the agent, its screen, the simulation and the page that draws it, the tools for an AI agent (`mcp`) and the page that puts a person in its seat (`seat`), `preflight`, `verify` and `withdraw` |
| `conformance` | Runs the bundles this code signs through the verifier's own engine (`defuse-core`, pinned) and compares answers with ours, word for word |

## What was checked, and how

- **Against the contract's code.** `conformance` builds NEAR's `defuse-core` at a pinned commit and
  runs our signed bundles through its engine next to our own rules, and requires the same answer
  from both, down to the wording of a refusal: a ring settling twice, a missing leg, an altered
  leg, another account's signature, an expired deadline, a stale salt, more given than held, and
  300 random batches that are one unit off. A further 400 random cases size rings with `market`
  at fees from zero to one percent, in amounts from single units to hundreds of NEAR in yocto, and
  every ring the search returns settles in the engine exactly as sized. An `ft_withdraw` signed
  here is read by the engine as the same token, receiver and amount, and stays outside a ring's
  netting when both are in one batch.
- **Against the live contract.** On 12 November 2026 the four `preflight` probes were run against
  `intents.near` on mainnet and got the expected answers. The fee was 1 pip (a millionth). A fifth
  probe, a withdraw, was added with `withdraw` and got its expected answer the same day.
- **Against misbehaving peers.** `node/src/tests.rs` runs whole markets in which a proposer
  collects signatures and vanishes, a maker never answers, signatures are withheld until they
  expire, or a stranger proposes rings it is not in. The honest nodes settle anyway, get their
  wants back, or turn the stranger away.
- **On mainnet, with funds.** On 12 November 2026 three agents on one machine settled a ring of
  three through `intents.near` in one `execute_intents`: 0.1 USDC, 0.03 wrapped NEAR and 0.1 USDT,
  each leg signed by its own maker, 19 Tgas burnt
  ([`CX1a9fxurEHkKm5TTyAHUnjd6muqqcYy61mhpv9AY1k4`](https://nearblocks.io/txns/CX1a9fxurEHkKm5TTyAHUnjd6muqqcYy61mhpv9AY1k4);
  [Pikespeak](https://pikespeak.ai/transaction-viewer/CX1a9fxurEHkKm5TTyAHUnjd6muqqcYy61mhpv9AY1k4)
  lists it under "Intent" as three swaps, each intent by its own maker).
  This reads it back from an archival node and ends with `VERIFIED` (run again on 14 November;
  the endpoint in the config had dropped the transaction by then):

  ```sh
  cargo run -- verify CX1a9fxurEHkKm5TTyAHUnjd6muqqcYy61mhpv9AY1k4 \
    --sender 6b19f425761ce66fa55fa235e0844fba46da0d63908723db7e68b98e6743e3cc \
    --config demo/mainnet/alice.toml --rpc https://archival-rpc.mainnet.fastnear.com
  ```

  Then `withdraw` took 0.05 USDC from one agent back out of the verifier to an ordinary account
  ([`C9ediagwiPguKwYjdKZAbFbaTdLzrjXV7v81voU4QTna`](https://nearblocks.io/txns/C9ediagwiPguKwYjdKZAbFbaTdLzrjXV7v81voU4QTna)).

## What this does not claim

- **No language model is bundled or called here.** A node holds wants and follows the protocol;
  what to want is decided outside it: by a person at the screen, by a program on
  `gossipmarket node --plain`, or by an AI agent through `gossipmarket mcp`, with whatever model
  that agent runs on. This repo is the part an agent cannot improvise: finding counterparties with
  no server and settling with nobody to trust.
- **It is not a better swap venue.** For liquid pairs, professional market makers behind the
  Intents solver relay quote tighter and faster. This is for trades the relay does not serve:
  agent to agent, long-tail assets, three or four parties with nobody holding inventory.
- **Nothing trades unless the counterparties are online** and their wants happen to close.
- **Gossip is open.** A node keeps at most 16 wants per maker and 10,000 in all, and wants expire
  in seconds, but there is no Sybil resistance and no reputation. A maker can announce a want it
  cannot pay for: the dry run refuses the ring, nothing is spent, and the others try again later.
- **A signed leg is a free option until its deadline.** A proposer who collects signatures and
  sits on them locks the other makers' wants for 30 seconds, and 10 more while each maker makes
  sure nothing settled at the last moment. It cannot take anything, and a maker refuses deadlines
  more than a minute ahead.
- **A maker's "busy" is taken at its word.** A maker that declines a try because its want is
  committed elsewhere is asked again later, and the proposer does not step aside for another ring
  meanwhile. One that keeps saying so, in the ring that comes first for someone's want, keeps that
  want out of every other ring. It takes nothing.
- **Not everything a peer sends is bounded.** The tries a node has been asked into and the
  settlements it has heard of are kept in tables with no cap: the first for up to about 100
  seconds an entry, the second for as long as the node runs. `docs/DEV-NOTES.md` lists these and
  the other known gaps.
- **Wants are public.** Anyone listening sees who wants what at which price.
- **The devnet is not a chain.** It is the verifier's rules over balances in memory, behind the
  same JSON-RPC calls, for rehearsing. `conformance` is what ties those rules to the real ones.

## Prior art

- **NEAR Intents solver relay.** Today counterparties meet through one hosted message bus, by
  request for quote. NEAR's docs say the protocol can run without it
  ([market makers](https://docs.near-intents.org/integration/market-makers/introduction)). This
  is one way to.
- **Terra matcher** (this hackathon). Settles pairs of `token_diff` intents through the verifier
  with mainnet receipts and a verification script, which set the bar for `verify` here. It matches
  two parties with a central deterministic matcher; this has no matcher and settles rings.
- **Setoff** (this hackathon). Nets circular debts between firms on its own contract. Same idea
  of cycles; here it is asset swaps through the existing verifier.
- **Zaki Manian's NEAR Intents on Mosaik prototype.** Replaces the relay with decentralized
  streams and keeps an auctioneer that batches. Here there is no auctioneer.

## How this was built

Written during the hackathon by one person with an AI coding assistant (Claude), who directed and
reviewed it. Two test vectors come from [NearBy](https://github.com/dastansam/nearby), another
entry in this hackathon by a different author: the NEP-413 signature in `intents/src/nep413.rs`
and the transaction encoding in `chain/src/tx.rs`. No code from the NEAR Intents repository is
copied; `conformance` depends on it to test against.

Libraries: libp2p, tokio, ratatui, axum (devnet only), reqwest, ed25519-dalek, clap.

Other entries in this hackathon: the author of Gossip Market also builds
[Ainek](https://github.com/sadabaevb/ainek) with a teammate. NearBy, the source of the two test
vectors above, is that teammate's own entry; its author is not part of this project.

## Next

- An agent of its own on the `mcp` tools, with a goal and a budget, deciding what to want and at
  what price.
- Wants with proof of funds, so that a book cannot be filled with wants nobody can pay for.
- Relay and hole punching, for agents behind NAT.
- Sealed wants for the first round of a ring, to narrow what listeners learn.

## License

MIT. See `LICENSE`.
