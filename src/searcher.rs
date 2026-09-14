//! Searching the work folder without stopping the editor (要件 2, 7.7).
//!
//! **This is the one place the editor reads many files at once.** A search of
//! the document in front costs nothing — the text is already in memory — but
//! 要件 7.7 also searches every file under the work folder, and how long that
//! takes is decided by the folder somebody opened rather than by anything the
//! editor did. 技術検証 7.5 measured 18–32ms over a handful of files and said
//! the number that matters is not there: what a folder of several hundred
//! costs, cold.
//!
//! Until now that was answered by refusing to grow — so many files, so many
//! lines from one file, so many lines in all. The bounds stay, because a list
//! nobody can read is no better than a wait (要件 7.7), but they are no longer
//! the only thing between a large folder and a window that has stopped
//! answering.
//!
//! Like [`crate::writer`], this touches no DirectWrite and no COM, so the
//! question 技術検証 7.3 leaves open — what happens when several threads lay
//! text out at once — does not arise. **That is why this is first**: it is the
//! slow work that can be moved without answering it (技術検証 7.5).
//!
//! It knows nothing about Slint either. The editor hands over a way to wake
//! itself and gets told when there is something to collect; what that costs the
//! window is the window's business.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::thread;
use std::time::Instant;

use crate::file_io;
use crate::file_tree;
use crate::find::{self, Hit};

/// One folder-wide search, with everything it is allowed to spend.
///
/// **The bounds travel with the job** rather than being read from constants
/// here: they are 要件 7.7's answer about what a person can read, which is the
/// editor's business and not this thread's.
pub struct SearchJob {
    pub exclusions: String,
    pub root: PathBuf,
    pub needle: String,
    /// Which search this is, counting up. **The editor's staleness check**: a
    /// person types on while one search runs, so an outcome has to be able to
    /// say which question it answers.
    pub generation: u64,
    /// The most files to read.
    pub files: usize,
    /// The most lines to report from any one file, and in all.
    pub hits_per_file: usize,
    pub hits_in_all: usize,
    /// The document limit (要件 2.3). A file past it is skipped, the same as
    /// one that cannot be opened.
    pub characters: usize,
}

/// One file that matched, and where in it.
pub struct FileHits {
    pub path: PathBuf,
    pub hits: Vec<Hit>,
}

/// What one search found.
///
/// **The matches, not the rows.** What a row says and how far in it sits is the
/// left pane's business (要件 6.2); this thread has no opinion about it, and
/// carrying rows across would put the pane's shape in a file that cannot see
/// the pane.
pub struct SearchOutcome {
    pub generation: u64,
    pub needle: String,
    pub files: Vec<FileHits>,
    /// Lines matched in all, which is what the status bar counts.
    pub total: usize,
    pub ms: f64,
}

/// Asked between files: whether this search is worth going on with.
///
/// **Between files rather than never**, because a folder-wide search is the one
/// piece of work here that can outlive the question it answers: somebody types
/// another letter and the running search is already about the wrong word.
/// Answered by the thread from its own queue, and by nothing at all when the
/// editor is doing the search itself.
pub trait Superseded {
    fn superseded(&mut self) -> bool;
}

/// Nothing supersedes a search the editor is waiting on itself.
pub struct NeverSuperseded;

impl Superseded for NeverSuperseded {
    fn superseded(&mut self) -> bool {
        false
    }
}

/// Read every file under `job.root` and report where `job.needle` is
/// (要件 7.7).
///
/// `None` when `stop` said a newer search had arrived. **Nothing is reported
/// for an abandoned search** — the editor would only throw it away, and a
/// half-done list is worse than none because it looks like an answer.
pub fn search(job: &SearchJob, stop: &mut dyn Superseded) -> Option<SearchOutcome> {
    let started = Instant::now();
    let patterns = exclusion_patterns(&job.exclusions);
    let paths = file_tree::files_under(
        &job.root,
        &|folder| {
            file_tree::read_folder(folder)
                .into_iter()
                .filter(|node| {
                    !patterns.iter().any(|(pattern, directory)| {
                        (!directory || node.folder) && pattern.is_match(&node.name)
                    })
                })
                .collect()
        },
        job.files,
    );
    let mut files: Vec<FileHits> = Vec::new();
    let mut total = 0usize;
    for path in paths {
        if total >= job.hits_in_all {
            break;
        }
        if stop.superseded() {
            return None;
        }
        let Ok(loaded) = file_io::read(&path, job.characters) else {
            continue;
        };
        let hits = find::hits_in(&loaded.text, &job.needle, job.hits_per_file);
        if hits.is_empty() {
            continue;
        }
        total += hits.len();
        files.push(FileHits { path, hits });
    }
    Some(SearchOutcome {
        generation: job.generation,
        needle: job.needle.clone(),
        files,
        total,
        ms: started.elapsed().as_secs_f64() * 1000.0,
    })
}

