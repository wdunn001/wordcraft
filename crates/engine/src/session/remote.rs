//! Edits that arrive from another peer, applied without echoing them back or touching this
//! peer's undo history.

use serde_json::Value;
use wordcraft_doc::Pos;

use super::{CmdError, Selection, Session};

impl Session {
    /// Runs a command that arrived from another peer.
    ///
    /// Three things differ from [`Session::run`]. It is not reported to the
    /// [`CollabSink`](crate::CollabSink), which is what stops an edit bouncing between two peers
    /// forever. It only accepts a [`crate::collab::shareable`] command, so a peer cannot open,
    /// save or undo on this machine. And it runs against `at`, the selection the sender had,
    /// because a WordCraft command says only "here": applying somebody else's Backspace at the
    /// reader's own caret would delete the reader's own text.
    ///
    /// The reader's selection is put back afterwards, moved by whatever the edit did to the text
    /// before it in the same paragraph (see [`crate::collab::follow`]). An edit that split or
    /// joined paragraphs cannot be followed that way, and the caret is clamped rather than
    /// guessed at.
    ///
    /// Undo stays local. The remote edit is not recorded on this peer's stack, and local history
    /// is cleared because a snapshot taken before it would wipe it if restored.
    pub fn apply_remote(&mut self, id: &str, params: &Value, at: Option<Selection>) -> Result<Value, CmdError> {
        let mutates = self.registry.get(id).map(|s| s.mutates).unwrap_or(false);
        if !crate::collab::shareable(id, mutates) {
            return Err(CmdError::Failed(format!("{id} is not a shared edit")));
        }
        let was = std::mem::replace(&mut self.applying_remote, true);
        // The reader's own state: a remote edit is not theirs to repeat, record or go back to,
        // and never joins their typing.
        let last_command = self.last_command.clone();
        let recording_len = self.recording.as_ref().map(|(_, s)| s.len());
        let (marks, step) = (self.edit_marks.clone(), self.go_back_step);
        self.typing_open = false;
        let mine = self.sel.clone();
        let there = at.unwrap_or_else(|| mine.clone());
        let edit = there.ordered().0;
        // Read before the edit and again after it: the difference is how much text appeared or
        // went away, whatever the command was. Only the reader's own paragraph matters.
        let same = mine.focus.story == edit.story && mine.focus.path == edit.path;
        let before = if same { self.para_len(&mine.focus) } else { None };
        self.sel = there;
        self.clamp_selection();
        let out = self.run(id, params);
        self.typing_open = false;
        self.last_command = last_command;
        if let (Some((_, steps)), Some(n)) = (self.recording.as_mut(), recording_len) {
            steps.truncate(n);
        }
        self.edit_marks = marks;
        self.go_back_step = step;
        let grew = match (before, self.para_len(&mine.focus)) {
            (Some(before), Some(after)) => (after as isize) - (before as isize),
            _ => 0,
        };
        self.sel = crate::collab::follow(mine, &edit, grew);
        self.clamp_selection();
        self.applying_remote = was;
        if out.is_ok() {
            self.history.clear();
            self.redo.clear();
        }
        out
    }

    /// Replace one paragraph's characters with the rendered CRDT text.
    ///
    /// Typing from another peer arrives as `crdt.text` operations. Those merge in the app's
    /// `TextDoc`; this writes the result into the document. It is not reported to
    /// [`CollabSink`](crate::CollabSink), so it does not bounce. Formatting collapses to one run:
    /// v1 is characters, not styles.
    pub fn apply_crdt_paragraph(&mut self, scope: &str, text: &str) -> Result<(), CmdError> {
        let Some(at) = crate::collab::parse_scope(scope) else {
            return Err(CmdError::Failed(format!("unknown paragraph {scope}")));
        };
        let was = std::mem::replace(&mut self.applying_remote, true);
        let para = match self.doc.para_mut(at.story, &at.path) {
            Ok(p) => p,
            Err(e) => {
                self.applying_remote = was;
                return Err(e.into());
            }
        };
        para.text = text.to_owned();
        let props = para.mark.clone();
        para.runs = if para.text.is_empty() {
            Vec::new()
        } else {
            vec![wordcraft_doc::Run { len: para.text.len(), props }]
        };
        para.touch();
        self.clamp_selection();
        self.applying_remote = was;
        self.touch();
        self.history.clear();
        self.redo.clear();
        Ok(())
    }

    /// How long the paragraph a position names is, when it names one.
    fn para_len(&self, at: &Pos) -> Option<usize> {
        self.doc.para_at(at).map(|p| p.len())
    }
}
