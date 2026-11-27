use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

/// Maximum length of an Agent Skills name per https://agentskills.io/specification.
pub const MAX_SKILL_NAME_LEN: usize = 64;

/// Maximum length of an Agent Skills description per specification.
pub const MAX_SKILL_DESCRIPTION_LEN: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationSeverity {
    Error,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillValidationIssue {
    pub severity: ValidationSeverity,
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillValidationReport {
    pub issues: Vec<SkillValidationIssue>,
}

impl SkillValidationReport {
    pub fn is_valid(&self) -> bool {
        !self
            .issues
            .iter()
            .any(|i| i.severity == ValidationSeverity::Error)
    }

    pub fn errors(&self) -> Vec<&SkillValidationIssue> {
        self.issues
            .iter()
            .filter(|i| i.severity == ValidationSeverity::Error)
            .collect()
    }

    pub fn warnings(&self) -> Vec<&SkillValidationIssue> {
        self.issues
            .iter()
            .filter(|i| i.severity == ValidationSeverity::Warning)
            .collect()
    }

    pub fn push_error(&mut self, code: impl Into<String>, message: impl Into<String>) {
        self.issues.push(SkillValidationIssue {
            severity: ValidationSeverity::Error,
            code: code.into(),
            message: message.into(),
        });
    }

    pub fn push_warning(&mut self, code: impl Into<String>, message: impl Into<String>) {
        self.issues.push(SkillValidationIssue {
            severity: ValidationSeverity::Warning,
            code: code.into(),
            message: message.into(),
        });
    }

    pub fn error_summary(&self) -> String {
        self.errors()
            .into_iter()
            .map(|e| e.message.as_str())
            .collect::<Vec<_>>()
            .join("; ")
    }
}

/// Parsed representation of a `SKILL.md` file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedSkillMd {
    pub name: String,
    pub description: String,
    /// Raw YAML frontmatter block (excluding the `---` fence lines).
    pub frontmatter_raw: String,
    /// Additional top-level YAML keys preserved verbatim (value text).
    pub extra_metadata: BTreeMap<String, String>,
    /// Markdown body after the closing `---` fence.
    pub body: String,
}

/// Metadata about a supporting file inside a skill directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelatedFileInfo {
    /// Forward-slash relative path inside the skill directory (e.g. `scripts/run.sh`).
    pub relative_path: String,
    pub size_bytes: u64,
    pub is_executable: bool,
}

