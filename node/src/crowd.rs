//! The market with many agents: a devnet verifier and `n` agents in one process, each with its own
//! key and libp2p node over real TCP on localhost, as in `sim`. Here the wants overlap, so rings
//! compete for them, and some agents quit partway.
//!
//! What is planted only guarantees there is something to settle: rings of two, three and four
//! priced to close, pairs of rings that both need one want, and agents with several wants. The
//! tokens and prices are shared, so wants from different planted rings also make rings nobody
//! planted, and which rings settle is the market's doing and varies from run to run. Before the
//! agents start, every planted ring is checked to close at the verifier's fee. What must hold every
//! run is checked at the end:
//!
//! - something settled, and no ring is left among the live agents' open wants;
//! - every transaction reads back and checks out from scratch;
//! - every balance is what the transactions say, and every token adds up to what was deposited;
//! - every live maker of a settled ring saw its own leg settle;
//! - no live maker is left with a want locked, or held a complete bundle that never settled.
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context, Result};
use chain::Verifier;
use intents::{fee_ceil, net_of_fee, AccountId, Ledger, TokenId};
use market::{find_rings, Want};
use serde_json::json;
use tokio::time::{sleep, timeout};

use crate::config::{devnet_key, NodeConfig, Timings, TokenConfig, Tokens, WantConfig};
use crate::node::{self, Command, Handle};
use crate::sim::{self, FEE_PIPS, VERIFIER};
use crate::verify::{self, Checked};
use crate::web::Board;

/// How long the market has to clear once every agent is up. Several times what it takes with the
/// processor to itself, about a minute for a hundred agents on four or five cores: the run is
/// bound by the processor, and one that lost it to something else for a minute was failed at three
/// minutes with its last rings still settling.
const LIMIT: Duration = Duration::from_secs(600);

/// Symbol, contract, decimals, and a price per whole token in millionths of a dollar. Everyone
/// prices at these, a little in the next maker's favour, so a cycle of wants closes whoever planted
/// it, unless sizing cuts a leg of GPU down: its fee is a whole unit whatever the amount, and a
/// smaller leg pays it out of a smaller margin. The decimals span 0 to 24, as real tokens do.
const TOKENS: [(&str, &str, u32, u128); 5] = [
    ("USDC", "usdc", 6, 1_000_000),
    ("NEAR", "wrap", 24, 3_000_000),
    ("CREDITS", "credits", 3, 10_000),
    ("DATA", "data", 3, 5_000_000),
    ("GPU", "gpu", 0, 2_000_000),
];

fn token_table() -> BTreeMap<String, TokenConfig> {
    TOKENS
        .iter()
        .enumerate()
        .map(|(token, (symbol, _, decimals, _))| {
            let config = TokenConfig {
                id: token_id(token),
                decimals: *decimals,
            };
            (symbol.to_string(), config)
        })
        .collect()
}

fn token_id(token: usize) -> TokenId {
    format!("nep141:{}.devnet", TOKENS[token].1)
}

/// splitmix64: the same plan from the same seed on every machine.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            items.swap(i, self.below(i + 1));
        }
    }

    /// `k` different tokens, in random order: the way value flows round a ring.
    fn flow(&mut self, k: usize) -> Vec<usize> {
        let mut tokens: Vec<usize> = (0..TOKENS.len()).collect();
        self.shuffle(&mut tokens);
        tokens.truncate(k);
        tokens
    }

    /// Between $5 and $50, in millionths of a dollar.
    fn value(&mut self) -> u128 {
        (5 + self.below(46) as u128) * 1_000_000
    }

    /// Between 0.5% and 2%, in hundredths of a percent.
    fn margin(&mut self) -> u128 {
        50 + self.below(151) as u128
    }
}

/// About `value` of `give`, for what that is worth in `take` less `margin` hundredths of a percent
/// and less the verifier's fee on it. The price is set on what the rounded amount given is worth,
/// so that rounding a token with few decimals never makes a want ask more than it gives. The fee
/// is at least one unit of what the next maker gives, which for a token with no decimals is worth
/// more than any margin here: asked for in full, a ring that received one never closed.
fn amounts(give: usize, take: usize, value: u128, margin: u128) -> (u128, u128) {
    let (_, _, give_decimals, give_price) = TOKENS[give];
    let (_, _, take_decimals, take_price) = TOKENS[take];
    let give_units = value * 10u128.pow(give_decimals) / give_price;
    let given = give_units * give_price / 10u128.pow(give_decimals);
    let take_units = given * (10_000 - margin) / 10_000 * 10u128.pow(take_decimals) / take_price;
    let take_units = net_of_fee(&token_id(take), take_units, FEE_PIPS).expect("amounts far from overflowing");
    (give_units, take_units)
}

enum Quit {
    /// At its first proposal or signature: in the middle of settling a ring.
    MidRing,
    /// This long after every agent is up.
    At(Duration),
}

#[derive(Default)]
struct Plan {
    /// (give token, units, take token, units)
    wants: Vec<(usize, u128, usize, u128)>,
    quit: Option<Quit>,
}

