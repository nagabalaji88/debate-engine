//! Prompt packs: versioned, content-addressed template assets, ARCHITECTURE §15 /
//! INTERFACES §23. "Exact replay is only as reproducible as the prompts, so a
//! prompt pack is a versioned, content-addressed asset — not strings inlined in
//! stage code."
//!
//! ```text
//! prompts/<pack_name>/<version>/
//!   positions.generate.md  claims.extract.md  ...  <stage>.md
//!   manifest.toml
//! ```
//!
//! Neither spec file defines the manifest's own schema or how a template
//! declares its variable schema beyond "each template declares its variable
//! schema" (PLAN_DEVIATIONS.md D19-category gap, logged as D28 for this task).
//! Resolved here as: `manifest.toml` carries only pack identity (`name`,
//! `version`); every `<stage>.md` file is loaded as one template, its stage name
//! taken from the filename (`positions.generate.md` → stage `positions.generate`)
//! so the file list is never duplicated between the manifest and the directory
//! and cannot drift out of sync with it; and each template's variable schema is
//! declared in a TOML front-matter block (`---` fenced) at the top of its own
//! `.md` file, since INTERFACES §23 places the schema on the template, not on
//! the manifest.

use crate::ids::StageName;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// `blake3:`-prefixed, matching ARCHITECTURE §16's convention for hashes in JSON
/// fields. INTERFACES §23 names this type `Hash`; renamed `PromptHash` here
/// because `Hash` collides with `std::hash::Hash`, which several `#[derive(...)]`
/// lists elsewhere in this workspace already bring into scope.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct PromptHash(String);

impl PromptHash {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PromptHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The whole pack's identity — snapshotted into the manifest by `init`
/// (`Manifest::pack_hash`, `arbiter-kernel::store`), and the value `--repack`
/// mismatches against on replay.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct PackHash(String);

impl PackHash {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PackHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A template's declared variable names — "validated before render" (INTERFACES
/// §23): a variable the body references but this schema doesn't declare, or a
/// declared variable the caller doesn't supply at render time, is a stage error,
/// never a silently malformed prompt. `BTreeSet` so two schemas with the same
/// names always canonicalize (and therefore hash) identically regardless of
/// declaration order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VariableSchema(BTreeSet<String>);

impl VariableSchema {
    pub fn new(names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self(names.into_iter().map(Into::into).collect())
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(String::as_str)
    }

    /// Canonical form fed into [`PromptTemplate::prompt_hash`]: a JSON array,
    /// which for a `BTreeSet<String>` serializes in sorted order — deterministic
    /// without a separate sort step.
    fn canonical(&self) -> String {
        serde_json::to_string(&self.0).expect("a BTreeSet<String> always serializes")
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PromptError {
    #[error("loading prompt pack: {0}")]
    Io(#[from] std::io::Error),
    #[error("manifest.toml: {0}")]
    ManifestParse(#[from] toml::de::Error),
    #[error(
        "template '{0}' has no TOML front-matter (`---` ... `---`) declaring its variable schema"
    )]
    MissingFrontMatter(String),
    #[error("template '{stage}' front-matter: {source}")]
    FrontMatterParse {
        stage: String,
        source: toml::de::Error,
    },
    #[error("template '{stage}' body references undeclared variable '{variable}'")]
    UndeclaredPlaceholder { stage: String, variable: String },
    #[error("rendering '{stage}': missing declared variable '{variable}'")]
    MissingVariable { stage: String, variable: String },
    #[error("rendering '{stage}': supplied variable '{variable}' is not declared")]
    UndeclaredVariable { stage: String, variable: String },
    #[error(
        "prompt pack mismatch: this run was recorded under pack hash {recorded}, but the \
         loaded pack hashes to {loaded} — replay refuses a differing pack_hash (INTERFACES §23); \
         use --repack to mint a new run under the new pack"
    )]
    PackMismatch { recorded: String, loaded: String },
}

/// One `<stage>.md` file: its declared schema and unrendered body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptTemplate {
    pub stage: StageName,
    pub body: String,
    pub variables: VariableSchema,
}

impl PromptTemplate {
    /// Substitutes every `{{name}}` placeholder in `body` with `vars[name]`.
    /// Every placeholder in the body must be declared, and every declared
    /// variable must be supplied — an exact match, not a superset in either
    /// direction, since a stray undeclared placeholder or an unused supplied
    /// variable both mean the schema and the template have drifted apart.
    pub fn render(&self, vars: &BTreeMap<String, String>) -> Result<String, PromptError> {
        let declared: BTreeSet<&str> = self.variables.names().collect();
        let supplied: BTreeSet<&str> = vars.keys().map(String::as_str).collect();

        if let Some(name) = supplied.difference(&declared).next() {
            return Err(PromptError::UndeclaredVariable {
                stage: self.stage.to_string(),
                variable: (*name).to_string(),
            });
        }
        if let Some(name) = declared.difference(&supplied).next() {
            return Err(PromptError::MissingVariable {
                stage: self.stage.to_string(),
                variable: (*name).to_string(),
            });
        }

        let mut rendered = String::with_capacity(self.body.len());
        let mut rest = self.body.as_str();
        while let Some(start) = rest.find("{{") {
            let Some(end) = rest[start..].find("}}") else {
                rendered.push_str(rest);
                rest = "";
                break;
            };
            let end = start + end;
            rendered.push_str(&rest[..start]);
            let name = rest[start + 2..end].trim();
            if !declared.contains(name) {
                return Err(PromptError::UndeclaredPlaceholder {
                    stage: self.stage.to_string(),
                    variable: name.to_string(),
                });
            }
            // Presence already proven by the exact-match check above.
            rendered.push_str(&vars[name]);
            rest = &rest[end + 2..];
        }
        rendered.push_str(rest);
        Ok(rendered)
    }

