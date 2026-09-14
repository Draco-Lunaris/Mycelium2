//! Agent run traces (v1 parity): one JSON record per agent run — the
//! input, every tool invocation (tool, args, paths touched), the final
//! answer, duration, and outcome. Stored as raw FileRepo payloads under
//! `/.traces/` in the caller's scope (never concepts — the registry and
//! search index never see them), pruned to the newest MAX_TRACES.
//!
//! Tracing is telemetry: a save failure is logged and never fails the
//! run.

use serde::{Deserialize, Serialize};

/// One tool invocation in an agent run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceStep {
    pub seq: u32,
    pub tool: String,
    /// Human-readable argument (query text, concept path…).
    pub summary: String,
    /// Concept paths touched/returned by this step.
    #[serde(default)]
    pub paths: Vec<String>,
    /// True for write tools.
    #[serde(default)]
    pub write: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TraceOutcome {
    Success,
    Partial,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TraceKind {
    Query,
    Mutation,
    Chat,
}

/// One agent run's full record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentTrace {
    pub id: String,
    pub kind: TraceKind,
    /// The user input that started the run (truncated).
    pub input: String,
    pub started_at: String,
    pub duration_ms: u128,
    pub steps: Vec<TraceStep>,
    /// Final answer/summary (truncated).
    pub answer: String,
    /// Compact one-line notation of the traversal.
    pub notation: String,
    pub outcome: TraceOutcome,
}

/// Collects steps during one agent run. Thread one instance through the
/// loop; `finalize` produces the persistable record.
#[derive(Debug, Default)]
pub struct TraceRecorder {
    steps: Vec<TraceStep>,
    started: Option<std::time::Instant>,
}

impl TraceRecorder {
    pub fn new() -> Self {
        Self {
            steps: Vec::new(),
            started: Some(std::time::Instant::now()),
        }
    }

    pub fn record(&mut self, tool: &str, summary: &str, paths: Vec<String>, write: bool) {
        self.steps.push(TraceStep {
            seq: self.steps.len() as u32 + 1,
            tool: tool.to_string(),
            summary: truncate(summary, 300),
            paths,
            write,
        });
    }

    pub fn finalize(
        mut self,
        kind: TraceKind,
        input: &str,
        answer: &str,
        outcome: TraceOutcome,
    ) -> AgentTrace {
        let duration_ms = self
            .started
            .take()
            .map(|t| t.elapsed().as_millis())
            .unwrap_or(0);
        let notation = build_notation(&self.steps, outcome);
        AgentTrace {
            id: format!(
                "{}-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u32)
                    .unwrap_or(0),
                rand_suffix()
            ),
            kind,
            input: truncate(input, 300),
            started_at: chrono::Utc::now().to_rfc3339(),
            duration_ms,
            steps: std::mem::take(&mut self.steps),
            answer: truncate(answer, 500),
            notation,
            outcome,
        }
    }
}

/// Persist a trace row (v1 .traces/ parity, DB-backed so listing and
/// pruning are indexed). Telemetry: errors are logged, never
/// propagated. `scope_id` is the caller's scope id (`user:<uuid>`).
pub async fn save_trace(pool: &sqlx::SqlitePool, scope_id: &str, trace: &AgentTrace) {
    let body = match serde_json::to_string(trace) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "trace serialize failed");
            return;
        }
    };
    let kind = match trace.kind {
        TraceKind::Query => "query",
        TraceKind::Mutation => "mutation",
        TraceKind::Chat => "chat",
    };
    let outcome = match trace.outcome {
        TraceOutcome::Success => "success",
        TraceOutcome::Partial => "partial",
        TraceOutcome::Failed => "failed",
    };
    let res = sqlx::query(
        "INSERT INTO agent_traces (id, scope, kind, input, answer, notation, outcome, duration_ms, created_at, body)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&trace.id)
    .bind(scope_id)
    .bind(kind)
    .bind(&trace.input)
    .bind(&trace.answer)
    .bind(&trace.notation)
    .bind(outcome)
    .bind(trace.duration_ms as i64)
    .bind(&trace.started_at)
    .bind(&body)
    .execute(pool)
    .await;
    if let Err(e) = res {
        tracing::warn!(error = %e, "trace save failed");
        return;
    }
    // Prune: keep the newest MAX_TRACES per scope.
    let _ = sqlx::query(
        "DELETE FROM agent_traces WHERE scope = ? AND id NOT IN (
            SELECT id FROM agent_traces WHERE scope = ?
            ORDER BY created_at DESC, id DESC LIMIT 50
        )",
    )
    .bind(scope_id)
    .bind(scope_id)
    .execute(pool)
    .await;
}