/// What was planted, for the reader and for counting the rings that settled as planted.
#[derive(Default)]
struct Planted {
    rings: [usize; 3],
    contested: usize,
    extra: usize,
    /// Each planted ring's wants: (maker, want), by index into the plans and into its wants.
    legs: Vec<Vec<(usize, usize)>>,
}

/// One ring: maker `i` gives `flow[i]` and takes what maker `i - 1` gives. Returns its wants.
fn plant_ring(plans: &mut [Plan], makers: &[usize], flow: &[usize], value: u128, rng: &mut Rng) -> Vec<(usize, usize)> {
    let k = makers.len();
    (0..k)
        .map(|i| {
            let (give, take) = (flow[i], flow[(i + k - 1) % k]);
            let (give_units, take_units) = amounts(give, take, value, rng.margin());
            plans[makers[i]].wants.push((give, give_units, take, take_units));
            (makers[i], plans[makers[i]].wants.len() - 1)
        })
        .collect()
}

/// The plan for `n` agents. For 100: 8 rings of two, 10 of three, 6 of four, 6 pairs of rings of
/// three that both need one want, 6 more rings through agents that already have a want, and 10
/// agents that quit.
fn plan(n: usize, rng: &mut Rng) -> (Vec<Plan>, Planted) {
    let mut plans: Vec<Plan> = (0..n).map(|_| Plan::default()).collect();
    let mut planted = Planted::default();
    // Agents are handed out in order here and shuffled at the end.
    let mut next = 0;
    fn take(next: &mut usize, count: usize) -> Vec<usize> {
        let makers: Vec<usize> = (*next..*next + count).collect();
        *next += count;
        makers
    }

    let mut shapes: Vec<usize> = [(2, n * 8 / 100), (3, n * 10 / 100), (4, n * 6 / 100)]
        .iter()
        .flat_map(|&(k, count)| std::iter::repeat_n(k, count))
        .collect();
    let contested = n * 6 / 100;
    // Whoever is left over goes into rings of two and three.
    let mut left = n - shapes.iter().sum::<usize>() - 5 * contested;
    while left >= 2 {
        let k = if left == 3 { 3 } else { 2 };
        shapes.push(k);
        left -= k;
    }
    for &k in &shapes {
        let makers = take(&mut next, k);
        let flow = rng.flow(k);
        let value = rng.value();
        let legs = plant_ring(&mut plans, &makers, &flow, value, rng);
        planted.rings[k - 2] += 1;
        planted.legs.push(legs);
    }
    // An agent that fits nowhere still announces a want, which asks twice what it gives.
    if left == 1 {
        let [maker] = take(&mut next, 1)[..] else {
            unreachable!()
        };
        let flow = rng.flow(2);
        let (give_units, take_units) = amounts(flow[0], flow[1], rng.value(), 0);
        plans[maker].wants.push((flow[0], give_units, flow[1], take_units * 2));
    }
    let disjoint = next;

    // Two rings of three through one want, which is only enough for one of them.
    for _ in 0..contested {
        let [w, a1, a2, b1, b2] = take(&mut next, 5)[..] else {
            unreachable!()
        };
        let flow = rng.flow(3);
        let value = rng.value();
        let legs = plant_ring(&mut plans, &[w, a1, a2], &flow, value, rng);
        let other = loop {
            let token = rng.below(TOKENS.len());
            if token != flow[0] && token != flow[2] {
                break token;
            }
        };
        let (b1_gives, b1_takes) = amounts(other, flow[0], value, rng.margin());
        plans[b1].wants.push((other, b1_gives, flow[0], b1_takes));
        let (b2_gives, b2_takes) = amounts(flow[2], other, value, rng.margin());
        plans[b2].wants.push((flow[2], b2_gives, other, b2_takes));
        let contesting = vec![
            legs[0],
            (b1, plans[b1].wants.len() - 1),
            (b2, plans[b2].wants.len() - 1),
        ];
        planted.legs.push(legs);
        planted.legs.push(contesting);
    }
    planted.contested = contested;

    // More rings through agents of the disjoint rings above, each giving a token it does not
    // already give: two wants giving the same token could be signed into two rings at once, and
    // the second would fail on the balance.
    for extra in 0..n * 6 / 100 {
        let k = 3 + extra % 2;
        for _attempt in 0..1_000 {
            let flow = rng.flow(k);
            let mut makers: Vec<usize> = Vec::new();
            for &give in &flow {
                let fits = |m: usize| {
                    plans[m].wants.len() < 3
                        && !makers.contains(&m)
                        && plans[m].wants.iter().all(|&(gives, ..)| gives != give)
                };
                let candidates: Vec<usize> = (0..disjoint).filter(|&m| fits(m)).collect();
                if candidates.is_empty() {
                    break;
                }
                makers.push(candidates[rng.below(candidates.len())]);
            }
            if makers.len() == k {
                let value = rng.value();
                let legs = plant_ring(&mut plans, &makers, &flow, value, rng);
                planted.extra += 1;
                planted.legs.push(legs);
                break;
            }
        }
    }

    // One agent in ten quits: half in the middle of a ring, half at a moment set now.
    let mut quitters: Vec<usize> = (0..n).collect();
    rng.shuffle(&mut quitters);
    for (i, &agent) in quitters.iter().take(n / 10).enumerate() {
        plans[agent].quit = Some(match i % 2 {
            0 => Quit::MidRing,
            _ => Quit::At(Duration::from_millis(500 + rng.below(3_500) as u64)),
        });
    }

    // Ring-mates were handed out side by side. Started side by side, they would mostly be told
    // each other's addresses; shuffled, they have to find each other.
    let mut order: Vec<(usize, Plan)> = plans.into_iter().enumerate().collect();
    rng.shuffle(&mut order);
    let mut position = vec![0; n];
    for (new, (old, _)) in order.iter().enumerate() {
        position[*old] = new;
    }
    let plans = order.into_iter().map(|(_, plan)| plan).collect();
    for (agent, _) in planted.legs.iter_mut().flatten() {
        *agent = position[*agent];
    }
    (plans, planted)
}

