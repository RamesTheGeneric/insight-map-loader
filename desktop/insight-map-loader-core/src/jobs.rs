//! Long, multi-step fleet operations with real progress and real errors.
//!
//! The existing button pattern is an `AtomicBool` plus a detached thread whose
//! result is `.ok()`-ed away. That is fine for blinking an LED. It is not fine
//! for sharing a map: nine adb steps across several pucks, minutes long, and
//! destructive in the middle — a failure at step six must not be
//! indistinguishable from success.
//!
//! So a job is a named list of steps, each of which can report progress while
//! it runs and fail with a reason. Jobs are surfaced through `View` like every
//! other piece of service state, and the GUI just paints them.
//!
//! Jobs run **one at a time, in submission order**, deliberately. Two
//! destructive fleet operations must never interleave over one adb server, and
//! several concurrent `dumpsys tracking` calls against one puck is exactly the
//! hazard rule 1 in `fleet.rs` warns about.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    Queued,
    Running,
    Ok,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepState {
    Pending,
    Running,
    Ok,
    /// Deliberately not run — e.g. a puck that already has the map.
    Skipped,
    Failed,
}

#[derive(Debug, Clone)]
pub struct JobStep {
    pub name: String,
    pub state: StepState,
    /// Live detail while the step runs ("waiting for .132 to relocalize: 18s").
    pub detail: String,
    pub elapsed: Duration,
}

