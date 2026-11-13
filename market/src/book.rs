//! The order book a node rebuilds from the wants it hears. There is no shared copy: two nodes
//! that have heard the same announcements hold the same book.
use std::collections::BTreeMap;

use intents::AccountId;

use crate::Want;

/// Open wants one maker may have in a book. Announcements are free; a book is not.
pub const MAX_WANTS_PER_MAKER: usize = 16;
/// Wants a book holds in all.
pub const MAX_WANTS: usize = 10_000;

#[derive(Debug, Clone, Default)]
pub struct Book {
    /// By maker and the maker's id, so that every node lists and searches them in one order.
    wants: BTreeMap<(AccountId, u64), Want>,
}

impl Book {
    /// Takes in an announcement that has been opened and checked. Returns whether the book changed.
    pub fn insert(&mut self, want: Want) -> bool {
        let key = (want.maker.clone(), want.id);
        match self.wants.get(&key) {
            // Gossip repeats itself and arrives out of order.
            Some(known) if !want.is_newer_than(known) => false,
            // Kept even when closed: an older open announcement may still be on its way.
            Some(_) => {
                self.wants.insert(key, want);
                true
            }
            None => {
                let theirs = self
                    .wants
                    .range((want.maker.clone(), 0)..=(want.maker.clone(), u64::MAX));
                if theirs.count() >= MAX_WANTS_PER_MAKER || self.wants.len() >= MAX_WANTS {
                    return false;
                }
                self.wants.insert(key, want);
                true
            }
        }
    }

    /// Forgets wants whose makers have stopped announcing them. Returns whether any went.
    pub fn expire(&mut self, now_ms: u64) -> bool {
        let before = self.wants.len();
        self.wants.retain(|_, want| want.expires_ms > now_ms);
        self.wants.len() != before
    }

    pub fn get(&self, maker: &str, id: u64) -> Option<&Want> {
        self.wants.get(&(maker.to_owned(), id))
    }

    /// Wants that can still be filled, in the book's order.
    pub fn open(&self) -> impl Iterator<Item = &Want> {
        self.wants.values().filter(|want| want.is_open())
    }

    pub fn len(&self) -> usize {
        self.open().count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::want::tests::{want, NOW};

    const A: &str = "nep141:a.near";
    const B: &str = "nep141:b.near";

    #[test]
    fn keeps_the_latest_announcement_of_each_want() {
        let mut book = Book::default();
        let first = want(1, (A, 10), (B, 20));
        assert!(book.insert(first.clone()));
        assert!(!book.insert(first.clone()), "a repeat changes nothing");

        // The maker re-announces every few seconds with a later expiry.
        let mut refreshed = first.clone();
        refreshed.expires_ms += 5_000;
        assert!(book.insert(refreshed.clone()));
        assert!(!book.insert(first.clone()), "an older copy arriving late is ignored");

        // Half filled.
        let mut filled = first.clone();
        (filled.rev, filled.give_amount, filled.take_amount) = (1, 5, 10);
        assert!(book.insert(filled.clone()));
        assert!(
            !book.insert(refreshed),
            "a later expiry does not bring back an older revision"
        );
        assert_eq!(book.get(&first.maker, 1), Some(&filled));
        assert_eq!(book.len(), 1);
    }

    // If a closed want were dropped at once, a copy of the open one still travelling through the
    // mesh would put it back, and nodes would keep proposing rings the maker turns down.
    #[test]
    fn a_closed_want_stays_closed() {
        let mut book = Book::default();
        let open = want(1, (A, 10), (B, 20));
        let mut closed = open.clone();
        (closed.rev, closed.give_amount, closed.take_amount) = (1, 0, 0);
        book.insert(open.clone());
        book.insert(closed);
        assert!(book.is_empty());
        assert!(!book.insert(open));
        assert!(book.is_empty());
    }

    #[test]
    fn forgets_wants_that_are_no_longer_announced() {
        let mut book = Book::default();
        book.insert(want(1, (A, 10), (B, 20)));
        assert!(!book.expire(NOW));
        assert!(book.expire(NOW + 20_000));
        assert!(book.is_empty());
    }

    #[test]
    fn one_maker_cannot_fill_the_book() {
        let mut book = Book::default();
        for id in 0..MAX_WANTS_PER_MAKER as u64 + 5 {
            let mut one = want(1, (A, 10), (B, 20));
            one.id = id;
            book.insert(one);
        }
        assert_eq!(book.len(), MAX_WANTS_PER_MAKER);
        // Someone else still gets in.
        assert!(book.insert(want(2, (B, 20), (A, 10))));
    }
}
