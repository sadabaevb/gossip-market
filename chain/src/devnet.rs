//! A verifier on localhost: `intents::Ledger` behind the handful of NEAR JSON-RPC calls an agent
//! makes, answered in the shapes mainnet nodes use. Agents cannot tell it from mainnet, which is
//! the point: the whole market runs and can be rehearsed with no funds, and the client code that
//! talks to it is the code that talks to `intents.near`.
//!
//! It is not a chain: one process, no consensus, blocks are a clock, and every implicit account
//! exists and has gas. Balances come from the genesis file.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::{extract::State, routing::post, Json, Router};
use base64::{engine::general_purpose::STANDARD, Engine};
use chrono::Utc;
use intents::nep413::{implicit_account, tag, verify_bytes};
use intents::{Ledger, SignedIntent};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tokio::sync::broadcast;

use crate::rpc::hash_text;
use crate::tx::{self, Received};

#[derive(Clone)]
pub struct Devnet {
    inner: Arc<Mutex<Inner>>,
    events: broadcast::Sender<String>,
}

struct Inner {
    ledger: Ledger,
    /// Per account: the last access-key nonce a transaction used.
    access_nonces: HashMap<String, u64>,
    /// By hash: what `send_tx` answered, for `tx` to answer again.
    transactions: HashMap<String, Value>,
    started: Instant,
}

impl Devnet {
    pub fn new(ledger: Ledger) -> Self {
        let inner = Inner {
            ledger,
            access_nonces: HashMap::new(),
            transactions: HashMap::new(),
            started: Instant::now(),
        };
        Self {
            inner: Arc::new(Mutex::new(inner)),
            events: broadcast::channel(256).0,
        }
    }

    /// Serves JSON-RPC on `listener` until the task is dropped.
    pub async fn serve(self, listener: TcpListener) -> std::io::Result<()> {
        let app = Router::new().route("/", post(rpc)).with_state(self);
        axum::serve(listener, app).await
    }

    /// The balances and nonces as they stand.
    pub fn ledger(&self) -> Ledger {
        self.lock().ledger.clone()
    }

    /// One line per transaction, for whoever runs the devnet to watch.
    pub fn events(&self) -> broadcast::Receiver<String> {
        self.events.subscribe()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("the devnet state is never left half-written")
    }
}

impl Inner {
    /// One block a second since the devnet started.
    fn height(&self) -> u64 {
        1 + self.started.elapsed().as_secs()
    }

    fn block_hash(&self) -> String {
        hash_text(&Sha256::digest(format!("gossipmarket devnet block {}", self.height())).into())
    }
}

#[derive(Deserialize)]
struct Request {
    #[serde(default)]
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

async fn rpc(State(devnet): State<Devnet>, Json(request): Json<Request>) -> Json<Value> {
    let answer = match request.method.as_str() {
        "query" => devnet.query(&request.params),
        "block" => devnet.block(),
        "send_tx" => devnet.send_tx(&request.params),
        "tx" => devnet.tx(&request.params),
        other => Err(handler_error("METHOD_NOT_FOUND", json!({ "method_name": other }))),
    };
    Json(match answer {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": request.id, "result": result }),
        Err(error) => json!({ "jsonrpc": "2.0", "id": request.id, "error": error }),
    })
}

/// An RPC error in the shape NEAR nodes send.
fn handler_error(name: &str, info: Value) -> Value {
    json!({
        "name": "HANDLER_ERROR",
        "cause": { "name": name, "info": info },
        "code": -32000,
        "message": "Server error",
        "data": info.to_string(),
    })
}

impl Devnet {
    fn block(&self) -> Result<Value, Value> {
        let inner = self.lock();
        Ok(json!({ "header": { "hash": inner.block_hash(), "height": inner.height() } }))
    }

