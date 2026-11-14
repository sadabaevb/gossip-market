//! The network a node lives in: how peers find each other, what they gossip, and what they ask
//! each other directly. Nothing here is specific to one peer: there is no server to run.
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use intents::{AccountId, Keypair, SignedIntent};
use libp2p::swarm::behaviour::toggle::Toggle;
use libp2p::swarm::dial_opts::DialOpts;
use libp2p::swarm::NetworkBehaviour;
use libp2p::{gossipsub, identify, identity, kad, mdns, noise, request_response, tcp, yamux};
use libp2p::{Multiaddr, PeerId, StreamProtocol, Swarm, SwarmBuilder};
use market::Ring;
use serde::{Deserialize, Serialize};

/// Signed wants, announced by their makers and relayed by everyone.
pub const WANTS_TOPIC: &str = "gossipmarket/wants/1";
/// Notices that a ring settled, for everyone's screens. They prove nothing: the chain does.
pub const SETTLED_TOPIC: &str = "gossipmarket/settled/1";

const KAD_PROTOCOL: StreamProtocol = StreamProtocol::new("/gossipmarket/kad/1");
const SETTLE_PROTOCOL: StreamProtocol = StreamProtocol::new("/gossipmarket/settle/1");
const IDENTIFY_PROTOCOL: &str = "/gossipmarket/1";

// libp2p's derive expands to a call that newer compilers report as unreachable. The code is
// theirs: the lint is off for what the derive writes, in a module of its own, and nowhere else.
mod behaviour {
    #![allow(unreachable_code)]
    use super::*;

    #[derive(NetworkBehaviour)]
    pub struct Behaviour {
        pub gossipsub: gossipsub::Behaviour,
        pub mdns: Toggle<mdns::tokio::Behaviour>,
        pub kad: kad::Behaviour<kad::store::MemoryStore>,
        pub identify: identify::Behaviour,
        pub settle: request_response::json::Behaviour<Request, Response>,
    }
}
pub use behaviour::{Behaviour, BehaviourEvent};

/// What one node asks another directly while a ring is being settled.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "ask", rename_all = "snake_case")]
pub enum Request {
    /// "Sign your leg of this ring."
    Propose(Proposal),
    /// "Here is every leg, signed. Submit it yourself if I do not."
    Bundle(Bundle),
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "answer", rename_all = "snake_case")]
pub enum Response {
    Signed { signed: SignedIntent },
    Declined { reason: String },
    Ack,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Proposal {
    pub ring: Ring,
    /// The verifier's fee the ring was sized for. A maker that reads a different fee declines:
    /// the ring would not net.
    pub fee_pips: u32,
    /// Unix milliseconds. Every leg is signed to expire at this moment, so that they all die
    /// together.
    pub deadline_ms: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Bundle {
    pub ring_id: String,
    /// In the order of the ring's legs.
    pub signed: Vec<SignedIntent>,
}

/// Gossiped after a ring settles.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Notice {
    pub ring: Ring,
    /// The transaction, as base58 text.
    pub tx: String,
    /// The account that sent it, which is what a node needs to look the transaction up.
    pub submitter: AccountId,
}

/// The libp2p identity of an agent's key: one key is the peer, the account and the signer.
pub fn identity(key: &Keypair) -> identity::Keypair {
    identity::Keypair::ed25519_from_bytes(key.seed()).expect("32 bytes are an ed25519 seed")
}

/// The peer behind an account id. Proposals go to makers, and a want names its maker by account.
pub fn peer_of(account: &str) -> Option<PeerId> {
    let key = intents::nep413::account_key(account)?;
    let public = identity::ed25519::PublicKey::try_from_bytes(&key).ok()?;
    Some(PeerId::from_public_key(&public.into()))
}

/// The account behind a peer id, when the peer's key is an ed25519 one, as every agent's is.
pub fn account_of(peer: &PeerId) -> Option<AccountId> {
    // An ed25519 peer id is short enough to hold the key itself: a multihash with the identity
    // code, around the key's protobuf encoding.
    let multihash = peer.as_ref();
    if multihash.code() != 0 {
        return None;
    }
    let public = identity::PublicKey::try_decode_protobuf(multihash.digest()).ok()?;
    Some(hex::encode(public.try_into_ed25519().ok()?.to_bytes()))
}

/// A dial to a known peer, from a fresh local port. libp2p dials from the listening port unless
/// told otherwise, and then two nodes that dial each other at the same moment, as two do that
/// have just heard each other's wants, open one TCP connection with a dialer at each end. The
/// handshake fails, and the pair of addresses stays unusable until TIME_WAIT runs out, half a
/// minute on macOS: long enough for every ring between them to fall through. The listening port
/// would only matter for hole punching, which nodes do not do.
pub fn dial_peer(peer: PeerId) -> DialOpts {
    DialOpts::peer_id(peer).allocate_new_port().build()
}

/// The same, for an address whose peer is not known yet: a bootstrap node.
pub fn dial_addr(addr: Multiaddr) -> DialOpts {
    DialOpts::unknown_peer_id().address(addr).allocate_new_port().build()
}

pub fn build(key: &Keypair, mdns_enabled: bool, request_timeout: Duration) -> Result<Swarm<Behaviour>> {
    let swarm = SwarmBuilder::with_existing_identity(identity(key))
        .with_tokio()
        .with_tcp(
            tcp::Config::default().nodelay(true),
            noise::Config::new,
            yamux::Config::default,
        )
        .context("setting up TCP")?
        .with_behaviour(|identity| {
            let peer = identity.public().to_peer_id();

            let gossip = gossipsub::ConfigBuilder::default()
                .heartbeat_interval(Duration::from_secs(1))
                // Signed by the peer that published, and checked by every relay.
                .validation_mode(gossipsub::ValidationMode::Strict)
                .build()
                .map_err(|e| anyhow!("gossipsub config: {e}"))?;
            let gossipsub = gossipsub::Behaviour::new(gossipsub::MessageAuthenticity::Signed(identity.clone()), gossip)
                .map_err(|e| anyhow!("gossipsub: {e}"))?;

            // mDNS needs multicast, which containers and some networks do not have. A node
            // without it still finds peers through the ones it is told to dial.
            let mdns = match mdns_enabled {
                true => mdns::tokio::Behaviour::new(mdns::Config::default(), peer).ok(),
                false => None,
            };

            let mut kad =
                kad::Behaviour::with_config(peer, kad::store::MemoryStore::new(peer), kad::Config::new(KAD_PROTOCOL));
            // Every agent answers lookups; none is only a client.
            kad.set_mode(Some(kad::Mode::Server));

            let identify = identify::Behaviour::new(identify::Config::new(IDENTIFY_PROTOCOL.into(), identity.public()));

            let settle = request_response::json::Behaviour::new(
                [(SETTLE_PROTOCOL, request_response::ProtocolSupport::Full)],
                request_response::Config::default().with_request_timeout(request_timeout),
            );

            Ok(Behaviour {
                gossipsub,
                mdns: mdns.into(),
                kad,
                identify,
                settle,
            })
        })
        .map_err(|e| anyhow!("setting up the network: {e}"))?
        // Agents keep their connections: a market of a few peers is fully connected.
        .with_swarm_config(|config| config.with_idle_connection_timeout(Duration::from_secs(3600)))
        .build();
    Ok(swarm)
}

#[cfg(test)]
mod tests {
    use super::*;

