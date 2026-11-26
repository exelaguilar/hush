//! Repair results shared by the unelevated client and the privileged helper.

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RepairReport {
    pub generation: u64,
    pub operation: String,
    pub time_unix: u64,
    pub result: String,
    pub message: String,
    pub endpoint: String,
}

impl RepairReport {
    pub fn succeeded(&self) -> bool {
        matches!(self.result.as_str(), "ok" | "repaired" | "restarted")
    }

    pub fn matches(&self, generation: u64, operation: &str) -> bool {
        self.generation > generation && self.operation == operation
    }

    pub fn render(&self) -> String {
        let one_line = |text: &str| text.replace(['\r', '\n'], " ");
        format!("generation = {}\noperation = {}\ntime_unix = {}\nresult = {}\nmessage = {}\nendpoint = {}\n",
            self.generation, one_line(&self.operation), self.time_unix,
            one_line(&self.result), one_line(&self.message), one_line(&self.endpoint))
    }

    pub fn parse(text: &str) -> Option<Self> {
        let mut report = Self::default();
        for (key, value) in text.lines().filter_map(|line| line.split_once('=')) {
            let value = value.trim();
            match key.trim() {
                "generation" => report.generation = value.parse().ok()?,
                "operation" => report.operation = value.into(),
                "time_unix" => report.time_unix = value.parse().ok()?,
                "result" => report.result = value.into(),
                "message" => report.message = value.into(),
                "endpoint" => report.endpoint = value.into(),
                _ => {}
            }
        }
        (report.time_unix > 0 && !report.result.is_empty()).then_some(report)
    }

    pub fn read() -> Option<Self> {
        use crate::machine_state::{self, REPAIR_REPORT};
        Self::parse(&machine_state::read(REPAIR_REPORT).ok()?)
    }
}

#[cfg(test)]
mod tests {
    use super::RepairReport;

    #[test]
    fn rejects_old_or_unrelated_results_even_in_the_same_second() {
        let report = RepairReport {
            generation: 13,
            operation: "repair".into(),
            time_unix: 123,
            result: "waiting".into(),
            ..RepairReport::default()
        };
        assert!(!report.matches(13, "repair"));
        assert!(!report.matches(12, "restart"));
        assert!(report.matches(12, "repair"));
        assert!(!report.succeeded());
        assert_eq!(RepairReport::parse(&report.render()), Some(report));
    }
}