/// Validates a skill name according to the Agent Skills specification:
/// lowercase ASCII letters, digits, and single hyphens; 1..=64 chars;
/// must not start or end with `-` or contain `--`.
pub fn validate_skill_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Skill name must not be empty".to_string());
    }
    if name.len() > MAX_SKILL_NAME_LEN {
        return Err(format!(
            "Skill name '{name}' exceeds maximum length of {MAX_SKILL_NAME_LEN} characters"
        ));
    }
    if name.starts_with('-') || name.ends_with('-') {
        return Err(format!(
            "Skill name '{name}' must not start or end with a hyphen"
        ));
    }
    if name.contains("--") {
        return Err(format!(
            "Skill name '{name}' must not contain consecutive hyphens"
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(format!(
            "Skill name '{name}' must contain only lowercase ASCII letters, digits, and hyphens"
        ));
    }
    Ok(())
}

/// Sanitizes an arbitrary string into a valid Agent Skills slug.
pub fn slugify_skill_name(raw: &str) -> String {
    let mut out = String::new();
    let mut prev_hyphen = true;
    for ch in raw.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_lowercase() || lower.is_ascii_digit() {
            out.push(lower);
            prev_hyphen = false;
        } else if !prev_hyphen && !out.is_empty() {
            out.push('-');
            prev_hyphen = true;
        }
        if out.len() >= MAX_SKILL_NAME_LEN {
            break;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "untitled-skill".to_string()
    } else {
        trimmed
    }
}

/// Parses a `SKILL.md` document into frontmatter fields and Markdown body.
/// Supports quoted strings, single-line scalars, and YAML block scalars (`>`, `>-`, `|`, `|-`),
/// while preserving unknown frontmatter keys.
pub fn parse_skill_md(content: &str) -> Result<ParsedSkillMd, String> {
    let normalized = content.replace("\r\n", "\n");
    let stripped = normalized.strip_prefix('\u{feff}').unwrap_or(&normalized);
    let mut lines = stripped.lines();
    let first = lines
        .next()
        .ok_or_else(|| "SKILL.md is empty; missing YAML frontmatter".to_string())?;
    if first.trim() != "---" {
        return Err("SKILL.md must start with a YAML frontmatter delimiter ('---')".to_string());
    }

    let mut fm_lines: Vec<&str> = Vec::new();
    let mut closed = false;
    let mut body_lines: Vec<&str> = Vec::new();
    for line in lines {
        if !closed {
            if line.trim() == "---" || line.trim() == "..." {
                closed = true;
            } else {
                fm_lines.push(line);
            }
        } else {
            body_lines.push(line);
        }
    }

    if !closed {
        return Err("SKILL.md frontmatter is not closed with '---'".to_string());
    }

    let frontmatter_raw = fm_lines.join("\n");
    let fields = parse_yaml_frontmatter_map(&fm_lines)?;

    let name = fields.get("name").cloned().unwrap_or_default();
    let description = fields.get("description").cloned().unwrap_or_default();
    let mut extra_metadata = BTreeMap::new();
    for (k, v) in fields {
        if k != "name" && k != "description" {
            extra_metadata.insert(k, v);
        }
    }

    // Trim leading blank lines from body for clean round-trip rendering.
    let body = body_lines.join("\n").trim_start_matches('\n').to_string();

    Ok(ParsedSkillMd {
        name,
        description,
        frontmatter_raw,
        extra_metadata,
        body,
    })
}

/// Lightweight YAML frontmatter parser tailored for `SKILL.md`.
/// Preserves unknown top-level keys (including nested YAML blocks as raw text).
fn parse_yaml_frontmatter_map(lines: &[&str]) -> Result<BTreeMap<String, String>, String> {
    let mut map = BTreeMap::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            i += 1;
            continue;
        }
        // Top-level key must not be indented.
        if line.starts_with(' ') || line.starts_with('\t') {
            return Err(format!(
                "Invalid YAML frontmatter indentation at line {}: '{line}'",
                i + 2
            ));
        }
        let Some((raw_key, raw_val)) = line.split_once(':') else {
            return Err(format!(
                "Invalid YAML frontmatter entry at line {}: expected 'key: value'",
                i + 2
            ));
        };
        let key = raw_key.trim();
        if key.is_empty()
            || !key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        {
            return Err(format!("Invalid YAML frontmatter key '{key}'"));
        }
        let val_trimmed = raw_val.trim();

        // Block scalar (`>`, `>-`, `|+`, `|`, `|-`, `>+`)
        if matches!(val_trimmed, ">" | ">-" | ">+" | "|" | "|-" | "|+") {
            let folded = val_trimmed.starts_with('>');
            i += 1;
            let mut block_lines: Vec<String> = Vec::new();
            let mut base_indent: Option<usize> = None;
            while i < lines.len() {
                let next = lines[i];
                if next.trim().is_empty() {
                    block_lines.push(String::new());
                    i += 1;
                    continue;
                }
                let indent = next.chars().take_while(|c| *c == ' ').count();
                if indent == 0 {
                    break;
                }
                let bi = *base_indent.get_or_insert(indent);
                let cut = bi.min(next.len());
                block_lines.push(next[cut..].to_string());
                i += 1;
            }
            while block_lines.last().is_some_and(|l| l.is_empty()) {
                block_lines.pop();
            }
            let value = if folded {
                let mut joined = String::new();
                for (idx, bl) in block_lines.iter().enumerate() {
                    if idx > 0 {
                        if bl.is_empty() || block_lines[idx - 1].is_empty() {
                            joined.push('\n');
                        } else {
                            joined.push(' ');
                        }
                    }
                    joined.push_str(bl.trim());
                }
                joined
            } else {
                block_lines.join("\n")
            };
            map.insert(key.to_string(), value);
            continue;
        }

        // Nested YAML block (e.g. `metadata:\n  author: ...` or list `allowed-tools:\n  - ...`)
        if val_trimmed.is_empty() {
            i += 1;
            let mut nested: Vec<&str> = Vec::new();
            while i < lines.len() {
                let next = lines[i];
                if next.trim().is_empty() || next.starts_with(' ') || next.starts_with('\t') {
                    nested.push(next);
                    i += 1;
                } else {
                    break;
                }
            }
            while nested.last().is_some_and(|l| l.trim().is_empty()) {
                nested.pop();
            }
            map.insert(key.to_string(), nested.join("\n"));
            continue;
        }

        // Inline scalar (possibly quoted)
        let parsed = unquote_yaml_scalar(val_trimmed)?;
        map.insert(key.to_string(), parsed);
        i += 1;
    }
    Ok(map)
}

