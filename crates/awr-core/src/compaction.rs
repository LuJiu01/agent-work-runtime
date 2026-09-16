use crate::{Error, Result, ensure_public_data};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionTrigger {
    Automatic,
    Manual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextMeasurementScope {
    /// Actual request footprint, including fixed instructions and tool definitions.
    FullRequest,
    HistoryOnly,
    Unknown,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextMeasurementBasis {
    HostReported,
    Estimated,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    /// Host-reported charge for this compaction, never an inferred session total.
    pub cost_usd: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionObservation {
    /// Stable native event identity, reused on delivery retries.
    pub compaction_id: String,
    /// Strictly increasing within the bound native session; gaps are allowed.
    pub sequence: u64,
    pub observed_at: i64,
    pub trigger: CompactionTrigger,
    pub model: String,
    /// Host telemetry field or event locator; not independent proof.
    pub source: String,
    pub measurement_scope: ContextMeasurementScope,
    pub measurement_basis: ContextMeasurementBasis,
    pub before_tokens: Option<u64>,
    pub after_tokens: Option<u64>,
    /// Effective host capacity, not the model's advertised maximum.
    pub context_window_tokens: Option<u64>,
    pub duration_ms: Option<u64>,
    pub usage: Option<CompactionUsage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionPolicy {
    /// Configurable heuristic, not a measured quality or cost break-even threshold.
    pub post_compaction_threshold_percent: u8,
}
impl Default for CompactionPolicy {
    fn default() -> Self {
        Self {
            post_compaction_threshold_percent: 50,
        }
    }
}
impl CompactionPolicy {
    pub fn validate(&self) -> Result<()> {
        if !(1..=100).contains(&self.post_compaction_threshold_percent) {
            return Err(Error::InvalidInput(
                "compaction threshold must be 1..100 percent".into(),
            ));
        }
        Ok(())
    }
}
impl CompactionObservation {
    pub fn validate(&self, now: i64) -> Result<()> {
        ensure_public_data(self)?;
        for (name, value, cap) in [
            ("compaction_id", &self.compaction_id, 256),
            ("model", &self.model, 128),
            ("source", &self.source, 512),
        ] {
            if value.trim().is_empty() || value.len() > cap || value.chars().any(char::is_control) {
                return Err(Error::InvalidInput(format!(
                    "{name} must be nonempty bounded text"
                )));
            }
        }
        if self.sequence == 0
            || self.sequence > i64::MAX as u64
            || self.observed_at <= 0
            || self.observed_at > now
        {
            return Err(Error::InvalidInput(
                "compaction requires a positive sequence and nonfuture observed_at".into(),
            ));
        }
        if self.context_window_tokens == Some(0)
            || [
                self.before_tokens,
                self.after_tokens,
                self.context_window_tokens,
            ]
            .into_iter()
            .flatten()
            .any(|n| n > 1_000_000_000)
        {
            return Err(Error::InvalidInput("context measurements must be at most 1B tokens and window must be positive; omit unknown values".into()));
        }
        if let Some(usage) = &self.usage {
            if usage.cost_usd.is_some_and(|n| !n.is_finite() || n < 0.0)
                || matches!((usage.cached_input_tokens, usage.input_tokens), (Some(c), Some(i)) if c > i)
            {
                return Err(Error::InvalidInput(
                    "invalid compaction usage or cost".into(),
                ));
            }
        }
        Ok(())
    }
    /// Never divide cumulative billing tokens, summary length, or unknown scope.
    pub fn occupancy_percent(&self) -> Option<f64> {
        if self.measurement_scope != ContextMeasurementScope::FullRequest
            || self.measurement_basis != ContextMeasurementBasis::HostReported
        {
            return None;
        }
        let (after, window) = (self.after_tokens?, self.context_window_tokens?);
        (window > 0).then(|| after as f64 * 100.0 / window as f64)
    }
}
