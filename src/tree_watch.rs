//! Read the visible tree off the UI thread, with at most one request in flight.
use crate::file_tree;
use std::{collections::BTreeSet, path::PathBuf, sync::mpsc};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub root: PathBuf,
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
                    let rows =
                        file_tree::rows(&request.root, &request.expanded, &file_tree::read_folder);
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
