//! The verifier's settlement rules over balances kept in memory: what `intents.near` does with a
//! batch of signed `token_diff` and `ft_withdraw` intents, in the same order and with the same
//! refusals.
//!
//! The local devnet settles on this, so that agents can be run and rehearsed without funds, and
//! agents check their own bundles against it before paying gas. It is a model, not the contract:
//! it knows implicit accounts, `token_diff` and plain `ft_withdraw` only, and `conformance` holds
//! it to the real engine.
use std::collections::{BTreeMap, HashSet};

use chrono::{DateTime, Utc};

use crate::diff::{fee_ceil, TokenDiff};
use crate::nep413::{implicit_account, open, tag, SignedIntent};
use crate::nonce::{self, Nonce, Salt};
use crate::withdraw::FtWithdraw;
use crate::{AccountId, Refusal, TokenId};

#[derive(Debug, Clone)]
pub struct Ledger {
    /// The account this verifier answers to: the `recipient` every intent must name.
    pub contract: AccountId,
    pub fee_pips: u32,
    pub fee_collector: AccountId,
    pub salt: Salt,
    balances: BTreeMap<(AccountId, TokenId), u128>,
    used_nonces: HashSet<(AccountId, [u8; 32])>,
}

/// What a dry run found. The contract panics on everything except an unbalanced batch, which it
/// reports instead, so that a solver can read off what is missing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Simulation {
    /// Per token: received plus fees, minus given. Empty when the batch nets.
    pub unmatched: BTreeMap<TokenId, i128>,
}

impl Ledger {
    pub fn new(contract: impl Into<AccountId>, fee_pips: u32) -> Self {
        Self {
            contract: contract.into(),
            fee_pips,
            fee_collector: "fees.devnet".into(),
            salt: [0x67, 0x6d, 0x30, 0x31],
            balances: BTreeMap::new(),
            used_nonces: HashSet::new(),
        }
    }

    /// Credits `account`, as a deposit into the verifier would.
    pub fn deposit(&mut self, account: &str, token: &str, amount: u128) {
        let balance = self.balances.entry((account.into(), token.into())).or_default();
        *balance = balance.saturating_add(amount);
    }

    pub fn balance_of(&self, account: &str, token: &str) -> u128 {
        self.balances
            .get(&(account.to_owned(), token.to_owned()))
            .copied()
            .unwrap_or(0)
    }

    pub fn is_nonce_used(&self, account: &str, nonce: &[u8; 32]) -> bool {
        self.used_nonces.contains(&(account.to_owned(), *nonce))
    }

    /// A dry run: nothing changes. `Err` is a panic on chain.
    pub fn simulate(&self, signed: &[SignedIntent], now: DateTime<Utc>) -> Result<Simulation, Refusal> {
        let mut scratch = self.clone();
        let unmatched = scratch.apply(signed, now)?;
        Ok(Simulation { unmatched })
    }

    /// Settles the whole batch or none of it.
    pub fn execute(&mut self, signed: &[SignedIntent], now: DateTime<Utc>) -> Result<(), Refusal> {
        let mut scratch = self.clone();
        let unmatched = scratch.apply(signed, now)?;
        if !unmatched.is_empty() {
            return Err(invariant_violated(&unmatched));
        }
        *self = scratch;
        Ok(())
    }

