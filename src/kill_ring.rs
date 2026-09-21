//! The Kill Ring (要件 11.6).
//!
//! **Not the clipboard.** 要件 11.6 asks for a list of strings that lives
//! inside the editor and is kept apart from Windows', so that `Ctrl+C` and
//! `Alt+W` do not tread on each other — the writer can carry one thing between
//! programs and another between paragraphs. `src/clipboard.rs` is the other
//! one, and the two share nothing but the operations that reach them.
//!
//! Pure Rust: a list of strings and a place in it. What a kill *is* — how far
//! `Ctrl+K` reaches, where a yank lands — belongs to the editor, not here.

/// How many kills are kept.
///
/// **要件 11.6 does not say**, and no requirement rests on the number. It is
/// here only so that a long session cannot grow the list without bound; the
/// writer who reaches thirty kills back is not the writer this serves.
const KEPT: usize = 30;

/// What the writer asked of the ring (要件 11.4).
///
/// The numbers are the ones the key rows send, in the order the requirement's
/// table lists the keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KillAction {
    /// `Ctrl+K`: from the caret to the end of the line.
    ToLineEnd,
    /// `Alt+W`: the selection, left where it is.
    Copy,
    /// `Alt+X`: the selection, taken out.
    Cut,
    /// `Alt+Y`: put the newest kill in.
    Yank,
    /// `Alt+Shift+Y`: put the one before it in instead.
    YankOlder,
}

impl KillAction {
    pub fn from_index(index: i32) -> Option<Self> {
        match index {
            0 => Some(Self::ToLineEnd),
            1 => Some(Self::Copy),
            2 => Some(Self::Cut),
            3 => Some(Self::Yank),
            4 => Some(Self::YankOlder),
            _ => None,
        }
    }
}

/// The kills, newest first, and where the last yank read from.
#[derive(Default)]
pub struct KillRing {
    entries: Vec<String>,
    /// Which entry the last yank handed out. Only [`older`](KillRing::older)
    /// reads it, and only straight after a yank.
    at: usize,
}

impl KillRing {
    /// Query availability without changing the position used by older().
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    /// Add a kill, and start the ring's reading again at the top.
    ///
    /// **Empty text is not a kill.** `Ctrl+K` at the very end of a document has
    /// nothing to take, and an empty entry would push a real one out of reach
    /// while looking like a kill that lost its text.
    ///
    /// The same text twice in a row is kept once: a writer who kills the same
    /// line again has not made a second thing to come back to, and 要件 12.4
    /// settled the same question the same way for the draft's history.
    pub fn add(&mut self, text: String) {
        if text.is_empty() {
            return;
        }
        self.at = 0;
        if self.entries.first().is_some_and(|first| *first == text) {
            return;
        }
        self.entries.insert(0, text);
        self.entries.truncate(KEPT);
    }

    /// The newest kill, for `Alt+Y`.
    pub fn newest(&mut self) -> Option<&str> {
        self.at = 0;
        self.entries.first().map(String::as_str)
    }

    /// The kill before the one last handed out, for `Alt+Shift+Y`.
    ///
    /// **It comes round.** Pressing on past the oldest returns to the newest
    /// rather than stopping, which is what makes the ring a ring — a writer who
    /// went one too far can keep going rather than start over.
    pub fn older(&mut self) -> Option<&str> {
        if self.entries.is_empty() {
            return None;
        }
        self.at = (self.at + 1) % self.entries.len();
        self.entries.get(self.at).map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::KillAction;
    use super::KillRing;

    #[test]
    fn availability_does_not_rewind_the_previous_yank() {
        let mut ring = KillRing::default();
        assert!(ring.is_empty());
        for value in ["one", "two", "three"] {
            ring.add(value.into());
        }
        assert_eq!(ring.newest(), Some("three"));
        assert_eq!(ring.older(), Some("two"));
        assert!(!ring.is_empty());
        assert_eq!(ring.older(), Some("one"));
    }

    #[test]
    fn the_newest_kill_is_the_one_a_yank_gets() {
        let mut ring = KillRing::default();
        ring.add("one".into());
        ring.add("two".into());

        assert_eq!(ring.newest(), Some("two"));
    }

    #[test]
    fn stepping_back_walks_towards_the_oldest_and_comes_round() {
        let mut ring = KillRing::default();
        ring.add("one".into());
        ring.add("two".into());
        ring.add("three".into());

        assert_eq!(ring.newest(), Some("three"));
        assert_eq!(ring.older(), Some("two"));
        assert_eq!(ring.older(), Some("one"));
        assert_eq!(ring.older(), Some("three"));
    }

    /// A yank starts the reading again, so a step back after it is a step back
    /// from the newest and not from wherever the last walk stopped.
    #[test]
    fn a_yank_starts_the_reading_again_at_the_top() {
        let mut ring = KillRing::default();
        ring.add("one".into());
        ring.add("two".into());

        assert_eq!(ring.newest(), Some("two"));
        assert_eq!(ring.older(), Some("one"));
        assert_eq!(ring.newest(), Some("two"));
        assert_eq!(ring.older(), Some("one"));
    }

    #[test]
    fn an_empty_kill_is_not_kept() {
        let mut ring = KillRing::default();
        ring.add("one".into());
        ring.add(String::new());

        assert_eq!(ring.newest(), Some("one"));
    }

    #[test]
    fn the_same_text_twice_running_is_kept_once() {
        let mut ring = KillRing::default();
        ring.add("line".into());
        ring.add("line".into());

        assert_eq!(ring.newest(), Some("line"));
        assert_eq!(
            ring.older(),
            Some("line"),
            "there is only one to come round to"
        );
    }

    #[test]
    fn an_empty_ring_hands_out_nothing() {
        let mut ring = KillRing::default();

        assert_eq!(ring.newest(), None);
        assert_eq!(ring.older(), None);
    }

    #[test]
    fn only_the_five_keys_name_an_action() {
        assert_eq!(KillAction::from_index(0), Some(KillAction::ToLineEnd));
        assert_eq!(KillAction::from_index(4), Some(KillAction::YankOlder));
        assert_eq!(KillAction::from_index(5), None);
        assert_eq!(KillAction::from_index(-1), None);
    }
}
