//! Candidate Cost Calculation and Global Planner

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
    pub fn total(&self) -> u64 {
        self.payload
            + self.codec_descriptor
            + self.group_descriptor
            + self.index_delta
            + self.integrity
            + self.padding
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
    fn raw_profile_keeps_every_nonempty_item_independent() {
        let groups = plan_solid_groups(CompressionProfile::Raw, &[3, 0, 5]).unwrap();
        assert_eq!(
            groups,
            vec![SolidGroupPlan::new(0, 1, 3), SolidGroupPlan::new(2, 1, 5)]
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
