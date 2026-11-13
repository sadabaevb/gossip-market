//! The same signed bundles through two verifiers: `intents::Ledger`, which the devnet settles on
//! and agents check their bundles against, and the engine from github.com/near/intents that the
//! `intents.near` contract runs. They must agree on every verdict, word for word, and on every
//! balance afterwards.
//!
//! What this does not cover: the contract's storage, gas, key registry and pausing. Those sit
//! around the engine, not in it.
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::{DateTime, Duration, Utc};
use defuse_core::{
    amounts::Amounts,
    engine::{Engine, Inspector, State, StateView},
    events::DefuseEvent,
    fees::Pips,
    intents::{
        auth::AuthCall,
        tokens::{FtWithdraw, MtWithdraw, NativeWithdraw, NftWithdraw, NotifyOnTransfer, StorageDeposit},
    },
    payload::multi::MultiPayload,
    token_id::TokenId,
    AccountId, AccountIdRef, DefuseError, Nonce, NoncePrefix, PublicKey, Salt, Timestamp,
};
use intents::{nonce, Keypair, Ledger, SignedIntent, TokenDiff};

const VERIFIER: &str = "intents.near";
const COLLECTOR: &str = "fees.near";
const USDC: &str = "nep141:usdc.near";
const CREDITS: &str = "nep141:credits.near";
const DATA: &str = "nep141:data.near";

/// The contract's state, as far as the engine asks about it.
#[derive(Clone, Default)]
struct Contract {
    fee: Pips,
    salt: [u8; 4],
    balances: HashMap<(AccountId, TokenId), u128>,
    nonces: HashSet<(AccountId, Nonce)>,
    /// Every withdraw the engine handed over, as it read it out of the signed JSON.
    withdrawn: Vec<(AccountId, FtWithdraw)>,
}

impl StateView for Contract {
    fn verifying_contract(&self) -> Cow<'_, AccountIdRef> {
        Cow::Borrowed(AccountIdRef::new_or_panic(VERIFIER))
    }
    fn wnear_id(&self) -> Cow<'_, AccountIdRef> {
        Cow::Borrowed(AccountIdRef::new_or_panic("wrap.near"))
    }
    fn fee(&self) -> Pips {
        self.fee
    }
    fn fee_collector(&self) -> Cow<'_, AccountIdRef> {
        Cow::Borrowed(AccountIdRef::new_or_panic(COLLECTOR))
    }
    // The contract's rule for an account that never touched its key registry.
    fn has_public_key(&self, account_id: &AccountIdRef, public_key: &PublicKey) -> bool {
        account_id == public_key.to_implicit_account_id()
    }
    fn iter_public_keys(&self, account_id: &AccountIdRef) -> impl Iterator<Item = PublicKey> + '_ {
        PublicKey::from_implicit_account_id(account_id).into_iter()
    }
    fn is_nonce_used(&self, account_id: &AccountIdRef, nonce: Nonce) -> bool {
        self.nonces.contains(&(account_id.to_owned(), nonce))
    }
    fn balance_of(&self, account_id: &AccountIdRef, token_id: &TokenId) -> u128 {
        self.balances
            .get(&(account_id.to_owned(), token_id.clone()))
            .copied()
            .unwrap_or(0)
    }
    fn is_account_locked(&self, _: &AccountIdRef) -> bool {
        false
    }
    fn is_auth_by_predecessor_id_enabled(&self, _: &AccountIdRef) -> bool {
        true
    }
    fn is_valid_salt(&self, salt: Salt) -> bool {
        salt.0 == self.salt
    }
}

impl State for Contract {
    fn commit_nonce(&mut self, account_id: AccountId, nonce: Nonce) -> defuse_core::Result<()> {
        if !self.nonces.insert((account_id, nonce)) {
            return Err(DefuseError::NonceUsed);
        }
        Ok(())
    }

