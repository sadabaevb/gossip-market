//! Rings: sets of wants in which each maker gives its token to the next and the last gives to the
//! first. Two wants that mirror each other are a ring of two; three makers none of whom wants what
//! the next one has are a ring of three, which no matcher of pairs can settle.
//!
//! The verifier settles a ring only if every token nets to zero after its fee, so sizing one is
//! arithmetic against the contract's rounding. `conformance` checks that arithmetic against the
//! contract's own engine.
use std::collections::{BTreeMap, HashMap};

use intents::{net_of_fee, AccountId, SignedIntent, TokenDiff, TokenId};
use primitive_types::{U256, U512};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::want::as_text;
use crate::Want;

/// Makers in one ring. Each one adds a signature to collect and a way for the ring to fall apart.
pub const MAX_RING: usize = 4;
/// Rings one search returns.
const MAX_RINGS: usize = 64;
/// Wants a search looks at per starting want and ring length, to bound a crowded book.
const MAX_STEPS: usize = 20_000;
/// Steps a cycle costs when it is sized: sizing it and naming the ring take about as long as
/// looking at a hundred wants. Without this, a book in which most cycles close would have a node
/// size tens of thousands of rings every time it looked.
const SIZE_STEPS: usize = 100;

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

/// Shortest first, then by id. Written once: `first_rings_of` compares rings by the order `search`
/// lists them in, and makers who ordered rings differently would each propose a different one.
fn order(ring: &Ring) -> (usize, String) {
    (ring.legs.len(), ring.id())
}

/// Every ring the open wants allow at this fee, sized as large as the wants permit. Shortest
/// first, then by id: every node lists them in the same order.
pub fn find_rings<'a>(wants: impl IntoIterator<Item = &'a Want>, fee_pips: u32) -> Vec<Ring> {
    let index = Index::new(wants);
    let starts: Vec<usize> = (0..index.wants.len()).collect();
    index.search(&starts, true, fee_pips, MAX_RING)
}

/// The rings `maker` is part of, in the order `find_rings` lists rings, from one search over the
/// maker's own wants. That costs its own handful of wants rather than the whole book, and rings it
/// is not in cannot crowd its own out of the first `MAX_RINGS`, though rings through one of its
/// wants can crowd out the others': `first_rings_of` searches want by want.
pub fn rings_of<'a>(wants: impl IntoIterator<Item = &'a Want>, fee_pips: u32, maker: &str) -> Vec<Ring> {
    let index = Index::new(wants);
    index.search(&index.of(maker), false, fee_pips, MAX_RING)
}

/// The rings `maker` is part of that come first, in the order `find_rings` lists rings, for every
/// want in them: no ring the search finds through any of their wants comes before them.
/// `passed_over` rings are left out of the comparison, as if they were not there: the ones a node
/// has seen fall through, and those it cannot pay for.
///
/// What a node proposes from. Where the search sees every ring, two of these never share a want,
/// so makers who each propose only these do not lock each other's wants into rings that cannot all
/// settle; and the first ring in the whole book is always one of them, so a crowded market still
/// settles a ring at a time at the least. Proposing from every ring through the node instead, a
/// market of a hundred agents whose wants overlapped settled ever fewer rings until it stopped.
///
/// Where the book is crowded enough to exhaust the search, the search from one want can miss a
/// ring that the search from another finds. The smallest ring any search finds then still comes
/// first for whoever found it, so some ring is always proposed, though two may share a want.
pub fn first_rings_of<'a>(
    wants: impl IntoIterator<Item = &'a Want>,
    fee_pips: u32,
    maker: &str,
    passed_over: impl Fn(&Ring) -> bool,
) -> Vec<Ring> {
    let index = Index::new(wants);
    // By (position, longest ring), the first ring found from that want: rings of the maker's that
    // share a counterparty's want ask the same question of it.
    let mut first_from: BTreeMap<(usize, usize), Option<(usize, String)>> = BTreeMap::new();
    let mut first = Vec::new();
    // From each of the maker's wants on its own. Searched together, the rings through one want
    // that is in many could fill the search, and those through the others would go unfound.
    for at in index.of(maker) {
        // The first ring through a want of the maker's is the only one that can come first for it.
        let rings = index.search(&[at], false, fee_pips, MAX_RING);
        let Some(ring) = rings.into_iter().find(|ring| !passed_over(ring)) else {
            continue;
        };
        let first_everywhere = ring.legs.iter().filter(|leg| leg.maker != maker).all(|leg| {
            // A ring that comes first is no longer than this one.
            let Some(at) = index.position(&leg.maker, leg.want_id) else {
                return false;
            };
            // Not "the first found is this ring": a search that ran out of steps may not find it,
            // and then no ring was first for every want and nobody proposed anything, as rings of
            // four were left in a market of a hundred.
            first_from
                .entry((at, ring.legs.len()))
                .or_insert_with(|| {
                    let rings = index.search(&[at], false, fee_pips, ring.legs.len());
                    rings
                        .into_iter()
                        .find(|other| !passed_over(other))
                        .map(|other| order(&other))
                })
                .as_ref()
                .is_none_or(|other| *other >= order(&ring))
        });
        if first_everywhere {
            first.push(ring);
        }
    }
    first.sort_by_cached_key(order);
    first
}

