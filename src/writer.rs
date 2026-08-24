//! Writing files without stopping the editor.
//!
//! 要件 2 asks that slow work not get in the way of the writer. The work copy
//! is the one write that happens **while somebody is typing** — two seconds
//! after they pause, and every five seconds if they never do (要件 8.1) — and
//! it ends in `sync_all`, which waits for the device to say it is done.
//!
//! Measured at 2.4–5.9ms for a few hundred bytes. **Almost none of that is the
//! bytes**: it is the flush and the rename, so it does not shrink with a
//! smaller document and it will not stay at 5ms with a larger one.
//!
//! This thread touches no DirectWrite and no COM, so the question 技術検証 7.3
//! leaves open — what happens when several threads lay text out at once — does
//! not arise here. It moves file writing off the UI thread and nothing else.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::thread;
use std::time::Instant;

use crate::file_io;

/// One file to write, whole.
///
/// Owned, not borrowed: the point of handing it to another thread is that the
/// editor goes on changing the document while this is being written.
pub struct WriteJob {
    pub path: PathBuf,
    pub bytes: Vec<u8>,
}

/// What became of one job, for the log.
pub struct WriteResult {
    pub path: PathBuf,
    pub bytes: usize,
    pub ms: f64,
    /// `None` when it was written.
    pub error: Option<String>,
}

/// A thread that writes files, and the two queues to it.
///
/// Neither queue is polled by a thread of its own: results are picked up on the
/// same timer that decides when to write, so nothing here needs to reach into
/// the UI and the whole arrangement stays on one thread at the editor's end.
pub struct FileWriter {
    jobs: Option<Sender<WriteJob>>,
    results: Receiver<WriteResult>,
}

impl FileWriter {
    /// Start the writer, or report that it could not be started.
    ///
    /// A failure here is not fatal: the caller writes on its own thread
    /// instead, which is what the editor did before this existed.
    pub fn start() -> Self {
        let (job_sender, job_receiver) = channel::<WriteJob>();
        let (result_sender, result_receiver) = channel::<WriteResult>();
        let spawned = thread::Builder::new()
            .name("rfnedit-writer".to_owned())
            .spawn(move || run(&job_receiver, &result_sender));
        Self {
            jobs: spawned.is_ok().then_some(job_sender),
            results: result_receiver,
        }
    }

    /// Hand over a file to be written. `false` means there is no thread to
    /// write it, and the caller has to.
    pub fn write(&self, path: PathBuf, bytes: Vec<u8>) -> bool {
        let Some(jobs) = &self.jobs else {
            return false;
        };
        jobs.send(WriteJob { path, bytes }).is_ok()
    }

    /// Everything finished since this was last asked.
    pub fn drain(&self) -> Vec<WriteResult> {
        let mut done = Vec::new();
        while let Ok(result) = self.results.try_recv() {
            done.push(result);
        }
        done
    }
}

/// The writer thread.
fn run(jobs: &Receiver<WriteJob>, results: &Sender<WriteResult>) {
    while let Ok(job) = jobs.recv() {
        // Only the newest write for each file matters: every job holds the
        // whole document, so an older one for the same path is already out of
        // date. Draining first means a slow disk costs one write rather than a
        // backlog that grows for as long as the typing lasts.
        let mut pending = vec![job];
        loop {
            match jobs.try_recv() {
                Ok(next) => {
                    pending.retain(|held| held.path != next.path);
                    pending.push(next);
                }
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
        for job in pending {
            let started = Instant::now();
            // The directory may not exist yet on the first write of a run.
            if let Some(parent) = job.path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let outcome = file_io::write_atomically(&job.path, &job.bytes);
            let result = WriteResult {
                path: job.path,
                bytes: job.bytes.len(),
                ms: started.elapsed().as_secs_f64() * 1000.0,
                error: outcome.err().map(|error| error.to_string()),
            };
            // A closed receiver means the editor is gone. Nothing to report to.
            if results.send(result).is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::thread::sleep;
    use std::time::Duration;

    use super::*;

    fn scratch_directory(name: &str) -> PathBuf {
        let temporary = std::env::temp_dir();
        let directory = temporary.join(format!("rfnedit-writer-{name}"));
        let _ = fs::remove_dir_all(&directory);
        directory
    }

    /// Waits for one result, or gives up. The thread is doing real file work,
    /// so this cannot be instantaneous, but it also must not hang a test run.
    fn wait_for_results(writer: &FileWriter, count: usize) -> Vec<WriteResult> {
        let mut collected = Vec::new();
        for _ in 0..200 {
            collected.extend(writer.drain());
            if collected.len() >= count {
                return collected;
            }
            sleep(Duration::from_millis(10));
        }
        collected
    }

    #[test]
    fn writes_a_file_on_another_thread() {
        let directory = scratch_directory("writes");
        let path = directory.join("note.rfnwork");
        let writer = FileWriter::start();
        assert!(writer.write(path.clone(), b"\xe6\x9c\xac\xe6\x96\x87".to_vec()));
        let results = wait_for_results(&writer, 1);
        assert_eq!(results.len(), 1, "the write reported back");
        assert!(results[0].error.is_none(), "{:?}", results[0].error);
        assert_eq!(fs::read(&path).expect("reads"), "本文".as_bytes());
        let _ = fs::remove_dir_all(&directory);
    }

    /// The directory is made on the way, so the first work copy of a run does
    /// not have to be preceded by anything.
    #[test]
    fn makes_the_directory_it_writes_into() {
        let directory = scratch_directory("makes-dir");
        let path = directory.join("deeper").join("note.rfnwork");
        let writer = FileWriter::start();
        writer.write(path.clone(), b"x".to_vec());
        wait_for_results(&writer, 1);
        assert!(path.exists());
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn reports_a_write_that_failed() {
        let directory = scratch_directory("fails");
        fs::create_dir_all(&directory).expect("creates");
        // A directory cannot be replaced by a file, so the rename fails.
        let path = directory.join("occupied");
        fs::create_dir_all(&path).expect("creates the obstacle");
        let writer = FileWriter::start();
        writer.write(path, b"x".to_vec());
        let results = wait_for_results(&writer, 1);
        assert_eq!(results.len(), 1);
        assert!(results[0].error.is_some(), "the failure is reported");
        let _ = fs::remove_dir_all(&directory);
    }
}
