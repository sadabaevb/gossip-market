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

## A hundred agents, 13 November 2026

`gossipmarket sim --agents 100` runs a hundred nodes in one process against the devnet, over five
tokens everyone prices alike, so that almost any cycle of wants closes and rings compete for
every want. Ten agents are set to quit partway. Until then nothing had run with more than three
agents. The runs found five problems:

- **A ring of four went round for ever.** A try that met one busy maker fell through, but the
  makers who had signed it were held until its deadline. Its proposer tried again two seconds
  later and met them; the makers who signed that try were held past the first, and they were
  never all free at once. A proposer now rests a ring from the deadline of the try that fell
  through.
- **The market ground to a halt.** Every node proposed every ring through its wants, so proposers
  kept locking one want into rings that could not all settle, and each failed try held its
  signers until its deadline. 21 rings settled in the first 15 seconds, 6 between 30 and 45, and
  then hardly any. A node now proposes only the rings that come first for each of their wants
  (`first_rings_of`): two such rings never share a want, and the first ring of the book is always
  one of them.
- **The makers of one ring took turns proposing it out of step.** Each try met the makers still
  held by the last. A maker asked to sign a try now waits out its deadline before proposing the
  ring itself.
- **Most of the work was Kademlia.** A profile of a run spent most of its time in DHT queries: a
  node asked for a walk of the DHT on every new connection, about 90 per node here. It no longer
  asks per connection. libp2p-kad starts a walk itself when a peer enters its routing table, at
  most one every half second, and every five minutes. The one walk a node still asks for, on its
  first connection, starts only if the table already holds a peer, as when mDNS found one; in
  `sim` it does not start, and the library's own walk is what finds the other peers.

- **The last rings of four were never proposed.** A ring had to be the first found from every
  one of its wants. Where every cycle of four closes, the search from one want runs out of steps
  before it reaches rings that the search from another want finds; then no ring came first for
  all its wants, and nobody proposed anything. One run stopped for good like that: rings of four
  open, no want locked, and not a line about the first of them from any of its makers. Another,
  under load, ended the same way. A ring now qualifies when nothing the search finds through its
  wants comes before it, so the smallest ring any search finds always does. `market` has a book
  of 32 wants on which the old rule proposes nothing.

A ring that failed because a maker was busy also stopped stepping aside for the rings it comes
before: it is alive, and the other makers still put it first. With that, seed 1's last ring
settled at 82 seconds instead of 149, on a build before the last fix.

A review of the code afterwards found four more, fixed the same day:

- **A stranger could keep a ring from being proposed.** A maker asked to sign a try held off
  proposing the ring whoever had asked, and before checking the deadline. Anyone could propose a
  ring to its makers every so often, with a deadline nobody would sign, and none of them would
  propose it. A maker now counts only proposals from the ring's own makers, with a deadline it
  could sign. `node/src/tests.rs` has a peer that proposes rings it is not in.
- **A ring a node could not pay for hid the ones it could.** A node chose the first ring through
  each of its wants, then checked its balance. A ring too big for it left the want with nothing
  proposed, though a smaller ring through it would have settled. Rings it cannot pay for are now
  left out before the choice.
- **One want could crowd out the others.** A node searched from all its wants at once, and the
  search keeps 64 rings. A want in more rings of two than that filled it, and the rings through
  the node's other wants were never found. It now searches from each want on its own.
- **A fifth to a third of the planted rings could never close.** The fee on a token with no decimals, GPU
  here, is at least one whole unit, worth more than the margin a want was priced with. In seeds 1
  to 5, 8 to 13 of the 42 planted rings could not close. Wants now ask for what is left after the
  fee, every planted ring is checked to close before the agents start, and a run in which nothing
  settled fails. Before, a search that found nothing would have left nothing open either, and
  the run would have passed.

With all of it, on an Apple M4 Max, one run at a time:

| Seed | Rings settled | Of two, three, four | Last ring after the last agent started | Tries that fell through |
|---|---|---|---|---|
| 1 | 78 | 60, 15, 3 | 59 s | 79 of 162 |
| 2 | 61 | 59, 2, 0 | 16 s | 64 of 129 |
| 3 | 70 | 62, 8, 0 | 37 s | 81 of 155 |
| 4 | 79 | 46, 33, 0 | 63 s | 73 of 157 |
| 5 | 73 | 64, 8, 1 | 38 s | 69 of 143 |

Each passed every check: something settled, no ring left among the live agents' open wants and
none locked, every transaction read back and checked, every balance what the transactions say,
every live maker of a settled ring aware of it, and every bundle that had gone out to a live maker
settled. In every run the proposer sent each ring itself; no maker had to. Each run used five to
seven cores on average.

