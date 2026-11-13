//! `ft_withdraw`: "send this much of a fungible token out of the verifier to an account on the
//! token's own contract". The only way tokens leave the verifier for an agent that holds no other
//! key. It stands outside the netting of `token_diff`: nothing is received in return.
use serde_json::{json, Value};

use crate::{AccountId, TokenId};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FtWithdraw {
    /// The token's contract, e.g. `usdt.tether-token.near`: the verifier's `nep141:` id without
    /// its prefix.
    pub token: AccountId,
    /// Who the token's contract pays. It must be registered there, or the transfer fails and the
    /// verifier puts the amount back.
    pub receiver_id: AccountId,
    pub amount: u128,
    pub memo: Option<String>,
}

impl FtWithdraw {
    /// `amount` of `token`, as the verifier names it (`nep141:<contract>`), to `receiver_id`.
    /// `None` for anything but a NEP-141 token: the others leave by other intents.
    pub fn new(token: &str, receiver_id: impl Into<AccountId>, amount: u128) -> Option<Self> {
        Some(Self {
            token: token.strip_prefix("nep141:")?.into(),
            receiver_id: receiver_id.into(),
            amount,
            memo: None,
        })
    }

    pub fn with_memo(mut self, memo: impl Into<String>) -> Self {
        self.memo = Some(memo.into());
        self
    }

    /// The balance it is taken from, as the verifier names it.
    pub fn token_id(&self) -> TokenId {
        format!("nep141:{}", self.token)
    }

    /// The intent as it goes into a signed message.
    pub fn to_intent(&self) -> Value {
        let mut intent = json!({
            "intent": "ft_withdraw",
            "token": self.token,
            "receiver_id": self.receiver_id,
            "amount": self.amount.to_string(),
        });
        if let Some(memo) = &self.memo {
            intent["memo"] = json!(memo);
        }
        intent
    }

    /// Reads an `ft_withdraw` out of a message's intent; `None` for any other kind, and for one
    /// that also asks for `ft_transfer_call` (`msg`) or a storage deposit, which take more from
    /// the signer than `amount` and which the ledger does not model.
    pub fn from_intent(intent: &Value) -> Option<Self> {
        let fields = intent.as_object()?;
        if intent["intent"] != "ft_withdraw" || fields.contains_key("msg") || fields.contains_key("storage_deposit") {
            return None;
        }
        Some(Self {
            token: intent["token"].as_str()?.into(),
            receiver_id: intent["receiver_id"].as_str()?.into(),
            amount: intent["amount"].as_str()?.parse().ok()?,
            memo: match intent.get("memo") {
                None => None,
                Some(memo) => Some(memo.as_str()?.into()),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The contract reads the amount as decimal text and the token as a bare contract account. A
    // number, or the `nep141:` prefix left on, is "JSON: ..." on chain and the tokens stay put.
    #[test]
    fn writes_the_intent_as_the_verifier_reads_it() {
        let withdraw = FtWithdraw::new("nep141:usdt.tether-token.near", "me.near", 100_000)
            .unwrap()
            .with_memo("gossipmarket withdraw");
        let intent = withdraw.to_intent();
        assert_eq!(
            intent,
            json!({
                "intent": "ft_withdraw",
                "token": "usdt.tether-token.near",
                "receiver_id": "me.near",
                "amount": "100000",
                "memo": "gossipmarket withdraw",
            })
        );
        assert_eq!(withdraw.token_id(), "nep141:usdt.tether-token.near");
        assert_eq!(FtWithdraw::from_intent(&intent), Some(withdraw));
    }

    #[test]
    fn only_a_plain_withdraw_of_a_fungible_token_is_read() {
        assert_eq!(FtWithdraw::new("nep171:nft.near:1", "me.near", 1), None);
        assert_eq!(FtWithdraw::new("usdc.near", "me.near", 1), None);

        let plain = FtWithdraw::new("nep141:usdc.near", "me.near", 5).unwrap().to_intent();
        // Each of these takes something besides `amount`, so the ledger must not settle it as if
        // it did not.
        for (field, value) in [("msg", json!("")), ("storage_deposit", json!("1250000000000000000000"))] {
            let mut more = plain.clone();
            more[field] = value;
            assert_eq!(FtWithdraw::from_intent(&more), None, "{field}");
        }
        assert_eq!(
            FtWithdraw::from_intent(&json!({ "intent": "token_diff", "diff": {} })),
            None
        );
    }
}
