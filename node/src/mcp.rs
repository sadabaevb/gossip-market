//! The market as tools for an AI agent, over the Model Context Protocol. The agent's client starts
//! this as a subprocess and speaks JSON-RPC to it on standard input and output, one message a
//! line. Behind the tools is an ordinary node: the agent decides what to want, the node finds the
//! ring and settles it.
//!
//! With no config this is a rehearsal: a devnet verifier and two other agents in this process,
//! whose wants close a ring of three with the right want from the agent. Nothing has to be set up
//! and nothing is spent.
//!
//! Both generations of the protocol are served. A request that names its protocol version in
//! `_meta` is answered on its own, as revision 2026-07-28 asks; a client that opens with
//! `initialize` is answered as the revisions before it did.
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use chain::Verifier;
use intents::{AccountId, Ledger};
use serde_json::{json, Value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::watch;
use tokio::time::sleep;

use crate::config::{devnet_key, pips, NodeConfig, Timings, Tokens, WantConfig};
use crate::node::{self, Closed, Command, Handle, Settlement, View, WantView};
use crate::{sim, verify};

/// The revisions answered request by request, and the ones answered after an `initialize`.
const MODERN: [&str; 1] = ["2026-07-28"];
const LEGACY: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const VERSION: &str = "io.modelcontextprotocol/protocolVersion";
const CLIENT_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";
const SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";

/// JSON-RPC's own error codes, and the one the protocol adds for a version nobody here speaks.
const NOT_JSON: i64 = -32700;
const NOT_A_REQUEST: i64 = -32600;
const NO_SUCH_METHOD: i64 = -32601;
const BAD_PARAMS: i64 = -32602;
const UNSUPPORTED_VERSION: i64 = -32022;

/// The longest `wait_for_settlement` waits. Requests are answered one at a time, so a longer wait
/// would hold up everything else the client asks meanwhile.
const LONGEST_WAIT: u64 = 60;

/// One agent's seat at the market: its node, and what the tools need to describe it.
pub struct Market {
    handle: Handle,
    tokens: Tokens,
    verifier: Verifier,
    /// What an agent is told once, on arriving: where it is and what the tools are for.
    about: String,
    /// The accounts this market can put a name to. In a rehearsal, everyone.
    names: BTreeMap<AccountId, String>,
    /// How many of this node's own settlements `wait_for_settlement` has already told of.
    told: usize,
    /// The other agents of a rehearsal. A node stops when its handle is dropped.
    others: Vec<Handle>,
    /// The node has read the verifier, and in a rehearsal has heard the others' wants.
    ready: bool,
}

/// A seat as somebody beside it sees it: the node's view as it changes, and the names to show
/// amounts and makers by. A tool can keep the seat itself for as long as it waits; a page that
/// draws the market must not wait with it.
#[derive(Clone)]
pub struct Onlooker {
    view: watch::Receiver<View>,
    tokens: Tokens,
    names: BTreeMap<AccountId, String>,
}

impl Onlooker {
    /// What a page draws: amounts written out and makers named, as the tools say them.
    pub fn state(&self) -> Value {
        let view = self.view.borrow().clone();
        let name = |account: &str| match self.names.get(account) {
            Some(name) => name.clone(),
            None => format!("{}…", node::short(account)),
        };
        let wants: Vec<Value> = view
            .wants
            .iter()
            .map(|want| {
                json!({
                    "id": want.id,
                    "give": self.tokens.show(&want.give, want.remaining),
                    "take": self.tokens.show(&want.take, want.take_for_remaining),
                    "standing": standing(want),
                })
            })
            .collect();
        let mut heard = view.book.clone();
        heard.sort_by(|a, b| (&a.maker, a.id).cmp(&(&b.maker, b.id)));
        let book: Vec<Value> = heard
            .iter()
            .map(|want| {
                json!({
                    "who": name(&want.maker),
                    "mine": want.maker == view.account,
                    "id": want.id,
                    "give": self.tokens.show(&want.give, want.give_amount),
                    "take": self.tokens.show(&want.take, want.take_amount),
                })
            })
            .collect();
        let settlements: Vec<Value> = view
            .settlements
            .iter()
            .map(|settled| {
                let legs: Vec<Value> = settled
                    .ring
                    .legs
                    .iter()
                    .map(|leg| {
                        json!({
                            "who": name(&leg.maker),
                            "gave": self.tokens.show(&leg.give, leg.give_amount),
                            "got": self.tokens.show(&leg.take, leg.take_amount),
                        })
                    })
                    .collect();
                json!({ "ring": settled.ring.id(), "mine": settled.mine, "tx": settled.tx, "legs": legs })
            })
            .collect();
        let holding: Vec<String> = view
            .balances
            .iter()
            .map(|(token, units)| self.tokens.show(token, *units))
            .collect();
        json!({
            "me": name(&view.account),
            "account": view.account,
            "verifier": view.verifier,
            "fee": view.fee_pips.map(pips),
            "peers": view.peers.len(),
            "holding": holding,
            "wants": wants,
            "book": book,
            "settlements": settlements,
        })
    }
}

/// Where a want of this node's stands, in a word.
fn standing(want: &WantView) -> &'static str {
    match (&want.closed, &want.locked) {
        (Some(Closed::Filled), _) => "filled",
        (Some(Closed::Cancelled), _) => "cancelled",
        (None, Some(_)) => "committed",
        (None, None) => "open",
    }
}

