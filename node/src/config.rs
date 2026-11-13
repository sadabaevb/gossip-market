//! What a node is told at start: its key, where to listen and whom to dial, which verifier to
//! settle through, the tokens it trades and the wants it opens with.
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Utc};
use intents::nonce::{self, Salt};
use intents::{Keypair, TokenId};
use serde::Deserialize;
use sha2::{Digest, Sha256};

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct NodeConfig {
    /// What the node calls itself on screen. Not sent to anyone.
    pub name: String,
    /// `devnet:<name>` for a throwaway key derived from the name, or `file:<path>` to a seed file
    /// written by `gossipmarket keygen`.
    pub key: String,
    /// The multiaddr to listen on. Port 0 picks a free one.
    #[serde(default = "default_listen")]
    pub listen: String,
    /// Multiaddrs of nodes to dial at start. Any node will do: there is no special one.
    #[serde(default)]
    pub bootstrap: Vec<String>,
    /// Find nodes on the local network by mDNS.
    #[serde(default = "yes")]
    pub mdns: bool,
    /// NEAR JSON-RPC: a mainnet node, or the local devnet.
    pub rpc: String,
    /// The verifier contract: `intents.near` on mainnet.
    pub verifier: String,
    /// Which nonce layout to sign.
    #[serde(default)]
    pub nonce: NonceKind,
    pub tokens: BTreeMap<String, TokenConfig>,
    /// Wants to open at start, e.g. `give = "10 USDC"`, `take = "95 CREDITS"`.
    #[serde(default, rename = "want")]
    pub wants: Vec<WantConfig>,
}

#[derive(Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NonceKind {
    /// Carries the contract's salt and a deadline: the layout the contract is moving to.
    #[default]
    Versioned,
    /// 32 random bytes: accepted today, announced to be switched off.
    Plain,
}

impl NonceKind {
    /// A fresh nonce of this kind, for something signed to expire at `deadline`.
    pub fn nonce(self, salt: Salt, deadline: DateTime<Utc>) -> [u8; 32] {
        match self {
            NonceKind::Versioned => nonce::versioned(salt, deadline),
            NonceKind::Plain => nonce::plain(),
        }
    }
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct TokenConfig {
    /// As the verifier names it: `nep141:wrap.near`.
    pub id: TokenId,
    pub decimals: u32,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct WantConfig {
    pub give: String,
    pub take: String,
}

fn default_listen() -> String {
    "/ip4/0.0.0.0/tcp/0".into()
}

fn yes() -> bool {
    true
}

impl NodeConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn keypair(&self) -> Result<Keypair> {
        keypair(&self.key, &self.rpc)
    }

    pub fn tokens(&self) -> Tokens {
        Tokens(self.tokens.clone())
    }
}

/// How long things take. The defaults suit mainnet, where a transaction takes a few seconds; the
/// simulation shortens them.
#[derive(Clone, Copy, Debug)]
pub struct Timings {
    /// How often open wants are announced again.
    pub announce_every: Duration,
    /// How long an announcement lives if not repeated.
    pub want_ttl: Duration,
    /// How long the maker ranked n-th in a ring waits, times n, before proposing it, so that
    /// usually one of them does.
    pub propose_stagger: Duration,
    /// How long a proposer waits for signatures.
    pub proposal_timeout: Duration,
    /// How long a signed leg can be settled, and so how long the want behind it stays locked.
    pub intent_ttl: Duration,
    /// How long a maker holding a complete bundle waits, times its rank, before submitting it
    /// itself.
    pub submit_stagger: Duration,
    /// How long a ring that fell through is left alone.
    pub retry_after: Duration,
    /// How long past its deadline a signed leg is still watched before the want behind it is
    /// reopened. A transaction that beat the deadline can show a little after it: this node's
    /// clock is not the chain's, and what it reads of the chain is a block or two behind.
    pub settle_grace: Duration,
    /// How often balances and the verifier's fee and salt are read again.
    pub refresh_chain: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Self {
            announce_every: Duration::from_secs(5),
            want_ttl: Duration::from_secs(20),
            propose_stagger: Duration::from_secs(2),
            proposal_timeout: Duration::from_secs(8),
            intent_ttl: Duration::from_secs(30),
            submit_stagger: Duration::from_secs(6),
            retry_after: Duration::from_secs(10),
            settle_grace: Duration::from_secs(10),
            refresh_chain: Duration::from_secs(10),
        }
    }
}

