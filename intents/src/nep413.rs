//! Signing and opening the messages the verifier executes: NEP-413 envelopes around a JSON list of
//! intents. One ed25519 key is the whole identity of an agent: its implicit account inside the
//! verifier is the hex of the public key, and the same key is its libp2p peer id.
use base64::{engine::general_purpose::STANDARD, Engine};
use chrono::{DateTime, SecondsFormat, Utc};
use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{AccountId, Refusal};

/// 2^31 + 413: what NEP-413 puts in front of a payload so that it can never be a transaction.
const NEP413_TAG: u32 = (1 << 31) + 413;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Payload {
    /// JSON text of a [`Message`]. Signed as text, so it is kept exactly as written.
    pub message: String,
    /// Base64 of 32 bytes.
    pub nonce: String,
    /// The verifier's account: a signature for one verifier settles nothing on another.
    pub recipient: String,
}

/// Exactly the fields the verifier hashes and checks.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SignedIntent {
    pub standard: String,
    pub payload: Payload,
    /// `ed25519:<base58>`
    pub public_key: String,
    /// `ed25519:<base58>`
    pub signature: String,
}

/// What a signature covers, as JSON text.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Message {
    pub signer_id: AccountId,
    /// RFC 3339, UTC.
    pub deadline: String,
    #[serde(default)]
    pub intents: Vec<Value>,
}

/// A signed message that proved to be well-formed and signed by the key it names.
#[derive(Debug, Clone, PartialEq)]
pub struct Opened {
    pub signer_id: AccountId,
    pub public_key: [u8; 32],
    pub recipient: String,
    pub nonce: [u8; 32],
    pub deadline: DateTime<Utc>,
    pub intents: Vec<Value>,
    /// What was signed: the verifier's name for this message.
    pub hash: [u8; 32],
}

/// sha256(borsh(tag) || borsh({message, nonce[32], recipient, callback_url: None}))
pub fn nep413_hash(message: &str, nonce: &[u8; 32], recipient: &str) -> [u8; 32] {
    let mut h = Sha256::new();
    let text = |h: &mut Sha256, s: &str| {
        h.update((s.len() as u32).to_le_bytes());
        h.update(s);
    };
    h.update(NEP413_TAG.to_le_bytes());
    text(&mut h, message);
    h.update(nonce);
    text(&mut h, recipient);
    h.update([0u8]);
    h.finalize().into()
}

/// A key or signature the way NEAR writes it.
pub fn tag(bytes: &[u8]) -> String {
    format!("ed25519:{}", bs58::encode(bytes).into_string())
}

/// The bytes of an `ed25519:<base58>` key or signature, if it is one of the right length.
pub fn untag<const N: usize>(tagged: &str) -> Option<[u8; N]> {
    let bytes = bs58::decode(tagged.strip_prefix("ed25519:")?).into_vec().ok()?;
    bytes.try_into().ok()
}

/// Implicit account id: the hex of the ed25519 public key.
pub fn implicit_account(public_key: &[u8; 32]) -> AccountId {
    hex::encode(public_key)
}

/// The key behind an implicit account id.
pub fn account_key(account: &str) -> Option<[u8; 32]> {
    let key: [u8; 32] = hex::decode(account).ok()?.try_into().ok()?;
    // Only the lowercase spelling is an account id.
    (implicit_account(&key) == account).then_some(key)
}

/// An agent's key.
#[derive(Clone)]
pub struct Keypair(SigningKey);