struct Agent {
    name: String,
    account: AccountId,
    handle: Handle,
    quit: Option<Quit>,
    /// When it quit, and what it was doing.
    gone: Option<(Duration, String)>,
    /// Log lines already read.
    read: u64,
}

impl Agent {
    /// How the page draws the agent: gone, with a want signed into a ring, with one still open,
    /// or with nothing left to trade.
    fn letter(&self) -> char {
        if self.gone.is_some() {
            return 'x';
        }
        let view = self.handle.view.borrow();
        if view.wants.iter().any(|want| want.locked.is_some()) {
            'L'
        } else if view
            .wants
            .iter()
            .any(|want| want.closed.is_none() && want.remaining > 0)
        {
            'o'
        } else {
            '.'
        }
    }
}

/// The contract's own words out of a failed transaction's status, which wraps them in JSON.
fn reason_of(failure: &str) -> &str {
    failure
        .split_once("panicked: ")
        .map_or(failure, |(_, rest)| rest.split('"').next().unwrap_or(rest))
}

/// What the agents' logs say happened. Read for the summary; nothing passes or fails on it.
#[derive(Default)]
struct Tally {
    proposals: usize,
    signed: usize,
    declined: usize,
    fell_through: usize,
    expired: usize,
    /// A maker that held the bundle sent it because the proposer had not.
    backups: usize,
    /// Why proposals were declined and rings fell through, with ring ids taken out.
    reasons: BTreeMap<String, usize>,
    /// The rings whose transaction the verifier refused, as their submitters logged it.
    refused: BTreeSet<String>,
}

impl Tally {
    /// Counts one log line. Returns what the agent is doing, if the line is one a mid-ring quitter
    /// quits at.
    fn read(&mut self, line: &str) -> Option<String> {
        let line = line.split_once(' ').map_or(line, |(_, rest)| rest);
        if let Some(rest) = line.strip_prefix("proposing ring ") {
            self.proposals += 1;
            return Some(format!("proposing ring {}", rest.split(':').next().unwrap_or(rest)));
        }
        if line.starts_with("signed my leg") {
            self.signed += 1;
            let ring = line
                .rsplit_once("(ring ")
                .map_or("", |(_, id)| id.trim_end_matches(')'));
            return Some(format!("having signed its leg of ring {ring}"));
        }
        if let Some(rest) = line.strip_prefix("declined ring ") {
            self.declined += 1;
            let (ring, reason) = rest.split_once(": ").unwrap_or((rest, ""));
            *self
                .reasons
                .entry(format!("declined: {}", Self::plain(ring, reason)))
                .or_default() += 1;
        } else if let Some((ring, reason)) = line
            .strip_prefix("ring ")
            .and_then(|rest| rest.split_once(" fell through: "))
        {
            self.fell_through += 1;
            *self
                .reasons
                .entry(format!("fell through: {}", Self::plain(ring, reason)))
                .or_default() += 1;
        } else if let Some((ring, _)) = line
            .strip_prefix("ring ")
            .and_then(|rest| rest.split_once(": the verifier refused: "))
        {
            self.refused.insert(ring.to_owned());
        } else if line.contains(" expired unsettled") {
            self.expired += 1;
        } else if line.contains("no word from the proposer") {
            self.backups += 1;
        }
        None
    }

    /// A reason with the ring ids taken out, so that alike reasons count together. `ring` starts
    /// with the id of the ring the reason is about.
    fn plain(ring: &str, reason: &str) -> String {
        let this = ring.split_whitespace().next().unwrap_or(ring);
        let reason = reason.strip_prefix("a maker declined: ").unwrap_or(reason);
        match reason.strip_prefix(node::COMMITTED) {
            Some(other) if other.starts_with(this) => "the want is committed to an earlier try of this ring".into(),
            Some(_) => "the want is committed to another ring".into(),
            None => reason.to_owned(),
        }
    }
}

