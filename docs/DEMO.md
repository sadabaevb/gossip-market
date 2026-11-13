# Running the demo

Three ways, from no setup to real money: the simulation, the devnet on screen, and mainnet.

## 1. The simulation

```sh
cargo run -- sim
```

Everything in one process, a few seconds, no setup. This is the one a reviewer runs first. It ends
with `SIMULATION PASSED` or a non-zero exit.

```sh
cargo run --release -- sim --agents 100
```

The same with a hundred agents whose wants compete, ten of whom quit partway. It prints each
settlement as it happens and a summary at the end, and takes about a minute once built. `--seed`
gives another market. Run one at a time: two at once run out of local ports.

## 2. The devnet, on screen

This is the one to record.

```sh
scripts/demo.sh
```

It builds in release mode and opens a tmux session: a local verifier on top, and three panes
below for Alice, Bob and Carol. Alice and Bob start at once. Carol's pane waits for Enter.

Without tmux, the same thing in four terminals:

```sh
cargo run --release -- devnet --genesis demo/devnet.toml
cargo run --release -- node --config demo/alice.toml
cargo run --release -- node --config demo/bob.toml
cargo run --release -- node --config demo/carol.toml
```

Make the terminal wide: each node's screen switches to its full layout at 100 columns, and three
side by side want about 300. On a laptop, shrink the font or use four separate windows.

### Three moments, about ninety seconds

**No server.** Alice and Bob are up. Each shows the other as a peer and the other's want in its
book. Alice gives USDC for credits; Bob gives credits for a dataset. Nothing settles: neither has
what the other wants.

> Two agents. They found each other on the network, there is no server between them, and they
> cannot trade: Alice wants what Bob has, but Bob does not want what Alice has.

**The ring.** Press Enter in Carol's pane. Within a few seconds she appears as a peer on both
other screens, her want appears in their books, and all three screens show the same settlement.
The top pane shows one `execute_intents` with three signed intents.

> Carol has the dataset and wants USDC. Now there is a ring. Each agent signed only its own leg,
> one of them sent all three to the verifier, and the verifier settled all three or none. Nobody
> matched them and nobody held anyone's tokens.

**Pull the plug.** Quit Alice (`quit`, or Esc). She is the node the other two were told to dial.
Then type into Bob's pane and Carol's pane:

```
want 50 CREDITS for 2 DATA
```
```
want 3 DATA for 40 CREDITS
```

They settle between the two of them.

> Alice was the only address the others were given. She is gone and the market carries on.

**Then check it.** When a node exits it prints, for each ring it settled, a line like

```
gossipmarket verify 6bDfRb…iqK8N --sender 218fe9…af86 --config demo/bob.toml
```

Run it (`cargo run --release -- verify …`, with the devnet still up) to read the transaction back
and recompute it: every signature, and every token netting to zero.

### Other things to type

```
want 5 USDC for 45 CREDITS      opens a want; refused if the node does not hold 5 USDC
cancel 2                        closes want #2
quit
```

A want fills in parts: open `want 100 CREDITS for 4 DATA` on Bob and `want 2 DATA for 40 CREDITS`
on Carol, and Bob's want stays open for the rest at the same price.

To start over, stop everything and run the script again. The devnet's balances live in memory.

## 3. Mainnet, with cents

NEAR Intents has no testnet, so this uses `intents.near` and real tokens. The amounts in
`demo/mainnet/` add up to well under a dollar, plus NEAR for gas.