/// List recent traces (newest first) for inspection.
pub async fn list_traces(pool: &sqlx::SqlitePool, scope_id: &str, limit: u32) -> Vec<AgentTrace> {
    let rows: Vec<(String,)> = match sqlx::query_as(
        "SELECT body FROM agent_traces WHERE scope = ? ORDER BY created_at DESC, id DESC LIMIT ?",
    )
    .bind(scope_id)
    .bind(limit as i64)
    .fetch_all(pool)
    .await
    {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    rows.into_iter()
        .filter_map(|(body,)| serde_json::from_str(&body).ok())
        .collect()
}

/// Compact single-line traversal notation, e.g.
/// `search "rate limit" (2) → read /apis/billing-api.md → ✓`
pub fn build_notation(steps: &[TraceStep], outcome: TraceOutcome) -> String {
    let parts: Vec<String> = steps
        .iter()
        .map(|s| match s.tool.as_str() {
            "search_knowledge" => format!(
                "search \"{}\" ({})",
                truncate(&s.summary, 30),
                s.paths.len()
            ),
            "read_concept" => format!(
                "read {}",
                short_path(s.paths.first().map(String::as_str).unwrap_or(&s.summary))
            ),
            "read_passage" => format!("passage {}", truncate(&s.summary, 40)),
            "list_directory" => "browse layout".to_string(),
            "lint_knowledge" => "lint graph".to_string(),
            "write_concept" => format!(
                "write {}",
                short_path(s.paths.first().map(String::as_str).unwrap_or(""))
            ),
            "patch_concept" => format!(
                "patch {}",
                short_path(s.paths.first().map(String::as_str).unwrap_or(""))
            ),
            "delete_concept" => format!(
                "delete {}",
                short_path(s.paths.first().map(String::as_str).unwrap_or(""))
            ),
            _ => s.tool.clone(),
        })
        .collect();
    let marker = match outcome {
        TraceOutcome::Success => "✓",
        TraceOutcome::Partial => "⚠ partial",
        TraceOutcome::Failed => "✗",
    };
    parts
        .into_iter()
        .chain(std::iter::once(marker.to_string()))
        .collect::<Vec<_>>()
        .join(" → ")
}

fn short_path(p: &str) -> String {
    p.rsplit('/').next().unwrap_or(p).to_string()
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() > n {
        let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
        out.push('…');
        out
    } else {
        s.to_string()
    }
}

/// Small random suffix so same-millisecond runs never collide. Uses the
/// process-global atomic counter — unique within a process, combined
/// with the millisecond timestamp for cross-restart uniqueness.
fn rand_suffix() -> String {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{n:04x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notation_renders_steps() {
        let steps = vec![
            TraceStep {
                seq: 1,
                tool: "search_knowledge".into(),
                summary: "rate limit".into(),
                paths: vec!["/a.md".into(), "/b.md".into()],
                write: false,
            },
            TraceStep {
                seq: 2,
                tool: "read_concept".into(),
                summary: "/apis/billing-api.md".into(),
                paths: vec!["/apis/billing-api.md".into()],
                write: false,
            },
        ];
        let n = build_notation(&steps, TraceOutcome::Success);
        assert!(n.contains("search \"rate limit\" (2)"));
        assert!(n.contains("read billing-api.md"));
        assert!(n.ends_with("✓"));
    }

    #[test]
    fn recorder_finalizes_with_duration() {
        let mut r = TraceRecorder::new();
        r.record("search_knowledge", "q", vec![], false);
        let t = r.finalize(TraceKind::Query, "input", "answer", TraceOutcome::Success);
        assert_eq!(t.steps.len(), 1);
        assert_eq!(t.input, "input");
        assert!(!t.id.is_empty());
    }
}
