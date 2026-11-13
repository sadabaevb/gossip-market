//! The verifier spends one 32-byte nonce per signed message. It reads two layouts.
//!
//! A *versioned* nonce starts with a magic prefix and carries the contract's current salt and a
//! deadline of its own; the contract checks both. Any other 32 bytes are a *plain* nonce with no
//! conditions. The contract's source says plain nonces "will be prohibited in the near future",
//! so agents, which are online and can read the salt, sign versioned ones by default.
use chrono::{DateTime, Utc};

/// First four bytes of every versioned nonce.
pub const VERSIONED_MAGIC: [u8; 4] = [0x56, 0x28, 0xf6, 0xc6];

/// The contract's salt: four bytes it can rotate to cancel every outstanding versioned nonce.
pub type Salt = [u8; 4];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nonce {
    Plain,
    /// `magic(4) | version = 0 (1) | salt(4) | deadline, i64 nanoseconds little-endian (8) | random(15)`
    V1 {
        salt: Salt,
        deadline_ns: i64,
    },
}

/// How the verifier reads these 32 bytes. A magic prefix followed by a version it does not know
/// is not an error there: it falls back to a plain nonce.
pub fn read(nonce: &[u8; 32]) -> Nonce {
    match nonce.strip_prefix(&VERSIONED_MAGIC) {
        Some([0, rest @ ..]) => Nonce::V1 {
            salt: rest[..4].try_into().expect("four bytes"),
            deadline_ns: i64::from_le_bytes(rest[4..12].try_into().expect("eight bytes")),
        },
        _ => Nonce::Plain,
    }
}

/// A versioned nonce valid until `deadline`. The message it goes into must not outlive it: the
/// contract refuses a message whose own deadline is later than its nonce's.
pub fn versioned(salt: Salt, deadline: DateTime<Utc>) -> [u8; 32] {
    let mut nonce = [0u8; 32];
    nonce[..4].copy_from_slice(&VERSIONED_MAGIC);
    nonce[5..9].copy_from_slice(&salt);
    nonce[9..17].copy_from_slice(&nanos(deadline).to_le_bytes());
    nonce[17..].copy_from_slice(&rand::random::<[u8; 15]>());
    nonce
}

/// 32 random bytes that the verifier will not mistake for a versioned nonce.
pub fn plain() -> [u8; 32] {
    loop {
        let nonce: [u8; 32] = rand::random();
        if !nonce.starts_with(&VERSIONED_MAGIC) {
            return nonce;
        }
    }
}

/// Nanoseconds since the epoch, as the contract counts time. Saturates in the year 2262.
pub fn nanos(at: DateTime<Utc>) -> i64 {
    at.timestamp_nanos_opt().unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    // Bytes laid out as the contract's borsh reads them: `(magic, VersionedNonce::V1(SaltedNonce {
    // salt, nonce: ExpirableNonce { deadline, nonce } }))`. One byte off and the contract sees a
    // plain nonce, which settles today and stops settling the day plain nonces are switched off.
    #[test]
    fn lays_a_versioned_nonce_out_as_the_contract_reads_it() {
        let deadline = Utc.with_ymd_and_hms(2026, 11, 14, 12, 0, 0).unwrap();
        let nonce = versioned([0x25, 0x28, 0x12, 0xb3], deadline);
        assert_eq!(hex::encode(&nonce[..17]), "5628f6c600252812b30080742594e7e718");
        assert_eq!(
            read(&nonce),
            Nonce::V1 {
                salt: [0x25, 0x28, 0x12, 0xb3],
                deadline_ns: 1_794_657_600_000_000_000
            }
        );
        // Fifteen random bytes: two nonces for the same deadline differ.
        assert_ne!(nonce, versioned([0x25, 0x28, 0x12, 0xb3], deadline));
    }

    #[test]
    fn anything_else_is_a_plain_nonce() {
        assert_eq!(read(&plain()), Nonce::Plain);
        assert_eq!(read(&[0; 32]), Nonce::Plain);
        // The magic prefix with a version the contract does not know.
        let mut unknown = versioned([1, 2, 3, 4], Utc::now());
        unknown[4] = 1;
        assert_eq!(read(&unknown), Nonce::Plain);
    }
}
