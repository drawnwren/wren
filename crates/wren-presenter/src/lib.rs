#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::fmt::Display;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};

use thiserror::Error;
use wren_term::TerminalBackend;
use wren_view::{DesiredGrid, TerminalPatch, diff_into};

pub type PresentationObserver = Arc<dyn Fn(u64) + Send + Sync + 'static>;

#[derive(Debug, Error)]
pub enum PresenterError {
    #[error("spawn presenter thread: {0}")]
    Spawn(#[source] io::Error),
    #[error("presenter state lock is poisoned")]
    Poisoned,
    #[error("presenter has stopped")]
    Stopped,
    #[error("presenter thread panicked")]
    Panicked,
    #[error("terminal presenter failed: {0}")]
    Backend(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresenterStats {
    pub published_frames: u64,
    pub dropped_frames: u64,
    pub presented_frames: u64,
    pub last_presented_epoch: u64,
}

#[derive(Debug, Default)]
struct QueueState {
    slot: Option<Arc<DesiredGrid>>,
    stopped: bool,
    published: u64,
    dropped: u64,
}

#[derive(Debug, Default)]
struct LatestFrameQueue {
    state: Mutex<QueueState>,
    changed: Condvar,
}

impl LatestFrameQueue {
    fn publish(&self, frame: Arc<DesiredGrid>) -> Result<(), PresenterError> {
        let mut state = self.state.lock().map_err(|_| PresenterError::Poisoned)?;
        if state.stopped {
            return Err(PresenterError::Stopped);
        }
        state.published = state.published.saturating_add(1);
        if state.slot.replace(frame).is_some() {
            state.dropped = state.dropped.saturating_add(1);
        }
        self.changed.notify_one();
        Ok(())
    }

    fn take(&self) -> Result<Option<Arc<DesiredGrid>>, PresenterError> {
        let mut state = self.state.lock().map_err(|_| PresenterError::Poisoned)?;
        while state.slot.is_none() && !state.stopped {
            state = self
                .changed
                .wait(state)
                .map_err(|_| PresenterError::Poisoned)?;
        }
        Ok(state.slot.take())
    }

    fn stop(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.stopped = true;
            self.changed.notify_all();
        }
    }

    fn publication_stats(&self) -> Result<(u64, u64), PresenterError> {
        let state = self.state.lock().map_err(|_| PresenterError::Poisoned)?;
        Ok((state.published, state.dropped))
    }
}

pub struct Presenter<B>
where
    B: TerminalBackend + Send + 'static,
    B::Error: Display + Send + 'static,
{
    backend: Arc<Mutex<B>>,
    queue: Arc<LatestFrameQueue>,
    failure: Arc<Mutex<Option<String>>>,
    presented: Arc<AtomicU64>,
    last_epoch: Arc<AtomicU64>,
    join: Option<JoinHandle<()>>,
}

impl<B> Presenter<B>
where
    B: TerminalBackend + Send + 'static,
    B::Error: Display + Send + 'static,
{
    pub fn start(backend: Arc<Mutex<B>>) -> Result<Self, PresenterError> {
        Self::start_observed(backend, None)
    }

    pub fn start_observed(
        backend: Arc<Mutex<B>>,
        observer: Option<PresentationObserver>,
    ) -> Result<Self, PresenterError> {
        let queue = Arc::new(LatestFrameQueue::default());
        let failure = Arc::new(Mutex::new(None));
        let presented = Arc::new(AtomicU64::new(0));
        let last_epoch = Arc::new(AtomicU64::new(0));
        let thread_backend = Arc::clone(&backend);
        let thread_queue = Arc::clone(&queue);
        let thread_failure = Arc::clone(&failure);
        let thread_presented = Arc::clone(&presented);
        let thread_last_epoch = Arc::clone(&last_epoch);
        let thread_observer = observer;
        let join = thread::Builder::new()
            .name("wren-presenter".to_owned())
            .spawn(move || {
                wren_scheduling::mark_interactive();
                presenter_loop(
                    &thread_backend,
                    &thread_queue,
                    &thread_failure,
                    &thread_presented,
                    &thread_last_epoch,
                    thread_observer.as_ref(),
                );
            })
            .map_err(PresenterError::Spawn)?;
        Ok(Self {
            backend,
            queue,
            failure,
            presented,
            last_epoch,
            join: Some(join),
        })
    }

    #[must_use]
    pub fn backend(&self) -> Arc<Mutex<B>> {
        Arc::clone(&self.backend)
    }

    pub fn publish(&self, frame: Arc<DesiredGrid>) -> Result<(), PresenterError> {
        self.check_failure()?;
        self.queue.publish(frame)
    }

    pub fn check_failure(&self) -> Result<(), PresenterError> {
        let failure = self.failure.lock().map_err(|_| PresenterError::Poisoned)?;
        if let Some(failure) = failure.as_ref() {
            return Err(PresenterError::Backend(failure.clone()));
        }
        Ok(())
    }

    pub fn stats(&self) -> Result<PresenterStats, PresenterError> {
        let (published_frames, dropped_frames) = self.queue.publication_stats()?;
        Ok(PresenterStats {
            published_frames,
            dropped_frames,
            presented_frames: self.presented.load(Ordering::Acquire),
            last_presented_epoch: self.last_epoch.load(Ordering::Acquire),
        })
    }

    pub fn finish(mut self) -> Result<PresenterStats, PresenterError> {
        self.stop_and_join()?;
        self.check_failure()?;
        self.stats()
    }

    fn stop_and_join(&mut self) -> Result<(), PresenterError> {
        self.queue.stop();
        if let Some(join) = self.join.take() {
            join.join().map_err(|_| PresenterError::Panicked)?;
        }
        Ok(())
    }
}

impl<B> Drop for Presenter<B>
where
    B: TerminalBackend + Send + 'static,
    B::Error: Display + Send + 'static,
{
    fn drop(&mut self) {
        let _ = self.stop_and_join();
    }
}

fn presenter_loop<B>(
    backend: &Mutex<B>,
    queue: &LatestFrameQueue,
    failure: &Mutex<Option<String>>,
    presented: &AtomicU64,
    last_epoch: &AtomicU64,
    observer: Option<&PresentationObserver>,
) where
    B: TerminalBackend,
    B::Error: Display,
{
    let mut last_fully_written: Option<Arc<DesiredGrid>> = None;
    let mut patches = Vec::<TerminalPatch>::new();
    loop {
        let frame = match queue.take() {
            Ok(Some(frame)) => frame,
            Ok(None) => return,
            Err(error) => {
                store_failure(failure, error.to_string());
                return;
            }
        };
        diff_into(last_fully_written.as_deref(), &frame, &mut patches);
        let write_result = backend
            .lock()
            .map_err(|_| "terminal backend lock is poisoned".to_owned())
            .and_then(|mut backend| backend.submit(&patches).map_err(|error| error.to_string()));
        if let Err(error) = write_result {
            store_failure(failure, error);
            queue.stop();
            return;
        }
        last_epoch.store(frame.epoch, Ordering::Release);
        presented.fetch_add(1, Ordering::AcqRel);
        if let Some(observer) = observer {
            observer(frame.epoch);
        }
        last_fully_written = Some(frame);
    }
}

fn store_failure(failure: &Mutex<Option<String>>, error: String) {
    if let Ok(mut failure) = failure.lock() {
        *failure = Some(error);
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::time::Duration;

    use wren_view::CellRow;

    use super::*;

    #[derive(Default)]
    struct SlowBackend;

    impl TerminalBackend for SlowBackend {
        type Error = Infallible;

        fn submit(&mut self, _patch: &[wren_view::TerminalPatch]) -> Result<(), Self::Error> {
            thread::sleep(Duration::from_millis(1));
            Ok(())
        }
    }

    fn grid(epoch: u64) -> Arc<DesiredGrid> {
        Arc::new(DesiredGrid {
            epoch,
            width: 1,
            height: 1,
            rows: vec![Arc::new(CellRow::default())],
            cursor: (0, 0),
        })
    }

    #[test]
    fn capacity_one_queue_drops_intermediate_grids_but_writes_the_latest() {
        let backend = Arc::new(Mutex::new(SlowBackend));
        let presenter = Presenter::start(backend).expect("presenter");
        for epoch in 1..=100 {
            presenter.publish(grid(epoch)).expect("publish");
        }
        let stats = presenter.finish().expect("finish");
        assert_eq!(stats.published_frames, 100);
        assert!(stats.dropped_frames > 0);
        assert!(stats.presented_frames < stats.published_frames);
        assert_eq!(stats.last_presented_epoch, 100);
    }

    struct BrokenBackend;

    impl TerminalBackend for BrokenBackend {
        type Error = &'static str;

        fn submit(&mut self, _patch: &[wren_view::TerminalPatch]) -> Result<(), Self::Error> {
            Err("write failed")
        }
    }

    #[test]
    fn backend_failure_stops_without_advancing_fully_written_epoch() {
        let backend = Arc::new(Mutex::new(BrokenBackend));
        let presenter = Presenter::start(backend).expect("presenter");
        presenter.publish(grid(7)).expect("publish");
        let error = presenter.finish().expect_err("backend failure");
        assert!(error.to_string().contains("write failed"));
    }

    #[test]
    fn observer_runs_only_after_a_frame_is_fully_written() {
        let backend = Arc::new(Mutex::new(SlowBackend));
        let epochs = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&epochs);
        let observer: PresentationObserver = Arc::new(move |epoch| {
            if let Ok(mut epochs) = observed.lock() {
                epochs.push(epoch);
            }
        });
        let presenter = Presenter::start_observed(backend, Some(observer)).expect("presenter");
        presenter.publish(grid(3)).expect("publish");
        presenter.finish().expect("finish");
        assert_eq!(*epochs.lock().expect("epochs"), vec![3]);
    }
}