impl JobStep {
    pub fn new(name: impl Into<String>) -> JobStep {
        JobStep {
            name: name.into(),
            state: StepState::Pending,
            detail: String::new(),
            elapsed: Duration::ZERO,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Job {
    pub id: u64,
    pub title: String,
    pub state: JobState,
    pub steps: Vec<JobStep>,
    /// The FIRST failure, verbatim. Never summarised, never swallowed.
    pub error: Option<String>,
    pub started: Option<Instant>,
    pub finished: Option<Instant>,
}

impl Job {
    pub fn new(id: u64, title: impl Into<String>, steps: Vec<JobStep>) -> Job {
        Job {
            id,
            title: title.into(),
            state: JobState::Queued,
            steps,
            error: None,
            started: None,
            finished: None,
        }
    }

    pub fn is_active(&self) -> bool {
        matches!(self.state, JobState::Queued | JobState::Running)
    }

    /// One-line summary for the events feed.
    pub fn headline(&self) -> String {
        match self.state {
            JobState::Ok => format!("{} — done", self.title),
            JobState::Failed => {
                format!("{} — FAILED: {}", self.title, self.error.as_deref().unwrap_or("?"))
            }
            JobState::Cancelled => format!("{} — cancelled", self.title),
            _ => self.title.clone(),
        }
    }
}

/// Handle passed to a running job so it can report progress and check for
/// cancellation. Every mutation goes through the shared job list, so the GUI
/// sees motion during a long step instead of a frozen button.
pub struct JobCtx {
    pub id: u64,
    jobs: Arc<std::sync::Mutex<Vec<Job>>>,
    cancel: Arc<AtomicBool>,
    step: usize,
    step_started: Instant,
}

impl JobCtx {
    /// Move to step `i`, marking it running. Steps are addressed by index so a
    /// job's shape is fixed up front and visible in the UI before it runs.
    pub fn begin(&mut self, i: usize) {
        self.step = i;
        self.step_started = Instant::now();
        self.with_step(i, |s| {
            s.state = StepState::Running;
            s.detail.clear();
        });
    }

    pub fn progress(&self, msg: impl Into<String>) {
        let d = msg.into();
        let el = self.step_started.elapsed();
        self.with_step(self.step, |s| {
            s.detail = d.clone();
            s.elapsed = el;
        });
    }

    pub fn finish_ok(&self, detail: impl Into<String>) {
        let d = detail.into();
        let el = self.step_started.elapsed();
        self.with_step(self.step, |s| {
            s.state = StepState::Ok;
            s.detail = d.clone();
            s.elapsed = el;
        });
    }

    pub fn finish_skipped(&self, detail: impl Into<String>) {
        let d = detail.into();
        self.with_step(self.step, |s| {
            s.state = StepState::Skipped;
            s.detail = d.clone();
        });
    }

    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    fn with_step(&self, i: usize, f: impl FnOnce(&mut JobStep)) {
        let mut jobs = self.jobs.lock().unwrap();
        if let Some(job) = jobs.iter_mut().find(|j| j.id == self.id) {
            if let Some(step) = job.steps.get_mut(i) {
                f(step);
            }
        }
    }
}

/// A queued unit of work: the job shell plus the closure that runs it.
pub struct JobRequest {
    pub job: Job,
    pub run: Box<dyn FnOnce(&mut JobCtx) -> Result<String, String> + Send>,
}

/// The runner's shared state. `Service` owns one.
pub struct JobQueue {
    pub jobs: Arc<std::sync::Mutex<Vec<Job>>>,
    next_id: std::sync::atomic::AtomicU64,
    cancel: Arc<AtomicBool>,
}

/// How many finished jobs to keep for the UI.
const KEEP: usize = 8;

impl JobQueue {
    pub fn new() -> JobQueue {
        JobQueue {
            jobs: Arc::new(std::sync::Mutex::new(Vec::new())),
            next_id: std::sync::atomic::AtomicU64::new(1),
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    pub fn snapshot(&self) -> Vec<Job> {
        self.jobs.lock().unwrap().clone()
    }

    pub fn any_active(&self) -> bool {
        self.jobs.lock().unwrap().iter().any(|j| j.is_active())
    }

    /// Cancellation is checked BETWEEN steps only. An in-flight adb call is
    /// already bounded by its own timeout, so the worst case is one step's
    /// wait; interrupting a `chcon` mid-flight would be worse than waiting.
    pub fn request_cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    fn push(&self, job: Job) {
        let mut jobs = self.jobs.lock().unwrap();
        jobs.push(job);
        // Trim finished jobs only; an active one is never dropped.
        while jobs.len() > KEEP && jobs.iter().any(|j| !j.is_active()) {
            if let Some(i) = jobs.iter().position(|j| !j.is_active()) {
                jobs.remove(i);
            } else {
                break;
            }
        }
    }

    fn ctx(&self, id: u64) -> JobCtx {
        JobCtx {
            id,
            jobs: Arc::clone(&self.jobs),
            cancel: Arc::clone(&self.cancel),
            step: 0,
            step_started: Instant::now(),
        }
    }

    fn set_state(&self, id: u64, state: JobState, error: Option<String>) -> Option<Job> {
        let mut jobs = self.jobs.lock().unwrap();
        let job = jobs.iter_mut().find(|j| j.id == id)?;
        job.state = state;
        if error.is_some() {
            job.error = error;
            // Whatever step was running is the one that failed.
            if let Some(s) = job.steps.iter_mut().find(|s| s.state == StepState::Running) {
                s.state = StepState::Failed;
            }
        }
        match state {
            JobState::Running => job.started = Some(Instant::now()),
            JobState::Ok | JobState::Failed | JobState::Cancelled => {
                job.finished = Some(Instant::now())
            }
            _ => {}
        }
        Some(job.clone())
    }
}

impl Default for JobQueue {
    fn default() -> Self {
        JobQueue::new()
    }
}

/// Run queued jobs forever, one at a time. `on_done` gets the finished job so
/// the caller can push a headline into the events feed.
///
/// IMPORTANT: `on_done` is called with NO lock held. Holding the job lock (or
/// the view lock) across a callback is how service.rs deadlocked the aggregate
/// thread and the GUI once already.
pub fn run_queue(
    queue: Arc<JobQueue>,
    rx: std::sync::mpsc::Receiver<JobRequest>,
    mut on_done: impl FnMut(&Job) + Send + 'static,
) {
    for req in rx {
        let id = req.job.id;
        queue.push(req.job);
        // A cancel raised while this job was queued applies to it, not to
        // some future one; clear the flag as each job starts.
        queue.cancel.store(false, Ordering::Relaxed);
        queue.set_state(id, JobState::Running, None);

        let mut ctx = queue.ctx(id);
        let outcome = (req.run)(&mut ctx);

        let finished = match outcome {
            Ok(summary) => {
                let j = queue.set_state(id, JobState::Ok, None);
                if !summary.is_empty() {
                    let mut jobs = queue.jobs.lock().unwrap();
                    if let Some(job) = jobs.iter_mut().find(|j| j.id == id) {
                        job.title = format!("{} — {summary}", job.title);
                    }
                }
                j
            }
            Err(e) if ctx.cancelled() => queue.set_state(id, JobState::Cancelled, Some(e)),
            Err(e) => queue.set_state(id, JobState::Failed, Some(e)),
        };
        if let Some(j) = finished {
            on_done(&j);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn a_failing_step_is_recorded_not_swallowed() {
        let q = Arc::new(JobQueue::new());
        let (tx, rx) = mpsc::channel();
        let id = q.next_id();
        tx.send(JobRequest {
            job: Job::new(id, "test", vec![JobStep::new("one"), JobStep::new("two")]),
            run: Box::new(|ctx| {
                ctx.begin(0);
                ctx.finish_ok("fine");
                ctx.begin(1);
                Err("the chcon failed".into())
            }),
        })
        .unwrap();
        drop(tx);
        let q2 = Arc::clone(&q);
        run_queue(q2, rx, |_| {});

        let jobs = q.snapshot();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].state, JobState::Failed);
        assert_eq!(jobs[0].error.as_deref(), Some("the chcon failed"));
        assert_eq!(jobs[0].steps[0].state, StepState::Ok);
        // The step that was running when it blew up is the one marked failed.
        assert_eq!(jobs[0].steps[1].state, StepState::Failed);
    }

    #[test]
    fn jobs_run_in_order_and_keep_a_bounded_history() {
        let q = Arc::new(JobQueue::new());
        let (tx, rx) = mpsc::channel();
        for n in 0..12 {
            let id = q.next_id();
            tx.send(JobRequest {
                job: Job::new(id, format!("job {n}"), vec![JobStep::new("s")]),
                run: Box::new(|ctx| {
                    ctx.begin(0);
                    ctx.finish_ok("");
                    Ok(String::new())
                }),
            })
            .unwrap();
        }
        drop(tx);
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let s2 = Arc::clone(&seen);
        run_queue(Arc::clone(&q), rx, move |j| s2.lock().unwrap().push(j.id));

        let order = seen.lock().unwrap().clone();
        assert_eq!(order, (1..=12).collect::<Vec<_>>(), "jobs must run in order");
        assert!(q.snapshot().len() <= KEEP, "history must stay bounded");
    }
}