impl Timings {
    /// For the simulation and tests: everything on one machine, nothing to wait for.
    pub fn fast() -> Self {
        Self {
            announce_every: Duration::from_millis(700),
            want_ttl: Duration::from_secs(5),
            propose_stagger: Duration::from_millis(400),
            proposal_timeout: Duration::from_secs(3),
            intent_ttl: Duration::from_secs(8),
            submit_stagger: Duration::from_secs(2),
            retry_after: Duration::from_secs(2),
            settle_grace: Duration::from_secs(1),
            refresh_chain: Duration::from_secs(1),
        }
    }
}

/// The key a config names. `rpc` is where the node will settle: throwaway keys are refused
/// anywhere but localhost, since anyone can derive them.
pub fn keypair(key: &str, rpc: &str) -> Result<Keypair> {
    match key.split_once(':') {
        Some(("devnet", name)) => {
            if !is_local(rpc) {
                bail!("`{key}` is a throwaway key that anyone can derive; it only works against a devnet on localhost, not {rpc}");
            }
            Ok(devnet_key(name))
        }
        Some(("file", path)) => read_seed(Path::new(path)),
        _ => bail!("key must be `devnet:<name>` or `file:<path>`, got `{key}`"),
    }
}

/// The devnet key for a name: deterministic, so that a genesis file can fund "alice" before any
/// node has started.
pub fn devnet_key(name: &str) -> Keypair {
    Keypair::from_seed(Sha256::digest(format!("gossipmarket devnet key: {name}")).into())
}

fn read_seed(path: &Path) -> Result<Keypair> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading the key file {}", path.display()))?;
    let seed: [u8; 32] = hex::decode(text.trim())
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| anyhow!("{} does not hold a 32-byte seed in hex", path.display()))?;
    Ok(Keypair::from_seed(seed))
}

fn is_local(rpc: &str) -> bool {
    let host = rpc.split_once("://").map_or(rpc, |(_, rest)| rest);
    let host = host.split(['/', '?', '#']).next().unwrap_or_default();
    // What comes before an `@` is a user name, not the host: `http://localhost:80@example.com`
    // goes to example.com.
    let host = host.rsplit_once('@').map_or(host, |(_, host)| host);
    let host = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => host.rsplit_once(':').map_or(host, |(name, _port)| name),
    };
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

/// "1 pip", "100 pips": the verifier's fee as people read it.
pub fn pips(fee_pips: u32) -> String {
    match fee_pips {
        1 => "1 pip".into(),
        n => format!("{n} pips"),
    }
}

/// The tokens a node knows by symbol, for reading and showing amounts.
#[derive(Clone, Debug, Default)]
pub struct Tokens(BTreeMap<String, TokenConfig>);

impl From<BTreeMap<String, TokenConfig>> for Tokens {
    fn from(table: BTreeMap<String, TokenConfig>) -> Self {
        Self(table)
    }
}

impl Tokens {
    pub fn ids(&self) -> Vec<TokenId> {
        self.0.values().map(|token| token.id.clone()).collect()
    }

    /// The token a symbol stands for.
    pub fn id(&self, symbol: &str) -> Option<TokenId> {
        self.0
            .iter()
            .find(|(known, _)| known.eq_ignore_ascii_case(symbol))
            .map(|(_, token)| token.id.clone())
    }

    /// "10.5 USDC" -> (token id, 10_500_000).
    pub fn parse(&self, text: &str) -> Result<(TokenId, u128)> {
        let (amount, symbol) = text
            .trim()
            .split_once(char::is_whitespace)
            .ok_or_else(|| anyhow!("expected an amount and a token, like `10 USDC`, got `{text}`"))?;
        let symbol = symbol.trim();
        let token = self
            .0
            .iter()
            .find(|(known, _)| known.eq_ignore_ascii_case(symbol))
            .map(|(_, token)| token)
            .ok_or_else(|| {
                anyhow!(
                    "unknown token `{symbol}`; this node knows {}",
                    self.symbols().join(", ")
                )
            })?;
        let units =
            to_units(amount, token.decimals).ok_or_else(|| anyhow!("`{amount}` is not an amount of {symbol}"))?;
        Ok((token.id.clone(), units))
    }

