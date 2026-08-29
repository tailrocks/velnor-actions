//! `velnor-actions-fleetlint` CLI.
//!
//! Parses every fleet member's mise task graph, expands the gates each member
//! schedules, and writes or verifies the committed snapshots under
//! `fleet/task-graphs/`. Exits nonzero when any member has findings, when
//! `--check` finds stale or missing snapshots, or when the fleet manifest is
//! invalid.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use velnor_actions_fleetlint::{
    TASK_GRAPH_DIR, analyze_member, parse_task_graph, propose_gates, snapshot_file_name,
};
use velnor_actions_generator::model::FleetManifest;

struct Options {
    root: PathBuf,
    repos_dir: Option<PathBuf>,
    out_dir: Option<PathBuf>,
    check: bool,
    json: bool,
    propose_gates: bool,
}

fn main() -> ExitCode {
    match run(std::env::args().skip(1)) {
        Ok(message) => {
            println!("{message}");
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: impl Iterator<Item = String>) -> Result<String, String> {
    let options = parse_args(args)?;
    if options.propose_gates {
        return propose(&options);
    }
    let manifest = FleetManifest::load(&options.root)?;
    let out_dir = options
        .out_dir
        .clone()
        .unwrap_or_else(|| options.root.join(TASK_GRAPH_DIR));
    let repos_dir = options
        .repos_dir
        .as_deref()
        .ok_or("fleetlint requires --repos-dir")?;

    let mut reports = Vec::new();
    for repository in manifest.repositories() {
        let gates = manifest
            .scheduled_gates(repository)
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        reports.push(analyze_member(repos_dir, repository, &gates));
    }

    for report in &reports {
        for finding in &report.findings {
            if options.json {
                println!("{}", serde_json::to_string(finding).expect("serializes"));
            } else {
                println!("{}: {}: {}", report.slug, finding.kind, finding.message);
            }
        }
    }

    let finding_count: usize = reports.iter().map(|r| r.findings.len()).sum();
    if finding_count > 0 {
        return Err(format!(
            "{finding_count} finding(s) across {} fleet member(s); fix the scheduled task graphs before committing snapshots",
            reports.len()
        ));
    }

    if options.check {
        verify_snapshots(&out_dir, &reports)?;
    } else {
        write_snapshots(&out_dir, &reports)?;
    }

    Ok(format!(
        "{} member task graphs clean{}",
        reports.len(),
        if options.check {
            " (snapshots verified)"
        } else {
            "; snapshots written"
        }
    ))
}

/// `--propose-gates`: print, per member, the greedy-clean subset of its class
/// gates for authoring `gates = [...]` rows in `fleet/repositories.toml`.
fn propose(options: &Options) -> Result<String, String> {
    let manifest = FleetManifest::load(&options.root)?;
    let Some(repos_dir) = options.repos_dir.as_deref() else {
        return Err("--propose-gates requires --repos-dir".to_string());
    };
    for repository in manifest.repositories() {
        let repo_dir = repos_dir.join(repository.name());
        let contract = manifest.class(repository.class);
        let proposed = match parse_task_graph(&repo_dir) {
            Ok(graph) => propose_gates(&repository.slug, repository.class, contract, &graph),
            Err(error) => {
                eprintln!("{}: parse-error: {error}", repository.slug);
                Vec::new()
            }
        };
        println!(
            "{} [class {}]: [{}]",
            repository.slug,
            repository.class.code(),
            proposed.join(", ")
        );
    }
    Ok(format!(
        "proposed gates for {} member(s); findings-free subsets only",
        manifest.repositories().len()
    ))
}

fn write_snapshots(
    out_dir: &Path,
    reports: &[velnor_actions_fleetlint::RepoReport],
) -> Result<(), String> {
    std::fs::create_dir_all(out_dir).map_err(|e| format!("creating {}: {e}", out_dir.display()))?;
    for report in reports {
        let path = out_dir.join(snapshot_file_name(&report.slug));
        std::fs::write(&path, report.snapshot_bytes())
            .map_err(|e| format!("writing {}: {e}", path.display()))?;
    }
    Ok(())
}

fn verify_snapshots(
    out_dir: &Path,
    reports: &[velnor_actions_fleetlint::RepoReport],
) -> Result<(), String> {
    let mut committed: Vec<PathBuf> = std::fs::read_dir(out_dir)
        .map_err(|e| format!("reading {}: {e}", out_dir.display()))?
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            (path.extension().is_some_and(|ext| ext == "json")).then_some(path)
        })
        .collect();
    committed.sort();
    let expected: Vec<PathBuf> = reports
        .iter()
        .map(|r| out_dir.join(snapshot_file_name(&r.slug)))
        .collect();
    let mut mismatches = Vec::new();
    for path in &committed {
        if !expected.contains(path) {
            mismatches.push(format!("unexpected snapshot {}", path.display()));
        }
    }
    for report in reports {
        let path = out_dir.join(snapshot_file_name(&report.slug));
        let committed = std::fs::read_to_string(&path).unwrap_or_default();
        if committed.is_empty() {
            mismatches.push(format!("missing snapshot {}", path.display()));
        } else if committed != report.snapshot_bytes() {
            mismatches.push(format!(
                "stale snapshot {} (rerun without --check to refresh)",
                path.display()
            ));
        }
    }
    if mismatches.is_empty() {
        return Ok(());
    }
    for mismatch in &mismatches {
        eprintln!("snapshot mismatch: {mismatch}");
    }
    Err(format!("{} snapshot mismatch(es)", mismatches.len()))
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Options, String> {
    let mut options = Options {
        root: PathBuf::from("."),
        repos_dir: None,
        out_dir: None,
        check: false,
        json: false,
        propose_gates: false,
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--root" => {
                options.root = PathBuf::from(value(&mut args, "--root")?);
            }
            "--repos-dir" => {
                options.repos_dir = Some(PathBuf::from(value(&mut args, "--repos-dir")?));
            }
            "--out-dir" => {
                options.out_dir = Some(PathBuf::from(value(&mut args, "--out-dir")?));
            }
            "--check" => options.check = true,
            "--json" => options.json = true,
            "--propose-gates" => options.propose_gates = true,
            other => return Err(format!("unexpected argument: {other}")),
        }
    }
    if options.repos_dir.is_none() && !options.propose_gates {
        return Err(
            "fleetlint requires --repos-dir (directory of fleet member clones)".to_string(),
        );
    }
    Ok(options)
}

fn value(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    args.next()
        .ok_or_else(|| format!("{flag} requires a value"))
}
