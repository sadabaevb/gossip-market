//! Gossip Market: agents that find each other over libp2p, gossip what they want, and settle
//! N-party trades through the NEAR Intents verifier with no relay, matcher or coordinator between
//! them.
mod config;
mod console;
mod crowd;
mod mcp;
mod net;
mod node;
mod preflight;
mod seat;
mod sim;
#[cfg(test)]
mod tests;
mod tui;
mod verify;
mod web;
mod withdraw;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chain::{ChainError, Devnet, Verifier};
use clap::{Parser, Subcommand};
use intents::Keypair;
use tokio::net::TcpListener;

use config::{Genesis, NodeConfig, Timings};

/// A mainnet RPC node that keeps every transaction, for `verify` to read an old settlement from.
const ARCHIVAL: &str = "https://archival-rpc.mainnet.fastnear.com";

#[derive(Parser)]
#[command(name = "gossipmarket", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Run,
}

#[derive(Subcommand)]
enum Run {
    /// The whole market on this machine in a few seconds: a local verifier, three agents, a
    /// three-party ring, then a trade after the first node is stopped. Needs nothing set up.
    Sim {
        /// Instead, this many agents with overlapping wants, some of whom quit partway. Passes
        /// when the market clears and every transaction checks out.
        #[arg(long)]
        agents: Option<usize>,
        /// Which plan of wants, bootstraps and quits to run with `--agents`.
        #[arg(long, requires = "agents", default_value_t = 1)]
        seed: u64,
        /// Draw the run with `--agents` on a page on localhost, as it happens. The market waits a
        /// few seconds for the page to be opened, and the page stays up while it is open.
        #[arg(long, requires = "agents")]
        web: bool,
        /// Where `--web` serves the page.
        #[arg(long, requires = "web", default_value_t = web::PORT)]
        port: u16,
        /// Write the run with `--agents` where the page reads a recording, e.g. docs/watch/run.js.
        #[arg(long, requires = "agents")]
        record: Option<PathBuf>,
    },
    /// Runs one agent.
    Node {
        /// Its config file, e.g. demo/alice.toml.
        #[arg(long)]
        config: PathBuf,
        /// Log to standard output instead of drawing the screen.
        #[arg(long)]
        plain: bool,
    },
    /// Serves the market to an AI agent as tools, over the Model Context Protocol on standard
    /// input and output. Without a config it is a rehearsal that needs nothing set up: a local
    /// verifier and two other agents whose wants close a ring of three with the agent's.
    Mcp {
        /// The node to trade as, e.g. demo/mainnet/alice.toml. Left out: the rehearsal.
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Serves a page on localhost where a person takes the agent's seat in the rehearsal of `mcp`:
    /// the same tools, behind a form and buttons. Nothing to set up and nothing real at stake.
    Seat {
        #[arg(long, default_value_t = seat::PORT)]
        port: u16,
    },
    /// Runs a local verifier that answers the NEAR JSON-RPC calls agents make, with the balances
    /// in a genesis file. For rehearsing without funds; not a chain.
    Devnet {
        #[arg(long, default_value = "demo/devnet.toml")]
        genesis: PathBuf,
        #[arg(long, default_value_t = 3030)]
        port: u16,
    },
    /// Writes a new key to a file and prints the account it controls.
    Keygen {
        #[arg(long)]
        out: PathBuf,
    },
    /// Prints an agent's account and what it holds inside the verifier.
    Account {
        #[arg(long)]
        config: PathBuf,
    },
    /// Asks the verifier, for free, whether it reads what this build signs: a throwaway key
    /// signs a leg and the contract dry-runs it. Run it against mainnet before funding anything.
    Preflight {
        #[arg(long)]
        config: PathBuf,
    },
    /// Fetches a settlement from the chain and checks it from scratch: every signature, and that
    /// the legs net to zero.
    Verify {
        /// The transaction hash, as a node logged it.
        tx: String,
        /// The account that sent the transaction.
        #[arg(long)]
        sender: String,
        /// Any node's config: it says which RPC and verifier to ask, and how to show amounts.
        #[arg(long)]
        config: PathBuf,
        /// Ask this endpoint instead of the config's. An ordinary RPC node forgets a transaction
        /// after about two days; an archival one, such as https://archival-rpc.mainnet.fastnear.com,
        /// keeps it.
        #[arg(long)]
        rpc: Option<String>,
    },
    /// Takes tokens out of the verifier to a NEAR account: the agent signs an `ft_withdraw`, the
    /// contract dry-runs it for free, and then the agent sends it and pays the gas. Stop the
    /// agent's node first, so that it does not trade the same balance meanwhile.
    Withdraw {
        /// The token's symbol in the config, e.g. USDC.
        token: String,
        /// The account to pay. It must be registered with the token's contract, as any account
        /// that has held the token is.
        #[arg(long)]
        to: String,
        /// How much, e.g. 0.05. Everything the agent holds when left out.
        #[arg(long)]
        amount: Option<String>,
        /// The agent's config: its key, and which RPC and verifier to use.
        #[arg(long)]
        config: PathBuf,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Cli::parse().command).await {
        Ok(()) => ExitCode::SUCCESS,
        // For the operator: what went wrong and why, a line each, and no backtrace.
        Err(e) => {
            eprintln!("error: {e}");
            for cause in e.chain().skip(1) {
                eprintln!("  because: {cause}");
            }
            ExitCode::FAILURE
        }
    }
}

/// The simulation with many agents, drawn on a page or recorded for one if asked.
async fn watched(n: usize, seed: u64, port: Option<u16>, record: Option<PathBuf>) -> Result<()> {
    let board = match port {
        Some(port) => {
            let (board, url) = web::Board::open(port).await?;
            println!("Watch it at {url} . The market starts when the page opens, or in 15 seconds.\n");
            board.wait_for_viewer(Duration::from_secs(15)).await;
            Some(board)
        }
        None => record.is_some().then(web::Board::default),
    };
    let outcome = crowd::run(n, seed, board.as_ref()).await;
    if let (Some(board), Some(path)) = (&board, &record) {
        board.record(path)?;
        println!("Recorded to {}; the page beside it plays it back.", path.display());
    }
    // The end of a run is the part worth looking at. Nobody looking, it ends here.
    if let (Some(board), Some(_)) = (&board, port) {
        if board.watched() {
            println!("The page stays up while it is open. Ctrl-C ends it.");
        }
        board.linger(Duration::from_secs(600)).await;
    }
    outcome
}

async fn run(command: Run) -> Result<()> {
    match command {
        Run::Sim { agents: None, .. } => sim::run().await,
        Run::Sim {
            agents: Some(n),
            seed,
            web,
            port,
            record,
        } => watched(n, seed, web.then_some(port), record).await,
        Run::Node { config: path, plain } => {
            let config = NodeConfig::load(&path)?;
            let tokens = config.tokens();
            let handle = node::spawn(config, Timings::default()).await?;
            if plain || !tui::available() {
                return console::run(handle, tokens).await;
            }
            // The screen takes its contents with it. What settled is worth keeping in the
            // scrollback, with the command that checks each one from scratch.
            let last = tui::run(handle, tokens).await?;
            for settlement in last.settlements.iter().filter(|settlement| settlement.mine) {
                let ring = &settlement.ring;
                println!("settled ring {} between {} parties", ring.id(), ring.legs.len());
                if let (Some(tx), Some(sender)) = (&settlement.tx, &settlement.submitter) {
                    println!(
                        "  gossipmarket verify {tx} --sender {sender} --config {}",
                        path.display()
                    );
                }
            }
            Ok(())
        }
        Run::Mcp { config } => mcp::run(config).await,
        Run::Seat { port } => {
            let url = seat::open(port).await?;
            println!("Take your seat at {url} . Ctrl-C ends it.");
            tokio::signal::ctrl_c().await?;
            Ok(())
        }
        Run::Devnet { genesis, port } => devnet(genesis, port).await,
        Run::Keygen { out } => keygen(out),
        Run::Account { config } => account(config).await,
        Run::Preflight { config } => preflight::run(&NodeConfig::load(&config)?).await,
        Run::Verify {
            tx,
            sender,
            config,
            rpc,
        } => {
            let config = NodeConfig::load(&config)?;
            let asked = rpc.unwrap_or_else(|| config.rpc.clone());
            let verifier = Verifier::new(asked.clone(), config.verifier.clone());
            let checked = match verify::check(&verifier, &tx, &sender).await {
                // A node that has dropped the transaction answers with a timeout, which reads like
                // a network fault: say what else it can be.
                Err(error) if matches!(error.downcast_ref(), Some(ChainError::Unknown(_))) => {
                    return Err(error.context(format!(
                        "{asked} did not answer for this transaction. An ordinary RPC node forgets a transaction \
                         after about two days; an archival one keeps it: --rpc {ARCHIVAL}"
                    )));
                }
                checked => checked?,
            };
            for line in checked.report(&config.tokens()) {
                println!("{line}");
            }
            println!("VERIFIED");
            Ok(())
        }
        Run::Withdraw {
            token,
            to,
            amount,
            config,
        } => {
            let config = NodeConfig::load(&config)?;
            let withdrawn = withdraw::withdraw(&config, &token, amount.as_deref(), &to).await?;
            for line in withdrawn.report(&config.tokens()) {
                println!("{line}");
            }
            Ok(())
        }
    }
}

async fn devnet(genesis: PathBuf, port: u16) -> Result<()> {
    let genesis = Genesis::load(&genesis)?;
    let devnet = Devnet::new(genesis.ledger()?);
    // Loopback only: the devnet hands out gas to anyone who asks.
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .with_context(|| format!("port {port} is taken; is a devnet already running?"))?;
    println!(
        "devnet verifier `{}` on http://{}  (fee {}; balances from the genesis file)",
        genesis.verifier,
        listener.local_addr()?,
        config::pips(genesis.fee_pips)
    );
    println!("This is the verifier's rules over balances in memory, for rehearsing. Stop it and they are gone.");
    let mut events = devnet.events();
    tokio::spawn(async move {
        while let Ok(line) = events.recv().await {
            println!("{line}");
        }
    });
    tokio::select! {
        served = devnet.serve(listener) => served.context("serving"),
        _ = tokio::signal::ctrl_c() => Ok(()),
    }
}

fn keygen(out: PathBuf) -> Result<()> {
    if out.exists() {
        bail!("{} exists; a key is not something to overwrite", out.display());
    }
    if let Some(dir) = out.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let key = Keypair::generate();
    write_private(&out, &hex::encode(key.seed()))?;
    println!("wrote {}  (keep it private: it is the whole account)", out.display());
    println!("account  {}", key.account_id());
    println!("peer id  {}", net::identity(&key).public().to_peer_id());
    println!("In a node's config: key = \"file:{}\"", out.display());
    Ok(())
}

/// Writes a file only its owner can read.
fn write_private(path: &std::path::Path, text: &str) -> Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    writeln!(file, "{text}").context("writing the key")
}

async fn account(config: PathBuf) -> Result<()> {
    let config = NodeConfig::load(&config)?;
    let key = config.keypair()?;
    let tokens = config.tokens();
    println!("name     {}", config.name);
    println!("account  {}", key.account_id());
    println!("peer id  {}", net::identity(&key).public().to_peer_id());
    println!("verifier {} at {}", config.verifier, config.rpc);
    let verifier = Verifier::new(config.rpc.clone(), config.verifier.clone());
    let ids = tokens.ids();
    let balances = verifier.balances(&key.account_id(), &ids).await?;
    println!("inside the verifier:");
    for (token, amount) in ids.iter().zip(balances) {
        println!("  {}", tokens.show(token, amount));
    }
    Ok(())
}
