// RFN Edit (RFN01-83): what a set of IME edits means, kept free of COM and of
// Slint so it can be tested. `tsf.rs` uses it; RFN Edit's own test run includes
// this file too (`src/main.rs`), since this vendored crate has no test setup.

/// Which text the IME is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Source {
    Field,
    Document,
}

/// The stretch of `old` that became something else, and what it became:
/// `(start, old_end, inserted)` in UTF-16 units of `old`.
pub(crate) fn diff(old: &[u16], new: &[u16]) -> (usize, usize, Vec<u16>) {
    let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let room = old.len().min(new.len()) - prefix;
    let suffix = old
        .iter()
        .rev()
        .zip(new.iter().rev())
        .take(room)
        .take_while(|(a, b)| a == b)
        .count();
    (
        prefix,
        old.len() - suffix,
        new[prefix..new.len() - suffix].to_vec(),
    )
}

/// The byte offset in `text` of UTF-16 position `unit` (clamped).
pub(crate) fn byte_at(text: &str, unit: u32) -> usize {
    let mut units = 0u32;
    for (at, c) in text.char_indices() {
        if units >= unit {
            return at;
        }
        units += c.len_utf16() as u32;
    }
    text.len()
}

/// The UTF-16 position of byte offset `byte` in `text` (clamped to a char).
pub(crate) fn unit_at(text: &str, byte: usize) -> u32 {
    text.char_indices()
        .take_while(|(at, _)| *at < byte)
        .map(|(_, c)| c.len_utf16() as u32)
        .sum()
}

/// What a lock's edits ask Slint (and the document) to do.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Plan {
    /// The document is to select this stretch before anything else.
    pub(crate) choose: Option<(u32, u32)>,
    /// The event for the focused `TextInput`, if any.
    pub(crate) event: Option<Event>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Event {
    pub(crate) commit: bool,
    /// Committed text inserted at the caret (after `replace` is deleted).
    pub(crate) text: String,
    /// Bytes relative to Slint's caret.
    pub(crate) replace: Option<(i32, i32)>,
    pub(crate) preedit: String,
    /// Bytes within `preedit`.
    pub(crate) preedit_selection: Option<(i32, i32)>,
    /// Slint's caret afterwards, bytes in its new text.
    pub(crate) cursor: Option<i32>,
}

/// What the IME has to be told after [`Text::take`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Notice {
    Nothing,
    Selection,
    /// `start..old_end` of the old text became `start..new_end` (UTF-16).
    Text(u32, u32, u32),
}

/// The store's state: what Slint or the document holds (`base`), and what the
/// IME has made of it (`text`, with the composition in it).
#[derive(Clone, Debug, Default)]
pub(crate) struct Text {
    pub(crate) source: Option<Source>,
    pub(crate) base: Vec<u16>,
    /// Slint's own text, for byte offsets (fields only).
    pub(crate) field: String,
    pub(crate) base_cursor: u32,
    pub(crate) base_anchor: u32,
    pub(crate) text: Vec<u16>,
    pub(crate) sel: (u32, u32),
    pub(crate) comp: Option<(u32, u32)>,
    /// A composition was shown the last time Slint was told anything.
    pub(crate) composing: bool,
    /// The stretch of the document already chosen for this composition.
    pub(crate) chosen: Option<(u32, u32)>,
    /// The document's text before a commit it has not taken yet: read back
    /// unchanged, it is the document being late, not the commit undone.
    pub(crate) stale: Option<Vec<u16>>,
}

impl Text {
    pub(crate) fn committed(&self) -> (Vec<u16>, String) {
        match self.comp {
            Some((a, b)) => {
                let (a, b) = (a as usize, (b as usize).min(self.text.len()));
                let mut rest = self.text[..a].to_vec();
                rest.extend_from_slice(&self.text[b..]);
                (rest, String::from_utf16_lossy(&self.text[a..b]))
            }
            None => (self.text.clone(), String::new()),
        }
    }

