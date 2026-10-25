//! Undo/redo history with typing coalescing.
//!
//! The history only records edits; [`crate::Buffer`] applies them. Every
//! distinct document state reachable through the history has a unique id,
//! which lets the buffer decide "dirty" by comparing the current state id
//! with the id that was current at the last save.

/// One primitive replacement: at char `start`, `removed` was replaced by
/// `inserted`. Lengths are cached in chars.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Edit {
    pub start: usize,
    pub removed: String,
    pub inserted: String,
    pub removed_chars: usize,
    pub inserted_chars: usize,
}

impl Edit {
    pub fn new(start: usize, removed: String, inserted: String) -> Self {
        let removed_chars = removed.chars().count();
        let inserted_chars = inserted.chars().count();
        Self {
            start,
            removed,
            inserted,
            removed_chars,
            inserted_chars,
        }
    }

    /// The single char this edit inserts, if it is a pure one-char insert.
    fn single_inserted(&self) -> Option<char> {
        (self.removed_chars == 0 && self.inserted_chars == 1)
            .then(|| self.inserted.chars().next())
            .flatten()
    }

    /// The single char this edit removes, if it is a pure one-char delete.
    fn single_removed(&self) -> Option<char> {
        (self.inserted_chars == 0 && self.removed_chars == 1)
            .then(|| self.removed.chars().next())
            .flatten()
    }
}

/// How a group may grow when the next edit arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GroupKind {
    /// Consecutive single-char inserts, each right after the previous.
    Typing,
    /// Consecutive single-char deletes moving left (Backspace).
    Backspace,
    /// Consecutive single-char deletes at the same position (Delete key).
    ForwardDelete,
    /// Anything else; never extended.
    Other,
}

#[derive(Debug, Clone)]
pub(crate) struct Group {
    id: u64,
    kind: GroupKind,
    sealed: bool,
    pub edits: Vec<Edit>,
}

/// Most text the undo stack keeps (removed plus inserted bytes); older
/// steps are dropped beyond it.
pub(crate) const MAX_UNDO_BYTES: usize = 64 * 1024 * 1024;