fn unquote_yaml_scalar(val: &str) -> Result<String, String> {
    if val.starts_with('"') {
        if !val.ends_with('"') || val.len() < 2 {
            return Err(format!("Unterminated double-quoted YAML string: {val}"));
        }
        let inner = &val[1..val.len() - 1];
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                match chars.next() {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some('r') => out.push('\r'),
                    Some('"') => out.push('"'),
                    Some('\\') => out.push('\\'),
                    Some(other) => {
                        out.push('\\');
                        out.push(other);
                    }
                    None => out.push('\\'),
                }
            } else {
                out.push(c);
            }
        }
        Ok(out)
    } else if val.starts_with('\'') {
        if !val.ends_with('\'') || val.len() < 2 {
            return Err(format!("Unterminated single-quoted YAML string: {val}"));
        }
        Ok(val[1..val.len() - 1].replace("''", "'"))
    } else {
        Ok(val.to_string())
    }
}

fn format_yaml_scalar(val: &str) -> String {
    if val.contains('\n') {
        let indented = val
            .lines()
            .map(|l| format!("  {l}"))
            .collect::<Vec<_>>()
            .join("\n");
        format!("|-\n{indented}")
    } else if val.is_empty()
        || val.starts_with([
            ' ', '-', '?', ':', '#', '&', '*', '!', '|', '>', '\'', '"', '%', '@', '`', '{', '[',
        ])
        || val.ends_with(' ')
        || val.contains(": ")
        || val.contains(" #")
    {
        let escaped = val.replace('\\', "\\\\").replace('"', "\\\"");
        format!("\"{escaped}\"")
    } else {
        val.to_string()
    }
}

/// Renders a standard `SKILL.md` file while preserving any extra frontmatter keys.
pub fn render_skill_md(
    name: &str,
    description: &str,
    extra_metadata: &BTreeMap<String, String>,
    body: &str,
) -> String {
    let mut out = String::new();
    out.push_str("---\n");
    out.push_str(&format!("name: {}\n", format_yaml_scalar(name)));
    out.push_str(&format!(
        "description: {}\n",
        format_yaml_scalar(description)
    ));
    for (k, v) in extra_metadata {
        if k == "name" || k == "description" {
            continue;
        }
        if v.lines().any(|l| l.starts_with(' ') || l.starts_with('\t')) {
            out.push_str(&format!("{k}:\n{v}\n"));
        } else {
            out.push_str(&format!("{k}: {}\n", format_yaml_scalar(v)));
        }
    }
    out.push_str("---\n\n");
    let body_trimmed = body.trim_start_matches('\n');
    out.push_str(body_trimmed);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Validates `SKILL.md` text and optionally checks that `name` matches `expected_dir_name`.
pub fn validate_skill_md(
    content: &str,
    expected_dir_name: Option<&str>,
) -> (Option<ParsedSkillMd>, SkillValidationReport) {
    let mut report = SkillValidationReport::default();
    let parsed = match parse_skill_md(content) {
        Ok(p) => p,
        Err(err) => {
            report.push_error("frontmatter_invalid", err);
            return (None, report);
        }
    };

    if let Err(err) = validate_skill_name(&parsed.name) {
        report.push_error("name_invalid", err);
    }

    if parsed.description.trim().is_empty() {
        report.push_error(
            "description_missing",
            "SKILL.md frontmatter must include a non-empty 'description'",
        );
    } else if parsed.description.chars().count() > MAX_SKILL_DESCRIPTION_LEN {
        report.push_warning(
            "description_too_long",
            format!("Description exceeds recommended {MAX_SKILL_DESCRIPTION_LEN} characters"),
        );
    }

    if let Some(dir_name) = expected_dir_name {
        if !parsed.name.is_empty() && parsed.name != dir_name {
            report.push_error(
                "dir_name_mismatch",
                format!(
                    "Directory name '{dir_name}' does not match SKILL.md name '{}'",
                    parsed.name
                ),
            );
        }
    }

    if parsed.body.trim().is_empty() {
        report.push_warning(
            "body_empty",
            "SKILL.md has no instruction body after frontmatter",
        );
    }

    // Detect suspicious markdown links escaping the skill directory (e.g. `../../etc/passwd`).
    if contains_parent_traversal_reference(&parsed.body) {
        report.push_error(
            "path_traversal_reference",
            "SKILL.md contains a relative path referencing outside the skill directory ('../')",
        );
    }

    (Some(parsed), report)
}

fn contains_parent_traversal_reference(markdown: &str) -> bool {
    let mut rest = markdown;
    while let Some(idx) = rest.find("](") {
        let after = &rest[idx + 2..];
        let Some(end) = after.find(')') else {
            break;
        };
        let target = after[..end].trim();
        let target = target.split_whitespace().next().unwrap_or("");
        if !target.starts_with("http://")
            && !target.starts_with("https://")
            && !target.starts_with('#')
            && (target.starts_with("../") || target.contains("/../") || target == "..")
        {
            return true;
        }
        rest = &after[end + 1..];
    }
    false
}

/// Validates a relative path intended to live inside a skill directory.
pub fn validate_relative_skill_path(rel: &str) -> Result<PathBuf, String> {
    if rel.trim().is_empty() {
        return Err("Relative file path must not be empty".to_string());
    }
    if rel.contains('\\') {
        return Err(format!("Path '{rel}' must use forward slashes"));
    }
    let path = Path::new(rel);
    if path.is_absolute() {
        return Err(format!(
            "Path '{rel}' must be relative to the skill directory"
        ));
    }
    let mut clean = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::Normal(part) => clean.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(format!(
                    "Path '{rel}' escapes the skill directory (path traversal rejected)"
                ));
            }
        }
    }
    if clean.as_os_str().is_empty() {
        return Err("Relative file path must not be empty".to_string());
    }
    Ok(clean)
}

