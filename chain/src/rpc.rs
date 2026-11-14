//! The verifier contract over NEAR JSON-RPC: what an agent reads from it and the one transaction
//! it sends. The same client talks to mainnet and to the local devnet.
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::LazyLock;
use std::time::Duration;

use base64::{engine::general_purpose::STANDARD, Engine};
use intents::nep413::tag;
use intents::nonce::Salt;
use intents::{Keypair, SignedIntent, TokenId};
use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::tx::{FunctionCall, MAX_GAS};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainError {
    /// The contract itself refused; the text is its reason ("nonce was already used"). A refusal
    /// is an answer about the bundle: sending it again will get the same one.
    Refused(String),
    /// The chain could not be reached, or answered something else. It says nothing about the
    /// bundle, which may even have settled.
    Unknown(String),
}

impl ChainError {
    pub fn reason(&self) -> &str {
        match self {
            Self::Refused(reason) | Self::Unknown(reason) => reason,
        }
    }

    /// For calls that do not judge a bundle: if the contract panics there, that says something
    /// about the contract, and nothing about any bundle.
    fn not_a_refusal(self) -> Self {
        Self::Unknown(self.reason().to_owned())
    }
}

impl std::fmt::Display for ChainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(reason) => write!(f, "the verifier refused: {reason}"),
            Self::Unknown(reason) => write!(f, "no answer from the chain: {reason}"),
        }
    }
}

impl std::error::Error for ChainError {}

/// What every bundle depends on and the contract's owners can change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifierState {
    /// Taken from every amount given in a `token_diff`, in millionths.
    pub fee_pips: u32,
    /// What a versioned nonce must carry to be accepted.
    pub salt: Salt,
}

/// A transaction as the chain recorded it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorded {
    pub signer: String,
    pub receiver: String,
    pub method: String,
    pub args: Vec<u8>,
    /// `None` if it succeeded, otherwise why it failed.
    pub failure: Option<String>,
}

pub struct Verifier {
    http: reqwest::Client,
    rpc: String,
    contract: String,
    /// The last access-key nonce this process used. The node answering a view can be a block
    /// behind the one that ran our previous transaction, and would hand the same nonce out again.
    last_nonce: AtomicU64,
}

