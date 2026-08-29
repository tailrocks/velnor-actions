//! Fleet mise task-graph linter.
//!
//! Each fleet member's CI executes the gates it schedules (declared per class in
//! `fleet/classes.toml` and per member in `fleet/repositories.toml`). Those gates
//! resolve to mise tasks, and mise tasks compose through `depends` and aliases.
//! This crate parses that task DAG from each member's checked-in `mise.toml`
//! (plus `.mise/tasks/*.toml` when present), expands every scheduled gate to its
//! leaf commands, and fails mechanically when the same leaf work would execute
//! more than once, when an aggregate gate re-runs a directly scheduled task,
//! when a CI-reachable task destroys caches, or when an aggregate task hides its
//! task composition behind `mise run` inside its shell command.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use regex::Regex;
use serde::Serialize;
use velnor_actions_generator::RepositoryClass;
use velnor_actions_generator::model::{ClassContract, Gate, Repository};

/// Directory of committed per-repository task-graph snapshots, relative to the
/// velnor-actions root.
pub const TASK_GRAPH_DIR: &str = "fleet/task-graphs";

/// Snapshot file name for a slug: `owner/repo` becomes `owner-repo.json`.
#[must_use]
pub fn snapshot_file_name(slug: &str) -> String {
    format!("{}.json", slug.replace('/', "-"))
}

/// One parsed mise task (inline or from `.mise/tasks/*.toml`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TaskDef {
    pub name: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub depends: Vec<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub cmd: String,
    pub dir: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<String>,
}

/// The parsed task table of one repository: canonical task name -> definition.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskGraph {
    pub tasks: BTreeMap<String, TaskDef>,
}

impl TaskGraph {
    /// Resolve a task or alias name to its canonical task name.
    #[must_use]
    pub fn resolve(&self, name: &str) -> Option<&str> {
        if let Some(task) = self.tasks.get(name) {
            return Some(task.name.as_str());
        }
        self.tasks
            .values()
            .find(|task| task.aliases.iter().any(|alias| alias == name))
            .map(|task| task.name.as_str())
    }
}

/// One mechanical violation in a member's scheduled task graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gate: Option<String>,
    pub message: String,
}

impl Finding {
    fn new(kind: &str, gate: Option<&str>, message: String) -> Finding {
        Finding {
            kind: kind.to_string(),
            gate: gate.map(str::to_string),
            message,
        }
    }
}

/// One leaf identity with the distinct tasks that execute it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReachableLeaf {
    pub identity: String,
    pub tasks: Vec<String>,
}

/// A task as recorded in the snapshot (pure aggregates carry no leaf identity).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TaskSnapshot {
    pub name: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub depends: Vec<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub cmd: String,
    pub dir: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub leaf_identity: Option<String>,
}

/// The full per-repository analysis result; serialized byte-stably into
/// `fleet/task-graphs/<owner>-<repo>.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepoReport {
    pub slug: String,
    pub class: String,
    pub scheduled_gates: Vec<String>,
    pub tasks: Vec<TaskSnapshot>,
    pub reachable_leaves: Vec<ReachableLeaf>,
    pub findings: Vec<Finding>,
}

impl RepoReport {
    /// The canonical snapshot bytes: pretty JSON with a trailing newline.
    #[must_use]
    pub fn snapshot_bytes(&self) -> String {
        let mut body = serde_json::to_string_pretty(self).expect("report serializes");
        body.push('\n');
        body
    }
}

