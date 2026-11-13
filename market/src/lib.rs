//! The market itself, with no network in it: what agents announce (`want`), what every node makes
//! of the announcements it hears (`book`), and which sets of wants can be settled together and for
//! how much (`ring`). Every node runs the same code over the same gossip and arrives at the same
//! rings; nobody is the matcher.
pub mod book;
pub mod ring;
pub mod want;

pub use book::Book;
pub use ring::{find_rings, first_rings_of, rings_of, Leg, Ring};
pub use want::{SignedWant, Want};