/// Validates an entire skill directory on disk:
/// - `SKILL.md` exists and passes validation
/// - directory name matches `name`
/// - no symlink escapes outside the canonical skill directory
/// - collects related files & checks for executable scripts
pub fn validate_skill_dir(
    dir: &Path,
) -> (
    Option<ParsedSkillMd>,
    Vec<RelatedFileInfo>,
    SkillValidationReport,
) {
    let mut report = SkillValidationReport::default();
    let mut related = Vec::new();

    if !dir.is_dir() {
        report.push_error(
            "dir_missing",
            format!("Skill directory does not exist: {}", dir.display()),
        );
        return (None, related, report);
    }

    let skill_md_path = dir.join("SKILL.md");
    if !skill_md_path.is_file() {
        report.push_error(
            "skill_md_missing",
            format!("Missing required SKILL.md in {}", dir.display()),
        );
        return (None, related, report);
    }

    let canonical_root = match fs::canonicalize(dir) {
        Ok(p) => p,
        Err(e) => {
            report.push_error(
                "dir_unreadable",
                format!("Failed to canonicalize {}: {e}", dir.display()),
            );
            return (None, related, report);
        }
    };

    let content = match fs::read_to_string(&skill_md_path) {
        Ok(c) => c,
        Err(e) => {
            report.push_error(
                "skill_md_unreadable",
                format!("Failed to read SKILL.md: {e}"),
            );
            return (None, related, report);
        }
    };

    let dir_name = dir.file_name().and_then(|s| s.to_str());
    let (parsed, md_report) = validate_skill_md(&content, dir_name);
    report.issues.extend(md_report.issues);

    // Walk directory safely, rejecting symlinks escaping canonical_root.
    if let Err(err) =
        collect_related_files(&canonical_root, &canonical_root, &mut related, &mut report)
    {
        report.push_error("walk_error", err.to_string());
    }
    related.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));

    (parsed, related, report)
}

