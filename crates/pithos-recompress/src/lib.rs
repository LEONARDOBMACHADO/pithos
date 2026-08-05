//! Structural Scanners and Exact Recompression Layer

pub trait StructuralScanner: Send + Sync {
    fn probe(&self, prefix: &[u8]) -> bool;
}