/// The open wants in (maker, id) order, so that comparing positions compares keys, with makers
/// and tokens numbered so that the search compares integers rather than account names.
struct Index<'a> {
    wants: Vec<&'a Want>,
    makers: Vec<u32>,
    gives: Vec<u32>,
    takes: Vec<u32>,
    /// By token, the positions of the wants that take it, in order: the ways a path can go on.
    taking: Vec<Vec<usize>>,
    /// By (token taken, token given), the positions of those wants, best price for the ring first:
    /// the want that closes a path is looked up, and the first one whose price cannot close it
    /// ends the lookup.
    closing: HashMap<(u32, u32), Vec<usize>>,
}

impl<'a> Index<'a> {
    /// The positions of `maker`'s wants.
    fn of(&self, maker: &str) -> Vec<usize> {
        (0..self.wants.len())
            .filter(|&i| self.wants[i].maker == maker)
            .collect()
    }

    fn new(wants: impl IntoIterator<Item = &'a Want>) -> Self {
        let mut wants: Vec<&Want> = wants.into_iter().filter(|want| want.is_open()).collect();
        wants.sort_by(|a, b| (&a.maker, a.id).cmp(&(&b.maker, b.id)));

        fn number<'a>(names: &mut HashMap<&'a str, u32>, name: &'a str) -> u32 {
            let next = names.len() as u32;
            *names.entry(name).or_insert(next)
        }
        let (mut makers, mut tokens) = (HashMap::new(), HashMap::new());
        let mut index = Index {
            makers: wants.iter().map(|want| number(&mut makers, &want.maker)).collect(),
            gives: wants.iter().map(|want| number(&mut tokens, &want.give)).collect(),
            takes: wants.iter().map(|want| number(&mut tokens, &want.take)).collect(),
            taking: vec![Vec::new(); tokens.len()],
            closing: HashMap::new(),
            wants,
        };
        for i in 0..index.wants.len() {
            index.taking[index.takes[i] as usize].push(i);
            index
                .closing
                .entry((index.takes[i], index.gives[i]))
                .or_default()
                .push(i);
        }
        let wants = &index.wants;
        for list in index.closing.values_mut() {
            // More given per unit taken comes first: give_a / take_a > give_b / take_b, multiplied
            // out. Ties stay in key order.
            list.sort_by(|&a, &b| {
                let a_first = U256::from(wants[a].give_amount) * U256::from(wants[b].take_amount);
                let b_first = U256::from(wants[b].give_amount) * U256::from(wants[a].take_amount);
                b_first.cmp(&a_first)
            });
        }
        index
    }

    /// Where the want `id` of `maker` is, if it is open.
    fn position(&self, maker: &str, id: u64) -> Option<usize> {
        self.wants
            .binary_search_by(|want| (want.maker.as_str(), want.id).cmp(&(maker, id)))
            .ok()
    }

    /// The rings of up to `longest` makers through the wants at `starts`, at most `MAX_RINGS`,
    /// shortest first and then by id. With `from_smallest`, a ring is only looked for from its
    /// smallest want, so that each is found once when every want is a start.
    fn search(&self, starts: &[usize], from_smallest: bool, fee_pips: u32, longest: usize) -> Vec<Ring> {
        let mut rings = Vec::new();
        // All rings of one length before any of the next: a crowded book spends its steps on the
        // rings with the fewest signatures to collect, and once there are `MAX_RINGS`, every
        // longer ring would be cut anyway.
        for len in 2..=longest.min(MAX_RING) {
            if rings.len() >= MAX_RINGS {
                break;
            }
            for &start in starts {
                let mut steps = 0;
                self.walk(&mut vec![start], len, from_smallest, &mut steps, &mut |path| {
                    // The one spelling starts from the smallest want.
                    let mut cycle: Vec<&Want> = path.iter().map(|&i| self.wants[i]).collect();
                    let smallest = (0..path.len()).min_by_key(|&k| path[k]).unwrap_or(0);
                    cycle.rotate_left(smallest);
                    rings.extend(size(&cycle, fee_pips));
                });
            }
        }
        rings.sort_by_cached_key(order);
        rings.truncate(MAX_RINGS);
        rings
    }

    /// Grows `path` by wants that take what its last want gives until it is one short of `len`,
    /// then closes it with the wants that give what its first one takes.
    fn walk(
        &self,
        path: &mut Vec<usize>,
        len: usize,
        from_smallest: bool,
        steps: &mut usize,
        found: &mut impl FnMut(&[usize]),
    ) {
        let (first, last) = (path[0], path[path.len() - 1]);
        if path.len() + 1 == len {
            let Some(closing) = self.closing.get(&(self.gives[last], self.takes[first])) else {
                return;
            };
            // A ring can only net if what its makers give, multiplied together, is at least what
            // they take: each gives at most what it received times its own price. Sizing would
            // find the same, in up to 64 laps of arithmetic per want.
            let product = |amount: fn(&Want) -> u128| {
                path.iter()
                    .fold(U512::one(), |product, &i| product * U512::from(amount(self.wants[i])))
            };
            let (given, taken) = (product(|want| want.give_amount), product(|want| want.take_amount));
            for &next in closing {
                *steps += 1;
                if *steps > MAX_STEPS {
                    return;
                }
                let want = self.wants[next];
                if given * U512::from(want.give_amount) < taken * U512::from(want.take_amount) {
                    // Every want after this one in the list prices the ring worse.
                    return;
                }
                if self.fits(path, next, from_smallest) {
                    path.push(next);
                    found(path);
                    path.pop();
                    *steps += SIZE_STEPS;
                }
            }
            return;
        }
        let taking = &self.taking[self.gives[last] as usize];
        // In key order, so the wants smaller than the first are skipped in one go.
        let skip = if from_smallest {
            taking.partition_point(|&i| i <= first)
        } else {
            0
        };
        for &next in &taking[skip..] {
            *steps += 1;
            if *steps > MAX_STEPS {
                return;
            }
            if self.fits(path, next, from_smallest) {
                path.push(next);
                self.walk(path, len, from_smallest, steps, found);
                path.pop();
            }
        }
    }

    /// Whether the want at `next` can join `path`: by a maker not already in it and, when rings
    /// are searched from their smallest want, after the first.
    fn fits(&self, path: &[usize], next: usize, from_smallest: bool) -> bool {
        (!from_smallest || next > path[0]) && path.iter().all(|&i| self.makers[i] != self.makers[next])
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
    use std::collections::BTreeSet;

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

    /// A want by any of millions of makers, which `want` with its byte of seed cannot name.
    fn want_by(maker: u64, id: u64, give: (&str, u128), take: (&str, u128)) -> Want {
        Want {
            maker: format!("{maker:064x}"),
            id,
            ..want(1, give, take)
        }
    }

    /// Every ring, found the slow way: each ordering of up to four wants by different makers in
    /// which every want gives what the next takes, kept only when it starts from its smallest want.
    fn every_ring(wants: &[Want], fee_pips: u32) -> Vec<Ring> {
        fn grow<'a>(open: &[&'a Want], path: &mut Vec<&'a Want>, fee_pips: u32, rings: &mut Vec<Ring>) {
            let n = path.len();
            let closes = (0..n).all(|i| path[i].give == path[(i + 1) % n].take);
            let smallest = path
                .iter()
                .skip(1)
                .all(|w| (&w.maker, w.id) > (&path[0].maker, path[0].id));
            if n >= 2 && closes && smallest {
                rings.extend(size(path, fee_pips));
            }
            if n == MAX_RING {
                return;
            }
            for want in open {
                if path.iter().all(|w| w.maker != want.maker) {
                    path.push(want);
                    grow(open, path, fee_pips, rings);
                    path.pop();
                }
            }
        }
        let open: Vec<&Want> = wants.iter().filter(|want| want.is_open()).collect();
        let mut rings = Vec::new();
        grow(&open, &mut Vec::new(), fee_pips, &mut rings);
        rings.sort_by_cached_key(|ring| (ring.legs.len(), ring.id()));
        rings
    }

    /// 300 small books by five makers over four tokens, each with its fee: prices within a few
    /// percent of each other, so that some rings close and some only nearly do. The same books
    /// every run, so that a failure is the same failure.
    fn random_books() -> Vec<(Vec<Want>, u32)> {
        // splitmix64
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = |below: u64| {
            state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            (z ^ (z >> 31)) % below
        };
        let tokens = ["nep141:a", "nep141:b", "nep141:c", "nep141:d"];
        (0..300)
            .map(|_| {
                let wants: Vec<Want> = (0..4 + next(11))
                    .map(|id| {
                        let give = next(4) as usize;
                        let take = (give + 1 + next(3) as usize) % 4;
                        want_by(
                            next(5),
                            id,
                            (tokens[give], 900 + next(200) as u128),
                            (tokens[take], 900 + next(200) as u128),
                        )
                    })
                    .collect();
                (wants, [0, 100, 10_000][next(3) as usize])
            })
            .collect()
    }

    // The search skips most of the book: it follows tokens, looks the closing want up, and stops
    // at the first price that cannot close. If any of that skipped a ring that is there, or found
    // one twice, a node would miss trades or propose one ring under two names.
    #[test]
    fn the_search_finds_what_trying_every_ordering_finds() {
        let (mut rings, mut longest) = (0, 0);
        for (wants, fee_pips) in random_books() {
            let expected = every_ring(&wants, fee_pips);
            assert!(expected.len() < MAX_RINGS, "a book this small should not need cutting");
            assert_eq!(find_rings(&wants, fee_pips), expected, "{wants:#?}");
            for maker in 0..5 {
                let maker = format!("{maker:064x}");
                let theirs: Vec<Ring> = expected
                    .iter()
                    .filter(|r| r.leg_of(&maker).is_some())
                    .cloned()
                    .collect();
                assert_eq!(rings_of(&wants, fee_pips, &maker), theirs, "{maker} in {wants:#?}");
            }
            rings += expected.len();
            longest = longest.max(expected.iter().map(|ring| ring.legs.len()).max().unwrap_or(0));
        }
        // Otherwise the books above would prove nothing.
        assert!(
            rings > 300 && longest == MAX_RING,
            "{rings} rings, the longest of {longest}"
        );
    }

    // A maker's wants are searched one at a time. Searched together, a want in more rings of two
    // than one search returns filled it, and a ring of three through the maker's other want was
    // never found, so never proposed.
    #[test]
    fn a_want_in_many_rings_does_not_crowd_out_the_makers_other_wants() {
        // Maker 0 gives a for b, mirrored by 70 others; and gives c for d in one ring of three.
        let mut wants = vec![
            want_by(0, 1, ("nep141:a", 1_000_000), ("nep141:b", 990_000)),
            want_by(0, 2, ("nep141:c", 1_000_000), ("nep141:d", 990_000)),
            want_by(1, 1, ("nep141:d", 1_000_000), ("nep141:e", 990_000)),
            want_by(2, 1, ("nep141:e", 1_000_000), ("nep141:c", 990_000)),
        ];
        wants.extend((3..73u64).map(|maker| want_by(maker, 1, ("nep141:b", 1_000_000), ("nep141:a", 990_000))));
        // Otherwise the rings of two fit in one search and this proves nothing.
        const { assert!(MAX_RINGS < 70, "70 rings of two fit in one search") };

        let first = first_rings_of(&wants, 1, &format!("{:064x}", 0), |_| false);
        assert!(
            first.iter().any(|ring| ring.legs.len() == 3),
            "the ring of three is not among {first:?}"
        );
        assert!(first.iter().any(|ring| ring.legs.len() == 2), "nor is a ring of two");
    }

    // Where rings are too many for the search to see them all, the search from one want misses
    // rings that the search from another finds. When a ring had to be the first found from every
    // one of its wants, no ring was, and a market of a hundred stopped with rings of four open and
    // nobody proposing any of them.
    #[test]
    fn a_book_too_crowded_to_search_whole_still_has_a_ring_someone_proposes() {
        // Wants only go a -> b -> c -> d -> a, eight on each step, so every ring has four legs, and
        // everyone asks 1% less than it gives, so every cycle of them closes.
        let tokens = ["nep141:a", "nep141:b", "nep141:c", "nep141:d"];
        let wants: Vec<Want> = (0..32u64)
            .map(|maker| {
                let step = (maker / 8) as usize;
                want_by(maker, 0, (tokens[step], 1_000_000), (tokens[(step + 3) % 4], 990_000))
            })
            .collect();
        // Otherwise the search would see every ring and the book would prove nothing.
        const {
            assert!(
                MAX_STEPS / SIZE_STEPS < 8 * 8 * 8,
                "512 rings through each want fit the search"
            )
        };
        let proposing = (0..32u64)
            .filter(|maker| !first_rings_of(&wants, 1, &format!("{maker:064x}"), |_| false).is_empty())
            .count();
        assert!(proposing > 0, "nobody proposes any ring");
    }

    // A node proposes only the rings that come first for each of their wants. If two such rings
    // could share a want, two proposers would lock it into rings that cannot both settle, which
    // is how a crowded market ground to a halt; if the makers of a ring disagreed on whether it
    // comes first, it would wait on a proposer who never proposes it; and if the first ring of
    // the book were not among them, nothing would settle.
    #[test]
    fn rings_that_come_first_never_share_a_want_and_the_first_ring_is_one() {
        let mut proposed_in_all = 0;
        for (wants, fee_pips) in random_books() {
            let every = every_ring(&wants, fee_pips);
            let mut proposed: BTreeMap<String, Ring> = BTreeMap::new();
            for maker in (0..5).map(|maker| format!("{maker:064x}")) {
                for ring in first_rings_of(&wants, fee_pips, &maker, |_| false) {
                    for leg in &ring.legs {
                        let theirs = first_rings_of(&wants, fee_pips, &leg.maker, |_| false);
                        assert!(theirs.contains(&ring), "{} does not see {ring:?} come first", leg.maker);
                    }
                    proposed.insert(ring.id(), ring);
                }
            }
            let mut used = BTreeSet::new();
            for ring in proposed.values() {
                for leg in &ring.legs {
                    assert!(
                        used.insert((&leg.maker, leg.want_id)),
                        "two rings share a want: {proposed:#?}"
                    );
                }
            }
            if let Some(best) = every.first() {
                assert!(
                    proposed.contains_key(&best.id()),
                    "the first ring is not proposed: {wants:#?}"
                );
            }
            // A ring the node saw fall through steps aside for the next.
            if let [best, next, ..] = &every[..] {
                let maker = &next.legs[0].maker;
                let passed_over = |ring: &Ring| ring == best;
                let without = first_rings_of(&wants, fee_pips, maker, passed_over);
                assert!(!without.contains(best) && without.contains(next), "{wants:#?}");
            }
            proposed_in_all += proposed.len();
        }
        assert!(proposed_in_all > 300, "{proposed_in_all} rings proposed in all");
    }

    // Wants that chain into one another but never close, and one ring of three among them.
    // Searching the whole book at every step, the search ran out of steps on chains of four before
    // it tried the ring, and missed it in any book of a hundred wants or more.
    #[test]
    fn a_ring_is_found_however_many_wants_surround_it() {
        let tokens = ["a", "b", "c", "d", "e", "f", "g", "h"].map(|t| format!("nep141:{t}"));
        let book = |noise: u64| -> Vec<Want> {
            let mut wants: Vec<Want> = (0..noise)
                .map(|n| {
                    let give = &tokens[(n % 8) as usize];
                    let take = &tokens[((n / 8 + n + 1) % 8) as usize];
                    let take = if take == give {
                        &tokens[((n + 1) % 8) as usize]
                    } else {
                        take
                    };
                    // Asks twice what it gives: no chain of these ever nets.
                    want_by(100 + n / 4, n % 4, (give, 1_000_000), (take, 2_000_000))
                })
                .collect();
            for (maker, give, take) in [(1, 0, 2), (2, 1, 0), (3, 2, 1)] {
                wants.push(want_by(maker, 0, (&tokens[give], 1_000_000), (&tokens[take], 990_000)));
            }
            wants
        };
        let wants = book(300);
        let rings = find_rings(&wants, 100);
        assert_eq!(rings.len(), 1);
        assert_eq!(rings[0].legs.len(), 3);
        for maker in 1..=3 {
            assert_eq!(rings_of(&wants, 100, &format!("{maker:064x}")), rings);
        }
        // What a node searches, in a book as full as a node lets it get.
        let full = book(crate::book::MAX_WANTS as u64 - 3);
        assert_eq!(rings_of(&full, 100, &format!("{:064x}", 1)), rings);
    }

    // A node proposes only rings it is part of. Cut to the first 64 of the whole book, a busy
    // market's rings of two would leave a node's ring of three off its list, and it would never
    // propose it or sign for it in its turn.
    #[test]
    fn a_node_sees_its_own_rings_however_many_others_there_are() {
        // A hundred pairs trading USDC for NEAR, and this node in a ring of three on other tokens.
        let mut wants: Vec<Want> = (0..100u64)
            .flat_map(|pair| {
                [
                    want_by(100 + 2 * pair, 0, (USDC, 1_000_000), (NEAR, 990_000)),
                    want_by(101 + 2 * pair, 0, (NEAR, 1_000_000), (USDC, 990_000)),
                ]
            })
            .collect();
        let x = "nep141:x.near";
        wants.push(want_by(1, 0, (CREDITS, 1_000_000), (DATA, 990_000)));
        wants.push(want_by(2, 0, (DATA, 1_000_000), (x, 990_000)));
        wants.push(want_by(3, 0, (x, 1_000_000), (CREDITS, 990_000)));
        let me = format!("{:064x}", 1);

        let everyone = find_rings(&wants, 100);
        assert_eq!(everyone.len(), MAX_RINGS);
        assert!(everyone.iter().all(|ring| ring.legs.len() == 2));

        let mine = rings_of(&wants, 100, &me);
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].legs.len(), 3);
        assert!(mine[0].leg_of(&me).is_some() && mine[0].is_sound(100));
        assert!(!everyone.contains(&mine[0]));
    }
}
