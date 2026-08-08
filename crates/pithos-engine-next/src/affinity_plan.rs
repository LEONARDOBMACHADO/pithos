use crate::archive_affinity::ContentClass;
use pithos_core::{PithosError, Result};
use pithos_planner::SolidGroupPlan;
use std::cell::Cell;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlannerMode {
    Global,
    ClassAware,
}

thread_local! {
    static MODE: Cell<PlannerMode> = const { Cell::new(PlannerMode::Global) };
}

pub(crate) fn with_mode<T>(mode: PlannerMode, operation: impl FnOnce() -> Result<T>) -> Result<T> {
    MODE.with(|cell| {
        let previous = cell.replace(mode);
        let result = operation();
        cell.set(previous);
        result
    })
}

pub(crate) fn plan(
    classes: &[ContentClass],
    lengths: &[u64],
    target_bytes: u64,
) -> Result<Vec<SolidGroupPlan>> {
    validate(classes, lengths, target_bytes)?;
    MODE.with(|mode| match mode.get() {
        PlannerMode::Global => plan_global(lengths),
        PlannerMode::ClassAware => plan_class_aware(classes, lengths, target_bytes),
    })
}

fn validate(classes: &[ContentClass], lengths: &[u64], target_bytes: u64) -> Result<()> {
    if classes.len() != lengths.len() {
        return Err(PithosError::InvalidMetadata("affinity plan input lengths"));
    }
    if target_bytes == 0 {
        return Err(PithosError::InvalidMetadata("affinity plan target"));
    }
    Ok(())
}

fn plan_global(lengths: &[u64]) -> Result<Vec<SolidGroupPlan>> {
    if lengths.is_empty() {
        return Ok(Vec::new());
    }
    let total = lengths.iter().try_fold(0_u64, |total, length| {
        total.checked_add(*length).ok_or(PithosError::IntegerOverflow)
    })?;
    Ok(vec![SolidGroupPlan::new(0, lengths.len(), total)])
}

fn plan_class_aware(
    classes: &[ContentClass],
    lengths: &[u64],
    target_bytes: u64,
) -> Result<Vec<SolidGroupPlan>> {
    if lengths.is_empty() {
        return Ok(Vec::new());
    }
    let mut groups = Vec::new();
    let mut start = 0_usize;
    let mut count = 0_usize;
    let mut total = 0_u64;
    let mut class = classes[0];
    for (index, (&next_class, &length)) in classes.iter().zip(lengths).enumerate() {
        let combined = total.checked_add(length).ok_or(PithosError::IntegerOverflow)?;
        let new_class = count > 0 && next_class != class;
        let over_target = count > 0 && combined > target_bytes;
        if new_class || over_target {
            groups.push(SolidGroupPlan::new(start, count, total));
            start = index;
            count = 0;
            total = 0;
            class = next_class;
        }
        count = count.checked_add(1).ok_or(PithosError::IntegerOverflow)?;
        total = total.checked_add(length).ok_or(PithosError::IntegerOverflow)?;
    }
    groups.push(SolidGroupPlan::new(start, count, total));
    Ok(groups)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoped_mode_switches_between_global_and_class_plans() {
        let classes = [ContentClass::Text, ContentClass::Text, ContentClass::Binary];
        let lengths = [4, 4, 4];
        let global = with_mode(PlannerMode::Global, || plan(&classes, &lengths, 8)).unwrap();
        let class = with_mode(PlannerMode::ClassAware, || plan(&classes, &lengths, 8)).unwrap();
        assert_eq!(global, vec![SolidGroupPlan::new(0, 3, 12)]);
        assert_eq!(
            class,
            vec![SolidGroupPlan::new(0, 2, 8), SolidGroupPlan::new(2, 1, 4)]
        );
    }
}
