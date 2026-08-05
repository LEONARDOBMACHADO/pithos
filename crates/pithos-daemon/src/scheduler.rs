use pithos_agent_api::{JobId, JobPriority, SessionId};
use std::array;
use std::collections::{BTreeMap, VecDeque};

const PRIORITY_WEIGHTS: [u8; 6] = [8, 6, 4, 3, 2, 1];

#[derive(Debug, Clone)]
pub(crate) struct ScheduledJob {
    pub owner: SessionId,
    pub job_id: JobId,
    pub priority: JobPriority,
    pub deadline_unix_ms: Option<u64>,
}

#[derive(Default)]
struct PriorityLane {
    sessions: BTreeMap<String, VecDeque<ScheduledJob>>,
    order: VecDeque<String>,
}

impl PriorityLane {
    fn push(&mut self, job: ScheduledJob) {
        let owner = job.owner.as_str().to_owned();
        if !self.sessions.contains_key(&owner) {
            self.order.push_back(owner.clone());
        }
        self.sessions.entry(owner).or_default().push_back(job);
    }

    fn pop(&mut self) -> Option<ScheduledJob> {
        while let Some(owner) = self.order.pop_front() {
            let (job, keep_session) = match self.sessions.get_mut(&owner) {
                Some(queue) => (queue.pop_front(), !queue.is_empty()),
                None => continue,
            };
            if keep_session {
                self.order.push_back(owner.clone());
            } else {
                self.sessions.remove(&owner);
            }
            if job.is_some() {
                return job;
            }
        }
        None
    }

    fn is_empty(&self) -> bool {
        self.order.is_empty()
    }
}

pub(crate) struct FairScheduler {
    lanes: [PriorityLane; 6],
    credits: [u8; 6],
    cursor: usize,
    len: usize,
}

impl Default for FairScheduler {
    fn default() -> Self {
        Self {
            lanes: array::from_fn(|_| PriorityLane::default()),
            credits: PRIORITY_WEIGHTS,
            cursor: 0,
            len: 0,
        }
    }
}

impl FairScheduler {
    pub fn push(&mut self, job: ScheduledJob) {
        self.lanes[priority_index(job.priority)].push(job);
        self.len = self.len.saturating_add(1);
    }

    pub fn pop(&mut self) -> Option<ScheduledJob> {
        if self.len == 0 {
            return None;
        }
        loop {
            if self.credits.iter().all(|credit| *credit == 0) {
                self.credits = PRIORITY_WEIGHTS;
                self.cursor = 0;
            }
            if self.credits[self.cursor] == 0 || self.lanes[self.cursor].is_empty() {
                self.credits[self.cursor] = 0;
                self.cursor = (self.cursor + 1) % self.lanes.len();
                continue;
            }
            let job = self.lanes[self.cursor].pop();
            self.credits[self.cursor] -= 1;
            if self.credits[self.cursor] == 0 {
                self.cursor = (self.cursor + 1) % self.lanes.len();
            }
            if job.is_some() {
                self.len -= 1;
                return job;
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn drain(&mut self) -> Vec<ScheduledJob> {
        let mut jobs = Vec::with_capacity(self.len);
        while let Some(job) = self.pop() {
            jobs.push(job);
        }
        jobs
    }
}

const fn priority_index(priority: JobPriority) -> usize {
    match priority {
        JobPriority::InteractiveRead => 0,
        JobPriority::InteractiveExtract => 1,
        JobPriority::VerifyRequested => 2,
        JobPriority::PackForeground => 3,
        JobPriority::PackBackground => 4,
        JobPriority::Benchmark => 5,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(owner: u64, number: u64, priority: JobPriority) -> ScheduledJob {
        ScheduledJob {
            owner: SessionId::new(format!("session-{owner:016x}")).unwrap(),
            job_id: JobId::new(format!("job-{number:016x}")).unwrap(),
            priority,
            deadline_unix_ms: None,
        }
    }

    #[test]
    fn higher_priority_overtakes_queued_background_work() {
        let mut queue = FairScheduler::default();
        queue.push(job(1, 1, JobPriority::Benchmark));
        queue.push(job(1, 2, JobPriority::InteractiveRead));

        assert_eq!(queue.pop().unwrap().priority, JobPriority::InteractiveRead);
        assert_eq!(queue.pop().unwrap().priority, JobPriority::Benchmark);
    }

    #[test]
    fn weighted_round_robin_prevents_priority_and_session_starvation() {
        let mut queue = FairScheduler::default();
        for number in 1..=32 {
            queue.push(job(1, number, JobPriority::InteractiveRead));
        }
        queue.push(job(2, 100, JobPriority::InteractiveRead));
        queue.push(job(3, 200, JobPriority::Benchmark));

        let first = queue.pop().unwrap();
        let second = queue.pop().unwrap();
        assert_ne!(first.owner, second.owner);

        let mut saw_benchmark = false;
        for _ in 0..8 {
            if queue.pop().unwrap().priority == JobPriority::Benchmark {
                saw_benchmark = true;
                break;
            }
        }
        assert!(
            saw_benchmark,
            "low priority work must make bounded progress"
        );
    }
}