    fn internal_add_balance(
        &mut self,
        owner_id: AccountId,
        tokens: impl IntoIterator<Item = (TokenId, u128)>,
    ) -> defuse_core::Result<()> {
        for (token_id, amount) in tokens {
            let balance = self.balances.entry((owner_id.clone(), token_id)).or_default();
            *balance = balance.checked_add(amount).ok_or(DefuseError::BalanceOverflow)?;
        }
        Ok(())
    }

    fn internal_sub_balance(
        &mut self,
        owner_id: &AccountIdRef,
        tokens: impl IntoIterator<Item = (TokenId, u128)>,
    ) -> defuse_core::Result<()> {
        for (token_id, amount) in tokens {
            let balance = self.balances.entry((owner_id.to_owned(), token_id)).or_default();
            *balance = balance.checked_sub(amount).ok_or(DefuseError::BalanceOverflow)?;
        }
        Ok(())
    }

    // Nothing an agent signs reaches any of these.
    fn add_public_key(&mut self, _: AccountId, _: PublicKey) -> defuse_core::Result<()> {
        unimplemented!()
    }
    fn remove_public_key(&mut self, _: AccountId, _: PublicKey) -> defuse_core::Result<()> {
        unimplemented!()
    }
    fn cleanup_nonce_by_prefix(&mut self, _: &AccountIdRef, _: NoncePrefix) -> defuse_core::Result<bool> {
        unimplemented!()
    }
    // What the contract's own state does with it (`engine/state/cached.rs`, which its dry runs
    // use): take the amount from the signer's balance, refuse nothing and too much. Sending the
    // tokens on is a promise to the token's contract, outside the engine.
    fn ft_withdraw(&mut self, owner_id: &AccountIdRef, withdraw: FtWithdraw) -> defuse_core::Result<()> {
        if withdraw.amount == 0 {
            return Err(DefuseError::InvalidIntent);
        }
        let token = TokenId::from(defuse_core::token_id::nep141::Nep141TokenId::new(
            withdraw.token.clone(),
        ));
        let balance = self.balances.entry((owner_id.to_owned(), token)).or_default();
        *balance = balance
            .checked_sub(withdraw.amount)
            .ok_or(DefuseError::BalanceOverflow)?;
        self.withdrawn.push((owner_id.to_owned(), withdraw));
        Ok(())
    }
    fn nft_withdraw(&mut self, _: &AccountIdRef, _: NftWithdraw) -> defuse_core::Result<()> {
        unimplemented!()
    }
    fn mt_withdraw(&mut self, _: &AccountIdRef, _: MtWithdraw) -> defuse_core::Result<()> {
        unimplemented!()
    }
    fn native_withdraw(&mut self, _: &AccountIdRef, _: NativeWithdraw) -> defuse_core::Result<()> {
        unimplemented!()
    }
    fn notify_on_transfer(
        &self,
        _: &AccountIdRef,
        _: AccountId,
        _: Amounts,
        _: NotifyOnTransfer,
    ) -> defuse_core::Result<()> {
        unimplemented!()
    }
    fn storage_deposit(&mut self, _: &AccountIdRef, _: StorageDeposit) -> defuse_core::Result<()> {
        unimplemented!()
    }
    fn set_auth_by_predecessor_id(&mut self, _: AccountId, _: bool) -> defuse_core::Result<bool> {
        unimplemented!()
    }
    fn auth_call(&mut self, _: &AccountIdRef, _: AuthCall) -> defuse_core::Result<()> {
        unimplemented!()
    }
    fn mint(&mut self, _: AccountId, _: Amounts, _: Option<String>) -> defuse_core::Result<()> {
        unimplemented!()
    }
    fn burn(&mut self, _: &AccountIdRef, _: Amounts, _: Option<String>) -> defuse_core::Result<()> {
        unimplemented!()
    }
}

struct Quiet;

