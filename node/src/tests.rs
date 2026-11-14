//! Whole markets in one process: a devnet verifier and real nodes over libp2p on localhost, as in
//! the simulation, but with the things that go wrong. Each test is a way a trade can fail to
//! happen and what the nodes that kept their word are left with.
use std::time::Duration;

use chain::Devnet;
use intents::Ledger;
use tokio::time::{sleep, timeout};

use crate::config::{devnet_key, NodeConfig, Timings, Tokens, WantConfig};
use crate::node::{self, Closed, Command, Conduct, Handle, View};
use crate::sim::{self, serve_devnet, token_table, VERIFIER};

struct Market {
    devnet: Devnet,
    rpc: String,
    tokens: Tokens,
    /// What every agent that joins from here on runs with.
    timings: Timings,
    agents: Vec<(&'static str, Handle)>,
}

impl Market {
    /// A verifier in which each named agent holds what is listed.
    async fn open(deposits: &[(&str, &str)]) -> Self {
        let tokens = Tokens::from(token_table());
        let mut ledger = Ledger::new(VERIFIER, 100);
        for (name, holds) in deposits {
            let (token, units) = tokens.parse(holds).unwrap();
            ledger.deposit(&devnet_key(name).account_id(), &token, units);
        }
        let (devnet, rpc) = serve_devnet(ledger).await.unwrap();
        Self {
            devnet,
            rpc,
            tokens,
            timings: Timings::fast(),
            agents: Vec::new(),
        }
    }

    /// An agent's config against this market's verifier, with no wants and nobody to dial.
    fn config(&self, name: &str) -> NodeConfig {
        sim::config(name, &self.rpc, Vec::new(), Vec::new())
    }

    /// Starts an agent with these wants, dialing whoever joined first.
    async fn join(&mut self, name: &'static str, wants: &[(&str, &str)], conduct: Conduct) {
        let bootstrap = self
            .agents
            .first()
            .map(|(_, first)| first.view.borrow().listen.clone())
            .unwrap_or_default();
        let config = NodeConfig {
            bootstrap,
            wants: wants
                .iter()
                .map(|(give, take)| WantConfig {
                    give: (*give).into(),
                    take: (*take).into(),
                })
                .collect(),
            ..self.config(name)
        };
        let handle = node::spawn_with(config, self.timings, conduct).await.unwrap();
        self.agents.push((name, handle));
    }

    fn view(&self, name: &str) -> View {
        let (_, handle) = self.agents.iter().find(|(agent, _)| *agent == name).unwrap();
        handle.view.borrow().clone()
    }

    async fn tell(&self, name: &str, command: Command) {
        let (_, handle) = self.agents.iter().find(|(agent, _)| *agent == name).unwrap();
        handle.commands.send(command).await.unwrap();
    }

    fn balance(&self, name: &str, symbol: &str) -> u128 {
        self.devnet
            .ledger()
            .balance_of(&devnet_key(name).account_id(), &self.tokens.id(symbol).unwrap())
    }

    /// How many rings `name` has seen settle through its own nonce.
    fn settled(&self, name: &str) -> usize {
        self.view(name)
            .settlements
            .iter()
            .filter(|settled| settled.mine)
            .count()
    }

    fn log(&self, name: &str) -> String {
        let lines: Vec<String> = self.view(name).log.into_iter().map(|(_, line)| line).collect();
        lines.join("\n")
    }

    /// Waits until `done`, or fails with every agent's log.
    async fn until(&self, what: &str, seconds: u64, done: impl Fn(&Market) -> bool) {
        let waited = timeout(Duration::from_secs(seconds), async {
            while !done(self) {
                sleep(Duration::from_millis(50)).await;
            }
        });
        if waited.await.is_err() {
            let logs: Vec<String> = self
                .agents
                .iter()
                .map(|(name, _)| format!("--- {name}\n{}", self.log(name)))
                .collect();
            panic!("timed out waiting until {what}\n{}", logs.join("\n"));
        }
    }

