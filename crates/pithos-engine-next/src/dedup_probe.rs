use pithos_analysis::{ChunkOrigin, ChunkingConfig, chunk_fastcdc};
use pithos_core::{PithosError, Result};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DuplicateOpportunity {
    pub chunk_count: u64,
    pub duplicate_chunks: u64,
    pub gross_duplicate_bytes: u64,
}

pub(crate) fn estimate(
    input: &[u8],
    member_lengths: &[u64],
) -> Result<DuplicateOpportunity> {
    validate_members(input.len(), member_lengths)?;
    let config = ChunkingConfig::default();
    let mut seen = HashMap::<[u8; 32], Vec<(usize, usize)>>::new();
    let mut member_base = 0_u64;
    let mut chunk_count = 0_u64;
    let mut duplicate_chunks = 0_u64;
    let mut gross_duplicate_bytes = 0_u64;

    for (member_id, member_length) in member_lengths.iter().copied().enumerate() {
        let member_start =
            usize::try_from(member_base).map_err(|_| PithosError::IntegerOverflow)?;
        let member_len =
            usize::try_from(member_length).map_err(|_| PithosError::IntegerOverflow)?;
        let member_end = member_start
            .checked_add(member_len)
            .ok_or(PithosError::IntegerOverflow)?;
        let member = input
            .get(member_start..member_end)
            .ok_or(PithosError::InvalidRange)?;
        let drafts = chunk_fastcdc(
            member,
            ChunkOrigin {
                entry_id: member_id as u64,
                object_id: 0,
                base_offset: member_base,
            },
            &config,
        )?;
        for draft in drafts {
            chunk_count = chunk_count
                .checked_add(1)
                .ok_or(PithosError::IntegerOverflow)?;
            let start =
                usize::try_from(draft.logical_offset).map_err(|_| PithosError::IntegerOverflow)?;
            let len = draft.length as usize;
            let end = start.checked_add(len).ok_or(PithosError::IntegerOverflow)?;
            let bytes = input.get(start..end).ok_or(PithosError::InvalidRange)?;
            let hash = *blake3::hash(bytes).as_bytes();
            let duplicate = seen.get(&hash).is_some_and(|candidates| {
                candidates.iter().any(|(candidate_start, candidate_len)| {
                    *candidate_len == len
                        && input
                            .get(*candidate_start..candidate_start.saturating_add(*candidate_len))
                            .is_some_and(|candidate| candidate == bytes)
                })
            });
            if duplicate {
                duplicate_chunks = duplicate_chunks
                    .checked_add(1)
                    .ok_or(PithosError::IntegerOverflow)?;
                gross_duplicate_bytes = gross_duplicate_bytes
                    .checked_add(len as u64)
                    .ok_or(PithosError::IntegerOverflow)?;
            } else {
                seen.entry(hash).or_default().push((start, len));
            }
        }
        member_base = member_base
            .checked_add(member_length)
            .ok_or(PithosError::IntegerOverflow)?;
    }

    Ok(DuplicateOpportunity {
        chunk_count,
        duplicate_chunks,
        gross_duplicate_bytes,
    })
}

fn validate_members(input_len: usize, member_lengths: &[u64]) -> Result<()> {
    let total = member_lengths.iter().try_fold(0_u64, |total, length| {
        total
            .checked_add(*length)
            .ok_or(PithosError::IntegerOverflow)
    })?;
    if total != input_len as u64 || (member_lengths.is_empty() && input_len != 0) {
        return Err(PithosError::InvalidMetadata("dedup probe member boundaries"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_duplicate_members_without_encoding_payload() {
        let member = b"same-member".repeat(128 * 1024);
        let mut input = member.clone();
        input.extend_from_slice(&member);
        let opportunity = estimate(&input, &[member.len() as u64, member.len() as u64]).unwrap();
        assert!(opportunity.duplicate_chunks > 0);
        assert!(opportunity.gross_duplicate_bytes >= member.len() as u64);
    }
}
