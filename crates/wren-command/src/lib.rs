#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use thiserror::Error;
use wren_types::{CommandTask, CommandTaskId, DocumentId, Effects};

type TaskWork = Box<dyn FnOnce(&mut TaskContext) -> Result<Effects, TaskFailure> + Send + 'static>;

enum WorkerMessage {
    Run {
        task: CommandTask,
        cancellation: CancellationToken,
        work: TaskWork,
    },
    Stop,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum TaskFailure {
    #[error("task was cancelled")]
    Cancelled,
    #[error("task failed: {0}")]
    Failed(Box<str>),
    #[error("task panicked")]
    Panicked,
}

#[derive(Debug, Error)]
pub enum TaskRunnerError {
    #[error("task queue is closed")]
    QueueClosed,
    #[error("task queue is full")]
    QueueFull,
    #[error("spawn task worker: {0}")]
    Spawn(#[source] io::Error),
    #[error("task runner lock is poisoned")]
    Poisoned,
    #[error("task ID {0:?} is already running")]
    DuplicateTask(CommandTaskId),
}

#[derive(Debug, Clone)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

impl CancellationToken {
    #[must_use]
    pub fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

#[derive(Debug)]
pub struct TaskContext {
    cancellation: CancellationToken,
    last_checkpoint: Instant,
    max_checkpoint_gap: Duration,
    checkpoints: u64,
}

impl TaskContext {
    fn new(cancellation: CancellationToken) -> Self {
        Self {
            cancellation,
            last_checkpoint: Instant::now(),
            max_checkpoint_gap: Duration::ZERO,
            checkpoints: 0,
        }
    }

    pub fn checkpoint(&mut self) -> Result<(), TaskFailure> {
        self.checkpoint_with(thread::yield_now)
    }

    fn checkpoint_with(&mut self, yield_worker: impl FnOnce()) -> Result<(), TaskFailure> {
        let now = Instant::now();
        self.max_checkpoint_gap = self
            .max_checkpoint_gap
            .max(now.saturating_duration_since(self.last_checkpoint));
        self.checkpoints = self.checkpoints.saturating_add(1);
        if self.cancellation.is_cancelled() {
            return Err(TaskFailure::Cancelled);
        }
        yield_worker();
        // A scheduler stall after yielding is time made available to the UI,
        // not time during which this task withheld its next checkpoint.
        self.last_checkpoint = Instant::now();
        Ok(())
    }

    #[must_use]
    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }
}

#[derive(Debug)]
pub struct TaskResult {
    pub task: CommandTask,
    pub outcome: Result<Effects, TaskFailure>,
    pub elapsed: Duration,
    pub max_checkpoint_gap: Duration,
    pub checkpoints: u64,
}

#[derive(Debug, Default)]
struct BarrierState {
    tasks: HashMap<CommandTaskId, Vec<DocumentId>>,
    documents: HashMap<DocumentId, usize>,
}

pub struct TaskRunner {
    sender: SyncSender<WorkerMessage>,
    results: Receiver<TaskResult>,
    barriers: Arc<Mutex<BarrierState>>,
    workers: Vec<JoinHandle<()>>,
}

impl TaskRunner {
    pub fn new(worker_count: usize, queue_capacity: usize) -> Result<Self, TaskRunnerError> {
        let (sender, receiver) = mpsc::sync_channel(queue_capacity.max(1));
        let receiver = Arc::new(Mutex::new(receiver));
        let (result_sender, results) = mpsc::channel();
        let barriers = Arc::new(Mutex::new(BarrierState::default()));
        let mut workers = Vec::with_capacity(worker_count.max(1));
        for index in 0..worker_count.max(1) {
            let receiver = Arc::clone(&receiver);
            let result_sender = result_sender.clone();
            let barriers = Arc::clone(&barriers);
            let worker = thread::Builder::new()
                .name(format!("wren-command-{index}"))
                .spawn(move || worker_loop(&receiver, &result_sender, &barriers))
                .map_err(TaskRunnerError::Spawn)?;
            workers.push(worker);
        }
        Ok(Self {
            sender,
            results,
            barriers,
            workers,
        })
    }

    pub fn submit(
        &self,
        task: CommandTask,
        work: impl FnOnce(&mut TaskContext) -> Result<Effects, TaskFailure> + Send + 'static,
    ) -> Result<CancellationToken, TaskRunnerError> {
        {
            let mut barriers = self
                .barriers
                .lock()
                .map_err(|_| TaskRunnerError::Poisoned)?;
            if barriers.tasks.contains_key(&task.task_id) {
                return Err(TaskRunnerError::DuplicateTask(task.task_id));
            }
            for document_id in &task.affected_documents {
                *barriers.documents.entry(*document_id).or_default() += 1;
            }
            barriers
                .tasks
                .insert(task.task_id, task.affected_documents.clone());
        }
        let cancellation = CancellationToken::new();
        match self.sender.try_send(WorkerMessage::Run {
            task: task.clone(),
            cancellation: cancellation.clone(),
            work: Box::new(work),
        }) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                release_barrier(&self.barriers, task.task_id);
                return Err(TaskRunnerError::QueueFull);
            }
            Err(TrySendError::Disconnected(_)) => {
                release_barrier(&self.barriers, task.task_id);
                return Err(TaskRunnerError::QueueClosed);
            }
        }
        Ok(cancellation)
    }