    /// (token id, 10_500_000) -> "10.5 USDC". Tokens this node does not know are shown raw.
    pub fn show(&self, token: &str, units: u128) -> String {
        match self.0.iter().find(|(_, known)| known.id == token) {
            Some((symbol, known)) => format!("{} {symbol}", from_units(units, known.decimals)),
            None => format!("{units} {token}"),
        }
    }

    fn symbols(&self) -> Vec<String> {
        self.0.keys().cloned().collect()
    }
}

/// Decimal text to base units, exactly: no floats near money.
fn to_units(text: &str, decimals: u32) -> Option<u128> {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let (whole, fraction) = match text.split_once('.') {
        Some((whole, fraction)) if digits(fraction) && fraction.len() <= decimals as usize => (whole, fraction),
        Some(_) => return None,
        None => (text, ""),
    };
    if !digits(whole) {
        return None;
    }
    let scale = 10u128.checked_pow(decimals)?;
    let fraction_units = match fraction.is_empty() {
        true => 0,
        false => fraction.parse::<u128>().ok()? * 10u128.pow(decimals - fraction.len() as u32),
    };
    whole
        .parse::<u128>()
        .ok()?
        .checked_mul(scale)?
        .checked_add(fraction_units)
}

fn from_units(units: u128, decimals: u32) -> String {
    let scale = 10u128.pow(decimals);
    let (whole, fraction) = (units / scale, units % scale);
    if fraction == 0 {
        return whole.to_string();
    }
    let fraction = format!("{fraction:0width$}", width = decimals as usize);
    format!("{whole}.{}", fraction.trim_end_matches('0'))
}

/// What the devnet starts with.
#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Genesis {
    pub verifier: String,
    /// The fee on every amount given, in millionths. Mainnet's is read from the contract.
    pub fee_pips: u32,
    pub tokens: BTreeMap<String, TokenConfig>,
    #[serde(default, rename = "account")]
    pub accounts: Vec<GenesisAccount>,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct GenesisAccount {
    /// `devnet:<name>`, or an account id in hex.
    pub key: String,
    /// What it holds inside the verifier, e.g. `["50 USDC"]`.
    pub balances: Vec<String>,
}

impl Genesis {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn ledger(&self) -> Result<intents::Ledger> {
        let tokens = Tokens(self.tokens.clone());
        let mut ledger = intents::Ledger::new(self.verifier.clone(), self.fee_pips);
        for account in &self.accounts {
            let id = match account.key.strip_prefix("devnet:") {
                Some(name) => devnet_key(name).account_id(),
                None => account.key.clone(),
            };
            if intents::nep413::account_key(&id).is_none() {
                bail!(
                    "`{}` is neither `devnet:<name>` nor an implicit account id",
                    account.key
                );
            }
            for balance in &account.balances {
                let (token, units) = tokens.parse(balance)?;
                ledger.deposit(&id, &token, units);
            }
        }
        Ok(ledger)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens() -> Tokens {
        Tokens(BTreeMap::from([
            (
                "USDC".into(),
                TokenConfig {
                    id: "nep141:usdc.near".into(),
                    decimals: 6,
                },
            ),
            (
                "NEAR".into(),
                TokenConfig {
                    id: "nep141:wrap.near".into(),
                    decimals: 24,
                },
            ),
        ]))
    }

    // Amounts are typed by a person and signed by a key. Parsed through a float, "0.1 NEAR" is a
    // few yocto off, and a want for the wrong amount is a trade at the wrong price.
    #[test]
    fn reads_amounts_exactly() {
        let t = tokens();
        assert_eq!(t.parse("10 USDC").unwrap(), ("nep141:usdc.near".into(), 10_000_000));
        assert_eq!(t.parse(" 0.000001 usdc ").unwrap().1, 1);
        assert_eq!(t.parse("0.1 NEAR").unwrap().1, 10u128.pow(23));
        assert_eq!(t.parse("1234.5 NEAR").unwrap().1, 12345 * 10u128.pow(23));
        for wrong in [
            "10",
            "USDC",
            "ten USDC",
            "1.0000001 USDC",
            "-1 USDC",
            "1e6 USDC",
            "1 DOGE",
            ".5 USDC",
            "1. USDC",
        ] {
            assert!(t.parse(wrong).is_err(), "{wrong}");
        }
    }