/// Serves the market on standard input and output until the client closes the input.
pub async fn run(config: Option<PathBuf>) -> Result<()> {
    let mut market = match config {
        Some(path) => Market::live(NodeConfig::load(&path)?).await?,
        None => Market::rehearsal().await?,
    };
    market
        .serve(BufReader::new(tokio::io::stdin()), tokio::io::stdout())
        .await?;
    market.close().await;
    Ok(())
}

impl Market {
    /// A node for the account in `config`, on whatever network the config names.
    pub async fn live(config: NodeConfig) -> Result<Self> {
        let tokens = config.tokens();
        let verifier = Verifier::new(config.rpc.clone(), config.verifier.clone());
        let about = format!(
            "{WHAT_THIS_IS} This node trades for the account in its config, through the verifier {} at {}. \
             If that is mainnet, what it trades is real. {HOW_TO_USE}",
            config.verifier, config.rpc
        );
        let handle = node::spawn(config, Timings::default()).await?;
        Ok(Self {
            handle,
            tokens,
            verifier,
            about,
            names: BTreeMap::new(),
            told: 0,
            others: Vec::new(),
            ready: false,
        })
    }

    /// A market on this machine alone: a devnet verifier, the agent as alice with 50 USDC and no
    /// wants yet, and bob and carol, whose wants close a ring with one of hers. Bob gives credits
    /// for data and Carol data for USDC, so no two of the three can trade.
    pub async fn rehearsal() -> Result<Self> {
        let tokens = Tokens::from(sim::token_table());
        let mut ledger = Ledger::new(sim::VERIFIER, sim::FEE_PIPS);
        let mut names = BTreeMap::new();
        for (name, holds) in [("alice", "50 USDC"), ("bob", "500 CREDITS"), ("carol", "20 DATA")] {
            let account = devnet_key(name).account_id();
            let (token, units) = tokens.parse(holds)?;
            ledger.deposit(&account, &token, units);
            names.insert(account, name.to_owned());
        }
        let (_devnet, rpc) = sim::serve_devnet(ledger).await?;
        let verifier = Verifier::new(rpc.clone(), sim::VERIFIER);

        let handle = node::spawn(sim::config("alice", &rpc, Vec::new(), Vec::new()), Timings::fast()).await?;
        let bootstrap = handle.view.borrow().listen.clone();
        let mut others = Vec::new();
        // Three times what one ring with `10 USDC for 95 CREDITS` takes, so that an agent can
        // trade more than once and see a want fill in parts.
        for (name, give, take) in [("bob", "300 CREDITS", "14.7 DATA"), ("carol", "15 DATA", "29.7 USDC")] {
            let wants = vec![WantConfig {
                give: give.into(),
                take: take.into(),
            }];
            let config = sim::config(name, &rpc, bootstrap.clone(), wants);
            others.push(node::spawn(config, Timings::fast()).await?);
        }
        let about = format!(
            "{WHAT_THIS_IS} This is a rehearsal on this machine: a local verifier with the rules of NEAR Intents, \
             and two other agents, bob and carol, in this process. Nothing here is real money. You are alice and \
             hold 50 USDC. {HOW_TO_USE}"
        );
        Ok(Self {
            handle,
            tokens,
            verifier,
            about,
            names,
            told: 0,
            others,
            ready: false,
        })
    }

    /// The seat for somebody who watches it without holding it.
    pub fn onlooker(&self) -> Onlooker {
        Onlooker {
            view: self.handle.view.clone(),
            tokens: self.tokens.clone(),
            names: self.names.clone(),
        }
    }

    /// Stops the node, and the rehearsal's others with it.
    pub async fn close(self) {
        for handle in self.others.into_iter().chain([self.handle]) {
            handle.commands.send(Command::Quit).await.ok();
            handle.task.await.ok();
        }
    }

