//! Rings: sets of wants in which each maker gives its token to the next and the last gives to the
//! first. Two wants that mirror each other are a ring of two; three makers none of whom wants what
//! the next one has are a ring of three, which no matcher of pairs can settle.
//!
//! The verifier settles a ring only if every token nets to zero after its fee, so sizing one is
//! arithmetic against the contract's rounding. `conformance` checks that arithmetic against the
//! contract's own engine.
use std::collections::BTreeMap;

use intents::{net_of_fee, AccountId, SignedIntent, TokenDiff, TokenId};
use primitive_types::U256;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::want::as_text;
use crate::Want;

/// Makers in one ring. Each one adds a signature to collect and a way for the ring to fall apart.
pub const MAX_RING: usize = 4;
/// Rings one search returns.
const MAX_RINGS: usize = 64;
/// Wants a search walks through per starting want, to bound a crowded book.
const MAX_STEPS: usize = 20_000;

/// One maker's part in a ring.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Leg {
    pub maker: AccountId,
    pub want_id: u64,
    pub want_rev: u64,
    pub give: TokenId,
    #[serde(with = "as_text")]
    pub give_amount: u128,
    pub take: TokenId,
    #[serde(with = "as_text")]
    pub take_amount: u128,
}

impl Leg {
    /// What the maker signs: exactly this leg, nothing about anyone else's.
    pub fn diff(&self) -> Option<TokenDiff> {
        TokenDiff::swap((&self.give, self.give_amount), (&self.take, self.take_amount))
    }

    /// Whether this leg is within what `want`, as its maker last announced it, offers: the same
    /// tokens, no more given than offered, and no worse a price.
    pub fn is_within(&self, want: &Want) -> Result<(), &'static str> {
        if (&self.maker, self.want_id) != (&want.maker, want.id) {
            return Err("not this want");
        }
        if self.want_rev != want.rev {
            return Err("the want has changed");
        }
        if (&self.give, &self.take) != (&want.give, &want.take) {
            return Err("wrong tokens");
        }
        if self.give_amount == 0 || self.take_amount == 0 {
            return Err("empty leg");
        }
        if self.give_amount > want.give_amount {
            return Err("gives more than offered");
        }
        // take / give >= want.take / want.give, without dividing.
        if U256::from(self.take_amount) * U256::from(want.give_amount)
            < U256::from(self.give_amount) * U256::from(want.take_amount)
        {
            return Err("takes less than asked");
        }
        Ok(())
    }
}

/// Legs in giving order: each gives its token to the next, the last to the first. The first leg is
/// the one with the smallest (maker, want id), so that a ring has one spelling.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Ring {
    pub legs: Vec<Leg>,
}

impl Ring {
    /// A name for exactly these legs and amounts: 16 hex characters of their hash.
    pub fn id(&self) -> String {
        let mut h = Sha256::new();
        for leg in &self.legs {
            let line = format!(
                "{}:{}:{}:{}:{}:{}:{}\n",
                leg.maker, leg.want_id, leg.want_rev, leg.give, leg.give_amount, leg.take, leg.take_amount
            );
            h.update(line);
        }
        hex::encode(&h.finalize()[..8])
    }

    /// The wants this ring is made of, whatever the amounts: what a node waits on before proposing.
    pub fn wants(&self) -> Vec<(AccountId, u64, u64)> {
        let mut wants: Vec<_> = self
            .legs
            .iter()
            .map(|leg| (leg.maker.clone(), leg.want_id, leg.want_rev))
            .collect();
        wants.sort();
        wants
    }

    pub fn leg_of(&self, maker: &str) -> Option<&Leg> {
        self.legs.iter().find(|leg| leg.maker == maker)
    }

