//! Env-global scenarios,
//! serialized into a single `#[test]` so env mutations never race the
//! parallel test threads (lane convention, like lane A's config_seq /
//! workspace_seq runners).
//!
//! The ast-grep sandbox demonstration uses the injected seam
//! ([`ibis_ingest_code::persist_validated_rule_with`]): when a real `sg`
//! binary is discoverable the sandbox run is exercised end to end, otherwise
//! the scenario degrades gracefully (D2 keeps sg an external process on
//! day 1 — tests must not require it).

use std::path::{Path, PathBuf};

use ibis_ingest_code::{ensure_language_glob, persist_validated_rule_with, RuleKind, SgSandbox};

/// Restores `IBIS_SG_ROOT` to its prior value when dropped.
struct EnvGuard {
    key: &'static str,
    prior: Option<String>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &Path) -> Self {
        let prior = std::env::var(key).ok();
        std::env::set_var(key, value);
        EnvGuard { key, prior }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.prior {
            Some(v) => std::env::set_var(self.key, v),
            None => std::env::remove_var(self.key),
        }
    }
}

fn seed_sg_root(name: &str, config: &str) -> PathBuf {
    let root = tempfile::tempdir().expect("tempdir").keep().join(name);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("sgconfig.yml"), config).unwrap();
    std::fs::create_dir_all(root.join("rules")).unwrap();
    root
}

#[test]
fn env_serialized_rule_synth_scenarios() {
    // --- TestEnsureLanguageGlobIdempotent ---
    let root = seed_sg_root("glob_root", "ruleDirs:\n  - rules\n");
    let _guard = EnvGuard::set("IBIS_SG_ROOT", &root);

    ensure_language_glob(".foobar", "html").expect("first insert");
    ensure_language_glob(".foobar", "html").expect("idempotent insert");
    let contents = std::fs::read_to_string(root.join("sgconfig.yml")).unwrap();
    assert!(contents.contains("*.foobar"), "glob missing: {contents}");
    assert_eq!(
        contents.matches("*.foobar").count(),
        1,
        "glob duplicated: {contents}"
    );

    // --- TestPersistGeneratedChunkRule (structural-only sandbox) ---
    std::env::set_var("IBIS_AST_RULE_STRUCTURAL_ONLY", "1");
    let yaml = "id: ai-generated-vue-script-chunk\nlanguage: html\nrule:\n  any:\n    - kind: script_element\n";
    let res = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(persist_validated_rule_with(
            yaml,
            "ai-generated-vue-script-chunk",
            "html",
            "",
            RuleKind::Chunk,
            SgSandbox::Discover,
        ))
        .expect("persist failed");
    assert!(res.validated, "unexpected: {res:?}");
    assert!(res.path.contains("ai-generated-"), "path: {}", res.path);
    assert!(res.rule_id.ends_with("-chunk"), "id={}", res.rule_id);
    // The rule file landed under the seeded root's rules/ dir.
    assert!(root
        .join("rules")
        .join(format!("{}.yml", res.rule_id))
        .is_file());
    std::env::remove_var("IBIS_AST_RULE_STRUCTURAL_ONLY");

    // --- Empty / oversized YAML rejections carry no partial result (
    // PersistValidatedRule early returns) ---
    let err = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(persist_validated_rule_with(
            "   \n\t ",
            "ai-generated-x-chunk",
            "html",
            "",
            RuleKind::Chunk,
            SgSandbox::Skip,
        ))
        .expect_err("empty yaml rejected");
    assert!(err.result.is_none(), "empty yaml carries no result");
    assert!(err.to_string().contains("empty yaml"));
    let big = format!("id: x\nlanguage: html\npattern: {}\n", "a".repeat(25 << 10));
    let err = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(persist_validated_rule_with(
            &big,
            "ai-generated-x-chunk",
            "html",
            "",
            RuleKind::Chunk,
            SgSandbox::Skip,
        ))
        .expect_err("oversized yaml rejected");
    assert!(err.result.is_none(), "oversized yaml carries no result");
    assert!(err.to_string().contains("yaml too large"));

    // --- Quarantine + partial result on structural failure ---
    let bad =
        "id: x-chunk\nlanguage: html\nrule:\n  any:\n    - kind: script_element\n!!python/exec\n";
    let err = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(persist_validated_rule_with(
            bad,
            "x-chunk",
            "html",
            "",
            RuleKind::Chunk,
            SgSandbox::Skip,
        ))
        .expect_err("dangerous yaml rejected");
    let partial = err.result.expect("partial result present");
    assert!(!partial.validated, "partial result not validated");
    assert!(
        partial.quarantine.contains("rules_quarantine"),
        "quarantine path: {}",
        partial.quarantine
    );
    assert!(
        Path::new(&partial.quarantine).is_file(),
        "quarantine file written: {}",
        partial.quarantine
    );

    // --- Sandbox validation through the injected seam ---
    // Discover an sg binary once; with one, a real sandbox run must accept a
    // correct chunk rule and reject a bogus sample; without one the scenario
    // skips gracefully.
    let locator = ibis_ingest_code::SgLocator::discover();
    if !locator.binary_available() {
        // No sg binary available: the seam must degrade without error.
        let res = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(persist_validated_rule_with(
                yaml,
                "ai-generated-vue-script-chunk",
                "html",
                "",
                RuleKind::Chunk,
                SgSandbox::Skip,
            ))
            .expect("structural-only path");
        assert!(res.validated);
        return;
    }

    let good = "id: ai-generated-sg-seam-chunk\nlanguage: javascript\nrule:\n  any:\n    - pattern: function $NAME() { $$$ }\n";
    let res = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(persist_validated_rule_with(
            good,
            "ai-generated-sg-seam-chunk",
            "javascript",
            "function hello() { world(); }",
            RuleKind::Chunk,
            SgSandbox::Binary(locator.binary.clone()),
        ))
        .expect("sandbox-validated rule");
    assert!(res.validated, "unexpected: {res:?}");

    // A rule whose sample cannot match is rejected by the sandbox.
    let mismatch = "id: ai-generated-sg-seam2-chunk\nlanguage: javascript\nrule:\n  any:\n    - pattern: class $NAME {}\n";
    let err = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(persist_validated_rule_with(
            mismatch,
            "ai-generated-sg-seam2-chunk",
            "javascript",
            "function hello() { world(); }",
            RuleKind::Chunk,
            SgSandbox::Binary(locator.binary.clone()),
        ))
        .expect_err("sandbox must reject non-matching sample");
    assert!(
        err.to_string().contains("sg produced no matches"),
        "unexpected error: {err}"
    );
}