impl Group {
    /// Text bytes this step holds.
    fn bytes(&self) -> usize {
        self.edits
            .iter()
            .map(|e| e.removed.len() + e.inserted.len())
            .sum()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct History {
    undo: Vec<Group>,
    redo: Vec<Group>,
    next_id: u64,
    /// State id of the document at the bottom of the undo stack.
    base_id: u64,
    limit: usize,
}

impl History {
    pub fn new(limit: usize) -> Self {
        Self {
            undo: Vec::new(),
            redo: Vec::new(),
            next_id: 1,
            base_id: 0,
            limit: limit.max(1),
        }
    }

    fn fresh_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Id of the current document state.
    pub fn state_id(&self) -> u64 {
        self.undo.last().map_or(self.base_id, |g| g.id)
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn set_limit(&mut self, limit: usize) {
        self.limit = limit.max(1);
        self.enforce_limit();
    }

    /// Prevents the next edit from merging into the current top group.
    pub fn seal(&mut self) {
        if let Some(top) = self.undo.last_mut() {
            top.sealed = true;
        }
    }

    /// Records one edit, coalescing it with the previous one when it
    /// continues a run of typing or deleting.
    pub fn record(&mut self, edit: Edit) {
        self.redo.clear();
        let kind = classify(&edit);
        if kind != GroupKind::Other {
            let new_id = self.fresh_id();
            if let Some(top) = self.undo.last_mut() {
                if !top.sealed && compatible(top.kind, kind) && try_merge(top, &edit) {
                    // The state after the merged group differs from the old
                    // one, so it needs a new identity.
                    top.id = new_id;
                    return;
                }
            }
            self.push(Group {
                id: new_id,
                kind,
                sealed: false,
                edits: vec![edit],
            });
        } else {
            self.record_group(vec![edit]);
        }
    }

    /// Records several edits (applied in order) as one undo step.
    pub fn record_group(&mut self, edits: Vec<Edit>) {
        if edits.is_empty() {
            return;
        }
        self.redo.clear();
        self.seal();
        let id = self.fresh_id();
        self.push(Group {
            id,
            kind: GroupKind::Other,
            sealed: true,
            edits,
        });
    }

    fn push(&mut self, group: Group) {
        self.undo.push(group);
        self.enforce_limit();
    }

    fn enforce_limit(&mut self) {
        let mut excess = self.undo.len().saturating_sub(self.limit);
        // Also cap the memory held by undo steps (newest step always kept).
        let mut bytes: usize = self.undo.iter().map(Group::bytes).sum();
        while bytes > MAX_UNDO_BYTES && excess + 1 < self.undo.len() {
            bytes -= self.undo[excess].bytes();
            excess += 1;
        }
        if excess > 0 {
            // The new bottom state is the one right after the newest dropped
            // group, so it inherits that group's id.
            self.base_id = self.undo[excess - 1].id;
            self.undo.drain(..excess);
        }
    }

    /// Pops the most recent group for undoing. The caller must apply the
    /// inverse of its edits in reverse order.
    pub fn undo(&mut self) -> Option<&Group> {
        let mut group = self.undo.pop()?;
        group.sealed = true;
        self.redo.push(group);
        self.redo.last()
    }

    /// Pops the most recently undone group for redoing. The caller must
    /// re-apply its edits in order.
    pub fn redo(&mut self) -> Option<&Group> {
        let group = self.redo.pop()?;
        self.undo.push(group);
        self.undo.last()
    }
}

fn classify(edit: &Edit) -> GroupKind {
    if edit.single_inserted().is_some_and(|c| c != '\n') {
        GroupKind::Typing
    } else if edit.single_removed().is_some_and(|c| c != '\n') {
        // Backspace vs forward delete is decided at merge time; tag as
        // Backspace and let try_merge reclassify if needed.
        GroupKind::Backspace
    } else {
        GroupKind::Other
    }
}

/// Whether a group of kind `group` may absorb an edit classified as `edit`.
/// Single-char deletes are classified as `Backspace`; whether they continue
/// a backspace or forward-delete run is decided by position in `try_merge`.
fn compatible(group: GroupKind, edit: GroupKind) -> bool {
    use GroupKind::*;
    matches!(
        (group, edit),
        (Typing, Typing) | (Backspace | ForwardDelete, Backspace)
    )
}

fn try_merge(top: &mut Group, edit: &Edit) -> bool {
    let [last] = top.edits.as_mut_slice() else {
        return false;
    };
    match top.kind {
        GroupKind::Typing => {
            if last.start + last.inserted_chars == edit.start {
                last.inserted.push_str(&edit.inserted);
                last.inserted_chars += 1;
                return true;
            }
            false
        }
        GroupKind::Backspace | GroupKind::ForwardDelete => {
            let backspace = edit.start + 1 == last.start;
            let forward = edit.start == last.start;
            let allowed_back = top.kind == GroupKind::Backspace || last.removed_chars == 1;
            let allowed_fwd = top.kind == GroupKind::ForwardDelete || last.removed_chars == 1;
            if backspace && allowed_back {
                last.start = edit.start;
                last.removed.insert_str(0, &edit.removed);
                last.removed_chars += 1;
                top.kind = GroupKind::Backspace;
                true
            } else if forward && allowed_fwd {
                last.removed.push_str(&edit.removed);
                last.removed_chars += 1;
                top.kind = GroupKind::ForwardDelete;
                true
            } else {
                false
            }
        }
        GroupKind::Other => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ins(at: usize, s: &str) -> Edit {
        Edit::new(at, String::new(), s.to_string())
    }

    fn del(at: usize, s: &str) -> Edit {
        Edit::new(at, s.to_string(), String::new())
    }

    #[test]
    fn typing_coalesces() {
        let mut h = History::new(100);
        h.record(ins(0, "a"));
        h.record(ins(1, "b"));
        h.record(ins(2, "c"));
        assert_eq!(h.undo.len(), 1);
        assert_eq!(h.undo[0].edits[0].inserted, "abc");
    }

    #[test]
    fn newline_and_jumps_break_groups() {
        let mut h = History::new(100);
        h.record(ins(0, "a"));
        h.record(ins(1, "\n"));
        h.record(ins(2, "b"));
        h.record(ins(0, "z")); // not contiguous
        assert_eq!(h.undo.len(), 4);
    }

    #[test]
    fn seal_breaks_group() {
        let mut h = History::new(100);
        h.record(ins(0, "a"));
        h.seal();
        h.record(ins(1, "b"));
        assert_eq!(h.undo.len(), 2);
    }

    #[test]
    fn backspace_and_forward_delete_coalesce() {
        let mut h = History::new(100);
        // "abc", backspace from end: removes c@2, b@1, a@0
        h.record(del(2, "c"));
        h.record(del(1, "b"));
        h.record(del(0, "a"));
        assert_eq!(h.undo.len(), 1);
        assert_eq!(h.undo[0].edits[0].removed, "abc");
        assert_eq!(h.undo[0].edits[0].start, 0);

        h.seal();
        // forward delete at 0 three times on "xyz"
        h.record(del(0, "x"));
        h.record(del(0, "y"));
        h.record(del(0, "z"));
        assert_eq!(h.undo.len(), 2);
        assert_eq!(h.undo[1].edits[0].removed, "xyz");
    }

    #[test]
    fn merging_changes_state_id() {
        let mut h = History::new(100);
        h.record(ins(0, "a"));
        let before = h.state_id();
        h.record(ins(1, "b"));
        assert_ne!(before, h.state_id());
    }

    #[test]
    fn limit_drops_oldest_and_moves_base() {
        let mut h = History::new(2);
        h.record_group(vec![ins(0, "a")]);
        let first = h.state_id();
        h.record_group(vec![ins(1, "b")]);
        h.record_group(vec![ins(2, "c")]);
        assert_eq!(h.undo.len(), 2);
        assert!(h.undo().is_some());
        assert!(h.undo().is_some());
        assert!(h.undo().is_none());
        // Bottom state is "after a", identified by the dropped group's id.
        assert_eq!(h.state_id(), first);
    }

    #[test]
    fn new_edit_clears_redo() {
        let mut h = History::new(10);
        h.record(ins(0, "a"));
        h.undo();
        assert!(h.can_redo());
        h.record(ins(0, "b"));
        assert!(!h.can_redo());
    }

    #[test]
    fn undo_memory_is_capped() {
        let mut h = History::new(100);
        let big = "x".repeat(MAX_UNDO_BYTES / 2 + 1);
        h.record_group(vec![ins(0, &big)]);
        h.record_group(vec![ins(0, "a")]);
        h.record_group(vec![ins(0, &big)]);
        // The first big step no longer fits next to the second one.
        assert!(h.undo().is_some());
        assert!(h.undo().is_some());
        assert!(h.undo().is_none());
    }
}