    #[test]
    fn shows_amounts_as_a_person_would_write_them() {
        let t = tokens();
        assert_eq!(t.show("nep141:usdc.near", 10_000_000), "10 USDC");
        assert_eq!(t.show("nep141:usdc.near", 9_999_000), "9.999 USDC");
        assert_eq!(t.show("nep141:usdc.near", 1), "0.000001 USDC");
        assert_eq!(t.show("nep141:wrap.near", 10u128.pow(23)), "0.1 NEAR");
        assert_eq!(t.show("nep141:other.near", 5), "5 nep141:other.near");
        // And back.
        assert_eq!(
            t.parse(&t.show("nep141:wrap.near", 123_456_789)).unwrap().1,
            123_456_789
        );
    }

    // The configs in demo/ are what gets run in front of people. A typo in one want, and three
    // agents sit and stare at each other while the camera rolls.
    #[test]
    fn the_demo_configs_form_a_ring_of_three() {
        for (dir, fee_pips) in [("demo", 1), ("demo/mainnet", 1), ("demo", 100)] {
            let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join(dir);
            let wants: Vec<market::Want> = ["alice", "bob", "carol"]
                .iter()
                .map(|name| {
                    let config = NodeConfig::load(&root.join(format!("{name}.toml"))).unwrap();
                    assert_eq!(config.name, *name);
                    let [want] = config.wants.as_slice() else {
                        panic!("{dir}/{name}.toml should open one want")
                    };
                    let (give, give_amount) = config.tokens().parse(&want.give).unwrap();
                    let (take, take_amount) = config.tokens().parse(&want.take).unwrap();
                    market::Want {
                        maker: devnet_key(name).account_id(),
                        id: 1,
                        rev: 0,
                        give,
                        give_amount,
                        take,
                        take_amount,
                        expires_ms: u64::MAX,
                        addrs: vec![],
                    }
                })
                .collect();
            let rings = market::find_rings(&wants, fee_pips);
            assert!(
                rings.iter().any(|ring| ring.legs.len() == 3),
                "{dir} at a fee of {}: {rings:?}",
                pips(fee_pips)
            );
            // And no two of them can trade without the third: that is the demo's first moment.
            assert!(market::find_rings(&wants[..2], fee_pips).is_empty(), "{dir}");
        }
        // The devnet starts with what the three wants need.
        let genesis = Genesis::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../demo/devnet.toml")).unwrap();
        let ledger = genesis.ledger().unwrap();
        assert_eq!(
            ledger.balance_of(&devnet_key("alice").account_id(), "nep141:usdc.devnet"),
            50_000_000
        );
    }

    // Anyone can derive a devnet key from its name. Pointed at mainnet, it would be a wallet the
    // whole world holds the key to.
    #[test]
    fn throwaway_keys_work_only_against_localhost() {
        for local in ["http://127.0.0.1:3030", "http://localhost:3030/", "http://[::1]:3030"] {
            assert!(keypair("devnet:alice", local).is_ok(), "{local}");
        }
        for remote in [
            "https://rpc.mainnet.near.org",
            "https://free.rpc.fastnear.com",
            "http://127.0.0.1.evil.example:3030",
            "http://localhost.example",
            "http://localhost:3030@evil.example/",
            "http://127.0.0.1@evil.example:3030",
            "http://evil.example#@localhost",
        ] {
            assert!(keypair("devnet:alice", remote).is_err(), "{remote}");
        }
        assert_eq!(
            keypair("devnet:alice", "http://127.0.0.1:1").unwrap().account_id(),
            devnet_key("alice").account_id()
        );
        assert_ne!(devnet_key("alice").account_id(), devnet_key("bob").account_id());
        assert!(keypair("alice", "http://127.0.0.1:1").is_err());
    }
}
