//! Taking an agent's tokens back out of the verifier. The agent signs one `ft_withdraw` intent, the
//! contract dry-runs it for free, and only then does the agent send it, paying the gas from its
//! own account. The token's contract then pays the receiver. If it will not, because the receiver
//! is not registered with it, the verifier puts the amount back: the balance afterwards says which.
use anyhow::{anyhow, bail, Context, Result};
use chain::Verifier;
use chrono::{DateTime, Duration, Utc};
use intents::{FtWithdraw, Keypair, SignedIntent, TokenId};

use crate::config::{NodeConfig, Tokens};

/// Long enough for one transaction to land, short enough that a signed withdraw that went
/// astray is dead soon.
const TTL: Duration = Duration::minutes(2);

#[derive(Debug)]
pub struct Withdrawn {
    pub account: String,
    pub token: TokenId,
    pub amount: u128,
    pub to: String,
    pub tx: String,
    /// The agent's balance of the token inside the verifier, before and after; after, why it
    /// could not be read instead, if it could not.
    pub before: u128,
    pub after: Result<u128, String>,
}

/// Withdraws `amount` of the token called `symbol` in the config, or all of it, to `to`.
pub async fn withdraw(config: &NodeConfig, symbol: &str, amount: Option<&str>, to: &str) -> Result<Withdrawn> {
    let key = config.keypair()?;
    let account = key.account_id();
    let tokens = config.tokens();
    let token = tokens
        .id(symbol)
        .ok_or_else(|| anyhow!("unknown token `{symbol}`; see [tokens] in the config"))?;
    let verifier = Verifier::new(config.rpc.clone(), config.verifier.clone());
    let held = || async {
        let balances = verifier.balances(&account, std::slice::from_ref(&token)).await;
        Ok::<_, anyhow::Error>(balances.context("reading the balance")?[0])
    };

    let before = held().await?;
    if before == 0 {
        bail!("{account} holds no {symbol} inside {}", config.verifier);
    }
    let amount = match amount {
        Some(text) => tokens.parse(&format!("{text} {symbol}"))?.1,
        None => before,
    };
    if amount == 0 {
        bail!("nothing to withdraw: the amount is zero");
    }
    let state = verifier.state().await.context("reading the verifier's salt")?;
    let deadline = Utc::now() + TTL;
    let nonce = config.nonce.nonce(state.salt, deadline);
    let signed = [sign(&key, &config.verifier, &token, to, amount, &nonce, deadline)
        .ok_or_else(|| anyhow!("{symbol} is not a NEP-141 token, the only kind this withdraws"))?];

    // A transaction that fails on chain still costs its gas; a dry run costs nothing.
    verifier
        .simulate(&signed)
        .await
        .context("the verifier's dry run turned it down, so nothing was sent")?;
    // Not retried here: a second try would be a second withdraw, and the first may have landed.
    let tx = verifier
        .execute(&key, &signed)
        .await
        .context("sending it; run `gossipmarket account` to see whether it went through before trying again")?;
    // It has been sent: from here on nothing may fail without the hash, or the operator, seeing
    // an error alone, would send it again.
    let after = held().await.map_err(|e| format!("{e:#}"));
    Ok(Withdrawn {
        account,
        token,
        amount,
        to: to.into(),
        tx,
        before,
        after,
    })
}

/// One `ft_withdraw` of `amount` of `token` to `to`, signed by `key`; `None` if the token is not
/// NEP-141. `preflight` signs its probe with this too, so that what it checks is what this sends.
pub fn sign(
    key: &Keypair,
    verifier: &str,
    token: &str,
    to: &str,
    amount: u128,
    nonce: &[u8; 32],
    deadline: DateTime<Utc>,
) -> Option<SignedIntent> {
    let intent = FtWithdraw::new(token, to, amount)?
        .with_memo("gossipmarket withdraw")
        .to_intent();
    Some(key.sign_intents(verifier, deadline, nonce, vec![intent]))
}

impl Withdrawn {
    /// What happened, for the operator.
    pub fn report(&self, tokens: &Tokens) -> Vec<String> {
        let show = |units| tokens.show(&self.token, units);
        let mut lines = vec![
            format!("withdrew {} from {} to {}", show(self.amount), self.account, self.to),
            format!("  tx {} sent by {}", self.tx, self.account),
        ];
        let after = match &self.after {
            Ok(after) => *after,
            Err(why) => {
                lines.push(format!(
                    "  inside the verifier: {} before; afterwards it could not be read: {why}",
                    show(self.before)
                ));
                lines.push("  => sent. Do not send it again: run `gossipmarket account` to see where it went".into());
                return lines;
            }
        };
        lines.push(format!(
            "  inside the verifier: {} before, {} after",
            show(self.before),
            show(after)
        ));
        let expected = self.before.saturating_sub(self.amount);
        lines.push(match after {
            after if after == expected => format!("  => {} is on its way to {}", show(self.amount), self.to),
            after if after == self.before => format!(
                "  => it came back: the token's contract refused to pay {}. Is that account registered with {}?",
                self.to,
                self.token.trim_start_matches("nep141:")
            ),
            _ => "  => the balance also moved for another reason: is a node running with this key?".into(),
        });
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::token_table;

    // Once a withdraw is sent, the hash is what tells the operator it went. Shown an error alone,
    // after a withdraw that landed, the operator would send a second one.
    #[test]
    fn a_withdraw_that_was_sent_reports_its_hash_even_when_the_balance_cannot_be_read() {
        let tokens = Tokens::from(token_table());
        let withdrawn = Withdrawn {
            account: "alice".into(),
            token: "nep141:usdc.devnet".into(),
            amount: 20_000_000,
            to: "alice.near".into(),
            tx: "4Tx".into(),
            before: 50_000_000,
            after: Err("reading the balance: no answer".into()),
        };
        let report = withdrawn.report(&tokens).join("\n");
        assert!(report.contains("tx 4Tx sent by alice"), "{report}");
        assert!(report.contains("Do not send it again"), "{report}");
    }
}
