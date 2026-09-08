//! Immutable, content-addressed experiment records. Imports are data, never state restoration.
use crate::{
    config::Config,
    state::{State, now},
};
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use regex::Regex;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    sync::LazyLock,
    time::Duration,
};

pub const SCHEMA: u32 = 1;
pub const MAX_BUNDLE: usize = 64 * 1024 * 1024;
const MAX_FILE: usize = 1024 * 1024;
const MAX_WORKSPACE: usize = 16 * 1024 * 1024;
const MAX_FILES: usize = 2000;

pub fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn valid_hash(id: &str) -> bool {
    id.len() == 64
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub sha256: String,
    pub bytes: usize,
    pub redacted: bool,
    #[serde(default)]
    pub executable: bool,
}

pub struct Capture<'a> {
    pub config: &'a Config,
    pub state: &'a State,
    pub kind: &'a str,
    pub label: &'a str,
    pub parent: Option<String>,
    pub request: Value,
    pub transcript: Option<Value>,
    pub versions: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema_version: u32,
    pub kind: String,
    pub created_at: Option<u64>,
    pub label: String,
    pub demo: bool,
    pub parent: Option<String>,
    pub recipe: Option<String>,
    pub artifacts: BTreeMap<String, Artifact>,
    pub omissions: Vec<String>,
    pub metadata: Value,
}

