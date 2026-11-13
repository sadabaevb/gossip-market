//! A want: "I give up to this much of one token for at least that much of another". Signed by the
//! maker's key and gossiped; anyone can check it without asking anyone.
use intents::nep413::{account_key, verify_bytes};
use intents::{AccountId, Keypair, TokenId};
use primitive_types::U256;
use serde::{Deserialize, Serialize};

/// What a want's signature covers, in front of its JSON text. The same key signs intents and NEAR
/// transactions, both as 32-byte hashes; a want is longer than that and starts with this, so it
/// can never be mistaken for either.
const DOMAIN: &[u8] = b"gossipmarket want v1:";

/// How far ahead a want may expire. Makers re-announce open wants every few seconds, so a long
/// life only keeps the wants of agents that have left in everybody's book.
pub const MAX_TTL_MS: u64 = 5 * 60 * 1000;

const MAX_BODY_BYTES: usize = 2048;
const MAX_ADDRS: usize = 8;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Want {
    /// The maker's implicit account inside the verifier: the hex of the key that signed this.
    pub maker: AccountId,
    /// The maker's own number for this want.
    pub id: u64,
    /// Goes up whenever the maker changes the want: a partial fill, a cancellation.
    pub rev: u64,
    pub give: TokenId,
    /// The most the maker gives, in base units. Zero closes the want.
    #[serde(with = "as_text")]
    pub give_amount: u128,
    pub take: TokenId,
    /// The least the maker takes for all of `give_amount`; less is given for proportionally less.
    #[serde(with = "as_text")]
    pub take_amount: u128,
    /// Unix milliseconds. Makers re-announce before this.
    pub expires_ms: u64,
    /// Multiaddrs the maker can be dialed at, so that a node which only heard of it by gossip can
    /// send it a proposal.
    #[serde(default)]
    pub addrs: Vec<String>,
}

impl Want {
    pub fn is_open(&self) -> bool {
        self.give_amount > 0
    }

    /// Whether this announcement supersedes `other` for the same maker and id.
    pub fn is_newer_than(&self, other: &Want) -> bool {
        (self.rev, self.expires_ms) > (other.rev, other.expires_ms)
    }

    pub fn sign(&self, key: &Keypair) -> SignedWant {
        let body = serde_json::to_string(self).expect("a want is plain JSON");
        let signature = key.sign_bytes(&[DOMAIN, body.as_bytes()].concat());
        SignedWant {
            body,
            signature: bs58::encode(signature).into_string(),
        }
    }
}

/// The least a maker takes for `remaining` of a want that offered `give_total` for `take_total`:
/// the same price, rounded up in the maker's favour. What a partly filled want is re-announced at.
pub fn take_for(remaining: u128, give_total: u128, take_total: u128) -> u128 {
    if give_total == 0 {
        return 0;
    }
    let exact = U256::from(remaining) * U256::from(take_total);
    let rounded_up = (exact + U256::from(give_total - 1)) / U256::from(give_total);
    u128::try_from(rounded_up).unwrap_or(u128::MAX)
}

/// A want as it travels: its JSON text, kept exactly as signed, and the maker's signature.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SignedWant {
    pub body: String,
    pub signature: String,
}

impl SignedWant {
    /// The want, if the maker it names signed it and it could be acted on at `now_ms`. The error
    /// is why not.
    pub fn open(&self, now_ms: u64) -> Result<Want, &'static str> {
        if self.body.len() > MAX_BODY_BYTES {
            return Err("too large");
        }
        let want: Want = serde_json::from_str(&self.body).map_err(|_| "malformed")?;
        let key = account_key(&want.maker).ok_or("maker is not an implicit account")?;
        let signature: [u8; 64] = bs58::decode(&self.signature)
            .into_vec()
            .ok()
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or("malformed signature")?;
        if !verify_bytes(&key, &[DOMAIN, self.body.as_bytes()].concat(), &signature) {
            return Err("invalid signature");
        }
        if want.expires_ms <= now_ms {
            return Err("expired");
        }
        if want.expires_ms > now_ms + MAX_TTL_MS {
            return Err("expires too far ahead");
        }
        if want.give == want.take {
            return Err("gives and takes the same token");
        }
        // A want that takes nothing is a gift, and one division by zero for every node.
        if want.take_amount == 0 && want.is_open() {
            return Err("takes nothing");
        }
        if want.addrs.len() > MAX_ADDRS {
            return Err("too many addresses");
        }
        Ok(want)
    }
}

