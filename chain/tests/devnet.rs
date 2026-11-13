//! The RPC client against the devnet, over real HTTP on localhost: every call an agent makes, and
//! what it is told when the verifier says no.
use chain::{ChainError, Devnet, Verifier};
use chrono::{Duration, Utc};
use intents::{nonce, Keypair, Ledger, SignedIntent, TokenDiff};

const CONTRACT: &str = "intents.devnet";
const USDC: &str = "nep141:usdc.devnet";
const CREDITS: &str = "nep141:credits.devnet";
const DATA: &str = "nep141:data.devnet";

struct World {
    devnet: Devnet,
    verifier: Verifier,
    rpc: String,
    a: Keypair,
    b: Keypair,
    c: Keypair,
}

async fn world() -> World {
    let [a, b, c] = [1, 2, 3].map(|seed| Keypair::from_seed([seed; 32]));
    let mut ledger = Ledger::new(CONTRACT, 100);
    ledger.deposit(&a.account_id(), USDC, 50_000_000);
    ledger.deposit(&b.account_id(), CREDITS, 500_000);
    ledger.deposit(&c.account_id(), DATA, 20_000);
    let devnet = Devnet::new(ledger);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rpc = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(devnet.clone().serve(listener));
    World {
        devnet,
        verifier: Verifier::new(rpc.clone(), CONTRACT),
        rpc,
        a,
        b,
        c,
    }
}

impl World {
    async fn ring(&self) -> Vec<SignedIntent> {
        let state = self.verifier.state().await.unwrap();
        let deadline = Utc::now() + Duration::seconds(30);
        let sign = |key: &Keypair, give, take| {
            let diff = TokenDiff::swap(give, take).unwrap();
            let nonce = nonce::versioned(state.salt, deadline);
            key.sign_intents(CONTRACT, deadline, &nonce, vec![diff.to_intent()])
        };
        vec![
            sign(&self.a, (USDC, 10_000_000), (CREDITS, 99_990)),
            sign(&self.b, (CREDITS, 100_000), (DATA, 4_999)),
            sign(&self.c, (DATA, 5_000), (USDC, 9_999_000)),
        ]
    }
}

fn refused(reason: &str) -> ChainError {
    ChainError::Refused(reason.into())
}

#[tokio::test]
async fn an_agent_reads_the_verifier_and_settles_a_ring_through_it() {
    let w = world().await;
    let state = w.verifier.state().await.unwrap();
    assert_eq!((state.fee_pips, state.salt), (100, w.devnet.ledger().salt));

    let tokens = [USDC.to_owned(), CREDITS.to_owned(), DATA.to_owned()];
    assert_eq!(
        w.verifier.balances(&w.a.account_id(), &tokens).await.unwrap(),
        [50_000_000, 0, 0]
    );

    let ring = w.ring().await;
    let a_nonce = intents::open(&ring[0]).unwrap().nonce;
    assert_eq!(w.verifier.is_nonce_used(&w.a.account_id(), &a_nonce).await, Ok(false));
    assert_eq!(w.verifier.simulate(&ring).await, Ok(()));

    // Carol pays the gas; any of the three could.
    let hash = w.verifier.execute(&w.c, &ring).await.unwrap();
    assert_eq!(w.verifier.is_nonce_used(&w.a.account_id(), &a_nonce).await, Ok(true));
    assert_eq!(
        w.verifier.balances(&w.a.account_id(), &tokens).await.unwrap(),
        [40_000_000, 99_990, 0]
    );
    assert_eq!(
        w.verifier.balances(&w.c.account_id(), &tokens).await.unwrap(),
        [9_999_000, 0, 15_000]
    );

    // The transaction can be fetched back and holds exactly the bundle that was signed.
    let recorded = w.verifier.transaction(&hash, &w.c.account_id()).await.unwrap();
    assert_eq!(recorded.failure, None);
    assert_eq!(
        (recorded.signer.as_str(), recorded.receiver.as_str()),
        (w.c.account_id().as_str(), CONTRACT)
    );
    assert_eq!(recorded.method, "execute_intents");
    let args: serde_json::Value = serde_json::from_slice(&recorded.args).unwrap();
    assert_eq!(args["signed"], serde_json::to_value(&ring).unwrap());
}

// The difference between "stop, this bundle is dead" and "try again" is what keeps an agent from
// burning gas, so each refusal has to come through the RPC layer as the contract's own words.
#[tokio::test]
async fn refusals_arrive_in_the_contracts_words() {
    let w = world().await;
    let ring = w.ring().await;

    // Two legs of three: a dry run says what is missing, and sending it anyway fails the same way.
    let missing = r#"invariant violated: {"error":"unmatched_deltas","unmatched_deltas":{"nep141:data.devnet":"4999","nep141:usdc.devnet":"-9999000"}}"#;
    let unbalanced = w.verifier.simulate(&ring[..2]).await.unwrap_err();
    let ChainError::Refused(reason) = &unbalanced else {
        panic!("{unbalanced:?}")
    };
    let parse = |text: &str| serde_json::from_str::<serde_json::Value>(text.split_once(": ").unwrap().1).unwrap();
    assert_eq!(parse(reason), parse(missing));
    assert_eq!(w.verifier.execute(&w.a, &ring[..2]).await, Err(refused(missing)));

    // A leg changed after it was signed.
    let mut tampered = ring.clone();
    tampered[1].payload.message = tampered[1].payload.message.replace("\"4999\"", "\"5000\"");
    assert_eq!(w.verifier.simulate(&tampered).await, Err(refused("invalid signature")));

    w.verifier.execute(&w.a, &ring).await.unwrap();
    assert_eq!(w.verifier.simulate(&ring).await, Err(refused("nonce was already used")));
    assert_eq!(
        w.verifier.execute(&w.b, &ring).await,
        Err(refused("nonce was already used"))
    );
}

#[tokio::test]
async fn an_unreachable_chain_is_not_a_refusal() {
    // A port nothing listens on.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rpc = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let nowhere = Verifier::new(rpc, CONTRACT);
    assert!(matches!(nowhere.state().await, Err(ChainError::Unknown(_))));
    assert!(matches!(nowhere.simulate(&[]).await, Err(ChainError::Unknown(_))));

    // A node that is up but does not have this contract.
    let w = world().await;
    let wrong = Verifier::new(w.rpc.clone(), "intents.near");
    assert!(matches!(wrong.state().await, Err(ChainError::Unknown(_))));
}
