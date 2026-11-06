use crate::saving::{Harness, edit, open_under, scratch_directory};
use crate::*;
use std::fs;

#[test]
fn an_overwrite_rechecks_dirty_tabs_and_then_shares_the_source_document() {
    let root = scratch_directory("move-over-tabs");
    let origin = root.join("source");
    let destination = root.join("destination");
    fs::create_dir(&origin).unwrap();
    fs::create_dir(&destination).unwrap();
    let from = origin.join("memo.md");
    let to = destination.join("memo.md");
    let (harness, source) = Harness::new(|weak| open_under(&origin, "memo.md", "source", weak));
    let (window, live) = (&harness.window, &harness.live);
    let id = PaneId::from_index(0);
    let old = open_under(&destination, "memo.md", "destination", window.as_weak());
    live.tabs.borrow_mut().panes[0]
        .tabs
        .push(PaneTab::showing(window, id, old.clone()));
    live.folder.borrow_mut().root = Some(root.clone());
    *live.tree_paths.borrow_mut() = vec![from.clone(), destination];
    drop_tree_row(window, live, 0, 1);
    assert!(matches!(
        *live.pending.borrow(),
        Some(Question::ReplaceOnMove(..))
    ));

    edit(&old, " edited");
    answer_question(window, live, 0);
    assert_eq!(fs::read_to_string(&to).unwrap(), "destination");
    assert_eq!(old.text.borrow().as_str(), "destination edited");
    assert_eq!(fs::read_to_string(&from).unwrap(), "source");

    saving::reload_document(window, live, &old);
    edit(&source, " edited");
    let documents = move_destination_documents(live, &to);
    assert!(can_replace_move_target(window, &documents));
    let overwrite = file_tree::Overwrite {
        version: disk_entry::Version::read(&to).unwrap(),
        recycle: &mut |path| fs::remove_file(path).is_ok(),
    };
    carry_entry(window, live, &from, &to, Some(overwrite), documents).unwrap();
    let tabs = live.tabs.borrow();
    assert!(
        tabs.panes[0]
            .tabs
            .iter()
            .all(|tab| Rc::ptr_eq(&tab.document, &source))
    );
    assert_eq!(source.file.borrow().path(), Some(to.as_path()));
    assert_eq!(source.text.borrow().as_str(), "source edited");
    assert_eq!(fs::read_to_string(&to).unwrap(), "source");
    assert!(!from.exists());
}

#[test]
fn a_failed_recovery_migration_keeps_the_old_copy_until_retry() {
    let root = scratch_directory("move-work-copy");
    let (harness, document) = Harness::new(|weak| open_under(&root, "a.md", "本文", weak));
    let (window, live) = (&harness.window, &harness.live);
    window.set_autosave(true);
    edit(&document, " 追記");
    let directory = app_data::work_directory().unwrap();
    let previous = work_identity(&document.file.borrow());
    saving::write_work_copy_of(window, live, &document);
    assert_eq!(saving::flush_work_copies(window, live), 0);
    let old = directory.join(app_data::work_file_name(&previous));
    assert!(old.is_file());

    let renamed = root.join("b.md");
    let new = directory.join(app_data::work_file_name(&app_data::WorkCopy {
        origin: Some(renamed.clone()),
        ..Default::default()
    }));
    fs::create_dir(&new).unwrap();
    move_entry(window, live, &root.join("a.md"), &renamed).unwrap();
    assert_eq!(saving::flush_work_copies(window, live), 1);
    assert!(old.is_file());
    assert_eq!(app_data::read_all_in(&directory)[0].text, "本文 追記");

    fs::remove_dir(&new).unwrap();
    let final_name = root.join("B.md");
    move_entry(window, live, &renamed, &final_name).unwrap();
    assert_eq!(saving::flush_work_copies(window, live), 0);
    let copies = app_data::read_all_in(&directory);
    assert_eq!(copies.len(), 1);
    assert_eq!(copies[0].origin.as_deref(), Some(final_name.as_path()));
    assert_eq!(copies[0].text, "本文 追記");
    assert!(!old.exists());
}