fn collect_related_files(
    root: &Path,
    current: &Path,
    out: &mut Vec<RelatedFileInfo>,
    report: &mut SkillValidationReport,
) -> Result<()> {
    for entry in fs::read_dir(current)
        .with_context(|| format!("Failed to read directory {}", current.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let meta = fs::symlink_metadata(&path)?;
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");

        if meta.file_type().is_symlink() {
            match fs::canonicalize(&path) {
                Ok(target) if target.starts_with(root) => {
                    // Internal symlink within skill dir; avoid infinite recursion by not descending into dir symlinks.
                    if target.is_file() && rel != "SKILL.md" {
                        let tmeta = fs::metadata(&target)?;
                        out.push(RelatedFileInfo {
                            is_executable: is_executable_file(&target, &tmeta),
                            relative_path: rel,
                            size_bytes: tmeta.len(),
                        });
                    }
                }
                Ok(_) => {
                    report.push_error(
                        "symlink_escape",
                        format!("Symlink '{rel}' points outside the skill directory"),
                    );
                }
                Err(_) => {
                    report.push_error(
                        "broken_symlink",
                        format!("Broken symlink '{rel}' inside skill directory"),
                    );
                }
            }
            continue;
        }

        if meta.is_dir() {
            collect_related_files(root, &path, out, report)?;
        } else if meta.is_file() && rel != "SKILL.md" {
            out.push(RelatedFileInfo {
                is_executable: is_executable_file(&path, &meta),
                relative_path: rel,
                size_bytes: meta.len(),
            });
        }
    }
    Ok(())
}

pub fn is_executable_file(path: &Path, meta: &fs::Metadata) -> bool {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    if matches!(
        ext.as_str(),
        "sh" | "bash"
            | "zsh"
            | "ps1"
            | "bat"
            | "cmd"
            | "py"
            | "js"
            | "mjs"
            | "ts"
            | "rb"
            | "pl"
            | "exe"
    ) {
        return true;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 != 0 {
            return true;
        }
    }
    #[cfg(not(unix))]
    let _ = meta;
    false
}

/// Computes a deterministic SHA-256 hex digest over all files (relative paths + contents)
/// inside a skill directory. Skips `.same-managed.json` metadata files.
pub fn compute_dir_hash(dir: &Path) -> Result<String> {
    let canonical_root = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    gather_files_for_hash(&canonical_root, &canonical_root, &mut files)?;
    files.sort_by(|a, b| a.0.cmp(&b.0));

    let mut hasher = Sha256::new();
    for (rel, path) in files {
        hasher.update(rel.as_bytes());
        hasher.update(&[0u8]);
        let meta = fs::metadata(&path)
            .with_context(|| format!("Failed to stat {} for hashing", path.display()))?;
        hasher.update(&[is_executable_file(&path, &meta) as u8]);
        let bytes = fs::read(&path)
            .with_context(|| format!("Failed to read {} for hashing", path.display()))?;
        hasher.update(&(bytes.len() as u64).to_le_bytes());
        hasher.update(&bytes);
        hasher.update(&[0xffu8]);
    }
    Ok(hasher.finalize_hex())
}

fn gather_files_for_hash(
    root: &Path,
    current: &Path,
    out: &mut Vec<(String, PathBuf)>,
) -> Result<()> {
    for entry in fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();
        let meta = fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        if rel == ".same-managed.json" {
            continue;
        }
        if meta.is_dir() {
            gather_files_for_hash(root, &path, out)?;
        } else if meta.is_file() {
            out.push((rel, path));
        }
    }
    Ok(())
}

/// Minimal pure-Rust SHA-256 implementation (FIPS 180-4) so `same-core` needs no extra crypto crate.
pub struct Sha256 {
    state: [u32; 8],
    buf: [u8; 64],
    buf_len: usize,
    total_bits: u64,
}

