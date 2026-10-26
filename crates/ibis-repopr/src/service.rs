//! The repo-PR service: provider resolution,
//! target-branch fallbacks, auto-built title/description and ticket-id
//! detection (Jira `ABC-12`, Azure DevOps `PROJ#10`, Redmine `#123`).
//!
//! Parity notes:
//! - read the PAT from the request context (`auth.AzureDevOpsPATContextKey`
//!   set by the HTTP auth middleware); the Rust service takes an explicit
//!   [`AuthContext`] and trims the PAT exactly like `authFromContext
//!   (redmine-lane precedent E-A13).
//! - The ticket regexes poriginally/RE2 semantics: ASCII `\b` word boundaries
//!   (`(?-u:\b)`) and ASCII `\d` → `[0-9]`, so exotic Unicode word chars or
//!   digits behave exactly like the original detector.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};

use regex::Regex;

use crate::error::RepoprError;
use crate::types::{
    AuthContext, CompleteParams, Config, CreateParams, Provider, ProviderName, PullRequest,
    PROVIDER_AZURE_DEVOPS,
};

/// `jiraPattern = \b[A-Z][A-Z0-9]+-\d+\b with ASCII boundaries/digits.
fn jira_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?-u:\b)[A-Z][A-Z0-9]+-[0-9]+(?-u:\b)").expect("jira ticket regex")
    })
}

/// `adoPattern = \b[A-Z][A-Z0-9]+#\d+\b with ASCII boundaries/digits.
fn ado_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?-u:\b)[A-Z][A-Z0-9]+#[0-9]+(?-u:\b)").expect("ado ticket regex")
    })
}

/// `redminePattern = (?:^|[\s,])(#\d+): `#N` must be preceded by start of
/// line/string, whitespace, `(` or `, ` — avoids false positives on URL anchors
/// and other trackers. `\s is ASCII (`[\t\n\f\r ]`), kept explicit.
fn redmine_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?:^|[\t\n\x0C\r ,(])(#[0-9]+)").expect("redmine ticket regex"))
}

/// Resolves providers and normalizes PR params before delegating.
pub struct Service {
    cfg: Config,
    providers: HashMap<ProviderName, Arc<dyn Provider>>,
}

impl Service {
    /// `NewService: empty default provider resolves to `azure_devops`;
    /// nil maps become empty maps.
    pub fn new(cfg: Config) -> Self {
        let mut cfg = cfg;
        if cfg.default_provider.is_empty() {
            cfg.default_provider = ProviderName::from(PROVIDER_AZURE_DEVOPS);
        }
        Service {
            cfg,
            providers: HashMap::new(),
        }
    }

    /// `RegisterProvider; a later registration with the same name replaces
    /// the earlier one (map assignment semantics). Registration is a
    /// construction-phase mutation (wire providers before sharing the service).
    pub fn register_provider(&mut self, provider: Arc<dyn Provider>) {
        self.providers.insert(provider.name(), provider);
    }

    /// Resolution order: explicit provider argument →
    /// project map → configured default.
    fn resolve_provider(
        &self,
        provider_arg: &str,
        project_name: &str,
    ) -> Result<(ProviderName, Arc<dyn Provider>), RepoprError> {
        let arg = provider_arg.trim();
        if !arg.is_empty() {
            let name = ProviderName::from(arg.to_lowercase());
            let provider = self
                .providers
                .get(&name)
                .ok_or_else(|| RepoprError::ProviderNotRegistered(name.to_string()))?;
            return Ok((name, Arc::clone(provider)));
        }
        if let Some(mapped) = self.cfg.project_provider_map.get(project_name) {
            if !mapped.is_empty() {
                let provider = self
                    .providers
                    .get(mapped)
                    .ok_or_else(|| RepoprError::ProviderNotRegistered(mapped.to_string()))?;
                return Ok((mapped.clone(), Arc::clone(provider)));
            }
        }
        let name = self.cfg.default_provider.clone();
        let provider = self
            .providers
            .get(&name)
            .ok_or_else(|| RepoprError::ProviderNotRegistered(name.to_string()))?;
        Ok((name, Arc::clone(provider)))
    }

    /// `authFromContext: trims the Azure DevOps PAT carried by the caller
    /// (HTTP middleware header `X-Azure-DevOps-PAT` in production).
    fn auth_from(&self, auth: &AuthContext) -> AuthContext {
        AuthContext {
            azure_devops_pat: auth.azure_devops_pat.trim().to_string(),
        }
    }

