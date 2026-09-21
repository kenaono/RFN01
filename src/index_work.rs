//! Bounded parallel directory traversal. Workers never write caches or UI state.
use std::{
    collections::{HashMap, VecDeque},
    fs,
    path::PathBuf,
    sync::{Condvar, Mutex, mpsc},
    time::{Duration, Instant},
};

pub type Inventory = Mutex<HashMap<PathBuf, (Instant, Vec<(PathBuf, fs::Metadata)>)>>;

pub enum Found {
    File(PathBuf, PathBuf, fs::Metadata),
    Failed(PathBuf),
    Directory,
}

pub fn walk(
    roots: &[PathBuf],
    excludes: &[String],
    workers: usize,
    inventory: &Inventory,
    current: &(impl Fn() -> bool + Sync),
    mut consume: impl FnMut(Found),
) {
    let queue = Mutex::new((
        roots
            .iter()
            .map(|r| (r.clone(), r.clone()))
            .collect::<VecDeque<_>>(),
        0usize,
    ));
    let wake = Condvar::new();
    let (tx, rx) = mpsc::sync_channel(128);
    std::thread::scope(|scope| {
        for _ in 0..workers.clamp(1, 8) {
            let (tx, queue, wake) = (tx.clone(), &queue, &wake);
            scope.spawn(move || {
                // The coordinator always drains this channel, including after cancel.
                // Blocking send wakes immediately when space opens; polling sleeps
                // imposed a millisecond penalty on each full queue.
                let send = |value| current() && tx.send(value).is_ok();
                loop {
                    let (root, dir) = {
                        let mut state = queue.lock().unwrap();
                        loop {
                            if !current() {
                                wake.notify_all();
                                return;
                            }
                            if let Some(job) = state.0.pop_front() {
                                state.1 += 1;
                                break job;
                            }
                            if state.1 == 0 {
                                return;
                            }
                            state = wake
                                .wait_timeout(state, Duration::from_millis(20))
                                .unwrap()
                                .0;
                        }
                    };
                    if !send(Found::Directory) {
                        return;
                    }
                    let cached = inventory
                        .lock()
                        .unwrap()
                        .get(&dir)
                        .filter(|(at, _)| at.elapsed() < Duration::from_secs(30))
                        .map(|(_, items)| items.clone());
                    if let Some(items) = cached {
                        for (path, meta) in items {
                            if !current() {
                                break;
                            }
                            if meta.is_dir() {
                                queue.lock().unwrap().0.push_back((root.clone(), path));
                                wake.notify_one();
                            } else {
                                send(Found::File(root.clone(), path, meta));
                            }
                        }
                        let mut state = queue.lock().unwrap();
                        state.1 -= 1;
                        wake.notify_all();
                        continue;
                    }
                    let mut listed = Vec::new();
                    let mut complete = true;
                    match fs::read_dir(&dir) {
                        Err(_) => {
                            complete = false;
                            send(Found::Failed(dir.clone()));
                        }
                        Ok(read) => {
                            for item in read {
                                if !current() {
                                    break;
                                }
                                let item = match item {
                                    Ok(i) => i,
                                    Err(_) => {
                                        complete = false;
                                        send(Found::Failed(dir.clone()));
                                        continue;
                                    }
                                };
                                if excludes
                                    .iter()
                                    .any(|x| item.file_name().to_string_lossy() == x.as_str())
                                {
                                    continue;
                                }
                                let path = item.path();
                                let meta = match item.metadata() {
                                    Ok(m) => m,
                                    Err(_) => {
                                        complete = false;
                                        send(Found::Failed(path));
                                        continue;
                                    }
                                };
                                if crate::workspace_index::is_unsafe_to_descend(&meta) {
                                    continue;
                                }
                                if meta.is_dir() || meta.is_file() {
                                    if listed.len() < 1024 {
                                        listed.push((path.clone(), meta.clone()));
                                    } else {
                                        complete = false;
                                    }
                                }
                                if meta.is_dir() {
                                    let mut state = queue.lock().unwrap();
                                    if state.0.len() < 100_000 {
                                        state.0.push_back((root.clone(), path));
                                        wake.notify_one();
                                    } else {
                                        complete = false;
                                        drop(state);
                                        send(Found::Failed(path));
                                    }
                                } else if meta.is_file()
                                    && !send(Found::File(root.clone(), path, meta))
                                {
                                    return;
                                }
                            }
                        }
                    }
                    if complete && current() {
                        let mut cache = inventory.lock().unwrap();
                        if cache.len() < 128 {
                            cache.insert(dir, (Instant::now(), listed));
                        }
                    }
                    let mut state = queue.lock().unwrap();
                    state.1 -= 1;
                    wake.notify_all();
                }
            });
        }
        drop(tx);
        for found in rx {
            if current() {
                consume(found);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let folder = std::env::temp_dir().join(format!(
            "rfn-index-work-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&folder);
        fs::create_dir_all(&folder).unwrap();
        folder
    }

    fn files_from_a_walk(root: &PathBuf, inventory: &Inventory) -> Vec<PathBuf> {
        let mut found = Vec::new();
        walk(
            std::slice::from_ref(root),
            &[],
            1,
            inventory,
            &|| true,
            |found_item| {
                if let Found::File(_, path, _) = found_item {
                    found.push(path);
                }
            },
        );
        found.sort();
        found
    }

    /// The 30-second listing is what a *scope change* reuses — see the
    /// deliberately asymmetric inventory check in `run_worker`. The same
    /// scope drops it before a repeat scan, which this half does not model;
    /// here only the reuse itself and the effect of dropping it are pinned.
    #[test]
    fn a_listing_is_reused_until_the_inventory_is_dropped() {
        let root = scratch("reuse");
        fs::write(root.join("one.md"), "# One\n").unwrap();
        let inventory = Inventory::default();

        assert_eq!(files_from_a_walk(&root, &inventory).len(), 1);

        fs::write(root.join("two.md"), "# Two\n").unwrap();
        assert_eq!(
            files_from_a_walk(&root, &inventory).len(),
            1,
            "a fresh listing is reused instead of the directory being read again"
        );

        inventory.lock().unwrap().clear();
        assert_eq!(
            files_from_a_walk(&root, &inventory)
                .iter()
                .filter(|path| path.ends_with("two.md"))
                .count(),
            1,
            "once dropped, the directory is read again and the new file appears"
        );

        fs::remove_dir_all(root).unwrap();
    }
}