impl Inspector for Quiet {
    fn on_deadline(&mut self, _: Timestamp) {}
    fn on_event(&mut self, _: DefuseEvent<'_>) {}
    fn on_intent_executed(&mut self, _: &AccountIdRef, _: [u8; 32], _: Nonce) {}
}

/// Both verifiers over the same deposits.
struct Both {
    ours: Ledger,
    theirs: Contract,
}

impl Both {
    fn new(fee_pips: u32) -> Self {
        let mut ours = Ledger::new(VERIFIER, fee_pips);
        ours.fee_collector = COLLECTOR.into();
        let theirs = Contract {
            fee: Pips::from_pips(fee_pips).unwrap(),
            salt: ours.salt,
            ..Contract::default()
        };
        Self { ours, theirs }
    }

    fn deposit(&mut self, account: &str, token: &str, amount: u128) {
        self.ours.deposit(account, token, amount);
        self.theirs
            .balances
            .insert((account.parse().unwrap(), token.parse().unwrap()), amount);
    }

    /// Executes the batch on both, checks that they agree, and returns the common verdict.
    fn execute(&mut self, signed: &[SignedIntent]) -> Result<(), String> {
        // The engine reads the clock itself; the ledger is handed the same moment.
        let ours = self.ours.execute(signed, Utc::now()).map_err(|refusal| refusal.0);

        // As the contract receives them: JSON.
        let payloads: Vec<MultiPayload> = serde_json::from_value(serde_json::to_value(signed).unwrap())
            .expect("the contract can read what an agent signs");
        let mut scratch = self.theirs.clone();
        let theirs = Engine::new(&mut scratch, Quiet)
            .execute_signed_intents(payloads)
            .map(|_transfers| ())
            .map_err(|e| e.to_string());
        if theirs.is_ok() {
            self.theirs = scratch; // a failed call changes nothing on chain
        }

        assert_eq!(ours, theirs, "the ledger and the verifier's engine disagree");
        self.assert_same_balances();
        ours
    }

    fn assert_same_balances(&self) {
        for ((account, token), amount) in &self.theirs.balances {
            assert_eq!(
                self.ours.balance_of(account.as_str(), &token.to_string()),
                *amount,
                "balance of {token} for {account}"
            );
        }
    }
}

struct Ring {
    both: Both,
    a: Keypair,
    b: Keypair,
    c: Keypair,
}

fn ring(fee_pips: u32) -> Ring {
    let [a, b, c] = [1, 2, 3].map(|seed| Keypair::from_seed([seed; 32]));
    let mut both = Both::new(fee_pips);
    both.deposit(&a.account_id(), USDC, 50_000_000);
    both.deposit(&b.account_id(), CREDITS, 500_000);
    both.deposit(&c.account_id(), DATA, 20_000);
    Ring { both, a, b, c }
}

fn in_30s() -> DateTime<Utc> {
    Utc::now() + Duration::seconds(30)
}

impl Ring {
    fn sign(&self, key: &Keypair, give: (&str, u128), take: (&str, u128)) -> SignedIntent {
        let deadline = in_30s();
        let diff = TokenDiff::swap(give, take).unwrap();
        let nonce = nonce::versioned(self.both.ours.salt, deadline);
        key.sign_intents(VERIFIER, deadline, &nonce, vec![diff.to_intent()])
    }