impl Keypair {
    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self(SigningKey::from_bytes(&seed))
    }

    pub fn generate() -> Self {
        Self::from_seed(rand::random())
    }

    pub fn seed(&self) -> [u8; 32] {
        self.0.to_bytes()
    }

    pub fn public_key(&self) -> [u8; 32] {
        self.0.verifying_key().to_bytes()
    }

    pub fn account_id(&self) -> AccountId {
        implicit_account(&self.public_key())
    }

    /// A raw signature, for messages that are not intents (wants, NEAR transactions).
    pub fn sign_bytes(&self, bytes: &[u8]) -> [u8; 64] {
        self.0.sign(bytes).to_bytes()
    }

    /// Signs `intents` for `verifier`, to be settled before `deadline` under `nonce`.
    pub fn sign_intents(
        &self,
        verifier: &str,
        deadline: DateTime<Utc>,
        nonce: &[u8; 32],
        intents: Vec<Value>,
    ) -> SignedIntent {
        let message = Message {
            signer_id: self.account_id(),
            deadline: deadline.to_rfc3339_opts(SecondsFormat::Millis, true),
            intents,
        };
        self.sign_message(
            verifier,
            serde_json::to_string(&message).expect("a message is plain JSON"),
            nonce,
        )
    }

    /// Signs message text as it stands.
    pub fn sign_message(&self, verifier: &str, message: String, nonce: &[u8; 32]) -> SignedIntent {
        let signature = self.0.sign(&nep413_hash(&message, nonce, verifier));
        SignedIntent {
            standard: "nep413".into(),
            payload: Payload {
                message,
                nonce: STANDARD.encode(nonce),
                recipient: verifier.into(),
            },
            public_key: tag(&self.public_key()),
            signature: tag(&signature.to_bytes()),
        }
    }
}

/// Checks a raw signature made by [`Keypair::sign_bytes`].
pub fn verify_bytes(public_key: &[u8; 32], bytes: &[u8], signature: &[u8; 64]) -> bool {
    VerifyingKey::from_bytes(public_key).is_ok_and(|key| {
        key.verify(bytes, &ed25519_dalek::Signature::from_bytes(signature))
            .is_ok()
    })
}

