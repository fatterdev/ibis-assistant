//! Temporal SQL predicate helpers.

/// `memoryCurrentnessSQL: "still valid now" — code-controlled literals
/// only, safe to interpolate (F-002 applies to content values, not constants).
pub(crate) fn memory_currentness_sql() -> String {
    "(status = 'current') AND (valid_until = NONE OR valid_until > time::now())".to_string()
}

/// `memoryStatusFilterIsAll: the explicit `all` marker (F4 §A.2) — a status
/// filter value meaning "no status constraint" (every status). Case- and
/// whitespace-tolerant, like the visibility `all` case.
pub(crate) fn memory_status_is_all(status: &str) -> bool {
    status.trim().eq_ignore_ascii_case("all")
}

/// `memoryStatusFilterSQL: empty/current → full currentness; explicit
/// non-current statuses match exactly (user-controlled value → bound var,
/// F-002). The `all` marker is handled by the callers (no status predicate
/// at all), not here — an exact match on the literal would silently return
/// zero rows.
pub(crate) fn memory_status_filter_sql(
    status: &str,
) -> (String, Vec<(String, ibis_db::value::Value)>) {
    let status = status.trim().to_lowercase();
    if status.is_empty() || status == "current" {
        return (memory_currentness_sql(), Vec::new());
    }
    (
        "status = $status_filter".to_string(),
        vec![(
            "status_filter".to_string(),
            ibis_db::value::Value::from_t(status),
        )],
    )
}

/// `askMemoryVisibilitySQL: without a trusted actor only team rows are
/// visible (never leak personal). The actor is user-controlled → bound var.
pub(crate) fn ask_memory_visibility_sql(
    trusted_actor: &str,
) -> (String, Vec<(String, ibis_db::value::Value)>) {
    let actor = trusted_actor.trim();
    if actor.is_empty() {
        return ("visibility = 'team'".to_string(), Vec::new());
    }
    (
        "(visibility = 'team' OR (visibility = 'personal' AND owner = $actor))".to_string(),
        vec![("actor".to_string(), ibis_db::value::Value::from_t(actor))],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn currentness_and_visibility_fragments() {
        assert_eq!(
            memory_currentness_sql(),
            "(status = 'current') AND (valid_until = NONE OR valid_until > time::now())"
        );
        let (sql, vars) = ask_memory_visibility_sql("");
        assert_eq!(sql, "visibility = 'team'");
        assert!(vars.is_empty());
        let (sql, vars) = ask_memory_visibility_sql("alice");
        assert!(sql.contains("owner = $actor"));
        assert_eq!(vars[0].0, "actor");
        assert_eq!(vars[0].1, ibis_db::value::Value::from_t("alice"));
    }

    #[test]
    fn status_filter_current_vs_explicit() {
        let (sql, vars) = memory_status_filter_sql("");
        assert_eq!(sql, memory_currentness_sql());
        assert!(vars.is_empty());
        let (sql, vars) = memory_status_filter_sql("Current");
        assert_eq!(sql, memory_currentness_sql());
        assert!(vars.is_empty());
        let (sql, vars) = memory_status_filter_sql("stale");
        assert_eq!(sql, "status = $status_filter");
        assert_eq!(vars[0].1, ibis_db::value::Value::from_t("stale"));
    }

    #[test]
    fn all_marker_is_the_no_constraint_value() {
        // F4 §A.2: the explicit empty/all marker means "every status" —
        // case- and whitespace-tolerant, never an exact-match value.
        assert!(memory_status_is_all("all"));
        assert!(memory_status_is_all(" ALL "));
        assert!(memory_status_is_all("All"));
        assert!(!memory_status_is_all(""));
        assert!(!memory_status_is_all("current"));
        assert!(!memory_status_is_all("stale"));
    }
}
