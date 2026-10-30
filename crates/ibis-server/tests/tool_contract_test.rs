//! Contract gate:
//! all 56 tool names present in the registry, the back-compat memory tools
//! registered, export/import describe the runtime transfer, and the legacy
//! `redmine_*` names forbidden. Data-driven against the D3 registry
//! (name + description + schema) instead of grepping a source file.

use serde_json::{json, Value};
use ibis_server::registry::{schemas, spec_of, TOOL_ANNOTATIONS, TOOL_SPECS};

/// `TestToolContracts_TicketAndPRNamesPresent + the
/// authoritative list: every one of the 56 tools must be registered.
#[test]
fn tool_contracts_all_56_names_present() {
    let required: [&str; 56] = [
        // Project session & ingest (4)
        "init_project",
        "update_project_status",
        "sync_local_patch",
        "ingest_code",
        // Git credentials (1)
        "git_configure_credentials",
        // Memory (26)
        "provide_collaborative_memory",
        "save_reasoning_outcome",
        "memory_add",
        "memory_bootstrap_git_store",
        "memory_rebuild_git_notes",
        "as_of_project",
        "memory_search",
        "memory_expand",
        "memory_update",
        "memory_delete",
        "memory_history",
        "store_rule",
        "get_rules",
        "set_working_state",
        "get_working_state",
        "recent_failures",
        "why_changed",
        "load_session_context",
        "capture_observation",
        "memory_consolidate",
        "memory_share",
        "memory_export",
        "memory_import",
        "ingest_design_note",
        "memory_insights",
        "knowledge_diff",
        // Code search (3)
        "ask_project",
        "analyze_blast_radius",
        "find_dead_code",
        // Settings (3)
        "settings_get",
        "settings_apply",
        "settings_open_ui",
        // Optimization (1)
        "optimize_knowledge",
        // Ticketing (15)
        "ticket_get_capabilities",
        "ticket_search",
        "ticket_search_my",
        "ticket_get",
        "ticket_create",
        "ticket_update",
        "ticket_add_comment",
        "ticket_assign",
        "ticket_transition",
        "ticket_list_statuses",
        "ticket_search_users",
        "ticket_list_projects",
        "ticket_mark_resolved",
        "ticket_mark_closed",
        "ticket_reopen",
        // Repo PR (2)
        "repo_pr_create",
        "repo_pr_complete",
        // Logs (1)
        "analyze_logs",
    ];
    assert_eq!(required.len(), 56, "the authoritative list has 56 tools");
    assert_eq!(
        TOOL_SPECS.len(),
        56,
        "registry registers exactly 56 tools (found {})",
        TOOL_SPECS.len()
    );
    for name in required {
        assert!(
            spec_of(name).is_some(),
            "missing MCP tool registration for {name}"
        );
    }
    // Registry names are unique.
    let mut names: Vec<&str> = TOOL_SPECS.iter().map(|s| s.name).collect();
    let deduped = names.len();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), deduped, "duplicate tool registration");
}

/// `TestToolContracts_BackCompatNamesUnchanged: the two legacy memory
/// tools stay registered while the typed memory model is wired.
#[test]
fn tool_contracts_back_compat_names_unchanged() {
    for name in ["provide_collaborative_memory", "save_reasoning_outcome"] {
        assert!(
            spec_of(name).is_some(),
            "back-compat tool {name} must stay registered"
        );
    }
}

/// `TestToolContracts_ExportImportDescribeRuntimeTransfer: the
/// export/import descriptions carry the plugin-isolated ↔ personal ↔ server
/// transfer semantics.
#[test]
fn tool_contracts_export_import_describe_runtime_transfer() {
    for name in ["memory_export", "memory_import"] {
        let spec = spec_of(name).unwrap_or_else(|| panic!("{name} registered"));
        for want in ["plugin-isolated", "personal", "server"] {
            assert!(
                spec.description.contains(want),
                "export/import tool surface missing {want:?} in {name} description: {}",
                spec.description
            );
        }
    }
}