    fn apply(&mut self, signed: &[SignedIntent], now: DateTime<Utc>) -> Result<BTreeMap<TokenId, i128>, Refusal> {
        // Per token: everything credited (to signers and to the fee collector) minus everything debited.
        let mut left: BTreeMap<TokenId, i128> = BTreeMap::new();
        for signed in signed {
            let opened = open(signed)?;
            if opened.recipient != self.contract {
                return Err(Refusal::new("wrong verifying_contract"));
            }
            if opened.deadline < now {
                return Err(Refusal::new("deadline has expired"));
            }
            // An implicit account answers to the key it is named after. Keys added later, and named
            // accounts, live in the contract's registry, which this model does not have.
            if opened.signer_id != implicit_account(&opened.public_key) {
                return Err(Refusal::new(format!(
                    "public key '{}' doesn't exist for account '{}'",
                    tag(&opened.public_key),
                    opened.signer_id
                )));
            }
            if let Nonce::V1 { salt, deadline_ns } = nonce::read(&opened.nonce) {
                if salt != self.salt {
                    return Err(Refusal::new("invalid salt"));
                }
                if nonce::nanos(opened.deadline) > deadline_ns {
                    return Err(Refusal::new("deadline is greater than nonce"));
                }
                if deadline_ns < nonce::nanos(now) {
                    return Err(Refusal::new("nonce was already expired"));
                }
            }
            if !self.used_nonces.insert((opened.signer_id.clone(), opened.nonce)) {
                return Err(Refusal::new("nonce was already used"));
            }
            for intent in &opened.intents {
                if let Some(diff) = TokenDiff::from_intent(intent) {
                    self.apply_diff(&opened.signer_id, &diff, &mut left)?;
                } else if let Some(withdraw) = FtWithdraw::from_intent(intent) {
                    self.withdraw(&opened.signer_id, &withdraw)?;
                } else {
                    return Err(Refusal::new(format!(
                        "JSON: this model settles token_diff and plain ft_withdraw only, got {}",
                        intent["intent"]
                    )));
                }
            }
        }
        left.retain(|_, total| *total != 0);
        Ok(left)
    }

    fn apply_diff(
        &mut self,
        signer: &str,
        diff: &TokenDiff,
        left: &mut BTreeMap<TokenId, i128>,
    ) -> Result<(), Refusal> {
        let overflow = || Refusal::new("insufficient balance or overflow");
        if diff.diff.is_empty() {
            return Err(Refusal::new("invalid intent"));
        }
        for (token, delta) in &diff.diff {
            if *delta == 0 {
                return Err(Refusal::new("invalid intent"));
            }
            let amount = delta.unsigned_abs();
            let balance = self.balances.entry((signer.to_owned(), token.clone())).or_default();
            *balance = if *delta < 0 {
                balance.checked_sub(amount)
            } else {
                balance.checked_add(amount)
            }
            .ok_or_else(overflow)?;
            let mut change = *delta;

            // The fee comes out of what is given, and goes to the collector.
            if *delta < 0 {
                let fee = fee_ceil(token, amount, self.fee_pips).ok_or_else(overflow)?;
                if fee > 0 {
                    let collected = self
                        .balances
                        .entry((self.fee_collector.clone(), token.clone()))
                        .or_default();
                    *collected = collected.checked_add(fee).ok_or_else(overflow)?;
                    change = change
                        .checked_add(i128::try_from(fee).map_err(|_| overflow())?)
                        .ok_or_else(overflow)?;
                }
            }
            let total = left.entry(token.clone()).or_default();
            *total = total.checked_add(change).ok_or_else(overflow)?;
        }
        Ok(())
    }

    /// The tokens leave the signer's balance and the verifier altogether, so they take no part in
    /// the netting, and no fee is taken. Where they go is the token contract's business: this
    /// model only debits.
    fn withdraw(&mut self, signer: &str, withdraw: &FtWithdraw) -> Result<(), Refusal> {
        if withdraw.amount == 0 {
            return Err(Refusal::new("invalid intent"));
        }
        let balance = self
            .balances
            .entry((signer.to_owned(), withdraw.token_id()))
            .or_default();
        *balance = balance
            .checked_sub(withdraw.amount)
            .ok_or_else(|| Refusal::new("insufficient balance or overflow"))?;
        Ok(())
    }
}

