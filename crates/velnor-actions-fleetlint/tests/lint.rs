//! Fleet task-graph linter tests: synthetic mise fixtures covering every
//! finding class, alias and `.mise/tasks` expansion, parse and coverage
//! failures, snapshot naming, and the greedy gate proposal.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use velnor_actions_fleetlint::{
    RepoReport, TaskGraph, analyze, analyze_member, normalize_command, parse_task_graph,
    propose_gates, snapshot_file_name,
};
use velnor_actions_generator::RepositoryClass;
use velnor_actions_generator::model::{Applicability, ClassContract, Gate};

fn gate(name: &str, command: &str) -> Gate {
    Gate {
        name: name.to_string(),
        command: command.to_string(),
        applicability: Applicability::Both,
    }
}

fn contract(gates: &[Gate]) -> ClassContract {
    ClassContract {
        class: RepositoryClass::Code,
        gates: gates.to_vec(),
        platform_only: false,
        platform_runner: None,
        platform_name: None,
        platform_timeout_minutes: None,
    }
}

/// A code-class gate list: install is an opaque command leaf, the rest resolve
/// to mise tasks.
fn code_gates() -> Vec<Gate> {
    vec![
        gate("install", "mise install --locked"),
        gate("build", "mise run build"),
        gate("test", "mise run test"),
        gate("lint", "mise run lint"),
        gate("format", "mise run fmt"),
    ]
}

struct TempRepo(PathBuf);