    /// Each gives what it has and receives what the previous one gave, less the fee.
    fn legs(&self, usdc: u128, credits: u128, data: u128) -> Vec<SignedIntent> {
        let fee = self.both.ours.fee_pips;
        let net = |token, amount| intents::net_of_fee(token, amount, fee).unwrap();
        vec![
            self.sign(&self.a, (USDC, usdc), (CREDITS, net(CREDITS, credits))),
            self.sign(&self.b, (CREDITS, credits), (DATA, net(DATA, data))),
            self.sign(&self.c, (DATA, data), (USDC, net(USDC, usdc))),
        ]
    }
}

#[test]
fn the_engine_settles_our_ring_of_three() {
    for fee_pips in [0, 1, 100, 10_000] {
        let mut r = ring(fee_pips);
        let legs = r.legs(10_000_000, 100_000, 5_000);
        assert_eq!(r.both.execute(&legs), Ok(()), "fee {fee_pips}");
        // And only once.
        assert_eq!(r.both.execute(&legs), Err("nonce was already used".into()));
    }
}

#[test]
fn the_engine_refuses_a_ring_with_a_leg_missing() {
    let mut r = ring(100);
    let legs = r.legs(10_000_000, 100_000, 5_000);
    let reason = r.both.execute(&legs[..2]).unwrap_err();
    assert_eq!(
        reason,
        r#"invariant violated: {"error":"unmatched_deltas","unmatched_deltas":{"nep141:data.near":"4999","nep141:usdc.near":"-9999000"}}"#
    );
    // The legs are still good: with the third they settle.
    assert_eq!(r.both.execute(&legs), Ok(()));
}

// One unit more or less on any leg and the batch no longer nets. The ring finder's arithmetic has
// to hit the contract's rounding exactly; this is the test that says it does.
#[test]
fn the_engine_and_the_ledger_agree_to_the_last_unit() {
    let mut seed = 0x9e3779b97f4a7c15u64;
    let mut random = move |below: u128| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        u128::from(seed) % below
    };
    for case in 0..300 {
        let fee_pips = [0, 1, 7, 100, 3_000, 10_000][case % 6];
        let mut r = ring(fee_pips);
        let (usdc, credits, data) = (1 + random(50_000_000), 1 + random(500_000), 1 + random(20_000));
        let net = |token, amount| intents::net_of_fee(token, amount, fee_pips).unwrap();
        // A leg that would receive nothing is not a token_diff the contract accepts.
        if net(USDC, usdc) == 0 || net(CREDITS, credits) == 0 || net(DATA, data) == 0 {
            continue;
        }

        let mut off_by_one = r.legs(usdc, credits, data);
        let extra = net(CREDITS, credits) + 1;
        off_by_one[0] = r.sign(&r.a, (USDC, usdc), (CREDITS, extra));
        let reason = r.both.execute(&off_by_one).unwrap_err();
        assert!(reason.starts_with("invariant violated"), "case {case}: {reason}");

        let exact = r.legs(usdc, credits, data);
        assert_eq!(r.both.execute(&exact), Ok(()), "case {case}: fee {fee_pips}");
    }
}