/// Collapse all whitespace runs to single spaces and trim. Two commands that
/// differ only in formatting execute the same work.
#[must_use]
pub fn normalize_command(command: &str) -> String {
    command.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Leaf identity: the normalized command, its working directory, and its
/// declared environment. Two leaves with the same identity execute the same
/// work, so scheduling both is duplication.
#[must_use]
pub fn leaf_identity(task: &TaskDef) -> String {
    identity_of(&normalize_command(&task.cmd), &task.dir, &task.env)
}

fn identity_of(command: &str, dir: &str, env: &BTreeMap<String, String>) -> String {
    let env = env
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(",");
    format!("{command}|dir={dir}|env={env}")
}

fn mise_run_regex() -> Regex {
    Regex::new(r"(?:^|[\s;&()])mise(?:\s+--\S+)*\s+(?:run|task|r|t)\s")
        .expect("opaque-aggregate regex compiles")
}

fn cache_destroying_regexes() -> Vec<Regex> {
    [
        r"rm\s+-rf?\s+[^|;&]*(?:target|node_modules|~/.cargo|\.gradle|\.cache)(?:/|\s|$)",
        r"\bcargo clean\b",
        r"\bgradlew? clean\b",
    ]
    .into_iter()
    .map(|pattern| Regex::new(pattern).expect("cache-destroying regex compiles"))
    .collect()
}

/// Parse one repository's mise task table from `mise.toml` plus every
/// `.mise/tasks/*.toml` file, in deterministic order. Unknown keys inside a task
/// are ignored (mise task files carry descriptions, `usage` specs, and more).
///
/// # Errors
///
/// Returns `Err` for missing files, TOML syntax errors, duplicate task names
/// across files, or non-string task fields.
pub fn parse_task_graph(repo_dir: &Path) -> Result<TaskGraph, String> {
    let mise = repo_dir.join("mise.toml");
    if !mise.is_file() {
        return Err(format!("no mise.toml under {}", repo_dir.display()));
    }
    let mut tasks = BTreeMap::new();
    merge_task_file(&mise, &mut tasks)?;

    let tasks_dir = repo_dir.join(".mise").join("tasks");
    if tasks_dir.is_dir() {
        let mut files: Vec<PathBuf> = std::fs::read_dir(&tasks_dir)
            .map_err(|e| format!("reading {}: {e}", tasks_dir.display()))?
            .filter_map(|entry| {
                let path = entry.ok()?.path();
                (path.extension().is_some_and(|ext| ext == "toml")).then_some(path)
            })
            .collect();
        files.sort();
        for file in files {
            merge_task_file(&file, &mut tasks)?;
        }
    }
    Ok(TaskGraph { tasks })
}

fn merge_task_file(path: &Path, tasks: &mut BTreeMap<String, TaskDef>) -> Result<(), String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let document: serde_json::Value =
        toml::from_str(&text).map_err(|e| format!("parsing {}: {e}", path.display()))?;
    let Some(table) = document.get("tasks").and_then(|value| value.as_object()) else {
        return Ok(());
    };
    for (name, spec) in table {
        let def = parse_task(path, name, spec)?;
        if tasks.insert(name.to_string(), def).is_some() {
            return Err(format!(
                "{}: task {name:?} is declared more than once",
                path.display()
            ));
        }
    }
    Ok(())
}

