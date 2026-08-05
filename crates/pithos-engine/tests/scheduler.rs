use pithos_core::PithosError;
use pithos_engine::scheduler::{
    CancellationToken, JobPriority, ResourceEstimate, ScheduledTask, SchedulerConfig,
    execute_scheduled,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

fn estimate(bytes: u64) -> ResourceEstimate {
    ResourceEstimate {
        input_bytes: 0,
        scratch_bytes: 0,
        output_bound: bytes,
    }
}

#[test]
fn scheduler_honors_dag_priority_and_returns_deterministic_spool_order() {
    let execution = Arc::new(Mutex::new(Vec::new()));
    let low_log = Arc::clone(&execution);
    let dependent_log = Arc::clone(&execution);
    let high_log = Arc::clone(&execution);
    let tasks = vec![
        ScheduledTask::new(30, JobPriority::Benchmark, vec![], estimate(16), move |_| {
            low_log.lock().unwrap().push(30);
            Ok(b"low".to_vec())
        }),
        ScheduledTask::new(
            20,
            JobPriority::InteractiveRead,
            vec![10],
            estimate(16),
            move |_| {
                dependent_log.lock().unwrap().push(20);
                Ok(b"dependent".to_vec())
            },
        ),
        ScheduledTask::new(
            10,
            JobPriority::InteractiveRead,
            vec![],
            estimate(16),
            move |_| {
                high_log.lock().unwrap().push(10);
                Ok(b"high".to_vec())
            },
        ),
    ];
    let directory = tempfile::tempdir().unwrap();
    let config = SchedulerConfig::new(1, 16, directory.path()).unwrap();
    let completed = execute_scheduled(tasks, config, CancellationToken::new()).unwrap();

    assert_eq!(execution.lock().unwrap().as_slice(), &[10, 20, 30]);
    assert_eq!(
        completed
            .iter()
            .map(|item| item.task_id)
            .collect::<Vec<_>>(),
        [10, 20, 30]
    );
    assert_eq!(completed[1].read_all().unwrap(), b"dependent");
}

#[test]
fn scheduler_enforces_weighted_memory_budget() {
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let make_task = |id| {
        let active = Arc::clone(&active);
        let maximum = Arc::clone(&maximum);
        ScheduledTask::new(
            id,
            JobPriority::PackForeground,
            vec![],
            estimate(8),
            move |_| {
                let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                maximum.fetch_max(current, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(10));
                active.fetch_sub(1, Ordering::SeqCst);
                Ok(vec![id as u8])
            },
        )
    };
    let directory = tempfile::tempdir().unwrap();
    let config = SchedulerConfig::new(2, 8, directory.path()).unwrap();
    execute_scheduled(
        vec![make_task(1), make_task(2)],
        config,
        CancellationToken::new(),
    )
    .unwrap();
    assert_eq!(maximum.load(Ordering::SeqCst), 1);
}

#[test]
fn scheduler_rejects_cycles_oversized_jobs_and_observes_cancellation() {
    let directory = tempfile::tempdir().unwrap();
    let config = SchedulerConfig::new(2, 8, directory.path()).unwrap();
    let cycle = vec![
        ScheduledTask::new(1, JobPriority::Benchmark, vec![2], estimate(1), |_| {
            Ok(vec![])
        }),
        ScheduledTask::new(2, JobPriority::Benchmark, vec![1], estimate(1), |_| {
            Ok(vec![])
        }),
    ];
    assert!(matches!(
        execute_scheduled(cycle, config.clone(), CancellationToken::new()),
        Err(PithosError::DependencyCycle)
    ));

    let oversized = vec![ScheduledTask::new(
        1,
        JobPriority::Benchmark,
        vec![],
        estimate(9),
        |_| Ok(vec![]),
    )];
    assert!(matches!(
        execute_scheduled(oversized, config.clone(), CancellationToken::new()),
        Err(PithosError::MemoryLimit)
    ));

    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let cancelled = vec![ScheduledTask::new(
        1,
        JobPriority::InteractiveRead,
        vec![],
        estimate(1),
        |_| Ok(vec![]),
    )];
    assert!(matches!(
        execute_scheduled(cancelled, config, cancellation),
        Err(PithosError::Cancelled)
    ));
}

#[test]
fn scheduler_queue_capacity_is_bounded_from_worker_count() {
    let directory = tempfile::tempdir().unwrap();
    assert_eq!(
        SchedulerConfig::new(1, 1, directory.path())
            .unwrap()
            .queue_capacity(),
        4
    );
    assert_eq!(
        SchedulerConfig::new(4, 1, directory.path())
            .unwrap()
            .queue_capacity(),
        8
    );
}

#[test]
fn scheduler_rejects_payload_larger_than_reserved_output_bound() {
    let directory = tempfile::tempdir().unwrap();
    let config = SchedulerConfig::new(1, 8, directory.path()).unwrap();
    let task = ScheduledTask::new(
        1,
        JobPriority::PackForeground,
        vec![],
        estimate(1),
        |_| Ok(vec![1, 2]),
    );
    assert!(matches!(
        execute_scheduled(vec![task], config, CancellationToken::new()),
        Err(PithosError::ResourceLimit(_))
    ));
}