    /// Creates a pull request: target-branch fallbacks
    /// (explicit → project map → default → `main`), auto-built title and
    /// description, ticket-id auto-detection over
    /// `source_branch + "\n" + title + "\n" + description` merged after the
    /// caller-provided ids, provider name normalized onto the params.
    pub async fn create_pr(
        &self,
        auth: &AuthContext,
        mut params: CreateParams,
    ) -> Result<PullRequest, RepoprError> {
        let (name, provider) = self.resolve_provider(&params.provider, &params.project_name)?;
        if params.target_branch.trim().is_empty() {
            let project_target = self
                .cfg
                .project_target_branch
                .get(&params.project_name)
                .cloned();
            if let Some(target) = project_target.filter(|t| !t.trim().is_empty()) {
                params.target_branch = target;
            } else if !self.cfg.default_target_branch.trim().is_empty() {
                params.target_branch = self.cfg.default_target_branch.clone();
            } else {
                params.target_branch = "main".to_string();
            }
        }
        if params.title.trim().is_empty() {
            params.title = build_pr_title(&params.source_branch, &params.ticket_ids);
        }
        if params.description.trim().is_empty() {
            params.description = build_pr_description(&params.source_branch, &params.ticket_ids);
        }

        let all_tickets = normalize_ticket_ids(&params.ticket_ids);
        let auto_detected = detect_ticket_ids(&format!(
            "{}\n{}\n{}",
            params.source_branch, params.title, params.description
        ));
        params.ticket_ids = merge_ticket_ids(&all_tickets, &auto_detected);
        params.provider = name.to_string();

        provider.create_pr(&self.auth_from(auth), &params).await
    }

    /// Completes/merges a pull request.
    pub async fn complete_pr(
        &self,
        auth: &AuthContext,
        mut params: CompleteParams,
    ) -> Result<PullRequest, RepoprError> {
        let (name, provider) = self.resolve_provider(&params.provider, &params.project_name)?;
        params.provider = name.to_string();
        provider.complete_pr(&self.auth_from(auth), &params).await
    }
}

/// `buildPRTitle: `<tickets> - <source_branch>` (auto-detected ids merged
/// in), or `PR from <source_branch>` when none.
fn build_pr_title(source_branch: &str, ticket_ids: &[String]) -> String {
    let tickets = merge_ticket_ids(
        &normalize_ticket_ids(ticket_ids),
        &detect_ticket_ids(source_branch),
    );
    if tickets.is_empty() {
        return format!("PR from {source_branch}");
    }
    format!("{} - {}", tickets.join(", "), source_branch)
}

/// `buildPRDescription: `Automated PR for branch <source_branch>` plus a
/// `Linked tickets: ...` block when ids are known.
fn build_pr_description(source_branch: &str, ticket_ids: &[String]) -> String {
    let tickets = merge_ticket_ids(
        &normalize_ticket_ids(ticket_ids),
        &detect_ticket_ids(source_branch),
    );
    if tickets.is_empty() {
        return format!("Automated PR for branch {source_branch}");
    }
    format!(
        "Automated PR for branch {source_branch}\n\nLinked tickets: {}",
        tickets.join(", ")
    )
}

/// Detects ticket references: Jira first, then Azure
/// DevOps, then Redmine (capturing `#N` only), deduped in first-seen order and
/// finally sorted byte-wise like `sort.Strings.
pub fn detect_ticket_ids(input: &str) -> Vec<String> {
    let jira = jira_pattern();
    let ado = ado_pattern();
    let redmine = redmine_pattern();

    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    let mut collect = |m: &str| {
        let m = m.trim();
        if !m.is_empty() && seen.insert(m.to_string()) {
            out.push(m.to_string());
        }
    };
    for m in jira.find_iter(input) {
        collect(m.as_str());
    }
    for m in ado.find_iter(input) {
        collect(m.as_str());
    }
    for caps in redmine.captures_iter(input) {
        if let Some(m) = caps.get(1) {
            collect(m.as_str());
        }
    }
    out.sort();
    out
}

/// Trims, drops empties and dedupes while preserving order.
pub(crate) fn normalize_ticket_ids(ids: &[String]) -> Vec<String> {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut out: Vec<String> = Vec::with_capacity(ids.len());
    for id in ids {
        let id = id.trim();
        if id.is_empty() || !seen.insert(id) {
            continue;
        }
        out.push(id.to_string());
    }
    out
}

