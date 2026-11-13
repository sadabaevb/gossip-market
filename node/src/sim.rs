//! The whole market on one machine: a devnet verifier and three agents, each with its own key and
//! its own libp2p node, talking over real TCP on localhost. Nothing is mocked between them.
//!
//! It plays the two moments the project exists for. First a ring of three: Alice, Bob and Carol
//! each hold what the next one wants, no two of them can trade, and all three settle in one
//! transaction. Then the node the others dialed first is stopped, and the two that are left settle
//! another trade, because there never was a server.
use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use chain::{Devnet, Verifier};
use intents::Ledger;
use tokio::net::TcpListener;
use tokio::time::{sleep, timeout};

use crate::config::{devnet_key, NodeConfig, Timings, TokenConfig, Tokens, WantConfig};
use crate::node::{self, Command, Handle, Settlement};
use crate::verify;

const VERIFIER: &str = "intents.devnet";
/// What `intents.near` took on 12 November 2026: one pip, a millionth of each amount given.
const FEE_PIPS: u32 = 1;

fn token_table() -> BTreeMap<String, TokenConfig> {
    let token = |id: &str, decimals| TokenConfig {
        id: format!("nep141:{id}.devnet"),
        decimals,
    };
    BTreeMap::from([
        ("USDC".into(), token("usdc", 6)),
        ("CREDITS".into(), token("credits", 3)),
        ("DATA".into(), token("data", 3)),
    ])
}

/// Who holds what, and what each wants for it.
const AGENTS: [(&str, &str, &str, &str); 3] = [
    ("alice", "50 USDC", "10 USDC", "95 CREDITS"),
    ("bob", "500 CREDITS", "100 CREDITS", "4.9 DATA"),
    ("carol", "20 DATA", "5 DATA", "9.9 USDC"),
];

struct Agent {
    name: &'static str,
    account: String,
    handle: Handle,
    /// Log lines already printed.
    printed: u64,
}

impl Agent {
    /// Prints what the node has logged since last time.
    fn print_new_lines(&mut self) {
        let view = self.handle.view.borrow().clone();
        for (seq, line) in &view.log {
            if *seq > self.printed {
                println!("  [{:5}] {line}", self.name);
                self.printed = *seq;
            }
        }
    }

    fn settlements(&self) -> Vec<Settlement> {
        self.handle
            .view
            .borrow()
            .settlements
            .iter()
            .filter(|settled| settled.mine)
            .cloned()
            .collect()
    }
}

/// Waits until `done` holds, printing the agents' logs as they come.
async fn until(agents: &mut [Agent], what: &str, limit: Duration, done: impl Fn(&[Agent]) -> bool) -> Result<()> {
    let waited = timeout(limit, async {
        loop {
            agents.iter_mut().for_each(Agent::print_new_lines);
            if done(agents) {
                return;
            }
            sleep(Duration::from_millis(50)).await;
        }
    });
    waited
        .await
        .map_err(|_| anyhow!("timed out after {limit:?} waiting until {what}"))
}

fn config(name: &str, rpc: &str, bootstrap: Vec<String>, wants: Vec<WantConfig>) -> NodeConfig {
    NodeConfig {
        name: name.into(),
        key: format!("devnet:{name}"),
        // Loopback only, and no mDNS: the simulation must behave the same in a container, on a
        // laptop and in CI.
        listen: "/ip4/127.0.0.1/tcp/0".into(),
        bootstrap,
        mdns: false,
        rpc: rpc.into(),
        verifier: VERIFIER.into(),
        nonce: Default::default(),
        tokens: token_table(),
        wants,
    }
}