impl Manifest {
    fn new(kind: &str, label: &str, demo: bool) -> Self {
        Self {
            schema_version: SCHEMA,
            kind: kind.into(),
            created_at: if kind == "recipe" { None } else { Some(now()) },
            label: redact(label).0,
            demo,
            parent: None,
            recipe: None,
            artifacts: BTreeMap::new(),
            omissions: vec![],
            metadata: json!({}),
        }
    }
    fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == SCHEMA,
            "Unsupported snapshot schema {}",
            self.schema_version
        );
        ensure!(
            [
                "recipe",
                "manual",
                "input",
                "result",
                "candidate",
                "evaluation"
            ]
            .contains(&self.kind.as_str()),
            "Unsupported snapshot kind"
        );
        ensure!(
            self.label.len() <= 1024
                && self.artifacts.len() <= MAX_FILES + 20
                && self.omissions.len() <= MAX_FILES + 20,
            "Manifest limits exceeded"
        );
        for id in [&self.parent, &self.recipe].into_iter().flatten() {
            ensure!(valid_hash(id), "Invalid manifest reference");
        }
        for (name, artifact) in &self.artifacts {
            ensure!(
                !name.is_empty()
                    && !name.contains('\\')
                    && Path::new(name)
                        .components()
                        .all(|c| matches!(c, Component::Normal(_))),
                "Unsafe artifact path"
            );
            ensure!(
                valid_hash(&artifact.sha256) && artifact.bytes <= MAX_WORKSPACE,
                "Invalid artifact reference"
            );
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bundle {
    pub schema_version: u32,
    pub root: String,
    pub manifests: BTreeMap<String, Manifest>,
    pub objects: BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecipeEdit {
    pub label: String,
    pub config: Value,
    pub manager_instructions: String,
    pub worker_instructions: String,
    #[serde(default)]
    pub meeting_instructions: Option<String>,
}

pub struct Archive {
    root: PathBuf,
    conn: Connection,
}

impl Archive {
    pub fn open(root: &Path) -> Result<Self> {
        fs::create_dir_all(root.join("objects"))?;
        fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
        let conn = Connection::open(root.join("index.sqlite"))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS manifests (id TEXT PRIMARY KEY, schema_version INTEGER NOT NULL, kind TEXT NOT NULL, created_at INTEGER, parent TEXT, recipe TEXT, label TEXT NOT NULL, demo INTEGER NOT NULL, json TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS manifests_kind_time ON manifests(kind, created_at);
            CREATE INDEX IF NOT EXISTS manifests_parent ON manifests(parent);
            CREATE INDEX IF NOT EXISTS manifests_recipe ON manifests(recipe);
            CREATE TABLE IF NOT EXISTS objects (sha256 TEXT PRIMARY KEY, bytes INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS artifact_refs (manifest_id TEXT NOT NULL, name TEXT NOT NULL, sha256 TEXT NOT NULL, PRIMARY KEY(manifest_id,name));")?;
        Ok(Self {
            root: root.to_owned(),
            conn,
        })
    }
    fn object_path(&self, id: &str) -> Result<PathBuf> {
        ensure!(valid_hash(id), "Invalid object hash");
        Ok(self.root.join("objects").join(id))
    }
    fn object(&self, id: &str) -> Result<Vec<u8>> {
        let path = self.object_path(id)?;
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        let mut bytes = Vec::new();
        file.take((MAX_WORKSPACE + 1) as u64)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= MAX_WORKSPACE && hash(&bytes) == id,
            "Content hash mismatch: {id}"
        );
        Ok(bytes)
    }
    fn put_object(&self, bytes: &[u8]) -> Result<Artifact> {
        ensure!(bytes.len() <= MAX_WORKSPACE, "Artifact too large");
        let id = hash(bytes);
        let path = self.object_path(&id)?;
        if path.exists() {
            ensure!(self.object(&id)? == bytes, "Existing object differs");
        } else {
            let mut temp = tempfile::NamedTempFile::new_in(self.root.join("objects"))?;
            temp.write_all(bytes)?;
            temp.as_file().sync_all()?;
            if let Err(error) = temp.persist_noclobber(&path) {
                if error.error.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(error.error.into());
                }
                ensure!(self.object(&id)? == bytes, "Concurrent object mismatch");
            }
            fs::File::open(self.root.join("objects"))?.sync_all()?;
        }
        Ok(Artifact {
            sha256: id,
            bytes: bytes.len(),
            redacted: false,
            executable: false,
        })
    }
    fn text(&self, value: &str) -> Result<Artifact> {
        let (text, changed) = redact(value);
        let mut artifact = self.put_object(text.as_bytes())?;
        artifact.redacted = changed;
        Ok(artifact)
    }
    fn json(&self, value: &impl Serialize) -> Result<Artifact> {
        let mut value = serde_json::to_value(value)?;
        let changed = redact_value(&mut value);
        let mut artifact = self.put_object(&serde_json::to_vec_pretty(&value)?)?;
        artifact.redacted = changed;
        Ok(artifact)
    }
    fn index(&mut self, manifest: &Manifest) -> Result<String> {
        manifest.validate()?;
        for reference in [&manifest.parent, &manifest.recipe].into_iter().flatten() {
            self.get(reference)?;
        }
        if let Some(recipe) = &manifest.recipe {
            ensure!(
                self.get(recipe)?.kind == "recipe",
                "Recipe reference is not a recipe"
            );
        }
        let bytes = serde_json::to_vec(manifest)?;
        let id = hash(&bytes);
        for artifact in manifest.artifacts.values() {
            ensure!(
                self.object(&artifact.sha256)?.len() == artifact.bytes,
                "Artifact length mismatch"
            );
        }
        self.put_object(&bytes)?; // Manifest itself is also an immutable object.
        let tx = self.conn.transaction()?;
        tx.execute("INSERT OR IGNORE INTO manifests(id,schema_version,kind,created_at,parent,recipe,label,demo,json) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)", params![id,manifest.schema_version,manifest.kind,manifest.created_at,manifest.parent,manifest.recipe,manifest.label,manifest.demo,String::from_utf8(bytes)?])?;
        for (name, artifact) in &manifest.artifacts {
            tx.execute(
                "INSERT OR IGNORE INTO objects(sha256,bytes) VALUES(?1,?2)",
                params![artifact.sha256, artifact.bytes],
            )?;
            tx.execute(
                "INSERT OR IGNORE INTO artifact_refs(manifest_id,name,sha256) VALUES(?1,?2,?3)",
                params![id, name, artifact.sha256],
            )?;
        }
        tx.commit()?;
        Ok(id)
    }
    pub fn get(&self, id: &str) -> Result<Manifest> {
        ensure!(valid_hash(id), "Invalid snapshot ID");
        let raw: String = self
            .conn
            .query_row("SELECT json FROM manifests WHERE id=?1", [id], |r| r.get(0))
            .optional()?
            .context("Snapshot not found")?;
        ensure!(
            hash(raw.as_bytes()) == id && self.object(id)? == raw.as_bytes(),
            "Manifest integrity check failed"
        );
        let manifest: Manifest = serde_json::from_str(&raw)?;
        manifest.validate()?;
        Ok(manifest)
    }
    pub fn list(&self) -> Result<Vec<Value>> {
        let mut query = self.conn.prepare("SELECT id,kind,created_at,parent,recipe,label,demo FROM manifests ORDER BY rowid DESC LIMIT 200")?;
        let rows = query.query_map([], |r| Ok(json!({"id":r.get::<_,String>(0)?,"kind":r.get::<_,String>(1)?,"created_at":r.get::<_,Option<u64>>(2)?,"parent":r.get::<_,Option<String>>(3)?,"recipe":r.get::<_,Option<String>>(4)?,"label":r.get::<_,String>(5)?,"demo":r.get::<_,bool>(6)?})))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }
    pub fn recipe_editor(&self, id: &str) -> Result<Value> {
        let manifest = self.get(id)?;
        let recipe = if manifest.kind == "recipe" {
            manifest
        } else {
            self.get(manifest.recipe.as_ref().context("Snapshot has no recipe")?)?
        };
        let read = |name: &str| -> Result<String> {
            String::from_utf8(
                self.object(
                    &recipe
                        .artifacts
                        .get(name)
                        .context("Recipe artifact missing")?
                        .sha256,
                )?,
            )
            .context("Recipe text is not UTF-8")
        };
        Ok(
            json!({"label":"Candidate recipe","config":serde_json::from_str::<Value>(&read("config.json")?)?,"manager_instructions":read("manager.md")?,"worker_instructions":read("worker.md")?,"meeting_instructions":if recipe.artifacts.contains_key("meeting.md") { Some(read("meeting.md")?) } else { None }}),
        )
    }
    pub fn preview(&self, id: &str, name: &str) -> Result<Value> {
        let manifest = self.get(id)?;
        let artifact = manifest
            .artifacts
            .get(name)
            .context("Artifact not found in snapshot")?;
        let bytes = self.object(&artifact.sha256)?;
        let limit = bytes.len().min(64 * 1024);
        let (encoding, content) = match std::str::from_utf8(&bytes) {
            Ok(text) => {
                let mut end = limit;
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                ("utf-8", text[..end].to_string())
            }
            Err(_) => ("base64", STANDARD.encode(&bytes[..limit])),
        };
        Ok(
            json!({"encoding":encoding,"content":content,"truncated":limit < bytes.len(),"artifact":artifact}),
        )
    }
    pub fn diff(&self, id: &str, against: Option<String>) -> Result<Value> {
        let current = self.get(id)?;
        let baseline_id = against
            .or_else(|| current.parent.clone())
            .context("Choose a baseline snapshot")?;
        let baseline = self.get(&baseline_id)?;
        let files = |manifest: &Manifest| -> Result<BTreeMap<String, Artifact>> {
            let mut files = manifest.artifacts.clone();
            if let Some(recipe) = &manifest.recipe {
                for (name, artifact) in self.get(recipe)?.artifacts {
                    files.insert(format!("recipe/{name}"), artifact);
                }
            }
            Ok(files)
        };
        let left = files(&baseline)?;
        let right = files(&current)?;
        let names: BTreeSet<_> = left.keys().chain(right.keys()).cloned().collect();
        let changes: Vec<_> = names.into_iter().filter_map(|name| {
            let before = left.get(&name); let after = right.get(&name);
            if before == after { None } else { Some(json!({"path":name,"change":if before.is_none(){"added"}else if after.is_none(){"removed"}else{"changed"},"before":before,"after":after})) }
        }).collect();
        Ok(
            json!({"baseline":baseline_id,"current":id,"recipe_changed":baseline.recipe != current.recipe,"changes":changes}),
        )
    }
    pub fn capture(&mut self, capture: Capture<'_>) -> Result<String> {
        let Capture {
            config,
            state,
            kind,
            label,
            parent,
            request,
            transcript,
            versions,
        } = capture;
        ensure!(
            ["manual", "input", "result"].contains(&kind),
            "Invalid capture kind"
        );
        let mut recipe = Manifest::new("recipe", "Firm recipe", state.demo);
        recipe
            .artifacts
            .insert("config.json".into(), self.json(config)?);
        recipe.artifacts.insert(
            "manager.md".into(),
            self.text(include_str!("../prompts/manager.md"))?,
        );
        recipe.artifacts.insert(
            "worker.md".into(),
            self.text(include_str!("../prompts/worker.md"))?,
        );
        recipe.artifacts.insert(
            "meeting.md".into(),
            self.text(include_str!("../prompts/meeting.md"))?,
        );
        recipe.metadata = json!({"manager_provider":state.manager_provider,"firm_version":env!("CARGO_PKG_VERSION"),"controller_source_sha256":hash(concat!(include_str!("main.rs"),include_str!("app.rs"),include_str!("worker.rs"),include_str!("codex.rs"),include_str!("config.rs"),include_str!("state.rs"),include_str!("snapshots.rs"),include_str!("web.rs"),include_str!("meetings.rs")).as_bytes()),"dependency_lock_sha256":hash(include_bytes!("../Cargo.lock"))});
        let recipe_id = self.index(&recipe)?;
        let mut manifest = Manifest::new(kind, label, state.demo);
        manifest.parent = parent;
        manifest.recipe = Some(recipe_id);
        manifest
            .artifacts
            .insert("context/state.json".into(), self.json(state)?);
        manifest
            .artifacts
            .insert("context/request.json".into(), self.json(&request)?);
        if let Some(transcript) = transcript {
            manifest
                .artifacts
                .insert("context/conversation.json".into(), self.json(&transcript)?);
        } else {
            manifest.omissions.push("Conversation history not captured: no readable manager thread, unavailable app-server, or demo mode".into());
        }
        manifest.omissions.push("Provider-internal state, implicit CLI context outside the workspace, environment variables, credentials, and live processes are not captured".into());
        manifest.omissions.push("Controller state retains at most 300 recent events and worker output at most 48 KiB per stream; workers' internal conversations are not exposed by the CLI text adapters".into());
        let root = config.workspace.canonicalize()?;
        let excluded_root = config
            .state_dir
            .canonicalize()
            .unwrap_or_else(|_| config.state_dir.clone());
        let mut budget = CaptureBudget { bytes: 0, files: 0 };
        self.walk(&root, &root, &excluded_root, &mut manifest, &mut budget)?;
        manifest.metadata = json!({"workspace_root":root,"workspace_bytes":budget.bytes,"workspace_files":budget.files,"workspace_capture":"file contents at capture time; not an atomic filesystem snapshot","limits":{"max_file_bytes":MAX_FILE,"max_workspace_bytes":MAX_WORKSPACE,"max_files":MAX_FILES},"versions":versions,"git_head":git_head(&root),"restores_usage_counters":false});
        self.index(&manifest)
    }
    fn walk(
        &self,
        root: &Path,
        dir: &Path,
        excluded_root: &Path,
        manifest: &mut Manifest,
        budget: &mut CaptureBudget,
    ) -> Result<()> {
        if budget.files >= MAX_FILES {
            return Ok(());
        }
        let mut entries = fs::read_dir(dir)?
            .take(MAX_FILES + 1)
            .collect::<std::io::Result<Vec<_>>>()?;
        if entries.len() > MAX_FILES {
            manifest.omissions.push(format!(
                "Directory entry limit: {} (remaining entries omitted)",
                dir.strip_prefix(root)?.display()
            ));
        }
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            if budget.files >= MAX_FILES {
                manifest
                    .omissions
                    .push("Workspace file count limit reached; remaining paths omitted".into());
                break;
            }
            let path = entry.path();
            let relative = path.strip_prefix(root)?;
            let Some(display) = relative.to_str().map(str::to_owned) else {
                manifest
                    .omissions
                    .push("Non-UTF-8 workspace path omitted".into());
                continue;
            };
            let kind = entry.file_type()?;
            if path.starts_with(excluded_root)
                || excluded(&entry.file_name().to_string_lossy())
                || kind.is_symlink()
            {
                if manifest.omissions.len() < MAX_FILES {
                    manifest.omissions.push(format!(
                        "Excluded: {display} (generated/state/credential path or symlink)"
                    ));
                }
                continue;
            }
            if kind.is_dir() {
                if relative.components().count() > 64 {
                    manifest
                        .omissions
                        .push(format!("Directory depth limit: {display}"));
                    continue;
                }
                self.walk(root, &path, excluded_root, manifest, budget)?;
                continue;
            }
            if !kind.is_file() {
                manifest
                    .omissions
                    .push(format!("Excluded special file: {display}"));
                continue;
            }
            budget.files += 1;
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&path)?;
            let before = file.metadata()?;
            if before.len() > MAX_FILE as u64
                || budget.bytes + before.len() as usize > MAX_WORKSPACE
            {
                manifest.omissions.push(format!("Size limit: {display}"));
                continue;
            }
            let mut bytes = Vec::new();
            file.take((MAX_FILE + 1) as u64).read_to_end(&mut bytes)?;
            if bytes.len() > MAX_FILE || budget.bytes + bytes.len() > MAX_WORKSPACE {
                manifest
                    .omissions
                    .push(format!("Changed/oversized while reading: {display}"));
                continue;
            }
            budget.bytes += bytes.len();
            let mut artifact = if let Ok(text) = std::str::from_utf8(&bytes) {
                self.text(text)?
            } else {
                self.put_object(&bytes)?
            };
            artifact.executable = before.permissions().mode() & 0o111 != 0;
            if artifact.redacted {
                manifest
                    .omissions
                    .push(format!("Credential-like text redacted: {display}"));
            }
            manifest
                .artifacts
                .insert(format!("workspace/{display}"), artifact);
        }
        Ok(())
    }
    pub fn fork(&mut self, id: &str, edit: RecipeEdit) -> Result<String> {
        ensure!(
            !edit.label.trim().is_empty()
                && edit.label.len() <= 1024
                && edit.manager_instructions.len() <= MAX_FILE
                && edit.worker_instructions.len() <= MAX_FILE,
            "Invalid recipe edit"
        );
        ensure!(
            edit.meeting_instructions
                .as_ref()
                .is_none_or(|s| s.len() <= MAX_FILE),
            "Meeting instructions too large"
        );
        let source = self.get(id)?;
        let parent_recipe = if source.kind == "recipe" {
            id.to_owned()
        } else {
            source.recipe.clone().context("Source has no recipe")?
        };
        // Validate the typed recipe without touching its machine-specific paths or executing it.
        let mut config: Config =
            serde_json::from_value(edit.config.clone()).context("Invalid recipe configuration")?;
        config.allowances.validate()?;
        config.normalize_providers()?;
        let mut recipe = Manifest::new("recipe", &edit.label, source.demo);
        recipe.parent = Some(parent_recipe.clone());
        recipe
            .artifacts
            .insert("config.json".into(), self.json(&edit.config)?);
        recipe
            .artifacts
            .insert("manager.md".into(), self.text(&edit.manager_instructions)?);
        recipe
            .artifacts
            .insert("worker.md".into(), self.text(&edit.worker_instructions)?);
        if let Some(instructions) = edit.meeting_instructions {
            recipe
                .artifacts
                .insert("meeting.md".into(), self.text(&instructions)?);
        } else if let Some(artifact) = self.get(&parent_recipe)?.artifacts.get("meeting.md") {
            recipe
                .artifacts
                .insert("meeting.md".into(), artifact.clone());
        }
        recipe.metadata = json!({"status":"candidate; not activated","change_origin":"manual"});
        let recipe_id = self.index(&recipe)?;
        let mut fork = source;
        fork.kind = "candidate".into();
        fork.created_at = Some(now());
        fork.label = redact(&edit.label).0;
        fork.parent = Some(id.into());
        fork.recipe = Some(recipe_id);
        fork.metadata = json!({"status":"candidate; not executed","source":id,"context_semantics":"inherited baseline evidence; edited recipe has not produced a new result"});
        self.index(&fork)
    }
    pub fn evaluate(&mut self, id: &str, note: &str, verdict: &str) -> Result<String> {
        ensure!(
            !note.trim().is_empty()
                && note.len() <= 16000
                && ["better", "worse", "inconclusive"].contains(&verdict),
            "Invalid evaluation"
        );
        let source = self.get(id)?;
        let mut manifest = Manifest::new("evaluation", "Evaluation note", source.demo);
        manifest.parent = Some(id.into());
        manifest.recipe = source.recipe;
        manifest
            .artifacts
            .insert("evaluation.md".into(), self.text(note)?);
        manifest.metadata = json!({"evaluator":"operator","evaluator_version":"manual-note/v1","verdict":verdict,"automatic_promotion":false});
        self.index(&manifest)
    }
    pub fn export(&self, id: &str) -> Result<Bundle> {
        let mut bundle = Bundle {
            schema_version: SCHEMA,
            root: id.into(),
            manifests: BTreeMap::new(),
            objects: BTreeMap::new(),
        };
        let mut pending = vec![id.to_owned()];
        let mut total = 0;
        while let Some(id) = pending.pop() {
            if bundle.manifests.contains_key(&id) {
                continue;
            }
            ensure!(bundle.manifests.len() < 1000, "Export lineage too large");
            let manifest = self.get(&id)?;
            for artifact in manifest.artifacts.values() {
                if !bundle.objects.contains_key(&artifact.sha256) {
                    let bytes = self.object(&artifact.sha256)?;
                    total += bytes.len();
                    ensure!(
                        total <= MAX_BUNDLE / 2,
                        "Export exceeds portable bundle limit"
                    );
                    bundle
                        .objects
                        .insert(artifact.sha256.clone(), STANDARD.encode(bytes));
                }
            }
            for reference in [&manifest.parent, &manifest.recipe].into_iter().flatten() {
                pending.push(reference.clone());
            }
            bundle.manifests.insert(id, manifest);
        }
        ensure!(
            serde_json::to_vec(&bundle)?.len() <= MAX_BUNDLE,
            "Export exceeds JSON bundle limit"
        );
        Ok(bundle)
    }
    pub fn import(&mut self, bundle: Bundle) -> Result<String> {
        ensure!(
            bundle.schema_version == SCHEMA
                && bundle.manifests.contains_key(&bundle.root)
                && bundle.manifests.len() <= 1000,
            "Invalid bundle root/version/size"
        );
        let mut decoded = BTreeMap::new();
        let mut total = 0;
        for (id, value) in &bundle.objects {
            ensure!(valid_hash(id), "Invalid object ID");
            let bytes = STANDARD.decode(value)?;
            total += bytes.len();
            ensure!(
                bytes.len() <= MAX_WORKSPACE && total <= MAX_BUNDLE / 2 && hash(&bytes) == *id,
                "Bundle content hash or size mismatch"
            );
            decoded.insert(id.clone(), bytes);
        }
        for (id, manifest) in &bundle.manifests {
            manifest.validate()?;
            ensure!(
                hash(&serde_json::to_vec(manifest)?) == *id,
                "Manifest hash mismatch"
            );
            for reference in [&manifest.parent, &manifest.recipe].into_iter().flatten() {
                ensure!(
                    bundle.manifests.contains_key(reference),
                    "Incomplete bundle lineage"
                );
            }
            if let Some(recipe) = &manifest.recipe {
                ensure!(
                    bundle.manifests[recipe].kind == "recipe",
                    "Invalid recipe reference"
                );
            }
            for artifact in manifest.artifacts.values() {
                ensure!(
                    decoded
                        .get(&artifact.sha256)
                        .is_some_and(|bytes| bytes.len() == artifact.bytes),
                    "Missing or incorrect bundle artifact"
                );
            }
        }
        // Validate acyclic lineage and reachability before writing anything.
        let mut order = Vec::new();
        let mut visiting = BTreeSet::new();
        let mut visited = BTreeSet::new();
        visit(
            &bundle.root,
            &bundle.manifests,
            &mut visiting,
            &mut visited,
            &mut order,
        )?;
        ensure!(
            visited.len() == bundle.manifests.len(),
            "Bundle contains unrelated manifests"
        );
        let referenced: BTreeSet<_> = bundle
            .manifests
            .values()
            .flat_map(|m| m.artifacts.values().map(|a| a.sha256.clone()))
            .collect();
        ensure!(
            referenced.len() == decoded.len(),
            "Bundle contains unreferenced objects"
        );
        for bytes in decoded.values() {
            self.put_object(bytes)?;
        }
        // One SQLite transaction publishes the whole imported graph; immutable files may be orphaned on failure.
        self.conn.execute_batch("SAVEPOINT bundle_import")?;
        let result = (|| {
            for id in order {
                self.index_import(&id, &bundle.manifests[&id])?;
            }
            Ok::<(), anyhow::Error>(())
        })();
        if let Err(error) = result {
            self.conn
                .execute_batch("ROLLBACK TO bundle_import; RELEASE bundle_import")?;
            return Err(error);
        }
        self.conn.execute_batch("RELEASE bundle_import")?;
        Ok(bundle.root)
    }
    fn index_import(&self, id: &str, manifest: &Manifest) -> Result<()> {
        let bytes = serde_json::to_vec(manifest)?;
        self.put_object(&bytes)?;
        self.conn.execute("INSERT OR IGNORE INTO manifests(id,schema_version,kind,created_at,parent,recipe,label,demo,json) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",params![id,manifest.schema_version,manifest.kind,manifest.created_at,manifest.parent,manifest.recipe,manifest.label,manifest.demo,String::from_utf8(bytes)?])?;
        for (name, a) in &manifest.artifacts {
            self.conn.execute(
                "INSERT OR IGNORE INTO objects(sha256,bytes) VALUES(?1,?2)",
                params![a.sha256, a.bytes],
            )?;
            self.conn.execute(
                "INSERT OR IGNORE INTO artifact_refs(manifest_id,name,sha256) VALUES(?1,?2,?3)",
                params![id, name, a.sha256],
            )?;
        }
        Ok(())
    }
}

