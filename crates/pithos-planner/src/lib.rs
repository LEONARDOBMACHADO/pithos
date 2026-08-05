//! Candidate Cost Calculation and Global Planner

use pithos_core::{CompressionProfile, PithosError, Result};

pub const MIB: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SolidGroupPlan {
    pub first_item: usize,
    pub item_count: usize,
    pub uncompressed_len: u64,
}

impl SolidGroupPlan {
    pub const fn new(first_item: usize, item_count: usize, uncompressed_len: u64) -> Self {
        Self {
            first_item,
            item_count,
            uncompressed_len,
        }
    }
}

pub const fn solid_group_target(profile: CompressionProfile) -> u64 {
    match profile {
        CompressionProfile::Raw => 0,
        CompressionProfile::Stream => 4 * MIB,
        CompressionProfile::Random => 8 * MIB,
        CompressionProfile::Balanced => 64 * MIB,
        CompressionProfile::ArchiveMax => 512 * MIB,
    }
}

pub fn plan_solid_groups(
    profile: CompressionProfile,
    lengths: &[u64],
) -> Result<Vec<SolidGroupPlan>> {
    if profile == CompressionProfile::Raw {
        return Ok(lengths
            .iter()
            .enumerate()
            .filter_map(|(index, length)| {
                (*length != 0).then_some(SolidGroupPlan::new(index, 1, *length))
            })
            .collect());
    }

    let target = solid_group_target(profile);
    let mut groups = Vec::new();
    let mut current: Option<SolidGroupPlan> = None;
    for (index, length) in lengths.iter().copied().enumerate() {
        if length == 0 {
            continue;
        }
        if let Some(mut group) = current.take() {
            let combined = group
                .uncompressed_len
                .checked_add(length)
                .ok_or(PithosError::IntegerOverflow)?;
            if combined <= target {
                group.item_count = group
                    .item_count
                    .checked_add(1)
                    .ok_or(PithosError::IntegerOverflow)?;
                group.uncompressed_len = combined;
                current = Some(group);
            } else {
                groups.push(group);
                current = Some(SolidGroupPlan::new(index, 1, length));
            }
        } else {
            current = Some(SolidGroupPlan::new(index, 1, length));
        }
    }
    if let Some(group) = current {
        groups.push(group);
    }
    Ok(groups)
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CandidateCost {
    pub payload: u64,
    pub codec_descriptor: u64,
    pub group_descriptor: u64,
    pub index_delta: u64,
    pub integrity: u64,
    pub padding: u64,
}

impl CandidateCost {
    pub fn total(&self) -> Result<u64> {
        [
            self.payload,
            self.codec_descriptor,
            self.group_descriptor,
            self.index_delta,
            self.integrity,
            self.padding,
        ]
        .into_iter()
        .try_fold(0_u64, |total, value| {
            total.checked_add(value).ok_or(PithosError::IntegerOverflow)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pithos_core::{CompressionProfile, PithosError};

    #[test]
    fn profile_targets_match_the_phase_two_contract() {
        assert_eq!(solid_group_target(CompressionProfile::Stream), 4 * MIB);
        assert_eq!(solid_group_target(CompressionProfile::Random), 8 * MIB);
        assert_eq!(solid_group_target(CompressionProfile::Balanced), 64 * MIB);
        assert_eq!(
            solid_group_target(CompressionProfile::ArchiveMax),
            512 * MIB
        );
    }

    #[test]
    fn groups_preserve_logical_order_and_do_not_split_large_items() {
        let lengths = [2 * MIB, 2 * MIB, 1, 5 * MIB, 1];
        let groups = plan_solid_groups(CompressionProfile::Stream, &lengths).unwrap();
        assert_eq!(
            groups,
            vec![
                SolidGroupPlan::new(0, 2, 4 * MIB),
                SolidGroupPlan::new(2, 1, 1),
                SolidGroupPlan::new(3, 1, 5 * MIB),
                SolidGroupPlan::new(4, 1, 1),
            ]
        );
    }

    #[test]
    fn raw_profile_keeps_every_item_independent_including_empty_files() {
        let groups = plan_solid_groups(CompressionProfile::Raw, &[3, 0, 5]).unwrap();
        assert_eq!(
            groups,
            vec![
                SolidGroupPlan::new(0, 1, 3),
                SolidGroupPlan::new(1, 1, 0),
                SolidGroupPlan::new(2, 1, 5),
            ]
        );
    }

    #[test]
    fn solid_groups_keep_empty_items_in_their_logical_position() {
        assert_eq!(
            plan_solid_groups(CompressionProfile::Stream, &[3, 0, 5]).unwrap(),
            vec![SolidGroupPlan::new(0, 3, 8)]
        );
    }

    #[test]
    fn planning_and_candidate_cost_reject_integer_overflow() {
        assert!(matches!(
            plan_solid_groups(CompressionProfile::Balanced, &[u64::MAX, 1]),
            Err(PithosError::IntegerOverflow)
        ));
        let cost = CandidateCost {
            payload: u64::MAX,
            codec_descriptor: 1,
            ..CandidateCost::default()
        };
        assert!(matches!(cost.total(), Err(PithosError::IntegerOverflow)));
    }
}
