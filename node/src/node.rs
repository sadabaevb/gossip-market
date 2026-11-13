//! One agent: it announces its wants, keeps the book it hears, proposes rings it is part of, signs
//! its leg of rings others propose, and settles through the verifier. Everything it believes about
//! a trade it has checked itself, against signatures or against the chain.
//!
//! One task owns the network and all state. Calls to the chain run in tasks of their own and
//! report back, so a slow RPC node never stalls gossip.
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use chain::{ChainError, Verifier, VerifierState};
use chrono::{DateTime, TimeZone, Utc};
use futures::StreamExt;
use intents::{AccountId, Keypair, SignedIntent, TokenId};
use libp2p::request_response::{self, OutboundRequestId, ResponseChannel};
use libp2p::swarm::SwarmEvent;
use libp2p::{gossipsub, identify, kad, mdns, Multiaddr, PeerId, Swarm};
use market::want::take_for;
use market::{first_rings_of, Book, Ring, SignedWant, Want};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::config::{pips, NodeConfig, NonceKind, Timings, Tokens};
use crate::net::{self, account_of, peer_of, Behaviour, BehaviourEvent, Bundle, Notice, Proposal, Request, Response};

/// Addresses a want carries.
const MAX_ADDRS: usize = 6;
/// Peers a node dials on its own account. A market this size is fully connected.
const MAX_PEERS: usize = 64;
/// Lines of log kept for the screen.
const LOG_LINES: usize = 400;
/// The furthest ahead a maker lets a leg it signs stay valid, whatever the proposer asks.
const MAX_INTENT_TTL: Duration = Duration::from_secs(60);

/// What an operator, or the simulation, tells a running node.
#[derive(Debug)]
pub enum Command {
    /// Open a want: give up to this much for at least that much.
    Want {
        give: (TokenId, u128),
        take: (TokenId, u128),
    },
    Cancel(u64),
    Quit,
}

/// Ways a node can let the others down. Only tests build nodes that do; what the honest nodes do
/// about each is what the tests are for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Conduct {
    #[default]
    Honest,
    /// Never answers a proposal.
    #[cfg(test)]
    IgnoresProposals,
    /// Proposes and collects every signature, then does nothing with them.
    #[cfg(test)]
    Withholds,
    /// Proposes, collects, shares the bundle with the makers, and never submits it.
    #[cfg(test)]
    SharesButNeverSubmits,
    /// Proposes the rings it is not in to their makers, over and over.
    #[cfg(test)]
    ProposesOthersRings,
}

/// A ring by the wants it is made of, whatever the amounts, as `Ring::wants` gives it.
type RingWants = Vec<(AccountId, u64, u64)>;

/// A running node, from outside.
pub struct Handle {
    pub commands: mpsc::Sender<Command>,
    pub view: watch::Receiver<View>,
    pub task: JoinHandle<()>,
}

/// What a node shows of itself.
#[derive(Clone, Debug, Default)]
pub struct View {
    pub name: String,
    pub account: AccountId,
    pub peer_id: String,
    pub listen: Vec<String>,
    pub verifier: String,
    pub rpc: String,
    pub fee_pips: Option<u32>,
    /// Connected peers, by account.
    pub peers: Vec<AccountId>,
    pub balances: Vec<(TokenId, u128)>,
    pub wants: Vec<WantView>,
    pub book: Vec<Want>,
    pub settlements: Vec<Settlement>,
    /// The rings this node holds every signed leg of, and so could settle by itself, once the
    /// bundle has gone out to every maker. A proposer's bundle counts only after the dry run: one
    /// the verifier refused rightly never settles.
    pub holding: Vec<String>,
    /// Numbered lines, oldest first.
    pub log: VecDeque<(u64, String)>,
}