fn exclusion_patterns(value: &str) -> Vec<(regex::Regex, bool)> {
    value
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|s| {
            let directory = s.ends_with('/') || s.ends_with('\\');
            let name = s.trim_end_matches(['/', '\\']);
            let pattern = regex::escape(name).replace("\\*", ".*").replace("\\?", ".");
            regex::Regex::new(&format!("(?i)^{pattern}$"))
                .ok()
                .map(|r| (r, directory))
        })
        .collect()
}

/// A thread that searches work folders, and the two queues to it.
pub struct Searcher {
    jobs: Option<Sender<SearchJob>>,
    outcomes: Receiver<SearchOutcome>,
}

impl Searcher {
    /// Start the searcher, or report that it could not be started.
    ///
    /// `wake` is called once for every outcome put on the queue, from the
    /// searching thread. A failure here is not fatal: the caller searches on
    /// its own thread instead, which is what the editor did before this
    /// existed.
    pub fn start(wake: impl Fn() + Send + 'static) -> Self {
        let (job_sender, job_receiver) = channel::<SearchJob>();
        let (outcome_sender, outcome_receiver) = channel::<SearchOutcome>();
        let spawned = thread::Builder::new()
            .name("rfnedit-searcher".to_owned())
            .spawn(move || run(&job_receiver, &outcome_sender, &wake));
        Self {
            jobs: spawned.is_ok().then_some(job_sender),
            outcomes: outcome_receiver,
        }
    }

    /// Hand over a search. The job comes back when there is no thread to do it
    /// and the caller has to.
    pub fn dispatch(&self, job: SearchJob) -> Result<(), SearchJob> {
        let Some(jobs) = &self.jobs else {
            return Err(job);
        };
        jobs.send(job).map_err(|failed| failed.0)
    }

    /// Everything finished since this was last asked.
    pub fn drain(&self) -> Vec<SearchOutcome> {
        let mut done = Vec::new();
        while let Ok(outcome) = self.outcomes.try_recv() {
            done.push(outcome);
        }
        done
    }
}

/// The searching thread's own view of its queue.
///
/// **A job is peeked, not taken.** Deciding to abandon the running search and
/// deciding what to do next are the same fact arriving once; taking it here
/// would leave the loop below with nothing to run.
struct Queue<'a> {
    jobs: &'a Receiver<SearchJob>,
    next: Option<SearchJob>,
    closed: bool,
}

impl Superseded for Queue<'_> {
    fn superseded(&mut self) -> bool {
        if self.next.is_none() && !self.closed {
            match self.jobs.try_recv() {
                Ok(job) => self.next = Some(job),
                Err(TryRecvError::Empty) => {}
                // The editor is gone. Abandoning is right for the same reason
                // a newer search is: nobody is waiting for this answer.
                Err(TryRecvError::Disconnected) => self.closed = true,
            }
        }
        self.next.is_some() || self.closed
    }
}