    /// INTERFACES §23: `prompt_hash(t, rendered) -> Hash`, `blake3(rendered ‖
    /// variable schema)`. Recorded on every `CALL_STARTED`. The schema is
    /// included, not just the rendered text, so "two prompts that render
    /// identically but declare different variables are different prompts, and
    /// must not share a cache entry" — a NUL byte separates the two halves
    /// (neither a valid template's variable-schema JSON nor typical rendered
    /// text contains one) so no rendered/schema split is ambiguous.
    pub fn prompt_hash(&self, rendered: &str) -> PromptHash {
        let mut buf = Vec::with_capacity(rendered.len() + 32);
        buf.extend_from_slice(rendered.as_bytes());
        buf.push(0);
        buf.extend_from_slice(self.variables.canonical().as_bytes());
        PromptHash(format!("blake3:{}", blake3::hash(&buf).to_hex()))
    }
}

/// A loaded, content-addressed prompt pack — `PromptPack { name, version, hash }`
/// per INTERFACES §23, plus the loaded templates themselves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptPack {
    pub name: String,
    pub version: String,
    pub hash: PackHash,
    templates: BTreeMap<StageName, PromptTemplate>,
}

#[derive(Deserialize)]
struct ManifestToml {
    name: String,
    version: String,
}

#[derive(Deserialize)]
struct FrontMatter {
    #[serde(default)]
    variables: Vec<String>,
}

impl PromptPack {
    pub fn template(&self, stage: &StageName) -> Option<&PromptTemplate> {
        self.templates.get(stage)
    }

    pub fn stages(&self) -> impl Iterator<Item = &StageName> {
        self.templates.keys()
    }

    /// Loads every `<stage>.md` in `dir` plus `manifest.toml`, and computes the
    /// pack hash over all of it — the manifest's identity and, for every
    /// template in stage-name order (so file-system iteration order can never
    /// change the hash), its stage name, body and canonical variable schema.
    pub fn load(dir: &Path) -> Result<Self, PromptError> {
        let manifest_text = std::fs::read_to_string(dir.join("manifest.toml"))?;
        let manifest: ManifestToml = toml::from_str(&manifest_text)?;

        let mut templates = BTreeMap::new();
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let stage = StageName::new(stem);
            let raw = std::fs::read_to_string(&path)?;
            let (front_matter, body) = split_front_matter(&raw)
                .ok_or_else(|| PromptError::MissingFrontMatter(stem.to_string()))?;
            let front_matter: FrontMatter =
                toml::from_str(front_matter).map_err(|source| PromptError::FrontMatterParse {
                    stage: stem.to_string(),
                    source,
                })?;
            templates.insert(
                stage.clone(),
                PromptTemplate {
                    stage,
                    body: body.to_string(),
                    variables: VariableSchema::new(front_matter.variables),
                },
            );
        }

        let mut hash_input = format!("{}\u{1}{}", manifest.name, manifest.version);
        for template in templates.values() {
            hash_input.push('\u{1}');
            hash_input.push_str(template.stage.as_str());
            hash_input.push('\u{1}');
            hash_input.push_str(&template.body);
            hash_input.push('\u{1}');
            hash_input.push_str(&template.variables.canonical());
        }
        let hash = PackHash(format!(
            "blake3:{}",
            blake3::hash(hash_input.as_bytes()).to_hex()
        ));