/// How a maker declines a want already in another try. A proposer reads it as "busy", not as
/// the ring failing, and does not step aside for it, so the text must stay the same at both ends.
pub(crate) const COMMITTED: &str = "that want is committed to ring ";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WantView {
    pub id: u64,
    /// Raised whenever the want is announced on new terms. Part of the id of every ring through it.
    pub rev: u64,
    pub give: TokenId,
    pub remaining: u128,
    pub take: TokenId,
    pub take_for_remaining: u128,
    /// The ring it is committed to, if any.
    pub locked: Option<String>,
    /// Why it is no longer on offer, if it is not.
    pub closed: Option<Closed>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Closed {
    Filled,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settlement {
    pub at: DateTime<Utc>,
    pub ring: Ring,
    pub tx: Option<String>,
    pub submitter: Option<AccountId>,
    /// This node signed a leg of it and saw its own nonce spent on chain. Otherwise it only heard.
    pub mine: bool,
}

/// A want of this node's own.
struct MyWant {
    id: u64,
    rev: u64,
    give: TokenId,
    take: TokenId,
    /// The price, as first asked: `take_total` for `give_total`.
    give_total: u128,
    take_total: u128,
    remaining: u128,
    locked: Option<String>,
    /// When and why it was closed. A closed want is announced as closed for one more lifetime.
    closed: Option<(Instant, Closed)>,
}

/// A ring this node proposed, until its bundle is out.
struct Proposing {
    ring: Ring,
    deadline_ms: u64,
    /// By maker.
    signatures: BTreeMap<AccountId, SignedIntent>,
    started: Instant,
    /// All signatures are in and the chain is being asked.
    submitting: bool,
}

/// A leg this node signed, until the chain says what became of it.
struct Committed {
    ring: Ring,
    want_id: u64,
    nonce: [u8; 32],
    deadline: DateTime<Utc>,
    /// The signature has left this node: only the chain can now say whether it was used.
    shared: bool,
    /// Every leg, signed: this node can settle the ring itself.
    bundle: Option<Vec<SignedIntent>>,
    /// When to stop waiting for the proposer and submit the bundle.
    submit_at: Option<Instant>,
    /// A chain call about it is in flight.
    busy: bool,
    recheck_at: Instant,
    /// As told by a notice; shown, not trusted.
    told: Option<(String, AccountId)>,
}

/// What a chain task reports back.
enum ChainEvent {
    Refreshed(Result<(VerifierState, Vec<u128>), ChainError>),
    Simulated {
        ring_id: String,
        result: Result<(), ChainError>,
    },
    Executed {
        ring_id: String,
        result: Result<String, ChainError>,
    },
    /// Whether this node's nonce for the ring has been spent.
    Checked {
        ring_id: String,
        used: Result<bool, ChainError>,
    },
}

struct Node {
    /// Anything but honest only in tests, which is also the only place it is read.
    #[cfg_attr(not(test), allow(dead_code))]
    conduct: Conduct,
    name: String,
    key: Keypair,
    me: AccountId,
    tokens: Tokens,
    nonce_kind: NonceKind,
    timings: Timings,
    rpc: String,
    chain: Arc<Verifier>,
    swarm: Swarm<Behaviour>,
    wants_topic: gossipsub::IdentTopic,
    settled_topic: gossipsub::IdentTopic,
    bootstrap: Vec<Multiaddr>,
    listen: Vec<Multiaddr>,
    peers: BTreeSet<PeerId>,

    state: Option<VerifierState>,
    balances: BTreeMap<TokenId, u128>,
    refreshing: bool,
    /// When the chain is next read, and when open wants are next announced.
    refresh_at: Instant,
    announce_at: Instant,

    book: Book,
    mine: BTreeMap<u64, MyWant>,
    next_want_id: u64,
    /// By the wants a ring is made of: when this node first saw it could be formed.
    first_seen: HashMap<RingWants, Instant>,
    /// By the same: when a try at it last fell through, or, while makers may still be committed
    /// to that try, when they will be free. The rest before trying again counts from then. And
    /// whether the ring failed in itself, rather than for want of a free maker: one never answered
    /// or could not be reached, or the verifier refused. Only such a ring steps aside, while it
    /// rests, for the rings it would come before; a ring that met a busy maker is alive, and the
    /// other makers still put it first.
    fell_through: HashMap<RingWants, (Instant, bool)>,
    /// By the same: until when a try of another maker's may hold the signatures of the ring's
    /// makers. Unlike a ring that fell through, such a ring is alive, and still comes first.
    tried: HashMap<RingWants, Instant>,
    proposing: HashMap<String, Proposing>,
    committed: HashMap<String, Committed>,
    outbound: HashMap<OutboundRequestId, String>,
    settlements: Vec<Settlement>,

    log: VecDeque<(u64, String)>,
    log_seq: u64,
    dirty: bool,
    chain_tx: mpsc::UnboundedSender<ChainEvent>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

pub(crate) fn short(account: &str) -> &str {
    &account[..account.len().min(8)]
}

/// Starts a node and returns once it is listening.
pub async fn spawn(config: NodeConfig, timings: Timings) -> Result<Handle> {
    spawn_with(config, timings, Conduct::Honest).await
}

pub async fn spawn_with(config: NodeConfig, timings: Timings, conduct: Conduct) -> Result<Handle> {
    let key = config.keypair()?;
    let tokens = config.tokens();
    let mut swarm = net::build(&key, config.mdns, timings.proposal_timeout)?;
    let wants_topic = gossipsub::IdentTopic::new(net::WANTS_TOPIC);
    let settled_topic = gossipsub::IdentTopic::new(net::SETTLED_TOPIC);
    swarm.behaviour_mut().gossipsub.subscribe(&wants_topic)?;
    swarm.behaviour_mut().gossipsub.subscribe(&settled_topic)?;
    let listen: Multiaddr = config
        .listen
        .parse()
        .with_context(|| format!("listen address `{}`", config.listen))?;
    swarm.listen_on(listen).context("listening")?;
    let bootstrap = config
        .bootstrap
        .iter()
        .map(|addr| addr.parse().with_context(|| format!("bootstrap address `{addr}`")))
        .collect::<Result<Vec<Multiaddr>>>()?;

    let mut opening = Vec::new();
    for want in &config.wants {
        opening.push((tokens.parse(&want.give)?, tokens.parse(&want.take)?));
    }

    let (commands, command_rx) = mpsc::channel(64);
    let (view_tx, view) = watch::channel(View::default());
    let (chain_tx, chain_rx) = mpsc::unbounded_channel();
    let mut node = Node {
        conduct,
        name: config.name.clone(),
        me: key.account_id(),
        key,
        tokens,
        nonce_kind: config.nonce,
        timings,
        chain: Arc::new(Verifier::new(config.rpc.clone(), config.verifier.clone())),
        rpc: config.rpc.clone(),
        swarm,
        wants_topic,
        settled_topic,
        bootstrap,
        listen: Vec::new(),
        peers: BTreeSet::new(),
        state: None,
        balances: BTreeMap::new(),
        refreshing: false,
        refresh_at: Instant::now(),
        announce_at: Instant::now(),
        book: Book::default(),
        mine: BTreeMap::new(),
        next_want_id: 1,
        first_seen: HashMap::new(),
        fell_through: HashMap::new(),
        tried: HashMap::new(),
        proposing: HashMap::new(),
        committed: HashMap::new(),
        outbound: HashMap::new(),
        settlements: Vec::new(),
        log: VecDeque::new(),
        log_seq: 0,
        dirty: true,
        chain_tx,
    };

    // Wait for the first listen address, so that whoever started the node can dial it.
    loop {
        let event = node.swarm.select_next_some().await;
        let listening = matches!(event, SwarmEvent::NewListenAddr { .. });
        node.on_swarm(event);
        if listening {
            break;
        }
    }
    for (give, take) in opening {
        node.open_want(give, take, false);
    }
    node.dial_bootstrap();
    view_tx.send_replace(node.view());
    let task = tokio::spawn(node.run(command_rx, chain_rx, view_tx));
    Ok(Handle { commands, view, task })
}

impl Node {
    async fn run(
        mut self,
        mut commands: mpsc::Receiver<Command>,
        mut chain_rx: mpsc::UnboundedReceiver<ChainEvent>,
        view: watch::Sender<View>,
    ) {
        let mut tick = tokio::time::interval(Duration::from_millis(200));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                event = self.swarm.select_next_some() => self.on_swarm(event),
                command = commands.recv() => match command {
                    Some(Command::Quit) | None => break,
                    Some(command) => self.on_command(command),
                },
                Some(event) = chain_rx.recv() => self.on_chain(event),
                _ = tick.tick() => self.on_tick(),
            }
            if std::mem::take(&mut self.dirty) {
                view.send_replace(self.view());
            }
        }
    }

    fn say(&mut self, line: impl Into<String>) {
        self.log_seq += 1;
        let line = format!("{} {}", chrono::Local::now().format("%H:%M:%S"), line.into());
        self.log.push_back((self.log_seq, line));
        if self.log.len() > LOG_LINES {
            self.log.pop_front();
        }
        self.dirty = true;
    }

    fn view(&self) -> View {
        View {
            name: self.name.clone(),
            account: self.me.clone(),
            peer_id: self.swarm.local_peer_id().to_string(),
            listen: self.listen.iter().map(Multiaddr::to_string).collect(),
            verifier: self.chain.contract().to_owned(),
            rpc: self.rpc.clone(),
            fee_pips: self.state.map(|state| state.fee_pips),
            peers: self
                .peers
                .iter()
                .map(|peer| account_of(peer).unwrap_or_else(|| peer.to_string()))
                .collect(),
            balances: self
                .balances
                .iter()
                .map(|(token, amount)| (token.clone(), *amount))
                .collect(),
            wants: self
                .mine
                .values()
                .map(|want| WantView {
                    id: want.id,
                    rev: want.rev,
                    give: want.give.clone(),
                    remaining: want.remaining,
                    take: want.take.clone(),
                    take_for_remaining: take_for(want.remaining, want.give_total, want.take_total),
                    locked: want.locked.clone(),
                    closed: want.closed.map(|(_, why)| why),
                })
                .collect(),
            book: self.book.open().cloned().collect(),
            settlements: self.settlements.clone(),
            holding: self
                .committed
                .values()
                .filter(|committed| committed.shared && committed.bundle.is_some())
                .map(|committed| committed.ring.id())
                .collect(),
            log: self.log.clone(),
        }
    }

    /// "10 USDC for 99.99 CREDITS", for the log.
    fn show_swap(&self, give: (&str, u128), take: (&str, u128)) -> String {
        format!(
            "{} for {}",
            self.tokens.show(give.0, give.1),
            self.tokens.show(take.0, take.1)
        )
    }

    fn show_ring(&self, ring: &Ring) -> String {
        let legs: Vec<String> = ring
            .legs
            .iter()
            .map(|leg| {
                format!(
                    "{} gives {}",
                    short(&leg.maker),
                    self.tokens.show(&leg.give, leg.give_amount)
                )
            })
            .collect();
        format!("ring {}: {}", ring.id(), legs.join(", "))
    }

    // ----- commands -----

    fn on_command(&mut self, command: Command) {
        match command {
            Command::Want { give, take } => self.open_want(give, take, true),
            Command::Cancel(id) => self.cancel_want(id),
            Command::Quit => {}
        }
    }

    fn open_want(&mut self, give: (TokenId, u128), take: (TokenId, u128), check_balance: bool) {
        if give.0 == take.0 || give.1 == 0 || take.1 == 0 {
            self.say("a want gives some of one token for some of another");
            return;
        }
        // Wants opened from the config are announced before the first balance is read; the chain
        // is the judge of those. One typed in later is checked against what is known to be held.
        let offered: u128 = self
            .mine
            .values()
            .filter(|want| want.closed.is_none() && want.give == give.0)
            .map(|want| want.remaining)
            .sum();
        let held = self.balances.get(&give.0).copied().unwrap_or(0);
        if check_balance && offered.saturating_add(give.1) > held {
            let held = self.tokens.show(&give.0, held);
            self.say(format!(
                "not opened: this node holds {held} in the verifier and has offered the rest"
            ));
            return;
        }
        let id = self.next_want_id;
        self.next_want_id += 1;
        self.say(format!(
            "opened want #{id}: {} or more",
            self.show_swap((&give.0, give.1), (&take.0, take.1))
        ));
        self.mine.insert(
            id,
            MyWant {
                id,
                rev: 0,
                give: give.0,
                take: take.0,
                give_total: give.1,
                take_total: take.1,
                remaining: give.1,
                locked: None,
                closed: None,
            },
        );
        self.announce(id);
    }

    fn cancel_want(&mut self, id: u64) {
        match self.mine.get_mut(&id) {
            None => self.say(format!("no want #{id}")),
            Some(want) if want.closed.is_some() => self.say(format!("want #{id} is already closed")),
            // Its leg is signed and out; cancelling here would not stop it settling.
            Some(want) if want.locked.is_some() => self.say(format!(
                "want #{id} is committed to a ring; it can be cancelled once that settles or expires"
            )),
            Some(want) => {
                want.rev += 1;
                want.remaining = 0;
                want.closed = Some((Instant::now(), Closed::Cancelled));
                self.say(format!("cancelled want #{id}"));
                self.announce(id);
            }
        }
    }

    // ----- wants -----

    /// A want of this node's as the market should see it now.
    fn announcement(&self, want: &MyWant) -> Want {
        Want {
            maker: self.me.clone(),
            id: want.id,
            rev: want.rev,
            give: want.give.clone(),
            give_amount: want.remaining,
            take: want.take.clone(),
            take_amount: take_for(want.remaining, want.give_total, want.take_total),
            expires_ms: now_ms() + self.timings.want_ttl.as_millis() as u64,
            addrs: self.listen.iter().take(MAX_ADDRS).map(Multiaddr::to_string).collect(),
        }
    }

    fn announce(&mut self, id: u64) {
        let Some(want) = self.mine.get(&id) else { return };
        let announcement = self.announcement(want);
        let signed = announcement.sign(&self.key);
        self.book.insert(announcement);
        let message = serde_json::to_vec(&signed).expect("a signed want is plain JSON");
        // With nobody connected yet there is nobody to tell; the next round repeats it.
        let _ = self
            .swarm
            .behaviour_mut()
            .gossipsub
            .publish(self.wants_topic.clone(), message);
        self.dirty = true;
    }

    fn on_want(&mut self, signed: SignedWant) {
        let Ok(want) = signed.open(now_ms()) else { return };
        if want.maker == self.me {
            return;
        }
        let known = self
            .book
            .get(&want.maker, want.id)
            .map(|known| (known.rev, known.is_open()));
        if !self.book.insert(want.clone()) {
            return;
        }
        self.dirty = true;
        match known {
            Some((rev, _)) if rev == want.rev => {} // the same want, announced again
            _ if want.is_open() => self.say(format!(
                "heard want {}#{}: {} or more",
                short(&want.maker),
                want.id,
                self.show_swap((&want.give, want.give_amount), (&want.take, want.take_amount))
            )),
            Some((_, true)) => self.say(format!("want {}#{} closed", short(&want.maker), want.id)),
            _ => {}
        }
        self.reach(&want);
    }

    /// Makes sure the maker of a want can be asked to sign: remembers its addresses and, in a
    /// market small enough, connects to it.
    fn reach(&mut self, want: &Want) {
        let Some(peer) = peer_of(&want.maker) else { return };
        for addr in want.addrs.iter().filter_map(|addr| addr.parse::<Multiaddr>().ok()) {
            self.swarm.add_peer_address(peer, addr);
        }
        if !self.swarm.is_connected(&peer) && self.peers.len() < MAX_PEERS && want.is_open() {
            let _ = self.swarm.dial(net::dial_peer(peer));
        }
    }

    // ----- rings -----

    /// Proposes the rings this node is part of that come first for each of their wants, when it is
    /// its turn.
    fn consider_rings(&mut self) {
        let Some(state) = self.state else { return };
        #[cfg(test)]
        if self.conduct == Conduct::ProposesOthersRings {
            return self.propose_others_rings(state);
        }
        let locked: BTreeSet<u64> = self
            .mine
            .values()
            .filter(|want| want.locked.is_some())
            .map(|want| want.id)
            .collect();
        let candidates = self
            .book
            .open()
            .filter(|want| want.maker != self.me || !locked.contains(&want.id));
        let now = Instant::now();
        // A ring that fell through and is resting does not hold the others back meanwhile: it may
        // never settle, if a maker is gone or cannot pay.
        let resting = |ring: &Ring| {
            self.fell_through
                .get(&ring.wants())
                .is_some_and(|(at, failed)| *failed && now.duration_since(*at) < self.timings.retry_after)
        };
        // A ring this node cannot pay for would only be refused by the verifier. Passed over here,
        // rather than once chosen, it does not hide the rings through the same want it can pay for.
        let unaffordable = |ring: &Ring| {
            ring.leg_of(&self.me)
                .is_some_and(|leg| self.balances.get(&leg.give).copied().unwrap_or(0) < leg.give_amount)
        };
        let rings = first_rings_of(candidates, state.fee_pips, &self.me, |ring| {
            resting(ring) || unaffordable(ring)
        });

        let possible: BTreeSet<_> = rings.iter().map(Ring::wants).collect();
        self.first_seen.retain(|wants, _| possible.contains(wants));
        // One horizon for both: they hold a ring back through the same rest below.
        let forget = self.timings.retry_after * 4;
        self.fell_through.retain(|_, (at, _)| now.duration_since(*at) < forget);
        self.tried.retain(|_, until| now.duration_since(*until) < forget);

        for ring in rings {
            let Some(leg) = ring.leg_of(&self.me) else { continue };
            let want_id = leg.want_id;
            if self.mine.get(&want_id).is_none_or(|want| want.locked.is_some()) {
                continue;
            }
            // The makers take turns, lowest account first, so that usually exactly one proposes.
            let mut makers: Vec<&AccountId> = ring.legs.iter().map(|leg| &leg.maker).collect();
            makers.sort();
            let rank = makers.iter().position(|maker| **maker == self.me).unwrap_or(0) as u32;
            let wants = ring.wants();
            let seen = *self.first_seen.entry(wants.clone()).or_insert(now);
            let my_turn = now.duration_since(seen) >= self.timings.propose_stagger * rank;
            let rest = self.timings.retry_after + self.timings.propose_stagger * rank;
            let rested = |since: Option<&Instant>| since.is_none_or(|at| now.duration_since(*at) >= rest);
            let fell_through = self.fell_through.get(&wants).map(|(at, _)| at);
            if my_turn && rested(fell_through) && rested(self.tried.get(&wants)) {
                self.propose(ring, state);
            }
        }
    }

    /// Signs this node's leg of `ring`, valid until `deadline`. Returns the nonce it spends.
    fn sign_leg(&self, ring: &Ring, state: VerifierState, deadline: DateTime<Utc>) -> Option<(SignedIntent, [u8; 32])> {
        let diff = ring
            .leg_of(&self.me)?
            .diff()?
            .with_memo(format!("gossipmarket:{}", ring.id()));
        let nonce = self.nonce_kind.nonce(state.salt, deadline);
        let signed = self
            .key
            .sign_intents(self.chain.contract(), deadline, &nonce, vec![diff.to_intent()]);
        Some((signed, nonce))
    }

    fn commit(&mut self, ring: &Ring, want_id: u64, nonce: [u8; 32], deadline: DateTime<Utc>, shared: bool) {
        let ring_id = ring.id();
        if let Some(want) = self.mine.get_mut(&want_id) {
            want.locked = Some(ring_id.clone());
        }
        self.committed.insert(
            ring_id,
            Committed {
                ring: ring.clone(),
                want_id,
                nonce,
                deadline,
                shared,
                bundle: None,
                submit_at: None,
                busy: false,
                recheck_at: Instant::now(),
                told: None,
            },
        );
        self.dirty = true;
    }

    #[cfg(test)]
    fn propose_others_rings(&mut self, state: VerifierState) {
        let deadline_ms = now_ms() + self.timings.intent_ttl.as_millis() as u64;
        for ring in market::find_rings(self.book.open(), state.fee_pips) {
            if ring.leg_of(&self.me).is_some() {
                continue;
            }
            let proposal = Proposal {
                ring: ring.clone(),
                fee_pips: state.fee_pips,
                deadline_ms,
            };
            for leg in &ring.legs {
                if let Some(peer) = peer_of(&leg.maker) {
                    let request = Request::Propose(proposal.clone());
                    self.swarm.behaviour_mut().settle.send_request(&peer, request);
                }
            }
        }
    }

    fn propose(&mut self, ring: Ring, state: VerifierState) {
        let ring_id = ring.id();
        let deadline_ms = now_ms() + self.timings.intent_ttl.as_millis() as u64;
        let Some(deadline) = Utc.timestamp_millis_opt(deadline_ms as i64).single() else {
            return;
        };
        let Some(leg) = ring.leg_of(&self.me).cloned() else {
            return;
        };
        let Some((signed, nonce)) = self.sign_leg(&ring, state, deadline) else {
            return;
        };
        self.say(format!("proposing {}", self.show_ring(&ring)));
        // Not shared yet: until the bundle goes out, this signature has not left the node.
        self.commit(&ring, leg.want_id, nonce, deadline, false);

        let proposal = Proposal {
            ring: ring.clone(),
            fee_pips: state.fee_pips,
            deadline_ms,
        };
        for other in &ring.legs {
            if other.maker == self.me {
                continue;
            }
            let Some(peer) = peer_of(&other.maker) else { continue };
            if let Some(want) = self.book.get(&other.maker, other.want_id).cloned() {
                self.reach(&want);
            }
            let request = self
                .swarm
                .behaviour_mut()
                .settle
                .send_request(&peer, Request::Propose(proposal.clone()));
            self.outbound.insert(request, ring_id.clone());
        }
        self.proposing.insert(
            ring_id,
            Proposing {
                ring,
                deadline_ms,
                signatures: BTreeMap::from([(self.me.clone(), signed)]),
                started: Instant::now(),
                submitting: false,
            },
        );
    }

    /// Gives up on a ring this node proposed before its bundle went out. Its own signature never
    /// left, so its want is free again at once.
    fn abandon(&mut self, ring_id: &str, why: &str, failed: bool) {
        let Some(proposing) = self.proposing.remove(ring_id) else {
            return;
        };
        self.outbound.retain(|_, ring| ring != ring_id);
        if let Some(committed) = self.committed.remove(ring_id) {
            self.unlock(committed.want_id);
        }
        // Whoever signed stays committed to this try until its deadline and turns the ring down
        // until then. Tried again sooner, the ring meets those refusals, the makers who sign the
        // new try are locked past the old ones, and they are never all free at once: in a market
        // of a hundred, rings of four went round like that until the end.
        let committed_for = Duration::from_millis(proposing.deadline_ms.saturating_sub(now_ms()));
        self.fell_through
            .insert(proposing.ring.wants(), (Instant::now() + committed_for, failed));
        self.say(format!("ring {ring_id} fell through: {why}"));
    }

    fn unlock(&mut self, want_id: u64) {
        if let Some(want) = self.mine.get_mut(&want_id) {
            want.locked = None;
        }
        self.dirty = true;
    }

    /// A maker's answer to a proposal of this node's.
    fn on_answer(&mut self, ring_id: String, from: PeerId, response: Response) {
        let Some(proposing) = self.proposing.get_mut(&ring_id) else {
            return;
        };
        let signed = match response {
            Response::Signed { signed } => signed,
            Response::Declined { reason } => {
                let busy = reason.starts_with(COMMITTED);
                return self.abandon(&ring_id, &format!("a maker declined: {reason}"), !busy);
            }
            Response::Ack => return,
        };
        let Some(maker) = account_of(&from) else { return };
        let Some(i) = proposing.ring.legs.iter().position(|leg| leg.maker == maker) else {
            return;
        };
        if let Err(why) = proposing
            .ring
            .check_signed(i, &signed, self.chain.contract(), proposing.deadline_ms)
        {
            return self.abandon(
                &ring_id,
                &format!("{} signed something else: {why}", short(&maker)),
                true,
            );
        }
        proposing.signatures.insert(maker, signed);
        if proposing.signatures.len() < proposing.ring.legs.len() || proposing.submitting {
            return;
        }

        // Every leg is in. Ask the verifier for a dry run before anything is spent or shared.
        proposing.submitting = true;
        let bundle: Vec<SignedIntent> = proposing
            .ring
            .legs
            .iter()
            .map(|leg| proposing.signatures[&leg.maker].clone())
            .collect();
        self.say(format!(
            "ring {ring_id}: all {} legs signed, asking the verifier",
            bundle.len()
        ));
        if let Some(committed) = self.committed.get_mut(&ring_id) {
            committed.bundle = Some(bundle.clone());
            committed.busy = true;
        }
        let (chain, tx) = (Arc::clone(&self.chain), self.chain_tx.clone());
        tokio::spawn(async move {
            let result = chain.simulate(&bundle).await;
            let _ = tx.send(ChainEvent::Simulated { ring_id, result });
        });
    }

    /// The dry run of a ring this node proposed came back.
    fn on_simulated(&mut self, ring_id: String, result: Result<(), ChainError>) {
        if let Err(e) = result {
            return self.abandon(&ring_id, &e.to_string(), true);
        }
        #[cfg(test)]
        if self.conduct == Conduct::Withholds {
            return;
        }
        let Some(proposing) = self.proposing.remove(&ring_id) else {
            return;
        };
        let Some(committed) = self.committed.get_mut(&ring_id) else {
            return;
        };
        let Some(bundle) = committed.bundle.clone() else { return };
        // From here the signatures are out of this node's hands: every maker gets the whole
        // bundle and can settle the ring itself if this node goes quiet.
        committed.shared = true;
        for leg in proposing.ring.legs.iter().filter(|leg| leg.maker != self.me) {
            let Some(peer) = peer_of(&leg.maker) else { continue };
            let share = Request::Bundle(Bundle {
                ring_id: ring_id.clone(),
                signed: bundle.clone(),
            });
            self.swarm.behaviour_mut().settle.send_request(&peer, share);
        }
        self.execute(ring_id, bundle);
    }

    fn execute(&mut self, ring_id: String, bundle: Vec<SignedIntent>) {
        #[cfg(test)]
        if self.conduct == Conduct::SharesButNeverSubmits {
            if let Some(committed) = self.committed.get_mut(&ring_id) {
                committed.busy = false;
            }
            return;
        }
        self.say(format!("ring {ring_id}: submitting to {}", self.chain.contract()));
        let (chain, tx, key) = (Arc::clone(&self.chain), self.chain_tx.clone(), self.key.clone());
        tokio::spawn(async move {
            let result = chain.execute(&key, &bundle).await;
            let _ = tx.send(ChainEvent::Executed { ring_id, result });
        });
    }

    fn on_executed(&mut self, ring_id: String, result: Result<String, ChainError>) {
        let Some(committed) = self.committed.get_mut(&ring_id) else {
            return;
        };
        committed.busy = false;
        match result {
            Ok(tx) => {
                let notice = Notice {
                    ring: committed.ring.clone(),
                    tx: tx.clone(),
                    submitter: self.me.clone(),
                };
                let message = serde_json::to_vec(&notice).expect("a notice is plain JSON");
                let _ = self
                    .swarm
                    .behaviour_mut()
                    .gossipsub
                    .publish(self.settled_topic.clone(), message);
                self.resolve(&ring_id, true, Some((tx, self.me.clone())));
            }
            // Someone else's transaction may have settled it, or may yet: the nonce will tell.
            Err(e) => {
                committed.recheck_at = Instant::now();
                self.say(format!("ring {ring_id}: {e}"));
            }
        }
    }

    // ----- being asked -----

    fn on_request(&mut self, from: PeerId, request: Request, channel: ResponseChannel<Response>) {
        #[cfg(test)]
        if self.conduct == Conduct::IgnoresProposals {
            return;
        }
        let response = match request {
            Request::Propose(proposal) => match self.consider_proposal(account_of(&from).as_deref(), &proposal) {
                Ok(signed) => Response::Signed { signed },
                Err(reason) => {
                    let from = account_of(&from).unwrap_or_default();
                    self.say(format!(
                        "declined ring {} from {}: {reason}",
                        proposal.ring.id(),
                        short(&from)
                    ));
                    Response::Declined { reason }
                }
            },
            Request::Bundle(bundle) => {
                self.on_bundle(bundle);
                Response::Ack
            }
        };
        let _ = self.swarm.behaviour_mut().settle.send_response(channel, response);
    }

    /// Signs this node's leg of a ring someone else proposed, if it is what this node offered.
    /// Nothing the proposer says is taken on trust: the ring is checked whole and the leg against
    /// the want as this node last announced it. `from` is the account of the peer that sent it.
    fn consider_proposal(&mut self, from: Option<&str>, proposal: &Proposal) -> Result<SignedIntent, String> {
        let state = self.state.ok_or("not ready: the verifier has not been read yet")?;
        if proposal.fee_pips != state.fee_pips {
            return Err(format!(
                "sized for a fee of {}, the verifier takes {}",
                pips(proposal.fee_pips),
                pips(state.fee_pips)
            ));
        }
        let ring = &proposal.ring;
        if !ring.is_sound(state.fee_pips) {
            return Err("not a ring that nets".into());
        }
        let leg = ring.leg_of(&self.me).ok_or("this node is not in the ring")?.clone();
        // A maker proposes only rings it is in. From anyone else, a proposal would hold this node
        // off the ring below for nothing, and a stranger could keep doing so.
        if !from.is_some_and(|from| from != self.me && ring.leg_of(from).is_some()) {
            return Err("the proposer is not in the ring".into());
        }
        let ahead = proposal.deadline_ms.saturating_sub(now_ms());
        if !(2_000..=MAX_INTENT_TTL.as_millis() as u64).contains(&ahead) {
            return Err("the deadline is too close or too far".into());
        }
        // Whatever this node answers now, the makers who sign this try are held until its
        // deadline. Proposing the ring from here before then would only meet them: the tries of a
        // ring's makers then go round out of step for ever, as they did for a ring of four in a
        // market of a hundred. Only a try every honest maker could sign counts, as above.
        let until = Instant::now() + Duration::from_millis(ahead);
        let tried = self.tried.entry(ring.wants()).or_insert(until);
        *tried = (*tried).max(until);
        let want = self.mine.get(&leg.want_id).ok_or("no such want")?;
        if want.closed.is_some() {
            return Err("that want is closed".into());
        }
        if let Some(ring_id) = &want.locked {
            return Err(format!("{COMMITTED}{ring_id}"));
        }
        leg.is_within(&self.announcement(want))?;
        if self.balances.get(&leg.give).copied().unwrap_or(0) < leg.give_amount {
            return Err("this node does not hold that much".into());
        }
        let deadline = Utc
            .timestamp_millis_opt(proposal.deadline_ms as i64)
            .single()
            .ok_or("bad deadline")?;

        let (signed, nonce) = self.sign_leg(ring, state, deadline).ok_or("could not sign")?;
        self.say(format!(
            "signed my leg: {} (ring {})",
            self.show_swap((&leg.give, leg.give_amount), (&leg.take, leg.take_amount)),
            ring.id()
        ));
        self.commit(ring, leg.want_id, nonce, deadline, true);
        Ok(signed)
    }

    /// The proposer shares every signed leg, so that this node can settle the ring itself.
    fn on_bundle(&mut self, bundle: Bundle) {
        let contract = self.chain.contract().to_owned();
        let me = self.me.clone();
        let stagger = self.timings.submit_stagger;
        let Some(committed) = self.committed.get_mut(&bundle.ring_id) else {
            return;
        };
        if committed.bundle.is_some() {
            return;
        }
        let ring = &committed.ring;
        let deadline_ms = committed.deadline.timestamp_millis() as u64;
        let complete = bundle.signed.len() == ring.legs.len()
            && bundle
                .signed
                .iter()
                .enumerate()
                .all(|(i, signed)| ring.check_signed(i, signed, &contract, deadline_ms).is_ok());
        if !complete {
            return;
        }
        // Makers take turns here too, so that a quiet proposer costs one transaction, not three.
        let mut makers: Vec<&AccountId> = ring.legs.iter().map(|leg| &leg.maker).collect();
        makers.sort();
        let rank = makers.iter().position(|maker| **maker == me).unwrap_or(0) as u32;
        committed.bundle = Some(bundle.signed);
        committed.submit_at = Some(Instant::now() + stagger * (rank + 1));
        self.dirty = true;
    }

    fn on_notice(&mut self, notice: Notice) {
        let ring_id = notice.ring.id();
        if let Some(committed) = self.committed.get_mut(&ring_id) {
            // Told, not shown: this node's own nonce on chain is what it goes by.
            committed.told = Some((notice.tx, notice.submitter));
            committed.recheck_at = Instant::now();
            return;
        }
        let known = self.settlements.iter().any(|settled| settled.ring.id() == ring_id);
        let fee = self.state.map(|state| state.fee_pips);
        if known || !fee.is_some_and(|fee| notice.ring.is_sound(fee)) {
            return;
        }
        self.say(format!(
            "heard that {} settled in tx {}",
            self.show_ring(&notice.ring),
            notice.tx
        ));
        self.settlements.push(Settlement {
            at: Utc::now(),
            ring: notice.ring,
            tx: Some(notice.tx),
            submitter: Some(notice.submitter),
            mine: false,
        });
    }

    // ----- what became of a signed leg -----

    /// Looks after the legs this node has signed: submits a bundle its proposer sat on, and asks
    /// the chain what happened once there is reason to think it knows.
    fn follow_up(&mut self) {
        let now = Instant::now();
        let mut submit = Vec::new();
        let mut check = Vec::new();
        for (ring_id, committed) in &mut self.committed {
            if committed.busy || !committed.shared || now < committed.recheck_at {
                continue;
            }
            let expired = Utc::now() > committed.deadline;
            let overdue = committed.submit_at.is_some_and(|at| now >= at);
            let told = committed.told.is_some();
            if !(expired || overdue || told) {
                continue;
            }
            committed.busy = true;
            committed.recheck_at = now + Duration::from_secs(1);
            if overdue && !expired && !told {
                committed.submit_at = None; // one try; after that the nonce is watched
                submit.push((
                    ring_id.clone(),
                    committed.nonce,
                    committed.bundle.clone().unwrap_or_default(),
                ));
            } else {
                check.push((ring_id.clone(), committed.nonce));
            }
        }
        for (ring_id, nonce) in check {
            let (chain, tx, me) = (Arc::clone(&self.chain), self.chain_tx.clone(), self.me.clone());
            tokio::spawn(async move {
                let used = chain.is_nonce_used(&me, &nonce).await;
                let _ = tx.send(ChainEvent::Checked { ring_id, used });
            });
        }
        for (ring_id, nonce, bundle) in submit {
            self.say(format!(
                "ring {ring_id}: no word from the proposer, submitting it myself"
            ));
            let (chain, tx, key) = (Arc::clone(&self.chain), self.chain_tx.clone(), self.key.clone());
            tokio::spawn(async move {
                // Free to ask, and usually the answer: the proposer's transaction was just slow.
                let event = match chain.is_nonce_used(&key.account_id(), &nonce).await {
                    Ok(false) => ChainEvent::Executed {
                        result: chain.execute(&key, &bundle).await,
                        ring_id,
                    },
                    used => ChainEvent::Checked { ring_id, used },
                };
                let _ = tx.send(event);
            });
        }
    }

    fn on_checked(&mut self, ring_id: String, used: Result<bool, ChainError>) {
        let Some(committed) = self.committed.get_mut(&ring_id) else {
            return;
        };
        committed.busy = false;
        match used {
            Ok(true) => {
                let told = committed.told.clone();
                self.resolve(&ring_id, true, told);
            }
            // Not settled, and it no longer can be.
            Ok(false) if Utc::now() > committed.deadline => self.resolve(&ring_id, false, None),
            // Not settled yet. A notice that proved wrong is forgotten.
            Ok(false) => committed.told = None,
            Err(_) => committed.recheck_at = Instant::now() + Duration::from_secs(3),
        }
    }

    /// The chain has spoken about a leg this node signed.
    fn resolve(&mut self, ring_id: &str, filled: bool, tx: Option<(String, AccountId)>) {
        let Some(committed) = self.committed.remove(ring_id) else {
            return;
        };
        self.proposing.remove(ring_id);
        self.unlock(committed.want_id);
        if !filled {
            self.fell_through
                .insert(committed.ring.wants(), (Instant::now(), false));
            self.say(format!(
                "ring {ring_id} expired unsettled; want #{} is open again",
                committed.want_id
            ));
            return;
        }
        let Some(leg) = committed.ring.leg_of(&self.me).cloned() else {
            return;
        };
        let (tx, submitter) = tx.unzip();
        let through = match (&tx, &submitter) {
            (Some(tx), Some(submitter)) => format!(" · tx {tx} sent by {submitter}"),
            _ => String::new(),
        };
        self.say(format!(
            "SETTLED: gave {}, got {} in a ring of {} ({ring_id}){through}",
            self.tokens.show(&leg.give, leg.give_amount),
            self.tokens.show(&leg.take, leg.take_amount),
            committed.ring.legs.len(),
        ));
        self.settlements.push(Settlement {
            at: Utc::now(),
            ring: committed.ring,
            tx,
            submitter,
            mine: true,
        });
        if let Some(want) = self.mine.get_mut(&leg.want_id) {
            want.remaining = want.remaining.saturating_sub(leg.give_amount);
            want.rev += 1;
            // Nothing left, or so little that its price rounds to nothing.
            if want.remaining == 0 || take_for(want.remaining, want.give_total, want.take_total) == 0 {
                want.remaining = 0;
                want.closed = Some((Instant::now(), Closed::Filled));
            }
            self.announce(leg.want_id);
        }
        self.refresh_at = Instant::now(); // balances changed: read them now
    }

    // ----- the chain -----

    fn on_chain(&mut self, event: ChainEvent) {
        match event {
            ChainEvent::Refreshed(result) => {
                self.refreshing = false;
                match result {
                    Ok((state, balances)) => {
                        if self.state != Some(state) {
                            self.say(format!(
                                "verifier {}: fee {}, salt {}",
                                self.chain.contract(),
                                pips(state.fee_pips),
                                hex::encode(state.salt)
                            ));
                        }
                        self.state = Some(state);
                        let balances: BTreeMap<TokenId, u128> = self.tokens.ids().into_iter().zip(balances).collect();
                        if balances != self.balances {
                            self.balances = balances;
                            self.dirty = true;
                        }
                    }
                    Err(e) if self.state.is_none() => self.say(format!("cannot read the verifier yet: {}", e.reason())),
                    Err(_) => {}
                }
            }
            ChainEvent::Simulated { ring_id, result } => self.on_simulated(ring_id, result),
            ChainEvent::Executed { ring_id, result } => self.on_executed(ring_id, result),
            ChainEvent::Checked { ring_id, used } => self.on_checked(ring_id, used),
        }
    }

    fn refresh(&mut self) {
        self.refreshing = true;
        self.refresh_at = Instant::now() + self.timings.refresh_chain;
        let (chain, tx, me, tokens) = (
            Arc::clone(&self.chain),
            self.chain_tx.clone(),
            self.me.clone(),
            self.tokens.ids(),
        );
        tokio::spawn(async move {
            let result = tokio::try_join!(chain.state(), chain.balances(&me, &tokens));
            let _ = tx.send(ChainEvent::Refreshed(result));
        });
    }

    // ----- time -----

    fn on_tick(&mut self) {
        let now = Instant::now();
        if !self.refreshing && now >= self.refresh_at {
            self.refresh();
        }
        if self.book.expire(now_ms()) {
            self.dirty = true;
        }
        if now >= self.announce_at {
            self.announce_at = now + self.timings.announce_every;
            // Closed wants are announced as closed for one lifetime, then forgotten.
            let ttl = self.timings.want_ttl;
            let live: Vec<u64> = self
                .mine
                .values()
                .filter(|want| want.closed.is_none_or(|(at, _)| now.duration_since(at) < ttl))
                .map(|want| want.id)
                .collect();
            for id in live {
                self.announce(id);
            }
            if self.peers.is_empty() {
                self.dial_bootstrap();
            }
        }
        let late: Vec<String> = self
            .proposing
            .iter()
            .filter(|(_, proposing)| {
                !proposing.submitting && proposing.started.elapsed() >= self.timings.proposal_timeout
            })
            .map(|(ring_id, _)| ring_id.clone())
            .collect();
        for ring_id in late {
            self.abandon(&ring_id, "not every maker answered in time", true);
        }
        self.follow_up();
        self.consider_rings();
    }

    // ----- the network -----

    fn dial_bootstrap(&mut self) {
        for addr in self.bootstrap.clone() {
            let _ = self.swarm.dial(net::dial_addr(addr));
        }
    }

    fn on_swarm(&mut self, event: SwarmEvent<BehaviourEvent>) {
        match event {
            SwarmEvent::NewListenAddr { address, .. } => {
                self.say(format!("listening on {address}"));
                self.listen.push(address);
            }
            SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                if self.peers.insert(peer_id) {
                    let who = account_of(&peer_id).unwrap_or_else(|| peer_id.to_string());
                    self.say(format!("connected to {}", short(&who)));
                    // One walk of the DHT, on joining, finds the peers beyond the first. One per
                    // connection was a walk for every peer each node met: with a hundred nodes,
                    // most of their work went on that.
                    if self.peers.len() == 1 {
                        let _ = self.swarm.behaviour_mut().kad.bootstrap();
                    }
                    // Tell the newcomer what is on offer as soon as it can hear: gossip takes a
                    // moment to learn that a new peer is listening.
                    self.announce_at = self.announce_at.min(Instant::now() + Duration::from_millis(700));
                }
            }
            SwarmEvent::ConnectionClosed {
                peer_id,
                num_established: 0,
                ..
            } => {
                if self.peers.remove(&peer_id) {
                    let who = account_of(&peer_id).unwrap_or_else(|| peer_id.to_string());
                    self.say(format!("lost {}", short(&who)));
                }
            }
            SwarmEvent::Behaviour(event) => self.on_behaviour(event),
            _ => {}
        }
    }

    fn on_behaviour(&mut self, event: BehaviourEvent) {
        match event {
            BehaviourEvent::Gossipsub(gossipsub::Event::Message { message, .. }) => {
                if message.topic == self.wants_topic.hash() {
                    if let Ok(want) = serde_json::from_slice(&message.data) {
                        self.on_want(want);
                    }
                } else if message.topic == self.settled_topic.hash() {
                    if let Ok(notice) = serde_json::from_slice(&message.data) {
                        self.on_notice(notice);
                    }
                }
            }
            BehaviourEvent::Mdns(mdns::Event::Discovered(found)) => {
                for (peer, addr) in found {
                    self.swarm.behaviour_mut().kad.add_address(&peer, addr.clone());
                    self.swarm.add_peer_address(peer, addr);
                    self.meet(peer);
                }
            }
            // What a peer says its addresses are goes into the routing table, which is how a
            // third node learns where to find it.
            BehaviourEvent::Identify(identify::Event::Received { peer_id, info, .. }) => {
                for addr in info.listen_addrs {
                    self.swarm.behaviour_mut().kad.add_address(&peer_id, addr);
                }
            }
            BehaviourEvent::Kad(kad::Event::RoutingUpdated { peer, .. }) => self.meet(peer),
            BehaviourEvent::Settle(request_response::Event::Message { peer, message, .. }) => match message {
                request_response::Message::Request { request, channel, .. } => self.on_request(peer, request, channel),
                request_response::Message::Response { request_id, response } => {
                    if let Some(ring_id) = self.outbound.remove(&request_id) {
                        self.on_answer(ring_id, peer, response);
                    }
                }
            },
            BehaviourEvent::Settle(request_response::Event::OutboundFailure { request_id, error, .. }) => {
                if let Some(ring_id) = self.outbound.remove(&request_id) {
                    self.abandon(&ring_id, &format!("a maker could not be reached: {error}"), true);
                }
            }
            _ => {}
        }
    }

    /// Connects to a peer this node has just learned of.
    fn meet(&mut self, peer: PeerId) {
        if peer != *self.swarm.local_peer_id() && !self.swarm.is_connected(&peer) && self.peers.len() < MAX_PEERS {
            let _ = self.swarm.dial(net::dial_peer(peer));
        }
    }
}
