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
}

impl UsageTotals {
    pub fn add(
        &mut self,
        input: Option<u64>,
        cached: Option<u64>,
        written: Option<u64>,
        output: Option<u64>,
    ) {
        self.requests += 1;
        self.input.add(input);
        self.cache_read.add(cached);
        self.cache_write.add(written);
        self.output.add(output);
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
