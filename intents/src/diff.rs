//! `token_diff`: "change my balances by these amounts". The verifier settles a batch of them only
//! if, token by token, everything given equals everything received plus the fee.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::TokenId;

/// One pip is a millionth: the verifier's fee unit.
pub const PIPS: u128 = 1_000_000;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct TokenDiff {
    /// Token -> signed change, written as decimal text. Negative is given, positive is received.
    #[serde(with = "as_text")]
    pub diff: BTreeMap<TokenId, i128>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memo: Option<String>,
}

impl TokenDiff {
    /// Give `give.1` of one token for `take.1` of another.
    pub fn swap(give: (&str, u128), take: (&str, u128)) -> Option<Self> {
        let given = i128::try_from(give.1).ok()?.checked_neg()?;
        let taken = i128::try_from(take.1).ok()?;
        if give.0 == take.0 {
            return None;
        }
        Some(Self {
            diff: BTreeMap::from([(give.0.to_owned(), given), (take.0.to_owned(), taken)]),
            memo: None,
        })
    }

    pub fn with_memo(mut self, memo: impl Into<String>) -> Self {
        self.memo = Some(memo.into());
        self
    }

    /// The intent as it goes into a signed message.
    pub fn to_intent(&self) -> Value {
        let mut intent = serde_json::to_value(self).expect("a token_diff is plain JSON");
        intent["intent"] = json!("token_diff");
        intent
    }

    /// Reads a `token_diff` out of a message's intent; `None` for any other kind.
    pub fn from_intent(intent: &Value) -> Option<Self> {
        if intent["intent"] != "token_diff" {
            return None;
        }
        Self::deserialize(intent).ok()
    }
}

/// The fee rate the verifier applies to `amount` of `token`. Fungible tokens pay it; single NFTs
/// and single multi-token units do not, since a fee would round up to the whole thing.
fn token_fee(token: &str, amount: u128, fee_pips: u32) -> u32 {
    match token.split_once(':').map(|(standard, _)| standard) {
        Some("nep141") => fee_pips,
        Some("nep245" | "imt") if amount > 1 => fee_pips,
        _ => 0,
    }
}

/// What the verifier keeps of `amount` given: `amount * fee / 1_000_000`, rounded up.
pub fn fee_ceil(token: &str, amount: u128, fee_pips: u32) -> Option<u128> {
    let fee = u128::from(token_fee(token, amount, fee_pips));
    Some(amount.checked_mul(fee)?.div_ceil(PIPS))
}

/// What reaches the other side when `amount` is given.
pub fn net_of_fee(token: &str, amount: u128, fee_pips: u32) -> Option<u128> {
    amount.checked_sub(fee_ceil(token, amount, fee_pips)?)
}

/// What is left over, token by token, once every diff in the batch is applied and the fee on each
/// amount given is set aside: received plus fees, minus given. The verifier settles the batch only
/// if nothing is left over, so an empty map means it nets. `None` on overflow.
pub fn unmatched<'a>(diffs: impl IntoIterator<Item = &'a TokenDiff>, fee_pips: u32) -> Option<BTreeMap<TokenId, i128>> {
    let mut left: BTreeMap<TokenId, i128> = BTreeMap::new();
    for diff in diffs {
        for (token, delta) in &diff.diff {
            let mut change = *delta;
            if *delta < 0 {
                let fee = fee_ceil(token, delta.unsigned_abs(), fee_pips)?;
                change = change.checked_add(i128::try_from(fee).ok()?)?;
            }
            let total = left.entry(token.clone()).or_default();
            *total = total.checked_add(change)?;
        }
    }
    left.retain(|_, total| *total != 0);
    Some(left)
}