pub async fn run() -> Result<()> {
    let tokens = Tokens::from(token_table());
    let id = |symbol: &str| tokens.id(symbol).expect("a token of the simulation");
    let show = |symbol: &str, units| tokens.show(&id(symbol), units);

    println!("== Gossip Market simulation ==");
    println!("A local verifier (the NEAR Intents rules, in memory) and three agents on libp2p over localhost.\n");

    // The verifier, with what each agent has deposited.
    let mut ledger = Ledger::new(VERIFIER, FEE_PIPS);
    for (name, holds, _, _) in AGENTS {
        let (token, units) = tokens.parse(holds)?;
        ledger.deposit(&devnet_key(name).account_id(), &token, units);
    }
    let devnet = Devnet::new(ledger);
    let listener = TcpListener::bind("127.0.0.1:0").await.context("binding the devnet")?;
    let rpc = format!("http://{}", listener.local_addr()?);
    tokio::spawn(devnet.clone().serve(listener));
    let verifier = Verifier::new(rpc.clone(), VERIFIER);
    println!("devnet verifier `{VERIFIER}` at {rpc}, fee {FEE_PIPS} pip (0.0001%, as on mainnet)");
    for (name, holds, give, take) in AGENTS {
        println!("  {name:5} holds {holds:11} and will give {give} for {take} or more");
    }

    // Alice starts alone. Bob and Carol are told her address and nothing else.
    println!("\n-- 1. Three agents find each other --");
    let mut agents: Vec<Agent> = Vec::new();
    for (name, _, give, take) in AGENTS {
        let bootstrap = agents
            .first()
            .map(|first| first.handle.view.borrow().listen.clone())
            .unwrap_or_default();
        let wants = vec![WantConfig {
            give: give.into(),
            take: take.into(),
        }];
        let handle = node::spawn(config(name, &rpc, bootstrap, wants), Timings::fast()).await?;
        agents.push(Agent {
            name,
            account: devnet_key(name).account_id(),
            handle,
            printed: 0,
        });
    }
    until(
        &mut agents,
        "every agent is connected to both others",
        Duration::from_secs(20),
        |agents| agents.iter().all(|agent| agent.handle.view.borrow().peers.len() == 2),
    )
    .await?;
    println!("  => each agent is connected to the other two; Bob and Carol met through Alice.");

    println!("\n-- 2. A ring of three settles in one transaction --");
    println!("  No two of these agents can trade: nobody wants what the next one has to give.");
    until(
        &mut agents,
        "all three see the ring settled",
        Duration::from_secs(40),
        |agents| agents.iter().all(|agent| !agent.settlements().is_empty()),
    )
    .await?;

    let settled = agents[0].settlements().remove(0);
    let ring_id = settled.ring.id();
    if agents.iter().any(|agent| agent.settlements()[0].ring.id() != ring_id) {
        bail!("the agents disagree on which ring settled");
    }
    if settled.ring.legs.len() != 3 {
        bail!("expected a ring of three, got {} legs", settled.ring.legs.len());
    }
    // One of them sent the transaction; the others learned its hash from the notice.
    let (tx, submitter) = agents
        .iter()
        .filter_map(|agent| {
            let settled = agent.settlements().remove(0);
            settled.tx.zip(settled.submitter)
        })
        .next()
        .context("no agent knows the transaction")?;
    println!("\n  Reading the transaction back from the verifier and checking it from scratch:");
    let checked = verify::check(&verifier, &tx, &submitter).await?;
    for line in checked.report(&tokens) {
        println!("  {line}");
    }
    let signers: Vec<&String> = checked.legs.iter().map(|(signer, _)| signer).collect();
    if checked.legs.len() != 3 || agents.iter().any(|agent| !signers.contains(&&agent.account)) {
        bail!("the transaction does not hold one leg from each agent");
    }

    // The balances inside the verifier, which is where the trade happened.
    let ledger = devnet.ledger();
    let balance = |name: &str, symbol: &str| ledger.balance_of(&devnet_key(name).account_id(), &id(symbol));
    let expected = [
        ("alice", "USDC", 40_000_000, "CREDITS", 99_999),
        ("bob", "CREDITS", 400_000, "DATA", 4_999),
        ("carol", "DATA", 15_000, "USDC", 9_999_990),
    ];
    println!("\n  Balances in the verifier afterwards:");
    for (name, gave, gave_left, got, got_now) in expected {
        let (left, now) = (balance(name, gave), balance(name, got));
        println!("  {name:5} {:>14} {:>16}", show(gave, left), show(got, now));
        if (left, now) != (gave_left, got_now) {
            bail!(
                "{name} should hold {} and {}",
                show(gave, gave_left),
                show(got, got_now)
            );
        }
    }
    println!("  => each gave what it offered and received what the previous one gave, less the fee.");

    println!("\n-- 3. The first node is stopped; the market carries on --");
    let alice = agents.remove(0);
    alice.handle.commands.send(Command::Quit).await.ok();
    alice.handle.task.await.ok();
    println!("  alice (the node the others dialed first) has exited.");
    until(&mut agents, "bob and carol notice", Duration::from_secs(20), |agents| {
        agents.iter().all(|agent| agent.handle.view.borrow().peers.len() == 1)
    })
    .await?;

    // Bob and Carol now want each other's tokens directly.
    let orders = [("50 CREDITS", "2 DATA"), ("3 DATA", "40 CREDITS")];
    for (agent, (give, take)) in agents.iter().zip(orders) {
        let want = Command::Want {
            give: tokens.parse(give)?,
            take: tokens.parse(take)?,
        };
        agent.handle.commands.send(want).await.ok();
        println!("  {} opens: {give} for {take} or more", agent.name);
    }
    until(
        &mut agents,
        "bob and carol settle a second trade",
        Duration::from_secs(40),
        |agents| agents.iter().all(|agent| agent.settlements().len() == 2),
    )
    .await?;
    let second = agents[0].settlements().remove(1);
    if second.ring.legs.len() != 2 || agents[1].settlements()[1].ring.id() != second.ring.id() {
        bail!("bob and carol should have settled one ring of two");
    }
    let ledger = devnet.ledger();
    let fees: u128 = ["USDC", "CREDITS", "DATA"]
        .iter()
        .map(|symbol| ledger.balance_of("fees.devnet", &id(symbol)))
        .sum();
    if fees == 0 {
        bail!("the verifier collected no fee");
    }
    println!(
        "  => bob and carol settled {} with nobody in between.",
        second.ring.id()
    );

    for agent in agents {
        agent.handle.commands.send(Command::Quit).await.ok();
        agent.handle.task.await.ok();
    }
    println!("\nSIMULATION PASSED: a three-party ring and a two-party trade settled peer to peer.");
    Ok(())
}
