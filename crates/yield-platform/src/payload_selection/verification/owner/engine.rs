use super::{Job, PayloadStagePhase, SelectionError};
use std::{
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread::{JoinHandle, Thread},
    time::Duration,
};

pub(super) struct Engine {
    pub(super) busy: AtomicBool,
    sender: SyncSender<Arc<Job>>,
    thread: OnceLock<Thread>,
    _join: Mutex<Option<JoinHandle<()>>>,
}
impl Engine {
    pub(super) fn global() -> Option<Arc<Self>> {
        static GLOBAL: OnceLock<Result<Arc<Engine>, SelectionError>> = OnceLock::new();
        GLOBAL.get_or_init(Self::new).as_ref().ok().cloned()
    }
    pub(super) fn new() -> Result<Arc<Self>, SelectionError> {
        let (sender, receiver) = mpsc::sync_channel(1);
        let engine = Arc::new(Self {
            busy: AtomicBool::new(false),
            sender,
            thread: OnceLock::new(),
            _join: Mutex::new(None),
        });
        let weak = Arc::downgrade(&engine);
        let thread = std::thread::Builder::new()
            .name("yield-payload-owner".into())
            .spawn(move || run(weak, receiver))
            .map_err(|_| SelectionError::Unavailable)?;
        engine
            .thread
            .set(thread.thread().clone())
            .map_err(|_| SelectionError::Unavailable)?;
        *engine
            ._join
            .lock()
            .map_err(|_| SelectionError::Unavailable)? = Some(thread);
        Ok(engine)
    }
    pub(super) fn wake(&self) {
        if let Some(thread) = self.thread.get() {
            thread.unpark();
        }
    }
    pub(super) fn submit(&self, job: Arc<Job>) -> Result<(), SelectionError> {
        self.sender
            .try_send(job)
            .map_err(|_| SelectionError::Unavailable)?;
        self.wake();
        Ok(())
    }
}
fn run(weak: Weak<Engine>, receiver: Receiver<Arc<Job>>) {
    loop {
        let Some(engine) = weak.upgrade() else {
            return;
        };
        let job = match receiver.recv_timeout(Duration::from_millis(25)) {
            Ok(job) => job,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                drop(engine);
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        };
        let result = super::pipeline::attempt(&job);
        let terminal = match result {
            Ok(payload) => {
                // This same serial owner retains the only verified buffer. No UI result/channel
                // carries it. Staged has no attempt deadline; explicit lifetime retirement applies.
                while job.lease.live() {
                    std::thread::park_timeout(Duration::from_millis(10));
                }
                drop(payload);
                PayloadStagePhase::Unstaged
            }
            Err(SelectionError::Offline) => PayloadStagePhase::Offline,
            Err(SelectionError::Unavailable) => PayloadStagePhase::Unavailable,
            Err(SelectionError::Invalid) => PayloadStagePhase::Failed,
            Err(SelectionError::Retired) => PayloadStagePhase::Unstaged,
        };
        // All pipeline stack buffers, sockets and any verified bytes have retired on this owner.
        // Only here may resource retirement be acknowledged and the next job admitted. The serial
        // worker remains alive; it cannot execute a second attempt concurrently or send late data.
        if let Ok(mut state) = job.state.lock() {
            state.phase = terminal;
            state.clear_bytes();
        }
        #[cfg(test)]
        if let Some(witness) = &job.witness {
            *witness.retired_thread.lock().unwrap() = Some(std::thread::current().id());
        }
        job.confirmed.store(true, Ordering::Release);
        engine.busy.store(false, Ordering::Release);
    }
}
