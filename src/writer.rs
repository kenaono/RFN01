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

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError, channel};
use std::thread;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::file_io;

/// One thing to do to one file.
///
/// Owned, not borrowed: the point of handing it to another thread is that the
/// editor goes on changing the document while this is being written.
///
/// **削除も同じ待ち行列を通る**（2026-09-08）。作業コピーを消すのは要件 8.2 の
/// 仕事で、それまでUIスレッドがその場で`remove_file`していた——**退避を頼んだ
/// 直後に保存すると、消してから古いコピーが書き上がる**（`sync_all`まで含めて
/// 2.4〜5.9msかかるので、順番が入れ替わるのは珍しくない）。次の起動でそれが
/// 戻ってくる。**1本の待ち行列に並べれば、順序を約束するものが1つで済む。**
pub struct WriteJob {
    pub path: PathBuf,
    /// 書くなら中身、消すなら`None`。
    pub bytes: Option<Vec<u8>>,
}

/// What became of one job, for the log.
pub struct WriteResult {
    pub path: PathBuf,
    pub bytes: usize,
    pub ms: f64,
    /// Whether this job was a delete rather than a write.
    pub removed: bool,
    /// **Nothing was done to the file**: a newer job for the same path arrived
    /// before this one ran, and every job holds the whole document, so the
    /// older one was already out of date (追加要件 2026-09-09).
    ///
    /// Reported rather than dropped because the editor now **counts what it
    /// handed over against what came back** ([`FileWriter::settle`]): a job
    /// that answered nothing would leave that count owed for ever, and the
    /// wait before closing would sit out its whole timeout every time.
    pub superseded: bool,
    /// `None` when it was done.
    pub error: Option<String>,
}

