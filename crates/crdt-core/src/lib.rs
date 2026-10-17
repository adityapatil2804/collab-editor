//! Sequence CRDT for collaborative text editing.
//!
//! - [`Doc`]: the production replica (Fugue merge rule + block/Fenwick storage).
//! - [`NaiveDoc`]: the same algorithm over a plain `Vec`, used as a test oracle.
//!
//! Ops may arrive late, out of order or duplicated: ops with missing
//! dependencies are buffered, and duplicates are ignored. Two replicas that have
//! applied the same set of ops always show the same text.

mod replica;
mod storage;
mod types;

pub use replica::Replica;
pub use storage::{BlockStorage, Storage, VecStorage};
pub use types::*;

pub type Doc = Replica<BlockStorage>;
pub type NaiveDoc = Replica<VecStorage>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_edits() {
        let mut d = Doc::new(1);
        d.insert_str(0, "helo");
        d.insert(3, 'l');
        assert_eq!(d.text(), "hello");
        d.delete(0);
        assert_eq!(d.text(), "ello");
        assert_eq!(d.tombstone_count(), 1);
    }

    #[test]
    fn cat_example_converges() {
        let mut a = Doc::new(1);
        let mut b = Doc::new(2);
        for op in a.insert_str(0, "cat") {
            b.apply(op);
        }
        let op_a = a.insert(3, 's'); // Alice: "cats"
        let op_b = b.delete(0); // Bob: "at"
        a.apply(op_b);
        b.apply(op_a);
        assert_eq!(a.text(), "ats");
        assert_eq!(a.text(), b.text());
    }

    #[test]
    fn duplicates_and_out_of_order() {
        let mut a = Doc::new(1);
        let ops = a.insert_str(0, "abc");
        let mut b = Doc::new(2);
        assert_eq!(b.apply(ops[2].clone()), ApplyResult::Buffered);
        assert_eq!(b.apply(ops[1].clone()), ApplyResult::Buffered);
        assert_eq!(b.apply(ops[0].clone()), ApplyResult::Applied);
        assert_eq!(b.apply(ops[1].clone()), ApplyResult::Duplicate);
        assert_eq!(b.text(), "abc");
        assert_eq!(b.pending_len(), 0);
    }

    fn concurrent_runs(type_backwards: bool) -> String {
        let mut a = Doc::new(1);
        let mut b = Doc::new(2);
        for (doc, word) in [(&mut a, "abc"), (&mut b, "xyz")] {
            if type_backwards {
                // Type the word right-to-left, always inserting at position 0.
                for ch in word.chars().rev() {
                    doc.insert(0, ch);
                }
            } else {
                doc.insert_str(0, word);
            }
        }
        a.sync(&mut b);
        assert_eq!(a.text(), b.text());
        a.text()
    }

    #[test]
    fn concurrent_forward_typing_does_not_interleave() {
        let t = concurrent_runs(false);
        assert!(t == "abcxyz" || t == "xyzabc", "got {t}");
    }

    #[test]
    fn concurrent_backward_typing_does_not_interleave() {
        // Plain RGA produces "axbycz"-style interleaving here; Fugue must not.
        let t = concurrent_runs(true);
        assert!(t == "abcxyz" || t == "xyzabc", "got {t}");
    }

    #[test]
    fn patches_describe_remote_changes() {
        let mut a = Doc::new(1);
        let mut b = Doc::new(2);
        let ops = a.insert_str(0, "hi");
        let mut patches = Vec::new();
        for op in ops.into_iter().rev() {
            b.apply_collect(op, &mut patches); // reversed: first is buffered
        }
        assert_eq!(
            patches,
            vec![
                Patch::Insert { pos: 0, ch: 'h' },
                Patch::Insert { pos: 1, ch: 'i' }
            ]
        );
    }
}
