//! gruntas_status, gruntas_changes and gruntas_ping (GA03, R-GOD-2,
//! R-GOD-3, R-GOD-5): the Status layer's tree from every board, compact and
//! paged; each machine's changes through a `changes` host command, the
//! silent ones listed as unknown; the Status layer's ping.

use super::super::Outcome;
use serde_json::Value;

/// Rows per page of gruntas_status (D1).
pub(super) const ROWS: usize = 200;

pub(super) fn status(_args: &Value) -> Outcome {
    super::not_yet("gruntas_status", "GA03")
}

pub(super) fn changes(_args: &Value) -> Outcome {
    super::not_yet("gruntas_changes", "GA03")
}

pub(super) fn ping(_args: &Value) -> Outcome {
    super::not_yet("gruntas_ping", "GA03")
}

#[cfg(test)]
mod tests {
    #[test]
    #[ignore = "GA03: status on board fixtures, filtered by project and paged beyond 200 rows"]
    fn ga03_status_filters_by_project_and_pages() {
        unimplemented!("GA03")
    }

    #[test]
    #[ignore = "GA03: changes on host-command fixtures, a silent machine listed as unknown"]
    fn ga03_changes_list_a_silent_machine_as_unknown() {
        unimplemented!("GA03")
    }
}