impl Verifier {
    /// `contract` on the network behind `rpc`.
    pub fn new(rpc: impl Into<String>, contract: impl Into<String>) -> Self {
        Self {
            // send_tx answers only once the transaction has run, which takes a few blocks.
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(60))
                .build()
                .expect("an HTTP client"),
            rpc: rpc.into(),
            contract: contract.into(),
            last_nonce: AtomicU64::new(0),
        }
    }

    pub fn contract(&self) -> &str {
        &self.contract
    }

    pub async fn state(&self) -> Result<VerifierState, ChainError> {
        let (fee, salt) = tokio::try_join!(self.view("fee", json!({})), self.view("current_salt", json!({})))
            .map_err(ChainError::not_a_refusal)?;
        let fee_pips = fee
            .as_u64()
            .and_then(|pips| u32::try_from(pips).ok())
            .ok_or_else(|| unexpected("fee", &fee))?;
        let salt = salt
            .as_str()
            .and_then(|text| hex::decode(text).ok())
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or_else(|| unexpected("salt", &salt))?;
        Ok(VerifierState { fee_pips, salt })
    }

    /// Base units of each token that `account` holds inside the verifier.
    pub async fn balances(&self, account: &str, tokens: &[TokenId]) -> Result<Vec<u128>, ChainError> {
        let args = json!({ "account_id": account, "token_ids": tokens });
        let answer = self
            .view("mt_batch_balance_of", args)
            .await
            .map_err(ChainError::not_a_refusal)?;
        let amounts: Option<Vec<u128>> = answer
            .as_array()
            .filter(|amounts| amounts.len() == tokens.len())
            .and_then(|amounts| amounts.iter().map(|a| a.as_str()?.parse().ok()).collect());
        amounts.ok_or_else(|| unexpected("balances", &answer))
    }

    /// Whether a message signed by `account` under this nonce has settled.
    pub async fn is_nonce_used(&self, account: &str, nonce: &[u8; 32]) -> Result<bool, ChainError> {
        let args = json!({ "account_id": account, "nonce": STANDARD.encode(nonce) });
        let answer = self
            .view("is_nonce_used", args)
            .await
            .map_err(ChainError::not_a_refusal)?;
        answer.as_bool().ok_or_else(|| unexpected("is_nonce_used", &answer))
    }

    /// A free dry run. Refused if the bundle would not settle right now.
    pub async fn simulate(&self, signed: &[SignedIntent]) -> Result<(), ChainError> {
        let report = self.view("simulate_intents", json!({ "signed": signed })).await?;
        // The one failure the contract reports instead of panicking: the batch does not net.
        match report.get("invariant_violated") {
            None | Some(Value::Null) => Ok(()),
            Some(violated) => Err(ChainError::Refused(format!("invariant violated: {violated}"))),
        }
    }

    /// Settles the bundle in one transaction signed and paid for by `payer`, whose implicit
    /// account must exist on chain and hold NEAR for gas. Returns the transaction hash once it has
    /// run. Refused if the contract turned the bundle down.
    pub async fn execute(&self, payer: &Keypair, signed: &[SignedIntent]) -> Result<String, ChainError> {
        let account = payer.account_id();
        let public_key = payer.public_key();
        let access = json!({ "request_type": "view_access_key", "finality": "optimistic", "account_id": account, "public_key": tag(&public_key) });
        // A final block: a transaction that names a block which is later dropped is invalid.
        let (access, block) = tokio::try_join!(
            self.call("query", access),
            self.call("block", json!({ "finality": "final" }))
        )
        .map_err(ChainError::not_a_refusal)?;
        let seen = access["nonce"]
            .as_u64()
            .ok_or_else(|| unexpected("access key", &access))?;
        // `fetch_update` has a newer name on the newest compilers. The old one builds on all of them.
        #[allow(deprecated)]
        let nonce = self
            .last_nonce
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |last| Some(last.max(seen) + 1))
            .map(|last| last.max(seen) + 1)
            .expect("always Some");
        let block_hash: [u8; 32] = block["header"]["hash"]
            .as_str()
            .and_then(|hash| bs58::decode(hash).into_vec().ok())
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or_else(|| unexpected("block", &block))?;

        let call = FunctionCall {
            signer: account,
            public_key,
            nonce,
            receiver: self.contract.clone(),
            block_hash,
            method: "execute_intents".into(),
            args: json!({ "signed": signed }).to_string().into_bytes(),
            gas: MAX_GAS,
            deposit: 0,
        };
        let sent =
            json!({ "signed_tx_base64": STANDARD.encode(call.sign(payer)), "wait_until": "EXECUTED_OPTIMISTIC" });
        let outcome = self.call("send_tx", sent).await?;
        if let Some(failure) = outcome["status"].get("Failure") {
            return Err(failed(failure));
        }
        // Only a transaction seen to succeed is reported as settled. One still in flight is found
        // through its nonces.
        match (
            outcome["status"].get("SuccessValue"),
            outcome["transaction"]["hash"].as_str(),
        ) {
            (Some(_), Some(hash)) => Ok(hash.to_owned()),
            _ => Err(unexpected("transaction outcome", &outcome)),
        }
    }

    /// Looks a transaction up by its hash and the account that sent it.
    pub async fn transaction(&self, hash: &str, sender: &str) -> Result<Recorded, ChainError> {
        let params = json!({ "tx_hash": hash, "sender_account_id": sender, "wait_until": "EXECUTED_OPTIMISTIC" });
        let outcome = self.call("tx", params).await.map_err(ChainError::not_a_refusal)?;
        let transaction = &outcome["transaction"];
        let call = &transaction["actions"][0]["FunctionCall"];
        let recorded = (|| {
            Some(Recorded {
                signer: transaction["signer_id"].as_str()?.to_owned(),
                receiver: transaction["receiver_id"].as_str()?.to_owned(),
                method: call["method_name"].as_str()?.to_owned(),
                args: STANDARD.decode(call["args"].as_str()?).ok()?,
                failure: match outcome["status"].get("Failure") {
                    Some(failure) => Some(failed(failure).reason().to_owned()),
                    None => {
                        outcome["status"].get("SuccessValue")?;
                        None
                    }
                },
            })
        })();
        recorded.ok_or_else(|| unexpected("transaction", &outcome))
    }

    /// A view call on the verifier, decoded from JSON.
    async fn view(&self, method: &str, args: Value) -> Result<Value, ChainError> {
        let query = json!({
            "request_type": "call_function",
            "finality": "optimistic",
            "account_id": self.contract,
            "method_name": method,
            "args_base64": STANDARD.encode(args.to_string()),
        });
        let result = self.call("query", query).await?;
        Vec::<u8>::deserialize(&result["result"])
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .ok_or_else(|| unexpected(method, &result))
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, ChainError> {
        let request = json!({ "jsonrpc": "2.0", "id": "gossipmarket", "method": method, "params": params });
        let sent = async {
            self.http
                .post(&self.rpc)
                .json(&request)
                .send()
                .await?
                .json::<Value>()
                .await
        };
        // Without the URL: it can hold the RPC provider's API key, and this text goes into logs.
        let mut response = sent
            .await
            .map_err(|e| ChainError::Unknown(e.without_url().to_string()))?;
        match response.get_mut("result") {
            // Some nodes report a failed view inside the result rather than as an RPC error.
            Some(result) if result.get("error").is_none() => Ok(result.take()),
            _ => Err(classify(&response)),
        }
    }
}