struct CaptureBudget {
    bytes: usize,
    files: usize,
}
fn excluded(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    [
        ".git",
        ".firm",
        "target",
        "node_modules",
        ".venv",
        "__pycache__",
        ".ssh",
        ".aws",
        ".config",
        ".codex",
        ".qwen",
        ".grok",
        ".pi",
        ".muse",
        "auth.json",
        "credentials.json",
        "credentials",
        "secrets.json",
    ]
    .contains(&name.as_str())
        || name.starts_with(".env")
        || name.ends_with(".pem")
        || name.ends_with(".key")
        || name.ends_with(".p12")
        || name.ends_with(".pfx")
}
static SECRET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(?:\b(?:[A-Za-z0-9]+_)*(?:api[_-]?key|access[_-]?token|refresh[_-]?token|password|secret)\b\s*[=:]\s*["']?[^\s,"'}]+)|(?:\bBearer\s+[A-Za-z0-9._~+/-]+=*)|(?:\bsk-[A-Za-z0-9_-]{16,})|(?:\bAKIA[A-Z0-9]{16}\b)"#).unwrap()
});
fn redact(text: &str) -> (String, bool) {
    let result = SECRET.replace_all(text, "[REDACTED]");
    let changed = result != text;
    (result.into_owned(), changed)
}
fn redact_value(value: &mut Value) -> bool {
    match value {
        Value::String(text) => {
            let (new, changed) = redact(text);
            *text = new;
            changed
        }
        Value::Array(values) => {
            let mut changed = false;
            for value in values {
                changed |= redact_value(value);
            }
            changed
        }
        Value::Object(values) => {
            let mut changed = false;
            for (key, value) in values {
                if [
                    "api_key",
                    "apikey",
                    "access_token",
                    "refresh_token",
                    "password",
                    "secret",
                    "authorization",
                ]
                .contains(&key.to_ascii_lowercase().as_str())
                {
                    *value = json!("[REDACTED]");
                    changed = true;
                } else {
                    changed |= redact_value(value);
                }
            }
            changed
        }
        _ => false,
    }
}
fn git_head(root: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args([
            "-c",
            "core.fsmonitor=false",
            "rev-parse",
            "--verify",
            "HEAD",
        ])
        .current_dir(root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let head = String::from_utf8(output.stdout).ok()?;
    let head = head.trim();
    if head.len() == 40 || head.len() == 64 {
        Some(head.into())
    } else {
        None
    }
}
fn visit(
    id: &str,
    manifests: &BTreeMap<String, Manifest>,
    visiting: &mut BTreeSet<String>,
    visited: &mut BTreeSet<String>,
    order: &mut Vec<String>,
) -> Result<()> {
    if visited.contains(id) {
        return Ok(());
    }
    ensure!(
        visiting.len() < 100 && visiting.insert(id.into()),
        "Cyclic or overly deep snapshot lineage"
    );
    let manifest = manifests.get(id).context("Missing manifest")?;
    for reference in [&manifest.parent, &manifest.recipe].into_iter().flatten() {
        visit(reference, manifests, visiting, visited, order)?;
    }
    visiting.remove(id);
    visited.insert(id.into());
    order.push(id.into());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (tempfile::TempDir, Config, State, Archive) {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::read(Path::new("firm.toml")).unwrap();
        config.workspace = dir.path().join("project");
        config.state_dir = dir.path().join("state");
        fs::create_dir_all(&config.workspace).unwrap();
        fs::write(config.workspace.join("main.rs"), "fn main() {}\n").unwrap();
        let state = State::new(true, config.allowances.clone());
        let archive = Archive::open(&config.state_dir.join("snapshots")).unwrap();
        (dir, config, state, archive)
    }
    fn capture(
        archive: &mut Archive,
        config: &Config,
        state: &State,
        parent: Option<String>,
    ) -> String {
        archive
            .capture(Capture {
                config,
                state,
                kind: "manual",
                label: "Baseline",
                parent,
                request: json!({"prompt":"Exact supplied input"}),
                transcript: None,
                versions: json!({"firm":"test"}),
            })
            .unwrap()
    }
    #[test]
    fn content_is_immutable_deduplicated_and_portable() {
        let (_dir, config, state, mut archive) = fixture();
        let first = capture(&mut archive, &config, &state, None);
        let original = archive.get(&first).unwrap();
        fs::write(
            config.workspace.join("main.rs"),
            "fn main() { println!(\"changed\"); }\n",
        )
        .unwrap();
        let second = capture(&mut archive, &config, &state, Some(first.clone()));
        let changed = archive.get(&second).unwrap();
        assert_eq!(original.recipe, changed.recipe);
        assert_ne!(
            original.artifacts["workspace/main.rs"],
            changed.artifacts["workspace/main.rs"]
        );
        assert_eq!(
            archive.preview(&first, "workspace/main.rs").unwrap()["content"],
            "fn main() {}\n"
        );
        let target = tempfile::tempdir().unwrap();
        let mut imported = Archive::open(target.path()).unwrap();
        let bundle = archive.export(&second).unwrap();
        let object_count = bundle.objects.len();
        assert_eq!(imported.import(bundle).unwrap(), second);
        assert_eq!(
            imported.export(&second).unwrap().objects.len(),
            object_count
        );
        assert_eq!(
            imported.import(archive.export(&second).unwrap()).unwrap(),
            second
        );
        assert_eq!(imported.get(&second).unwrap().parent, Some(first));
        assert!(!target.path().join("main.rs").exists());
    }
    #[test]
    fn secrets_and_symlinks_are_excluded_with_explicit_coverage() {
        let (_dir, config, state, mut archive) = fixture();
        fs::write(config.workspace.join(".env"), "API_KEY=private-env-value").unwrap();
        fs::write(
            config.workspace.join("settings.txt"),
            "OPENAI_API_KEY=private-setting-value\nnormal=true",
        )
        .unwrap();
        std::os::unix::fs::symlink("/etc/passwd", config.workspace.join("outside")).unwrap();
        let id = capture(&mut archive, &config, &state, None);
        let manifest = archive.get(&id).unwrap();
        assert!(!manifest.artifacts.contains_key("workspace/.env"));
        assert!(!manifest.artifacts.contains_key("workspace/outside"));
        assert!(manifest.artifacts["workspace/settings.txt"].redacted);
        assert!(
            manifest
                .omissions
                .iter()
                .any(|v| v.contains("settings.txt"))
        );
        let bundle = archive.export(&id).unwrap();
        for value in bundle.objects.values() {
            let text = String::from_utf8(STANDARD.decode(value).unwrap()).unwrap();
            assert!(!text.contains("private-env-value") && !text.contains("private-setting-value"));
        }
    }
    #[test]
    fn corrupt_or_unsafe_import_does_not_publish_any_manifest() {
        let (_dir, config, state, mut archive) = fixture();
        let id = capture(&mut archive, &config, &state, None);
        let target = tempfile::tempdir().unwrap();
        let mut imported = Archive::open(target.path()).unwrap();
        let mut bundle = archive.export(&id).unwrap();
        *bundle.objects.values_mut().next().unwrap() = STANDARD.encode(b"tampered");
        assert!(imported.import(bundle).is_err());
        assert!(imported.list().unwrap().is_empty());
        let mut bundle = archive.export(&id).unwrap();
        let mut manifest = bundle.manifests.remove(&id).unwrap();
        let artifact = manifest.artifacts.remove("workspace/main.rs").unwrap();
        manifest.artifacts.insert("../escape".into(), artifact);
        let malicious_id = hash(&serde_json::to_vec(&manifest).unwrap());
        bundle.root = malicious_id.clone();
        bundle.manifests.insert(malicious_id, manifest);
        assert!(imported.import(bundle).is_err());
        assert!(imported.list().unwrap().is_empty());
        let mut bundle = archive.export(&id).unwrap();
        bundle.schema_version = 999;
        assert!(imported.import(bundle).is_err());
        assert!(imported.list().unwrap().is_empty());
    }
    #[test]
    fn recipe_forks_and_evaluations_preserve_the_baseline() {
        let (_dir, config, state, mut archive) = fixture();
        let id = capture(&mut archive, &config, &state, None);
        let baseline = archive.recipe_editor(&id).unwrap();
        let mut edit: RecipeEdit = serde_json::from_value(baseline.clone()).unwrap();
        edit.manager_instructions
            .push_str("\nPrefer exploratory briefs for ambiguous work.");
        edit.config["allowances"]["manager_turns"] = json!(1);
        let fork = archive.fork(&id, edit).unwrap();
        let diff = archive.diff(&fork, None).unwrap();
        assert_eq!(diff["recipe_changed"], true);
        assert!(
            diff["changes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|change| change["path"] == "recipe/manager.md")
        );
        assert_eq!(archive.recipe_editor(&id).unwrap(), baseline);
        assert_eq!(
            archive.recipe_editor(&fork).unwrap()["config"]["allowances"]["manager_turns"],
            1
        );
        assert_eq!(archive.get(&fork).unwrap().parent, Some(id.clone()));
        assert_eq!(state.allowances.manager_turns, 4);
        let evaluation = archive
            .evaluate(&fork, "Needs a comparable live trial", "inconclusive")
            .unwrap();
        let target = tempfile::tempdir().unwrap();
        let mut imported = Archive::open(target.path()).unwrap();
        imported
            .import(archive.export(&evaluation).unwrap())
            .unwrap();
        assert_eq!(imported.get(&evaluation).unwrap().parent, Some(fork));
        assert_eq!(imported.get(&id).unwrap().kind, "manual");
    }
    #[test]
    fn changed_artifacts_are_detected_on_read() {
        let (_dir, config, state, mut archive) = fixture();
        let id = capture(&mut archive, &config, &state, None);
        let manifest = archive.get(&id).unwrap();
        let object = &manifest.artifacts["workspace/main.rs"].sha256;
        fs::write(archive.object_path(object).unwrap(), b"corrupt").unwrap();
        assert!(archive.preview(&id, "workspace/main.rs").is_err());
        assert!(archive.export(&id).is_err());
    }
}