    /// Work out what the IME's edits mean, and take them as the new base.
    pub(crate) fn plan(&mut self) -> Plan {
        let (committed, preedit) = self.committed();
        let (start, end, inserted) = diff(&self.base, &committed);
        let changed = start != end || !inserted.is_empty();
        let inserted_text = String::from_utf16_lossy(&inserted);
        let composing = self.comp.is_some();
        if !changed && !composing && !self.composing {
            return Plan::default();
        }
        let preedit_selection = self.comp.map(|(a, _)| {
            let local = |u: u32| byte_at(&preedit, u.saturating_sub(a)) as i32;
            (local(self.sel.0), local(self.sel.1))
        });
        let mut plan = Plan::default();
        match self.source.unwrap_or(Source::Field) {
            Source::Field => {
                let cursor = byte_at(&self.field, self.base_cursor) as i32;
                let replace = changed.then(|| {
                    (
                        byte_at(&self.field, start as u32) as i32 - cursor,
                        byte_at(&self.field, end as u32) as i32 - cursor,
                    )
                });
                let new_field = String::from_utf16_lossy(&committed);
                let caret = match self.comp {
                    Some((a, _)) => a,
                    None => self.sel.1.min(committed.len() as u32),
                };
                let caret_bytes = byte_at(&new_field, caret) as i32;
                let moved = changed || caret != self.base_cursor;
                plan.event = Some(Event {
                    commit: !composing,
                    text: inserted_text,
                    replace,
                    preedit,
                    preedit_selection,
                    cursor: moved.then_some(caret_bytes),
                });
                self.field = new_field;
                self.base = committed;
                self.base_cursor = caret;
                self.base_anchor = caret;
            }
            Source::Document
                if !composing
                    && let Some((from, to)) = self.chosen
                    && let Some(length) =
                        (committed.len() + (to - from) as usize).checked_sub(self.base.len())
                    && from as usize + length <= committed.len() =>
            {
                // A reconversion ends: **what it made replaces what it took**,
                // even when that is the same text — the stretch is selected in
                // the document, and an empty commit would leave it selected,
                // for the next Space to convert again (書き手の報告 2026-11-03).
                let made = &committed[from as usize..from as usize + length];
                plan.event = Some(Event {
                    commit: true,
                    text: String::from_utf16_lossy(made),
                    replace: None,
                    preedit: String::new(),
                    preedit_selection: None,
                    cursor: None,
                });
                if changed {
                    self.stale = Some(std::mem::take(&mut self.base));
                }
                self.base = committed;
                self.base_cursor = from + length as u32;
                self.base_anchor = self.base_cursor;
                self.chosen = None;
            }
            Source::Document => {
                let selection = (
                    self.base_cursor.min(self.base_anchor),
                    self.base_cursor.max(self.base_anchor),
                );
                let stretch = (start as u32, end as u32);
                if start != end && self.chosen != Some(stretch) && selection != stretch {
                    plan.choose = Some(stretch);
                }
                if start != end {
                    self.chosen = Some(stretch);
                }
                if composing {
                    plan.event = Some(Event {
                        commit: false,
                        text: String::new(),
                        replace: None,
                        preedit,
                        preedit_selection,
                        cursor: None,
                    });
                } else {
                    plan.event = Some(Event {
                        commit: true,
                        text: inserted_text,
                        replace: None,
                        preedit: String::new(),
                        preedit_selection: None,
                        cursor: None,
                    });
                    // What the document will hold once it has taken the commit;
                    // `refresh` reads it back from the document itself.
                    if changed {
                        self.stale = Some(std::mem::take(&mut self.base));
                    }
                    self.base = committed;
                    self.base_cursor = (start + inserted.len()) as u32;
                    self.base_anchor = self.base_cursor;
                    self.chosen = None;
                }
            }
        }
        self.composing = composing;
        plan
    }

