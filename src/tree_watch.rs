//! Read visible tree rows away from the event loop.
use crate::file_tree;
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub roots: Vec<PathBuf>,
    pub multi: bool,
    pub expanded: BTreeSet<PathBuf>,
    pub displayed_paths: Vec<PathBuf>,
}

type ReadFolder = dyn Fn(&Path) -> Vec<file_tree::Node> + Send + Sync;

struct Job {
    request: Request,
    cancelled: Arc<AtomicBool>,
    reply: mpsc::Receiver<Vec<file_tree::Row>>,
}

#[derive(Default)]
pub struct Watcher {
    jobs: Vec<Job>,
    pending: Option<Request>,
}

impl Watcher {
    pub fn request(&mut self, request: Request) {
        if self.pending.as_ref() == Some(&request)
            || self
                .jobs
                .iter()
                .any(|job| job.request == request && !job.cancelled.load(Ordering::Relaxed))
        {
            return;
        }
        self.cancel();
        self.pending = Some(request);
    }

    pub fn cancel(&mut self) {
        self.pending = None;
        for job in &self.jobs {
            job.cancelled.store(true, Ordering::Relaxed);
        }
    }

    pub fn working(&self) -> bool {
        self.pending.is_some() || !self.jobs.is_empty()
    }

    pub fn collect(&mut self, current: &Request) -> Option<Vec<file_tree::Row>> {
        self.collect_with(current, Arc::new(file_tree::read_folder))
    }

    fn collect_with(
        &mut self,
        current: &Request,
        read: Arc<ReadFolder>,
    ) -> Option<Vec<file_tree::Row>> {
        let mut latest = None;
        self.jobs.retain(|job| match job.reply.try_recv() {
            Ok(rows) => {
                if !job.cancelled.load(Ordering::Relaxed) && job.request == *current {
                    latest = Some(rows);
                }
                false
            }
            Err(mpsc::TryRecvError::Empty) => true,
            Err(mpsc::TryRecvError::Disconnected) => false,
        });
        if self
            .pending
            .as_ref()
            .is_some_and(|request| request != current)
        {
            self.pending = None;
        }
        // A read blocked in the OS keeps its slot. Leave another slot for
        // a new root, and retain only the newest pending request.
        if self.jobs.len() < 2
            && let Some(request) = self.pending.take()
        {
            let (send, reply) = mpsc::channel();
            let cancelled = Arc::new(AtomicBool::new(false));
            let stop = cancelled.clone();
            let task = request.clone();
            if std::thread::Builder::new()
                .name("explorer-refresh".into())
                .spawn(move || {
                    let read = |path: &Path| {
                        if stop.load(Ordering::Relaxed) {
                            Vec::new()
                        } else {
                            read(path)
                        }
                    };
                    let rows = if task.multi {
                        file_tree::multi_rows(&task.roots, &task.expanded, &read)
                    } else {
                        task.roots.first().map_or_else(Vec::new, |root| {
                            file_tree::rows(root, &task.expanded, &read)
                        })
                    };
                    let _ = send.send(rows);
                })
                .is_ok()
            {
                self.jobs.push(Job {
                    request,
                    cancelled,
                    reply,
                });
            }
        }
        latest
    }

    #[cfg(test)]
    pub fn poll(&mut self, current: &Request) -> Option<Vec<file_tree::Row>> {
        self.request(current.clone());
        self.collect(current)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn a_blocked_root_does_not_delay_another_root_or_replace_its_rows() {
        let mut watcher = Watcher::default();
        let make_request = |root: &str| Request {
            roots: vec![root.into()],
            multi: false,
            expanded: BTreeSet::new(),
            displayed_paths: Vec::new(),
        };
        let (entered, wait) = mpsc::channel();
        let (unblock, gate) = mpsc::channel();
        let gate = std::sync::Mutex::new(gate);
        let read: Arc<ReadFolder> = Arc::new(move |path| {
            if path == Path::new("blocked") {
                entered.send(()).unwrap();
                let _ = gate.lock().unwrap().recv_timeout(Duration::from_secs(5));
            }
            Vec::new()
        });
        let old = make_request("blocked");
        watcher.request(old.clone());
        assert!(watcher.collect_with(&old, read.clone()).is_none());
        wait.recv_timeout(Duration::from_secs(2)).unwrap();
        let current = make_request("current");
        watcher.request(current.clone());
        let until = Instant::now() + Duration::from_secs(2);
        while watcher.collect_with(&current, read.clone()).is_none() {
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(5));
        }
        unblock.send(()).unwrap();
        while watcher.working() {
            assert!(watcher.collect_with(&current, read.clone()).is_none());
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