/// Legacy `redmine_*` tools are forbidden on the MCP surface gate).
#[test]
fn tool_contracts_legacy_redmine_names_forbidden() {
    for legacy in [
        "redmine_search_issues",
        "redmine_update_issue",
        "redmine_create_issue",
        "redmine_get_issue",
    ] {
        assert!(
            spec_of(legacy).is_none(),
            "legacy redmine_* tools should not be registered"
        );
    }
    // No registered name starts with the legacy prefix at all.
    for spec in TOOL_SPECS {
        assert!(
            !spec.name.starts_with("redmine_"),
            "legacy redmine_* tools should not be registered: {}",
            spec.name
        );
    }
}

/// Every registration carries a description and an reference-shaped schema:
/// `{"type":"object","properties":{…}}` with per-property
/// `{"description": …, "type": …}` (D3 byte-match shape).
#[test]
fn tool_contracts_schema_shapes() {
    for spec in TOOL_SPECS {
        assert!(
            !spec.description.is_empty(),
            "{} has an empty description",
            spec.name
        );
        let schema = schemas().get(spec.name).expect("schema cached");
        assert_eq!(schema["type"], "object", "{}", spec.name);
        let properties = schema["properties"].as_object().expect("properties object");
        assert_eq!(
            properties.len(),
            spec.properties.len(),
            "{} property count",
            spec.name
        );
        for (name, description, kind) in spec.properties {
            let prop = properties
                .get(*name)
                .unwrap_or_else(|| panic!("{}.{} property", spec.name, name));
            assert_eq!(prop["description"], *description, "{}.{}", spec.name, name);
            assert_eq!(prop["type"], kind.as_str(), "{}.{}", spec.name, name);
        }
    }
}

