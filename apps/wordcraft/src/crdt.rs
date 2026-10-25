//! Per-paragraph RGA text for shared typing.
//!
//! Each paragraph is one [`TextDoc`]. Character ids for text that was already in the file use
//! space 0, so two peers who open the same document start from the same identities. New typing
//! uses this replica's space. CollabCraft still carries the ops; this module is the merge.

use std::collections::HashMap;

use craft_crdt::ids::Allocator;
use craft_crdt::text::{TextDoc, TextOp};
use serde::{Deserialize, Serialize};

/// What rides as `params` on a `crdt.text` command.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CrdtText {
    /// The paragraph, as [`wordcraft_engine::collab::scope_of`] spells it.
    pub scope: String,
    /// Inserts and deletes, in the order this replica produced them.
    pub ops: Vec<TextOp>,
}

/// One replica's paragraph texts.
pub struct Crdt {
    texts: HashMap<String, TextDoc>,
    alloc: Allocator,
    space: u32,
}

impl Crdt {
    /// A replica that will mint new character ids in `space` (never 0: that is the file).
    #[must_use]
    pub fn new(space: u32) -> Crdt {
        Crdt { texts: HashMap::new(), alloc: Allocator::new(), space: space.max(1) }
    }

    /// Switch the id space once the relay has named this peer.
    pub fn set_space(&mut self, space: u32) {
        self.space = space.max(1);
    }

    /// Load a paragraph from the file. Space-0 ids, so every replica that sees the same
    /// string starts with the same identities. A paragraph already loaded is left alone.
    pub fn seed(&mut self, scope: &str, text: &str) {
        if self.texts.contains_key(scope) {
            return;
        }
        let mut doc = TextDoc::new();
        if !text.is_empty() {
            let _ = doc.local_insert(0, text, &mut self.alloc, 0);
        }
        self.texts.insert(scope.to_owned(), doc);
    }

    /// Record a local insert and return the ops to send.
    pub fn local_insert(&mut self, scope: &str, char_pos: usize, s: &str) -> Vec<TextOp> {
        let space = self.space;
        let alloc = &mut self.alloc;
        self.texts.entry(scope.to_owned()).or_default().local_insert(char_pos, s, alloc, space)
    }

    /// Record a local delete and return the ops to send.
    pub fn local_delete(&mut self, scope: &str, char_pos: usize, len: usize) -> Vec<TextOp> {
        self.texts.entry(scope.to_owned()).or_default().local_delete(char_pos, len)
    }

    /// Apply remote ops and return the rendered paragraph.
    pub fn apply(&mut self, scope: &str, ops: &[TextOp]) -> String {
        let doc = self.texts.entry(scope.to_owned()).or_default();
        for op in ops {
            doc.apply(op);
        }
        doc.render()
    }

    /// The rendered paragraph, if we have one.
    #[must_use]
    pub fn render(&self, scope: &str) -> Option<String> {
        self.texts.get(scope).map(TextDoc::render)
    }
}

/// Byte offset in a UTF-8 string to a live-character index.
#[must_use]
pub fn char_pos(text: &str, byte: usize) -> usize {
    text.get(..byte.min(text.len())).map(|s| s.chars().count()).unwrap_or(0)
}

/// A replica space derived from the relay's peer id. Never 0.
#[must_use]
pub fn space_for(peer: &str) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in peer.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash.max(1)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn concurrent_inserts_in_one_paragraph_converge() {
        let mut a = Crdt::new(1);
        let mut b = Crdt::new(2);
        a.seed("/body/0", "hello");
        b.seed("/body/0", "hello");
        assert_eq!(a.render("/body/0").as_deref(), Some("hello"));
        assert_eq!(b.render("/body/0").as_deref(), Some("hello"));

        let ops_a = a.local_insert("/body/0", 5, " world");
        let ops_b = b.local_insert("/body/0", 5, " there");
        let rendered_a = a.apply("/body/0", &ops_b);
        let rendered_b = b.apply("/body/0", &ops_a);
        assert_eq!(rendered_a, rendered_b);
        assert_eq!(rendered_a, "hello there world");
    }

    #[test]
    fn opposite_order_is_the_same_string() {
        let mut a = Crdt::new(1);
        let mut b = Crdt::new(2);
        a.seed("/body/0", "");
        b.seed("/body/0", "");
        let ops_a = a.local_insert("/body/0", 0, "A");
        let ops_b = b.local_insert("/body/0", 0, "B");
        let from_a = a.apply("/body/0", &ops_b);
        let from_b = b.apply("/body/0", &ops_a);
        assert_eq!(from_a, from_b);
    }

    #[test]
    fn a_delete_tombstones_on_both_replicas() {
        let mut a = Crdt::new(1);
        let mut b = Crdt::new(2);
        a.seed("/body/0", "cat");
        b.seed("/body/0", "cat");
        let ops = a.local_delete("/body/0", 1, 1);
        assert_eq!(a.render("/body/0").as_deref(), Some("ct"));
        assert_eq!(b.apply("/body/0", &ops), "ct");
    }

    #[test]
    fn space_for_never_uses_the_file_space() {
        assert_ne!(space_for("alice"), 0);
        assert_ne!(space_for(""), 0);
        assert_eq!(space_for("alice"), space_for("alice"));
        assert_ne!(space_for("alice"), space_for("bob"));
    }

    #[test]
    fn char_pos_counts_characters_not_bytes() {
        assert_eq!(char_pos("héllo", 0), 0);
        assert_eq!(char_pos("héllo", 1), 1);
        assert_eq!(char_pos("héllo", 3), 2);
    }
}
