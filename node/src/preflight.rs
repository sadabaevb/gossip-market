//! Asking the real verifier, for free, whether it reads what agents sign. A throwaway key that
//! holds nothing signs a leg exactly as a node would, and the contract is asked for a dry run.
//! If it gets as far as "insufficient balance", it has accepted the signature, the account's key,
//! the nonce and the deadline: everything about the message except the money. Nothing is sent,
//! nothing can settle, and no funds or gas are needed.
use anyhow::{Context, Result};
use chain::{ChainError, Verifier, VerifierState};
use chrono::{Duration, Utc};
use intents::{nonce, FtWithdraw, Keypair, SignedIntent, TokenDiff, TokenId};

use crate::config::{pips, NodeConfig};

/// One question for the verifier and the answer that shows agents and contract agree.
pub struct Probe {
    pub what: &'static str,
    pub signed: Vec<SignedIntent>,
    /// What the contract must say.
    pub expect: &'static str,
    /// What that answer shows.
    pub shows: &'static str,
}

/// The questions, signed by a key made up on the spot.
pub fn probes(verifier: &str, state: VerifierState, give: &TokenId, take: &TokenId) -> Vec<Probe> {
    let key = Keypair::generate();
    let deadline = Utc::now() + Duration::minutes(5);
    let leg = TokenDiff::swap((give, 1_000), (take, 1_000))
        .expect("two different tokens")
        .with_memo("gossipmarket preflight")
        .to_intent();
    let sign = |nonce: [u8; 32]| key.sign_intents(verifier, deadline, &nonce, vec![leg.clone()]);

    let versioned = sign(nonce::versioned(state.salt, deadline));
    let mut stale_salt = state.salt;
    stale_salt[0] ^= 0xff;
    let mut altered = versioned.clone();
    altered.payload.message = altered.payload.message.replace("\"-1000\"", "\"-1\"");

    // To the key's own account: a receiver that certainly exists, and is nobody's.
    let withdraw = FtWithdraw::new(give, key.account_id(), 1_000).map(|withdraw| Probe {
        what: "a withdraw signed as `gossipmarket withdraw` signs it",
        signed: vec![key.sign_intents(
            verifier,
            deadline,
            &nonce::versioned(state.salt, deadline),
            vec![withdraw.with_memo("gossipmarket preflight").to_intent()],
        )],
        expect: "insufficient balance or overflow",
        shows: "the contract read the withdraw's token, receiver and amount, and stopped only at the balance",
    });

    let mut probes = vec![
        Probe {
            what: "a leg signed as nodes sign it (versioned nonce with the live salt)",
            signed: vec![versioned],
            expect: "insufficient balance or overflow",
            shows: "the contract accepted the signature, the implicit account's key, the nonce and the deadline, and stopped only at the balance",
        },
        Probe {
            what: "the same leg under a plain nonce",
            signed: vec![sign(nonce::plain())],
            expect: "insufficient balance or overflow",
            shows: "plain nonces are still accepted (`nonce = \"plain\"` in a config would work today)",
        },
        Probe {
            what: "a nonce carrying a salt that is not the contract's",
            signed: vec![sign(nonce::versioned(stale_salt, deadline))],
            expect: "invalid salt",
            shows: "the contract reads our versioned nonces as versioned, and checks their salt",
        },
        Probe {
            what: "a leg altered after it was signed",
            signed: vec![altered],
            expect: "invalid signature",
            shows: "what is signed is what settles",
        },
    ];
    probes.extend(withdraw);
    probes
}

/// Whether the verifier's answer is the one expected.
pub fn agrees(answer: &Result<(), ChainError>, expect: &str) -> bool {
    matches!(answer, Err(ChainError::Refused(reason)) if reason == expect)
}

pub async fn run(config: &NodeConfig) -> Result<()> {
    let verifier = Verifier::new(config.rpc.clone(), config.verifier.clone());
    let state = verifier
        .state()
        .await
        .with_context(|| format!("reading {} at {}", config.verifier, config.rpc))?;
    println!(
        "{} at {}: fee {}, salt {}",
        config.verifier,
        config.rpc,
        pips(state.fee_pips),
        hex::encode(state.salt)
    );
    let tokens = config.tokens().ids();
    let [give, take, ..] = tokens.as_slice() else {
        anyhow::bail!("the config needs at least two tokens");
    };

    let mut agreed = true;
    for probe in probes(&config.verifier, state, give, take) {
        let answer = verifier.simulate(&probe.signed).await;
        let said = match &answer {
            Ok(()) => "it would settle".to_owned(),
            Err(e) => e.reason().to_owned(),
        };
        match agrees(&answer, probe.expect) {
            true => println!("ok    {}\n      -> \"{said}\": {}", probe.what, probe.shows),
            false => {
                agreed = false;
                println!(
                    "WRONG {}\n      -> \"{said}\", expected \"{}\"",
                    probe.what, probe.expect
                );
            }
        }
    }
    if !agreed {
        anyhow::bail!("the verifier does not read what this build signs; do not fund anything yet");
    }
    println!("PREFLIGHT PASSED: the verifier reads what this build signs. Nothing was sent or spent.");
    Ok(())
}
