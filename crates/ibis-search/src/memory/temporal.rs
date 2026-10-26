//! Temporal SQL predicate helpers.

/// `memoryCurrentnessSQL: "still valid now" — code-controlled literals
/// only, safe to interpolate (F-002 applies to content values, not constants).
pub(crate) fn memory_currentness_sql() -> String {
    "(status = 'current') AND (valid_until = NONE OR valid_until > time::now())".to_string()
}

/// `memoryStatusFilterSQL: empty/current → full currentness; explicit
/// non-current statuses match exactly (user-controlled value → bound var,
/// F-002).
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
}