    /// Whether `signed` is the `i`-th maker's signature on exactly its leg of this ring, for this
    /// verifier, expiring at `deadline_ms`. The proposer checks each reply with this, and a maker
    /// each leg of a bundle it is handed: one wrong leg and the batch settles nothing, or settles
    /// something other than the ring.
    pub fn check_signed(
        &self,
        i: usize,
        signed: &SignedIntent,
        verifier: &str,
        deadline_ms: u64,
    ) -> Result<(), String> {
        let leg = self.legs.get(i).ok_or("no such leg")?;
        let opened = intents::open(signed).map_err(|refusal| refusal.0)?;
        if opened.signer_id != leg.maker || opened.signer_id != intents::nep413::implicit_account(&opened.public_key) {
            return Err("signed by someone else".into());
        }
        if opened.recipient != verifier {
            return Err("signed for another verifier".into());
        }
        if opened.deadline.timestamp_millis() != i64::try_from(deadline_ms).unwrap_or(i64::MAX) {
            return Err("wrong deadline".into());
        }
        let [intent] = opened.intents.as_slice() else {
            return Err("not exactly one intent".into());
        };
        let diff = TokenDiff::from_intent(intent).ok_or("not a token_diff")?;
        if Some(&diff.diff) != leg.diff().as_ref().map(|expected| &expected.diff) {
            return Err("signed amounts differ from the leg".into());
        }
        Ok(())
    }

    /// What the verifier would find left over if these legs were settled at this fee. Empty means
    /// the ring nets; `None` that it is malformed.
    pub fn unmatched(&self, fee_pips: u32) -> Option<BTreeMap<TokenId, i128>> {
        let diffs: Vec<TokenDiff> = self.legs.iter().map(Leg::diff).collect::<Option<_>>()?;
        intents::unmatched(&diffs, fee_pips)
    }

    /// Whether this is a ring at all: two to four distinct makers, in its one spelling, netting to
    /// zero at this fee. A node checks this of any ring it did not compute itself.
    pub fn is_sound(&self, fee_pips: u32) -> bool {
        let n = self.legs.len();
        let first = self.legs.iter().map(|leg| (&leg.maker, leg.want_id)).min();
        let mut makers: Vec<_> = self.legs.iter().map(|leg| &leg.maker).collect();
        makers.sort();
        makers.dedup();
        (2..=MAX_RING).contains(&n)
            && makers.len() == n
            && first == self.legs.first().map(|leg| (&leg.maker, leg.want_id))
            && self.unmatched(fee_pips).is_some_and(|left| left.is_empty())
    }
}

/// Every ring the open wants allow at this fee, sized as large as the wants permit. Shortest
/// first, then by id: every node lists them in the same order.
pub fn find_rings<'a>(wants: impl IntoIterator<Item = &'a Want>, fee_pips: u32) -> Vec<Ring> {
    let wants: Vec<&Want> = wants.into_iter().filter(|want| want.is_open()).collect();
    let mut rings = Vec::new();
    for start in 0..wants.len() {
        let mut steps = 0;
        extend(&wants, &mut vec![start], fee_pips, &mut rings, &mut steps);
    }
    rings.sort_by_cached_key(|ring: &Ring| (ring.legs.len(), ring.id()));
    rings.truncate(MAX_RINGS);
    rings
}

/// Depth-first: grows `path` by wants that take what its last want gives, and closes it when the
/// first want takes what the last gives. Only paths whose first want is their smallest are kept,
/// so each ring is found once, in its one spelling.
fn extend(wants: &[&Want], path: &mut Vec<usize>, fee_pips: u32, rings: &mut Vec<Ring>, steps: &mut usize) {
    let key = |i: usize| (&wants[i].maker, wants[i].id);
    let (first, last) = (path[0], path[path.len() - 1]);
    if path.len() >= 2 && wants[first].take == wants[last].give {
        let cycle: Vec<&Want> = path.iter().map(|&i| wants[i]).collect();
        rings.extend(size(&cycle, fee_pips));
    }
    if path.len() == MAX_RING {
        return;
    }
    for next in 0..wants.len() {
        *steps += 1;
        if *steps > MAX_STEPS {
            return;
        }
        let fits = wants[next].take == wants[last].give
            && key(next) > key(first)
            && path.iter().all(|&i| wants[i].maker != wants[next].maker);
        if fits {
            path.push(next);
            extend(wants, path, fee_pips, rings, steps);
            path.pop();
        }
    }
}