#[test]
fn the_engine_refuses_for_the_reasons_the_ledger_gives() {
    let mut r = ring(100);
    let good = r.legs(10_000_000, 100_000, 5_000);
    let with_first = |first: SignedIntent| {
        let mut legs = good.clone();
        legs[0] = first;
        legs
    };
    let diff = TokenDiff::swap((USDC, 10_000_000), (CREDITS, 99_990)).unwrap();

    let more_than_held = with_first(r.sign(&r.a, (USDC, 50_000_001), (CREDITS, 99_990)));
    assert_eq!(
        r.both.execute(&more_than_held),
        Err("insufficient balance or overflow".into())
    );

    let expired = Utc::now() - Duration::seconds(5);
    let late =
        r.a.sign_intents(VERIFIER, expired, &nonce::plain(), vec![diff.to_intent()]);
    assert_eq!(r.both.execute(&with_first(late)), Err("deadline has expired".into()));

    let for_devnet =
        r.a.sign_intents("intents.devnet", in_30s(), &nonce::plain(), vec![diff.to_intent()]);
    assert_eq!(
        r.both.execute(&with_first(for_devnet)),
        Err("wrong verifying_contract".into())
    );

    let stale_salt = nonce::versioned([9, 9, 9, 9], in_30s());
    let salted =
        r.a.sign_intents(VERIFIER, in_30s(), &stale_salt, vec![diff.to_intent()]);
    assert_eq!(r.both.execute(&with_first(salted)), Err("invalid salt".into()));

    let short = nonce::versioned(r.both.ours.salt, Utc::now() + Duration::seconds(10));
    let outlives = r.a.sign_intents(VERIFIER, in_30s(), &short, vec![diff.to_intent()]);
    assert_eq!(
        r.both.execute(&with_first(outlives)),
        Err("deadline is greater than nonce".into())
    );

    let mut tampered = good.clone();
    tampered[0].payload.message = tampered[0].payload.message.replace("\"99990\"", "\"999900\"");
    assert_eq!(r.both.execute(&tampered), Err("invalid signature".into()));

    let zero = TokenDiff {
        diff: BTreeMap::from([(USDC.into(), 0)]),
        memo: None,
    };
    let nothing =
        r.a.sign_intents(VERIFIER, in_30s(), &nonce::plain(), vec![zero.to_intent()]);
    assert_eq!(r.both.execute(&with_first(nothing)), Err("invalid intent".into()));

    // Carol signing in Alice's name.
    let message = good[0].payload.message.clone();
    let forged = r.c.sign_message(VERIFIER, message, &nonce::plain());
    let reason = r.both.execute(&with_first(forged)).unwrap_err();
    assert!(reason.contains("doesn't exist for account"), "{reason}");

    // None of that spent anything: the ring still settles.
    assert_eq!(r.both.execute(&good), Ok(()));
}

// A withdraw is the only way an agent's tokens leave intents.near. If the engine read ours as
// another token, receiver or amount, or counted it in the netting, the funds would go elsewhere or
// stay stuck, and the ledger behind the devnet and every dry run would not say so. The refusals
// for nothing and too much come from the contract's state, which the stub above copies; what this
// pins is that the engine parses, routes and nets our JSON as we mean it.
#[test]
fn the_engine_reads_our_withdraw_as_we_wrote_it() {
    let mut r = ring(100);
    let (salt, a) = (r.both.ours.salt, r.a.clone());
    let withdraw = |amount| {
        let deadline = in_30s();
        let intent = intents::FtWithdraw::new(USDC, "alice.near", amount)
            .unwrap()
            .with_memo("gossipmarket withdraw")
            .to_intent();
        a.sign_intents(VERIFIER, deadline, &nonce::versioned(salt, deadline), vec![intent])
    };

    let twenty = [withdraw(20_000_000)];
    assert_eq!(r.both.execute(&twenty), Ok(()));
    let [(owner, read)] = r.both.theirs.withdrawn.as_slice() else {
        panic!("the engine handed over {} withdraws", r.both.theirs.withdrawn.len())
    };
    assert_eq!(owner.as_str(), a.account_id());
    assert_eq!(
        (read.token.as_str(), read.receiver_id.as_str(), read.amount),
        ("usdc.near", "alice.near", 20_000_000)
    );
    assert_eq!(read.memo.as_deref(), Some("gossipmarket withdraw"));
    // Plain `ft_transfer`, nothing else taken: no call, no storage deposit.
    assert!(read.msg.is_none() && read.storage_deposit.is_none());
    assert_eq!(r.both.execute(&twenty), Err("nonce was already used".into()));

    assert_eq!(
        r.both.execute(&[withdraw(30_000_001)]),
        Err("insufficient balance or overflow".into())
    );
    assert_eq!(r.both.execute(&[withdraw(0)]), Err("invalid intent".into()));

    // In one batch with a ring: the ring still has to net, and the withdraw is not part of that.
    let mut batch = r.legs(10_000_000, 100_000, 5_000);
    batch.push(withdraw(20_000_000));
    assert_eq!(r.both.execute(&batch), Ok(()));
    assert_eq!(r.both.ours.balance_of(&a.account_id(), USDC), 0);
}