    pub fn try_result(&self) -> Result<Option<TaskResult>, TaskRunnerError> {
        match self.results.try_recv() {
            Ok(result) => Ok(Some(result)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(TaskRunnerError::QueueClosed),
        }
    }

    #[must_use]
    pub fn is_document_blocked(&self, document_id: DocumentId) -> bool {
        self.barriers
            .lock()
            .ok()
            .is_some_and(|barriers| barriers.documents.contains_key(&document_id))
    }
}

impl Drop for TaskRunner {
    fn drop(&mut self) {
        for _ in &self.workers {
            let _ = self.sender.send(WorkerMessage::Stop);
        }
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

fn worker_loop(
    receiver: &Mutex<mpsc::Receiver<WorkerMessage>>,
    results: &mpsc::Sender<TaskResult>,
    barriers: &Mutex<BarrierState>,
) {
    loop {
        let message = match receiver.lock() {
            Ok(receiver) => receiver.recv(),
            Err(_) => return,
        };
        let Ok(message) = message else {
            return;
        };
        let WorkerMessage::Run {
            task,
            cancellation,
            work,
        } = message
        else {
            return;
        };
        let started = Instant::now();
        let mut context = TaskContext::new(cancellation);
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(&mut context)))
            .unwrap_or(Err(TaskFailure::Panicked));
        let elapsed = started.elapsed();
        release_barrier_direct(barriers, task.task_id);
        let _ = results.send(TaskResult {
            task,
            outcome,
            elapsed,
            max_checkpoint_gap: context.max_checkpoint_gap,
            checkpoints: context.checkpoints,
        });
    }
}

fn release_barrier(barriers: &Arc<Mutex<BarrierState>>, task_id: CommandTaskId) {
    if let Ok(mut barriers) = barriers.lock() {
        release_barrier_state(&mut barriers, task_id);
    }
}

fn release_barrier_direct(barriers: &Mutex<BarrierState>, task_id: CommandTaskId) {
    if let Ok(mut barriers) = barriers.lock() {
        release_barrier_state(&mut barriers, task_id);
    }
}

fn release_barrier_state(barriers: &mut BarrierState, task_id: CommandTaskId) {
    let Some(documents) = barriers.tasks.remove(&task_id) else {
        return;
    };
    for document_id in documents {
        if let Some(count) = barriers.documents.get_mut(&document_id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                barriers.documents.remove(&document_id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::sync::mpsc;

    use super::*;

    fn task(id: u64) -> CommandTask {
        CommandTask {
            task_id: CommandTaskId::new(id),
            affected_documents: vec![DocumentId::new(4)],
            label: "test".into(),
        }
    }

    fn wait_result(runner: &TaskRunner) -> TaskResult {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(result) = runner.try_result().expect("poll result") {
                return result;
            }
            assert!(Instant::now() < deadline, "task timed out");
            thread::yield_now();
        }
    }

    #[test]
    fn task_runs_off_thread_and_document_barrier_clears_on_completion() {
        let runner = TaskRunner::new(1, 2).expect("runner");
        let (started, wait) = mpsc::channel();
        let (release, released) = mpsc::channel();
        runner
            .submit(task(1), move |context| {
                started.send(()).expect("started");
                released.recv().expect("release");
                context.checkpoint()?;
                Ok(Effects {
                    messages: vec!["complete".into()],
                    ..Effects::default()
                })
            })
            .expect("submit");
        wait.recv().expect("task started");
        assert!(runner.is_document_blocked(DocumentId::new(4)));
        release.send(()).expect("release task");
        let result = wait_result(&runner);
        assert_eq!(
            result.outcome.expect("success").messages,
            vec![Box::<str>::from("complete")]
        );
        assert!(!runner.is_document_blocked(DocumentId::new(4)));
    }

    #[test]
    fn checkpoint_gap_restarts_after_the_worker_yields() {
        let mut context = TaskContext::new(CancellationToken::new());
        let yielded_at = Cell::new(None);
        context
            .checkpoint_with(|| yielded_at.set(Some(Instant::now())))
            .expect("checkpoint");

        assert!(context.last_checkpoint >= yielded_at.get().expect("yield timestamp"));
    }

    #[test]
    fn cooperative_cancellation_is_reported_and_releases_barrier() {
        let runner = TaskRunner::new(1, 2).expect("runner");
        let (started, wait) = mpsc::channel();
        let cancellation = runner
            .submit(task(2), move |context| {
                started.send(()).expect("started");
                loop {
                    context.checkpoint()?;
                }
            })
            .expect("submit");
        wait.recv().expect("task started");
        cancellation.cancel();
        let result = wait_result(&runner);
        assert_eq!(result.outcome, Err(TaskFailure::Cancelled));
        assert!(result.checkpoints > 0);
        assert!(!runner.is_document_blocked(DocumentId::new(4)));
    }

    #[test]
    fn panics_are_contained_at_the_task_failure_boundary() {
        let runner = TaskRunner::new(1, 1).expect("runner");
        runner
            .submit(task(3), |_| panic!("provider bug"))
            .expect("submit");
        let result = wait_result(&runner);
        assert_eq!(result.outcome, Err(TaskFailure::Panicked));
    }
}
