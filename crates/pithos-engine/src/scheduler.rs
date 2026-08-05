use crossbeam_channel::bounded;
use pithos_core::{PithosError, Result};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tempfile::NamedTempFile;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobPriority {
    InteractiveRead,
    InteractiveExtract,
    VerifyRequested,
    PackForeground,
    PackBackground,
    Benchmark,
}

impl JobPriority {
    const fn rank(self) -> u8 {
        match self {
            Self::InteractiveRead => 0,
            Self::InteractiveExtract => 1,
            Self::VerifyRequested => 2,
            Self::PackForeground => 3,
            Self::PackBackground => 4,
            Self::Benchmark => 5,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceEstimate {
    pub input_bytes: u64,
    pub scratch_bytes: u64,
    pub output_bound: u64,
}

impl ResourceEstimate {
    pub fn total(self) -> Result<u64> {
        self.input_bytes
            .checked_add(self.scratch_bytes)
            .and_then(|value| value.checked_add(self.output_bound))
            .ok_or(PithosError::IntegerOverflow)
    }
}

#[derive(Debug, Clone, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    pub fn checkpoint(&self) -> Result<()> {
        if self.is_cancelled() {
            Err(PithosError::Cancelled)
        } else {
            Ok(())
        }
    }
}

type TaskAction = Box<dyn FnOnce(&CancellationToken) -> Result<Vec<u8>> + Send + 'static>;

pub struct ScheduledTask {
    pub task_id: u64,
    pub priority: JobPriority,
    pub dependencies: Vec<u64>,
    pub resources: ResourceEstimate,
    action: TaskAction,
}

impl ScheduledTask {
    pub fn new<F>(
        task_id: u64,
        priority: JobPriority,
        dependencies: Vec<u64>,
        resources: ResourceEstimate,
        action: F,
    ) -> Self
    where
        F: FnOnce(&CancellationToken) -> Result<Vec<u8>> + Send + 'static,
    {
        Self {
            task_id,
            priority,
            dependencies,
            resources,
            action: Box::new(action),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    workers: usize,
    memory_budget: u64,
    spool_directory: PathBuf,
}

impl SchedulerConfig {
    pub fn new(workers: usize, memory_budget: u64, spool_directory: &Path) -> Result<Self> {
        if workers == 0 || memory_budget == 0 {
            return Err(PithosError::InvalidMetadata(
                "scheduler limits must be nonzero",
            ));
        }
        if !fs::metadata(spool_directory)?.is_dir() {
            return Err(PithosError::InvalidMetadata(
                "spool path is not a directory",
            ));
        }
        Ok(Self {
            workers,
            memory_budget,
            spool_directory: spool_directory.to_path_buf(),
        })
    }

    pub fn queue_capacity(&self) -> usize {
        self.workers.saturating_mul(2).max(4)
    }
}

#[derive(Debug)]
pub struct SpoolHandle {
    pub task_id: u64,
    file: NamedTempFile,
    pub len: u64,
}

impl SpoolHandle {
    pub fn path(&self) -> &Path {
        self.file.path()
    }

    pub fn read_all(&self) -> Result<Vec<u8>> {
        let mut file = self.file.reopen()?;
        let capacity = usize::try_from(self.len).map_err(|_| PithosError::IntegerOverflow)?;
        let mut output = Vec::with_capacity(capacity);
        file.read_to_end(&mut output)?;
        Ok(output)
    }
}

struct WorkerResult {
    memory: u64,
    result: Result<SpoolHandle>,
}

pub fn execute_scheduled(
    tasks: Vec<ScheduledTask>,
    config: SchedulerConfig,
    cancellation: CancellationToken,
) -> Result<Vec<SpoolHandle>> {
    validate_dag(&tasks)?;
    cancellation.checkpoint()?;

    let task_count = tasks.len();
    let mut pending = Vec::with_capacity(task_count);
    for task in tasks {
        let memory = task.resources.total()?;
        if memory > config.memory_budget {
            return Err(PithosError::MemoryLimit);
        }
        pending.push(Some((task, memory)));
    }

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(config.workers)
        .build()
        .map_err(|_| PithosError::InvalidMetadata("failed to build scheduler worker pool"))?;
    let (sender, receiver) = bounded::<WorkerResult>(config.queue_capacity());
    let mut available_memory = config.memory_budget;
    let mut running = 0_usize;
    let mut completed_ids = BTreeSet::new();
    let mut completed = Vec::with_capacity(task_count);

    while completed.len() < task_count {
        let mut dispatched = false;
        while running < config.workers && !cancellation.is_cancelled() {
            let Some(index) = next_ready_task(&pending, &completed_ids, available_memory) else {
                break;
            };
            let (task, memory) = pending[index]
                .take()
                .ok_or(PithosError::InvalidMetadata("task dispatched twice"))?;
            available_memory = available_memory
                .checked_sub(memory)
                .ok_or(PithosError::IntegerOverflow)?;
            running = running.checked_add(1).ok_or(PithosError::IntegerOverflow)?;
            dispatched = true;

            let sender = sender.clone();
            let task_cancellation = cancellation.clone();
            let spool_directory = config.spool_directory.clone();
            pool.spawn(move || {
                let result = run_task(task, &spool_directory, &task_cancellation);
                let _ = sender.send(WorkerResult { memory, result });
            });
        }

        if running == 0 {
            if cancellation.is_cancelled() {
                return Err(PithosError::Cancelled);
            }
            return Err(PithosError::DependencyCycle);
        }

        if !dispatched || running >= config.workers {
            let worker = receiver
                .recv()
                .map_err(|_| PithosError::InvalidMetadata("scheduler result channel closed"))?;
            running = running.checked_sub(1).ok_or(PithosError::IntegerOverflow)?;
            available_memory = available_memory
                .checked_add(worker.memory)
                .ok_or(PithosError::IntegerOverflow)?;
            match worker.result {
                Ok(spool) => {
                    completed_ids.insert(spool.task_id);
                    completed.push(spool);
                }
                Err(error) => {
                    cancellation.cancel();
                    return Err(error);
                }
            }
        }
    }

    completed.sort_by_key(|spool| spool.task_id);
    Ok(completed)
}

fn run_task(
    task: ScheduledTask,
    spool_directory: &Path,
    cancellation: &CancellationToken,
) -> Result<SpoolHandle> {
    cancellation.checkpoint()?;
    let task_id = task.task_id;
    let output_bound = task.resources.output_bound;
    let payload = (task.action)(cancellation)?;
    cancellation.checkpoint()?;
    let len = u64::try_from(payload.len()).map_err(|_| PithosError::IntegerOverflow)?;
    if len > output_bound {
        return Err(PithosError::ResourceLimit(
            "scheduled task exceeded reserved output bound",
        ));
    }
    let mut file = tempfile::Builder::new()
        .prefix("pithos-spool-")
        .tempfile_in(spool_directory)?;
    file.write_all(&payload)?;
    file.flush()?;
    Ok(SpoolHandle { task_id, file, len })
}

fn next_ready_task(
    pending: &[Option<(ScheduledTask, u64)>],
    completed: &BTreeSet<u64>,
    available_memory: u64,
) -> Option<usize> {
    pending
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            let (task, memory) = item.as_ref()?;
            if *memory > available_memory
                || !task
                    .dependencies
                    .iter()
                    .all(|dependency| completed.contains(dependency))
            {
                return None;
            }
            Some(((task.priority.rank(), task.task_id), index))
        })
        .min_by_key(|(key, _)| *key)
        .map(|(_, index)| index)
}

fn validate_dag(tasks: &[ScheduledTask]) -> Result<()> {
    let ids = tasks
        .iter()
        .map(|task| task.task_id)
        .collect::<BTreeSet<_>>();
    if ids.len() != tasks.len() {
        return Err(PithosError::InvalidMetadata("duplicate scheduler task ID"));
    }

    let mut indegree = BTreeMap::new();
    let mut dependents = BTreeMap::<u64, Vec<u64>>::new();
    for task in tasks {
        let unique_dependencies = task.dependencies.iter().copied().collect::<BTreeSet<_>>();
        if unique_dependencies.len() != task.dependencies.len()
            || unique_dependencies
                .iter()
                .any(|dependency| !ids.contains(dependency))
        {
            return Err(PithosError::InvalidMetadata("invalid scheduler dependency"));
        }
        indegree.insert(task.task_id, unique_dependencies.len());
        for dependency in unique_dependencies {
            dependents.entry(dependency).or_default().push(task.task_id);
        }
    }

    let mut ready = indegree
        .iter()
        .filter_map(|(id, count)| (*count == 0).then_some(*id))
        .collect::<VecDeque<_>>();
    let mut visited = 0_usize;
    while let Some(id) = ready.pop_front() {
        visited = visited.checked_add(1).ok_or(PithosError::IntegerOverflow)?;
        if let Some(children) = dependents.get(&id) {
            for child in children {
                let count = indegree
                    .get_mut(child)
                    .ok_or(PithosError::InvalidMetadata("invalid scheduler dependency"))?;
                *count = count.checked_sub(1).ok_or(PithosError::IntegerOverflow)?;
                if *count == 0 {
                    ready.push_back(*child);
                }
            }
        }
    }
    if visited != tasks.len() {
        return Err(PithosError::DependencyCycle);
    }
    Ok(())
}