Two runs at once fail: see the known gaps. One earlier run, under that load, ended with a ring
whose every signed leg a live maker held and that no transaction was sent for. Its log was not
kept, and it has not happened since. The check now counts a proposer's bundle only once it has
passed the dry run and gone out (a bundle the verifier refused rightly never settles), and
prints what the holder said about the ring if it happens again.

## Two reviews and a profile, 14 November 2026

The code was reviewed twice and a hundred-agent run was profiled, the day before the deadline.
What that found, and what was done about it:

- **A third of a node's work was its own screen.** Every change to what a node shows rebuilt the
  list of its peers' accounts, and getting an account out of a peer id decodes an ed25519 key.
  With about 90 peers each, that was a third of the busy processor time of a hundred agents. A
  node now works the name out once, when the peer connects: seed 1 took 454 seconds of processor
  time before and 318 in the next run. What is left is mostly the connections themselves: about
  two thirds of the busy time goes on reading, writing and checking gossip. The ring search,
  which a node runs five times a second, is about 3%.
- **The simulation could fail a market that had cleared.** It read the devnet's transactions and
  then the agents. The last ring's transaction could land between the two looks: the market was
  seen to clear with a transaction nobody had read, and the run failed on it. The transactions
  are now read after the agents. This came from reading the code; no run was seen to fail on it.
- **A hundred agents had three minutes.** One run of seed 1 that lost the processor to other work
  stalled for a minute, with 269 tries timing out for want of an answer against 6 in the next
  run, and was failed at 180 seconds with 78 rings settled and three wants still locked. The
  limit is now ten minutes.
- **A bundle that reached the verifier late failed the run.** The last check wants every bundle
  that went out to a live maker to settle. In a run that lost the processor to other work for its
  first half minute, three transactions reached the verifier after the 8-second deadline their
  legs were signed with and were refused. The market cleared and every other check passed, and
  the run failed on two rings that never settled. A bundle whose transaction the verifier refused
  is no longer counted as lost, and the checks say how many there were.
- **A name off the network could stop a node.** Accounts were shortened for the log at the eighth
  byte. A notice naming a maker with a longer character there panicked the node that logged it.
  Names are cut at a character.
- **A leg was given up at the first unspent answer after its deadline.** A transaction that beat
  the deadline can show a little after it, by a node's own clock and through an RPC node a block
  or two behind. The want was reopened meanwhile and could have been filled a second time. A node
  now watches a leg for 10 seconds past its deadline, 1 in the simulations, before it reopens the
  want.
- **A maker's one try at settling a ring could be spent on nothing.** The try begins with a free
  look at whether the nonce is spent. If that look failed, the try was gone and nothing had been
  sent. The try is now spent when a transaction goes out.
- **A notice could hold a maker off.** A maker told that its ring had settled looked at the chain
  instead of submitting, and looked again at each notice. With the old code, a proposer that
  shared the bundle, never submitted and published a false notice every 20 ms kept the ring
  from settling until its signatures expired, every time it was tried. A notice no longer holds
  the submit off, since the submit asks the chain first anyway, and no longer brings the next
  look forward.
- **`http://localhost:80@example.com` passed as local** in the check that keeps the public
  `devnet:` keys off real networks. What comes before an `@` is now left out of the host.
- **The node did not walk the DHT when these notes said it did.** See "Most of the work was
  Kademlia" above. In `sim`, the walk a node asks for on its first connection did not start on
  any of the three nodes.

With all of it, on an Apple M2 Pro doing other work at the time, one run at a time:

| Seed | Rings settled | Of two, three, four | Last ring after the last agent started | Processor time |
|---|---|---|---|---|
| 1 | 79 | 59, 20, 0 | 72 s | 270 s |
| 2 | 62 | 61, 1, 0 | 54 s | 189 s |
| 3 | 68 | 58, 10, 0 | 73 s | 269 s |
| 4 | 78 | 50, 27, 1 | 52 s | 221 s |
| 5 | 70 | 59, 10, 1 | 48 s | 216 s |

Each passed every check, on four to five cores. With eight busy processes competing for the same
ten cores, seed 1 settled 81 rings, the last 226 seconds after the last agent started, and passed
every check. What the reviews found and nothing was done about is under the known gaps.

## Tools for an AI agent, 14 November 2026

`gossipmarket mcp` puts a node behind the Model Context Protocol, on standard input and output.
The protocol's current revision, 2026-07-28, dropped the `initialize` handshake: every request
names its revision in `_meta` and is answered on its own, and a server must answer
`server/discover`. Clients of the earlier revisions still open with `initialize`. The server
tells the two apart by whether a request names its revision, and answers each in its own shape.
A request that names no revision and shook no hands is answered in the earlier shape, so that the
tools can be tried by hand.

