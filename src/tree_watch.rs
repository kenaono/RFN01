//! Read the visible tree off the UI thread, with at most one request in flight.
use crate::file_tree;
use std::{collections::BTreeSet, path::PathBuf, sync::mpsc};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    /// The active roots to walk — one entry for the classic single work
    /// folder, every registered folder of an active Workspace otherwise.
    pub roots: Vec<PathBuf>,
    /// Whether `roots` are drawn as their own labelled rows
    /// ([`file_tree::multi_rows`]) or as the classic single folder's own
    /// children ([`file_tree::rows`]) — Workspace設計.md phase 3's
    /// multi-root tree only exists when a Workspace is active, never for a
    /// plain "Open Folder" even if it happened to hold more than one root
    /// worth watching.
    pub multi: bool,
    pub expanded: BTreeSet<PathBuf>,
    pub displayed_paths: Vec<PathBuf>,
}

pub struct Watcher {
    send: mpsc::Sender<Request>,
    receive: mpsc::Receiver<(Request, Vec<file_tree::Row>)>,
    busy: bool,
}

impl Watcher {
    pub fn new() -> std::io::Result<Self> {
        let (send, requests) = mpsc::channel::<Request>();
        let (results, receive) = mpsc::channel();
        std::thread::Builder::new()
            .name("explorer-refresh".into())
            .spawn(move || {
                while let Ok(request) = requests.recv() {
                    let rows = if request.multi {
                        file_tree::multi_rows(
                            &request.roots,
                            &request.expanded,
                            &file_tree::read_folder,
                        )
                    } else {
                        request
                            .roots
                            .first()
                            .map(|root| {
                                file_tree::rows(root, &request.expanded, &file_tree::read_folder)
                            })
                            .unwrap_or_default()
                    };
                    if results.send((request, rows)).is_err() {
                        break;
                    }
                }
            })?;
        Ok(Self {
            send,
            receive,
            busy: false,
        })
    }

    pub fn poll(&mut self, current: &Request) -> Option<Vec<file_tree::Row>> {
        let result = match self.receive.try_recv() {
            Ok((request, rows)) => {
                self.busy = false;
                (request == *current).then_some(rows)
            }
            Err(_) => None,
        };
        if !self.busy && self.send.send(current.clone()).is_ok() {
            self.busy = true;
        }
        result
    }
}
