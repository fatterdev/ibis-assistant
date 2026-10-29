//! Ticket-arg helpers gate
//!: `buildTicketSearchParamsFromArgs` parsing/defaults,
//! `getIntArg` string coercion and `getStringSliceArg` comma splitting.

use threski_server::args::{
    build_ticket_search_params_from_args, derive_repository_from_origin, get_bool_arg, get_int_arg,
    get_string_slice_arg,
};
use serde_json::{json, Value};

fn args(pairs: &[(&str, Value)]) -> serde_json::Map<String, Value> {
    let mut map = serde_json::Map::new();
    for (k, v) in pairs {
        map.insert((*k).to_string(), v.clone());
    }
    map
}

/// `TestBuildTicketSearchParamsFromArgs_Valid.
#[test]
fn build_ticket_search_params_from_args_valid() {
    let a = args(&[
        ("provider", json!("jira")),
        ("query", json!("timeout")),
        ("project_key", json!("core")),
        ("status", json!("open")),
        ("limit", json!(20.0)),
        ("offset", json!(10.0)),
        ("sort", json!("updated DESC")),
    ]);
    let params = build_ticket_search_params_from_args(&a).expect("valid args");
    assert_eq!(params.provider, "jira");
    assert_eq!(params.query, "timeout");
    assert_eq!(params.limit, 20);
    assert_eq!(params.offset, 10);
}

/// `TestBuildTicketSearchParamsFromArgs_DefaultLimit.
#[test]
fn build_ticket_search_params_from_args_default_limit() {
    let params = build_ticket_search_params_from_args(&args(&[])).expect("no error");
    assert_eq!(params.limit, 20);
}

/// `TestGetIntArg_StringValue.
#[test]
fn get_int_arg_string_value() {
    let a = args(&[("limit", json!("15"))]);
    assert_eq!(get_int_arg(&a, "limit").expect("no error"), 15);
}

/// `TestGetStringSliceArg_CommaSeparated.
#[test]
fn get_string_slice_arg_comma_separated() {
    let a = args(&[("ticket_ids", json!("#1,ABC-2, AB#3"))]);
    let values = get_string_slice_arg(&a, "ticket_ids");
    assert_eq!(values.len(), 3, "expected 3 values, got {values:?}");
}

/// The stringly-typed PR booleans (L-A02: `"true"` strings across
/// the MCP surface) parse through `get_bool_arg`.
#[test]
fn repo_pr_boolean_args_accept_wire_strings() {
    let a = args(&[
        ("delete_source_branch", json!("true")),
        ("squash", json!("false")),
        ("auto_complete", json!("1")),
    ]);
    assert!(get_bool_arg(&a, "delete_source_branch"));
    assert!(!get_bool_arg(&a, "squash"));
    assert!(get_bool_arg(&a, "auto_complete"));
    assert!(!get_bool_arg(&a, "absent"));
}

/// `deriveRepositoryFromOrigin
#[test]
fn derive_repository_from_origin_shapes() {
    assert_eq!(
        derive_repository_from_origin("https://github.com/org/repo.git"),
        "repo"
    );
    assert_eq!(
        derive_repository_from_origin("https://dev.azure.com/org/project/_git/repo"),
        "repo"
    );
    assert_eq!(derive_repository_from_origin(""), "");
}