What was checked: four tests, one of which closes a ring of three with nothing but the tools; and
the replies of the built binary to `server/discover`, `tools/list`, `tools/call`, a failed tool
call and an unsupported revision, against the schemas published with the protocol for 2026-07-28
and 2025-11-25. Driving the binary by hand found what the tests had not: a tool called within a
moment of starting was told the node held nothing, because the node had not read the verifier
yet. The first tool call now waits for that, and in a rehearsal for the others' wants to arrive.

Later the same day an AI agent was given the tools. Claude Code, with the server added by
`claude mcp add gossipmarket -- <the release binary> mcp`, listed it as connected, and asked in
one sentence (in Russian) to look at the market, offer 10 USDC for at least 95 CREDITS, wait for
the settlement and check it, called the tools seven times in 36 seconds. Its report had the
market as it found it, the reason its want needed a ring of three, the ring `f4f5ff5acf5df14c`
with each maker's two amounts, the verdict `VERIFIED`, and what it and the others were left
holding. Asked again ten minutes later in English, with the server still running, it made nine
calls in 37 seconds and said before anything else that the node was not fresh: an identical want
had settled at 18:35, so the one it placed was #2 and the balances showed both trades. That
session was recorded and is in the demo video. The client was Claude Code 2.1.325. Which revision
of the protocol it spoke was not looked at, and no other client was tried. Adding the server by hand in this repository made Claude Code warn that `gossipmarket` is
defined in two scopes with different commands, since a `.mcp.json` here named it too. That file
is gone: one way to connect is enough, and its `cargo run` would have compiled for minutes the
first time a client started it.

Requests are answered one at a time, so a `wait_for_settlement` holds up whatever the client asks
meanwhile, for at most a minute, and a `notifications/cancelled` does not cut it short.

## The run on a page, 14 November 2026

`sim --agents N --web` serves `docs/watch/index.html` on localhost and tells it the run as a list
of events that only grows: who is in the market, each ring as it settles, each agent that quits,
each transaction the verifier refuses, a letter per agent for how it stands, and the checks at the
end. The page asks for the events past the ones it has, a few times a second. `--record` writes
the same list as `run.js`, and the same page beside that file plays it back with no server, so it
opens from disk or from any static host.

The market waits up to 15 seconds for the page to be opened before the agents start, and the
page is served after the run for as long as it is open. With nobody watching neither wait
happens, so a run started by a script ends when the market does.

What was checked: the page in a browser, served live through a run of a hundred agents and
playing back the recording that run made, with nothing in the browser's console in either; and
one test that the events of a thirty-agent run name real agents and end with the result. Nothing
checks what the page draws against the events it is given.

## Reading an old settlement back, 14 November 2026

Two days after the ring on mainnet, the `verify` command in the README stopped working: the
endpoint in the mainnet configs (`free.rpc.fastnear.com`) answered the `tx` call with
`TIMEOUT_ERROR` and cause `NOT_OBSERVED` after thirty seconds, and so did
`rpc.mainnet.fastnear.com`. An ordinary NEAR node keeps a transaction for about five epochs and
then answers as if it had never seen it. `archival-rpc.mainnet.fastnear.com` and
`archival-rpc.mainnet.near.org` both returned the transaction, and `verify` ended with `VERIFIED`
against each.

So `verify` takes `--rpc` to ask another endpoint than the config's, and when the chain gives no
answer it says that the transaction may only be old and names an archival node. The configs keep
the ordinary endpoint: a node reads the present, not the past. The same day `preflight` was run
again against `intents.near` and passed all five probes.

## The ring on mainnet again, 14 November 2026

For the demo video the three mainnet agents were run once more, by a script that starts
`gossipmarket node --plain` for each, a second and a half apart, and keeps every line with the
time it came. Carol, the lowest account and so the first to propose, saw the third want 4.3
seconds after the first node started, proposed at 4.4, had both other legs signed in the same
tenth of a second, sent the transaction at 5.0 and logged `SETTLED` at 9.4; the others followed
within half a second. The transaction is `GEeSFMnxE3bj86JXX9yMeyFTPV3FLhuZWnu9z4t45KVn`, 18 Tgas
burnt, 0.00112 NEAR off the submitter's balance.

Before it: `preflight` passed, and each agent's holdings inside the verifier were read with
`account`. After it: `verify` ended with `VERIFIED`, and the holdings had moved by what the three
legs say. Only Carol held the 0.31 NEAR a submitter needs; the other two could not have sent it
had she not.

## A seat for a person, 14 November 2026