    /// Answers every request on `reader`, a line each, until it ends.
    pub async fn serve<R, W>(&mut self, reader: R, mut writer: W) -> Result<()>
    where
        R: AsyncBufRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let mut lines = reader.lines();
        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            let reply = match serde_json::from_str::<Value>(&line) {
                Ok(message) => self.answer(&message).await,
                Err(e) => Some(error(Value::Null, NOT_JSON, &format!("not JSON: {e}"), None)),
            };
            // Nothing but messages may go out on this stream, one a line: serde writes none of
            // its own newlines.
            if let Some(reply) = reply {
                writer.write_all(format!("{reply}\n").as_bytes()).await?;
                writer.flush().await?;
            }
        }
        Ok(())
    }

    /// The answer to one message, or none if it was a notification.
    async fn answer(&mut self, message: &Value) -> Option<Value> {
        let id = message.get("id").cloned();
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            return Some(error(
                id.unwrap_or(Value::Null),
                NOT_A_REQUEST,
                "not a request: no method",
                None,
            ));
        };
        // A notification has no id and is never answered: `notifications/initialized` and
        // `notifications/cancelled` are the ones a client sends.
        let id = id?;
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let meta = &params["_meta"];

        // The era is the request's own to say. One that names its version is the current kind and
        // must name one served here and say what its client can do; one that does not is from a
        // client that shook hands first, or from someone typing by hand.
        let modern = meta.get(VERSION).is_some();
        if modern {
            let requested = meta[VERSION].as_str().unwrap_or_default();
            if !MODERN.contains(&requested) {
                let data = json!({ "supported": MODERN, "requested": requested });
                return Some(error(
                    id,
                    UNSUPPORTED_VERSION,
                    "Unsupported protocol version",
                    Some(data),
                ));
            }
            if meta.get(CLIENT_CAPABILITIES).is_none() {
                return Some(error(
                    id,
                    BAD_PARAMS,
                    &format!("the request's _meta lacks {CLIENT_CAPABILITIES}"),
                    None,
                ));
            }
        }

        let (body, cacheable) = match method {
            "server/discover" => {
                let body = json!({
                    "supportedVersions": MODERN,
                    "capabilities": { "tools": {} },
                    "instructions": self.about,
                });
                // Always in the current shape: only a client of the current kind asks this.
                return Some(result(id, body, true, true));
            }
            "initialize" => {
                let requested = params["protocolVersion"].as_str().unwrap_or_default();
                let version = match LEGACY.contains(&requested) {
                    true => requested,
                    false => LEGACY[0],
                };
                let body = json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": {} },
                    "serverInfo": server_info(),
                    "instructions": self.about,
                });
                (body, false)
            }
            // Gone from the current revision; a client that shook hands may still send it.
            "ping" if !modern => (json!({}), false),
            "tools/list" => (json!({ "tools": tools() }), true),
            "tools/call" => {
                let Some(name) = params["name"].as_str() else {
                    return Some(error(id, BAD_PARAMS, "tools/call names no tool", None));
                };
                if !tools()
                    .as_array()
                    .is_some_and(|all| all.iter().any(|tool| tool["name"] == name))
                {
                    return Some(error(id, BAD_PARAMS, &format!("Unknown tool: {name}"), None));
                }
                // What went wrong inside a tool goes back as its result, so that the agent can
                // read why and try again.
                let (text, failed) = match self.call(name, &params["arguments"]).await {
                    Ok(text) => (text, false),
                    Err(why) => (why, true),
                };
                let body = json!({ "content": [{ "type": "text", "text": text }], "isError": failed });
                (body, false)
            }
            other => return Some(error(id, NO_SUCH_METHOD, &format!("Method not found: {other}"), None)),
        };
        Some(result(id, body, modern, cacheable))
    }

    fn view(&self) -> View {
        self.handle.view.borrow().clone()
    }

    /// A maker as an agent would want it named: by name where one is known, and always by enough
    /// of the account to tell it from the others.
    fn who(&self, account: &str) -> String {
        match self.names.get(account) {
            Some(name) => format!("{name} ({}…)", node::short(account)),
            None => format!("{}…", node::short(account)),
        }
    }

    /// Waits, the first time a tool is called, for the node to have something true to say. A
    /// client can call a tool within a moment of starting this process, before the node has read
    /// what it holds: it then said it held nothing, and turned down a want it could pay for.
    async fn settle_in(&mut self) {
        let limit = Instant::now() + Duration::from_secs(10);
        while !self.ready && Instant::now() < limit {
            let view = self.view();
            // In a rehearsal each of the others announces one want.
            self.ready = view.fee_pips.is_some() && !view.balances.is_empty() && view.book.len() >= self.others.len();
            if !self.ready {
                sleep(Duration::from_millis(50)).await;
            }
        }
        // Past the limit the tools answer with what there is: a verifier that cannot be reached
        // is something the agent should be told, not kept waiting for.
        self.ready = true;
    }

    /// Runs one tool. `Err` is a failure the agent can act on: a token it misspelt, a want the
    /// node turned down.
    pub async fn call(&mut self, name: &str, args: &Value) -> Result<String, String> {
        self.settle_in().await;
        match name {
            "market_status" => Ok(self.status()),
            "list_open_wants" => Ok(self.open_wants()),
            "place_want" => {
                let text = |key: &str| {
                    args[key]
                        .as_str()
                        .ok_or(format!("`{key}` is missing: an amount and a token, like \"10 USDC\""))
                };
                self.place_want(text("give")?, text("take")?).await
            }
            "cancel_want" => {
                let id = args["id"]
                    .as_u64()
                    .ok_or("`id` is missing: the number of the want to cancel")?;
                self.cancel_want(id).await
            }
            "wait_for_settlement" => {
                let seconds = args["seconds"].as_u64().unwrap_or(20).clamp(1, LONGEST_WAIT);
                Ok(self.wait_for_settlement(seconds).await)
            }
            "list_settlements" => Ok(self.settlements()),
            "verify_settlement" => {
                let tx = args["tx"]
                    .as_str()
                    .ok_or("`tx` is missing: the transaction of a settlement")?;
                self.verify(tx).await
            }
            "recent_events" => {
                let lines = args["lines"].as_u64().unwrap_or(20).clamp(1, 100) as usize;
                let log: Vec<String> = self.view().log.into_iter().map(|(_, line)| line).collect();
                Ok(log[log.len().saturating_sub(lines)..].join("\n"))
            }
            other => Err(format!("no such tool: {other}")),
        }
    }

    fn status(&self) -> String {
        let view = self.view();
        let fee = view.fee_pips.map_or("not read yet".to_owned(), |fee| {
            format!("{} of what each maker gives", pips(fee))
        });
        let holding: Vec<String> = view
            .balances
            .iter()
            .map(|(token, units)| self.tokens.show(token, *units))
            .collect();
        let holding = match holding.is_empty() {
            true => "not read yet".to_owned(),
            false => holding.join(", "),
        };
        let mut lines = vec![
            format!("You are {}, account {}.", view.name, view.account),
            format!("Verifier {} at {}, fee {fee}.", view.verifier, view.rpc),
            format!("Connected to {} peers.", view.peers.len()),
            format!("Holding inside the verifier: {holding}."),
        ];
        match view.wants.is_empty() {
            true => lines.push("Your wants: none yet. place_want opens one.".into()),
            false => lines.push("Your wants:".into()),
        }
        for want in &view.wants {
            let state = match &want.locked {
                Some(ring) if want.closed.is_none() => {
                    format!("committed to ring {ring}, waiting for it to settle or expire")
                }
                _ => standing(want).to_owned(),
            };
            lines.push(match want.closed {
                Some(_) => format!("- #{}: {state}", want.id),
                None => format!(
                    "- #{}: give up to {} for at least {}; {state}",
                    want.id,
                    self.tokens.show(&want.give, want.remaining),
                    self.tokens.show(&want.take, want.take_for_remaining)
                ),
            });
        }
        lines.join("\n")
    }

    fn open_wants(&self) -> String {
        let view = self.view();
        let mut book = view.book.clone();
        book.sort_by(|a, b| (&a.maker, a.id).cmp(&(&b.maker, b.id)));
        if book.is_empty() {
            return "No open wants heard yet. Peers announce theirs every few seconds.".into();
        }
        let mut lines = vec![
            "Open wants heard on the market. Each gives up to the first amount and takes at least the second \
             for all of it, or the same price for a part:"
                .to_owned(),
        ];
        for want in &book {
            let whose = match want.maker == view.account {
                true => "you".to_owned(),
                false => self.who(&want.maker),
            };
            lines.push(format!(
                "- {whose} #{}: gives {} for {}",
                want.id,
                self.tokens.show(&want.give, want.give_amount),
                self.tokens.show(&want.take, want.take_amount)
            ));
        }
        lines.push(
            "A trade settles when wants close a ring of two to four makers: each gives what the next one takes.".into(),
        );
        lines.join("\n")
    }

    async fn place_want(&mut self, give: &str, take: &str) -> Result<String, String> {
        let give = self.tokens.parse(give).map_err(|e| format!("give: {e:#}"))?;
        let take = self.tokens.parse(take).map_err(|e| format!("take: {e:#}"))?;
        let before = self.view();
        let known: BTreeSet<u64> = before.wants.iter().map(|want| want.id).collect();
        let read = before.log.back().map_or(0, |(seq, _)| *seq);
        let said = (self.tokens.show(&give.0, give.1), self.tokens.show(&take.0, take.1));
        self.send(Command::Want { give, take }).await?;

        // The node answers in its log: the want it opened, or why it did not.
        let limit = Instant::now() + Duration::from_secs(3);
        while Instant::now() < limit {
            let view = self.view();
            if let Some(want) = view.wants.iter().find(|want| !known.contains(&want.id)) {
                return Ok(format!(
                    "Opened want #{}: give up to {} for at least {}. It is announced to the market and settles, \
                     in whole or in part, when it closes a ring with other wants. wait_for_settlement tells you \
                     when; nothing is spent until then.",
                    want.id, said.0, said.1
                ));
            }
            let refusal = view
                .log
                .iter()
                .filter(|(seq, _)| *seq > read)
                .map(|(_, line)| strip_time(line))
                .find(|line| line.starts_with("not opened") || line.starts_with("a want gives"));
            if let Some(why) = refusal {
                return Err(why.to_owned());
            }
            sleep(Duration::from_millis(50)).await;
        }
        Err("the node did not answer; recent_events shows what it is doing".into())
    }

    async fn cancel_want(&mut self, id: u64) -> Result<String, String> {
        let read = self.view().log.back().map_or(0, |(seq, _)| *seq);
        self.send(Command::Cancel(id)).await?;
        let limit = Instant::now() + Duration::from_secs(3);
        while Instant::now() < limit {
            let view = self.view();
            let said = view
                .log
                .iter()
                .filter(|(seq, _)| *seq > read)
                .map(|(_, line)| strip_time(line))
                .find(|line| line.contains(&format!("want #{id}")));
            match said {
                Some(line) if line.starts_with("cancelled") => return Ok(line.to_owned()),
                Some(line) => return Err(line.to_owned()),
                None => sleep(Duration::from_millis(50)).await,
            }
        }
        Err("the node did not answer; recent_events shows what it is doing".into())
    }

    async fn send(&self, command: Command) -> Result<(), String> {
        self.handle
            .commands
            .send(command)
            .await
            .map_err(|_| "the node has stopped".to_owned())
    }

    /// Waits for a settlement of this node's that has not been told of yet.
    async fn wait_for_settlement(&mut self, seconds: u64) -> String {
        let limit = Instant::now() + Duration::from_secs(seconds);
        let held = self.view().balances;
        let mut waited = false;
        loop {
            let view = self.view();
            let mine: Vec<&Settlement> = view.settlements.iter().filter(|settled| settled.mine).collect();
            if mine.len() > self.told {
                // The node reads its balances again a moment after a ring settles. An agent that
                // asked what it held in that moment was told what it held before the trade.
                let fresh = Instant::now() + Duration::from_secs(2);
                while waited && self.view().balances == held && Instant::now() < fresh {
                    sleep(Duration::from_millis(50)).await;
                }
                let mut lines: Vec<String> = mine[self.told..]
                    .iter()
                    .map(|settled| self.describe(settled, &view.account))
                    .collect();
                self.told = mine.len();
                lines.push(
                    "The node knows this from its own nonce on the verifier, not from a peer. verify_settlement \
                     reads the transaction back and checks every signature and the netting from scratch."
                        .into(),
                );
                return lines.join("\n");
            }
            if Instant::now() >= limit {
                return format!(
                    "Nothing of yours settled in {seconds} s.\n{}\nA want settles only when it closes a ring at a \
                     price every maker in it accepts: list_open_wants shows what the others ask, and \
                     recent_events what this node has tried.",
                    self.status()
                );
            }
            waited = true;
            sleep(Duration::from_millis(100)).await;
        }
    }

    /// One settlement, from where `me` stands.
    fn describe(&self, settled: &Settlement, me: &str) -> String {
        let ring = &settled.ring;
        let with: Vec<String> = ring
            .legs
            .iter()
            .filter(|leg| leg.maker != me)
            .map(|leg| self.who(&leg.maker))
            .collect();
        let through = match (&settled.tx, &settled.submitter) {
            (Some(tx), Some(submitter)) => format!(" Transaction {tx}, sent by {}.", self.who(submitter)),
            _ => String::new(),
        };
        match ring.leg_of(me).filter(|_| settled.mine) {
            Some(leg) => format!(
                "SETTLED ring {} of {} makers, with {}: you gave {} and got {}.{through}",
                ring.id(),
                ring.legs.len(),
                with.join(" and "),
                self.tokens.show(&leg.give, leg.give_amount),
                self.tokens.show(&leg.take, leg.take_amount)
            ),
            None => format!(
                "Heard, not checked: ring {} of {} makers settled between {}.{through}",
                ring.id(),
                ring.legs.len(),
                with.join(" and ")
            ),
        }
    }

    fn settlements(&self) -> String {
        let view = self.view();
        if view.settlements.is_empty() {
            return "No settlements yet.".into();
        }
        let lines: Vec<String> = view
            .settlements
            .iter()
            .map(|settled| self.describe(settled, &view.account))
            .collect();
        lines.join("\n")
    }

    async fn verify(&self, tx: &str) -> Result<String, String> {
        let view = self.view();
        // The verifier is asked for the transaction by its sender's account as well as its hash.
        let sender = view
            .settlements
            .iter()
            .find(|settled| settled.tx.as_deref() == Some(tx))
            .and_then(|settled| settled.submitter.clone())
            .ok_or("this node knows no settlement in that transaction; list_settlements has the ones it does")?;
        let checked = verify::check(&self.verifier, tx, &sender)
            .await
            .map_err(|e| format!("NOT VERIFIED: {e:#}"))?;
        let mut lines = checked.report(&self.tokens);
        lines.push("VERIFIED".into());
        Ok(lines.join("\n"))
    }
}

