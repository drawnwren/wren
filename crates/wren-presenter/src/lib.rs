#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::fmt::Display;
use std::io;
use std::sync::Arc;
#[cfg(any(test, feature = "benchmarking"))]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};

use parking_lot::Mutex;
use thiserror::Error;
use wren_term::TerminalBackend;
use wren_view::{DesiredGrid, TerminalUpdate, diff_into};

#[cfg(any(test, feature = "benchmarking"))]
pub type PresentationObserver = Arc<dyn Fn(u64) + Send + Sync + 'static>;

#[derive(Debug, Error)]
pub enum PresenterError {
    #[error("spawn presenter thread: {0}")]
    Spawn(#[source] io::Error),
    #[error("presenter has stopped")]
    Stopped,
    #[error("presenter thread panicked")]
    Panicked,
    #[error("terminal presenter failed: {0}")]
    Backend(String),
}

#[cfg(any(test, feature = "benchmarking"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresenterStats {
    pub published_frames: u64,
    pub dropped_frames: u64,
    pub presented_frames: u64,
    pub last_presented_epoch: u64,
}

#[cfg(not(any(test, feature = "benchmarking")))]
pub type PresenterStats = ();

#[derive(Debug, Default)]
struct LatestFrameQueue {
    slot: Mutex<Option<Arc<DesiredGrid>>>,
    stopped: AtomicBool,
    #[cfg(any(test, feature = "benchmarking"))]
    published: AtomicU64,
    #[cfg(any(test, feature = "benchmarking"))]
    dropped: AtomicU64,
}

impl LatestFrameQueue {
    fn publish(&self, frame: Arc<DesiredGrid>) -> Result<(), PresenterError> {
        let mut slot = self.slot.lock();
        if self.stopped.load(Ordering::Acquire) {
            return Err(PresenterError::Stopped);
        }
        #[cfg(any(test, feature = "benchmarking"))]
        {
            let replaced = slot.replace(frame).is_some();
            self.published.fetch_add(1, Ordering::AcqRel);
            if replaced {
                self.dropped.fetch_add(1, Ordering::AcqRel);
            }
        }
        #[cfg(not(any(test, feature = "benchmarking")))]
        slot.replace(frame);
        Ok(())
    }

    fn take(&self) -> Option<Arc<DesiredGrid>> {
        loop {
            if let Some(frame) = self.slot.lock().take() {
                return Some(frame);
            }
            if self.stopped.load(Ordering::Acquire) {
                return None;
            }
            thread::park();
        }
    }

    fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
    }
}

pub struct Presenter<B>
where
    B: TerminalBackend + Send + 'static,
    B::Error: Display + Send + 'static,
{
    _backend: Arc<Mutex<B>>,
    queue: Arc<LatestFrameQueue>,
    failure: Arc<Mutex<Option<String>>>,
    #[cfg(any(test, feature = "benchmarking"))]
    presented: Arc<AtomicU64>,
    #[cfg(any(test, feature = "benchmarking"))]
    last_epoch: Arc<AtomicU64>,
    worker: thread::Thread,
    join: Option<JoinHandle<()>>,
}

