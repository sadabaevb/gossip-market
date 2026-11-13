//! Checking a settlement without taking anyone's word for it: fetch the transaction from the
//! chain, check every signature in it, and add the legs up again.
use anyhow::{bail, Context, Result};
use chain::Verifier;
use intents::{AccountId, SignedIntent, TokenDiff};
use serde::Deserialize;

use crate::config::{pips, Tokens};

/// A settlement as read back from the chain and recomputed here.
#[derive(Debug, Clone, PartialEq)]
pub struct Checked {
    pub tx: String,
    pub submitter: AccountId,
    /// Each signer and what it signed for.
    pub legs: Vec<(AccountId, TokenDiff)>,
    pub fee_pips: u32,
}

impl Checked {
    /// The lines a person reads.
    pub fn report(&self, tokens: &Tokens) -> Vec<String> {
        let mut lines = vec![format!(
            "tx {} sent by {}: one execute_intents with {} signed intents",
            self.tx,
            self.submitter,
            self.legs.len()
        )];
        for (signer, diff) in &self.legs {
            let side = |given: bool| -> Vec<String> {
                diff.diff
                    .iter()
                    .filter(|(_, delta)| (**delta < 0) == given)
                    .map(|(token, delta)| tokens.show(token, delta.unsigned_abs()))
                    .collect()
            };
            lines.push(format!(
                "  {signer} gave {} and got {}  (signature valid)",
                side(true).join(" + "),
                side(false).join(" + ")
            ));
        }
        lines.push(format!(
            "  every token nets to zero after the verifier's fee of {}: the legs settled together or not at all",
            pips(self.fee_pips)
        ));
        lines
    }
}

/// Reads the transaction `hash` sent by `submitter` and checks that it is a settled ring.
pub async fn check(verifier: &Verifier, hash: &str, submitter: &str) -> Result<Checked> {
    let recorded = verifier
        .transaction(hash, submitter)
        .await
        .with_context(|| format!("fetching transaction {hash}"))?;
    if let Some(failure) = recorded.failure {
        bail!("the transaction failed on chain: {failure}");
    }
    if recorded.receiver != verifier.contract() || recorded.method != "execute_intents" {
        bail!("not a settlement: it calls {}.{}", recorded.receiver, recorded.method);
    }
    #[derive(Deserialize)]
    struct Args {
        signed: Vec<SignedIntent>,
    }
    let args: Args = serde_json::from_slice(&recorded.args).context("reading the transaction's arguments")?;

    let mut legs = Vec::new();
    for signed in &args.signed {
        let opened = intents::open(signed).map_err(|refusal| anyhow::anyhow!("a leg does not check out: {refusal}"))?;
        for intent in &opened.intents {
            let diff = TokenDiff::from_intent(intent).context("a leg is not a token_diff")?;
            legs.push((opened.signer_id.clone(), diff));
        }
    }
    // The fee as it is now. If the contract's owners changed it since, the sums below would not
    // close and this would say so rather than guess.
    let fee_pips = verifier.state().await?.fee_pips;
    let left = intents::unmatched(legs.iter().map(|(_, diff)| diff), fee_pips).context("the amounts overflow")?;
    if !left.is_empty() {
        bail!(
            "the legs do not net to zero at the current fee of {}: {left:?}",
            pips(fee_pips)
        );
    }
    Ok(Checked {
        tx: hash.to_owned(),
        submitter: recorded.signer,
        legs,
        fee_pips,
    })
}