    fn query(&self, params: &Value) -> Result<Value, Value> {
        let inner = self.lock();
        let at = json!({ "block_height": inner.height(), "block_hash": inner.block_hash() });
        let with_block = |mut answer: Value| {
            answer["block_height"] = at["block_height"].clone();
            answer["block_hash"] = at["block_hash"].clone();
            answer
        };
        let account = params["account_id"].as_str().unwrap_or_default();
        match params["request_type"].as_str() {
            // Every account has the key it is named after, and that key has never been used up.
            Some("view_access_key") => {
                let nonce = inner.access_nonces.get(account).copied().unwrap_or(0);
                Ok(with_block(json!({ "nonce": nonce, "permission": "FullAccess" })))
            }
            Some("call_function") => {
                if account != inner.ledger.contract {
                    return Err(handler_error(
                        "UNKNOWN_ACCOUNT",
                        json!({ "requested_account_id": account }),
                    ));
                }
                let args: Value = params["args_base64"]
                    .as_str()
                    .and_then(|text| STANDARD.decode(text).ok())
                    .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                    .unwrap_or(Value::Null);
                let method = params["method_name"].as_str().unwrap_or_default();
                match view(&inner.ledger, method, &args) {
                    Ok(value) => Ok(with_block(
                        json!({ "result": value.to_string().into_bytes(), "logs": [] }),
                    )),
                    // As mainnet nodes answered on 12 November 2026: the panic comes back inside
                    // the result, as Rust debug text.
                    Err(reason) => Ok(with_block(json!({
                        "error": format!("wasm execution failed with error: HostError(GuestPanic {{ panic_msg: {reason:?} }})"),
                        "logs": [],
                    }))),
                }
            }
            _ => Err(handler_error("UNSUPPORTED_QUERY", params.clone())),
        }
    }

    fn send_tx(&self, params: &Value) -> Result<Value, Value> {
        let invalid = |why: &str| handler_error("INVALID_TRANSACTION", json!({ "reason": why }));
        let signed = params["signed_tx_base64"]
            .as_str()
            .and_then(|text| STANDARD.decode(text).ok())
            .ok_or_else(|| invalid("signed_tx_base64 is not base64"))?;
        let Received { call, hash, signature } =
            tx::decode_signed(&signed).ok_or_else(|| invalid("not a single function call"))?;
        if !verify_bytes(&call.public_key, &hash, &signature) {
            return Err(invalid("InvalidSignature"));
        }
        // No named accounts and no key registry here: an account is the key it is named after.
        if call.signer != implicit_account(&call.public_key) {
            return Err(invalid(
                "InvalidAccessKeyError: the devnet knows implicit accounts only",
            ));
        }

        let mut inner = self.lock();
        let last = inner.access_nonces.entry(call.signer.clone()).or_default();
        if call.nonce <= *last {
            return Err(invalid("InvalidNonce"));
        }
        *last = call.nonce;

        let status = execute(&mut inner.ledger, &call);
        let hash = hash_text(&hash);
        let settled = status.get("SuccessValue").is_some();
        let outcome = json!({
            "final_execution_status": "EXECUTED_OPTIMISTIC",
            "status": status,
            "transaction": {
                "hash": hash,
                "signer_id": call.signer,
                "public_key": tag(&call.public_key),
                "nonce": call.nonce,
                "receiver_id": call.receiver,
                "actions": [{ "FunctionCall": {
                    "method_name": call.method,
                    "args": STANDARD.encode(&call.args),
                    "gas": call.gas,
                    "deposit": call.deposit.to_string(),
                } }],
                "signature": tag(&signature),
            },
            "transaction_outcome": { "id": hash, "block_hash": inner.block_hash() },
            "receipts_outcome": [],
        });
        inner.transactions.insert(hash.clone(), outcome.clone());
        let verdict = match settled {
            true => "settled".to_owned(),
            false => format!("failed: {}", status["Failure"]),
        };
        // Nobody listening is fine.
        let _ = self.events.send(format!(
            "block {} tx {hash} from {}… {verdict}",
            inner.height(),
            &call.signer[..8]
        ));
        Ok(outcome)
    }