/// The contract's own wording, so that agents read one message whichever verifier refused.
fn invariant_violated(unmatched: &BTreeMap<TokenId, i128>) -> Refusal {
    let deltas: BTreeMap<&TokenId, String> = unmatched.iter().map(|(token, d)| (token, d.to_string())).collect();
    let detail = serde_json::json!({ "error": "unmatched_deltas", "unmatched_deltas": deltas });
    Refusal::new(format!("invariant violated: {detail}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nep413::Keypair;
    use chrono::Duration;

    const USDC: &str = "nep141:usdc.near";
    const CREDITS: &str = "nep141:credits.near";
    const DATA: &str = "nep141:data.near";
    const VERIFIER: &str = "intents.devnet";

    struct World {
        ledger: Ledger,
        now: DateTime<Utc>,
        a: Keypair,
        b: Keypair,
        c: Keypair,
    }

    /// Alice holds USDC and wants credits, Bob holds credits and wants data, Carol holds data and
    /// wants USDC. No two of them can trade with each other.
    fn world() -> World {
        let [a, b, c] = [1, 2, 3].map(|seed| Keypair::from_seed([seed; 32]));
        let mut ledger = Ledger::new(VERIFIER, 100);
        ledger.deposit(&a.account_id(), USDC, 50_000_000);
        ledger.deposit(&b.account_id(), CREDITS, 500_000);
        ledger.deposit(&c.account_id(), DATA, 20_000);
        World {
            ledger,
            now: Utc::now(),
            a,
            b,
            c,
        }
    }

    impl World {
        fn sign(&self, key: &Keypair, give: (&str, u128), take: (&str, u128)) -> SignedIntent {
            let deadline = self.now + Duration::seconds(30);
            let nonce = nonce::versioned(self.ledger.salt, deadline);
            let diff = TokenDiff::swap(give, take).unwrap();
            key.sign_intents(VERIFIER, deadline, &nonce, vec![diff.to_intent()])
        }

        fn ring(&self) -> Vec<SignedIntent> {
            vec![
                self.sign(&self.a, (USDC, 10_000_000), (CREDITS, 99_990)),
                self.sign(&self.b, (CREDITS, 100_000), (DATA, 4_999)),
                self.sign(&self.c, (DATA, 5_000), (USDC, 9_999_000)),
            ]
        }
    }

    fn refused(reason: &str) -> Result<(), Refusal> {
        Err(Refusal::new(reason))
    }

    #[test]
    fn settles_a_ring_of_three_in_one_batch() {
        let mut w = world();
        let ring = w.ring();
        assert_eq!(w.ledger.simulate(&ring, w.now), Ok(Simulation::default()));
        // A dry run moves nothing.
        assert_eq!(w.ledger.balance_of(&w.a.account_id(), USDC), 50_000_000);

        w.ledger.execute(&ring, w.now).unwrap();
        let balance = |key: &Keypair, token| w.ledger.balance_of(&key.account_id(), token);
        assert_eq!((balance(&w.a, USDC), balance(&w.a, CREDITS)), (40_000_000, 99_990));
        assert_eq!((balance(&w.b, CREDITS), balance(&w.b, DATA)), (400_000, 4_999));
        assert_eq!((balance(&w.c, DATA), balance(&w.c, USDC)), (15_000, 9_999_000));
        // Nothing is created or lost: the difference is the fee.
        assert_eq!(w.ledger.balance_of("fees.devnet", USDC), 1_000);
        assert_eq!(w.ledger.balance_of("fees.devnet", CREDITS), 10);
        assert_eq!(w.ledger.balance_of("fees.devnet", DATA), 1);
    }

    // The property the whole market rests on: nobody's tokens move unless everybody's do.
    #[test]
    fn two_legs_of_a_ring_settle_nothing() {
        let mut w = world();
        let ring = w.ring();
        let dry = w.ledger.simulate(&ring[..2], w.now).unwrap();
        assert_eq!(
            dry.unmatched,
            BTreeMap::from([(USDC.into(), -9_999_000), (DATA.into(), 4_999)])
        );
        let reason = w.ledger.execute(&ring[..2], w.now).unwrap_err().0;
        assert!(
            reason.starts_with("invariant violated: {\"error\":\"unmatched_deltas\""),
            "{reason}"
        );
        assert_eq!(w.ledger.balance_of(&w.a.account_id(), USDC), 50_000_000);
        // And the nonces are not spent: the same legs still settle once the third arrives.
        w.ledger.execute(&ring, w.now).unwrap();
    }

    #[test]
    fn a_batch_settles_once() {
        let mut w = world();
        let ring = w.ring();
        w.ledger.execute(&ring, w.now).unwrap();
        assert!(w
            .ledger
            .is_nonce_used(&w.a.account_id(), &open(&ring[0]).unwrap().nonce));
        assert_eq!(w.ledger.execute(&ring, w.now), refused("nonce was already used"));
    }

    #[test]
    fn refuses_what_the_contract_refuses() {
        let mut w = world();
        let ring = w.ring();

        assert_eq!(
            w.ledger.execute(&ring, w.now + Duration::seconds(31)),
            refused("deadline has expired")
        );

        let mut poor = ring.clone();
        poor[0] = w.sign(&w.a, (USDC, 50_000_001), (CREDITS, 99_990));
        assert_eq!(
            w.ledger.execute(&poor, w.now),
            refused("insufficient balance or overflow")
        );

        let mut elsewhere = Ledger::new("intents.near", 100);
        elsewhere.salt = w.ledger.salt;
        assert_eq!(elsewhere.execute(&ring, w.now), refused("wrong verifying_contract"));

        // After the contract rotates its salt, nonces made for the old one are dead.
        let mut rotated = w.ledger.clone();
        rotated.salt = [9, 9, 9, 9];
        assert_eq!(rotated.execute(&ring, w.now), refused("invalid salt"));

        // A message may not outlive its nonce.
        let deadline = w.now + Duration::seconds(30);
        let short_nonce = nonce::versioned(w.ledger.salt, deadline - Duration::seconds(1));
        let diff = TokenDiff::swap((USDC, 10_000_000), (CREDITS, 99_990)).unwrap();
        let mut outlives = ring.clone();
        outlives[0] =
            w.a.sign_intents(VERIFIER, deadline, &short_nonce, vec![diff.to_intent()]);
        assert_eq!(
            w.ledger.execute(&outlives, w.now),
            refused("deadline is greater than nonce")
        );

        // Signing for an account that is not the key's own.
        let mut forged = ring.clone();
        let message = forged[0].payload.message.replace(&w.a.account_id(), &w.b.account_id());
        forged[0] = w.c.sign_message(VERIFIER, message, &nonce::plain());
        let reason = w.ledger.execute(&forged, w.now).unwrap_err().0;
        assert!(reason.contains("doesn't exist for account"), "{reason}");

        // Nothing above moved anything, and the ring still settles.
        w.ledger.execute(&ring, w.now).unwrap();
    }

    // A withdraw is how an agent's tokens get back out. If the ledger counted it in the netting,
    // the devnet would refuse every withdraw as unbalanced; if it took more or less than signed,
    // or let a second copy through, an agent's dry run would not predict mainnet.
    #[test]
    fn a_withdraw_takes_exactly_what_was_signed_and_needs_no_counterparty() {
        let mut w = world();
        let (deadline, salt, a) = (w.now + Duration::seconds(30), w.ledger.salt, w.a.clone());
        let sign = |amount| {
            let withdraw = FtWithdraw::new(USDC, "alice.near", amount).unwrap();
            let nonce = nonce::versioned(salt, deadline);
            a.sign_intents(VERIFIER, deadline, &nonce, vec![withdraw.to_intent()])
        };

        let twenty = [sign(20_000_000)];
        assert_eq!(w.ledger.simulate(&twenty, w.now), Ok(Simulation::default()));
        w.ledger.execute(&twenty, w.now).unwrap();
        assert_eq!(w.ledger.balance_of(&w.a.account_id(), USDC), 30_000_000);
        // No fee: the fee is a token_diff's.
        assert_eq!(w.ledger.balance_of("fees.devnet", USDC), 0);
        assert_eq!(w.ledger.execute(&twenty, w.now), refused("nonce was already used"));

        assert_eq!(
            w.ledger.execute(&[sign(30_000_001)], w.now),
            refused("insufficient balance or overflow")
        );
        assert_eq!(w.ledger.execute(&[sign(0)], w.now), refused("invalid intent"));
        w.ledger.execute(&[sign(30_000_000)], w.now).unwrap();
        assert_eq!(w.ledger.balance_of(&w.a.account_id(), USDC), 0);
    }

    // Plain nonces carry no salt and no deadline of their own; the contract still takes them.
    #[test]
    fn settles_plain_nonces_too() {
        let mut w = world();
        let deadline = w.now + Duration::seconds(30);
        let sign = |key: &Keypair, give, take| {
            let diff = TokenDiff::swap(give, take).unwrap();
            key.sign_intents(VERIFIER, deadline, &nonce::plain(), vec![diff.to_intent()])
        };
        let pair = [
            sign(&w.a, (USDC, 1_000_000), (CREDITS, 9_999)),
            sign(&w.b, (CREDITS, 10_000), (USDC, 999_900)),
        ];
        w.ledger.execute(&pair, w.now).unwrap();
    }
}