    /// Whether `name` has a want that is open and not committed to any ring.
    fn is_free(&self, name: &str) -> bool {
        self.view(name)
            .wants
            .iter()
            .any(|want| want.closed.is_none() && want.locked.is_none())
    }
}

/// Alice, Bob and Carol each hold what the next wants. Bob's account sorts first, so the ring is
/// his to propose.
async fn ring_of_three(bob: Conduct, carol: Conduct) -> Market {
    ring_of_three_on(Timings::fast(), bob, carol).await
}

/// The same, with every agent on `timings`.
async fn ring_of_three_on(timings: Timings, bob: Conduct, carol: Conduct) -> Market {
    let mut market = Market::open(&[("alice", "50 USDC"), ("bob", "500 CREDITS"), ("carol", "20 DATA")]).await;
    market.timings = timings;
    market
        .join("alice", &[("10 USDC", "95 CREDITS")], Conduct::Honest)
        .await;
    market.join("bob", &[("100 CREDITS", "4.9 DATA")], bob).await;
    market.join("carol", &[("5 DATA", "9.9 USDC")], carol).await;
    market
}

// The proposer holds every signature first. If it then goes quiet, the others must not be left
// waiting on it: each was handed the whole bundle and one of them settles the ring itself.
#[tokio::test]
async fn a_maker_settles_the_ring_when_the_proposer_goes_quiet() {
    let market = ring_of_three(Conduct::SharesButNeverSubmits, Conduct::Honest).await;
    market
        .until("all three see the ring settled", 30, |m| {
            ["alice", "bob", "carol"].iter().all(|name| m.settled(name) == 1)
        })
        .await;
    assert!(market
        .log("alice")
        .contains("no word from the proposer, submitting it myself"));
    // Settled once, by one of them, for exactly the amounts of the ring.
    assert_eq!(
        (market.balance("alice", "USDC"), market.balance("alice", "CREDITS")),
        (40_000_000, 99_990)
    );
    assert_eq!(
        (market.balance("bob", "CREDITS"), market.balance("bob", "DATA")),
        (400_000, 4_999)
    );
    assert_eq!(
        (market.balance("carol", "DATA"), market.balance("carol", "USDC")),
        (15_000, 9_999_000)
    );
}

// A notice is hearsay. One saying a ring settled must not keep a maker from settling it: whoever
// said so every moment, the proposer that went quiet or anyone listening, held the ring until its
// signatures expired.
#[tokio::test]
async fn a_false_notice_does_not_keep_a_maker_from_settling_the_ring() {
    let market = ring_of_three(Conduct::SharesAndClaimsItSettled, Conduct::Honest).await;
    market
        .until("all three see the ring settled", 30, |m| {
            ["alice", "bob", "carol"].iter().all(|name| m.settled(name) == 1)
        })
        .await;
    let submitted = |name| market.log(name).contains("submitting it myself");
    assert!(submitted("alice") || submitted("carol"));
    // On the first try, not on a later one after the signatures had expired.
    for name in ["alice", "carol"] {
        assert!(!market.log(name).contains("expired unsettled"), "{}", market.log(name));
    }
    // Settled once, on chain, whatever was said: the balances are the ring's.
    assert_eq!(market.devnet.transactions().len(), 1);
    assert_eq!(
        (market.balance("alice", "USDC"), market.balance("alice", "CREDITS")),
        (40_000_000, 99_990)
    );
    assert_eq!(
        (market.balance("carol", "DATA"), market.balance("carol", "USDC")),
        (15_000, 9_999_000)
    );
}

// A maker's one try at settling a ring itself begins with a free look at the chain. A look that
// failed used to spend the try with nothing sent, and a ring whose proposer had gone quiet then
// expired with every signature in its makers' hands.
#[tokio::test]
async fn a_maker_whose_look_at_the_chain_failed_tries_again() {
    // A deadline far enough off for a second try: a maker looks again three seconds after a look
    // that failed.
    let timings = Timings {
        intent_ttl: Duration::from_secs(20),
        ..Timings::fast()
    };
    let market = ring_of_three_on(timings, Conduct::SharesButNeverSubmits, Conduct::Honest).await;
    // Until a maker's turn to submit comes nobody asks about a nonce, so these two are the first
    // look of each of the two makers who hold the bundle.
    market.devnet.fail_next("is_nonce_used", 2);
    market
        .until("all three see the ring settled", 30, |m| {
            ["alice", "bob", "carol"].iter().all(|name| m.settled(name) == 1)
        })
        .await;
    // With the signatures of the first try, not with those of a second ring after they expired.
    for name in ["alice", "carol"] {
        assert!(!market.log(name).contains("expired unsettled"), "{}", market.log(name));
    }
    assert_eq!(market.devnet.transactions().len(), 1);
    assert_eq!(
        (market.balance("bob", "CREDITS"), market.balance("bob", "DATA")),
        (400_000, 4_999)
    );
}

// A maker that never answers costs the others a few seconds and nothing else. The proposer's own
// signature never left it, so it is free at once; a maker that did sign stays committed until its
// signature can no longer be used, then asks the chain and reopens.
#[tokio::test]
async fn a_silent_maker_costs_the_others_only_time() {
    let market = ring_of_three(Conduct::Honest, Conduct::IgnoresProposals).await;
    market
        .until("bob gives up on the ring", 20, |m| {
            m.log("bob").contains("fell through")
        })
        .await;
    market
        .until("alice's signed leg expires and her want reopens", 30, |m| {
            m.log("alice").contains("expired unsettled; want #1 is open again")
        })
        .await;
    assert!(market.is_free("alice"));
    assert_eq!(
        market.settled("alice") + market.settled("bob") + market.settled("carol"),
        0
    );
    // Nothing moved.
    assert_eq!(market.balance("alice", "USDC"), 50_000_000);
    assert_eq!(market.balance("bob", "CREDITS"), 500_000);
    assert_eq!(market.balance("carol", "DATA"), 20_000);
}

// The worst a proposer can do with the signatures it collected is nothing. They are only good
// together and only until the deadline every leg was signed with.
#[tokio::test]
async fn signatures_a_proposer_sits_on_expire() {
    let market = ring_of_three(Conduct::Withholds, Conduct::Honest).await;
    market
        .until("alice and carol sign", 20, |m| {
            ["alice", "carol"]
                .iter()
                .all(|name| m.log(name).contains("signed my leg:"))
        })
        .await;
    assert!(
        !market.is_free("alice") && !market.is_free("carol"),
        "a signed leg commits the want"
    );
    market
        .until("their wants reopen after the deadline", 30, |m| {
            ["alice", "carol"]
                .iter()
                .all(|name| m.log(name).contains("expired unsettled"))
        })
        .await;
    assert_eq!(market.balance("alice", "USDC"), 50_000_000);
    assert_eq!(market.balance("carol", "DATA"), 20_000);
}

// A want is filled by whoever turns up, a part at a time, and never below the price first asked.
#[tokio::test]
async fn a_want_fills_in_parts_at_its_price() {
    let mut market = Market::open(&[("alice", "50 USDC"), ("bob", "500 CREDITS"), ("carol", "500 CREDITS")]).await;
    // Alice buys credits at 9.5 or more per USDC. Bob sells 40 at up to 10 per USDC.
    market
        .join("alice", &[("10 USDC", "95 CREDITS")], Conduct::Honest)
        .await;
    market.join("bob", &[("40 CREDITS", "3.9 USDC")], Conduct::Honest).await;
    market
        .until("the first part settles", 30, |m| {
            m.settled("alice") == 1 && m.settled("bob") == 1
        })
        .await;

    // Bob gave all 40 credits; Alice paid for them at her own price and has the rest on offer.
    assert_eq!(market.balance("bob", "CREDITS"), 460_000);
    let received = market.balance("alice", "CREDITS");
    assert_eq!(received, 39_996, "40 credits less the 0.01% fee");
    let paid = 50_000_000 - market.balance("alice", "USDC");
    assert!(
        paid * 95_000 <= u128::from(received as u64) * 10_000_000,
        "never worse than 9.5 credits per USDC"
    );
    market
        .until("alice announces what is left", 10, |m| {
            m.view("alice").wants[0].remaining == 10_000_000 - paid && m.view("alice").wants[0].locked.is_none()
        })
        .await;
    let left = market.view("alice").wants[0].clone();
    assert_eq!(left.closed, None);
    assert!(
        left.take_for_remaining * 10_000_000 >= left.remaining * 95_000,
        "the rest keeps the price"
    );

    // Carol sells enough for the rest.
    market
        .join("carol", &[("100 CREDITS", "9 USDC")], Conduct::Honest)
        .await;
    market
        .until("the rest settles", 30, |m| {
            m.settled("alice") == 2 && m.settled("carol") == 1
        })
        .await;
    market
        .until("alice's want closes", 10, |m| {
            m.view("alice").wants[0].closed == Some(Closed::Filled)
        })
        .await;
    assert_eq!(market.balance("alice", "USDC"), 40_000_000);
}

// Cancelling is an announcement like any other; once heard, nobody proposes a ring through it.
#[tokio::test]
async fn a_cancelled_want_is_left_alone() {
    let mut market = Market::open(&[("alice", "50 USDC"), ("bob", "500 CREDITS")]).await;
    market
        .join("alice", &[("10 USDC", "95 CREDITS")], Conduct::Honest)
        .await;
    market.tell("alice", Command::Cancel(1)).await;
    market
        .until("alice's want is closed", 10, |m| {
            m.view("alice").wants[0].closed == Some(Closed::Cancelled)
        })
        .await;
    market.join("bob", &[("100 CREDITS", "9 USDC")], Conduct::Honest).await;
    market
        .until("bob is connected and has read the verifier", 20, |m| {
            m.view("bob").peers.len() == 1 && m.view("bob").fee_pips.is_some()
        })
        .await;
    // Long enough for several rounds of announcements and ring searches.
    sleep(Duration::from_secs(3)).await;
    assert_eq!(market.settled("alice") + market.settled("bob"), 0);
    assert!(market
        .view("bob")
        .book
        .iter()
        .all(|want| want.maker != market.view("alice").account));
    assert_eq!(market.balance("alice", "USDC"), 50_000_000);

    // And a want typed in for more than the node holds is not opened at all.
    let too_much = Command::Want {
        give: market.tokens.parse("600 CREDITS").unwrap(),
        take: market.tokens.parse("50 USDC").unwrap(),
    };
    market.tell("bob", too_much).await;
    market
        .until("bob refuses it", 10, |m| m.log("bob").contains("not opened"))
        .await;
    assert_eq!(market.view("bob").wants.len(), 1);
}

// The withdraw is how funds come back from mainnet. It must take exactly what was asked, take
// everything when no amount is given, and when the verifier would say no, stop at the free dry run:
// a transaction that fails on chain still costs its gas.
#[tokio::test]
async fn a_withdraw_takes_what_was_asked_and_is_dry_run_before_it_is_sent() {
    let market = Market::open(&[("alice", "50 USDC")]).await;
    let alice = market.config("alice");
    let to = "alice.near";

    let some = crate::withdraw::withdraw(&alice, "USDC", Some("20"), to).await.unwrap();
    assert_eq!(
        (some.amount, some.before, some.after.clone()),
        (20_000_000, 50_000_000, Ok(30_000_000))
    );
    assert_eq!(market.balance("alice", "USDC"), 30_000_000);
    assert!(some.report(&market.tokens)[3].contains("on its way to alice.near"));

    let mut sent = market.devnet.events();
    let too_much = crate::withdraw::withdraw(&alice, "USDC", Some("30.000001"), to)
        .await
        .unwrap_err();
    let reason = format!("{too_much:#}");
    assert!(
        reason.contains("dry run") && reason.contains("insufficient balance or overflow"),
        "{reason}"
    );
    assert!(
        sent.try_recv().is_err(),
        "a transaction was sent after the dry run refused"
    );
    assert_eq!(market.balance("alice", "USDC"), 30_000_000);

    let rest = crate::withdraw::withdraw(&alice, "USDC", None, to).await.unwrap();
    assert_eq!((rest.amount, rest.after.clone()), (30_000_000, Ok(0)));
    // The same check as above, seen to catch a transaction when there is one.
    assert!(sent.try_recv().is_ok());

    let nothing = crate::withdraw::withdraw(&alice, "USDC", None, to).await.unwrap_err();
    assert!(nothing.to_string().contains("holds no USDC"), "{nothing}");
}

// The preflight is run against mainnet before any funds move. Here it runs against the devnet,
// whose rules are held to the contract's engine by `conformance`: if the two ever disagreed on
// these five answers, the preflight would cry wolf on mainnet, or worse, stay quiet.
#[tokio::test]
async fn the_preflight_gets_the_answers_it_expects() {
    let market = Market::open(&[]).await;
    let verifier = chain::Verifier::new(market.rpc.clone(), VERIFIER);
    let state = verifier.state().await.unwrap();
    let (usdc, data) = (market.tokens.id("USDC").unwrap(), market.tokens.id("DATA").unwrap());
    let probes = crate::preflight::probes(VERIFIER, state, &usdc, &data);
    assert_eq!(probes.len(), 5);
    for probe in probes {
        let answer = verifier.simulate(&probe.signed).await;
        assert!(
            crate::preflight::agrees(&answer, probe.expect),
            "{}: {answer:?}",
            probe.what
        );
    }
}

// Thirty agents whose wants overlap, so that rings compete for them, three of whom quit partway,
// must clear their market: no ring left among the live agents, every transaction and balance as
// it should be, no live want left locked. Before rings were rested until their makers were free,
// and before nodes proposed only the rings that come first for their wants, a market this size
// went round one ring of four until the end; with either, it clears. What needs each of them, and
// waiting out tries a node was asked into, is a hundred: `gossipmarket sim --agents 100`.
//
// The run is told to a board as it goes, as with `--web` and `--record`. The page draws only what
// it is told: a ring between the wrong agents, or no word that the run ended, and what is on
// screen is not the market that settled.
#[tokio::test(flavor = "multi_thread")]
async fn a_crowded_market_clears() {
    let board = crate::web::Board::default();
    crate::crowd::run(30, 1, Some(&board)).await.unwrap();

    let run = board.recording();
    assert_eq!(run["market"]["agents"].as_array().unwrap().len(), 30);
    let events = run["events"].as_array().unwrap();
    let of = |kind: &'static str| events.iter().filter(move |event| event["k"] == kind);
    assert!(of("ring").count() > 0);
    for ring in of("ring") {
        let makers: Vec<u64> = ring["makers"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|m| m.as_u64())
            .collect();
        assert!(
            (2..=4).contains(&makers.len()) && makers.iter().all(|&maker| maker < 30),
            "{ring}"
        );
        assert_eq!(ring["gives"].as_array().unwrap().len(), makers.len(), "{ring}");
    }
    assert!(of("pulse").all(|pulse| pulse["s"].as_str().unwrap().len() == 30));
    // An agent set to quit in the middle of a ring quits only if it gets into one, so how many
    // quit is the run's to say; the page must be told of each that did.
    let gone = of("pulse")
        .next_back()
        .map_or(0, |pulse| pulse["s"].as_str().unwrap().matches('x').count());
    assert_eq!(of("quit").count(), gone);
    assert_eq!(of("check").count(), 5);
    let last = events.last().unwrap();
    assert_eq!(
        (&last["k"], &last["ok"]),
        (&serde_json::json!("done"), &serde_json::json!(true)),
        "{last}"
    );
}

// Only a ring's makers propose it. If a stranger's proposal were taken up, anyone who hears the
// gossip could keep a ring's makers holding off proposing it themselves, by proposing it to them
// every so often.
#[tokio::test]
async fn a_proposal_from_outside_the_ring_is_turned_away() {
    // Carol never answers, so the ring stays open and the stranger keeps proposing it.
    let mut market = ring_of_three(Conduct::Honest, Conduct::IgnoresProposals).await;
    market.join("dave", &[], Conduct::ProposesOthersRings).await;
    market
        .until("alice and bob turn dave away", 20, |m| {
            ["alice", "bob"]
                .iter()
                .all(|name| m.log(name).contains("the proposer is not in the ring"))
        })
        .await;
}

// A node proposes only rings it can pay for, and of those the ones that come first. Choosing first
// and checking the balance after, a ring too big for what the node holds hid every ring through
// the same want, and none of them settled.
#[tokio::test]
async fn a_ring_a_maker_cannot_pay_for_does_not_hide_one_it_can() {
    let mut market = Market::open(&[("alice", "6 USDC"), ("bob", "500 CREDITS"), ("carol", "500 CREDITS")]).await;
    // More than Alice holds: a ring with Bob takes all 10, one with Carol only 4.
    market
        .join("alice", &[("10 USDC", "95 CREDITS")], Conduct::Honest)
        .await;
    market.join("bob", &[("100 CREDITS", "10 USDC")], Conduct::Honest).await;
    market.join("carol", &[("40 CREDITS", "4 USDC")], Conduct::Honest).await;
    market
        .until("alice hears both wants", 20, |m| m.view("alice").book.len() == 3)
        .await;
    // Otherwise the ring with Carol comes first anyway, and this proves nothing.
    let first = market::find_rings(&market.view("alice").book, 100).remove(0);
    let bob = devnet_key("bob").account_id();
    assert!(first.leg_of(&bob).is_some(), "the ring with Bob comes first: {first:?}");

    market
        .until("alice and carol settle", 30, |m| {
            m.settled("alice") == 1 && m.settled("carol") == 1
        })
        .await;
    assert_eq!(market.balance("carol", "CREDITS"), 460_000, "carol gave her 40 credits");
    assert_eq!(market.balance("bob", "CREDITS"), 500_000, "bob gave nothing");
}