    fn tx(&self, params: &Value) -> Result<Value, Value> {
        let hash = params["tx_hash"].as_str().unwrap_or_default();
        self.lock()
            .transactions
            .get(hash)
            .cloned()
            .ok_or_else(|| handler_error("UNKNOWN_TRANSACTION", json!({ "requested_transaction_hash": hash })))
    }
}

/// Runs a transaction's one call and returns its status the way a node reports it.
fn execute(ledger: &mut Ledger, call: &tx::FunctionCall) -> Value {
    let failure = |kind: Value| json!({ "Failure": { "ActionError": { "index": 0, "kind": kind } } });
    let panicked = |reason: &str| {
        failure(json!({ "FunctionCallError": { "ExecutionError": format!("Smart contract panicked: {reason}") } }))
    };
    if call.receiver != ledger.contract {
        return failure(json!({ "AccountDoesNotExist": { "account_id": call.receiver } }));
    }
    if call.method != "execute_intents" {
        return failure(json!({ "FunctionCallError": { "MethodResolveError": "MethodNotFound" } }));
    }
    #[derive(Deserialize)]
    struct Args {
        signed: Vec<SignedIntent>,
    }
    match serde_json::from_slice::<Args>(&call.args) {
        Err(_) => panicked("Failed to deserialize input from JSON."),
        Ok(args) => match ledger.execute(&args.signed, Utc::now()) {
            Ok(()) => json!({ "SuccessValue": "" }),
            Err(refusal) => panicked(&refusal.0),
        },
    }
}

/// The verifier's read-only methods. The error is the panic message.
fn view(ledger: &Ledger, method: &str, args: &Value) -> Result<Value, String> {
    let bad_args = || "Failed to deserialize input from JSON.".to_owned();
    let text = |field: &str| args[field].as_str().ok_or_else(bad_args);
    match method {
        "fee" => Ok(json!(ledger.fee_pips)),
        "current_salt" => Ok(json!(hex::encode(ledger.salt))),
        "mt_balance_of" => Ok(json!(ledger
            .balance_of(text("account_id")?, text("token_id")?)
            .to_string())),
        "mt_batch_balance_of" => {
            let account = text("account_id")?;
            let tokens = args["token_ids"].as_array().ok_or_else(bad_args)?;
            let balances: Option<Vec<String>> = tokens
                .iter()
                .map(|token| Some(ledger.balance_of(account, token.as_str()?).to_string()))
                .collect();
            Ok(json!(balances.ok_or_else(bad_args)?))
        }
        "is_nonce_used" => {
            let nonce: [u8; 32] = STANDARD
                .decode(text("nonce")?)
                .ok()
                .and_then(|bytes| bytes.try_into().ok())
                .ok_or_else(bad_args)?;
            Ok(json!(ledger.is_nonce_used(text("account_id")?, &nonce)))
        }
        "simulate_intents" => {
            let signed: Vec<SignedIntent> = serde_json::from_value(args["signed"].clone()).map_err(|_| bad_args())?;
            let simulation = ledger.simulate(&signed, Utc::now()).map_err(|refusal| refusal.0)?;
            let mut report = json!({
                "intents_executed": [],
                "logs": [],
                "state": { "fee": ledger.fee_pips, "current_salt": hex::encode(ledger.salt) },
            });
            if !simulation.unmatched.is_empty() {
                let deltas: HashMap<_, _> = simulation
                    .unmatched
                    .iter()
                    .map(|(token, delta)| (token, delta.to_string()))
                    .collect();
                report["invariant_violated"] = json!({ "error": "unmatched_deltas", "unmatched_deltas": deltas });
            }
            Ok(report)
        }
        _ => Err(format!("MethodNotFound: {method}")),
    }
}
