use crate::archive_affinity::ContentClass;
use pithos_core::{PithosError, Result};
use pithos_planner::SolidGroupPlan;

/// R3 global-pool experiment.
///
/// `sources` are already deterministically ordered by affinity before this
/// planner is called. For this branch we intentionally expose the complete
/// ArchiveMax byte stream to one native candidate so FastCDC canonicalization
/// can cross file/class boundaries. v14 re-partitions canonical chunks by
/// content class internally before entropy coding.
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
    let total = lengths.iter().try_fold(0_u64, |total, length| {
        total
            .checked_add(*length)
            .ok_or(PithosError::IntegerOverflow)
    })?;
    Ok(vec![SolidGroupPlan::new(0, lengths.len(), total)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r3_global_pool_keeps_one_archive_wide_group() {
        let classes = [
            ContentClass::Text,
            ContentClass::StructuredText,
            ContentClass::Binary,
            ContentClass::Archive,
        ];
        let plans = plan(&classes, &[3, 4, 5, 6], 8).unwrap();
        assert_eq!(plans, vec![SolidGroupPlan::new(0, 4, 18)]);
    }
}