const WHAT_THIS_IS: &str = "Gossip Market: agents announce what they want over a peer-to-peer network and settle \
     rings of two to four makers through the NEAR Intents verifier, in one transaction, with no relay, matcher or \
     coordinator between them.";

const HOW_TO_USE: &str = "Start with market_status and list_open_wants. place_want announces what you would give \
     and take; it settles only when it closes a ring in which each maker gives what the next one takes, at a price \
     every one of them accepts. wait_for_settlement tells you when that happens, and verify_settlement checks the \
     transaction against the verifier, so that you take nobody's word for it.";

fn server_info() -> Value {
    json!({ "name": "gossipmarket", "title": "Gossip Market", "version": env!("CARGO_PKG_VERSION") })
}

/// The tools, always in this order: a client may cache the list.
fn tools() -> Value {
    let no_arguments = json!({ "type": "object", "properties": {}, "additionalProperties": false });
    let tool = |name: &str, description: &str, schema: Value| json!({ "name": name, "description": description, "inputSchema": schema });
    json!([
        tool(
            "market_status",
            "Who this agent is on the market, what it holds inside the verifier, the state of its own wants, and how \
             many peers it is connected to. Start here, and come back to see what a trade changed.",
            no_arguments.clone()
        ),
        tool(
            "list_open_wants",
            "Every open want heard on the market: who gives how much of what, and what they take for it. Read it to \
             find a want of your own that would close a ring of two to four makers.",
            no_arguments.clone()
        ),
        tool(
            "place_want",
            "Announce a want: give up to an amount of one token for at least an amount of another. It settles, in \
             whole or in part, when it closes a ring with other open wants, and never at a worse price than asked. \
             Nothing is spent until then. Returns the want's number.",
            json!({
                "type": "object",
                "properties": {
                    "give": {
                        "type": "string",
                        "description": "The most to give: an amount and a token, like \"10 USDC\"."
                    },
                    "take": {
                        "type": "string",
                        "description": "The least to take for all of it: an amount and a token, like \"95 CREDITS\"."
                    }
                },
                "required": ["give", "take"],
                "additionalProperties": false
            })
        ),
        tool(
            "cancel_want",
            "Withdraw one of your wants by its number. A want already committed to a ring cannot be cancelled until \
             that ring settles or expires.",
            json!({
                "type": "object",
                "properties": {
                    "id": { "type": "integer", "minimum": 1, "description": "The want's number, as place_want gave it." }
                },
                "required": ["id"],
                "additionalProperties": false
            })
        ),
        tool(
            "wait_for_settlement",
            "Wait until a ring this agent is in settles on the verifier, or the time runs out. Says what was given \
             and received, with whom, and in which transaction. Call it after place_want.",
            json!({
                "type": "object",
                "properties": {
                    "seconds": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": LONGEST_WAIT,
                        "description": "How long to wait at most. 20 if left out."
                    }
                },
                "additionalProperties": false
            })
        ),
        tool(
            "list_settlements",
            "The rings this agent settled, and the ones it only heard of from peers.",
            no_arguments
        ),
        tool(
            "verify_settlement",
            "Read a settlement's transaction back from the chain and check it from scratch: every maker's \
             signature, and that every token nets to zero after the verifier's fee. Ends with VERIFIED.",
            json!({
                "type": "object",
                "properties": {
                    "tx": { "type": "string", "description": "The transaction hash, as a settlement gives it." }
                },
                "required": ["tx"],
                "additionalProperties": false
            })
        ),
        tool(
            "recent_events",
            "The last lines of this node's log: wants heard, rings proposed, legs signed, refusals and settlements. \
             For finding out why nothing has settled.",
            json!({
                "type": "object",
                "properties": {
                    "lines": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 100,
                        "description": "How many lines. 20 if left out."
                    }
                },
                "additionalProperties": false
            })
        ),
    ])
}

