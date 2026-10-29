//! Claim file codec: `.threski/claims/<id>.md` YAML frontmatter, strict
//! parse/render, ID rules.

use chrono::{DateTime, Utc};

pub const SCHEMA_VERSION: i64 = 1;
pub const CLAIMS_DIR: &str = ".threski/claims";

pub const KIND_DECISION: &str = "decision";
pub const KIND_RULE: &str = "rule";
pub const KIND_FIX: &str = "fix";

pub const VISIBILITY_TEAM: &str = "team";
pub const VISIBILITY_PERSONAL: &str = "personal";

pub const REDACTED_STUB: &str = "stub";

#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct ClaimError(pub String);

fn err(msg: impl Into<String>) -> ClaimError {
    ClaimError(msg.into())
}

fn id_matches(id: &str) -> bool {
    let b = id.as_bytes();
    if b.is_empty() || b.len() > 128 {
        return false;
    }
    if !(b[0].is_ascii_alphanumeric()) {
        return false;
    }
    b[1..]
        .iter()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}

const RESERVED_NAMES: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

fn valid_id(id: &str) -> bool {
    id_matches(id) && !RESERVED_NAMES.contains(&id.to_lowercase().as_str())
}

fn key_matches(key: &str) -> bool {
    !key.is_empty()
        && key
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

/// A git-committed knowledge unit (`.threski/claims/<id>.md`).
#[derive(Debug, Clone, PartialEq)]
pub struct Claim {
    pub schema: i64,
    pub id: String,
    pub kind: String,
    pub title: String,
    pub visibility: String,
    pub related_paths: Vec<String>,
    pub supersedes: String,
    pub created: Option<DateTime<Utc>>,
    pub redacted: String,
    pub body: String,
}

impl Claim {
    pub fn validate(&self) -> Result<(), ClaimError> {
        if self.schema != SCHEMA_VERSION {
            return Err(err(format!(
                "claims: unsupported schema {} (want {SCHEMA_VERSION})",
                self.schema
            )));
        }
        if !valid_id(&self.id) {
            return Err(err(format!("claims: invalid id {:?}", self.id)));
        }
        if !matches!(self.kind.as_str(), KIND_DECISION | KIND_RULE | KIND_FIX) {
            return Err(err(format!(
                "claims: invalid kind {:?} (decision | rule | fix)",
                self.kind
            )));
        }
        if self.title.trim().is_empty() {
            return Err(err("claims: title is required"));
        }
        if self.title.contains(['\n', '\r']) {
            return Err(err("claims: title must be a single line"));
        }
        if !self.visibility.is_empty()
            && self.visibility != VISIBILITY_TEAM
            && self.visibility != VISIBILITY_PERSONAL
        {
            return Err(err(format!(
                "claims: invalid visibility {:?} (team | personal)",
                self.visibility
            )));
        }
        if !self.supersedes.is_empty() && !valid_id(&self.supersedes) {
            return Err(err(format!(
                "claims: invalid supersedes id {:?}",
                self.supersedes
            )));
        }
        if self.created.is_none() {
            return Err(err("claims: created is required"));
        }
        if self.redacted.contains(['\n', '\r']) {
            return Err(err("claims: redacted must be a single line"));
        }
        for p in &self.related_paths {
            if p.trim().is_empty() || p.contains(['\n', '\r']) {
                return Err(err(format!("claims: invalid related_paths entry {:?}", p)));
            }
        }
        Ok(())
    }
}

/// Filesystem path of a claim file inside the repository.
pub fn path(id: &str) -> Result<String, ClaimError> {
    if !valid_id(id) {
        return Err(err(format!("claims: invalid id {:?}", id)));
    }
    Ok(format!("{CLAIMS_DIR}/{id}.md"))
}

/// Render the claim to its canonical on-disk form (strict validation first).
pub fn render(c: &Claim) -> Result<Vec<u8>, ClaimError> {
    c.validate()?;
    let mut b = String::new();
    b.push_str("---\n");
    b.push_str(&format!("schema: {}\n", c.schema));
    b.push_str(&format!("id: {}\n", scalar(&c.id)));
    b.push_str(&format!("kind: {}\n", scalar(&c.kind)));
    b.push_str(&format!("title: {}\n", scalar(&c.title)));
    if !c.visibility.is_empty() {
        b.push_str(&format!("visibility: {}\n", scalar(&c.visibility)));
    }
    if !c.related_paths.is_empty() {
        b.push_str("related_paths:\n");
        for p in &c.related_paths {
            b.push_str(&format!("  - {}\n", scalar(p)));
        }
    }
    if !c.supersedes.is_empty() {
        b.push_str(&format!("supersedes: {}\n", scalar(&c.supersedes)));
    }
    b.push_str(&format!(
        "created: {}\n",
        c.created
            .unwrap_or_else(Utc::now)
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    ));
    if !c.redacted.is_empty() {
        b.push_str(&format!("redacted: {}\n", scalar(&c.redacted)));
    }
    b.push_str("---\n\n");
    let mut body = c.body.clone();
    if !body.is_empty() && !body.ends_with('\n') {
        body.push('\n');
    }
    b.push_str(&body);
    Ok(b.into_bytes())
}

fn trim_cr(s: &str) -> &str {
    s.strip_suffix('\r').unwrap_or(s)
}

/// Parse a claim file strictly (duplicate keys rejected, block-list only
/// related_paths, RFC3339 created).
pub fn parse(data: &[u8]) -> Result<Claim, ClaimError> {
    let mut c = Claim::default_schema();
    let raw = String::from_utf8_lossy(data);
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(raw.as_ref());
    if raw.is_empty() {
        return Err(err("claims: empty claim file"));
    }
    let lines: Vec<&str> = raw.split('\n').collect();
    if trim_cr(lines[0]) != "---" {
        return Err(err("claims: missing frontmatter opener"));
    }
    let mut end: Option<usize> = None;
    for (i, line) in lines.iter().enumerate().skip(1) {
        if trim_cr(line) == "---" {
            end = Some(i);
            break;
        }
    }
    let Some(end) = end else {
        return Err(err("claims: frontmatter not closed"));
    };

    let mut fields: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut related: Vec<String> = Vec::new();
    let mut in_related = false;
    for line in &lines[1..end] {
        let trimmed = trim_cr(line);
        if trimmed.trim().is_empty() {
            continue;
        }
        if trimmed.starts_with(' ') || trimmed.starts_with('\t') {
            let Some(item) = list_item(trimmed) else {
                return Err(err("claims: unexpected indentation in frontmatter"));
            };
            if !in_related {
                return Err(err("claims: unexpected indentation in frontmatter"));
            }
            related.push(item);
            continue;
        }
        let Some(idx) = trimmed.find(':') else {
            return Err(err(format!(
                "claims: malformed frontmatter line {trimmed:?}"
            )));
        };
        if idx == 0 {
            return Err(err(format!(
                "claims: malformed frontmatter line {trimmed:?}"
            )));
        }
        let key = trimmed[..idx].trim();
        let value = trimmed[idx + 1..].trim();
        if !key_matches(key) {
            return Err(err(format!("claims: invalid frontmatter key {key:?}")));
        }
        if fields.contains_key(key) {
            return Err(err(format!("claims: duplicate frontmatter key {key:?}")));
        }
        fields.insert(key.to_string(), value.to_string());
        if key == "related_paths" && !value.is_empty() {
            return Err(err(format!(
                "claims: related_paths must be a block list, got {value:?}"
            )));
        }
        in_related = key == "related_paths";
    }

    let Some(v) = fields.get("schema") else {
        return Err(err("claims: missing required field schema"));
    };
    let Ok(schema) = v.parse::<i64>() else {
        return Err(err(format!("claims: schema must be an integer, got {v:?}")));
    };
    c.schema = schema;

    let Some(v) = fields.get("id") else {
        return Err(err("claims: missing required field id"));
    };
    c.id = unquote_scalar(v);

    let Some(v) = fields.get("kind") else {
        return Err(err("claims: missing required field kind"));
    };
    c.kind = unquote_scalar(v);

    let Some(v) = fields.get("title") else {
        return Err(err("claims: missing required field title"));
    };
    c.title = unquote_scalar(v);

    let Some(v) = fields.get("created") else {
        return Err(err("claims: missing required field created"));
    };
    let created = DateTime::parse_from_rfc3339(&unquote_scalar(v))
        .map_err(|_| err(format!("claims: created must be RFC3339, got {v:?}")))?;
    c.created = Some(created.with_timezone(&Utc));

    if let Some(v) = fields.get("visibility") {
        c.visibility = unquote_scalar(v);
    }
    if let Some(v) = fields.get("supersedes") {
        c.supersedes = unquote_scalar(v);
    }
    if let Some(v) = fields.get("redacted") {
        c.redacted = unquote_scalar(v);
    }
    c.related_paths = related;

    let mut body = lines[end + 1..].join("\n");
    body = body.replace("\r\n", "\n");
    body = body
        .strip_prefix('\n')
        .map(|s| s.to_string())
        .unwrap_or(body);
    c.body = body;

    c.validate()?;
    Ok(c)
}

/// Renders the created timestamp likewise time.RFC3339 (UTC, seconds precision).
pub fn created_now() -> DateTime<Utc> {
    Utc::now()
}

impl Claim {
    fn default_schema() -> Self {
        Claim {
            schema: 0,
            id: String::new(),
            kind: String::new(),
            title: String::new(),
            visibility: String::new(),
            related_paths: Vec::new(),
            supersedes: String::new(),
            created: None,
            redacted: String::new(),
            body: String::new(),
        }
    }
}

fn scalar(s: &str) -> String {
    if needs_quote(s) {
        quote_string(s)
    } else {
        s.to_string()
    }
}

fn needs_quote(s: &str) -> bool {
    if s.is_empty() {
        return true;
    }
    let b = s.as_bytes();
    if matches!(
        b[0],
        b'!' | b'&'
            | b'*'
            | b'-'
            | b'?'
            | b'|'
            | b'>'
            | b'%'
            | b'@'
            | b'`'
            | b'"'
            | b'#'
            | b','
            | b':'
            | b'['
            | b']'
            | b'{'
            | b'}'
            | b' '
            | b'\t'
    ) {
        return true;
    }
    if s.contains(['\n', '\r']) {
        return true;
    }
    if s.contains(": ") || s.contains(" #") || s.contains('\t') {
        return true;
    }
    if s != s.trim() || s.ends_with(':') {
        return true;
    }
    false
}

/// strconv.Quote equivalent (double-quoted escapes — a YAML-compatible subset).
fn quote_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// strconv.Unquote equivalent for the quote_string output shape.
fn unquote_string(s: &str) -> Option<String> {
    let inner = s.strip_prefix('"')?.strip_suffix('"')?;
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next()? {
            '"' => out.push('"'),
            '\\' => out.push('\\'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'x' => {
                let h1 = chars.next()?;
                let h2 = chars.next()?;
                let byte = u8::from_str_radix(&format!("{h1}{h2}"), 16).ok()?;
                // strconv.Unquote produces the decoded rune(s).
                out.push_str(&String::from_utf8_lossy(&[byte]));
            }
            _ => return None,
        }
    }
    Some(out)
}

fn list_item(line: &str) -> Option<String> {
    let trimmed = line.trim_start_matches([' ', '\t']);
    let rest = trimmed.strip_prefix("- ")?;
    Some(unquote_scalar(rest.trim()))
}

fn unquote_scalar(s: &str) -> String {
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        if let Some(unquoted) = unquote_string(s) {
            return unquoted;
        }
    }
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn sample_claim() -> Claim {
        Claim {
            schema: SCHEMA_VERSION,
            id: "dec-2026-10-24-001".into(),
            kind: KIND_DECISION.into(),
            title: "Short imperative title".into(),
            visibility: VISIBILITY_TEAM.into(),
            related_paths: vec!["src/search/memory_add.rs".into()],
            supersedes: "dec-2026-09-30-007".into(),
            created: DateTime::parse_from_rfc3339("2026-10-24T15:04:05Z")
                .ok()
                .map(|d| d.with_timezone(&Utc)),
            redacted: REDACTED_STUB.into(),
            body: "Body markdown. The claim.\n".into(),
        }
    }

    #[test]
    fn render_golden() {
        let got = render(&sample_claim()).unwrap();
        let want = "---\nschema: 1\nid: dec-2026-10-24-001\nkind: decision\ntitle: Short imperative title\nvisibility: team\nrelated_paths:\n  - src/search/memory_add.rs\nsupersedes: dec-2026-09-30-007\ncreated: 2026-10-24T15:04:05Z\nredacted: stub\n---\n\nBody markdown. The claim.\n";
        assert_eq!(String::from_utf8(got).unwrap(), want);
    }

    #[test]
    fn render_minimal_omits_optionals() {
        let mut c = sample_claim();
        c.visibility = String::new();
        c.related_paths = Vec::new();
        c.supersedes = String::new();
        c.redacted = String::new();
        let got = String::from_utf8(render(&c).unwrap()).unwrap();
        for field in ["visibility:", "related_paths:", "supersedes:", "redacted:"] {
            assert!(
                !got.contains(field),
                "expected optional field {field:?} to be omitted:\n{got}"
            );
        }
    }

    #[test]
    fn round_trip_byte_stable() {
        let first = render(&sample_claim()).unwrap();
        let parsed = parse(&first).unwrap();
        let second = render(&parsed).unwrap();
        assert_eq!(first, second, "round-trip not byte-stable");
        assert_eq!(parsed, sample_claim());
    }

    #[test]
    fn round_trip_non_utc_created_normalizes() {
        let mut c = sample_claim();
        let cet = chrono::FixedOffset::east_opt(7200).unwrap();
        let local = cet.with_ymd_and_hms(2026, 10, 24, 17, 4, 5).unwrap();
        c.created = Some(local.with_timezone(&Utc));
        // Render from an offset-preserving timestamp: emulate by rendering the
        // local datetime string directly through parse.
        let first = format!(
            "---\nschema: 1\nid: {}\nkind: {}\ntitle: {}\ncreated: {}\n---\n\nbody\n",
            c.id,
            c.kind,
            scalar(&c.title),
            local.to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
        );
        let parsed = parse(first.as_bytes()).unwrap();
        assert_eq!(parsed.created.unwrap(), local.with_timezone(&Utc));
        let second = render(&parsed).unwrap();
        assert!(
            String::from_utf8_lossy(&second).contains("created: 2026-10-24T15:04:05Z"),
            "expected UTC-normalized created:\n{}",
            String::from_utf8_lossy(&second)
        );
        let third = parse(&second).unwrap();
        assert_eq!(
            second,
            render(&third).unwrap(),
            "round-trip not byte-stable after UTC normalization"
        );
    }

    #[test]
    fn quotes_when_needed() {
        let mut c = sample_claim();
        c.title = r#"Use: "careful" #tags"#.into();
        let got = String::from_utf8(render(&c).unwrap()).unwrap();
        assert!(
            got.contains(r#"title: "Use: \"careful\" #tags""#),
            "expected quoted title:\n{got}"
        );
        let parsed = parse(got.as_bytes()).unwrap();
        assert_eq!(parsed.title, c.title);
        let again = render(&parsed).unwrap();
        assert_eq!(
            got,
            String::from_utf8(again).unwrap(),
            "quoted round-trip not byte-stable"
        );
    }

    #[test]
    fn path_rules() {
        assert_eq!(
            path("dec-2026-10-24-001").unwrap(),
            ".threski/claims/dec-2026-10-24-001.md"
        );
        for bad in [
            "",
            "../evil",
            "a/b",
            ".hidden",
            "con",
            "COM1",
            "x".repeat(129).as_str(),
        ] {
            assert!(path(bad).is_err(), "expected error for id {bad:?}");
        }
    }

    #[test]
    fn parse_tolerates_unknown_keys() {
        let base = String::from_utf8(render(&sample_claim()).unwrap()).unwrap();
        let with_extra = base.replacen(
            "kind: decision\n",
            "kind: decision\nfuture_field: whatever\n",
            1,
        );
        let parsed = parse(with_extra.as_bytes()).unwrap();
        assert_eq!(parsed.id, sample_claim().id);
        assert_eq!(parsed.kind, KIND_DECISION);
    }

    #[test]
    fn parse_crlf() {
        let unix = String::from_utf8(render(&sample_claim()).unwrap()).unwrap();
        let crlf = unix.replace('\n', "\r\n");
        let parsed = parse(crlf.as_bytes()).unwrap();
        assert_eq!(parsed.title, sample_claim().title);
        assert_eq!(parsed.body, sample_claim().body);
    }

    #[test]
    fn parse_bom() {
        let unix = render(&sample_claim()).unwrap();
        let mut bom = vec![0xEF, 0xBB, 0xBF];
        bom.extend_from_slice(&unix);
        let parsed = parse(&bom).unwrap();
        assert_eq!(parsed.id, sample_claim().id);
    }

    #[test]
    fn parse_validation_errors() {
        let valid = String::from_utf8(render(&sample_claim()).unwrap()).unwrap();
        let replace = |from: &str, to: &str| valid.replacen(from, to, 1).into_bytes();
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("no frontmatter", b"just some body text".to_vec()),
            ("unclosed frontmatter", b"---\nid: x\n".to_vec()),
            ("empty file", Vec::new()),
            ("bad schema zero", replace("schema: 1", "schema: 0")),
            ("bad schema version", replace("schema: 1", "schema: 2")),
            ("schema not integer", replace("schema: 1", "schema: one")),
            ("missing id", replace("id: dec-2026-10-24-001\n", "")),
            (
                "bad id charset",
                replace("id: dec-2026-10-24-001", "id: ../evil"),
            ),
            ("bad kind", replace("kind: decision", "kind: fact")),
            (
                "missing title",
                replace("title: Short imperative title\n", ""),
            ),
            (
                "multiline title",
                replace("title: Short imperative title", "title: one\ntitle: two"),
            ),
            (
                "bad visibility",
                replace("visibility: team", "visibility: public"),
            ),
            (
                "bad supersedes",
                replace(
                    "supersedes: dec-2026-09-30-007",
                    "supersedes: not a valid id",
                ),
            ),
            (
                "missing created",
                replace("created: 2026-10-24T15:04:05Z\n", ""),
            ),
            (
                "bad created",
                replace("created: 2026-10-24T15:04:05Z", "created: yesterday"),
            ),
            (
                "duplicate key",
                replace("kind: decision", "kind: decision\nkind: fix"),
            ),
            (
                "stray list item",
                replace(
                    "id: dec-2026-10-24-001",
                    "id: dec-2026-10-24-001\n  - stray",
                ),
            ),
            (
                "inline related_paths",
                replace("related_paths:", "related_paths: [a, b]"),
            ),
            (
                "empty related entry",
                replace("- src/search/memory_add.rs", "-   "),
            ),
        ];
        for (name, data) in cases {
            assert!(
                parse(&data).is_err(),
                "expected parse error for case {name}"
            );
        }
    }

    #[test]
    fn validate_rejects() {
        let mut c = sample_claim();
        c.schema = 2;
        assert!(c.validate().is_err());
        let mut c = sample_claim();
        c.body = "body".into();
        c.created = None;
        assert!(c.validate().is_err());
        let mut c = sample_claim();
        c.related_paths = vec!["ok/path.rs".into(), String::new()];
        assert!(c.validate().is_err());
    }

    #[test]
    fn empty_body_allowed() {
        let mut c = sample_claim();
        c.body = String::new();
        let got = String::from_utf8(render(&c).unwrap()).unwrap();
        assert!(
            got.ends_with("---\n\n"),
            "expected empty body render:\n{got:?}"
        );
        let parsed = parse(got.as_bytes()).unwrap();
        assert_eq!(parsed.body, "");
    }
}
