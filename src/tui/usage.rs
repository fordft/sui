//! Presentation totals retain missing measurements independently for every field.

#[derive(Debug, Default)]
pub struct UsageCount {
    pub total: u64,
    pub known_requests: u64,
}

impl UsageCount {
    fn add(&mut self, value: Option<u64>) {
        if let Some(value) = value {
            self.total = self.total.saturating_add(value);
            self.known_requests += 1;
        }
    }

    pub fn display(&self, requests: u64) -> String {
        if self.known_requests == 0 {
            "—".into()
        } else if self.known_requests < requests {
            format!("{}+ (partial)", self.total)
        } else {
            self.total.to_string()
        }
    }
}

#[derive(Debug, Default)]
pub struct UsageTotals {
    pub requests: u64,
    pub input: UsageCount,
    pub cache_read: UsageCount,
    pub cache_write: UsageCount,
    pub output: UsageCount,
    pub cache: CacheFraction,
    pub first: CacheFraction,
    pub subsequent: CacheFraction,
    pub last_request: Option<crate::events::RequestDetails>,
    pub changes: Vec<&'static str>,
}

#[derive(Debug, Default)]
pub struct CacheFraction {
    pub requests: u64,
    pub measured: u64,
    input: u128,
    cached: u128,
}
impl CacheFraction {
    fn add(&mut self, usage: &crate::types::Usage) {
        self.requests += 1;
        if usage.complete && !usage.estimated {
            if let (Some(input), Some(cached)) = (usage.input_tokens, usage.cache_read_tokens) {
                if cached <= input {
                    self.measured += 1;
                    self.input += input as u128;
                    self.cached += cached as u128;
                }
            }
        }
    }
    pub fn percent(&self) -> Option<f64> {
        (self.input > 0).then(|| self.cached as f64 * 100.0 / self.input as f64)
    }
    pub fn display(&self) -> String {
        let value = self
            .percent()
            .map(|v| format!("{v:.2}%"))
            .unwrap_or_else(|| "—".into());
        format!(
            "{value} · measured {}/{} requests",
            self.measured, self.requests
        )
    }
}

impl UsageTotals {
    pub fn add(
        &mut self,
        input: Option<u64>,
        cached: Option<u64>,
        written: Option<u64>,
        output: Option<u64>,
    ) {
        self.add_request(
            &crate::types::Usage {
                input_tokens: input,
                cache_read_tokens: cached,
                cache_write_tokens: written,
                output_tokens: output,
                complete: true,
                estimated: false,
            },
            None,
            None,
        );
    }

    pub fn add_request(
        &mut self,
        usage: &crate::types::Usage,
        request: Option<&crate::events::RequestDetails>,
        previous: Option<&crate::events::RequestDetails>,
    ) {
        self.requests += 1;
        if !usage.estimated {
            self.input.add(usage.input_tokens);
            self.cache_read.add(usage.cache_read_tokens);
            self.cache_write.add(usage.cache_write_tokens);
            self.output.add(usage.output_tokens);
        }
        self.cache.add(usage);
        if let Some(request) = request {
            self.changes.clear();
            let continuing = previous.is_some_and(|p| p.session_id == request.session_id);
            if let Some(prev) = previous {
                if !continuing {
                    self.changes.push("session");
                }
                for (changed, label) in [
                    (prev.requested_model != request.requested_model, "model"),
                    (prev.epoch_id != request.epoch_id, "epoch"),
                    (
                        prev.static_prefix_hash != request.static_prefix_hash,
                        "system prefix",
                    ),
                    (prev.tool_schema_hash != request.tool_schema_hash, "tools"),
                    (
                        prev.guidance_hash != request.guidance_hash,
                        "project guidance",
                    ),
                    (
                        prev.cache_key_fingerprint != request.cache_key_fingerprint,
                        "cache key",
                    ),
                ] {
                    if changed {
                        self.changes.push(label);
                    }
                }
            }
            if continuing
                && previous.is_some_and(|p| {
                    p.epoch_id == request.epoch_id && p.requested_model == request.requested_model
                })
            {
                self.subsequent.add(usage);
            } else {
                self.first.add(usage);
            }
            self.last_request = Some(request.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_zero_and_partial_measurements_stay_distinct() {
        let mut totals = UsageTotals::default();
        totals.add(Some(100), None, Some(0), None);
        assert_eq!(totals.input.display(totals.requests), "100");
        assert_eq!(totals.cache_read.display(totals.requests), "—");
        assert_eq!(totals.cache_write.display(totals.requests), "0");
        assert_eq!(totals.output.display(totals.requests), "—");
        totals.add(Some(20), Some(5), None, None);
        assert_eq!(totals.input.display(totals.requests), "120");
        assert_eq!(totals.cache_read.display(totals.requests), "5+ (partial)");
        assert_eq!(totals.cache_write.display(totals.requests), "0+ (partial)");
        assert_eq!(totals.output.display(totals.requests), "—");
    }
}
