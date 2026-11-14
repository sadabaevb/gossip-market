//! A seat at the market for a person. `gossipmarket mcp` gives an AI agent eight tools; this
//! serves a page on localhost with the same tools behind a form and buttons, in the same
//! rehearsal, so that what an agent is told can be read, and tried, by hand.
//!
//! Only the rehearsal is served. A page on localhost answers whoever on this machine asks, which
//! is no way to hold a real account.
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::{Path as Segment, State};
use axum::response::Html;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use crate::mcp::{Market, Onlooker};

const PAGE: &str = include_str!("../../docs/seat/index.html");

/// Where the page is served unless told otherwise.
pub const PORT: u16 = 7879;

#[derive(Clone)]
struct Seat {
    /// One tool at a time, as over MCP: a node answers its agent in turn.
    market: Arc<Mutex<Market>>,
    /// What the page draws meanwhile, without waiting for the tool that is running.
    onlooker: Onlooker,
}

/// Starts the rehearsal and serves its page on localhost. Returns the page's address; the market
/// lives for as long as the process does.
pub async fn open(port: u16) -> Result<String> {
    let market = Market::rehearsal().await?;
    let seat = Seat {
        onlooker: market.onlooker(),
        market: Arc::new(Mutex::new(market)),
    };
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port)))
        .await
        .with_context(|| format!("serving the page on port {port}; `--port` picks another"))?;
    let url = format!("http://{}", listener.local_addr()?);
    let app = Router::new()
        .route("/", get(|| async { Html(PAGE) }))
        .route("/state", get(state))
        .route("/tool/{name}", post(tool))
        .with_state(seat);
    tokio::spawn(async move { axum::serve(listener, app).await });
    Ok(url)
}

async fn state(State(seat): State<Seat>) -> Json<Value> {
    Json(seat.onlooker.state())
}

/// Runs one tool with the arguments in the request's body, and answers with the text an agent
/// would be given, word for word.
async fn tool(State(seat): State<Seat>, Segment(name): Segment<String>, Json(args): Json<Value>) -> Json<Value> {
    let answer = seat.market.lock().await.call(&name, &args).await;
    Json(match answer {
        Ok(text) => json!({ "ok": true, "text": text }),
        Err(text) => json!({ "ok": false, "text": text }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::ask;

    // The page is the only way a person reaches the tools. If a click did not reach the node, or
    // the page were told less than the tools say, the seat would look alive and trade nothing.
    #[tokio::test]
    async fn a_person_at_the_page_closes_a_ring_with_the_tools_an_agent_is_given() {
        let url = open(0).await.unwrap();
        let tool = |name: &str, args: Value| {
            let url = format!("{url}/tool/{name}");
            async move { ask("POST", &url, Some(&args)).await }
        };

        let before = ask("GET", &format!("{url}/state"), None).await;
        assert_eq!(before["me"], "alice");
        assert_eq!(before["settlements"], json!([]));

        let placed = tool("place_want", json!({ "give": "10 USDC", "take": "95 CREDITS" })).await;
        assert_eq!(placed["ok"], true, "{placed}");
        let settled = tool("wait_for_settlement", json!({ "seconds": 20 })).await;
        assert!(
            settled["text"].as_str().unwrap().starts_with("SETTLED ring"),
            "{settled}"
        );

        // What the page draws the ring from: every maker, and what each gave.
        let after = ask("GET", &format!("{url}/state"), None).await;
        let ring = &after["settlements"][0];
        assert_eq!(ring["mine"], true, "{after}");
        let legs = ring["legs"].as_array().unwrap();
        let makers: Vec<&str> = legs.iter().map(|leg| leg["who"].as_str().unwrap()).collect();
        assert_eq!(makers.len(), 3);
        for name in ["alice", "bob", "carol"] {
            assert!(makers.contains(&name), "{makers:?}");
        }
        let mine = legs.iter().find(|leg| leg["who"] == "alice").unwrap();
        assert_eq!(mine["gave"], "10 USDC");
        let holding = after["holding"].to_string();
        assert!(holding.contains("40 USDC") && holding.contains("CREDITS"), "{holding}");

        let verified = tool("verify_settlement", json!({ "tx": ring["tx"] })).await;
        assert!(verified["text"].as_str().unwrap().ends_with("VERIFIED"), "{verified}");

        // A want the node turns down comes back as a failure, in the node's own words.
        let refused = tool("place_want", json!({ "give": "10 GOLD", "take": "1 USDC" })).await;
        assert_eq!(refused["ok"], false, "{refused}");
        assert!(refused["text"].as_str().unwrap().contains("GOLD"), "{refused}");
    }
}
