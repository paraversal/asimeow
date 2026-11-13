//! The parallel directory walk shared by `scan` and (later) `recommend`.
//!
//! Callers decide what to do with each directory and which subdirectories to walk next;
//! this module only distributes the work over threads and knows when it's all done.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};
use std::thread;

/// Walks from `roots` with `threads` workers. `visit` is called once per directory and
/// returns the subdirectories to walk next. Returns when every directory was visited.
pub fn walk<F>(roots: Vec<PathBuf>, threads: usize, visit: F)
where
    F: Fn(&Path) -> Vec<PathBuf> + Sync,
{
    let queue = Queue::new(roots);
    thread::scope(|scope| {
        for _ in 0..threads.max(1) {
            scope.spawn(|| {
                while let Some(dir) = queue.pop() {
                    queue.finish(visit(&dir));
                }
            });
        }
    });
}

/// Work queue for the walk. `pending` counts directories that are queued or being
/// processed, and both live under one lock, so "no work left" can't be observed while
/// another worker is between taking a directory and queueing its children.
struct Queue {
    inner: Mutex<(VecDeque<PathBuf>, usize)>,
    changed: Condvar,
}

impl Queue {
    fn new(roots: Vec<PathBuf>) -> Self {
        let pending = roots.len();
        Queue {
            inner: Mutex::new((roots.into(), pending)),
            changed: Condvar::new(),
        }
    }

    /// Next directory to process, or None once the whole tree is done.
    fn pop(&self) -> Option<PathBuf> {
        let mut inner = self.inner.lock().unwrap();
        loop {
            if let Some(path) = inner.0.pop_front() {
                return Some(path);
            }
            if inner.1 == 0 {
                return None;
            }
            inner = self.changed.wait(inner).unwrap();
        }
    }

    /// Marks one directory as done and queues its subdirectories.
    fn finish(&self, children: Vec<PathBuf>) {
        let mut inner = self.inner.lock().unwrap();
        inner.1 = inner.1 + children.len() - 1;
        inner.0.extend(children);
        self.changed.notify_all();
    }
}
