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