`gossipmarket seat` serves `docs/seat/index.html` on localhost: the rehearsal of `mcp` with a
form and buttons where the AI agent would be. A button posts a tool's name and arguments, the
node runs the tool as it would for an agent, and the page shows the answer as text. What the page
draws, the makers and the last ring between them, it reads from the node's view, not from that
text, and without waiting for a tool that is running: `wait_for_settlement` keeps the seat for
up to 20 seconds.

It serves the rehearsal and nothing else. The page has no login, so a real node behind it would
trade for whoever on the machine asked. A tool is run only for a request that says its body is
JSON, which a page from another site cannot send here without asking first.

What was checked: one test that closes a ring of three and verifies it through the page's own
requests; the page driven in headless Chrome through placing the suggested want, the ring and the
verification, with nothing in the browser's console; and by hand over the same requests, a want
that asks too much (nothing settles in 20 seconds), cancelling it, and a fair one after it.
Nothing checks what the page draws against what it is told, and no other browser was tried.

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
- The ring search is bounded at 20,000 steps per starting want and ring length, so in a book
  crowded enough for the steps to run out, a ring of four can be missed. A node searches only from
  its own wants, at most 16. Measured on an Apple M4 Max in release, 13 November 2026, with a full
  book of 10,000 wants, for `first_rings_of`, which a node runs (`rings_of` alone is a little
  faster):
  - Wants that chain but never close, with one ring of three hidden among them: a node finds the
    ring in about 2 ms per look.
  - Every opposite pair of wants closes: 1.4 to 1.5 ms per look.
  - Before, every node searched the whole book. That took 1.2 s per look, and it missed the hidden
    ring in any book of 100 wants or more.
  - `find_rings` searches from every want. It is for tests and checks, and takes seconds on a full
    book.
- Under contention, a try that falls through still holds the makers who signed it until its
  deadline: 30 seconds with the default timings, 8 in the simulations. In the hundred-agent runs
  close to half of all tries fell through, mostly on a want committed to another ring or to an
  earlier try of the same one, and the last rings took up to 63 seconds to settle. A first round
  that reserves wants without signing anything would let a failed try release them at once;
  nothing does that yet. A ring of two that was only declined, where nobody but the proposer had
  signed, is held off just the same.
- A maker's refusal "that want is committed to ring …" is read off the text of its answer and
  taken at its word: the proposer does not rest the ring or step aside from it. A maker that
  keeps answering so, in the ring that comes first for another's want, keeps that want out of
  every other ring. A field in the answer, and a limit on how often it is believed, would close
  this; both change the messages.
- A maker that submits a bundle itself asks whether its nonce is spent and then sends, without
  the dry run a proposer makes. If the ring can no longer settle, the transaction fails on chain
  and costs its gas.
- A node checks a leg against its whole balance, not against what its other signed legs leave
  it. With two wants giving the same token it can sign for more than it holds. The verifier
  refuses whichever ring comes second; a dry run made before the first settled does not catch
  it, and that costs a failed transaction's gas.
- The search keeps the first 64 rings through a want, then sets aside the ones the node cannot
  pay for or that are resting. If all 64 are set aside, a ring behind them is not found.
- Two tables grow with what peers send and have no cap: the tries a node was asked into by any
  peer that puts the two of them in a ring that nets, for up to about 100 seconds an entry, and
  the settlements it heard of, for as long as it runs.
- A maker waits `(rank + 1) × submit stagger` after it receives a bundle before submitting it
  itself, and does not once the deadline has passed. With the simulations' timings (8 seconds
  ahead, 2 seconds a rank) the last maker of a ring of four never gets to; with the defaults
  (30 and 6) it gets to only if the bundle came within 6 seconds of the proposal.
- In a book crowded enough to exhaust the ring search, two rings that each come first for their
  wants can share one, and then one of them falls through on it.
- A node dials from a fresh local port only when it dials a peer itself. When a request goes to
  a peer it is not connected to, libp2p dials from the listening port, which fails if the two
  nodes dial each other at the same moment. A node stops dialing for itself at 64 peers, which
  the hundred-agent runs reach. A few tries in each run fall through on "Failed to dial", which
  may be this.
- A node copies its whole book for its screen whenever a want is announced again, even unchanged.
  At the book's limit of 10,000 wants that is more than a node can keep up with, by an estimate,
  not a measurement.
- A hundred agents in one process keep four to five cores busy on an Apple M2 Pro; before 14
  November, five to eight on an Apple M4 Max. They hold about 6,500 loopback
  connections, and leave about 9,300 sockets in TIME_WAIT for 30 seconds when they exit, against
  16,384 ephemeral ports on macOS. Two hundred-agent runs at once, or one started straight after
  another, run out of ports: dials fail, and the run fails or does not clear.
- `proc-macro-error2`, a transitive dependency, prints a future-incompatibility note on build.
  It is harmless today.
