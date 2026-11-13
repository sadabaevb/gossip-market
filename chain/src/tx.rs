//! A NEAR transaction holding one function call, in borsh: all an agent ever sends to the chain.
//! Encoded by hand, as NearBy's relayer does, to keep the NEAR SDKs out of the build; decoding is
//! for the devnet, which has to read what agents send it.
use intents::Keypair;
use sha2::{Digest, Sha256};

/// The most gas one transaction may use. The call gets all of it and is charged what it burns.
pub const MAX_GAS: u64 = 300_000_000_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionCall {
    pub signer: String,
    pub public_key: [u8; 32],
    /// The access key's nonce: each transaction's must be higher than the last.
    pub nonce: u64,
    pub receiver: String,
    /// A recent block: the transaction is valid for a day after it.
    pub block_hash: [u8; 32],
    pub method: String,
    pub args: Vec<u8>,
    pub gas: u64,
    pub deposit: u128,
}

impl FunctionCall {
    pub fn encode(&self) -> Vec<u8> {
        let mut tx = Vec::new();
        let bytes = |tx: &mut Vec<u8>, b: &[u8]| {
            tx.extend((b.len() as u32).to_le_bytes());
            tx.extend(b);
        };
        bytes(&mut tx, self.signer.as_bytes());
        tx.push(0); // key type: ed25519
        tx.extend(self.public_key);
        tx.extend(self.nonce.to_le_bytes());
        bytes(&mut tx, self.receiver.as_bytes());
        tx.extend(self.block_hash);
        tx.extend(1u32.to_le_bytes()); // one action
        tx.push(2); // action: FunctionCall
        bytes(&mut tx, self.method.as_bytes());
        bytes(&mut tx, &self.args);
        tx.extend(self.gas.to_le_bytes());
        tx.extend(self.deposit.to_le_bytes());
        tx
    }

    /// The signed transaction: the transaction, then a signature over its sha256.
    pub fn sign(&self, key: &Keypair) -> Vec<u8> {
        let mut signed = self.encode();
        let signature = key.sign_bytes(&hash(&signed));
        signed.push(0); // key type: ed25519
        signed.extend(signature);
        signed
    }
}

/// A transaction's hash: sha256 of its encoding without the signature. Base58 of this is the id
/// explorers show.
pub fn hash(transaction: &[u8]) -> [u8; 32] {
    Sha256::digest(transaction).into()
}

/// A signed transaction as the devnet received it.
pub struct Received {
    pub call: FunctionCall,
    pub hash: [u8; 32],
    pub signature: [u8; 64],
}

/// Reads a signed transaction of the one shape [`FunctionCall::sign`] writes. `None` for any other.
pub fn decode_signed(signed: &[u8]) -> Option<Received> {
    let mut rest = signed;
    let call = FunctionCall {
        signer: text(&mut rest)?,
        public_key: {
            expect(&mut rest, &[0])?;
            take(&mut rest)?
        },
        nonce: u64::from_le_bytes(take(&mut rest)?),
        receiver: text(&mut rest)?,
        block_hash: take(&mut rest)?,
        method: {
            expect(&mut rest, &1u32.to_le_bytes())?;
            expect(&mut rest, &[2])?;
            text(&mut rest)?
        },
        args: bytes(&mut rest)?,
        gas: u64::from_le_bytes(take(&mut rest)?),
        deposit: u128::from_le_bytes(take(&mut rest)?),
    };
    let transaction = &signed[..signed.len() - rest.len()];
    expect(&mut rest, &[0])?;
    let signature = take(&mut rest)?;
    rest.is_empty().then(|| Received {
        call,
        hash: hash(transaction),
        signature,
    })
}

fn take<const N: usize>(rest: &mut &[u8]) -> Option<[u8; N]> {
    let (head, tail) = rest.split_at_checked(N)?;
    *rest = tail;
    head.try_into().ok()
}

fn expect(rest: &mut &[u8], prefix: &[u8]) -> Option<()> {
    *rest = rest.strip_prefix(prefix)?;
    Some(())
}

fn bytes(rest: &mut &[u8]) -> Option<Vec<u8>> {
    let length = u32::from_le_bytes(take(rest)?) as usize;
    let (head, tail) = rest.split_at_checked(length)?;
    *rest = tail;
    Some(head.to_vec())
}