/// Checks the signature and reads the message, in the order the verifier does, with its reasons.
/// What depends on the verifier's state (deadline, nonce, balances) is the ledger's to check.
pub fn open(signed: &SignedIntent) -> Result<Opened, Refusal> {
    let invalid_signature = || Refusal::new("invalid signature");
    // Agents sign NEP-413 only; the verifier's other envelopes are for other wallets.
    if signed.standard != "nep413" {
        return Err(Refusal::new(format!(
            "JSON: unsupported standard `{}`",
            signed.standard
        )));
    }
    let malformed = |what: &str| Refusal::new(format!("JSON: malformed {what}"));
    let public_key: [u8; 32] = untag(&signed.public_key).ok_or_else(|| malformed("public_key"))?;
    let signature: [u8; 64] = untag(&signed.signature).ok_or_else(|| malformed("signature"))?;
    let nonce: [u8; 32] = STANDARD
        .decode(&signed.payload.nonce)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| malformed("nonce"))?;

    let hash = nep413_hash(&signed.payload.message, &nonce, &signed.payload.recipient);
    if !verify_bytes(&public_key, &hash, &signature) {
        return Err(invalid_signature());
    }

    let message: Message =
        serde_json::from_str(&signed.payload.message).map_err(|e| Refusal::new(format!("JSON: {e}")))?;
    let deadline = DateTime::parse_from_rfc3339(&message.deadline)
        .map_err(|e| Refusal::new(format!("JSON: deadline: {e}")))?
        .with_timezone(&Utc);
    Ok(Opened {
        signer_id: message.signer_id,
        public_key,
        recipient: signed.payload.recipient.clone(),
        nonce,
        deadline,
        intents: message.intents,
        hash,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{nonce, TokenDiff};
    use chrono::TimeZone;

    fn alice() -> Keypair {
        Keypair::from_seed([7; 32])
    }

    // NearBy's voucher vector (testdata/voucher-vector.json in that project), which
    // pins that project's Rust and Dart signers to each other and settles on mainnet. Ed25519 is
    // deterministic, so the same key, message, nonce and recipient must give the same signature.
    // If this drifts, every intent an agent signs is "invalid signature" on chain.
    #[test]
    fn signs_the_same_bytes_as_nearby_vouchers() {
        let key = Keypair::from_seed([1; 32]);
        let nonce: [u8; 32] = hex::decode("252812b307070707070707070707070707070707070707070707070707070707")
            .unwrap()
            .try_into()
            .unwrap();
        let message = r#"{"signer_id":"8a88e3dd7409f195fd52db2d3cba5d72ca6709bf1d94121bf3748801b40f6f5c","deadline":"2026-11-12T10:00:00.000Z","intents":[{"intent":"transfer","receiver_id":"merchant.near","tokens":{"nep141:usdc.near":"2500000"},"memo":"seq:17 prev:00"}]}"#;
        let signed = key.sign_message("intents.near", message.into(), &nonce);

        assert_eq!(
            key.account_id(),
            "8a88e3dd7409f195fd52db2d3cba5d72ca6709bf1d94121bf3748801b40f6f5c"
        );
        assert_eq!(signed.payload.nonce, "JSgSswcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=");
        assert_eq!(
            signed.public_key,
            "ed25519:AKnL4NNf3DGWZJS6cPknBuEGnVsV4A4m5tgebLHaRSZ9"
        );
        assert_eq!(
            signed.signature,
            "ed25519:2g8zV63t6jrWsMzLmorMcXUh1XHohBcsauzA1nz8MXWPaB1dRXNSHh7XMV4MNLYpzPRVhPLaN9as2KgSvWd1UxbT"
        );
        assert_eq!(open(&signed).unwrap().signer_id, key.account_id());
    }

    #[test]
    fn a_signed_intent_opens_to_what_was_signed() {
        let deadline = Utc.with_ymd_and_hms(2026, 11, 14, 12, 0, 0).unwrap();
        let nonce = nonce::plain();
        let diff = TokenDiff::swap(("nep141:a.near", 5), ("nep141:b.near", 7)).unwrap();
        let signed = alice().sign_intents("intents.near", deadline, &nonce, vec![diff.to_intent()]);

        assert_eq!(signed.standard, "nep413");
        let opened = open(&signed).unwrap();
        assert_eq!(opened.signer_id, alice().account_id());
        assert_eq!(opened.public_key, alice().public_key());
        assert_eq!(opened.recipient, "intents.near");
        assert_eq!(opened.nonce, nonce);
        assert_eq!(opened.deadline, deadline);
        assert_eq!(TokenDiff::from_intent(&opened.intents[0]), Some(diff));
    }

    // Each of these is a way to make an agent pay for something it did not sign.
    #[test]
    fn refuses_anything_the_signature_does_not_cover() {
        let deadline = Utc.with_ymd_and_hms(2026, 11, 14, 12, 0, 0).unwrap();
        let diff = TokenDiff::swap(("nep141:a.near", 5), ("nep141:b.near", 7)).unwrap();
        let signed = alice().sign_intents("intents.near", deadline, &nonce::plain(), vec![diff.to_intent()]);
        let invalid = Err(Refusal::new("invalid signature"));

        let mut more = signed.clone();
        more.payload.message = more.payload.message.replace("\"-5\"", "\"-500\"");
        assert_eq!(open(&more), invalid);

        let mut elsewhere = signed.clone();
        elsewhere.payload.recipient = "intents.devnet".into();
        assert_eq!(open(&elsewhere), invalid);

        let mut replayed = signed.clone();
        replayed.payload.nonce = STANDARD.encode(nonce::plain());
        assert_eq!(open(&replayed), invalid);

        let mut by_someone_else = signed.clone();
        by_someone_else.public_key = tag(&Keypair::from_seed([8; 32]).public_key());
        assert_eq!(open(&by_someone_else), invalid);
    }

    #[test]
    fn an_account_id_is_the_lowercase_hex_of_a_key() {
        let account = alice().account_id();
        assert_eq!(account_key(&account), Some(alice().public_key()));
        assert_eq!(account_key(&account.to_uppercase()), None);
        assert_eq!(account_key("alice.near"), None);
    }
}