    /// Take what Slint or the document holds now, and say what the IME has to
    /// be told.
    ///
    /// **Only what changed**: TSF keeps what it knows of the text — the
    /// reading each word was typed with — on the ranges it was told about, and
    /// "everything changed" throws it all away, which a reconversion may need.
    ///
    /// **Not while composing**: the composition is the IME's, and what it is
    /// compared against has to stay the text it was made in — a caret moved by
    /// a click, mid-composition, would otherwise read as the IME having
    /// rewritten a different paragraph. The caller reads again once it ends.
    pub(crate) fn take(
        &mut self,
        source: Source,
        base: Vec<u16>,
        field: String,
        cursor: u32,
        anchor: u32,
    ) -> Notice {
        if self.comp.is_some() {
            return Notice::Nothing;
        }
        let switched = self.source != Some(source);
        // The document read back before it took the commit: wait for it.
        // (Telling the IME the commit went away and came back drops what it
        // knows of the committed words.)
        if !switched && source == Source::Document && self.stale.as_ref() == Some(&base) {
            return Notice::Nothing;
        }
        self.stale = None;
        self.source = Some(source);
        self.base = base;
        self.field = field;
        self.base_cursor = cursor;
        self.base_anchor = anchor;
        let sel = (cursor.min(anchor), cursor.max(anchor));
        let (start, old_end, inserted) = diff(&self.text, &self.base);
        let text_changed = start != old_end || !inserted.is_empty();
        let notice = if switched || text_changed {
            if switched {
                Notice::Text(0, self.text.len() as u32, self.base.len() as u32)
            } else {
                Notice::Text(
                    start as u32,
                    old_end as u32,
                    (start + inserted.len()) as u32,
                )
            }
        } else if self.sel != sel {
            Notice::Selection
        } else {
            Notice::Nothing
        };
        self.text = self.base.clone();
        self.sel = sel;
        notice
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn units(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    fn field(text: &str, caret: u32) -> Text {
        let mut t = Text::default();
        t.take(Source::Field, units(text), text.into(), caret, caret);
        t
    }

    #[test]
    fn the_changed_stretch_is_what_lies_between_the_common_ends() {
        assert_eq!(diff(&units("abc"), &units("abc")), (3, 3, vec![]));
        assert_eq!(diff(&units("abc"), &units("aXc")), (1, 2, units("X")));
        assert_eq!(diff(&units("ab"), &units("abc")), (2, 2, units("c")));
        assert_eq!(diff(&units("aaa"), &units("aa")), (2, 3, vec![]));
    }

    #[test]
    fn byte_and_unit_offsets_agree_across_wide_characters() {
        let s = "あa😀b";
        assert_eq!(byte_at(s, 0), 0);
        assert_eq!(byte_at(s, 1), 3);
        assert_eq!(byte_at(s, 2), 4);
        assert_eq!(byte_at(s, 4), 8);
        assert_eq!(unit_at(s, 8), 4);
        assert_eq!(unit_at(s, 100), 5);
    }

    #[test]
    fn a_composition_in_a_field_is_a_preedit_with_its_caret() {
        let mut t = field("ab", 1);
        t.text = units("aのんb");
        t.comp = Some((1, 3));
        t.sel = (2, 2);
        let plan = t.plan();
        let event = plan.event.unwrap();
        assert!(!event.commit);
        assert_eq!(event.preedit, "のん");
        assert_eq!(event.preedit_selection, Some((3, 3)));
        assert_eq!(event.replace, None);
        assert_eq!(event.cursor, None);
        assert_eq!(t.field, "ab");
    }

    #[test]
    fn a_commit_in_a_field_inserts_at_the_caret() {
        let mut t = field("ab", 1);
        t.text = units("aのんb");
        t.comp = Some((1, 3));
        t.plan();
        t.text = units("a今日b");
        t.comp = None;
        t.sel = (3, 3);
        let event = t.plan().event.unwrap();
        assert!(event.commit);
        assert_eq!(event.text, "今日");
        assert_eq!(event.replace, Some((0, 0)));
        assert_eq!(event.preedit, "");
        assert_eq!(event.cursor, Some(7));
        assert_eq!(t.field, "a今日b");
    }

    #[test]
    fn a_reconversion_in_a_field_takes_the_stretch_out_of_its_text() {
        let mut t = field("今日は", 3);
        // The IME makes the first two characters the composition again.
        t.comp = Some((0, 2));
        t.sel = (2, 2);
        let event = t.plan().event.unwrap();
        assert!(!event.commit);
        assert_eq!(event.replace, Some((-9, -3)));
        assert_eq!(event.preedit, "今日");
        assert_eq!(event.cursor, Some(0));
        assert_eq!(t.field, "は");
    }

    #[test]
    fn a_reconversion_in_a_document_chooses_the_stretch_once() {
        let mut t = Text::default();
        t.take(Source::Document, units("今日は晴れ"), String::new(), 5, 5);
        t.comp = Some((0, 2));
        let plan = t.plan();
        assert_eq!(plan.choose, Some((0, 2)));
        assert_eq!(plan.event.unwrap().preedit, "今日");
        t.sel = (1, 1);
        assert_eq!(t.plan().choose, None);
        // Committed: the document takes the text in place of what was chosen.
        t.text = units("京は晴れ");
        t.comp = None;
        let plan = t.plan();
        assert_eq!(plan.choose, None);
        let event = plan.event.unwrap();
        assert!(event.commit);
        assert_eq!(event.text, "京");
    }

    #[test]
    fn a_composition_over_the_selection_needs_no_choosing() {
        let mut t = Text::default();
        t.take(Source::Document, units("abcd"), String::new(), 3, 1);
        t.text = units("aのd");
        t.comp = Some((1, 2));
        assert_eq!(t.plan().choose, None);
    }

    #[test]
    fn a_cancelled_composition_clears_the_preedit() {
        let mut t = field("ab", 1);
        t.text = units("aかb");
        t.comp = Some((1, 2));
        t.plan();
        t.text = units("ab");
        t.comp = None;
        let event = t.plan().event.unwrap();
        assert!(event.commit);
        assert_eq!(event.text, "");
        assert_eq!(event.preedit, "");
        assert_eq!(event.replace, None);
    }

    #[test]
    fn a_caret_moved_mid_composition_does_not_change_what_it_is_compared_with() {
        let mut t = Text::default();
        t.take(Source::Document, units(""), String::new(), 0, 0);
        t.text = units("のんら");
        t.comp = Some((0, 3));
        t.plan();
        // The writer clicked into another paragraph while converting.
        assert_eq!(
            t.take(
                Source::Document,
                units("# 見出しの段落"),
                String::new(),
                3,
                3
            ),
            Notice::Nothing
        );
        // The IME commits the composition where it was made.
        t.comp = None;
        let plan = t.plan();
        assert_eq!(plan.choose, None);
        let event = plan.event.unwrap();
        assert!(event.commit);
        assert_eq!(event.text, "のんら");
        // Then the new paragraph is taken.
        assert_eq!(
            t.take(
                Source::Document,
                units("# 見出しの段落"),
                String::new(),
                3,
                3
            ),
            Notice::Text(0, 3, 8)
        );
        assert_eq!(t.text, units("# 見出しの段落"));
    }

    #[test]
    fn only_the_changed_range_is_reported() {
        let mut t = Text::default();
        t.take(
            Source::Document,
            units("今日はいい天気"),
            String::new(),
            3,
            3,
        );
        // The document took a commit of「とても」at the caret.
        assert_eq!(
            t.take(
                Source::Document,
                units("今日はとてもいい天気"),
                String::new(),
                6,
                6
            ),
            Notice::Text(3, 3, 6)
        );
        // Switching to a field is everything.
        assert_eq!(
            t.take(Source::Field, units("x"), "x".into(), 1, 1),
            Notice::Text(0, 10, 1)
        );
    }

    #[test]
    fn a_document_read_back_before_it_took_the_commit_is_not_a_change() {
        let mut t = Text::default();
        t.take(Source::Document, units(""), String::new(), 0, 0);
        t.text = units("のち");
        t.comp = Some((0, 2));
        t.plan();
        t.text = units("後");
        t.comp = None;
        t.sel = (1, 1);
        assert!(t.plan().event.unwrap().commit);
        // Still the old text: the document has not taken the commit yet.
        assert_eq!(
            t.take(Source::Document, units(""), String::new(), 0, 0),
            Notice::Nothing
        );
        assert_eq!(t.text, units("後"));
        // Now it has: nothing to tell either.
        assert_eq!(
            t.take(Source::Document, units("後"), String::new(), 1, 1),
            Notice::Nothing
        );
        // From here on, the old text is a real change.
        assert_eq!(
            t.take(Source::Document, units(""), String::new(), 0, 0),
            Notice::Text(0, 1, 0)
        );
    }

    #[test]
    fn a_reconversion_committed_unchanged_still_replaces_the_selection() {
        let mut t = Text::default();
        // 「今日は」selected, and converted again.
        t.take(Source::Document, units("今日は"), String::new(), 3, 0);
        t.comp = Some((0, 3));
        assert_eq!(t.plan().choose, None);
        // Committed as it was.
        t.comp = None;
        t.sel = (3, 3);
        let event = t.plan().event.unwrap();
        assert!(event.commit);
        assert_eq!(event.text, "今日は");
        assert_eq!((t.base_cursor, t.base_anchor), (3, 3));
    }

    #[test]
    fn a_reconversion_committed_changed_replaces_the_whole_selection() {
        let mut t = Text::default();
        t.take(Source::Document, units("今日は晴れ"), String::new(), 3, 0);
        t.comp = Some((0, 3));
        t.plan();
        t.text = units("きょうは晴れ");
        t.comp = Some((0, 4));
        t.plan();
        t.comp = None;
        let plan = t.plan();
        assert_eq!(plan.choose, None);
        assert_eq!(plan.event.unwrap().text, "きょうは");
    }

    #[test]
    fn nothing_changed_is_nothing_to_say() {
        let mut t = field("ab", 1);
        assert_eq!(t.plan(), Plan::default());
        assert_eq!(
            t.take(Source::Field, units("ab"), "ab".into(), 1, 1),
            Notice::Nothing
        );
        assert_eq!(
            t.take(Source::Field, units("ab"), "ab".into(), 2, 2),
            Notice::Selection
        );
        assert_eq!(
            t.take(Source::Field, units("abc"), "abc".into(), 3, 3),
            Notice::Text(2, 2, 3)
        );
    }
}
