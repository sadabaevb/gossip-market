# Gossip Market protocol, version 1

What nodes say to each other and to the verifier, precisely enough to write another node. The code
is the reference; where this and the code disagree, the code is right and this is a bug.

## 1. Identity

An agent is one ed25519 key.

| Use | Derived as |
|---|---|
| NEAR implicit account, and account inside the verifier | lowercase hex of the 32-byte public key |
| Verifier signer | the same key: the verifier treats an implicit account's own key as registered |
| libp2p peer id | the ed25519 public key as a libp2p identity |
| Gas payer | the implicit account on NEAR, once it holds NEAR |

So a want's `maker` field names an account, a signer and a peer at once (`node/src/net.rs`,
`peer_of` and `account_of`).

## 2. Network

libp2p over TCP with Noise and Yamux.

| Protocol | Name | Use |
|---|---|---|
| identify | `/gossipmarket/1` | peers tell each other their listen addresses |
| Kademlia | `/gossipmarket/kad/1` | finding peers beyond the ones dialed; every node is a server |
| mDNS | | finding peers on the local network; optional |
| gossipsub | topic `gossipmarket/wants/1` | signed wants |
| gossipsub | topic `gossipmarket/settled/1` | notices that a ring settled |
| request-response, JSON | `/gossipmarket/settle/1` | proposals and bundles, maker to maker |

Gossip messages are signed by the publishing peer and validated strictly. A node dials the peers
in its `bootstrap` list, any peer mDNS reports, and the maker of any open want it hears of, up to
64 peers. Bootstrap peers are ordinary nodes; the market does not depend on them once two other
nodes are connected.

## 3. Wants

A want says: I give up to `give_amount` of `give` for at least `take_amount` of `take`, and
proportionally less for less.

```json
{
  "maker": "a457b704…22c3",
  "id": 1,
  "rev": 0,
  "give": "nep141:usdc.near",
  "give_amount": "10000000",
  "take": "nep141:credits.near",
  "take_amount": "95000",
  "expires_ms": 1794500000000,
  "addrs": ["/ip4/192.168.1.7/tcp/4001"]
}
```

Amounts are base units as decimal text. Token ids are the verifier's (`nep141:<contract>`).

On the wire a want is `{"body": "<the JSON above, as text>", "signature": "<base58>"}`. The
signature is ed25519 by the maker's key over the bytes `gossipmarket want v1:` followed by `body`.
The body is kept exactly as signed. The prefix keeps a want from ever being mistaken for an intent
or a transaction, which the same key also signs.

A node accepts a want if (`market/src/want.rs`, `SignedWant::open`):

- the body is at most 2048 bytes and parses;
- `maker` is an implicit account and its key made the signature;
- `expires_ms` is in the future and at most 5 minutes ahead;
- `give` and `take` differ, and `take_amount` is not zero while the want is open;
- there are at most 8 addresses.

It replaces an earlier want with the same `(maker, id)` if `(rev, expires_ms)` is greater. A node
keeps at most 16 wants per maker and 10,000 in all, and drops wants as they expire.

Makers announce each open want every 5 seconds with a lifetime of 20 seconds, and once more
shortly after any new peer connects. `rev` goes up whenever the want changes. A partial fill is
announced as the remainder at the original price, rounded up in the maker's favour. A want with
`give_amount` zero is closed: filled or cancelled. Closed wants are announced for one more
lifetime so that everyone hears.

## 4. Rings

A ring is two to four wants by distinct makers in which each maker gives its token to the next and
the last gives to the first.

### Search

Every node runs the same search over the open wants it holds (`market/src/ring.rs`,
`find_rings`): depth-first over wants that take what the previous one gives, closing when the
first takes what the last gives. A ring is written starting from its smallest `(maker, id)`, so it
has one spelling. Results are ordered shortest first, then by ring id. The ring id is the first 16
hex characters of the SHA-256 of its legs.

### Sizing

The verifier takes a fee from every amount given: `ceil(amount × fee_pips / 1 000 000)`. The
receiver gets the rest. (Single NFTs pay no fee.) A batch settles only if, per token, what is
received plus the fees equals what is given.

So a leg's `take_amount` is not a choice: it is exactly what the previous maker gives, less the
fee. Sizing a ring means choosing the `give_amount`s:

1. Start with every maker giving all it offers.
2. For each maker in turn, compute what it receives, and lower its `give_amount` to what that
   pays for at its own price, rounded down.
3. Repeat until nothing changes. Lowering one amount only lowers others, so this ends at the
   largest amounts that satisfy everyone. If an amount reaches zero, or the amounts keep shrinking
   past 64 laps, the prices do not close and there is no ring.

Whatever a maker gives beyond what the next one asked stays with the receiver. There is no
proposer's cut.

`conformance` runs 400 random cases of this through the contract's engine.

### Soundness

A node that receives a ring it did not compute checks (`Ring::is_sound`): two to four legs,
distinct makers, the canonical spelling, and that the legs net to zero at the fee the node itself
reads from the contract.

## 5. Settling a ring

All messages are JSON over `/gossipmarket/settle/1`.

### Whose turn

A node considers the rings that include one of its own unlocked wants and that it can pay for. The
makers of a ring are ranked by account id. The maker ranked `n` (from zero) proposes once it has
known the ring could be formed for `n × 2 s`. A ring that fell through is left alone for 10
seconds, plus the same stagger.

### Propose

The proposer picks a deadline 30 seconds ahead, signs its own leg, locks its want, and sends each
other maker:

```json
{"ask": "propose", "ring": {"legs": [ … ]}, "fee_pips": 1, "deadline_ms": 1794500030000}
```

A maker signs its leg only if all of these hold (`Node::consider_proposal`):

- `fee_pips` equals the fee it reads from the contract;
- the ring is sound at that fee;
- it has a leg in the ring, for a want of its own that is open and not locked;
- the leg is within that want as last announced: same tokens and revision, no more given, no
  worse a price;
- it holds the amount inside the verifier;
- the deadline is between 2 and 60 seconds ahead.

It answers `{"answer": "signed", "signed": { … }}` or `{"answer": "declined", "reason": "…"}`,
and locks the want until it learns what became of the signature.

### What is signed

One NEP-413 message per maker, holding one `token_diff` intent with exactly its leg:

```json
{
  "standard": "nep413",
  "payload": {
    "message": "{\"signer_id\":\"a457…22c3\",\"deadline\":\"2026-11-12T14:42:02.000Z\",\"intents\":[{\"intent\":\"token_diff\",\"diff\":{\"nep141:credits.near\":\"99999\",\"nep141:usdc.near\":\"-10000000\"},\"memo\":\"gossipmarket:f7a0d7e6a7365c54\"}]}",
    "nonce": "<base64 of 32 bytes>",
    "recipient": "intents.near"
  },
  "public_key": "ed25519:…",
  "signature": "ed25519:…"
}
```

The signature is over `sha256(borsh(2^31 + 413) ‖ borsh({message, nonce, recipient, callback_url:
None}))`. The nonce is the verifier's versioned layout by default:

```
5628f6c6 | 00 | salt (4) | deadline, i64 nanoseconds little-endian (8) | random (15)
```

with the salt read from the contract's `current_salt` and the deadline equal to the message's.
Plain nonces (32 random bytes) are still accepted by the contract and can be selected with
`nonce = "plain"` in a node's config.

The memo names the ring, for whoever reads the transaction later; the verifier only logs it.

A leg says nothing about the other legs. That is what makes it safe to hand over: whoever holds it
can only settle it in a batch that gives the signer what the leg says.

### Simulate, share, submit

When every signature is in, the proposer checks each one against its leg (`Ring::check_signed`:
signer, verifier, deadline, exactly one `token_diff` with the leg's amounts), then:

1. calls `simulate_intents` on the verifier, a view call. If the verifier refuses, the ring is
   abandoned and nothing was spent;
2. sends every maker the whole bundle: `{"ask": "bundle", "ring_id": "…", "signed": [ … ]}`, in
   leg order;
3. calls `execute_intents` with the bundle, in a transaction from its own account;
4. publishes a notice on `gossipmarket/settled/1`: the ring, the transaction hash and the account
   that sent it.

If a maker has not answered within 8 seconds the proposer gives up. Its own signature has not left
it, so its want is free at once.

### If the proposer goes quiet

A maker that receives a bundle checks every leg in it. If no settlement is seen, the maker ranked
`n` waits `(n + 1) × 6 s`, checks that its nonce is still unspent, and submits the bundle itself,
once.

### Learning the outcome

The node that submitted goes by the result of its own transaction. For everyone else a notice is
a hint and nothing more: a maker that signed a leg decides only by asking the verifier
`is_nonce_used` for its own nonce:

- spent: the ring settled. The want is reduced by what was given, and closed if nothing is left;
- unspent after the deadline: the signature can no longer be used. The want is open again;
- unspent before the deadline: keep waiting.

## 6. The verifier

Calls a node makes, all through NEAR JSON-RPC (`chain/src/rpc.rs`):

| Call | Kind | For |
|---|---|---|
| `fee`, `current_salt` | view | sizing rings, building nonces; read again every 10 s |
| `mt_batch_balance_of` | view | what the agent holds |
| `simulate_intents` | view | the dry run |
| `is_nonce_used` | view | the outcome |
| `execute_intents` | transaction, 300 Tgas, no deposit | settlement |
| `tx` | RPC | `gossipmarket verify` |

An error from the contract ("nonce was already used", "invalid signature", "insufficient balance
or overflow", "deadline has expired", …) is an answer about the bundle and ends the attempt. Any
other failure (a timeout, an RPC error) leaves the outcome unknown, and the nonce is watched.

## 7. Timings

Defaults, in `node/src/config.rs`:

| | |
|---|---|
| Announce each open want | every 5 s |
| Want lifetime | 20 s |
| Propose stagger per rank | 2 s |
| Wait for signatures | 8 s |
| Intent deadline | 30 s ahead; makers accept 2 to 60 s |
| Submit stagger per rank | 6 s |
| Rest after a ring falls through | 10 s |
| Re-read fee, salt and balances | every 10 s |

## 8. What a misbehaving node can do

| It | The cost to others |
|---|---|
| announces wants it cannot pay for | a refused dry run; the ring rests 10 s |
| never answers a proposal | the proposer waits 8 s; makers that signed wait out the deadline, then reopen |
| collects signatures and does nothing | wants locked until the deadline, at most 60 s |
| collects signatures and settles a different batch with one of them | that maker gets exactly what its leg says; the others' legs expire |
| sends a false notice | nothing: makers go by their own nonce |
| floods wants | bounded per maker and in total, not per network: there is no Sybil resistance |
| listens | learns every want and price |

None of these moves anyone's tokens against what they signed. `node/src/tests.rs` runs the first
three against honest nodes.
