//! Credential masking tests for the git runner.

use ibis_git::{AuthType, Credential, GitRunner};

fn runner_with_token() -> GitRunner {
    let cred = Credential {
        target: "github.com".to_string(),
        provider: "github".to_string(),
        auth_type: AuthType::token(),
        token: "ghp_superSecretToken12345".to_string(),
        username: String::new(),
        ssh_private_key: String::new(),
    };
    GitRunner::new(Some(cred))
}

#[test]
fn test_git_command_runner_mask_secrets() {
    let runner = runner_with_token();

    let input_url = b"Cloning into 'repo' from https://oauth2:ghp_superSecretToken12345@github.com/org/repo.git...";
    let masked_url = runner.mask_secrets(input_url);
    let masked_url = String::from_utf8_lossy(&masked_url);
    assert!(
        !masked_url.contains("ghp_superSecretToken12345"),
        "expected URL token to be masked, got: {masked_url}"
    );
    assert!(
        masked_url.contains("[REDACTED]"),
        "expected [REDACTED] in masked URL, got: {masked_url}"
    );

    let input_text = b"error: ghp_superSecretToken12345 occurred";
    let masked_text = runner.mask_secrets(input_text);
    let masked_text = String::from_utf8_lossy(&masked_text);
    assert!(
        !masked_text.contains("ghp_superSecretToken12345"),
        "expected token to be masked in text, got: {masked_text}"
    );
    assert!(
        masked_text.contains("[REDACTED]"),
        "expected [REDACTED] in masked text, got: {masked_text}"
    );
}

#[test]
fn test_git_command_runner_mask_secrets_anonymous_runner() {
    // Even without a credential, URL-embedded basic auth is redacted.
    let runner = GitRunner::anonymous();
    let input = b"from https://oauth2:ghp_superSecretToken12345@github.com/org/repo.git...";
    let masked_bytes = runner.mask_secrets(input);
    let masked = String::from_utf8_lossy(&masked_bytes);
    assert!(!masked.contains("ghp_superSecretToken12345"));
    assert!(masked.contains("[REDACTED]"));
}

#[tokio::test]
async fn test_git_command_runner_run_local_git() {
    let runner = GitRunner::anonymous();
    let tmp_dir = tempfile::tempdir().unwrap();
    let dir = tmp_dir.path().to_string_lossy().into_owned();

    let out = runner.run(&dir, &["version"]).await.unwrap();
    let out = String::from_utf8_lossy(&out);
    assert!(
        out.contains("git version"),
        "expected output to contain 'git version', got: {out}"
    );
}

#[tokio::test]
async fn test_git_command_runner_run_includes_output_in_error() {
    let runner = GitRunner::anonymous();
    let tmp_dir = tempfile::tempdir().unwrap();
    let dir = tmp_dir.path().to_string_lossy().into_owned();

    let err = runner
        .run(
            &dir,
            &["rev-parse", "--verify", "definitely-not-a-ref^{commit}"],
        )
        .await;
    let err = err.unwrap_err();
    assert!(err.to_string().starts_with("git command failed: "), "{err}");
    assert!(err.command_output().is_some(), "error should carry output");
}