impl TempRepo {
    fn new(tag: &str, mise_toml: &str) -> TempRepo {
        let dir = std::env::temp_dir().join(format!(
            "velnor-actions-fleetlint-test-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("mise.toml"), mise_toml).unwrap();
        TempRepo(dir)
    }

    fn write_task_file(&self, name: &str, body: &str) {
        std::fs::create_dir_all(self.0.join(".mise").join("tasks")).unwrap();
        std::fs::write(self.0.join(".mise").join("tasks").join(name), body).unwrap();
    }

    fn dir(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempRepo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn kinds(report: &RepoReport) -> Vec<&str> {
    report.findings.iter().map(|f| f.kind.as_str()).collect()
}

fn analyze_str(slug: &str, gates: &[Gate], mise_toml: &str) -> RepoReport {
    let graph = parse_task_graph_str(mise_toml);
    analyze(slug, RepositoryClass::Code, gates, &graph)
}

fn parse_task_graph_str(mise_toml: &str) -> TaskGraph {
    let repo = TempRepo::new("parse", mise_toml);
    parse_task_graph(repo.dir()).expect("fixture parses")
}

const CLEAN_MISE: &str = r#"
[tasks.build]
run = "cargo build --workspace --locked"

[tasks.test]
run = "cargo nextest run --workspace --locked"

[tasks.lint]
run = "cargo clippy --workspace --all-targets --locked -- -D warnings"

[tasks.fmt]
run = "cargo fmt --all -- --check"
"#;

#[test]
fn clean_graph_has_no_findings_and_distinct_leaves() {
    let report = analyze_str("tailrocks/example", &code_gates(), CLEAN_MISE);
    assert_eq!(
        report.findings,
        Vec::new(),
        "clean graph: {:#?}",
        report.findings
    );
    assert_eq!(
        report.scheduled_gates,
        ["install", "build", "test", "lint", "format"]
    );
    // Five scheduled gates, five distinct leaves: one opaque install command
    // plus four task leaves.
    assert_eq!(report.reachable_leaves.len(), 5);
    let identities: Vec<&str> = report
        .reachable_leaves
        .iter()
        .map(|leaf| leaf.identity.as_str())
        .collect();
    assert!(identities.contains(&"mise install --locked|dir=.|env="));
    assert!(identities.contains(&"cargo build --workspace --locked|dir=.|env="));
}

#[test]
fn same_leaf_through_two_gates_is_a_duplicate() {
    // `ci` aggregates build+test, and the build gate schedules `ci` while the
    // test gate schedules `test`: cargo nextest runs twice through one gate.
    let mise = r#"
[tasks.ci]
depends = ["build", "test"]

[tasks.build]
run = "cargo build --workspace --locked"

[tasks.test]
run = "cargo nextest run --workspace --locked"

[tasks.lint]
run = "cargo clippy --workspace --all-targets --locked -- -D warnings"

[tasks.fmt]
run = "cargo fmt --all -- --check"
"#;
    let gates = vec![
        gate("install", "mise install --locked"),
        gate("build", "mise run ci"),
        gate("test", "mise run test"),
        gate("lint", "mise run lint"),
        gate("format", "mise run fmt"),
    ];
    let report = analyze_str("tailrocks/example", &gates, mise);
    let duplicates: Vec<_> = report
        .findings
        .iter()
        .filter(|f| f.kind == "duplicate-leaf")
        .collect();
    assert_eq!(duplicates.len(), 1, "{:#?}", report.findings);
    assert!(
        duplicates[0].message.contains("executes 2 times"),
        "{}",
        duplicates[0].message
    );
    assert!(
        duplicates[0].message.contains("build>ci>test"),
        "{}",
        duplicates[0].message
    );
    assert!(
        duplicates[0].message.contains("test>test"),
        "{}",
        duplicates[0].message
    );
    // The aggregate overlap is reported too: the build gate's subtree contains
    // the test gate's entrypoint.
    assert!(kinds(&report).contains(&"aggregate-overlap"));
}

#[test]
fn same_leaf_twice_under_one_gate_is_a_duplicate() {
    // One gate fans into two tasks that run byte-identical commands with
    // different task names.
    let mise = r#"
[tasks.check]
depends = ["check-a", "check-b"]

[tasks.check-a]
run = "cargo clippy --workspace --locked -- -D warnings"

[tasks.check-b]
run = "cargo clippy --workspace   --locked -- -D warnings"
"#;
    let gates = vec![
        gate("install", "mise install --locked"),
        gate("lint", "mise run check"),
    ];
    let report = analyze_str("tailrocks/example", &gates, mise);
    let duplicates: Vec<_> = report
        .findings
        .iter()
        .filter(|f| f.kind == "duplicate-leaf")
        .collect();
    assert_eq!(duplicates.len(), 1, "{:#?}", report.findings);
    assert!(
        duplicates[0].message.contains("executes 2 times"),
        "{}",
        duplicates[0].message
    );
}

#[test]
fn diamond_dependency_within_one_gate_is_not_a_duplicate() {
    // mise dedups a shared dependency inside one run: one gate reaching a
    // diamond executes the shared leaf exactly once.
    let mise = r#"
[tasks.check]
depends = ["lint", "test"]

[tasks.lint]
depends = ["build"]
run = "cargo clippy --workspace --locked -- -D warnings"

[tasks.test]
depends = ["build"]
run = "cargo nextest run --workspace --locked"

[tasks.build]
run = "cargo build --workspace --locked"
"#;
    let gates = vec![
        gate("install", "mise install --locked"),
        gate("test", "mise run check"),
    ];
    let report = analyze_str("tailrocks/example", &gates, mise);
    assert_eq!(report.findings, Vec::new(), "{:#?}", report.findings);
}

#[test]
fn shared_leaf_across_two_gates_is_a_duplicate() {
    // Each gate is its own `mise run` invocation, so the same leaf reached from
    // two separately scheduled gates executes twice.
    let mise = r#"
[tasks.lint]
depends = ["build"]
run = "cargo clippy --workspace --locked -- -D warnings"

[tasks.test]
depends = ["build"]
run = "cargo nextest run --workspace --locked"

[tasks.build]
run = "cargo build --workspace --locked"
"#;
    let gates = vec![
        gate("install", "mise install --locked"),
        gate("test", "mise run test"),
        gate("lint", "mise run lint"),
    ];
    let report = analyze_str("tailrocks/example", &gates, mise);
    let duplicates: Vec<_> = report
        .findings
        .iter()
        .filter(|f| f.kind == "duplicate-leaf")
        .collect();
    assert_eq!(duplicates.len(), 1, "{:#?}", report.findings);
    assert!(
        duplicates[0].message.contains("test>test>build")
            && duplicates[0].message.contains("lint>lint>build"),
        "{}",
        duplicates[0].message
    );
}

#[test]
fn aggregate_containing_directly_scheduled_task_is_overlap() {
    // The check gate aggregates lint, which the lint gate schedules directly —
    // distinct leaf commands, so only aggregate-overlap fires.
    let mise = r#"
[tasks.check]
depends = ["lint", "unit"]

[tasks.lint]
run = "cargo clippy --workspace --locked -- -D warnings"

[tasks.unit]
run = "cargo nextest run --workspace --locked"
"#;
    let gates = vec![
        gate("install", "mise install --locked"),
        gate("test", "mise run check"),
        gate("lint", "mise run lint"),
    ];
    let report = analyze_str("tailrocks/example", &gates, mise);
    let overlaps: Vec<_> = report
        .findings
        .iter()
        .filter(|f| f.kind == "aggregate-overlap")
        .collect();
    assert_eq!(overlaps.len(), 1, "{:#?}", report.findings);
    assert!(
        overlaps[0]
            .message
            .contains("gate test expands task lint, which gate lint also schedules directly"),
        "{}",
        overlaps[0].message
    );
}

#[test]
fn cache_destroying_commands_fail_only_when_ci_reachable() {
    let mise = r#"
[tasks.build]
run = "cargo build --workspace --locked"

[tasks.clean-rebuild]
run = "rm -rf target && cargo build --workspace --locked"

[tasks.fmt]
run = "rm -rf node_modules && prettier --check ."

[tasks.operator-clean]
run = "cargo clean"

[tasks.gradle-clean]
run = "./gradlew clean dist"
"#;
    let gates = vec![
        gate("install", "mise install --locked"),
        gate("build", "mise run build"),
    ];
    let report = analyze_str("tailrocks/example", &gates, mise);
    // build and the two scheduled gates stay clean; the operator-only tasks
    // (clean-rebuild, fmt, operator-clean, gradle-clean) are unreachable.
    assert_eq!(report.findings, Vec::new(), "{:#?}", report.findings);

    let gates = vec![
        gate("install", "mise install --locked"),
        gate("build", "mise run build"),
        gate("format", "mise run fmt"),
        gate("deep-clean", "mise run clean-rebuild"),
        gate("wipe", "cargo clean"),
    ];
    let report = analyze_str("tailrocks/example", &gates, mise);
    let destroying: Vec<_> = report
        .findings
        .iter()
        .filter(|f| f.kind == "cache-destroying")
        .collect();
    assert_eq!(destroying.len(), 3, "{:#?}", report.findings);
    let messages: String = destroying
        .iter()
        .map(|f| f.message.clone())
        .collect::<Vec<_>>()
        .join(" | ");
    assert!(messages.contains("task \"fmt\""), "{messages}");
    assert!(messages.contains("task \"clean-rebuild\""), "{messages}");
    assert!(
        messages.contains("gate command \"cargo clean\""),
        "{messages}"
    );
}

#[test]
fn opaque_aggregate_composing_mise_tasks_fails() {
    let mise = r#"
[tasks.desktop-ci]
run = '''
set -euo pipefail
mise run desktop-lint
cargo xtask desktop build
mise r desktop-test
'''

[tasks.desktop-lint]
run = "cargo clippy --workspace --locked"

[tasks.desktop-test]
run = "cargo nextest run --workspace --locked"
"#;
    let gates = vec![
        gate("install", "mise install --locked"),
        gate("test", "mise run desktop-ci"),
    ];
    let report = analyze_str("jackin-project/example", &gates, mise);
    let opaque: Vec<_> = report
        .findings
        .iter()
        .filter(|f| f.kind == "opaque-aggregate")
        .collect();
    assert_eq!(opaque.len(), 1, "{:#?}", report.findings);
    assert!(
        opaque[0].message.contains("desktop-ci"),
        "{}",
        opaque[0].message
    );
}

#[test]
fn gate_targeting_missing_task_and_broken_dependence_are_reported() {
    let mise = r#"
[tasks.build]
depends = ["toolchain-that-is-gone"]
run = "cargo build --workspace --locked"
"#;
    let gates = vec![
        gate("install", "mise install --locked"),
        gate("build", "mise run build"),
        gate("test", "mise run test"),
    ];
    let report = analyze_str("tailrocks/example", &gates, mise);
    let missing: Vec<_> = report
        .findings
        .iter()
        .filter(|f| f.kind == "missing-task")
        .collect();
    assert_eq!(missing.len(), 2, "{:#?}", report.findings);
    let messages: String = missing.iter().map(|f| format!("[{}]", f.message)).collect();
    assert!(
        messages.contains("gate command \"mise run test\" targets mise task \"test\""),
        "{messages}"
    );
    assert!(
        messages.contains("depends on \"toolchain-that-is-gone\""),
        "{messages}"
    );
}

#[test]
fn task_cycles_are_reported() {
    let mise = r#"
[tasks.a]
depends = ["b"]
run = "echo a"

[tasks.b]
depends = ["a"]
run = "echo b"
"#;
    let gates = vec![gate("test", "mise run a")];
    let report = analyze_str("tailrocks/example", &gates, mise);
    assert!(
        kinds(&report).contains(&"task-cycle"),
        "{:#?}",
        report.findings
    );
}

#[test]
fn aliases_resolve_to_canonical_tasks() {
    let mise = r#"
[tasks.check]
aliases = ["lint", "clippy"]
run = "cargo clippy --workspace --locked -- -D warnings"
"#;
    let gates = vec![gate("lint", "mise run clippy")];
    let report = analyze_str("tailrocks/example", &gates, mise);
    assert_eq!(report.findings, Vec::new(), "{:#?}", report.findings);
    assert_eq!(report.reachable_leaves.len(), 1);
    assert_eq!(
        report.reachable_leaves[0].tasks,
        ["check"],
        "leaf is recorded under the canonical task name"
    );
}

#[test]
fn mise_task_files_merge_and_duplicates_fail() {
    let repo = TempRepo::new(
        "merge",
        "[tasks.build]\nrun = \"cargo build --workspace --locked\"\n",
    );
    repo.write_task_file(
        "lint.toml",
        "[tasks.lint]\nrun = \"cargo clippy --workspace --locked\"\n",
    );
    let graph = parse_task_graph(repo.dir()).expect("merged graph parses");
    assert_eq!(graph.tasks.len(), 2);
    assert_eq!(graph.tasks["lint"].cmd, "cargo clippy --workspace --locked");
    assert_eq!(graph.tasks["build"].dir, ".");

    repo.write_task_file("dup.toml", "[tasks.build]\nrun = \"cargo build\"\n");
    let err = parse_task_graph(repo.dir()).unwrap_err();
    assert!(
        err.contains("task \"build\" is declared more than once"),
        "{err}"
    );
}

#[test]
fn task_directories_and_declared_env_join_leaf_identity() {
    // Same command in a different directory or with different declared env is
    // different work — no duplicate finding.
    let mise = r#"
[tasks.lint-root]
run = "cargo clippy --workspace --locked"

[tasks.lint-crates]
dir = "crates/gui"
run = "cargo clippy --workspace --locked"

[tasks.lint-staged]
env = { RUSTFLAGS = "-D warnings" }
run = "cargo clippy --workspace --locked"

[tasks.check]
depends = ["lint-root", "lint-crates", "lint-staged"]
"#;
    let gates = vec![gate("lint", "mise run check")];
    let report = analyze_str("tailrocks/example", &gates, mise);
    assert_eq!(report.findings, Vec::new(), "{:#?}", report.findings);
    assert_eq!(report.reachable_leaves.len(), 3);
}

#[test]
fn missing_repo_and_parse_error_become_findings() {
    // Coverage: a manifest member whose clone is absent must produce a
    // missing-repo finding, never a silent skip.
    let manifest_dir = TempRepo::new("manifest", "");
    let repos_dir = manifest_dir.dir().join("nowhere");
    let repository = velnor_actions_generator::model::Repository {
        slug: "tailrocks/example".to_string(),
        organization: "tailrocks".to_string(),
        class: RepositoryClass::Code,
        baseline_sha: "0000000000000000000000000000000000000000".to_string(),
        gates: vec!["install".to_string()],
    };
    let report = analyze_member(&repos_dir, &repository, &code_gates());
    assert_eq!(kinds(&report), ["missing-repo"]);
    assert!(
        report.findings[0].message.contains("not checked out"),
        "{}",
        report.findings[0].message
    );

    let broken = TempRepo::new("broken", "[tasks.build\nrun = \"oops\"");
    let repository = velnor_actions_generator::model::Repository {
        slug: "tailrocks/broken".to_string(),
        organization: "tailrocks".to_string(),
        class: RepositoryClass::Code,
        baseline_sha: "0000000000000000000000000000000000000000".to_string(),
        gates: vec!["install".to_string()],
    };
    // The clone directory exists (named after the repo) but its mise.toml is
    // malformed: parse failure becomes a finding, not a crash or a skip.
    let repos_dir = broken.dir().parent().unwrap().join("clones");
    let _ = std::fs::remove_dir_all(&repos_dir);
    std::fs::create_dir_all(repos_dir.join("broken")).unwrap();
    std::fs::copy(
        broken.dir().join("mise.toml"),
        repos_dir.join("broken").join("mise.toml"),
    )
    .unwrap();
    let report = analyze_member(&repos_dir, &repository, &code_gates());
    assert_eq!(kinds(&report), ["parse-error"]);
    let _ = std::fs::remove_dir_all(&repos_dir);
}

#[test]
fn propose_gates_keeps_the_maximal_clean_subset() {
    // lint's entrypoint duplicates the leaf build already executes, so the
    // proposal drops exactly lint and keeps the rest in class order.
    let mise = r#"
[tasks.build]
run = "cargo build --workspace --locked"

[tasks.test]
run = "cargo nextest run --workspace --locked"

[tasks.lint]
depends = ["build"]
run = "cargo build --workspace --locked"

[tasks.fmt]
run = "cargo fmt --all -- --check"
"#;
    let graph = parse_task_graph_str(mise);
    let proposed = propose_gates(
        "tailrocks/example",
        RepositoryClass::Code,
        &contract(&code_gates()),
        &graph,
    );
    assert_eq!(proposed, ["install", "build", "test", "format"]);
}

#[test]
fn snapshot_names_and_command_normalization_are_stable() {
    assert_eq!(
        snapshot_file_name("tailrocks/parallax"),
        "tailrocks-parallax.json"
    );
    assert_eq!(
        normalize_command("  cargo   build\n\t --locked  "),
        "cargo build --locked"
    );
    assert_eq!(normalize_command("cargo\tbuild"), "cargo build");
}

#[test]
fn snapshot_bytes_are_pretty_json_with_trailing_newline() {
    let report = analyze_str("tailrocks/example", &code_gates(), CLEAN_MISE);
    let body = report.snapshot_bytes();
    assert!(body.ends_with("}\n"), "trailing newline");
    assert!(body.contains("\n  \"slug\": \"tailrocks/example\","));
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed["class"], "code");
    assert_eq!(parsed["reachable_leaves"].as_array().map(Vec::len), Some(5));
}

#[test]
fn run_arrays_and_table_dependencies_parse() {
    let mise = r#"
[tasks.check]
depends = [
    { task = "build" },
    "lint",
]
run = [
    "echo first",
    "echo second",
]
"#;
    let repo = TempRepo::new("shapes", mise);
    repo.write_task_file(
        "leaves.toml",
        "[tasks.build]\nrun = \"cargo build\"\n\n[tasks.lint]\nrun = \"cargo clippy\"\n",
    );
    let graph = parse_task_graph(repo.dir()).unwrap();
    assert_eq!(graph.tasks["check"].depends, ["build", "lint"]);
    assert_eq!(graph.tasks["check"].cmd, "echo first\necho second");
    let report = analyze(
        "tailrocks/example",
        RepositoryClass::Code,
        &[gate("test", "mise run check")],
        &graph,
    );
    assert_eq!(report.findings, Vec::new(), "{:#?}", report.findings);
    assert_eq!(report.reachable_leaves.len(), 3);
}

#[test]
fn opaque_gate_commands_are_leaves_of_their_own() {
    // `mise install --locked` is not a task invocation: it counts as one leaf
    // under a pseudo-task named after the gate.
    let report = analyze_str("tailrocks/example", &code_gates(), CLEAN_MISE);
    let install_leaf = report
        .reachable_leaves
        .iter()
        .find(|leaf| leaf.identity == "mise install --locked|dir=.|env=")
        .expect("install gate contributes its own leaf");
    assert_eq!(install_leaf.tasks, ["gate:install"]);
}

// TaskGraph::resolve and leaf identity helpers are exercised above; the
// remaining public surface is byte-level:
#[test]
fn env_map_serializes_sorted() {
    let mut env = BTreeMap::new();
    env.insert("B".to_string(), "2".to_string());
    env.insert("A".to_string(), "1".to_string());
    let task = velnor_actions_fleetlint::TaskDef {
        name: "t".to_string(),
        aliases: vec![],
        depends: vec![],
        cmd: "echo hi".to_string(),
        dir: ".".to_string(),
        env,
        sources: vec![],
        outputs: vec![],
    };
    assert_eq!(
        velnor_actions_fleetlint::leaf_identity(&task),
        "echo hi|dir=.|env=A=1,B=2"
    );
}