/// D3 byte-match spot checks against the original reference marshaling: property
/// names and object keys serialize sorted, and `required` is the empty
/// array the reference library emits for every tool (live reference probe, NG-A01).
#[test]
fn tool_contracts_schema_wire_shapes() {
    let schema = schemas().get("init_project").expect("init_project");
    let serialized = serde_json::to_string(schema).expect("serialize");
    // Sorted property order (branch < commit < origin_url < project_name).
    assert!(
        serialized.contains(
            r#""properties":{"branch":{"description":"Target branch","type":"string"},"commit""#
        ),
        "sorted reference property order: {serialized}"
    );
    // Top-level keys sorted: properties < required < type.
    assert!(serialized.starts_with(r#"{"properties":"#), "{serialized}");
    assert!(
        serialized.ends_with(r#","required":[],"type":"object"}"#),
        "{serialized}"
    );
    let value: Value = serde_json::from_str(&serialized).expect("valid JSON");
    assert!(
        value
            .get("required")
            .and_then(Value::as_array)
            .map(|r| r.is_empty())
            .unwrap_or(false),
        "the reference MCP library emits an empty required array on every input_schema"
    );

    // Boolean and number property shapes (memory_add confidence / dry_run).
    let memory_add = schemas().get("memory_add").expect("memory_add");
    assert_eq!(memory_add["properties"]["confidence"]["type"], "number");
    let bootstrap = schemas()
        .get("memory_bootstrap_git_store")
        .expect("bootstrap");
    assert_eq!(bootstrap["properties"]["dry_run"]["type"], "boolean");

    // Tools with no arguments expose an empty properties object
    // (settings_get / ticket_get_capabilities).
    for name in [
        "settings_get",
        "ticket_get_capabilities",
        "settings_open_ui",
    ] {
        let schema = schemas().get(name).expect(name);
        assert_eq!(
            schema["properties"].as_object().map(|p| p.len()),
            Some(0),
            "{name} has no properties"
        );
    }
}

/// Progress emission is pinned to exactly the three long tools.
#[test]
fn tool_contracts_progress_tools_pinned() {
    for name in ["init_project", "ingest_code", "analyze_logs"] {
        assert!(
            ibis_server::progress::tool_emits_progress(name),
            "{name} emits progress"
        );
    }
    for name in ["memory_add", "ask_project", "ticket_search", "settings_get"] {
        assert!(
            !ibis_server::progress::tool_emits_progress(name),
            "{name} must not emit progress"
        );
    }
}

/// F0.2 completeness gate: every tool declares its annotations — a bijection
/// between [`TOOL_SPECS`] and [`TOOL_ANNOTATIONS`], a non-empty title per
/// row, and the read-only set pinned to exactly the pure-read tools.
#[test]
fn tool_annotations_complete_and_pinned() {
    // Bijection: every spec has exactly one annotation row, every row names
    // a registered tool.
    for spec in TOOL_SPECS {
        let rows: Vec<_> = TOOL_ANNOTATIONS
            .iter()
            .filter(|a| a.name == spec.name)
            .collect();
        assert_eq!(
            rows.len(),
            1,
            "{} must have exactly one annotation row",
            spec.name
        );
        assert!(
            !rows[0].title.is_empty(),
            "{} must carry a title",
            spec.name
        );
    }
    assert_eq!(
        TOOL_ANNOTATIONS.len(),
        TOOL_SPECS.len(),
        "one annotation row per registered tool"
    );
    for ann in TOOL_ANNOTATIONS {
        assert!(
            spec_of(ann.name).is_some(),
            "annotation row for unregistered tool {}",
            ann.name
        );
    }

    // The read-only set is exactly the pure-read surface; everything else
    // is an explicit mutator (readOnlyHint: false).
    let read_only: [&str; 22] = [
        // Code search & analysis
        "ask_project",
        "analyze_blast_radius",
        "find_dead_code",
        // Memory reads
        "memory_search",
        "memory_expand",
        "memory_history",
        "memory_insights",
        "why_changed",
        "as_of_project",
        "knowledge_diff",
        "get_working_state",
        "load_session_context",
        "recent_failures",
        "get_rules",
        // Settings read
        "settings_get",
        // Ticketing reads
        "ticket_get",
        "ticket_search",
        "ticket_search_my",
        "ticket_list_statuses",
        "ticket_list_projects",
        "ticket_search_users",
        "ticket_get_capabilities",
    ];
    for spec in TOOL_SPECS {
        let ann = TOOL_ANNOTATIONS
            .iter()
            .find(|a| a.name == spec.name)
            .expect("annotation row (bijection asserted above)");
        assert_eq!(
            ann.read_only,
            read_only.contains(&spec.name),
            "{} must declare readOnlyHint={} (F0.2 read-only split)",
            spec.name,
            read_only.contains(&spec.name)
        );
    }
}

/// F0.2 wire gate: the `Tool` objects the registry builds carry the title
/// (top-level and inside the annotations) and the explicit `readOnlyHint`
/// serialized camelCase as `readOnlyHint`.
#[test]
fn tool_annotations_reach_the_tool_objects() {
    for spec in TOOL_SPECS {
        let ann = ibis_server::registry::annotation_of(spec.name)
            .unwrap_or_else(|| panic!("{} annotation row", spec.name));
        let tool = ibis_server::registry::annotated_tool(spec);
        assert_eq!(
            tool.title.as_deref(),
            Some(ann.title),
            "{} title",
            spec.name
        );
        let annotations = tool.annotations.as_ref().expect("annotations present");
        assert_eq!(
            annotations.read_only_hint,
            Some(ann.read_only),
            "{} annotations.read_only_hint",
            spec.name
        );
        assert_eq!(
            annotations.title.as_deref(),
            Some(ann.title),
            "{} annotations.title",
            spec.name
        );

        // Wire shape: camelCase key, boolean value, title inside annotations.
        let wire = serde_json::to_value(&tool).expect("tool serializes");
        assert_eq!(
            wire["annotations"]["readOnlyHint"],
            json!(ann.read_only),
            "{} wire readOnlyHint",
            spec.name
        );
        assert_eq!(
            wire["annotations"]["title"],
            json!(ann.title),
            "{}",
            spec.name
        );
        assert_eq!(wire["title"], json!(ann.title), "{}", spec.name);
    }
}