    // One key, three names. If the peer id and the account id ever came apart, a proposal for a
    // maker's want would be sent to someone else.
    #[test]
    fn an_account_and_its_peer_are_the_same_key() {
        let key = Keypair::from_seed([5; 32]);
        let peer = identity(&key).public().to_peer_id();
        assert_eq!(peer_of(&key.account_id()), Some(peer));
        assert_eq!(account_of(&peer), Some(key.account_id()));
        assert_eq!(peer_of("alice.near"), None);
    }

    // Two nodes that hear each other's wants dial each other at once. Dialing from the listening
    // port, such pairs often never connected and every ring between them fell through with
    // "Failed to dial the requested peer": two simulations in five failed that way, and this test
    // within its first two pairs. Timing decides it, so this tries many pairs.
    #[tokio::test]
    async fn two_nodes_that_dial_each_other_at_once_get_connected() {
        use futures::StreamExt;
        use libp2p::swarm::SwarmEvent;

        async fn listening(swarm: &mut Swarm<Behaviour>) -> Multiaddr {
            swarm.listen_on("/ip4/127.0.0.1/tcp/0".parse().unwrap()).unwrap();
            loop {
                if let SwarmEvent::NewListenAddr { address, .. } = swarm.select_next_some().await {
                    return address;
                }
            }
        }

        for pair in 0..12u8 {
            let keys = [
                Keypair::from_seed([100 + 2 * pair; 32]),
                Keypair::from_seed([101 + 2 * pair; 32]),
            ];
            let [mut a, mut b] = keys
                .each_ref()
                .map(|key| build(key, false, Duration::from_secs(5)).unwrap());
            let (at_a, at_b) = (listening(&mut a).await, listening(&mut b).await);
            let (peer_a, peer_b) = (*a.local_peer_id(), *b.local_peer_id());
            a.add_peer_address(peer_b, at_b);
            b.add_peer_address(peer_a, at_a);
            a.dial(dial_peer(peer_b)).unwrap();
            b.dial(dial_peer(peer_a)).unwrap();

            let connected = tokio::time::timeout(Duration::from_secs(5), async {
                while !(a.is_connected(&peer_b) && b.is_connected(&peer_a)) {
                    tokio::select! {
                        _ = a.select_next_some() => {}
                        _ = b.select_next_some() => {}
                    }
                }
            });
            assert!(connected.await.is_ok(), "pair {pair} never connected");
        }
    }
}