fn parse_task(path: &Path, name: &str, spec: &serde_json::Value) -> Result<TaskDef, String> {
    let reject =
        |field: &str| format!("{}: task {name:?} has a non-string {field}", path.display());
    let strings = |field: &str, value: &serde_json::Value| -> Result<Vec<String>, String> {
        match value {
            serde_json::Value::String(text) => Ok(vec![text.clone()]),
            serde_json::Value::Array(items) => items
                .iter()
                .map(|item| {
                    if let serde_json::Value::String(text) = item {
                        Ok(text.clone())
                    } else if let Some(task) = item.get("task").and_then(|t| t.as_str()) {
                        Ok(task.to_string())
                    } else {
                        Err(reject(field))
                    }
                })
                .collect(),
            _ => Err(reject(field)),
        }
    };
    let cmd = match spec.get("run") {
        None => String::new(),
        Some(value @ (serde_json::Value::String(_) | serde_json::Value::Array(_))) => {
            strings("run", value)?.join("\n")
        }
        Some(_) => return Err(reject("run")),
    };
    let depends = match spec.get("depends") {
        None => Vec::new(),
        Some(value) => strings("depends", value)?,
    };
    let aliases = match spec.get("aliases") {
        None => Vec::new(),
        Some(value) => strings("aliases", value)?,
    };
    let sources = match spec.get("sources") {
        None => Vec::new(),
        Some(value) => strings("sources", value)?,
    };
    let outputs = match spec.get("outputs") {
        None => Vec::new(),
        Some(value) => strings("outputs", value)?,
    };
    let dir = match spec.get("dir") {
        None => ".".to_string(),
        Some(serde_json::Value::String(text)) => text.clone(),
        Some(_) => return Err(reject("dir")),
    };
    let mut env = BTreeMap::new();
    if let Some(declared) = spec.get("env").and_then(|value| value.as_object()) {
        for (key, value) in declared {
            let text = match value {
                serde_json::Value::String(text) => text.clone(),
                serde_json::Value::Number(number) => number.to_string(),
                serde_json::Value::Bool(flag) => flag.to_string(),
                _ => {
                    return Err(format!(
                        "{}: task {name:?} declares non-scalar env {key}",
                        path.display()
                    ));
                }
            };
            env.insert(key.clone(), text);
        }
    }
    Ok(TaskDef {
        name: name.to_string(),
        aliases,
        depends,
        cmd,
        dir,
        env,
        sources,
        outputs,
    })
}

/// Everything one scheduled gate expands to over the task graph.
struct GateReach {
    /// Every task reachable from this gate (including its entrypoint).
    tasks: BTreeSet<String>,
    /// identity -> (executing task -> chain label) for leaves under this gate.
    leaves: BTreeMap<String, BTreeMap<String, String>>,
    /// The resolved entrypoint task, when the gate command is a mise task.
    entrypoint: Option<String>,
}