impl<B> Presenter<B>
where
    B: TerminalBackend + Send + 'static,
    B::Error: Display + Send + 'static,
{
    pub fn start(backend: Arc<Mutex<B>>) -> Result<Self, PresenterError> {
        Self::start_inner(
            backend,
            #[cfg(any(test, feature = "benchmarking"))]
            None,
        )
    }

    #[cfg(any(test, feature = "benchmarking"))]
    pub fn start_observed(backend: Arc<Mutex<B>>, observer: Option<PresentationObserver>) -> Result<Self, PresenterError> {
        Self::start_inner(backend, observer)
    }

    fn start_inner(backend: Arc<Mutex<B>>, #[cfg(any(test, feature = "benchmarking"))] observer: Option<PresentationObserver>) -> Result<Self, PresenterError> {
        let queue = Arc::new(LatestFrameQueue::default());
        let failure = Arc::new(Mutex::new(None));
        #[cfg(any(test, feature = "benchmarking"))]
        let presented = Arc::new(AtomicU64::new(0));
        #[cfg(any(test, feature = "benchmarking"))]
        let last_epoch = Arc::new(AtomicU64::new(0));
        let thread_backend = Arc::clone(&backend);
        let thread_queue = Arc::clone(&queue);
        let thread_failure = Arc::clone(&failure);
        #[cfg(any(test, feature = "benchmarking"))]
        let thread_presented = Arc::clone(&presented);
        #[cfg(any(test, feature = "benchmarking"))]
        let thread_last_epoch = Arc::clone(&last_epoch);
        #[cfg(any(test, feature = "benchmarking"))]
        let thread_observer = observer;
        let join = wren_scheduling::spawn_interactive("wren-presenter", move || {
            presenter_loop(
                &thread_backend,
                &thread_queue,
                &thread_failure,
                #[cfg(any(test, feature = "benchmarking"))]
                &thread_presented,
                #[cfg(any(test, feature = "benchmarking"))]
                &thread_last_epoch,
                #[cfg(any(test, feature = "benchmarking"))]
                thread_observer.as_ref(),
            );
        })
        .map_err(PresenterError::Spawn)?;
        let worker = join.thread().clone();
        Ok(Self {
            _backend: backend,
            queue,
            failure,
            #[cfg(any(test, feature = "benchmarking"))]
            presented,
            #[cfg(any(test, feature = "benchmarking"))]
            last_epoch,
            worker,
            join: Some(join),
        })
    }

    pub fn publish(&self, frame: Arc<DesiredGrid>) -> Result<(), PresenterError> {
        self.check_failure()?;
        self.queue.publish(frame)?;
        self.worker.unpark();
        Ok(())
    }

    pub fn check_failure(&self) -> Result<(), PresenterError> {
        let failure = self.failure.lock();
        if let Some(failure) = failure.as_ref() {
            return Err(PresenterError::Backend(failure.clone()));
        }
        Ok(())
    }

    #[cfg(any(test, feature = "benchmarking"))]
    pub fn stats(&self) -> Result<PresenterStats, PresenterError> {
        Ok(PresenterStats {
            published_frames: self.queue.published.load(Ordering::Acquire),
            dropped_frames: self.queue.dropped.load(Ordering::Acquire),
            presented_frames: self.presented.load(Ordering::Acquire),
            last_presented_epoch: self.last_epoch.load(Ordering::Acquire),
        })
    }

    pub fn finish(mut self) -> Result<PresenterStats, PresenterError> {
        self.stop_and_join()?;
        self.check_failure()?;
        #[cfg(any(test, feature = "benchmarking"))]
        return self.stats();
        #[cfg(not(any(test, feature = "benchmarking")))]
        Ok(())
    }

    fn stop_and_join(&mut self) -> Result<(), PresenterError> {
        self.queue.stop();
        self.worker.unpark();
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
    #[cfg(any(test, feature = "benchmarking"))] presented: &AtomicU64,
    #[cfg(any(test, feature = "benchmarking"))] last_epoch: &AtomicU64,
    #[cfg(any(test, feature = "benchmarking"))] observer: Option<&PresentationObserver>,
) where
    B: TerminalBackend,
    B::Error: Display,
{
    let mut last_fully_written: Option<Arc<DesiredGrid>> = None;
    let mut update = TerminalUpdate::default();
    loop {
        let Some(frame) = queue.take() else { return };
        diff_into(last_fully_written.as_deref(), &frame, &mut update);
        let write_result = backend.lock().submit(&update).map_err(|error| error.to_string());
        if let Err(error) = write_result {
            store_failure(failure, error);
            queue.stop();
            return;
        }
        #[cfg(any(test, feature = "benchmarking"))]
        {
            last_epoch.store(frame.epoch, Ordering::Release);
            presented.fetch_add(1, Ordering::AcqRel);
            if let Some(observer) = observer {
                observer(frame.epoch);
            }
        }
        last_fully_written = Some(frame);
    }
}

fn store_failure(failure: &Mutex<Option<String>>, error: String) {
    *failure.lock() = Some(error);
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

        fn submit(&mut self, _update: &wren_view::TerminalUpdate) -> Result<(), Self::Error> {
            thread::sleep(Duration::from_millis(1));
            Ok(())
        }
    }

    fn grid(epoch: u64) -> Arc<DesiredGrid> {
        Arc::new(DesiredGrid { epoch, width: 1, height: 1, rows: vec![Arc::new(CellRow::default())], cursor: (0, 0), raster_overlay: None })
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

        fn submit(&mut self, _update: &wren_view::TerminalUpdate) -> Result<(), Self::Error> {
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
        let observer: PresentationObserver = Arc::new(move |epoch| observed.lock().push(epoch));
        let presenter = Presenter::start_observed(backend, Some(observer)).expect("presenter");
        presenter.publish(grid(3)).expect("publish");
        presenter.finish().expect("finish");
        assert_eq!(*epochs.lock(), vec![3]);
    }
}