/// A result as the request's era expects it. The current revision marks every result as complete,
/// names the server in it, and says how long a list may be kept; a client of an earlier one was
/// promised none of that.
fn result(id: Value, mut body: Value, modern: bool, cacheable: bool) -> Value {
    if modern {
        body["resultType"] = json!("complete");
        body["_meta"] = json!({ SERVER_INFO: server_info() });
        if cacheable {
            // The tools are the same for every client and for as long as the process runs.
            body["ttlMs"] = json!(3_600_000);
            body["cacheScope"] = json!("public");
        }
    }
    json!({ "jsonrpc": "2.0", "id": id, "result": body })
}

fn error(id: Value, code: i64, message: &str, data: Option<Value>) -> Value {
    let mut error = json!({ "code": code, "message": message });
    if let Some(data) = data {
        error["data"] = data;
    }
    json!({ "jsonrpc": "2.0", "id": id, "error": error })
}

/// A log line without the time of day it starts with.
fn strip_time(line: &str) -> &str {
    line.split_once(' ').map_or(line, |(_, rest)| rest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    /// A request of the current revision: its version and its client's capabilities travel with it.
    fn modern(id: u64, method: &str, mut params: Value) -> Value {
        params["_meta"] = json!({ VERSION: MODERN[0], CLIENT_CAPABILITIES: {} });
        json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
    }

    fn legacy(id: u64, method: &str, params: Value) -> Value {
        json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
    }

    async fn ask(market: &mut Market, message: Value) -> Value {
        market.answer(&message).await.expect("a request is answered")
    }

    /// Calls a tool as a client of the current revision and returns its text and whether it failed.
    async fn tool(market: &mut Market, name: &str, arguments: Value) -> (String, bool) {
        let request = modern(7, "tools/call", json!({ "name": name, "arguments": arguments }));
        let reply = ask(market, request).await;
        let result = &reply["result"];
        assert_eq!(result["resultType"], "complete", "{reply}");
        let text = result["content"][0]["text"].as_str().unwrap_or_default().to_owned();
        (text, result["isError"] == true)
    }

    // A client of the current revision shakes no hands: it may ask anything first, and each
    // request says which revision it speaks. Answered as the old kind, it would wait for a
    // handshake that never comes, or be given results it must reject for lacking their type.
    #[tokio::test]
    async fn a_client_of_the_current_revision_is_answered_request_by_request() {
        let mut market = Market::rehearsal().await.unwrap();

        let found = ask(&mut market, modern(1, "server/discover", json!({}))).await;
        assert_eq!(found["result"]["resultType"], "complete", "{found}");
        assert_eq!(found["result"]["supportedVersions"], json!(MODERN));
        assert_eq!(found["result"]["_meta"][SERVER_INFO]["name"], "gossipmarket");
        assert!(found["result"]["instructions"].as_str().unwrap().contains("rehearsal"));

        let listed = ask(&mut market, modern(2, "tools/list", json!({}))).await;
        let result = &listed["result"];
        assert_eq!(result["resultType"], "complete");
        assert_eq!(result["cacheScope"], "public");
        assert!(result["ttlMs"].as_u64().is_some());
        let names: Vec<&str> = result["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert_eq!(names.len(), 8);
        assert!(names.contains(&"place_want") && names.contains(&"verify_settlement"));

        // A revision nobody here speaks is turned down with the ones that are, so that the client
        // can pick one and ask again.
        let mut too_new = modern(3, "tools/list", json!({}));
        too_new["params"]["_meta"][VERSION] = json!("2099-01-01");
        let refused = ask(&mut market, too_new).await;
        assert_eq!(refused["error"]["code"], UNSUPPORTED_VERSION);
        assert_eq!(refused["error"]["data"]["supported"], json!(MODERN));

        let mut incomplete = modern(4, "tools/list", json!({}));
        incomplete["params"]["_meta"] = json!({ VERSION: MODERN[0] });
        assert_eq!(ask(&mut market, incomplete).await["error"]["code"], BAD_PARAMS);
        market.close().await;
    }

    // Most clients in use still open with `initialize`. Turned away, they have nothing to fall
    // forward to, and the agent behind them simply has no market.
    #[tokio::test]
    async fn a_client_that_shakes_hands_first_is_answered_as_before() {
        let mut market = Market::rehearsal().await.unwrap();

        let hello = legacy(
            1,
            "initialize",
            json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "test", "version": "0" } }),
        );
        let welcomed = ask(&mut market, hello).await;
        assert_eq!(welcomed["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(welcomed["result"]["serverInfo"]["name"], "gossipmarket");
        assert!(welcomed["result"]["capabilities"]["tools"].is_object());
        // A revision it never heard of gets the newest of the kind it asked in.
        let newer = legacy(2, "initialize", json!({ "protocolVersion": "2025-12-31" }));
        assert_eq!(ask(&mut market, newer).await["result"]["protocolVersion"], LEGACY[0]);

        let ready = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        assert!(market.answer(&ready).await.is_none(), "a notification is not answered");

        let listed = ask(&mut market, legacy(3, "tools/list", json!({}))).await;
        assert_eq!(listed["result"]["tools"].as_array().unwrap().len(), 8);
        assert!(listed["result"].get("resultType").is_none(), "{listed}");
        assert_eq!(
            ask(&mut market, legacy(4, "ping", Value::Null)).await["result"],
            json!({})
        );

        let unknown = ask(&mut market, legacy(5, "resources/list", json!({}))).await;
        assert_eq!(unknown["error"]["code"], NO_SUCH_METHOD);
        let no_tool = legacy(6, "tools/call", json!({ "name": "sell_everything", "arguments": {} }));
        assert_eq!(ask(&mut market, no_tool).await["error"]["code"], BAD_PARAMS);
        market.close().await;
    }

    // The point of the whole server: an agent that knows nothing but the tools reads the market,
    // works out the one want that closes a ring of three, and checks the settlement itself. If
    // any tool lied or hung, an agent would trade blind or not at all.
    #[tokio::test]
    async fn an_agent_closes_a_ring_of_three_with_nothing_but_the_tools() {
        let mut market = Market::rehearsal().await.unwrap();

        // Asked at once, as a client does: the first answer already has what the node holds and
        // the wants Bob and Carol gossiped, a moment after they started.
        let (book, _) = tool(&mut market, "list_open_wants", json!({})).await;
        assert!(book.contains("bob") && book.contains("carol"), "{book}");
        assert!(book.contains("gives 300 CREDITS for 14.7 DATA"), "{book}");
        let (status, _) = tool(&mut market, "market_status", json!({})).await;
        assert!(
            status.contains("You are alice") && status.contains("50 USDC"),
            "{status}"
        );

        // A token the market does not have is the agent's to fix, so it comes back as the tool's
        // result, with the tokens there are.
        let (why, failed) = tool(
            &mut market,
            "place_want",
            json!({ "give": "10 DOGE", "take": "95 CREDITS" }),
        )
        .await;
        assert!(failed && why.contains("unknown token") && why.contains("USDC"), "{why}");
        // So is offering more than it holds.
        let (why, failed) = tool(
            &mut market,
            "place_want",
            json!({ "give": "60 USDC", "take": "500 CREDITS" }),
        )
        .await;
        assert!(failed && why.contains("not opened"), "{why}");

        let (opened, failed) = tool(
            &mut market,
            "place_want",
            json!({ "give": "10 USDC", "take": "95 CREDITS" }),
        )
        .await;
        assert!(!failed && opened.starts_with("Opened want #1"), "{opened}");

        let (settled, _) = tool(&mut market, "wait_for_settlement", json!({ "seconds": 30 })).await;
        assert!(settled.starts_with("SETTLED ring"), "{settled}");
        assert!(
            settled.contains("of 3 makers") && settled.contains("you gave 10 USDC"),
            "{settled}"
        );
        assert!(settled.contains("bob") && settled.contains("carol"), "{settled}");

        let tx = market.view().settlements[0]
            .tx
            .clone()
            .expect("a settlement of its own has its transaction");
        let (verified, failed) = tool(&mut market, "verify_settlement", json!({ "tx": tx })).await;
        assert!(!failed && verified.ends_with("VERIFIED"), "{verified}");
        assert_eq!(verified.matches("(signature valid)").count(), 3, "{verified}");

        // Told once: the next wait is for the next ring, and there is none.
        let (nothing, _) = tool(&mut market, "wait_for_settlement", json!({ "seconds": 1 })).await;
        assert!(nothing.starts_with("Nothing of yours settled"), "{nothing}");
        let (status, _) = tool(&mut market, "market_status", json!({})).await;
        assert!(status.contains("40 USDC") && status.contains("#1: filled"), "{status}");
        market.close().await;
    }

    // The client reads standard output a line at a time and takes each line for a message. One
    // message over two lines, or a line that is not a message, and it drops the connection.
    #[tokio::test]
    async fn every_reply_is_one_line_of_json_and_a_notification_gets_none() {
        let mut market = Market::rehearsal().await.unwrap();
        let (mut client, server) = duplex(64 * 1024);
        let (from_client, to_client) = tokio::io::split(server);

        let requests = [
            modern(1, "server/discover", json!({})).to_string(),
            json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": 1 } }).to_string(),
            "this is not json".to_owned(),
            modern(2, "tools/call", json!({ "name": "market_status", "arguments": {} })).to_string(),
        ];
        client
            .write_all(format!("{}\n", requests.join("\n")).as_bytes())
            .await
            .unwrap();
        client.shutdown().await.unwrap();
        market.serve(BufReader::new(from_client), to_client).await.unwrap();

        let mut written = String::new();
        tokio::io::AsyncReadExt::read_to_string(&mut client, &mut written)
            .await
            .unwrap();
        let replies: Vec<Value> = written
            .lines()
            .map(|line| serde_json::from_str(line).expect("each line is a message"))
            .collect();
        assert_eq!(replies.len(), 3, "{written}");
        assert_eq!(replies[0]["id"], 1);
        assert_eq!(replies[1]["error"]["code"], NOT_JSON);
        // A status has several lines; they travel inside one.
        assert!(replies[2]["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains('\n'));
        market.close().await;
    }
}