/// The largest amounts at which this cycle of wants settles: nobody gives more than offered,
/// everybody receives all the previous maker gave less the fee, and that is at least what they
/// asked for it. `None` if the prices do not close.
///
/// Whatever a maker is willing to give beyond what the next one asked stays with the receiver:
/// a ring has no middleman to keep the difference.
fn size(cycle: &[&Want], fee_pips: u32) -> Option<Ring> {
    let n = cycle.len();
    let prev = |i: usize| (i + n - 1) % n;
    let received = |give: &[u128], i: usize| net_of_fee(&cycle[prev(i)].give, give[prev(i)], fee_pips);

    // Start from everything on offer and shrink until each maker's price holds. Lowering one
    // amount can only lower the others, so this ends at the largest amounts that work.
    let mut give: Vec<u128> = cycle.iter().map(|want| want.give_amount).collect();
    let mut settled = false;
    // Prices that close settle within a lap or two; ones that do not would shrink for ever.
    for _lap in 0..64 {
        let mut changed = false;
        for i in 0..n {
            // What this maker gives for what it receives, at its own price, rounded in its favour.
            let affordable =
                U256::from(received(&give, i)?) * U256::from(cycle[i].give_amount) / U256::from(cycle[i].take_amount);
            let affordable = u128::try_from(affordable).unwrap_or(u128::MAX);
            if affordable < give[i] {
                give[i] = affordable;
                changed = true;
            }
            if give[i] == 0 {
                return None;
            }
        }
        if !changed {
            settled = true;
            break;
        }
    }
    if !settled {
        return None;
    }

    let legs: Option<Vec<Leg>> = (0..n)
        .map(|i| {
            let take_amount = received(&give, i).filter(|amount| *amount > 0)?;
            Some(Leg {
                maker: cycle[i].maker.clone(),
                want_id: cycle[i].id,
                want_rev: cycle[i].rev,
                give: cycle[i].give.clone(),
                give_amount: give[i],
                take: cycle[i].take.clone(),
                take_amount,
            })
        })
        .collect();
    Some(Ring { legs: legs? })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::want::tests::{key, want};

    const USDC: &str = "nep141:usdc.near";
    const CREDITS: &str = "nep141:credits.near";
    const DATA: &str = "nep141:data.near";
    const NEAR: &str = "nep141:wrap.near";

    /// Alice has USDC and wants credits, Bob has credits and wants data, Carol has data and wants
    /// USDC, each asking a little less than the next one offers.
    fn three() -> [Want; 3] {
        [
            want(1, (USDC, 10_000_000), (CREDITS, 95_000)),
            want(2, (CREDITS, 100_000), (DATA, 4_900)),
            want(3, (DATA, 5_000), (USDC, 9_900_000)),
        ]
    }

    fn check(ring: &Ring, wants: &[Want], fee_pips: u32) {
        assert!(ring.is_sound(fee_pips), "{ring:?}");
        for leg in &ring.legs {
            let want = wants.iter().find(|w| w.maker == leg.maker).unwrap();
            assert_eq!(leg.is_within(want), Ok(()), "{leg:?}");
        }
    }

    #[test]
    fn finds_the_ring_of_three_that_no_pair_could_settle() {
        let wants = three();
        let rings = find_rings(&wants, 100);
        assert_eq!(rings.len(), 1);
        let ring = &rings[0];
        check(ring, &wants, 100);

        // In giving order from the smallest maker; Carol's key happens to sort first.
        let by_maker = |seed| ring.leg_of(&key(seed).account_id()).unwrap();
        // Everyone gives all they offered and receives all the previous one gave, less 0.01%.
        assert_eq!((by_maker(1).give_amount, by_maker(1).take_amount), (10_000_000, 99_990));
        assert_eq!((by_maker(2).give_amount, by_maker(2).take_amount), (100_000, 4_999));
        assert_eq!((by_maker(3).give_amount, by_maker(3).take_amount), (5_000, 9_999_000));
        for i in 0..3 {
            assert_eq!(ring.legs[i].give, ring.legs[(i + 1) % 3].take, "each gives to the next");
        }

        // No two of them make a ring.
        assert!(find_rings(&wants[..2], 100).is_empty());
        assert!(find_rings([&wants[0], &wants[2]], 100).is_empty());
    }

    // Every node must name and spell a ring the same way, whatever order it heard the wants in,
    // or two nodes would propose the same ring as two different ones.
    #[test]
    fn a_ring_has_one_spelling() {
        let wants = three();
        let forwards = find_rings(&wants, 100);
        let backwards = find_rings(wants.iter().rev(), 100);
        assert_eq!(forwards, backwards);
        assert_eq!(forwards[0].id(), backwards[0].id());
        assert_eq!(forwards[0].id().len(), 16);

        // A rotation of the same legs is not the ring.
        let mut rotated = forwards[0].clone();
        rotated.legs.rotate_left(1);
        assert!(!rotated.is_sound(100));
    }

    #[test]
    fn sizes_a_ring_to_its_smallest_want() {
        // Bob has only a fifth of the credits Alice would buy.
        let mut wants = three();
        (wants[1].give_amount, wants[1].take_amount) = (20_000, 980);
        let rings = find_rings(&wants, 100);
        let ring = &rings[0];
        check(ring, &wants, 100);
        let by_maker = |seed| ring.leg_of(&key(seed).account_id()).unwrap();
        // Bob gives all he has; Alice pays for what she gets at her own price, and so on round.
        assert_eq!(by_maker(2).give_amount, 20_000);
        assert_eq!(by_maker(1).take_amount, 19_998);
        assert!(by_maker(1).give_amount < 10_000_000 / 4);
        assert!(by_maker(3).give_amount < 5_000);
    }

    #[test]
    fn prices_that_do_not_close_make_no_ring() {
        let mut wants = three();
        // Carol now asks more USDC for her data than Alice pays for the credits it buys from Bob.
        // Scaling the ring down does not help: the prices multiply to more than one.
        wants[2].take_amount = 12_000_000;
        assert!(find_rings(&wants, 100).is_empty());

        // Prices that close exactly with no fee stop closing once there is one.
        let exact = [
            want(1, (USDC, 1_000_000), (NEAR, 250_000)),
            want(2, (NEAR, 250_000), (USDC, 1_000_000)),
        ];
        assert_eq!(find_rings(&exact, 0).len(), 1);
        assert!(find_rings(&exact, 100).is_empty());
    }

    #[test]
    fn two_wants_that_mirror_each_other_are_a_ring_of_two() {
        let wants = [
            want(1, (USDC, 5_000_000), (NEAR, 1_000_000)),
            want(2, (NEAR, 1_100_000), (USDC, 4_900_000)),
        ];
        let rings = find_rings(&wants, 100);
        assert_eq!(rings.len(), 1);
        check(&rings[0], &wants, 100);
        assert_eq!(rings[0].legs.len(), 2);
    }

    // 1 NEAR is 10^24 units; two such amounts multiplied do not fit in 128 bits. An overflow here
    // would silently make every ring that touches NEAR unfindable.
    #[test]
    fn sizes_rings_in_yoctonear() {
        let near = 10u128.pow(24);
        let wants = [
            want(1, (USDC, 10_000_000), (NEAR, 4 * near)),
            want(2, (NEAR, 5 * near), (USDC, 9_000_000)),
        ];
        let rings = find_rings(&wants, 100);
        assert_eq!(rings.len(), 1);
        check(&rings[0], &wants, 100);
        assert_eq!(rings[0].leg_of(&key(2).account_id()).unwrap().give_amount, 5 * near);
    }

    #[test]
    fn a_maker_appears_in_a_ring_once() {
        // Alice on both sides of her own trade is not a market.
        let mut other = want(1, (CREDITS, 100_000), (USDC, 9_000_000));
        other.id = 2;
        let wants = [want(1, (USDC, 10_000_000), (CREDITS, 95_000)), other];
        assert!(find_rings(&wants, 100).is_empty());
    }

    #[test]
    fn closed_wants_take_no_part() {
        let mut wants = three();
        wants[1].give_amount = 0;
        assert!(find_rings(&wants, 100).is_empty());
    }

    // What a maker checks before signing a leg someone else proposed. Each line is a way to take
    // more from the maker, or give it less, than it announced.
    #[test]
    fn a_leg_must_stay_within_the_want_it_fills() {
        let wants = three();
        let alice = &wants[0];
        let ring = find_rings(&wants, 100).remove(0);
        let leg = ring.leg_of(&alice.maker).unwrap().clone();
        assert_eq!(leg.is_within(alice), Ok(()));

        let changed = |change: fn(&mut Leg)| {
            let mut leg = leg.clone();
            change(&mut leg);
            leg.is_within(&three()[0]).unwrap_err()
        };
        assert_eq!(changed(|l| l.give_amount += 1), "gives more than offered");
        assert_eq!(changed(|l| l.take_amount = 94_999), "takes less than asked");
        assert_eq!(changed(|l| l.take = DATA.into()), "wrong tokens");
        assert_eq!(changed(|l| l.want_rev = 1), "the want has changed");
        assert_eq!(changed(|l| l.want_id = 9), "not this want");
        assert_eq!(changed(|l| l.take_amount = 0), "empty leg");
        // Half the size at the same price is fine: wants fill in parts.
        let mut half = leg.clone();
        (half.give_amount, half.take_amount) = (5_000_000, 47_500);
        assert_eq!(half.is_within(alice), Ok(()));
        half.take_amount -= 1;
        assert_eq!(half.is_within(alice), Err("takes less than asked"));
    }

    // The proposer holds every signature before anything settles, and a maker holding a bundle
    // may submit it. Both must know that each signature is for this ring and nothing else.
    #[test]
    fn a_signature_counts_only_for_its_own_leg() {
        use chrono::{TimeZone, Utc};
        let wants = three();
        let ring = find_rings(&wants, 100).remove(0);
        let deadline_ms = 1_800_000_020_000u64;
        let deadline = Utc.timestamp_millis_opt(deadline_ms as i64).unwrap();
        let seed_of = |leg: &Leg| (1..=3).find(|seed| key(*seed).account_id() == leg.maker).unwrap();
        let sign = |leg: &Leg, by: u8, verifier: &str, at| {
            let intent = leg.diff().unwrap().with_memo("ring").to_intent();
            key(by).sign_intents(verifier, at, &intents::nonce::plain(), vec![intent])
        };
        let first = &ring.legs[0];
        let own = seed_of(first);
        let good = sign(first, own, "intents.near", deadline);
        assert_eq!(ring.check_signed(0, &good, "intents.near", deadline_ms), Ok(()));

        let check = |signed: &SignedIntent| ring.check_signed(0, signed, "intents.near", deadline_ms).unwrap_err();
        assert_eq!(
            ring.check_signed(1, &good, "intents.near", deadline_ms).unwrap_err(),
            "signed by someone else"
        );
        assert_eq!(
            check(&sign(first, own, "intents.devnet", deadline)),
            "signed for another verifier"
        );
        assert_eq!(
            check(&sign(
                first,
                own,
                "intents.near",
                deadline + chrono::Duration::seconds(60)
            )),
            "wrong deadline"
        );
        let mut richer = first.clone();
        richer.take_amount += 1;
        assert_eq!(
            check(&sign(&richer, own, "intents.near", deadline)),
            "signed amounts differ from the leg"
        );
        let mut tampered = good.clone();
        tampered.payload.message = tampered.payload.message.replace("ring", "gnir");
        assert_eq!(check(&tampered), "invalid signature");
        let two = {
            let intent = first.diff().unwrap().to_intent();
            key(own).sign_intents(
                "intents.near",
                deadline,
                &intents::nonce::plain(),
                vec![intent.clone(), intent],
            )
        };
        assert_eq!(check(&two), "not exactly one intent");
    }

    // Four makers, each wanting what the next has: the longest ring a node will propose.
    #[test]
    fn finds_rings_of_four_and_stops_there() {
        let chain = |tokens: &[&str]| -> Vec<Want> {
            (0..tokens.len())
                .map(|i| {
                    want(
                        i as u8 + 1,
                        (tokens[i], 1_000_000),
                        (tokens[(i + 1) % tokens.len()], 900_000),
                    )
                })
                .collect()
        };
        let four = chain(&["nep141:a", "nep141:b", "nep141:c", "nep141:d"]);
        let rings = find_rings(&four, 100);
        assert_eq!(rings.len(), 1);
        check(&rings[0], &four, 100);
        assert_eq!(rings[0].legs.len(), 4);

        let five = chain(&["nep141:a", "nep141:b", "nep141:c", "nep141:d", "nep141:e"]);
        assert!(find_rings(&five, 100).is_empty());
    }
}