/// Merges caller ids first, then extras, deduped order-preserving
///
pub(crate) fn merge_ticket_ids(current: &[String], extra: &[String]) -> Vec<String> {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for set in [current, extra] {
        for id in set {
            let id = id.as_str();
            if !seen.insert(id) {
                continue;
            }
            out.push(id.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Mutex;

    struct FakeProvider {
        name: ProviderName,
        calls: Mutex<FakeCalls>,
    }

    #[derive(Default)]
    struct FakeCalls {
        last_create: Option<CreateParams>,
        last_complete: Option<CompleteParams>,
        last_auth_pat: String,
    }

    impl FakeProvider {
        fn new(name: &str) -> Self {
            FakeProvider {
                name: ProviderName::from(name),
                calls: Mutex::new(FakeCalls::default()),
            }
        }
    }

    #[async_trait]
    impl Provider for FakeProvider {
        fn name(&self) -> ProviderName {
            self.name.clone()
        }

        async fn create_pr(
            &self,
            auth: &AuthContext,
            params: &CreateParams,
        ) -> Result<PullRequest, RepoprError> {
            let mut calls = self.calls.lock().unwrap();
            calls.last_create = Some(params.clone());
            calls.last_auth_pat = auth.azure_devops_pat.clone();
            Ok(PullRequest {
                provider: self.name.clone(),
                id: "10".into(),
                title: params.title.clone(),
                status: "active".into(),
                ..PullRequest::default()
            })
        }

        async fn complete_pr(
            &self,
            auth: &AuthContext,
            params: &CompleteParams,
        ) -> Result<PullRequest, RepoprError> {
            let mut calls = self.calls.lock().unwrap();
            calls.last_complete = Some(params.clone());
            calls.last_auth_pat = auth.azure_devops_pat.clone();
            Ok(PullRequest {
                provider: self.name.clone(),
                id: params.pr_id.clone(),
                status: "completed".into(),
                ..PullRequest::default()
            })
        }
    }

    // Scenario: TestCreatePRAppliesTargetBranchAndAutodetect.
    #[tokio::test]
    async fn create_pr_applies_target_branch_and_autodetect() {
        let mut svc = Service::new(Config {
            default_provider: ProviderName::from(PROVIDER_AZURE_DEVOPS),
            default_target_branch: "develop".into(),
            ..Config::default()
        });
        let provider = Arc::new(FakeProvider::new(PROVIDER_AZURE_DEVOPS));
        svc.register_provider(provider.clone());

        let res = svc
            .create_pr(
                &AuthContext::default(),
                CreateParams {
                    source_branch: "feature/ABC-123-fix".into(),
                    ..CreateParams::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(res.id, "10");

        let calls = provider.calls.lock().unwrap();
        let last = calls.last_create.as_ref().unwrap();
        assert_eq!(last.target_branch, "develop");
        assert!(
            !last.ticket_ids.is_empty() && last.ticket_ids[0] == "ABC-123",
            "expected autodetected ticket ABC-123, got {:?}",
            last.ticket_ids
        );
    }

    // Scenario: TestProviderPrecedence.
    #[tokio::test]
    async fn provider_precedence() {
        let mut svc = Service::new(Config {
            default_provider: ProviderName::from(PROVIDER_AZURE_DEVOPS),
            project_provider_map: HashMap::from([(
                "proj".to_string(),
                ProviderName::from(PROVIDER_AZURE_DEVOPS),
            )]),
            ..Config::default()
        });
        let provider = Arc::new(FakeProvider::new(PROVIDER_AZURE_DEVOPS));
        svc.register_provider(provider.clone());

        svc.create_pr(
            &AuthContext::default(),
            CreateParams {
                provider: "azure_devops".into(),
                project_name: "proj".into(),
                source_branch: "feature/x".into(),
                ..CreateParams::default()
            },
        )
        .await
        .unwrap();

        let calls = provider.calls.lock().unwrap();
        assert_eq!(calls.last_create.as_ref().unwrap().provider, "azure_devops");
    }

    // Extra (REP-A06): exact error text for unregistered providers.
    #[tokio::test]
    async fn unregistered_provider_error() {
        let svc = Service::new(Config::default());
        let err = svc
            .create_pr(
                &AuthContext::default(),
                CreateParams {
                    provider: "gitlab".into(),
                    source_branch: "feature/x".into(),
                    ..CreateParams::default()
                },
            )
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "repo provider 'gitlab' not registered");
    }

    // Extra: explicit argument beats project map beats default; unregistered
    // map values fail with the mapped name.
    #[tokio::test]
    async fn provider_resolution_matrix() {
        let azure = Arc::new(FakeProvider::new(PROVIDER_AZURE_DEVOPS));
        let jira = Arc::new(FakeProvider::new("jira"));
        let mut svc = Service::new(Config {
            default_provider: ProviderName::from(PROVIDER_AZURE_DEVOPS),
            project_provider_map: HashMap::from([
                ("proj".to_string(), ProviderName::from("jira")),
                ("broken".to_string(), ProviderName::from("gitlab")),
            ]),
            ..Config::default()
        });
        svc.register_provider(azure.clone());
        svc.register_provider(jira.clone());

        // project map wins over default
        svc.create_pr(
            &AuthContext::default(),
            CreateParams {
                project_name: "proj".into(),
                source_branch: "b".into(),
                ..CreateParams::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            jira.calls
                .lock()
                .unwrap()
                .last_create
                .as_ref()
                .unwrap()
                .provider,
            "jira"
        );

        // explicit argument wins over project map
        svc.create_pr(
            &AuthContext::default(),
            CreateParams {
                provider: "azure_devops".into(),
                project_name: "proj".into(),
                source_branch: "b".into(),
                ..CreateParams::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            azure
                .calls
                .lock()
                .unwrap()
                .last_create
                .as_ref()
                .unwrap()
                .provider,
            "azure_devops"
        );

        // unknown project falls back to the default provider
        svc.create_pr(
            &AuthContext::default(),
            CreateParams {
                project_name: "other".into(),
                source_branch: "b".into(),
                ..CreateParams::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            azure
                .calls
                .lock()
                .unwrap()
                .last_create
                .as_ref()
                .unwrap()
                .provider,
            "azure_devops"
        );

        // unregistered map value errors with the mapped name
        let err = svc
            .create_pr(
                &AuthContext::default(),
                CreateParams {
                    project_name: "broken".into(),
                    source_branch: "b".into(),
                    ..CreateParams::default()
                },
            )
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "repo provider 'gitlab' not registered");
    }

    // Extra: target-branch fallback matrix (explicit → project map → default
    // → "main"; blank project-map entries fall through like the trim check).
    #[tokio::test]
    async fn target_branch_fallback_matrix() {
        let provider = Arc::new(FakeProvider::new(PROVIDER_AZURE_DEVOPS));
        let mut svc = Service::new(Config {
            default_provider: ProviderName::from(PROVIDER_AZURE_DEVOPS),
            default_target_branch: "develop".into(),
            project_target_branch: HashMap::from([
                ("mapped".to_string(), "release/1".to_string()),
                ("blank".to_string(), "   ".to_string()),
            ]),
            ..Config::default()
        });
        svc.register_provider(provider.clone());

        async fn run(
            svc: &Service,
            provider: &FakeProvider,
            project: &str,
            target: &str,
        ) -> String {
            svc.create_pr(
                &AuthContext::default(),
                CreateParams {
                    project_name: project.into(),
                    source_branch: "b".into(),
                    target_branch: target.into(),
                    ..CreateParams::default()
                },
            )
            .await
            .unwrap();
            provider
                .calls
                .lock()
                .unwrap()
                .last_create
                .as_ref()
                .unwrap()
                .target_branch
                .clone()
        }

        assert_eq!(
            run(&svc, &provider, "any", "explicit/1").await,
            "explicit/1"
        );
        assert_eq!(run(&svc, &provider, "mapped", "").await, "release/1");
        assert_eq!(run(&svc, &provider, "blank", "").await, "develop");
        assert_eq!(run(&svc, &provider, "unmapped", "").await, "develop");
        assert_eq!(run(&svc, &provider, "unmapped", "  ").await, "develop");

        // no default configured at all → "main"
        let bare = Arc::new(FakeProvider::new(PROVIDER_AZURE_DEVOPS));
        let mut svc2 = Service::new(Config {
            default_provider: ProviderName::from(PROVIDER_AZURE_DEVOPS),
            ..Config::default()
        });
        svc2.register_provider(bare.clone());
        svc2.create_pr(
            &AuthContext::default(),
            CreateParams {
                source_branch: "b".into(),
                ..CreateParams::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            bare.calls
                .lock()
                .unwrap()
                .last_create
                .as_ref()
                .unwrap()
                .target_branch,
            "main"
        );
    }

    // Extra: title/description auto-build parity with `buildPRTitle/`buildPRDescription`.
    #[tokio::test]
    async fn auto_builds_title_and_description() {
        let provider = Arc::new(FakeProvider::new(PROVIDER_AZURE_DEVOPS));
        let mut svc = Service::new(Config {
            default_provider: ProviderName::from(PROVIDER_AZURE_DEVOPS),
            ..Config::default()
        });
        svc.register_provider(provider.clone());

        svc.create_pr(
            &AuthContext::default(),
            CreateParams {
                source_branch: "feature/x".into(),
                ticket_ids: vec!["REL-9".into()],
                ..CreateParams::default()
            },
        )
        .await
        .unwrap();
        {
            let calls = provider.calls.lock().unwrap();
            let last = calls.last_create.as_ref().unwrap();
            assert_eq!(last.title, "REL-9 - feature/x");
            assert_eq!(
                last.description,
                "Automated PR for branch feature/x\n\nLinked tickets: REL-9"
            );
            assert_eq!(last.ticket_ids, vec!["REL-9".to_string()]);
        }

        svc.create_pr(
            &AuthContext::default(),
            CreateParams {
                source_branch: "feature/x".into(),
                ..CreateParams::default()
            },
        )
        .await
        .unwrap();
        {
            let calls = provider.calls.lock().unwrap();
            let last = calls.last_create.as_ref().unwrap();
            assert_eq!(last.title, "PR from feature/x");
            assert_eq!(last.description, "Automated PR for branch feature/x");
            assert!(last.ticket_ids.is_empty());
        }
    }

    // Extra: the service trims the PAT like `authFromContext and passes it
    // to the provider.
    #[tokio::test]
    async fn auth_pat_is_trimmed_and_passed_through() {
        let provider = Arc::new(FakeProvider::new(PROVIDER_AZURE_DEVOPS));
        let mut svc = Service::new(Config {
            default_provider: ProviderName::from(PROVIDER_AZURE_DEVOPS),
            ..Config::default()
        });
        svc.register_provider(provider.clone());

        svc.create_pr(
            &AuthContext {
                azure_devops_pat: "  header-pat  ".into(),
            },
            CreateParams {
                source_branch: "b".into(),
                ..CreateParams::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(provider.calls.lock().unwrap().last_auth_pat, "header-pat");

        svc.complete_pr(
            &AuthContext {
                azure_devops_pat: "hdr2".into(),
            },
            CompleteParams {
                pr_id: "88".into(),
                ..CompleteParams::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(provider.calls.lock().unwrap().last_auth_pat, "hdr2");
    }

    // Extra: complete flow normalizes the provider name and returns the fake result.
    #[tokio::test]
    async fn complete_pr_passes_provider_through() {
        let provider = Arc::new(FakeProvider::new(PROVIDER_AZURE_DEVOPS));
        let mut svc = Service::new(Config {
            default_provider: ProviderName::from(PROVIDER_AZURE_DEVOPS),
            ..Config::default()
        });
        svc.register_provider(provider.clone());

        let res = svc
            .complete_pr(
                &AuthContext::default(),
                CompleteParams {
                    pr_id: "88".into(),
                    ..CompleteParams::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(res.id, "88");
        assert_eq!(res.status, "completed");
        let calls = provider.calls.lock().unwrap();
        let last = calls.last_complete.as_ref().unwrap();
        assert_eq!(last.provider, "azure_devops");
        assert_eq!(last.pr_id, "88");
    }

    // Extra: service → real azuredevops provider → stub server, pinning the
    // auth header path end-to-end ( pinned this in the provider tests).
    #[tokio::test]
    async fn create_pr_with_real_provider_end_to_end() {
        use base64::engine::general_purpose::STANDARD as BASE64_STD;
        use base64::Engine;

        let seen = Arc::new(Mutex::new(String::new()));
        let seen_in_handler = seen.clone();
        let app = axum::Router::new().route(
            "/core/_apis/git/repositories/repo/pullrequests",
            axum::routing::post(move |headers: axum::http::HeaderMap| {
                let seen = seen_in_handler.clone();
                async move {
                    let authz = headers
                        .get(axum::http::header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_string();
                    *seen.lock().unwrap() = authz;
                    axum::Json(serde_json::json!({
                        "pullRequestId": 7,
                        "title": "t",
                        "status": "active",
                    }))
                }
            }),
        );
        let base = crate::test_support::serve(app).await;

        let mut svc = Service::new(Config {
            default_provider: ProviderName::from(PROVIDER_AZURE_DEVOPS),
            ..Config::default()
        });
        svc.register_provider(Arc::new(
            crate::providers::azuredevops::AzureDevOpsProvider::new(
                crate::providers::azuredevops::Config {
                    organization_url: base,
                    project: "core".into(),
                    repository: "repo".into(),
                    pat: String::new(),
                },
            ),
        ));

        let res = svc
            .create_pr(
                &AuthContext {
                    azure_devops_pat: "hdr-pat".into(),
                },
                CreateParams {
                    source_branch: "feature/x".into(),
                    title: "t".into(),
                    description: "d".into(),
                    ..CreateParams::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(res.id, "7");
        assert_eq!(
            *seen.lock().unwrap(),
            format!("Basic {}", BASE64_STD.encode(":hdr-pat"))
        );
    }

    // `detectTicketIDs matrix: provider order, dedupe, byte-wise sort,
    // single-letter prefixes, URL anchors, separators, ASCII digit/boundary parity.
    #[test]
    fn detect_ticket_ids_matrix() {
        // order across providers, then sorted byte-wise ("#" < "A" < "P")
        assert_eq!(
            detect_ticket_ids("fix ABC-2 and PROJ#10 plus #3"),
            vec!["#3".to_string(), "ABC-2".to_string(), "PROJ#10".to_string()]
        );
        // dedupe
        assert_eq!(detect_ticket_ids("ABC-1 ABC-1"), vec!["ABC-1"]);
        // [A-Z][A-Z0-9]+ requires a 2+ char prefix ("A-1" is not Jira-shaped)
        assert_eq!(detect_ticket_ids("A-1"), Vec::<String>::new());
        assert_eq!(detect_ticket_ids("AB-1"), vec!["AB-1"]);
        // URL anchors are excluded: '/' is not a redmine separator
        assert_eq!(
            detect_ticket_ids("https://host/path#12"),
            Vec::<String>::new()
        );
        // comma/paren separators match; '#' anchor after '(' too
        assert_eq!(
            detect_ticket_ids("see #3,#4 and (#7) and ( #8"),
            vec!["#3", "#4", "#7", "#8"]
        );
        // newline separators
        assert_eq!(detect_ticket_ids("#1\n#2"), vec!["#1", "#2"]);
        // no separator → second reference is lost ( RE2 non-overlapping scan)
        assert_eq!(detect_ticket_ids("#5#6"), vec!["#5"]);
        // ASCII word boundaries: '_' and digits block a match
        assert_eq!(detect_ticket_ids("_AB-1"), Vec::<String>::new());
        assert_eq!(detect_ticket_ids("1ABC-2"), Vec::<String>::new());
        // \b is ASCII: a non-ASCII letter before the token is NOT a word char
        assert_eq!(detect_ticket_ids("éABC-2"), vec!["ABC-2"]);
        // \d is ASCII digits: Unicode digits never match
        assert_eq!(detect_ticket_ids("#٣"), Vec::<String>::new());
        // lowercase is never a ticket
        assert_eq!(detect_ticket_ids("abc-12"), Vec::<String>::new());
    }

    #[test]
    fn normalize_preserves_order_and_dedupes() {
        let ids: Vec<String> = [" B ", "A", "B", "", "  "]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            normalize_ticket_ids(&ids),
            vec!["B".to_string(), "A".to_string()]
        );
    }

    #[test]
    fn merge_keeps_current_first() {
        let current = vec!["A".to_string()];
        let extra = vec!["B".to_string(), "A".to_string(), "C".to_string()];
        assert_eq!(
            merge_ticket_ids(&current, &extra),
            vec!["A".to_string(), "B".to_string(), "C".to_string()]
        );
    }

    #[test]
    fn build_title_and_description_formats() {
        assert_eq!(build_pr_title("feature/x", &[]), "PR from feature/x");
        assert_eq!(
            build_pr_title("feature/x", &["REL-9".to_string()]),
            "REL-9 - feature/x"
        );
        // source-branch tickets are auto-detected into the title
        assert_eq!(
            build_pr_title("feature/ABC-1-fix", &[]),
            "ABC-1 - feature/ABC-1-fix"
        );
        assert_eq!(
            build_pr_description("feature/x", &[]),
            "Automated PR for branch feature/x"
        );
        assert_eq!(
            build_pr_description("feature/x", &["REL-9".to_string()]),
            "Automated PR for branch feature/x\n\nLinked tickets: REL-9"
        );
    }
}