/// Expand the member's scheduled gates over the parsed task graph and collect
/// every mechanical violation.
#[must_use]
pub fn analyze(
    slug: &str,
    class: RepositoryClass,
    gates: &[Gate],
    graph: &TaskGraph,
) -> RepoReport {
    let mut findings = Vec::new();
    let destroyers = cache_destroying_regexes();
    let mut reaches: Vec<(&Gate, GateReach)> = Vec::new();
    for gate in gates {
        let mut reach = GateReach {
            tasks: BTreeSet::new(),
            leaves: BTreeMap::new(),
            entrypoint: None,
        };
        match gate.mise_task().as_deref().and_then(|name| graph.resolve(name)) {
            Some(canonical) => {
                reach.entrypoint = Some(canonical.to_string());
                expand(slug, gate, graph, canonical, &mut Vec::new(), &mut reach, &mut findings);
            }
            None => match gate.mise_task() {
                Some(target) => findings.push(Finding::new(
                    "missing-task",
                    Some(&gate.name),
                    format!(
                        "gate command {:?} targets mise task {target:?}, which the repository does not define",
                        gate.command
                    ),
                )),
                None => {
                    // Opaque gate command: a leaf of its own, identified by bytes.
                    // It still executes in CI, so cache destroyers do not get to
                    // hide in raw gate commands.
                    for destroyer in &destroyers {
                        if destroyer.is_match(&gate.command) {
                            findings.push(Finding::new(
                                "cache-destroying",
                                Some(&gate.name),
                                format!(
                                    "gate command {:?} runs a cache-destroying command ({})",
                                    gate.command,
                                    destroyer.as_str()
                                ),
                            ));
                            break;
                        }
                    }
                    let identity =
                        identity_of(&normalize_command(&gate.command), ".", &BTreeMap::new());
                    reach
                        .leaves
                        .entry(identity)
                        .or_default()
                        .insert(format!("gate:{}", gate.name), format!("gate:{}", gate.name));
                }
            },
        }
        reaches.push((gate, reach));
    }

    // (a) duplicate leaf scheduling: one leaf identity executing more than once,
    // whether from more than one scheduled gate or twice under one gate.
    let mut by_identity: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for (gate, reach) in &reaches {
        for (identity, tasks) in &reach.leaves {
            for (task, chain) in tasks {
                by_identity
                    .entry(identity.clone())
                    .or_default()
                    .push((gate.name.clone(), chain.clone()));
                let _ = task;
            }
        }
    }
    for (identity, executions) in &by_identity {
        if executions.len() > 1 {
            let chains = executions
                .iter()
                .map(|(_, chain)| chain.as_str())
                .collect::<Vec<_>>()
                .join("; ");
            findings.push(Finding::new(
                "duplicate-leaf",
                None,
                format!(
                    "leaf {identity:?} executes {} times ({chains})",
                    executions.len()
                ),
            ));
        }
    }

    // (b) aggregate overlap: one gate's task subtree contains a task another
    // gate schedules directly.
    for (outer, outer_reach) in &reaches {
        for (inner, _) in &reaches {
            if inner.name == outer.name {
                continue;
            }
            let Some(inner_entry) = inner.mise_task().as_deref().and_then(|n| graph.resolve(n))
            else {
                continue;
            };
            if outer_reach.tasks.contains(inner_entry) {
                findings.push(Finding::new(
                    "aggregate-overlap",
                    Some(&outer.name),
                    format!(
                        "gate {} expands task {}, which gate {} also schedules directly",
                        outer.name, inner_entry, inner.name
                    ),
                ));
            }
        }
    }

    // (c) cache-destroying commands on CI-reachable tasks only; operator-only
    // tasks are free to clean. Raw gate commands were checked above.
    let mut flagged: BTreeSet<(String, String)> = BTreeSet::new();
    for (gate, reach) in &reaches {
        for task in &reach.tasks {
            if !flagged.insert((gate.name.clone(), task.clone())) {
                continue;
            }
            let Some(def) = graph.tasks.get(task) else {
                continue;
            };
            for destroyer in &destroyers {
                if destroyer.is_match(&def.cmd) {
                    findings.push(Finding::new(
                        "cache-destroying",
                        Some(&gate.name),
                        format!(
                            "CI-reachable task {task:?} runs a cache-destroying command ({})",
                            destroyer.as_str()
                        ),
                    ));
                    break;
                }
            }
        }
    }

    // (e) opaque aggregates: a task that composes other mise tasks inside its
    // shell command instead of `depends` hides duplication from this analysis.
    let opaque = mise_run_regex();
    let mut opaque_flagged: BTreeSet<String> = BTreeSet::new();
    for (gate, reach) in &reaches {
        for task in &reach.tasks {
            if !opaque_flagged.insert(task.clone()) {
                continue;
            }
            let Some(def) = graph.tasks.get(task) else {
                continue;
            };
            if opaque.is_match(&def.cmd) {
                findings.push(Finding::new(
                    "opaque-aggregate",
                    Some(&gate.name),
                    format!(
                        "CI-reachable task {task:?} invokes other mise tasks inside its command instead of using depends; its duplication cannot be expanded"
                    ),
                ));
            }
        }
    }

    let reachable_leaves = by_identity
        .into_iter()
        .map(|(identity, executions)| {
            let mut tasks = executions
                .into_iter()
                .map(|(_, chain)| chain.rsplit('>').next().unwrap_or_default().to_string())
                .collect::<Vec<_>>();
            tasks.sort();
            tasks.dedup();
            ReachableLeaf { identity, tasks }
        })
        .collect::<Vec<_>>();

    let tasks = graph
        .tasks
        .values()
        .map(|def| TaskSnapshot {
            name: def.name.clone(),
            aliases: def.aliases.clone(),
            depends: def.depends.clone(),
            cmd: def.cmd.clone(),
            dir: def.dir.clone(),
            sources: def.sources.clone(),
            outputs: def.outputs.clone(),
            leaf_identity: (!def.cmd.is_empty()).then(|| leaf_identity(def)),
        })
        .collect::<Vec<_>>();

    RepoReport {
        slug: slug.to_string(),
        class: class.code().to_string(),
        scheduled_gates: gates.iter().map(|gate| gate.name.clone()).collect(),
        tasks,
        reachable_leaves,
        findings,
    }
}

