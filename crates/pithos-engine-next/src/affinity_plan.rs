use crate::archive_affinity::ContentClass;
use pithos_core::{PithosError, Result};
use pithos_planner::SolidGroupPlan;

pub(crate) fn plan(
    classes: &[ContentClass],
    lengths: &[u64],
    target_bytes: u64,
) -> Result<Vec<SolidGroupPlan>> {
    if classes.len() != lengths.len() {
        return Err(PithosError::InvalidMetadata("affinity plan input lengths"));
    }
    if target_bytes == 0 {
        return Err(PithosError::InvalidMetadata("affinity plan target"));
    }
    if lengths.is_empty() {
        return Ok(Vec::new());
    }

    let mut groups = Vec::new();
    let mut start = 0_usize;
    let mut count = 0_usize;
    let mut total = 0_u64;
    let mut class = classes[0];

    for (index, (&next_class, &length)) in classes.iter().zip(lengths).enumerate() {
        let combined = total
            .checked_add(length)
            .ok_or(PithosError::IntegerOverflow)?;
        let starts_new_class = count > 0 && next_class != class;
        let exceeds_target = count > 0 && combined > target_bytes;
        if starts_new_class || exceeds_target {
            groups.push(SolidGroupPlan::new(start, count, total));
            start = index;
            count = 0;
            total = 0;
            class = next_class;
        }
        count = count.checked_add(1).ok_or(PithosError::IntegerOverflow)?;
        total = total
            .checked_add(length)
            .ok_or(PithosError::IntegerOverflow)?;
    }
    groups.push(SolidGroupPlan::new(start, count, total));
    Ok(groups)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_boundary_starts_a_new_solid_group() {
        let classes = [
            ContentClass::Text,
            ContentClass::Text,
            ContentClass::Binary,
            ContentClass::Binary,
        ];
        let plans = plan(&classes, &[3, 4, 5, 6], 100).unwrap();
        assert_eq!(
            plans,
            vec![SolidGroupPlan::new(0, 2, 7), SolidGroupPlan::new(2, 2, 11)]
        );
    }

    #[test]
    fn target_still_splits_a_large_class() {
        let classes = [ContentClass::Text; 3];
        let plans = plan(&classes, &[4, 4, 4], 8).unwrap();
        assert_eq!(
            plans,
            vec![SolidGroupPlan::new(0, 2, 8), SolidGroupPlan::new(2, 1, 4)]
        );
    }
}