/// A settlement read back from the devnet.
struct Settled {
    at: Duration,
    ring_id: String,
    checked: Checked,
}

fn ring_id_of(checked: &Checked) -> String {
    let memo = checked
        .legs
        .first()
        .and_then(|(_, diff)| diff.memo.clone())
        .unwrap_or_default();
    memo.strip_prefix("gossipmarket:").unwrap_or(&memo).to_owned()
}

fn median(mut values: Vec<usize>) -> usize {
    values.sort_unstable();
    values.get(values.len() / 2).copied().unwrap_or(0)
}

/// A want as the ring search is asked about it here: what could still settle, not what a peer
/// announced, so it never expires and has no addresses.
fn open_want(maker: AccountId, id: u64, rev: u64, give: (TokenId, u128), take: (TokenId, u128)) -> Want {
    Want {
        maker,
        id,
        rev,
        give: give.0,
        give_amount: give.1,
        take: take.0,
        take_amount: take.1,
        expires_ms: u64::MAX,
        addrs: Vec::new(),
    }
}

/// A planted ring that cannot close leaves less to settle than the plan says. If none could, the
/// market would clear with nothing traded, and the checks at the end would pass on an empty market.
fn every_planted_ring_closes(plans: &[Plan], planted: &Planted, account: impl Fn(usize) -> AccountId) -> Result<()> {
    for legs in &planted.legs {
        let wants: Vec<Want> = legs
            .iter()
            .map(|&(i, w)| {
                let (give, give_amount, take, take_amount) = plans[i].wants[w];
                open_want(
                    account(i),
                    w as u64,
                    0,
                    (token_id(give), give_amount),
                    (token_id(take), take_amount),
                )
            })
            .collect();
        ensure!(
            find_rings(&wants, FEE_PIPS)
                .iter()
                .any(|ring| ring.legs.len() == legs.len()),
            "a planted ring does not close at the verifier's fee: {wants:#?}"
        );
    }
    Ok(())
}