        Ok(PromptPack {
            name: manifest.name,
            version: manifest.version,
            hash,
            templates,
        })
    }

    /// INTERFACES §23: "exact replay refuses a differing `pack_hash`." Compares
    /// this loaded pack's hash against the one `init` snapshotted into the
    /// manifest at run start (`Manifest::pack_hash`) — detects a mismatch, never
    /// repairs one, the same posture [`crate`]'s hash-chain verification takes.
    pub fn verify_pack_hash(&self, recorded_pack_hash: &str) -> Result<(), PromptError> {
        if self.hash.as_str() != recorded_pack_hash {
            return Err(PromptError::PackMismatch {
                recorded: recorded_pack_hash.to_string(),
                loaded: self.hash.as_str().to_string(),
            });
        }
        Ok(())
    }
}

/// Splits a leading `---\n ... \n---\n` front-matter block from the rest of the
/// body. `None` if the file doesn't open with one.
fn split_front_matter(raw: &str) -> Option<(&str, &str)> {
    let rest = raw
        .strip_prefix("---\n")
        .or_else(|| raw.strip_prefix("---\r\n"))?;
    let end = rest.find("\n---").or_else(|| rest.find("\r\n---"))?;
    let front_matter = &rest[..end];
    let after_fence = &rest[end..];
    let after_fence = after_fence
        .strip_prefix("\n---")
        .or_else(|| after_fence.strip_prefix("\r\n---"))
        .unwrap_or(after_fence);
    let body = after_fence
        .strip_prefix("\r\n")
        .or_else(|| after_fence.strip_prefix('\n'))
        .unwrap_or(after_fence);
    Some((front_matter, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_pack_dir(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "arbiter_prompt_test_{label}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(dir: &Path, name: &str, contents: &str) {
        std::fs::write(dir.join(name), contents).unwrap();
    }

    fn write_manifest(dir: &Path, name: &str, version: &str) {
        write(
            dir,
            "manifest.toml",
            &format!("name = \"{name}\"\nversion = \"{version}\"\n"),
        );
    }

    #[test]
    fn loading_reads_every_md_file_as_a_stage() {
        let dir = temp_pack_dir("load_stages");
        write_manifest(&dir, "default", "v1");
        write(
            &dir,
            "positions.generate.md",
            "---\nvariables = [\"question\"]\n---\nAnswer: {{question}}\n",
        );
        write(
            &dir,
            "judge.evaluate.md",
            "---\nvariables = []\n---\nEvaluate anonymised positions.\n",
        );

        let pack = PromptPack::load(&dir).unwrap();
        assert_eq!(pack.name, "default");
        assert_eq!(pack.version, "v1");
        assert!(
            pack.template(&StageName::new("positions.generate"))
                .is_some()
        );
        assert!(pack.template(&StageName::new("judge.evaluate")).is_some());
        assert_eq!(pack.stages().count(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_template_with_no_front_matter_fails_to_load() {
        let dir = temp_pack_dir("missing_front_matter");
        write_manifest(&dir, "default", "v1");
        write(&dir, "claims.extract.md", "no front matter here at all\n");

        let result = PromptPack::load(&dir);
        assert!(matches!(result, Err(PromptError::MissingFrontMatter(_))));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn render_substitutes_every_declared_placeholder() {
        let template = PromptTemplate {
            stage: StageName::new("positions.generate"),
            body: "Question: {{question}}\nPanelist: {{model}}\n".to_string(),
            variables: VariableSchema::new(["question", "model"]),
        };
        let mut vars = BTreeMap::new();
        vars.insert("question".to_string(), "Should we ship?".to_string());
        vars.insert("model".to_string(), "gpt-x".to_string());

        let rendered = template.render(&vars).unwrap();
        assert_eq!(rendered, "Question: Should we ship?\nPanelist: gpt-x\n");
    }

    #[test]
    fn a_missing_declared_variable_is_a_stage_error() {
        let template = PromptTemplate {
            stage: StageName::new("positions.generate"),
            body: "{{question}}".to_string(),
            variables: VariableSchema::new(["question"]),
        };
        let result = template.render(&BTreeMap::new());
        assert!(matches!(
            result,
            Err(PromptError::MissingVariable { variable, .. }) if variable == "question"
        ));
    }

    #[test]
    fn a_supplied_but_undeclared_variable_is_rejected() {
        let template = PromptTemplate {
            stage: StageName::new("judge.evaluate"),
            body: "no placeholders here".to_string(),
            variables: VariableSchema::default(),
        };
        let mut vars = BTreeMap::new();
        vars.insert("extra".to_string(), "value".to_string());

        let result = template.render(&vars);
        assert!(matches!(
            result,
            Err(PromptError::UndeclaredVariable { variable, .. }) if variable == "extra"
        ));
    }

    #[test]
    fn a_body_placeholder_not_in_the_schema_is_rejected() {
        // The schema and the body have drifted: the body references a variable
        // the schema never declared, so it can never be legally supplied.
        let template = PromptTemplate {
            stage: StageName::new("claims.extract"),
            body: "{{typo_name}}".to_string(),
            variables: VariableSchema::new(["typo_name_declared_wrong"]),
        };
        let mut vars = BTreeMap::new();
        vars.insert("typo_name_declared_wrong".to_string(), "x".to_string());

        let result = template.render(&vars);
        assert!(matches!(
            result,
            Err(PromptError::UndeclaredPlaceholder { variable, .. }) if variable == "typo_name"
        ));
    }

    #[test]
    fn prompt_hash_changes_when_only_the_schema_changes() {
        // Same rendered text, but the declared schema differs -- "two prompts
        // that render identically but declare different variables are
        // different prompts, and must not share a cache entry" (INTERFACES §23).
        let a = PromptTemplate {
            stage: StageName::new("claims.extract"),
            body: "static text, no placeholders".to_string(),
            variables: VariableSchema::new(["a"]),
        };
        let b = PromptTemplate {
            stage: StageName::new("claims.extract"),
            body: "static text, no placeholders".to_string(),
            variables: VariableSchema::new(["b"]),
        };
        let rendered = "static text, no placeholders";
        assert_ne!(a.prompt_hash(rendered), b.prompt_hash(rendered));
    }

    #[test]
    fn prompt_hash_is_stable_for_identical_inputs() {
        let template = PromptTemplate {
            stage: StageName::new("claims.extract"),
            body: "{{x}}".to_string(),
            variables: VariableSchema::new(["x"]),
        };
        assert_eq!(
            template.prompt_hash("rendered"),
            template.prompt_hash("rendered")
        );
    }

    #[test]
    fn pack_hash_is_independent_of_file_system_iteration_order() {
        let dir_a = temp_pack_dir("order_a");
        write_manifest(&dir_a, "default", "v1");
        write(&dir_a, "aaa.md", "---\nvariables = []\n---\nbody a\n");
        write(&dir_a, "zzz.md", "---\nvariables = []\n---\nbody z\n");

        let dir_b = temp_pack_dir("order_b");
        write_manifest(&dir_b, "default", "v1");
        write(&dir_b, "zzz.md", "---\nvariables = []\n---\nbody z\n");
        write(&dir_b, "aaa.md", "---\nvariables = []\n---\nbody a\n");

        let pack_a = PromptPack::load(&dir_a).unwrap();
        let pack_b = PromptPack::load(&dir_b).unwrap();
        assert_eq!(pack_a.hash, pack_b.hash);

        let _ = std::fs::remove_dir_all(&dir_a);
        let _ = std::fs::remove_dir_all(&dir_b);
    }

    #[test]
    fn pack_hash_changes_when_a_template_body_changes() {
        let dir_a = temp_pack_dir("content_a");
        write_manifest(&dir_a, "default", "v1");
        write(
            &dir_a,
            "claims.extract.md",
            "---\nvariables = []\n---\nversion one\n",
        );

        let dir_b = temp_pack_dir("content_b");
        write_manifest(&dir_b, "default", "v1");
        write(
            &dir_b,
            "claims.extract.md",
            "---\nvariables = []\n---\nversion two\n",
        );

        let pack_a = PromptPack::load(&dir_a).unwrap();
        let pack_b = PromptPack::load(&dir_b).unwrap();
        assert_ne!(pack_a.hash, pack_b.hash);

        let _ = std::fs::remove_dir_all(&dir_a);
        let _ = std::fs::remove_dir_all(&dir_b);
    }

    #[test]
    fn replay_refuses_a_pack_mismatch() {
        let dir = temp_pack_dir("mismatch");
        write_manifest(&dir, "default", "v1");
        write(
            &dir,
            "claims.extract.md",
            "---\nvariables = []\n---\nbody\n",
        );
        let pack = PromptPack::load(&dir).unwrap();

        let result = pack.verify_pack_hash("blake3:not-the-real-hash");
        assert!(matches!(result, Err(PromptError::PackMismatch { .. })));

        // The matching case: a run recorded under exactly this pack's own hash
        // replays cleanly.
        assert!(pack.verify_pack_hash(pack.hash.as_str()).is_ok());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