/// A map of amounts written as decimal text, the way the verifier reads and writes them.
pub(crate) mod as_text {
    use super::*;
    use serde::{de::Error, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(amounts: &BTreeMap<TokenId, i128>, s: S) -> Result<S::Ok, S::Error> {
        s.collect_map(amounts.iter().map(|(token, amount)| (token, amount.to_string())))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<BTreeMap<TokenId, i128>, D::Error> {
        BTreeMap::<TokenId, String>::deserialize(d)?
            .into_iter()
            .map(|(token, amount)| Ok((token, amount.parse().map_err(D::Error::custom)?)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const USDC: &str = "nep141:usdc.near";
    const CREDITS: &str = "nep141:credits.near";
    const DATA: &str = "nep141:data.near";

    // The contract takes `ceil(amount * fee / 1e6)` from each amount given. Rounding the other way
    // would leave a one-unit gap and every ring would fail with "invariant violated".
    #[test]
    fn the_fee_rounds_up_and_is_taken_from_what_is_given() {
        assert_eq!(fee_ceil(USDC, 10_000_000, 100), Some(1_000));
        assert_eq!(fee_ceil(USDC, 10_000_001, 100), Some(1_001));
        assert_eq!(fee_ceil(USDC, 1, 1), Some(1));
        assert_eq!(fee_ceil(USDC, 0, 100), Some(0));
        assert_eq!(net_of_fee(USDC, 10_000_000, 100), Some(9_999_000));
        assert_eq!(fee_ceil(USDC, u128::MAX, 100), None);
    }

    #[test]
    fn single_nfts_and_multi_token_units_pay_no_fee() {
        assert_eq!(fee_ceil("nep171:nft.near:1", 1, 100), Some(0));
        assert_eq!(fee_ceil("nep245:mt.near:a", 1, 100), Some(0));
        assert_eq!(fee_ceil("nep245:mt.near:a", 1_000_000, 100), Some(100));
    }

    // Three parties, none of whom wants what the next one has: the case a matcher of pairs cannot
    // settle. Each receives what the previous one gave, less the fee.
    #[test]
    fn a_ring_of_three_nets_only_when_each_side_receives_net_of_fee() {
        let fee = 100;
        let ring = |credits_to_a: u128, data_to_b: u128, usdc_to_c: u128| {
            [
                TokenDiff::swap((USDC, 10_000_000), (CREDITS, credits_to_a)).unwrap(),
                TokenDiff::swap((CREDITS, 100_000), (DATA, data_to_b)).unwrap(),
                TokenDiff::swap((DATA, 5_000), (USDC, usdc_to_c)).unwrap(),
            ]
        };
        assert_eq!(unmatched(&ring(99_990, 4_999, 9_999_000), fee), Some(BTreeMap::new()));

        // Ignoring the fee, as a first sketch of a ring does: three tokens short.
        let naive = unmatched(&ring(100_000, 5_000, 10_000_000), fee).unwrap();
        assert_eq!(
            naive,
            BTreeMap::from([(USDC.into(), 1_000), (CREDITS.into(), 10), (DATA.into(), 1)])
        );

        // With no fee the naive ring is exact.
        assert_eq!(unmatched(&ring(100_000, 5_000, 10_000_000), 0), Some(BTreeMap::new()));
    }

    // The verifier parses amounts from text and tags the intent's kind; a number or a missing tag
    // is "JSON: ..." on chain, after the gas is spent.
    #[test]
    fn writes_the_intent_as_the_verifier_reads_it() {
        let diff = TokenDiff::swap((USDC, 10), (CREDITS, 100))
            .unwrap()
            .with_memo("ring 1f");
        let intent = diff.to_intent();
        assert_eq!(
            intent,
            json!({ "intent": "token_diff", "diff": { CREDITS: "100", USDC: "-10" }, "memo": "ring 1f" })
        );
        assert_eq!(TokenDiff::from_intent(&intent), Some(diff));
        assert_eq!(
            TokenDiff::from_intent(&json!({ "intent": "transfer", "diff": {} })),
            None
        );
        assert!(TokenDiff::swap((USDC, 1), (USDC, 1)).is_none());
    }
}
