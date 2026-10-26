//! Redmine wire DTOs and search/update parameter validation
//!

use thiserror::Error;

/// One Redmine issue (simplified projection).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Issue {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub subject: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub status: NamedObj,
    #[serde(default)]
    pub tracker: NamedObj,
    #[serde(default)]
    pub author: NamedObj,
    #[serde(rename = "created_on", default)]
    pub created_on: String,
    #[serde(rename = "updated_on", default)]
    pub updated_on: String,
}

/// Id-name pair.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct NamedObj {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub name: String,
}

/// Redmine user.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct User {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub login: String,
    #[serde(default)]
    pub firstname: String,
    #[serde(default)]
    pub lastname: String,
    #[serde(default)]
    pub mail: String,
}

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct IssuesResponse {
    #[serde(default)]
    pub issues: Vec<Issue>,
    #[serde(rename = "total_count", default)]
    pub total_count: i64,
    #[serde(default)]
    pub offset: i64,
    #[serde(default)]
    pub limit: i64,
}

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct UsersResponse {
    #[serde(default)]
    pub users: Vec<User>,
    #[serde(rename = "total_count", default)]
    pub total_count: i64,
    #[serde(default)]
    pub offset: i64,
    #[serde(default)]
    pub limit: i64,
}

/// Search filters.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SearchIssuesParams {
    pub query: String,
    pub project_id: String,
    pub status_id: String,
    pub tracker_id: String,
    pub assigned_to_id: String,
    pub author_id: String,
    pub priority_id: String,
    pub updated_from: String,
    pub updated_to: String,
    pub limit: i64,
    pub offset: i64,
    pub sort: String,
}

/// Paginated search result.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SearchIssuesResult {
    pub issues: Vec<Issue>,
    pub total_count: i64,
    pub offset: i64,
    pub limit: i64,
}

/// Issue update fields; zero values are omitted.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UpdateIssueParams {
    pub notes: String,
    pub status_id: i64,
    pub priority_id: i64,
    pub assigned_to_id: i64,
    pub fixed_version_id: i64,
}

#[derive(Debug, Error)]
#[error("{0}")]
pub struct ValidationError(pub String);

/// Validates search parameters: limit/offset
/// bounds, the sort allowlist, and the date formats.
pub fn validate_search_params(params: &SearchIssuesParams) -> Result<(), ValidationError> {
    if params.limit < 0 {
        return Err(ValidationError("limit must be >= 0".to_string()));
    }
    if params.limit > 100 {
        return Err(ValidationError("limit must be <= 100".to_string()));
    }
    if params.offset < 0 {
        return Err(ValidationError("offset must be >= 0".to_string()));
    }

    let allowed_sort = [
        "",
        "updated_on:desc",
        "updated_on:asc",
        "priority:desc",
        "priority:asc",
        "id:desc",
        "id:asc",
        "created_on:desc",
        "created_on:asc",
    ];
    if !allowed_sort.contains(&params.sort.as_str()) {
        return Err(ValidationError(format!("invalid sort '{}'", params.sort)));
    }

    if !params.updated_from.is_empty() {
        validate_date(&params.updated_from)
            .map_err(|_| ValidationError("updated_from: must be YYYY-MM-DD or RFC3339".into()))?;
    }
    if !params.updated_to.is_empty() {
        validate_date(&params.updated_to)
            .map_err(|_| ValidationError("updated_to: must be YYYY-MM-DD or RFC3339".into()))?;
    }
    Ok(())
}

/// Accepts exactly the original layouts: `2006-01-02` (strictly zero-padded) or
/// RFC3339 (E-A14).
pub fn validate_date(value: &str) -> Result<(), ValidationError> {
    let date_only_ok = value.len() == 10
        && value.as_bytes()[4] == b'-'
        && value.as_bytes()[7] == b'-'
        && value.as_bytes()[..4].iter().all(u8::is_ascii_digit)
        && value.as_bytes()[5..7].iter().all(u8::is_ascii_digit)
        && value.as_bytes()[8..10].iter().all(u8::is_ascii_digit)
        && chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d").is_ok();
    if date_only_ok || chrono::DateTime::parse_from_rfc3339(value).is_ok() {
        return Ok(());
    }
    Err(ValidationError("must be YYYY-MM-DD or RFC3339".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_search_params_matrix() {
        let mut p = SearchIssuesParams {
            limit: 101,
            ..Default::default()
        };
        assert!(validate_search_params(&p).is_err(), "limit > 100");
        p.limit = 0;
        p.offset = -1;
        assert!(validate_search_params(&p).is_err(), "negative offset");
        p.offset = 0;
        p.sort = "bad:desc".into();
        assert!(validate_search_params(&p).is_err(), "invalid sort");
        p.sort = "updated_on:desc".into();
        p.updated_from = "2026/01/01".into();
        assert!(validate_search_params(&p).is_err(), "invalid date");
        p.updated_from = "2026-01-01".into();
        assert!(validate_search_params(&p).is_ok(), "valid params");
        p.limit = -5;
        assert!(validate_search_params(&p).is_err(), "negative limit");
    }

    #[test]
    fn validate_date_formats() {
        for ok in [
            "2026-01-02",
            "2026-02-28",
            "2026-01-02T15:04:05Z",
            "2026-01-02T15:04:05+01:00",
        ] {
            assert!(validate_date(ok).is_ok(), "{ok}");
        }
        for bad in [
            "2026/01/01",
            "2026-1-1",
            "2026-13-01",
            "2026-02-30",
            "01-02-2026",
            "2026-01-02T15:04:05",
            "",
        ] {
            assert!(validate_date(bad).is_err(), "{bad}");
        }
    }
}