fn text(rest: &mut &[u8]) -> Option<String> {
    String::from_utf8(bytes(rest)?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::STANDARD, Engine};

    /// The public key of the vector below. The secret it was signed with is not kept here: the
    /// vector carries the signature, and a signature is checked with the public key alone.
    const VECTOR_KEY: &str = "ed4928c628d1c2c6eae90338905995612959273a5c63f93636c14614ac8737d1";

    fn call(public_key: [u8; 32]) -> FunctionCall {
        FunctionCall {
            signer: "relayer.near".into(),
            public_key,
            nonce: 123456789012345,
            receiver: "intents.near".into(),
            block_hash: [9; 32],
            method: "execute_intents".into(),
            args: br#"{"signed":[{"a":1}]}"#.to_vec(),
            gas: MAX_GAS,
            deposit: 0,
        }
    }

    // The vector NearBy's relayer is pinned to, produced by near-api-js 4.0.4 (createTransaction +
    // functionCall + sign) for the same inputs. A transaction encoded any other way is rejected by
    // every node, and nothing settles.
    #[test]
    fn encodes_a_transaction_as_near_api_js_does_and_its_signature_checks() {
        let call = call(hex::decode(VECTOR_KEY).unwrap().try_into().unwrap());
        let encoded = call.encode();
        assert_eq!(hex::encode(&encoded), "0c00000072656c617965722e6e65617200ed4928c628d1c2c6eae90338905995612959273a5c63f93636c14614ac8737d179df0d86487000000c000000696e74656e74732e6e656172090909090909090909090909090909090909090909090909090909090909090901000000020f000000657865637574655f696e74656e7473140000007b227369676e6564223a5b7b2261223a317d5d7d00c06e31d910010000000000000000000000000000000000");
        // What near-api-js sent for it: those bytes, the kind of signature, the signature.
        let theirs = STANDARD
            .decode("DAAAAHJlbGF5ZXIubmVhcgDtSSjGKNHCxurpAziQWZVhKVknOlxj+TY2wUYUrIc30XnfDYZIcAAADAAAAGludGVudHMubmVhcgkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJAQAAAAIPAAAAZXhlY3V0ZV9pbnRlbnRzFAAAAHsic2lnbmVkIjpbeyJhIjoxfV19AMBuMdkQAQAAAAAAAAAAAAAAAAAAAAAAAPDbVIaWuhrFnP4GMd7vrGQmX32eImYjO3+1Ji2puDe4EIVBvPjKIK8WHWPo+MnBSOHT71Vrdb5k/uk+EajQLAs=")
            .unwrap();
        let received = decode_signed(&theirs).unwrap();
        assert_eq!(received.call, call);
        assert_eq!([encoded.as_slice(), &[0], &received.signature].concat(), theirs);
        // An ed25519 signature is the only one its key makes for a message. Theirs checks against
        // our hash of our encoding, so it is the one this code makes with that key.
        assert_eq!(received.hash, hash(&encoded));
        assert!(intents::nep413::verify_bytes(
            &call.public_key,
            &received.hash,
            &received.signature
        ));
    }

    // The devnet must read exactly what the client writes, or it would accept transactions that
    // mainnet rejects and the rehearsal would prove nothing.
    #[test]
    fn reads_back_what_it_signed() {
        let key = Keypair::from_seed([7; 32]);
        let call = call(key.public_key());
        let signed = call.sign(&key);
        let received = decode_signed(&signed).unwrap();
        assert_eq!(received.call, call);
        // Laid out as near-api-js lays it out in the vector above.
        assert_eq!([call.encode().as_slice(), &[0], &received.signature].concat(), signed);
        assert_eq!(received.hash, hash(&call.encode()));
        assert!(intents::nep413::verify_bytes(
            &call.public_key,
            &received.hash,
            &received.signature
        ));

        assert!(decode_signed(&signed[..signed.len() - 1]).is_none());
        let mut longer = signed.clone();
        longer.push(0);
        assert!(decode_signed(&longer).is_none());
        assert!(decode_signed(&[]).is_none());
    }
}
