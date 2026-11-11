//! G20 — `harness/profiles.json` is the single source of the host wiring
//! matrix. These tests fail when a real manifest drifts from the descriptor
//! (or when a surface is added/removed without updating the committed file).

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize)]
struct Profiles {
    version: u32,
    surfaces: Vec<Surface>,
}

#[derive(Debug, Deserialize)]
struct Surface {
    id: String,
    connection: Connection,
    actor: String,
    #[serde(default)]
    manifest: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Connection {
    default: String,
    #[serde(default)]
    attach_url: Option<String>,
    #[serde(default)]
    stdio: Option<Stdio>,
    #[serde(default)]
    bridge: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Stdio {
    command: String,
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/ibis-server sits at <root>/crates/ibis-server")
        .to_path_buf()
}

fn load_profiles() -> Profiles {
    let path = workspace_root().join("harness/profiles.json");
    let text = std::fs::read_to_string(&path).expect("harness/profiles.json");
    serde_json::from_str(&text).expect("valid harness/profiles.json")
}

fn surface<'a>(profiles: &'a Profiles, id: &str) -> &'a Surface {
    profiles
        .surfaces
        .iter()
        .find(|surface| surface.id == id)
        .unwrap_or_else(|| panic!("surface '{id}' missing from harness/profiles.json"))
}

fn manifest(path: &str) -> Value {
    let text = std::fs::read_to_string(workspace_root().join(path))
        .unwrap_or_else(|err| panic!("read {path}: {err}"));
    serde_json::from_str(&text).unwrap_or_else(|err| panic!("parse {path}: {err}"))
}

#[test]
fn profiles_json_is_valid() {
    let profiles = load_profiles();
    assert_eq!(profiles.version, 1, "descriptor version");
    assert_eq!(profiles.surfaces.len(), 5, "surface count");

    let mut ids = BTreeSet::new();
    for surface in &profiles.surfaces {
        assert!(!surface.id.is_empty(), "empty surface id");
        assert!(
            ids.insert(surface.id.as_str()),
            "duplicate id {}",
            surface.id
        );
        assert!(!surface.actor.is_empty(), "{} actor", surface.id);
        assert!(
            !surface.connection.default.is_empty(),
            "{} connection default",
            surface.id
        );
        match surface.connection.default.as_str() {
            "stdio" => assert!(
                surface.connection.stdio.is_some(),
                "{} stdio block",
                surface.id
            ),
            "auto" => assert!(
                surface.connection.attach_url.is_some(),
                "{} attach_url",
                surface.id
            ),
            "bridge" => assert!(surface.connection.bridge.is_some(), "{} bridge", surface.id),
            _ => {}
        }
        if let Some(manifest) = &surface.manifest {
            assert!(
                workspace_root().join(manifest).is_file(),
                "{} manifest {} exists",
                surface.id,
                manifest
            );
        }
    }
}

#[test]
fn every_expected_surface_is_declared() {
    let profiles = load_profiles();
    let declared: BTreeSet<&str> = profiles.surfaces.iter().map(|s| s.id.as_str()).collect();
    let expected: BTreeSet<&str> = [
        "vscode",
        "cursor",
        "copilot-plugin",
        "claude-desktop",
        "cli",
    ]
    .into_iter()
    .collect();
    assert_eq!(declared, expected);
}

#[test]
fn plugin_manifest_matches() {
    let profiles = load_profiles();
    let expected = surface(&profiles, "copilot-plugin");
    let stdio = expected.connection.stdio.as_ref().expect("stdio block");
    let doc = manifest(expected.manifest.as_deref().expect("manifest path"));
    let server = &doc["mcpServers"]["ibis"];
    assert_eq!(server["type"], "stdio");
    assert_eq!(server["command"], stdio.command);
    assert_eq!(server["args"], serde_json::json!(stdio.args));
    assert_eq!(server["env"], serde_json::json!(stdio.env));
}

#[test]
fn cursor_manifest_matches() {
    let profiles = load_profiles();
    let expected = surface(&profiles, "cursor");
    let stdio = expected.connection.stdio.as_ref().expect("stdio block");
    let doc = manifest(expected.manifest.as_deref().expect("manifest path"));
    let server = &doc["mcpServers"]["ibis"];
    assert!(
        server.get("type").is_none(),
        "cursor manifest must not set type: {server}"
    );
    assert_eq!(server["command"], stdio.command);
    assert_eq!(server["args"], serde_json::json!(stdio.args));
    assert_eq!(server["env"], serde_json::json!(stdio.env));
}

#[test]
fn vscode_defaults_match() {
    let profiles = load_profiles();
    let expected = surface(&profiles, "vscode");
    assert_eq!(expected.connection.default, "auto");
    let attach_url = expected
        .connection
        .attach_url
        .as_deref()
        .expect("vscode attach_url");
    let doc = manifest(expected.manifest.as_deref().expect("manifest path"));
    let properties = &doc["contributes"]["configuration"]["properties"];
    assert_eq!(properties["ibis.runtime.mode"]["default"], "auto");
    assert_eq!(properties["ibis.runtime.mcpUrl"]["default"], attach_url);

    let modes: BTreeSet<&str> = properties["ibis.runtime.mode"]["enum"]
        .as_array()
        .expect("runtime.mode enum")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(
        modes,
        ["auto", "spawn", "attach"].into_iter().collect(),
        "runtime.mode enum"
    );
}