/// The searching thread.
fn run(jobs: &Receiver<SearchJob>, outcomes: &Sender<SearchOutcome>, wake: &dyn Fn()) {
    let mut queue = Queue {
        jobs,
        next: None,
        closed: false,
    };
    loop {
        // Whatever was peeked while the last search ran, else wait for one.
        let job = match queue.next.take() {
            Some(job) => job,
            None => match jobs.recv() {
                Ok(job) => job,
                Err(_) => return,
            },
        };
        // Only the newest search matters, so anything already queued behind it
        // wins outright — the older one is about a word the writer has already
        // finished typing.
        while let Ok(newer) = jobs.try_recv() {
            queue.next = Some(newer);
        }
        let job = queue.next.take().unwrap_or(job);
        let Some(outcome) = search(&job, &mut queue) else {
            continue;
        };
        // A closed receiver means the editor is gone. Nothing to report to.
        if outcomes.send(outcome).is_err() {
            return;
        }
        wake();
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread::sleep;
    use std::time::Duration;

    use super::*;

    fn scratch_folder(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!("rfnedit-searcher-{name}"));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).expect("creates the folder");
        directory
    }

    fn write(directory: &Path, name: &str, text: &str) {
        fs::write(directory.join(name), text).expect("writes the file");
    }

    fn job(root: &Path, needle: &str) -> SearchJob {
        SearchJob {
            exclusions: String::new(),
            root: root.to_path_buf(),
            needle: needle.to_owned(),
            generation: 1,
            files: 5_000,
            hits_per_file: 50,
            hits_in_all: 500,
            characters: 1_000_000,
        }
    }

    /// Waits for one outcome, or gives up. The thread is doing real file work,
    /// so this cannot be instantaneous, but it also must not hang a test run.
    fn wait_for_outcomes(searcher: &Searcher, count: usize) -> Vec<SearchOutcome> {
        let mut collected = Vec::new();
        for _ in 0..200 {
            collected.extend(searcher.drain());
            if collected.len() >= count {
                return collected;
            }
            sleep(Duration::from_millis(10));
        }
        collected
    }

    #[test]
    fn exclusions_prune_folders_before_the_file_limit() {
        let folder = scratch_folder("exclusions");
        fs::create_dir_all(folder.join("backup")).unwrap();
        write(&folder.join("backup"), "hidden.md", "needle");
        write(&folder, "A.md", "needle");
        write(&folder, "z.md", "needle");
        let mut request = job(&folder, "needle");
        request.files = 1;
        request.exclusions = "backup/;a.*".into();
        let found = search(&request, &mut NeverSuperseded).unwrap();
        assert_eq!(found.files.len(), 1);
        assert_eq!(found.files[0].path, folder.join("z.md"));
        request.files = 10;
        request.exclusions.clear();
        assert_eq!(
            search(&request, &mut NeverSuperseded).unwrap().files.len(),
            3
        );
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn searches_a_folder_on_another_thread() {
        let folder = scratch_folder("finds");
        write(&folder, "one.md", "本文\n検索テスト用キーワード\n");
        write(&folder, "two.md", "何もない\n");
        let woken = Arc::new(AtomicUsize::new(0));
        let counter = woken.clone();

        let searcher = Searcher::start(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        let taken = searcher.dispatch(job(&folder, "検索テスト用キーワード"));
        assert!(taken.is_ok(), "the thread took the job");
        let outcomes = wait_for_outcomes(&searcher, 1);

        assert_eq!(outcomes.len(), 1, "the search reported back");
        assert_eq!(outcomes[0].total, 1);
        assert_eq!(outcomes[0].files.len(), 1, "only the file that matched");
        assert_eq!(outcomes[0].files[0].hits[0].line, 2);
        assert_eq!(outcomes[0].generation, 1);
        // The editor is told there is something to collect, or it never looks.
        assert!(woken.load(Ordering::SeqCst) >= 1, "the wake-up was called");
        let _ = fs::remove_dir_all(&folder);
    }

    /// **A half-done list is worse than none**, because it looks like an
    /// answer. A search that is given up on says nothing at all.
    #[test]
    fn an_abandoned_search_reports_nothing() {
        let folder = scratch_folder("abandons");
        write(&folder, "one.md", "見つかる語\n");

        struct Always;
        impl Superseded for Always {
            fn superseded(&mut self) -> bool {
                true
            }
        }

        assert!(search(&job(&folder, "見つかる語"), &mut Always).is_none());
        // And the same search, with nothing overtaking it, does find it.
        let found = search(&job(&folder, "見つかる語"), &mut NeverSuperseded);
        assert_eq!(found.expect("an outcome").total, 1);
        let _ = fs::remove_dir_all(&folder);
    }

    /// 要件 7.7: the bounds are about what a person can read. **In all, not per
    /// file** — one file matching on every line must not be able to fill the
    /// pane, and neither must a thousand files matching once.
    #[test]
    fn stops_at_the_lines_it_is_allowed_to_report() {
        let folder = scratch_folder("bounds");
        let many = "語\n".repeat(40);
        for index in 0..6 {
            write(&folder, &format!("{index}.md"), &many);
        }

        let mut asked = job(&folder, "語");
        asked.hits_per_file = 10;
        asked.hits_in_all = 25;
        let found = search(&asked, &mut NeverSuperseded).expect("an outcome");

        // Three files of ten, and then the fourth is never opened: the bound is
        // read before a file rather than inside it, so the last file to be let
        // in is reported whole.
        assert_eq!(found.total, 30);
        assert_eq!(found.files.len(), 3);
        assert!(found.files.iter().all(|file| file.hits.len() == 10));
        let _ = fs::remove_dir_all(&folder);
    }

    /// Only the newest question is answered. **Peeked rather than taken**: the
    /// job that ends one search is the job that begins the next.
    ///
    /// Which of the two the thread manages to abandon is a race and is not
    /// asserted — over a folder this small the older one may well finish first.
    /// What must hold either way is that the newer one is answered.
    #[test]
    fn the_newest_search_is_the_one_that_runs() {
        let folder = scratch_folder("newest");
        write(&folder, "one.md", "古い語\n新しい語\n");

        let searcher = Searcher::start(|| {});
        let mut older = job(&folder, "古い語");
        older.generation = 7;
        let mut newer = job(&folder, "新しい語");
        newer.generation = 8;
        assert!(searcher.dispatch(older).is_ok(), "the older job was taken");
        assert!(searcher.dispatch(newer).is_ok(), "the newer job was taken");

        let mut outcomes = Vec::new();
        for _ in 0..200 {
            outcomes.extend(searcher.drain());
            if outcomes.iter().any(|found| found.generation == 8) {
                break;
            }
            sleep(Duration::from_millis(10));
        }

        let newest = outcomes
            .iter()
            .find(|outcome| outcome.generation == 8)
            .expect("the newest search was answered");
        assert_eq!(newest.total, 1);
        let _ = fs::remove_dir_all(&folder);
    }
}