// Until the contract switches them off, as its source says it will.
#[test]
fn the_engine_still_takes_plain_nonces() {
    let mut r = ring(100);
    let sign = |key: &Keypair, give, take| {
        let diff = TokenDiff::swap(give, take).unwrap();
        key.sign_intents(VERIFIER, in_30s(), &nonce::plain(), vec![diff.to_intent()])
    };
    r.both.deposit(&r.b.account_id(), USDC, 0);
    let pair = [
        sign(&r.a, (USDC, 1_000_000), (CREDITS, 9_999)),
        sign(&r.b, (CREDITS, 10_000), (USDC, 999_900)),
    ];
    assert_eq!(r.both.execute(&pair), Ok(()));
}

// The ring finder against the contract. Wants at random prices and sizes, in amounts from single
// units to hundreds of NEAR in yocto, at fees from none to one percent: every ring the finder
// returns must settle in the verifier's engine exactly as sized, and stay within what each maker
// announced. This is the arithmetic that mainnet would otherwise be the first to check.
#[test]
fn the_engine_settles_every_ring_the_finder_sizes() {
    use market::{find_rings, Want};

    let mut seed = 0x2545f4914f6cdd1du64;
    let mut random = move |below: u128| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        u128::from(seed) % below
    };
    const NEAR: &str = "nep141:wrap.near";
    let tokens = [USDC, CREDITS, DATA, NEAR];
    let scale = |token: &str| if token == NEAR { 10u128.pow(21) } else { 1 };
    let keys: Vec<Keypair> = (1..=4).map(|seed| Keypair::from_seed([seed; 32])).collect();

    let (mut settled, mut sizes) = (0, [0usize; 5]);
    for case in 0..400 {
        let fee_pips = [0, 1, 1, 100, 2_500, 10_000][case % 6];
        let makers = 2 + case % 3;
        // Each maker gives one token and takes the next, at a price near one-to-one in units, some
        // a little generous and some a little greedy, so that some rings close and some do not.
        let wants: Vec<Want> = (0..makers)
            .map(|i| {
                let (give, take) = (tokens[i], tokens[(i + 1) % makers]);
                let give_amount = (1 + random(5_000_000)) * scale(give);
                let asked = give_amount / scale(give) * (90 + random(15)) / 100;
                Want {
                    maker: keys[i].account_id(),
                    id: 1,
                    rev: 0,
                    give: give.into(),
                    give_amount,
                    take: take.into(),
                    take_amount: (1 + asked) * scale(take),
                    expires_ms: u64::MAX,
                    addrs: vec![],
                }
            })
            .collect();

        for ring in find_rings(&wants, fee_pips) {
            assert!(ring.is_sound(fee_pips), "case {case}: {ring:?}");
            let mut both = Both::new(fee_pips);
            let deadline = in_30s();
            let mut signed = Vec::new();
            for leg in &ring.legs {
                let want = wants.iter().find(|want| want.maker == leg.maker).unwrap();
                assert_eq!(leg.is_within(want), Ok(()), "case {case}");
                // Exactly what the leg gives, and not a unit more: the ring must not overdraw.
                both.deposit(&leg.maker, &leg.give, leg.give_amount);
                let key = keys.iter().find(|key| key.account_id() == leg.maker).unwrap();
                let nonce = nonce::versioned(both.ours.salt, deadline);
                signed.push(key.sign_intents(VERIFIER, deadline, &nonce, vec![leg.diff().unwrap().to_intent()]));
            }
            assert_eq!(both.execute(&signed), Ok(()), "case {case}: fee {fee_pips}, {ring:?}");
            settled += 1;
            sizes[ring.legs.len()] += 1;
        }
    }
    // The cases must actually exercise rings of every size, or this would pass by finding none.
    assert!(settled > 100, "only {settled} rings were found");
    assert!(sizes[2] > 10 && sizes[3] > 10 && sizes[4] > 10, "{sizes:?}");
}
