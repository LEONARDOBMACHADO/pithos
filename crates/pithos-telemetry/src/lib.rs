//! Deterministic telemetry records for Pithos pack/unpack and benchmark runs.

use serde::{Deserialize, Serialize};
use std::io::{self, Write};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const TELEMETRY_SCHEMA_VERSION: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Pack,
    Unpack,
    Verify,
    Benchmark,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Scan,
    Hashing,
    Chunking,
    Fingerprinting,
    ExactDedup,
    Similarity,
    Clustering,
    Planning,
    Encoding,
    Writing,
    Verify,
    Commit,
    Decode,
    Restore,
    PackTotal,
    UnpackTotal,
    ExternalCompress,
    ExternalDecompress,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StageMetric {
    pub stage: Stage,
    pub elapsed_ns: u64,
    pub input_bytes: Option<u64>,
    pub output_bytes: Option<u64>,
    pub items: Option<u64>,
    pub note: Option<String>,
}

impl StageMetric {
    pub fn elapsed(&self) -> Duration {
        Duration::from_nanos(self.elapsed_ns)
    }

    pub fn saved_bytes(&self) -> Option<i128> {
        Some(i128::from(self.input_bytes?) - i128::from(self.output_bytes?))
    }

    pub fn savings_percent(&self) -> Option<f64> {
        let input = self.input_bytes?;
        let output = self.output_bytes?;
        if input == 0 {
            return Some(0.0);
        }
        Some((1.0 - (output as f64 / input as f64)) * 100.0)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunTelemetry {
    pub schema_version: u16,
    pub run_id: String,
    pub operation: Operation,
    pub profile: Option<String>,
    pub inputs: Vec<String>,
    pub output: Option<String>,
    pub original_bytes: Option<u64>,
    pub result_bytes: Option<u64>,
    pub elapsed_ns: u64,
    pub stages: Vec<StageMetric>,
}

impl RunTelemetry {
    pub fn compression_ratio(&self) -> Option<f64> {
        let original = self.original_bytes?;
        let result = self.result_bytes?;
        if original == 0 {
            return Some(1.0);
        }
        Some(result as f64 / original as f64)
    }

    pub fn savings_percent(&self) -> Option<f64> {
        Some((1.0 - self.compression_ratio()?) * 100.0)
    }

    pub fn stage_time_percent(&self, stage: Stage) -> Option<f64> {
        if self.elapsed_ns == 0 {
            return Some(0.0);
        }
        let elapsed = self
            .stages
            .iter()
            .filter(|metric| metric.stage == stage)
            .fold(0_u128, |total, metric| total + u128::from(metric.elapsed_ns));
        Some((elapsed as f64 / self.elapsed_ns as f64) * 100.0)
    }
}

#[derive(Debug)]
pub struct TelemetryCollector {
    run_id: String,
    operation: Operation,
    profile: Option<String>,
    inputs: Vec<String>,
    output: Option<String>,
    started: Instant,
    stages: Mutex<Vec<StageMetric>>,
}

impl TelemetryCollector {
    pub fn new(
        run_id: impl Into<String>,
        operation: Operation,
        profile: Option<String>,
        inputs: Vec<String>,
        output: Option<String>,
    ) -> Self {
        Self {
            run_id: run_id.into(),
            operation,
            profile,
            inputs,
            output,
            started: Instant::now(),
            stages: Mutex::new(Vec::new()),
        }
    }

    pub fn record(
        &self,
        stage: Stage,
        elapsed: Duration,
        input_bytes: Option<u64>,
        output_bytes: Option<u64>,
        items: Option<u64>,
        note: Option<String>,
    ) {
        let elapsed_ns = elapsed.as_nanos().min(u128::from(u64::MAX)) as u64;
        self.stages
            .lock()
            .expect("telemetry mutex poisoned")
            .push(StageMetric {
                stage,
                elapsed_ns,
                input_bytes,
                output_bytes,
                items,
                note,
            });
    }

    pub fn finish(self, original_bytes: Option<u64>, result_bytes: Option<u64>) -> RunTelemetry {
        let elapsed_ns = self.started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
        let mut stages = self
            .stages
            .into_inner()
            .expect("telemetry mutex poisoned");
        stages.sort_by_key(|metric| metric.stage);
        RunTelemetry {
            schema_version: TELEMETRY_SCHEMA_VERSION,
            run_id: self.run_id,
            operation: self.operation,
            profile: self.profile,
            inputs: self.inputs,
            output: self.output,
            original_bytes,
            result_bytes,
            elapsed_ns,
            stages,
        }
    }
}

pub fn write_jsonl<W: Write, T: Serialize>(writer: &mut W, value: &T) -> io::Result<()> {
    serde_json::to_writer(&mut *writer, value).map_err(io::Error::other)?;
    writer.write_all(b"\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn savings_and_stage_percentages_are_stable() {
        let run = RunTelemetry {
            schema_version: TELEMETRY_SCHEMA_VERSION,
            run_id: "r1".into(),
            operation: Operation::Pack,
            profile: Some("balanced".into()),
            inputs: vec!["a.bin".into()],
            output: Some("a.bin.pits".into()),
            original_bytes: Some(1_000),
            result_bytes: Some(625),
            elapsed_ns: 1_000,
            stages: vec![StageMetric {
                stage: Stage::Encoding,
                elapsed_ns: 250,
                input_bytes: Some(1_000),
                output_bytes: Some(600),
                items: Some(1),
                note: None,
            }],
        };
        assert_eq!(run.savings_percent(), Some(37.5));
        assert_eq!(run.stage_time_percent(Stage::Encoding), Some(25.0));
        assert_eq!(run.stages[0].saved_bytes(), Some(400));
        assert_eq!(run.stages[0].savings_percent(), Some(40.0));
    }

    #[test]
    fn jsonl_is_one_record_per_line() {
        let mut out = Vec::new();
        write_jsonl(&mut out, &serde_json::json!({"b": 2, "a": 1})).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.ends_with('\n'));
        assert_eq!(text.lines().count(), 1);
    }
}
