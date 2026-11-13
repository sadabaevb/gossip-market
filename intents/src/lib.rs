//! What a Gossip Market agent signs, and the rules the NEAR Intents verifier settles it by.
//!
//! No network and no async here: `nep413` signs and opens intents, `diff` holds `token_diff` and
//! the fee arithmetic, `withdraw` the `ft_withdraw` that takes tokens out, `nonce` the two nonce
//! layouts, and `ledger` the verifier's rules over balances kept in memory. The `conformance`
//! crate runs the same bundles through the verifier's own engine, so a difference between
//! `ledger` and the contract is a failing test.
pub mod diff;
pub mod ledger;
pub mod nep413;
pub mod nonce;
pub mod withdraw;

pub use diff::{fee_ceil, net_of_fee, unmatched, TokenDiff};
pub use ledger::{Ledger, Simulation};
pub use nep413::{open, Keypair, Opened, Payload, SignedIntent};
pub use withdraw::FtWithdraw;

/// An account inside the verifier. Agents use implicit accounts: the hex of their ed25519 key.
pub type AccountId = String;
/// A token as the verifier names it, e.g. `nep141:wrap.near`.
pub type TokenId = String;

/// The verifier on mainnet.
pub const MAINNET_VERIFIER: &str = "intents.near";

/// The verifier's reason for turning something down, in its own words ("nonce was already used").
/// The same text comes back from the contract as a panic message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal(pub String);

impl Refusal {
    pub(crate) fn new(reason: impl Into<String>) -> Self {
        Self(reason.into())
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refusal {}