/// A thread that writes files, and the two queues to it.
///
/// Neither queue is polled by a thread of its own: results are picked up on the
/// same timer that decides when to write, so nothing here needs to reach into
/// the UI and the whole arrangement stays on one thread at the editor's end.
pub struct FileWriter {
    /// **`RefCell`なのは終わり方のため**：送り口を落とすことがスレッドへの
    /// 「もう来ない」の合図で、[`FileWriter::finish`]はそれをしてから合流する。
    jobs: RefCell<Option<Sender<WriteJob>>>,
    worker: RefCell<Option<JoinHandle<()>>>,
    results: Receiver<WriteResult>,
    /// How many jobs have been handed over, and how many answers have been
    /// taken back. The difference is **what the disk still owes**, which is
    /// what [`FileWriter::settle`] waits out.
    handed_over: Cell<usize>,
    answered: Cell<usize>,
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
        let (jobs, worker) = match spawned {
            Ok(handle) => (Some(job_sender), Some(handle)),
            Err(_) => (None, None),
        };
        Self {
            jobs: RefCell::new(jobs),
            worker: RefCell::new(worker),
            results: result_receiver,
            handed_over: Cell::new(0),
            answered: Cell::new(0),
        }
    }

    /// Hand over a file to be written. `false` means there is no thread to
    /// write it, and the caller has to.
    pub fn write(&self, path: PathBuf, bytes: Vec<u8>) -> bool {
        self.hand_over(WriteJob {
            path,
            bytes: Some(bytes),
        })
    }

    /// Hand over a file to be taken away, **behind whatever is queued for it**.
    /// `false` means there is no thread, and the caller has to.
    pub fn remove(&self, path: PathBuf) -> bool {
        self.hand_over(WriteJob { path, bytes: None })
    }

    fn hand_over(&self, job: WriteJob) -> bool {
        let jobs = self.jobs.borrow();
        let Some(jobs) = jobs.as_ref() else {
            return false;
        };
        if jobs.send(job).is_err() {
            return false;
        }
        self.handed_over.set(self.handed_over.get() + 1);
        true
    }

    /// Everything finished since this was last asked.
    pub fn drain(&self) -> Vec<WriteResult> {
        let mut done = Vec::new();
        while let Ok(result) = self.results.try_recv() {
            done.push(result);
        }
        self.answered.set(self.answered.get() + done.len());
        done
    }

    /// Wait until everything handed over has been answered, or the time is up
    /// (追加要件 2026-09-09).
    ///
    /// **[`FileWriter::finish`]の、閉じない版である。**終了の直前に「最後の
    /// 退避は書けたのか」を訊くには、答えを窓が開いているうちに受け取らな
    /// ければならない——`finish`は送り口を落とすので、そのあと書き直すことが
    /// できない。こちらは行列をそのまま残すので、失敗を見て**もう一度頼める**。
    ///
    /// **上限を切ってあるのは、閉じられない窓を作らないため。**時間切れは
    /// 失敗ではない（まだ書いている最中かもしれない）ので、呼び出し側は
    /// 「失敗したものは無い」として先へ進み、残りは`finish`が待ち切る。
    pub fn settle(&self, longest: Duration) -> Vec<WriteResult> {
        let mut done = self.drain();
        let until = Instant::now() + longest;
        while self.handed_over.get() > self.answered.get() {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            match self.results.recv_timeout(left) {
                Ok(result) => {
                    self.answered.set(self.answered.get() + 1);
                    done.push(result);
                }
                // A disconnected channel means the thread is gone, and nothing
                // else is coming: waiting longer would only spend the timeout.
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
            }
        }
        done
    }

    /// Let everything queued finish, then stop the thread (2026-09-08).
    ///
    /// **終了の直前に呼ぶ。**要件 8.1 の退避は別スレッドで走っているので、
    /// 窓が閉じてプロセスが終われば、書き上がっていないものはそのまま消える
    /// ——「入力の2秒後に退避する」の2秒が、最後の1回だけ守られない。
    /// 送り口を落としてから合流するので、**待つのは行列に残っているぶんだけ**。
    ///
    /// 呼んだあとの[`FileWriter::write`]は`false`を返し、呼び出し側がその場で
    /// 書く——スレッドが最初から起動できなかったときと同じ道である。
    pub fn finish(&self) -> Vec<WriteResult> {
        // **落とすのが合図**：`recv`が`Err`を返して初めてスレッドは輪を出る。
        drop(self.jobs.borrow_mut().take());
        if let Some(worker) = self.worker.borrow_mut().take() {
            let _ = worker.join();
        }
        self.drain()
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
                    // **The one it replaces is still answered for**
                    // (追加要件 2026-09-09). The editor counts answers against
                    // jobs to know when the disk has caught up, and a job that
                    // quietly vanished here would leave that count owed for the
                    // rest of the run.
                    let mut replaced = Vec::new();
                    pending.retain(|held| {
                        if held.path == next.path {
                            replaced.push(held.path.clone());
                            return false;
                        }
                        true
                    });
                    for path in replaced {
                        let result = WriteResult {
                            path,
                            bytes: 0,
                            ms: 0.0,
                            removed: false,
                            superseded: true,
                            error: None,
                        };
                        if results.send(result).is_err() {
                            return;
                        }
                    }
                    pending.push(next);
                }
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
        for job in pending {
            let started = Instant::now();
            let bytes = job.bytes.as_ref().map(Vec::len).unwrap_or(0);
            let outcome = match &job.bytes {
                Some(content) => {
                    // The directory may not exist yet on the first write of a
                    // run.
                    if let Some(parent) = job.path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    file_io::write_atomically(&job.path, content).map(|_| ())
                }
                // **無いものを消すのは成功**。同じコピーを二度消す道は普通に
                // あり（保存したあとタブを閉じる）、そのたびに失敗を報告しても
                // 読む人が困るだけである。
                None => match std::fs::remove_file(&job.path) {
                    Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
                    _ => Ok(()),
                },
            };
            let result = WriteResult {
                path: job.path,
                bytes,
                ms: started.elapsed().as_secs_f64() * 1000.0,
                removed: job.bytes.is_none(),
                superseded: false,
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

    /// 2026-09-08: **消すのも同じ行列。**書き込みを頼んだ直後に削除を頼むと、
    /// 消えたままになる——UIスレッドがその場で`remove_file`していた頃は、
    /// 消してから古いコピーが書き上がり、次の起動で戻ってきた。
    #[test]
    fn a_delete_queued_behind_a_write_leaves_the_file_gone() {
        let directory = scratch_directory("delete-after-write");
        let path = directory.join("note.rfnwork");
        let writer = FileWriter::start();
        assert!(writer.write(path.clone(), b"x".to_vec()));
        assert!(writer.remove(path.clone()));
        let results = wait_for_results(&writer, 1);

        // 畳み込みで1件になることもあれば2件のこともある（どちらの順で
        // 拾われたか次第）。**約束しているのは結果のほう**で、件数ではない。
        assert!(results.iter().all(|result| result.error.is_none()));
        assert!(!path.exists(), "書いたものが残っていない");
        let _ = fs::remove_dir_all(&directory);
    }

    /// 無いものを消すのは失敗ではない。保存してからタブを閉じれば同じコピーを
    /// 二度消しにいくので、そのたびに失敗を報告しても読む人が困るだけである。
    #[test]
    fn removing_what_is_not_there_is_not_a_failure() {
        let directory = scratch_directory("delete-missing");
        let writer = FileWriter::start();
        assert!(writer.remove(directory.join("never.rfnwork")));
        let results = wait_for_results(&writer, 1);

        assert_eq!(results.len(), 1);
        assert!(results[0].error.is_none(), "{:?}", results[0].error);
        assert!(results[0].removed);
    }

    /// 追加要件 2026-09-09（残り2）: **閉じる前に、書けたかどうかを訊ける。**
    /// `finish`と違って送り口を落とさないので、失敗を見てからもう一度頼める
    /// ——それが「再試行」を成り立たせている唯一の性質である。
    #[test]
    fn waiting_for_the_queue_leaves_it_open_for_another_write() {
        let directory = scratch_directory("settle");
        let path = directory.join("note.rfnwork");
        let writer = FileWriter::start();
        assert!(writer.write(path.clone(), b"one".to_vec()));
        let settled = writer.settle(Duration::from_secs(3));

        assert_eq!(settled.len(), 1, "待った先で答えが返る");
        assert!(settled[0].error.is_none(), "{:?}", settled[0].error);
        // **行列はまだ生きている。**
        assert!(writer.write(path.clone(), b"two".to_vec()));
        let again = writer.settle(Duration::from_secs(3));
        assert_eq!(again.len(), 1);
        assert_eq!(fs::read(&path).expect("reads"), b"two");
        let _ = fs::remove_dir_all(&directory);
    }

    /// **置き換えられた仕事も答えを返す。**返さなければ、渡した数と返った数が
    /// 永久に食い違い、[`FileWriter::settle`]は毎回その上限を待ち切ることに
    /// なる——閉じるたびに3秒黙る窓ができる。
    #[test]
    fn a_job_replaced_by_a_newer_one_still_answers() {
        let directory = scratch_directory("superseded");
        let path = directory.join("note.rfnwork");
        let writer = FileWriter::start();
        for round in 0..8 {
            assert!(writer.write(path.clone(), vec![b'a' + round]));
        }
        let waited = Instant::now();
        let settled = writer.settle(Duration::from_secs(3));

        assert_eq!(settled.len(), 8, "渡した数だけ返る");
        assert!(
            waited.elapsed() < Duration::from_secs(2),
            "上限を待ち切っていない"
        );
        assert!(settled.iter().any(|result| result.superseded));
        assert!(settled.iter().all(|result| result.error.is_none()));
        // 畳まれたぶんは**何もしていない**。最後に頼んだ中身が残っている。
        assert_eq!(fs::read(&path).expect("reads"), b"h");
        let _ = fs::remove_dir_all(&directory);
    }

    /// 2026-09-08: 終わる前に行列を空にする。**乗せただけでプロセスが終われば、
    /// 乗せたことに意味が無い**（要件 8.1 の最後の1回）。
    #[test]
    fn finishing_waits_for_what_is_queued() {
        let directory = scratch_directory("finish");
        let writer = FileWriter::start();
        let paths: Vec<PathBuf> = (0..8)
            .map(|at| directory.join(format!("note-{at}.rfnwork")))
            .collect();
        for path in &paths {
            assert!(writer.write(path.clone(), b"x".to_vec()));
        }
        let left = writer.finish();

        assert!(left.iter().all(|result| result.error.is_none()));
        for path in &paths {
            assert!(path.exists(), "{} が書き上がっている", path.display());
        }
        // 終わったあとは、その場で書く側へ落ちる（スレッドが起動できなかった
        // ときと同じ道）。
        assert!(!writer.write(directory.join("after.rfnwork"), b"x".to_vec()));
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