/// Runs the market. With a `board`, what happens is also told to it as it happens, for the page
/// that draws the run and for a recording of it.
pub async fn run(n: usize, seed: u64, board: Option<&Board>) -> Result<()> {
    ensure!((10..=500).contains(&n), "--agents takes from 10 to 500 agents, not {n}");
    let tokens = Tokens::from(token_table());
    let mut rng = Rng(seed);
    let (plans, planted) = plan(n, &mut rng);
    let name = |i: usize| format!("agent{i:03}");
    let account = |i: usize| devnet_key(&name(i)).account_id();
    every_planted_ring_closes(&plans, &planted, account)?;

    if let Some(board) = board {
        let symbol = |token: usize| TOKENS[token].0;
        let agents: Vec<_> = plans
            .iter()
            .enumerate()
            .map(|(i, plan)| {
                let wants: Vec<_> = plan
                    .wants
                    .iter()
                    .map(|&(give, _, take, _)| json!({ "give": symbol(give), "take": symbol(take) }))
                    .collect();
                json!({ "name": name(i), "wants": wants })
            })
            .collect();
        board.set_market(json!({
            "about": format!(
                "{n} agents on one machine, no server between them. Each dot is a node with its own key. A line \
                 is a ring of two to four of them, settled in one transaction under the rules of the NEAR \
                 Intents verifier."
            ),
            "recorded": format!("a recorded run: {n} agents, seed {seed}"),
            "tokens": TOKENS.map(|token| token.0),
            "agents": agents,
        }));
    }
    println!("== Gossip Market: {n} agents (seed {seed}) ==");
    println!(
        "A local verifier and {n} agents on libp2p over localhost, trading {}.",
        TOKENS.map(|t| t.0).join(", ")
    );
    let several = plans.iter().filter(|plan| plan.wants.len() > 1).count();
    println!(
        "Planted: {} rings of two, {} of three, {} of four; {} pairs of rings of three that need the same want; \
         {} more rings through {several} agents with several wants.",
        planted.rings[0], planted.rings[1], planted.rings[2], planted.contested, planted.extra
    );
    let quitting = plans.iter().filter(|plan| plan.quit.is_some()).count();
    println!(
        "Everyone prices at the same rates, so wants of different planted rings make rings too. \
         {quitting} agents will quit partway."
    );

    let mut ledger = Ledger::new(VERIFIER, FEE_PIPS);
    let mut deposits: BTreeMap<(AccountId, TokenId), u128> = BTreeMap::new();
    for (i, plan) in plans.iter().enumerate() {
        for &(give, units, ..) in &plan.wants {
            ledger.deposit(&account(i), &token_id(give), units);
            *deposits.entry((account(i), token_id(give))).or_default() += units;
        }
    }
    let (devnet, rpc) = sim::serve_devnet(ledger).await?;
    let verifier = Verifier::new(rpc.clone(), VERIFIER);

    // Each agent is told two or three that started before it, and nothing else.
    let started = Instant::now();
    let mut agents: Vec<Agent> = Vec::new();
    for (i, plan) in plans.into_iter().enumerate() {
        let mut earlier: Vec<usize> = (0..i).collect();
        rng.shuffle(&mut earlier);
        earlier.truncate(2 + rng.below(2));
        let bootstrap = earlier
            .iter()
            .flat_map(|&j| agents[j].handle.view.borrow().listen.clone())
            .collect();
        let wants = plan
            .wants
            .iter()
            .map(|&(give, give_units, take, take_units)| WantConfig {
                give: tokens.show(&token_id(give), give_units),
                take: tokens.show(&token_id(take), take_units),
            })
            .collect();
        let config = NodeConfig {
            tokens: token_table(),
            ..sim::config(&name(i), &rpc, bootstrap, wants)
        };
        let handle = node::spawn(config, Timings::fast()).await?;
        agents.push(Agent {
            name: name(i),
            account: account(i),
            handle,
            quit: plan.quit,
            gone: None,
            read: 0,
        });
    }
    let up = started.elapsed();
    println!(
        "\nStarted {n} agents in {:.1}s. Each was told the addresses of two or three that started before it.\n",
        up.as_secs_f64()
    );

    let names: HashMap<AccountId, String> = agents.iter().map(|a| (a.account.clone(), a.name.clone())).collect();
    let who = |account: &str| {
        names
            .get(account)
            .cloned()
            .unwrap_or_else(|| node::short(account).to_owned())
    };
    let index: HashMap<AccountId, usize> = agents
        .iter()
        .enumerate()
        .map(|(i, agent)| (agent.account.clone(), i))
        .collect();
    // What the page last drew of each agent, a letter each, sent again only when it changes.
    let mut drawn = String::new();
    let mut tally = Tally::default();
    let mut settled: Vec<Settled> = Vec::new();
    let mut refused: Vec<(String, String)> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    // Live agents that held a complete bundle, and the ring.
    let mut held: BTreeSet<(usize, String)> = BTreeSet::new();
    let mut cleared_at = None;
    // The live agents' open wants and how many were locked, as last seen: what is left if the
    // market does not clear.
    let mut left: (Vec<Want>, usize) = (Vec::new(), 0);

    while started.elapsed() < up + LIMIT {
        let now = started.elapsed();
        for (i, agent) in agents.iter_mut().enumerate().filter(|(_, agent)| agent.gone.is_none()) {
            let (lines, holding) = {
                let view = agent.handle.view.borrow();
                let lines: Vec<(u64, String)> = view.log.iter().filter(|(seq, _)| *seq > agent.read).cloned().collect();
                (lines, view.holding.clone())
            };
            held.extend(holding.into_iter().map(|ring| (i, ring)));
            let mut doing = None;
            for (seq, line) in lines {
                agent.read = seq;
                doing = doing.or(tally.read(&line));
            }
            let why = match agent.quit {
                Some(Quit::MidRing) => doing,
                Some(Quit::At(after)) if now >= up + after => Some("at its set moment".to_owned()),
                _ => None,
            };
            if let Some(why) = why {
                agent.handle.commands.send(Command::Quit).await.ok();
                println!("[{:6.1}s] {} quits {why}", now.as_secs_f64(), agent.name);
                if let Some(board) = board {
                    board.push(now.saturating_sub(up), "quit", json!({ "a": i, "why": why }));
                }
                agent.gone = Some((now, why));
            }
        }

        // Cleared: every set quit has happened, no live want is locked in a ring, and no ring is
        // left among the live agents' open wants.
        let quits_done = agents
            .iter()
            .all(|agent| !matches!(agent.quit, Some(Quit::At(_))) || agent.gone.is_some());
        let mut open = Vec::new();
        let mut locked = 0;
        for agent in agents.iter().filter(|agent| agent.gone.is_none()) {
            for want in &agent.handle.view.borrow().wants {
                locked += usize::from(want.locked.is_some());
                if want.closed.is_none() && want.remaining > 0 && want.locked.is_none() {
                    open.push(open_want(
                        agent.account.clone(),
                        want.id,
                        want.rev,
                        (want.give.clone(), want.remaining),
                        (want.take.clone(), want.take_for_remaining),
                    ));
                }
            }
        }
        let cleared = quits_done && locked == 0 && find_rings(&open, FEE_PIPS).is_empty();
        if let Some(board) = board {
            let letters: String = agents.iter().map(Agent::letter).collect();
            if letters != drawn {
                board.push(now.saturating_sub(up), "pulse", json!({ "s": letters }));
                drawn = letters;
            }
        }

        // The transactions are read after the agents, not before. A maker unlocks its want only
        // once its ring's transaction is on the devnet, so whatever let the market clear above is
        // already there to read. Read first, the last ring's transaction could land between the
        // two looks, and a market that had cleared was failed for a transaction nobody had read.
        for sent in devnet.transactions() {
            if !seen.insert(sent.hash.clone()) {
                continue;
            }
            if let Some(failure) = sent.failure {
                println!(
                    "[{:6.1}s] refused: tx {}… from {}: {failure}",
                    now.as_secs_f64(),
                    node::short(&sent.hash),
                    who(&sent.signer)
                );
                if let Some(board) = board {
                    let who = index.get(&sent.signer).copied().unwrap_or(usize::MAX);
                    board.push(
                        now.saturating_sub(up),
                        "refused",
                        json!({ "a": who, "why": reason_of(&failure) }),
                    );
                }
                refused.push((sent.hash, failure));
                continue;
            }
            let checked = verify::check(&verifier, &sent.hash, &sent.signer).await?;
            let legs: Vec<String> = checked
                .legs
                .iter()
                .map(|(signer, diff)| {
                    let given = diff.diff.iter().find(|(_, delta)| **delta < 0);
                    let given = given.map_or("nothing".into(), |(token, d)| tokens.show(token, d.unsigned_abs()));
                    format!("{} gives {given}", who(signer))
                })
                .collect();
            let ring_id = ring_id_of(&checked);
            if let Some(board) = board {
                // In the bundle's order, in which each maker gives to the next.
                let makers: Vec<usize> = checked
                    .legs
                    .iter()
                    .map(|(signer, _)| index.get(signer).copied().unwrap_or(usize::MAX))
                    .collect();
                let gives: Vec<String> = checked
                    .legs
                    .iter()
                    .map(|(_, diff)| {
                        let given = diff.diff.iter().find(|(_, delta)| **delta < 0);
                        given.map_or(String::new(), |(token, d)| tokens.show(token, d.unsigned_abs()))
                    })
                    .collect();
                board.push(
                    now.saturating_sub(up),
                    "ring",
                    json!({ "makers": makers, "gives": gives, "tx": node::short(&sent.hash), "id": ring_id }),
                );
            }
            println!(
                "[{:6.1}s] settled #{:<3} ring {ring_id} of {}: {}  (tx {}… sent by {})",
                now.as_secs_f64(),
                settled.len() + 1,
                checked.legs.len(),
                legs.join(" -> "),
                node::short(&sent.hash),
                who(&sent.signer)
            );
            settled.push(Settled {
                at: now,
                ring_id,
                checked,
            });
        }

        if cleared {
            cleared_at = Some(now);
            if let Some(board) = board {
                board.push(now.saturating_sub(up), "cleared", json!({}));
            }
            break;
        }
        left = (open, locked);
        sleep(Duration::from_millis(100)).await;
    }

    // The numbers, whether or not the market cleared.
    let live: Vec<usize> = (0..agents.len()).filter(|&i| agents[i].gone.is_none()).collect();
    let summary = || {
        println!("\n-- Summary --");
        let mut by_size = [0usize; 3];
        for s in &settled {
            if let Some(count) = s.checked.legs.len().checked_sub(2).and_then(|k| by_size.get_mut(k)) {
                *count += 1;
            }
        }
        let makers_of =
            |s: &Settled| -> BTreeSet<AccountId> { s.checked.legs.iter().map(|(signer, _)| signer.clone()).collect() };
        let planted_sets: Vec<BTreeSet<AccountId>> = planted
            .legs
            .iter()
            .map(|legs| legs.iter().map(|&(i, _)| account(i)).collect())
            .collect();
        let as_planted = settled.iter().filter(|s| planted_sets.contains(&makers_of(s))).count();
        println!(
            "  settled      {} rings: {} of two, {} of three, {} of four; {as_planted} exactly as planted",
            settled.len(),
            by_size[0],
            by_size[1],
            by_size[2]
        );
        if !refused.is_empty() {
            println!("  refused      {} transactions, nothing spent but gas", refused.len());
        }
        let times: Vec<f64> = settled.iter().map(|s| (s.at - up).as_secs_f64()).collect();
        if let (Some(first), Some(last)) = (times.first(), times.last()) {
            println!(
                "  timing       after the last agent started: first ring at {first:.1}s, half by {:.1}s, \
                 last at {last:.1}s",
                times[times.len() / 2]
            );
        }
        println!(
            "  proposals    {} proposed, {} legs signed, {} declined",
            tally.proposals, tally.signed, tally.declined
        );
        println!(
            "  fell through {} rings, {} expired unsettled; {} bundles sent by a maker because the proposer had not",
            tally.fell_through, tally.expired, tally.backups
        );
        let mut reasons: Vec<(&String, &usize)> = tally.reasons.iter().collect();
        reasons.sort_by(|a, b| b.1.cmp(a.1));
        for (reason, count) in reasons.iter().take(8) {
            println!("               {count:>4}  {reason}");
        }
        let gone: Vec<String> = agents
            .iter()
            .filter_map(|a| {
                a.gone
                    .as_ref()
                    .map(|(at, why)| format!("{} at {:.1}s {why}", a.name, at.as_secs_f64()))
            })
            .collect();
        println!("  quit         {} agents", gone.len());
        for line in gone {
            println!("                 {line}");
        }
        // Otherwise the count above falls short of the plan with nothing said.
        for agent in agents
            .iter()
            .filter(|a| matches!(a.quit, Some(Quit::MidRing)) && a.gone.is_none())
        {
            println!(
                "                 {} stayed: it was to quit in a ring, and never proposed or signed one",
                agent.name
            );
        }
        let peers: Vec<usize> = live
            .iter()
            .map(|&i| agents[i].handle.view.borrow().peers.len())
            .collect();
        println!(
            "  peers        per live agent: {} to {}, median {} (a node dials at most 64; others dial it)",
            peers.iter().min().unwrap_or(&0),
            peers.iter().max().unwrap_or(&0),
            median(peers.clone())
        );
    };

    let Some(cleared_at) = cleared_at else {
        let (open, locked) = left;
        // What the agents with a locked want were last doing: the first thing to read.
        for agent in agents.iter().filter(|agent| agent.gone.is_none()) {
            let view = agent.handle.view.borrow();
            let Some(want) = view.wants.iter().find(|want| want.locked.is_some()) else {
                continue;
            };
            println!(
                "\n{} want #{} locked to ring {}:",
                agent.name,
                want.id,
                want.locked.as_deref().unwrap_or("")
            );
            for (_, line) in view.log.iter().rev().take(6).collect::<Vec<_>>().into_iter().rev() {
                println!("    {line}");
            }
        }
        let open = find_rings(&open, FEE_PIPS);
        // With nothing locked, what the makers of a ring left open said about it is the next thing.
        if let Some(ring) = open.first() {
            let id = ring.id();
            for leg in &ring.legs {
                let Some(agent) = agents.iter().find(|agent| agent.account == leg.maker) else {
                    continue;
                };
                let view = agent.handle.view.borrow();
                let lines: Vec<&String> = view
                    .log
                    .iter()
                    .map(|(_, line)| line)
                    .filter(|line| line.contains(&id))
                    .collect();
                println!(
                    "\n{} on ring {id}, {} lines, the last of them:",
                    agent.name,
                    lines.len()
                );
                for line in &lines[lines.len().saturating_sub(8)..] {
                    println!("    {line}");
                }
            }
        }
        let rings: Vec<String> = open
            .iter()
            .take(5)
            .map(|ring| {
                let makers: Vec<String> = ring.legs.iter().map(|leg| who(&leg.maker)).collect();
                format!("ring {} of {}", ring.id(), makers.join(", "))
            })
            .collect();
        summary();
        if let Some(board) = board {
            let text = format!("the market did not clear within {} s", LIMIT.as_secs());
            board.push(
                started.elapsed().saturating_sub(up),
                "done",
                json!({ "ok": false, "text": text }),
            );
        }
        bail!(
            "the market did not clear within {}s: {} rings settled, {locked} live wants are locked, and these rings \
             are still open among the live agents' wants: {}",
            LIMIT.as_secs(),
            settled.len(),
            rings.join("; ")
        );
    };

    // A check that passed is said on the terminal and to the page alike.
    let ok = |text: String| {
        println!("  ok  {text}");
        if let Some(board) = board {
            board.push(cleared_at.saturating_sub(up), "check", json!({ "text": text }));
        }
    };
    // The summary follows whatever the checks find, so that a failed check comes with it.
    let checked = (|| -> Result<()> {
        println!("\n-- Checks --");
        ensure!(
            !settled.is_empty(),
            "the market cleared with nothing settled, though every planted ring closes"
        );
        ok(format!(
            "the market cleared at {:.1}s: no ring is left among the {} live agents' open wants, \
             and none is locked",
            cleared_at.as_secs_f64(),
            live.len()
        ));

        // Every transaction was read back from the devnet and checked from scratch above, the last
        // of them after the market was seen to clear. One that arrives after that still fails the
        // run: the balances below are checked against what was read.
        for sent in devnet.transactions() {
            ensure!(
                seen.contains(&sent.hash),
                "transaction {} arrived after the market cleared",
                sent.hash
            );
        }
        let ids: BTreeSet<&String> = settled.iter().map(|s| &s.ring_id).collect();
        ensure!(ids.len() == settled.len(), "a ring settled in two transactions");
        ok(format!(
            "{} transactions read back from the verifier: every signature valid, \
             every one nets to zero after the fee",
            settled.len()
        ));

        // The balances the transactions add up to, against the verifier's.
        let ledger = devnet.ledger();
        let mut expected: BTreeMap<(AccountId, TokenId), i128> = deposits
            .iter()
            .map(|(key, units)| (key.clone(), *units as i128))
            .collect();
        let mut fees: BTreeMap<TokenId, u128> = BTreeMap::new();
        for s in &settled {
            for (signer, diff) in &s.checked.legs {
                for (token, delta) in &diff.diff {
                    *expected.entry((signer.clone(), token.clone())).or_default() += delta;
                    if *delta < 0 {
                        let given = delta.unsigned_abs();
                        *fees.entry(token.clone()).or_default() +=
                            fee_ceil(token, given, FEE_PIPS).context("a fee that overflows")?;
                    }
                }
            }
        }
        for ((holder, token), units) in &expected {
            let actual = ledger.balance_of(holder, token) as i128;
            ensure!(
                actual == *units,
                "{} holds {} of {token}, but the transactions leave it {units}",
                who(holder),
                actual
            );
        }
        for t in 0..TOKENS.len() {
            let token = token_id(t);
            let fee = fees.get(&token).copied().unwrap_or(0);
            ensure!(
                ledger.balance_of(&ledger.fee_collector, &token) == fee,
                "the fees in {token} are not what the transactions took"
            );
            let deposited: u128 = deposits
                .iter()
                .filter(|((_, tok), _)| *tok == token)
                .map(|(_, u)| u)
                .sum();
            let now: u128 = agents
                .iter()
                .map(|a| ledger.balance_of(&a.account, &token))
                .sum::<u128>()
                + fee;
            ensure!(deposited == now, "{token}: {deposited} deposited, {now} there now");
        }
        ok("every balance is what the transactions say; every token adds up to what was deposited".into());

        let by_id: HashMap<&String, &Settled> = settled.iter().map(|s| (&s.ring_id, s)).collect();
        for &i in &live {
            let mine: BTreeSet<String> = agents[i]
                .handle
                .view
                .borrow()
                .settlements
                .iter()
                .filter(|settlement| settlement.mine)
                .map(|settlement| settlement.ring.id())
                .collect();
            for s in &settled {
                let in_it = s.checked.legs.iter().any(|(signer, _)| *signer == agents[i].account);
                ensure!(
                    !in_it || mine.contains(&s.ring_id),
                    "{} is in ring {} on chain but does not know it settled",
                    agents[i].name,
                    s.ring_id
                );
            }
            for id in &mine {
                ensure!(
                    by_id.contains_key(id),
                    "{} thinks ring {id} settled; the verifier has no such transaction",
                    agents[i].name
                );
            }
        }
        ok("every live maker of a settled ring saw its own leg settle, \
            and none thinks a ring settled that did not"
            .into());

        let held_live: Vec<&(usize, String)> = held.iter().filter(|(i, _)| agents[*i].gone.is_none()).collect();
        // A bundle whose transaction the verifier refused rightly never settles: on a machine
        // short of processor, one can reach the verifier after the deadline its legs were signed
        // with. What this check is for is a bundle that went out and nobody settled or was refused.
        let unsettled: Vec<&&(usize, String)> = held_live
            .iter()
            .filter(|(_, ring)| !by_id.contains_key(ring) && !tally.refused.contains(ring))
            .collect();
        // What the holder said about the ring is what tells a backup that never fired from a bundle
        // that should not have settled.
        for (i, ring) in &unsettled {
            println!("\n  {} held every signed leg of ring {ring}:", agents[*i].name);
            for (_, line) in agents[*i]
                .handle
                .view
                .borrow()
                .log
                .iter()
                .filter(|(_, line)| line.contains(ring.as_str()))
            {
                println!("      {line}");
            }
        }
        if let Some((i, ring)) = unsettled.first() {
            bail!(
                "{} held every signed leg of ring {ring}, and it never settled",
                agents[*i].name
            );
        }
        let held_rings: BTreeSet<&String> = held_live.iter().map(|(_, ring)| ring).collect();
        let refused_held = held_rings.iter().filter(|ring| !by_id.contains_key(**ring)).count();
        ok(format!(
            "of the {} rings a live maker held the complete bundle for, {} settled and the verifier refused \
             the transaction of {refused_held}",
            held_rings.len(),
            held_rings.len() - refused_held
        ));
        Ok(())
    })();
    summary();
    let at = cleared_at.saturating_sub(up);
    if let (Some(board), Err(failed)) = (board, &checked) {
        board.push(
            at,
            "done",
            json!({ "ok": false, "text": format!("a check failed: {failed}") }),
        );
    }
    checked?;

    for agent in &agents {
        agent.handle.commands.send(Command::Quit).await.ok();
    }
    for agent in agents {
        let _ = timeout(Duration::from_secs(5), agent.handle.task).await;
    }
    let passed = format!(
        "SIMULATION PASSED: {} rings settled among {n} agents, and the market cleared.",
        settled.len()
    );
    if let Some(board) = board {
        board.push(at, "done", json!({ "ok": true, "text": passed }));
    }
    println!("\n{passed}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every planted ring must close at the verifier's fee, or the hundred-agent run plants less
    // than it says, and a market with nothing that closes would clear and pass with nothing
    // traded. A token with no decimals pays a whole unit in fee, more than any margin; priced
    // without it, about half the rings that received one never closed.
    #[test]
    fn every_planted_ring_closes_for_any_seed() {
        for seed in 1..=20 {
            let (plans, planted) = plan(100, &mut Rng(seed));
            let account = |i: usize| devnet_key(&format!("agent{i:03}")).account_id();
            every_planted_ring_closes(&plans, &planted, account).unwrap_or_else(|e| panic!("seed {seed}: {e:#}"));
        }
    }
}