/// Amounts as decimal text, the way the verifier writes them: JSON numbers stop at 2^53.
pub(crate) mod as_text {
    use serde::{de::Error, Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(amount: &u128, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(amount)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u128, D::Error> {
        String::deserialize(d)?.parse().map_err(D::Error::custom)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub const NOW: u64 = 1_800_000_000_000;

    pub fn key(seed: u8) -> Keypair {
        Keypair::from_seed([seed; 32])
    }

    pub fn want(seed: u8, give: (&str, u128), take: (&str, u128)) -> Want {
        Want {
            maker: key(seed).account_id(),
            id: 1,
            rev: 0,
            give: give.0.into(),
            give_amount: give.1,
            take: take.0.into(),
            take_amount: take.1,
            expires_ms: NOW + 20_000,
            addrs: vec!["/ip4/127.0.0.1/tcp/4001".into()],
        }
    }

    #[test]
    fn a_signed_want_opens_to_what_the_maker_announced() {
        let announced = want(
            1,
            ("nep141:usdc.near", 10_000_000),
            ("nep141:wrap.near", 4 * 10u128.pow(24)),
        );
        let signed = announced.sign(&key(1));
        assert_eq!(signed.open(NOW), Ok(announced));
        // Amounts travel as text: yoctoNEAR does not fit in a JSON number.
        assert!(signed.body.contains(r#""take_amount":"4000000000000000000000000""#));
    }

    // Wants are free to send and every node stores them, so each of these is either a way to trade
    // in someone else's name or a way to fill everybody's book with junk.
    #[test]
    fn refuses_wants_nobody_could_act_on() {
        let announced = want(1, ("nep141:a.near", 10), ("nep141:b.near", 20));
        let signed = announced.sign(&key(1));

        let mut cheaper = signed.clone();
        cheaper.body = cheaper.body.replace(r#""take_amount":"20""#, r#""take_amount":"2""#);
        assert_eq!(cheaper.open(NOW), Err("invalid signature"));

        // Signed by a key other than the maker's.
        assert_eq!(announced.sign(&key(2)).open(NOW), Err("invalid signature"));

        assert_eq!(signed.open(announced.expires_ms), Err("expired"));
        assert_eq!(signed.open(NOW - MAX_TTL_MS), Err("expires too far ahead"));

        let reject = |change: fn(&mut Want)| {
            let mut changed = announced.clone();
            change(&mut changed);
            changed.sign(&key(1)).open(NOW).unwrap_err()
        };
        assert_eq!(reject(|w| w.take = w.give.clone()), "gives and takes the same token");
        assert_eq!(reject(|w| w.take_amount = 0), "takes nothing");
        assert_eq!(
            reject(|w| w.maker = "alice.near".into()),
            "maker is not an implicit account"
        );
        assert_eq!(
            reject(|w| w.addrs = vec!["/ip4/1.1.1.1/tcp/1".into(); 9]),
            "too many addresses"
        );
        assert_eq!(reject(|w| w.addrs = vec!["x".repeat(3000)]), "too large");
    }

    // After a partial fill the rest is offered at the price first asked, never a cheaper one:
    // rounding down here would let a ring of small fills walk the price away from the maker.
    #[test]
    fn the_rest_of_a_want_keeps_its_price() {
        assert_eq!(take_for(10, 10, 20), 20);
        assert_eq!(take_for(5, 10, 20), 10);
        assert_eq!(take_for(1, 3, 10), 4);
        assert_eq!(take_for(0, 10, 20), 0);
        let near = 10u128.pow(24);
        assert_eq!(take_for(3 * near, 5 * near, 10 * near), 6 * near);
    }

    // A closed want is how a maker tells the market to forget it; it has to get through.
    #[test]
    fn a_closed_want_is_still_a_valid_announcement() {
        let mut closed = want(1, ("nep141:a.near", 0), ("nep141:b.near", 0));
        closed.rev = 3;
        assert_eq!(closed.sign(&key(1)).open(NOW).map(|w| w.is_open()), Ok(false));
    }
}