fn unexpected(what: &str, got: &Value) -> ChainError {
    ChainError::Unknown(format!("unexpected answer for {what}: {got}"))
}

/// A transaction that ran and failed. Whatever the reason, that is the chain's answer about this
/// bundle: called anything else, agents would send it again and each try would cost gas.
fn failed(failure: &Value) -> ChainError {
    ChainError::Refused(classify(failure).reason().to_owned())
}

/// An RPC error or failed execution, as a refusal when it is the contract panicking.
fn classify(error: &Value) -> ChainError {
    static DEBUG_FORM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"panic_msg: "((?:[^"\\]|\\.)*)""#).unwrap());
    // Nodes spell a panic three ways: a "panic_msg" field, Rust debug text holding one, or a sentence.
    fn panic_in(v: &Value) -> Option<String> {
        match v {
            Value::String(s) => match DEBUG_FORM.captures(s) {
                // Debug text escapes the quotes of the JSON inside "invariant violated: {...}".
                Some(found) => Some(found[1].replace("\\\"", "\"").replace("\\\\", "\\")),
                None => s
                    .split_once("Smart contract panicked: ")
                    .map(|(_, reason)| reason.to_owned()),
            },
            Value::Array(items) => items.iter().find_map(panic_in),
            Value::Object(fields) => match fields.get("panic_msg") {
                Some(Value::String(reason)) => Some(reason.clone()),
                _ => fields.values().find_map(panic_in),
            },
            _ => None,
        }
    }
    match panic_in(error) {
        Some(reason) => ChainError::Refused(reason),
        None => ChainError::Unknown(error.to_string()),
    }
}

/// The base58 text of a transaction hash, as explorers and the `tx` call take it.
pub fn hash_text(hash: &[u8; 32]) -> String {
    bs58::encode(hash).into_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // "Refused" is an answer about the bundle and stops the gas; anything else means the bundle
    // may still be good. Getting this backwards either burns gas on a bundle that cannot settle
    // or drops one that can.
    #[test]
    fn tells_a_contract_refusal_from_an_outage() {
        let refused = |reason: &str| ChainError::Refused(reason.into());
        let debug_text = json!({ "error": "wasm execution failed with error: HostError(GuestPanic { panic_msg: \"insufficient balance or overflow\" })" });
        assert_eq!(classify(&debug_text), refused("insufficient balance or overflow"));
        let sentence = json!({ "ActionError": { "kind": { "FunctionCallError": { "ExecutionError": "Smart contract panicked: deadline has expired" } } } });
        assert_eq!(classify(&sentence), refused("deadline has expired"));
        let structured = json!({ "cause": { "info": { "vm_error": { "HostError": { "GuestPanic": { "panic_msg": "invalid signature" } } } } } });
        assert_eq!(classify(&structured), refused("invalid signature"));
        assert!(matches!(
            classify(&json!({ "name": "REQUEST_VALIDATION_ERROR", "message": "Timeout" })),
            ChainError::Unknown(_)
        ));

        // A transaction that ran and failed for any other reason (say, out of gas) is still an answer.
        let out_of_gas = json!({ "ActionError": { "kind": { "FunctionCallError": { "ExecutionError": "Exceeded the prepaid gas." } } } });
        assert!(matches!(classify(&out_of_gas), ChainError::Unknown(_)));
        assert!(matches!(failed(&out_of_gas), ChainError::Refused(_)));
    }

    // The reason for an unbalanced bundle is itself JSON, and Rust's debug text escapes it. Read
    // back wrongly, an agent could not tell which token was short.
    #[test]
    fn reads_a_reason_that_contains_quotes() {
        let reason = r#"invariant violated: {"error":"unmatched_deltas","unmatched_deltas":{"nep141:usdc.near":"-5"}}"#;
        let debug_text = json!({ "error": format!("HostError(GuestPanic {{ panic_msg: {reason:?} }})") });
        assert_eq!(classify(&debug_text), ChainError::Refused(reason.into()));
    }
}