impl Sha256 {
    pub fn new() -> Self {
        Self {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            buf: [0u8; 64],
            buf_len: 0,
            total_bits: 0,
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.total_bits = self.total_bits.wrapping_add((data.len() as u64) * 8);
        if self.buf_len > 0 {
            let take = (64 - self.buf_len).min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
            if self.buf_len == 64 {
                let block = self.buf;
                self.compress(&block);
                self.buf_len = 0;
            }
        }
        while data.len() >= 64 {
            let mut block = [0u8; 64];
            block.copy_from_slice(&data[..64]);
            self.compress(&block);
            data = &data[64..];
        }
        if !data.is_empty() {
            self.buf[..data.len()].copy_from_slice(data);
            self.buf_len = data.len();
        }
    }

    pub fn finalize_hex(mut self) -> String {
        let bits = self.total_bits;
        self.buf[self.buf_len] = 0x80;
        self.buf_len += 1;
        if self.buf_len > 56 {
            self.buf[self.buf_len..64].fill(0);
            let block = self.buf;
            self.compress(&block);
            self.buf_len = 0;
        }
        self.buf[self.buf_len..56].fill(0);
        self.buf[56..64].copy_from_slice(&bits.to_be_bytes());
        let block = self.buf;
        self.compress(&block);

        let mut hex = String::with_capacity(64);
        for word in self.state {
            hex.push_str(&format!("{word:08x}"));
        }
        hex
    }

    fn compress(&mut self, block: &[u8; 64]) {
        const K: [u32; 64] = [
            0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
            0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
            0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
            0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
            0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
            0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
            0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
            0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
            0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
            0xc67178f2,
        ];
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                block[i * 4],
                block[i * 4 + 1],
                block[i * 4 + 2],
                block[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        self.state[0] = self.state[0].wrapping_add(a);
        self.state[1] = self.state[1].wrapping_add(b);
        self.state[2] = self.state[2].wrapping_add(c);
        self.state[3] = self.state[3].wrapping_add(d);
        self.state[4] = self.state[4].wrapping_add(e);
        self.state[5] = self.state[5].wrapping_add(f);
        self.state[6] = self.state[6].wrapping_add(g);
        self.state[7] = self.state[7].wrapping_add(h);
    }
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_skill_names() {
        assert!(validate_skill_name("code-review").is_ok());
        assert!(validate_skill_name("a").is_ok());
        assert!(validate_skill_name("skill123-v2").is_ok());
        assert!(validate_skill_name("").is_err());
        assert!(validate_skill_name("-leading").is_err());
        assert!(validate_skill_name("trailing-").is_err());
        assert!(validate_skill_name("double--hyphen").is_err());
        assert!(validate_skill_name("UpperCase").is_err());
        assert!(validate_skill_name("with_underscore").is_err());
        assert!(validate_skill_name(&"a".repeat(65)).is_err());
    }

    #[test]
    fn parses_and_preserves_unknown_frontmatter() {
        let md = "---\nname: my-skill\ndescription: >-\n  Handles release\n  checks cleanly.\nlicense: Apache-2.0\ncustom-tag: \"hello: world\"\n---\n\n# My Skill\n\nDo the thing.\n";
        let (parsed, report) = validate_skill_md(md, Some("my-skill"));
        assert!(report.is_valid(), "issues: {:?}", report.issues);
        let parsed = parsed.unwrap();
        assert_eq!(parsed.name, "my-skill");
        assert_eq!(parsed.description, "Handles release checks cleanly.");
        assert_eq!(
            parsed.extra_metadata.get("license").map(String::as_str),
            Some("Apache-2.0")
        );
        assert_eq!(
            parsed.extra_metadata.get("custom-tag").map(String::as_str),
            Some("hello: world")
        );
        assert!(parsed.body.starts_with("# My Skill"));

        let rendered = render_skill_md(
            &parsed.name,
            &parsed.description,
            &parsed.extra_metadata,
            &parsed.body,
        );
        let (reparsed, report2) = validate_skill_md(&rendered, Some("my-skill"));
        assert!(report2.is_valid());
        let reparsed = reparsed.unwrap();
        assert_eq!(reparsed.name, parsed.name);
        assert_eq!(reparsed.description, parsed.description);
        assert_eq!(reparsed.extra_metadata, parsed.extra_metadata);
    }

    #[test]
    fn rejects_dir_mismatch_and_traversal() {
        let md = "---\nname: alpha\ndescription: test\n---\nSee [secret](../../etc/passwd)\n";
        let (_, report) = validate_skill_md(md, Some("beta"));
        assert!(!report.is_valid());
        assert!(report.issues.iter().any(|i| i.code == "dir_name_mismatch"));
        assert!(report
            .issues
            .iter()
            .any(|i| i.code == "path_traversal_reference"));
        assert!(validate_relative_skill_path("../outside.sh").is_err());
        assert!(validate_relative_skill_path("/abs/path.sh").is_err());
        assert!(validate_relative_skill_path("scripts/check.sh").is_ok());
    }

    #[test]
    fn sha256_matches_standard_vector() {
        let mut h = Sha256::new();
        h.update(b"abc");
        assert_eq!(
            h.finalize_hex(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
