// Counter/gauge names as pub const strings

pub const GENERATE_REQUESTS_TOTAL: &str = "videogen_generate_requests_total";
pub const GENERATE_DURATION_MS: &str = "videogen_generate_duration_ms";
pub const ANSUMAN_REQUESTS_TOTAL: &str = "videogen_ansuman_requests_total";
pub const VAST_SUBMIT_TOTAL: &str = "videogen_vast_submit_total";
pub const COMPLETION_CALLBACKS_TOTAL: &str = "videogen_completion_callbacks_total";
pub const COMPLETION_HMAC_FAILURES_TOTAL: &str = "videogen_completion_hmac_failures_total";
pub const CONTEXTS_BY_STATE: &str = "videogen_contexts_by_state";
pub const RECONCILIATION_ACTIONS_TOTAL: &str = "videogen_reconciliation_actions_total";
pub const DRAFT_CREATION_TOTAL: &str = "videogen_draft_creation_total";

/// Call at startup to register all metric descriptions with the recorder.
pub fn init_metrics() {
    metrics::describe_counter!(GENERATE_REQUESTS_TOTAL, "Total videogen generate requests");
    metrics::describe_histogram!(
        GENERATE_DURATION_MS,
        "Videogen generate request duration in ms"
    );
    metrics::describe_counter!(ANSUMAN_REQUESTS_TOTAL, "Total Ansuman moderation requests");
    metrics::describe_counter!(VAST_SUBMIT_TOTAL, "Total Vast submit attempts");
    metrics::describe_counter!(COMPLETION_CALLBACKS_TOTAL, "Total completion callbacks");
    metrics::describe_counter!(
        COMPLETION_HMAC_FAILURES_TOTAL,
        "Total completion HMAC failures"
    );
    metrics::describe_gauge!(CONTEXTS_BY_STATE, "Videogen contexts by state");
    metrics::describe_counter!(RECONCILIATION_ACTIONS_TOTAL, "Total reconciliation actions");
    metrics::describe_counter!(DRAFT_CREATION_TOTAL, "Total draft creation attempts");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_metric_names_defined() {
        assert!(!GENERATE_REQUESTS_TOTAL.is_empty());
        assert!(!GENERATE_DURATION_MS.is_empty());
        assert!(!ANSUMAN_REQUESTS_TOTAL.is_empty());
        assert!(!VAST_SUBMIT_TOTAL.is_empty());
        assert!(!COMPLETION_CALLBACKS_TOTAL.is_empty());
        assert!(!COMPLETION_HMAC_FAILURES_TOTAL.is_empty());
        assert!(!CONTEXTS_BY_STATE.is_empty());
        assert!(!RECONCILIATION_ACTIONS_TOTAL.is_empty());
        assert!(!DRAFT_CREATION_TOTAL.is_empty());
    }
}