/// Depth-first expansion of one task under one gate. A task with `depends`
/// expands into its dependencies; a task with a command contributes its own leaf
/// (mise runs `depends` first, then the task's own `run`). A shared dependency is
/// executed once per gate, so re-reaching it is not duplication.
fn expand(
    slug: &str,
    gate: &Gate,
    graph: &TaskGraph,
    task: &str,
    path: &mut Vec<String>,
    reach: &mut GateReach,
    findings: &mut Vec<Finding>,
) {
    if path.iter().any(|visited| visited == task) {
        findings.push(Finding::new(
            "task-cycle",
            Some(&gate.name),
            format!("{slug}: task cycle {}->{task}", path.join(">")),
        ));
        return;
    }
    if !reach.tasks.insert(task.to_string()) {
        return;
    }
    path.push(task.to_string());
    if let Some(def) = graph.tasks.get(task) {
        if !def.cmd.is_empty() {
            let identity = leaf_identity(def);
            reach.leaves.entry(identity).or_default().insert(
                def.name.clone(),
                format!("{}>{}", gate.name, path.join(">")),
            );
        }
        for dependency in &def.depends {
            match graph.resolve(dependency) {
                Some(canonical) => {
                    expand(slug, gate, graph, canonical, path, reach, findings);
                }
                None => findings.push(Finding::new(
                    "missing-task",
                    Some(&gate.name),
                    format!(
                        "task {task:?} depends on {dependency:?}, which the repository does not define"
                    ),
                )),
            }
        }
    }
    path.pop();
}

/// Greedy proposal: walk the class gates in declaration order and keep each one
/// only if scheduling it alongside the already accepted gates produces zero
/// findings for this member. The result is always a findings-free subset, used
/// to author `gates = [...]` rows in `fleet/repositories.toml`.
#[must_use]
pub fn propose_gates(
    slug: &str,
    class: RepositoryClass,
    contract: &ClassContract,
    graph: &TaskGraph,
) -> Vec<String> {
    let mut accepted: Vec<Gate> = Vec::new();
    for gate in &contract.gates {
        accepted.push(gate.clone());
        if !analyze(slug, class, &accepted, graph).findings.is_empty() {
            accepted.pop();
        }
    }
    accepted.into_iter().map(|gate| gate.name).collect()
}

/// Analyze one fleet member located under `repos_dir`, mapping coverage and
/// parse failures to findings so every member always produces a snapshot.
#[must_use]
pub fn analyze_member(repos_dir: &Path, repository: &Repository, gates: &[Gate]) -> RepoReport {
    let repo_dir = repos_dir.join(repository.name());
    if !repo_dir.is_dir() {
        let mut report = empty_report(repository, gates);
        report.findings.push(Finding::new(
            "missing-repo",
            None,
            format!(
                "fleet member {} is not checked out under {}",
                repository.slug,
                repo_dir.display()
            ),
        ));
        return report;
    }
    match parse_task_graph(&repo_dir) {
        Ok(graph) => analyze(&repository.slug, repository.class, gates, &graph),
        Err(error) => {
            let mut report = empty_report(repository, gates);
            report
                .findings
                .push(Finding::new("parse-error", None, error));
            report
        }
    }
}

fn empty_report(repository: &Repository, gates: &[Gate]) -> RepoReport {
    RepoReport {
        slug: repository.slug.clone(),
        class: repository.class.code().to_string(),
        scheduled_gates: gates.iter().map(|gate| gate.name.clone()).collect(),
        tasks: Vec::new(),
        reachable_leaves: Vec::new(),
        findings: Vec::new(),
    }
}