You need [near-cli-rs](https://github.com/near/near-cli-rs) and a NEAR account of your own
(`you.near` below) holding a little NEAR, USDC and USDT.

Nothing in this repo ever touches `you.near` or its key. The agents get keys of their own, and you
send them only what you decide to.

### a. Make the agents' keys

```sh
cargo build --release
alias gm=target/release/gossipmarket

gm keygen --out keys/alice.key
gm keygen --out keys/bob.key
gm keygen --out keys/carol.key
```

Each prints the account the key controls: 64 hex characters. `keys/` is in `.gitignore`. Each file
is a whole account; whoever has it has what is in it.

### b. Ask the contract first, for free

```sh
gm preflight --config demo/mainnet/alice.toml
```

Expect five `ok` lines and `PREFLIGHT PASSED`. This sends nothing. It shows that the live contract
accepts what this build signs, and reads the current fee and salt. If any line says `WRONG`, stop:
the contract has changed since this was written.

### c. Fund the three accounts

Each agent needs NEAR on its account for gas, and its token inside the verifier.

Gas. Any of the three may be the one that submits, and each withdraws its own tokens at the end,
so each needs some. A transaction from this build attaches 300 Tgas, and NEAR reserves all of it
up front at a pessimistic price: about 0.301 NEAR must be free on the account, or the chain turns
the transaction away with `NotEnoughBalance`. What is burnt is far less: the first mainnet ring
burnt 19 Tgas (0.0011 NEAR) and a withdraw 33 Tgas. Send each 0.35 NEAR:

```sh
near tokens you.near send-near <alice account> '0.35 NEAR' network-config mainnet sign-with-keychain send
near tokens you.near send-near <bob account>   '0.35 NEAR' network-config mainnet sign-with-keychain send
near tokens you.near send-near <carol account> '0.35 NEAR' network-config mainnet sign-with-keychain send
```

The accounts do not exist until this first transfer, so near CLI asks whether to send to them
anyway. Yes: sending NEAR is what creates an implicit account.

Tokens. A deposit into the verifier is an `ft_transfer_call` to `intents.near` on the token's
contract, with the receiving account as the message. Alice gets 0.10 USDC:

```sh
near contract call-function as-transaction \
  17208628f84f5d6ad33f0da3bbbeb27ffcb398eac501a31bd6ad2011e36133a1 ft_transfer_call \
  json-args '{"receiver_id":"intents.near","amount":"100000","msg":"<alice account>"}' \
  prepaid-gas '100 Tgas' attached-deposit '1 yoctoNEAR' \
  sign-as you.near network-config mainnet sign-with-keychain send
```

Carol gets 0.10 USDT:

```sh
near contract call-function as-transaction usdt.tether-token.near ft_transfer_call \
  json-args '{"receiver_id":"intents.near","amount":"100000","msg":"<carol account>"}' \
  prepaid-gas '100 Tgas' attached-deposit '1 yoctoNEAR' \
  sign-as you.near network-config mainnet sign-with-keychain send
```

Bob gets 0.03 wrapped NEAR. Wrap a little more than that first (the first wrap from an account
also pays for its storage on `wrap.near`), then deposit 0.03:

```sh
near contract call-function as-transaction wrap.near near_deposit json-args '{}' \
  prepaid-gas '30 Tgas' attached-deposit '0.05 NEAR' \
  sign-as you.near network-config mainnet sign-with-keychain send

near contract call-function as-transaction wrap.near ft_transfer_call \
  json-args '{"receiver_id":"intents.near","amount":"30000000000000000000000","msg":"<bob account>"}' \
  prepaid-gas '100 Tgas' attached-deposit '1 yoctoNEAR' \
  sign-as you.near network-config mainnet sign-with-keychain send
```

Check what arrived:

```sh
gm account --config demo/mainnet/alice.toml
gm account --config demo/mainnet/bob.toml
gm account --config demo/mainnet/carol.toml
```

Each should list the token it was sent under "inside the verifier".

### d. Run it

Three terminals, from the repository root:

```sh
gm node --config demo/mainnet/alice.toml
gm node --config demo/mainnet/bob.toml
gm node --config demo/mainnet/carol.toml
```

Within about ten seconds of the third node starting, the ring settles. Each node logs `SETTLED`
with the transaction hash and the account that sent it. Open the hash in a NEAR explorer: one
`execute_intents` call on `intents.near` carrying three signed intents.

Then:

```sh
gm verify <tx hash> --sender <account that sent it> --config demo/mainnet/alice.toml
```

It should end with `VERIFIED`. Put the hash and that command in the README, under "What was
checked".

The prices in `demo/mainnet/*.toml` are placeholders chosen so that the ring closes. All three
agents are yours, so the price only decides which of your accounts ends up with what.

On two or three machines: keep `mdns = true` on one local network and the nodes find each other;
across networks, put the first machine's reachable address in the others' `bootstrap`.

### e. Take the funds back

What is left sits inside the verifier under the agents' accounts. Stop the nodes first, so that
none of them trades a balance while it is being taken out. Then, for each agent and token it holds:

```sh
gm withdraw USDC --to you.near --config demo/mainnet/alice.toml
gm withdraw NEAR --to you.near --config demo/mainnet/bob.toml
gm withdraw USDT --to you.near --config demo/mainnet/carol.toml
```

Without `--amount` it takes everything the agent holds of that token; `--amount 0.05` takes part.
The agent signs an `ft_withdraw`, the contract dry-runs it for free, and only then does the agent
send it, paying the gas from its own NEAR. The output ends with the balance before and after.

`you.near` must be registered with the token's contract, as any account that has held the token
is. If it is not, the token's contract refuses, the verifier puts the amount back, and the command
says so. Wrapped NEAR arrives as wNEAR; `near_withdraw` on `wrap.near` turns it back into NEAR.

The NEAR on the agents' own accounts, for gas, stays there: this build has no command to send it
back.

### If something does not happen

| You see | It means |
|---|---|
| all three wants in every book, and nothing proposed | a maker's deposit has not arrived, or went to another account: a node does not propose a ring it cannot pay for. Check `gm account` |
| `declined ring … : this node does not hold that much` | the same, seen from the maker that was asked |
| `ring … fell through: <reason>` | the contract refused the dry run; the reason is its own. Nothing was spent |
| `ring …: <error>` after `submitting to intents.near` | the submission failed. An account with no NEAR has no access key on chain yet and cannot pay for gas; another maker will try after a few seconds |
| `NotEnoughBalance` in that error | the submitter cannot reserve the 300 Tgas: it needs about 0.31 NEAR free. Nothing was spent. Send it more NEAR, or fund another maker |
| `ring … expired unsettled` | nobody got the bundle on chain before the deadline. The wants reopen and the ring is tried again |
| nodes do not list each other as peers | they are not on one network, or `bootstrap` is empty or wrong |
| `devnet:… is a throwaway key that anyone can derive` | a devnet key is being pointed at mainnet, which is refused on purpose |
