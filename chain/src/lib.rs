//! The chain side of Gossip Market: `rpc` is the verifier contract over NEAR JSON-RPC, `tx` the
//! one kind of transaction an agent sends, and `devnet` a local stand-in that answers the same
//! calls from `intents::Ledger`.
pub mod devnet;
pub mod rpc;
pub mod tx;

pub use devnet::{Devnet, Sent};
pub use rpc::{ChainError, Recorded, Verifier, VerifierState};
