//! Independent latest-request worker for open notes and their direct targets.
use crate::{
    document,
    workspace_index::{self, Entry},
    workspace_links,
};
use std::{
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

pub struct Request {
    pub roots: Vec<PathBuf>,
    pub entries: Arc<Vec<Entry>>,
    pub documents: Vec<workspace_links::ValidityDocument>,
}
pub struct ResultSet {
    pub generation: u64,
    pub entries: Vec<Entry>,
    pub live_paths: Vec<PathBuf>,
    pub elapsed_ms: f64,
    pub wait_ms: f64,
}
type Pending = Option<(u64, Instant, Request)>;
pub struct Priority {
    pending: Arc<(Mutex<Pending>, Condvar)>,
    result: Arc<Mutex<Option<ResultSet>>>,
    current: Arc<AtomicU64>,
}
impl Priority {
    pub fn new() -> Self {
        let pending = Arc::new((Mutex::new(None::<(u64, Instant, Request)>), Condvar::new()));
        let result = Arc::new(Mutex::new(None));
        let current = Arc::new(AtomicU64::new(0));
        let (p, r, c) = (pending.clone(), result.clone(), current.clone());
        std::thread::Builder::new()
            .name("workspace-priority".into())
            .spawn(move || {
                loop {
                    let mut slot = p.0.lock().unwrap();
                    while slot.is_none() && c.load(Ordering::Relaxed) != u64::MAX {
                        slot = p.1.wait(slot).unwrap();
                    }
                    if c.load(Ordering::Relaxed) == u64::MAX {
                        break;
                    }
                    let (generation, queued, request) = slot.take().unwrap();
                    drop(slot);
                    let started = Instant::now();
                    let valid = || c.load(Ordering::Relaxed) == generation;
                    let (entries, live_paths) = build(request, &valid);
                    if valid() {
                        *r.lock().unwrap() = Some(ResultSet {
                            generation,
                            entries,
                            live_paths,
                            elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
                            wait_ms: started.duration_since(queued).as_secs_f64() * 1000.0,
                        });
                    }
                }
            })
            .expect("workspace priority worker");
        Self {
            pending,
            result,
            current,
        }
    }
    pub fn submit(&self, request: Request) -> u64 {
        let generation = self.current.fetch_add(1, Ordering::SeqCst) + 1;
        *self.pending.0.lock().unwrap() = Some((generation, Instant::now(), request));
        self.pending.1.notify_one();
        generation
    }
    pub fn poll(&self) -> Option<ResultSet> {
        self.result
            .lock()
            .unwrap()
            .take()
            .filter(|r| r.generation == self.current.load(Ordering::Relaxed))
    }
}
impl Drop for Priority {
    fn drop(&mut self) {
        let _guard = self.pending.0.lock().unwrap();
        self.current.store(u64::MAX, Ordering::Relaxed);
        self.pending.1.notify_one();
    }
}

fn build(request: Request, current: &impl Fn() -> bool) -> (Vec<Entry>, Vec<PathBuf>) {
    let mut entries = std::collections::HashMap::new();
    let roots: Vec<_> = request
        .roots
        .iter()
        .filter_map(|p| p.canonicalize().ok())
        .collect();
    for doc in &request.documents {
        if !current() {
            return (Vec::new(), Vec::new());
        }
        let Some(path) = &doc.path else {
            continue;
        };
        if !workspace_index::is_markdown(path) {
            continue;
        }
        if let Some(mut entry) = workspace_index::read_entry(path, &roots) {
            entry.headings = document::outline(&doc.text);
            entry.headings_complete = true;
            entries.insert(entry.canonical.clone(), entry);
        }
    }
    let live_paths = entries.keys().cloned().collect();
    let mut view = request.entries.as_ref().clone();
    view.retain(|e| !entries.contains_key(&e.canonical));
    view.extend(entries.values().cloned());
    let mut visited = std::collections::HashSet::new();
    for doc in &request.documents {
        if !current() {
            break;
        }
        let Some(source) = &doc.path else {
            continue;
        };
        if !workspace_index::is_markdown(source) {
            continue;
        }
        let mut targets: Vec<(String, bool, bool)> = document::link_target_ranges(&doc.text)
            .into_iter()
            .map(|(range, wiki)| (doc.text[range].to_owned(), wiki, false))
            .collect();
        let styles = document::line_styles_reading(&doc.text, document::Reading::all());
        for (line, style) in doc.text.lines().zip(styles) {
            if !style.kind.is_code() {
                if let Some(image) = document::image_of_line(line) {
                    targets.push((image.target.to_owned(), true, true));
                }
            }
        }
        for (target, wiki, image) in targets {
            if !current() || visited.len() >= workspace_index::MAX_ENTRIES {
                break;
            }
            let file = crate::link_completion::split_target_heading(
                target.trim().trim_matches(['<', '>']),
            )
            .0;
            if file.is_empty() {
                continue;
            }
            let decoded = crate::link_completion::percent_decode(file);
            // Direct disk targets need not have been enumerated yet.
            let direct = source.parent().map(|p| p.join(&decoded));
            let mut found = None;
            if let Some(path) = direct {
                found = workspace_index::read_entry(&path, &roots);
                if found.is_none() && !image && path.extension().is_none() {
                    found = workspace_index::read_entry(
                        &PathBuf::from(format!("{}.md", path.display())),
                        &roots,
                    );
                }
            }
            if found.is_none() {
                if let Ok(path) = workspace_links::resolve_indexed_file(
                    &decoded,
                    wiki,
                    Some(source),
                    &view,
                    image,
                ) {
                    found = workspace_index::read_entry(&path, &roots);
                }
            }
            if let Some(entry) = found {
                if visited.insert(entry.canonical.clone()) {
                    entries.entry(entry.canonical.clone()).or_insert(entry);
                }
            }
        }
    }
    (entries.into_values().collect(), live_paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn priority_progresses_while_bulk_results_are_not_consumed() {
        let root = std::env::temp_dir().join(format!(
            "rfn-priority-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        for i in 0..1200 {
            std::fs::write(root.join(format!("note{i}.md")), "# Disk\n").unwrap();
        }
        std::fs::write(root.join("photo.png"), "metadata").unwrap();
        let mut bulk = workspace_index::Indexer::new();
        bulk.restart_memory(vec![root.clone()], workspace_index::ScanOptions::default());
        let priority = Priority::new();
        let request = || Request {
            roots: vec![root.clone()],
            entries: Arc::new(Vec::new()),
            documents: vec![workspace_links::ValidityDocument {
                id: 1,
                path: Some(root.join("note0.md")),
                text: "# Unsaved\n[[./note1]]\n![[photo.png]]\n".into(),
            }],
        };
        priority.submit(request());
        let newest = priority.submit(request());
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(result) = priority.poll() {
                assert_eq!(result.generation, newest);
                let live = result
                    .entries
                    .iter()
                    .find(|e| e.canonical.ends_with("note0.md"))
                    .unwrap();
                assert_eq!(live.headings[0].text, "Unsaved");
                assert!(
                    result
                        .entries
                        .iter()
                        .any(|e| e.canonical.ends_with("note1.md") && e.headings[0].text == "Disk")
                );
                assert!(
                    result
                        .entries
                        .iter()
                        .any(|e| e.canonical.ends_with("photo.png") && e.headings.is_empty())
                );
                break;
            }
            assert!(Instant::now() < deadline, "priority blocked by full scan");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        bulk.cancel();
    }
}
