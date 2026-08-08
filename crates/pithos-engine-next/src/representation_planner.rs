use crate::{affinity_plan, prescreen_pack};
use pithos_core::{CompressionProfile, PithosError, Result};
use pithos_engine_legacy::{CancellationToken, PackLimits, PackRequest};
use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use std::time::Instant;

pub fn pack(request: PackRequest) -> Result<()> {
    pack_with_control(request, &CancellationToken::new())
}

pub fn pack_with_control(request: PackRequest, cancellation: &CancellationToken) -> Result<()> {
    pack_with_limits_and_control(request, &PackLimits::default(), cancellation)
}

pub fn pack_with_limits_and_control(
    request: PackRequest,
    limits: &PackLimits,
    cancellation: &CancellationToken,
) -> Result<()> {
    if request.profile != CompressionProfile::ArchiveMax {
        return prescreen_pack::pack_with_limits_and_control(request, limits, cancellation);
    }
    checkpoint(cancellation)?;
    if request.inputs.is_empty() {
        return Err(PithosError::InvalidMetadata("nenhuma entrada"));
    }
    if path_entry_exists(&request.output)? {
        return Err(PithosError::OutputExists);
    }

    // Explicit finite temporary-space budgets preserve their existing semantic:
    // do not hold two full candidate archives at once. The proven class-aware
    // strategy is used in that constrained mode. The default/unbounded planner
    // performs exact complete-archive arbitration.
    if limits.max_temp_bytes != u64::MAX {
        return affinity_plan::with_mode(affinity_plan::PlannerMode::ClassAware, || {
            prescreen_pack::pack_with_limits_and_control(request, limits, cancellation)
        });
    }

    let PackRequest {
        inputs,
        output,
        profile,
    } = request;
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;

    // With one member the class-aware and global plans are physically
    // equivalent: both contain exactly one solid group with that member. Do not
    // encode the same archive twice merely to prove an equality that follows
    // from the planner inputs. This materially reduces R5 individual-case work
    // without changing the selected bytes.
    if inputs.len() == 1 {
        trace_archive_scope("begin", "class-aware");
        let started = Instant::now();
        let result = affinity_plan::with_mode(affinity_plan::PlannerMode::ClassAware, || {
            prescreen_pack::pack_with_limits_and_control(
                PackRequest {
                    inputs,
                    output: output.clone(),
                    profile,
                },
                limits,
                cancellation,
            )
        });
        let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        trace_archive_scope("end", "class-aware");
        result?;
        let bytes = fs::metadata(&output)?.len();
        if representation_trace_enabled() {
            eprintln!(
                "PITHOS_REP_TRACE\tstage=archive_candidate\tcandidate=class-aware\tbytes={bytes}\tms={elapsed_ms:.3}\treason=single-input-equivalent"
            );
            eprintln!(
                "PITHOS_REP_TRACE\tstage=archive_winner\twinner=class-aware\tbytes={bytes}\tclass_bytes={bytes}\tglobal_bytes={bytes}\ttotal_candidate_ms={elapsed_ms:.3}\treason=single-input-equivalent"
            );
        }
        return Ok(());
    }

    let candidates = tempfile::Builder::new()
        .prefix(".pithos-representation-planner-")
        .tempdir_in(parent)?;
    let class_path = candidates.path().join("class-aware.pits");
    let global_path = candidates.path().join("global.pits");

    trace_archive_scope("begin", "class-aware");
    let class_started = Instant::now();
    let class_result = affinity_plan::with_mode(affinity_plan::PlannerMode::ClassAware, || {
        prescreen_pack::pack_with_limits_and_control(
            PackRequest {
                inputs: inputs.clone(),
                output: class_path.clone(),
                profile,
            },
            limits,
            cancellation,
        )
    });
    let class_ms = class_started.elapsed().as_secs_f64() * 1000.0;
    trace_archive_scope("end", "class-aware");
    class_result?;
    checkpoint(cancellation)?;

    trace_archive_scope("begin", "global");
    let global_started = Instant::now();
    let global_result = affinity_plan::with_mode(affinity_plan::PlannerMode::Global, || {
        prescreen_pack::pack_with_limits_and_control(
            PackRequest {
                inputs,
                output: global_path.clone(),
                profile,
            },
            limits,
            cancellation,
        )
    });
    let global_ms = global_started.elapsed().as_secs_f64() * 1000.0;
    trace_archive_scope("end", "global");
    global_result?;
    checkpoint(cancellation)?;

    let class_bytes = fs::metadata(&class_path)?.len();
    let global_bytes = fs::metadata(&global_path)?.len();
    let (winner_name, winner) = if global_bytes < class_bytes {
        ("global", &global_path)
    } else {
        ("class-aware", &class_path)
    };

    if representation_trace_enabled() {
        eprintln!(
            "PITHOS_REP_TRACE\tstage=archive_candidate\tcandidate=class-aware\tbytes={class_bytes}\tms={class_ms:.3}"
        );
        eprintln!(
            "PITHOS_REP_TRACE\tstage=archive_candidate\tcandidate=global\tbytes={global_bytes}\tms={global_ms:.3}"
        );
        eprintln!(
            "PITHOS_REP_TRACE\tstage=archive_winner\twinner={winner_name}\tbytes={}\tclass_bytes={class_bytes}\tglobal_bytes={global_bytes}\ttotal_candidate_ms={:.3}",
            class_bytes.min(global_bytes),
            class_ms + global_ms
        );
    }

    fs::rename(winner, &output)?;
    sync_parent(&output)?;
    Ok(())
}

fn trace_archive_scope(phase: &str, candidate: &str) {
    if representation_trace_enabled() {
        eprintln!(
            "PITHOS_REP_TRACE\tstage=archive_scope\tphase={phase}\tcandidate={candidate}"
        );
    }
}

fn representation_trace_enabled() -> bool {
    std::env::var("PITHOS_REP_TRACE").ok().is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

fn checkpoint(cancellation: &CancellationToken) -> Result<()> {
    if cancellation.is_cancelled() {
        Err(PithosError::Cancelled)
    } else {
        Ok(())
    }
}

fn path_entry_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn sync_parent(_path: &Path) -> Result<()> {
    Ok(())
}
