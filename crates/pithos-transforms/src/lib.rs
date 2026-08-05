//! Advanced Transforms (Delta, Graph, Enumerative, Residual, Grammar, Math, Synthetic)

pub enum RefToken {
    Literal {
        len: u32,
    },
    LocalCopy {
        distance: u32,
        len: u32,
    },
    ExactRef {
        chunk_id: u64,
    },
    BaseCopy {
        base_slot: u8,
        offset: u32,
        len: u32,
    },
    DictCopy {
        dict_id: u32,
        offset: u32,
        len: u32,
    },
    Rule {
        rule_id: u32,
    },
    Math {
        rule_id: u32,
        count: u32,
    },
    Run {
        byte: u8,
        len: u32,
    },
}
