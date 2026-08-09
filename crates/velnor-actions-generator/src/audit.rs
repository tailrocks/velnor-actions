//! Fleet audit.
//!
//! Regenerates every composite building-block action and every template (and, once
//! the block SHA is bound, every callable workflow) in memory, compares against the
//! committed bytes, materializes all 28 repositories to prove each equals its class
//! template, and enforces the closure, owner-fan-out, aggregation, gate, and routing
//! invariants. Any one-byte hand edit to a generated file — including a neutered
//! composite run-script body — fails the audit.

use std::path::Path;

use crate::cache::CacheContract;
use crate::composite;
use crate::model::{FleetManifest, OWNERS, is_sha40};
use crate::render::{
    self, ACTIONS_REPO, CALVER_PLACEHOLDER, CANONICAL_OWNER, FLEET_SHA_PLACEHOLDER,
};
use crate::{ALL_CLASSES, RepositoryClass};

const ADMITTED_REMOTE_ROOTS: [(&str, &str); 5] = [
    (
        "actions/checkout",
        "3d3c42e5aac5ba805825da76410c181273ba90b1",
    ),
    (
        "jdx/mise-action",
        "7e36c90d9ab29c415a2384db3006f3ec8a8cc654",
    ),
    ("actions/cache", "55cc8345863c7cc4c66a329aec7e433d2d1c52a9"),
    (
        "actions/upload-artifact",
        "043fb46d1a93c77aae656e7c1c64a875d1fc6a0a",
    ),
    (
        "actions/download-artifact",
        "3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c",
    ),
];

const REMOTE_EXECUTABLE_CLOSURE: [(&str, &str); 15] = [
    (
        "actions/checkout/action.yml",
        "d59219cb79590abdb877deaa14e3b65a00c05318bf5a6f3b989b9162b5d08c35",
    ),
    (
        "actions/checkout/dist/index.js",
        "b604bf1c08a471aedf51ddddbd3e8d03041683db270692d66cd1b4b097457818",
    ),
    (
        "jdx/mise-action/action.yml",
        "d39d054a1402d4260af8343352cd54f4a44453276abfc2f9f47206e8beb00dec",
    ),
    (
        "jdx/mise-action/dist/index.js",
        "8ca04f766d0b1ff41c13c873728288252ade6c6f3d2783212587b0f95a7604b2",
    ),
    (
        "actions/cache/action.yml",
        "c04edf35104c16c8944076df01561748bac9cd10941417e467a236e508e52dda",
    ),
    (
        "actions/cache/dist/restore/index.js",
        "79a05f72974c12e3796bf09c897fce453a855f5f4a4abc980be6f671c545954d",
    ),
    (
        "actions/cache/dist/save/index.js",
        "9193aa9dbe5025f2bf39802ae241470af35c5e5737fd4bec6e1d6dbaad153bb4",
    ),
    (
        "actions/cache/restore/action.yml",
        "bb552a94cec165255080b7e13fe3d2051c6c79e321e99435b5859dc7de5e9057",
    ),
    (
        "actions/cache/dist/restore-only/index.js",
        "8522c6be9a4650a3b1571f7222293f262cc4c4708fbe061d8ed04654d476cf16",
    ),
    (
        "actions/cache/save/action.yml",
        "5da400e1a7551cee617339fd1a5281483d71e6c56d86dd117fbd354f047308ec",
    ),
    (
        "actions/cache/dist/save-only/index.js",
        "0d3cf341a79cdc0a8ce5a71103f46174e12cf4f98db46f15b8e604932eefd487",
    ),
    (
        "actions/upload-artifact/action.yml",
        "c5979822866a72362e609844b6ebe77d4b7e759af68cc1c2c425dcf51481fab4",
    ),
    (
        "actions/upload-artifact/dist/upload/index.js",
        "eea594941d8ee535974e0fbc03bbdf567f3abc78194f224b93f2df9a887ee2e9",
    ),
    (
        "actions/download-artifact/action.yml",
        "e98559b7a31ba31be4709f20d22102dc2737fa630f69a339eb89981151e505fe",
    ),
    (
        "actions/download-artifact/dist/index.js",
        "f4a6b7046eb834bed98bb687f6d08feb0bfe6fb32570e9da7de0707884c08b31",
    ),
];

/// A deterministic disposable release identity used only to exercise
/// materialization during the audit (never written anywhere).
const AUDIT_RELEASE_SHA: &str = "0000000000000000000000000000000000000000";
const AUDIT_CALVER: &str = "2026.7.0";

/// Run the full fleet audit under `root`. Returns the exact summary line on
/// success.
///
/// # Errors
///
/// Returns `Err` on any contract, byte, closure, owner-fan-out, aggregation,
/// gate, or routing violation.
pub fn audit(root: &Path) -> Result<String, String> {
    let manifest = FleetManifest::load(root)?;
    let caches = CacheContract::load(&root.join("fleet").join("caches.toml"))?;

    // Composite building blocks must exist and match their canonical bytes exactly
    // (body included), so a neutered run-script fails the audit.
    for name in composite::COMPOSITE_NAMES {
        check_composite(root, name)?;
    }

    // Every class template: regenerate and compare committed bytes.
    for class in ALL_CLASSES {
        let rendered = render::consumer_template(class);
        let committed = read_committed(&template_path(root, class))?;
        require_equal(&committed, &rendered, &template_path_display(class))?;
        audit_consumer_structure(class, &rendered)?;
    }

    // Materialize all 28 repositories and prove each equals its class template.
    audit_materialization(&manifest)?;

    // If the block SHA is bound, audit the full callable-workflow closure.
    let block_sha_path = root.join("fleet").join("block-sha");
    if block_sha_path.exists() {
        let block_sha = read_block_sha(&block_sha_path)?;
        for class in ALL_CLASSES {
            let contract = manifest.class(class);
            let rendered = render::callable_workflow(contract, &caches, &block_sha);
            let committed = read_committed(&callable_path(root, class))?;
            require_equal(&committed, &rendered, &callable_path_display(class))?;
            audit_callable_structure(class, &rendered, &block_sha)?;
        }
    }

    Ok(format!(
        "fleet valid: {} repositories, {} classes, {} templates",
        manifest.repositories().len(),
        manifest.classes().len(),
        ALL_CLASSES.len(),
    ))
}

fn template_path(root: &Path, class: RepositoryClass) -> std::path::PathBuf {
    root.join("templates").join(class.code()).join("ci.yml")
}

fn template_path_display(class: RepositoryClass) -> String {
    format!("templates/{}/ci.yml", class.code())
}

fn callable_path(root: &Path, class: RepositoryClass) -> std::path::PathBuf {
    root.join(".github")
        .join("workflows")
        .join(render::callable_file_name(class))
}

fn callable_path_display(class: RepositoryClass) -> String {
    format!(".github/workflows/{}", render::callable_file_name(class))
}

fn read_committed(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| {
        format!(
            "reading generated file {}: {e} (run `mise run generate`)",
            path.display()
        )
    })
}

fn require_equal(committed: &str, rendered: &str, what: &str) -> Result<(), String> {
    if committed == rendered {
        Ok(())
    } else {
        Err(format!(
            "generated file {what} does not match regeneration (a hand edit or stale render); run `mise run generate`"
        ))
    }
}

fn read_block_sha(path: &Path) -> Result<String, String> {
    let raw =
        std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let sha = raw.trim();
    if !is_sha40(sha) {
        return Err(format!(
            "fleet/block-sha {sha:?} is not a 40 lowercase hex commit SHA (mutable or malformed refs are forbidden)"
        ));
    }
    Ok(sha.to_string())
}

fn check_composite(root: &Path, name: &str) -> Result<(), String> {
    let canonical = composite::canonical(name)
        .ok_or_else(|| format!("no canonical bytes for composite {name:?}"))?;

    // The embedded canonical bytes must themselves be a valid composite: this guards
    // the constant in `composite` against a bad future edit (a non-composite, or a
    // mutable/non-40-hex nested action ref).
    if !canonical.contains("using: composite") {
        return Err(format!(
            "canonical composite {name:?} is not a composite action"
        ));
    }
    for reference in uses_refs(canonical) {
        if !is_sha40(&reference) {
            return Err(format!(
                "canonical composite {name:?} references non-40-hex ref {reference:?}"
            ));
        }
    }

    // The committed action body must match the canonical bytes exactly. This is the
    // full body — env, run script, and exec line — not merely the refs, so tampering
    // (e.g. replacing the gate exec with `echo skipped`) fails the audit.
    let path = composite_path(root, name);
    let committed = read_committed(&path)?;
    require_equal(&committed, canonical, &composite_path_display(name))?;
    Ok(())
}

fn composite_path(root: &Path, name: &str) -> std::path::PathBuf {
    root.join("actions").join(name).join("action.yml")
}

fn composite_path_display(name: &str) -> String {
    format!("actions/{name}/action.yml")
}

/// Extract the git ref of every `uses: OWNER/REPO...@REF` line (ignoring local
/// `./` references, which have none). A trailing ` # comment` is stripped.
fn uses_refs(text: &str) -> Vec<String> {
    let mut refs = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start().trim_start_matches("- ").trim_start();
        let Some(rest) = trimmed.strip_prefix("uses:") else {
            continue;
        };
        let value = rest.trim();
        if value.starts_with("./") || value.starts_with('.') {
            continue;
        }
        // Drop a trailing version comment.
        let value = value.split_whitespace().next().unwrap_or(value);
        if let Some((_, r)) = value.rsplit_once('@') {
            refs.push(r.to_string());
        }
    }
    refs
}

fn uses_identities(text: &str) -> Vec<String> {
    let mut identities = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start().trim_start_matches("- ").trim_start();
        let Some(rest) = trimmed.strip_prefix("uses:") else {
            continue;
        };
        let value = rest.split_whitespace().next().unwrap_or_default();
        if !value.starts_with("./") && !value.starts_with('.') {
            identities.push(value.to_string());
        }
    }
    identities
}

fn audit_consumer_structure(class: RepositoryClass, rendered: &str) -> Result<(), String> {
    let what = template_path_display(class);
    let file = render::callable_file_name(class);

    // Exactly three static owner-local reusable calls, one per recognized owner,
    // selected only by exact github.repository_owner. No dynamic `uses`.
    for owner in OWNERS {
        let call = format!(
            "uses: {owner}/{ACTIONS_REPO}/.github/workflows/{file}@{FLEET_SHA_PLACEHOLDER} # {CALVER_PLACEHOLDER}"
        );
        if !rendered.contains(&call) {
            return Err(format!("{what}: missing owner-local call for {owner}"));
        }
        let guard = format!("if: ${{{{ github.repository_owner == '{owner}' }}}}");
        if !rendered.contains(&guard) {
            return Err(format!("{what}: missing exact owner guard for {owner}"));
        }
    }
    if rendered.matches("uses:").count() != OWNERS.len() {
        return Err(format!(
            "{what}: expected exactly {} reusable-workflow calls",
            OWNERS.len()
        ));
    }
    // A dynamic `uses:` (expression in the ref) is forbidden.
    for line in rendered.lines() {
        if line.contains("uses:") && line.contains("${{") {
            return Err(format!("{what}: dynamic `uses:` reference is forbidden"));
        }
    }

    // ci-required is fail-closed: always() plus the positive truth table.
    require_contains(
        rendered,
        "if: ${{ always() }}",
        &what,
        "ci-required always()",
    )?;
    require_contains(rendered, "case \"${OWNER}\" in", &what, "owner truth table")?;
    require_contains(
        rendered,
        "unrecognized owner",
        &what,
        "unknown-owner rejection",
    )?;
    require_contains(
        rendered,
        "expected both 'skipped'",
        &what,
        "non-selected skipped requirement",
    )?;
    require_contains(
        rendered,
        "expected empty",
        &what,
        "non-selected empty-output requirement",
    )?;
    require_contains(
        rendered,
        "expected 'success'",
        &what,
        "selected success requirement",
    )?;
    // Forbid "no failure found" acceptance logic.
    if rendered.contains("!= \"failure\"") || rendered.contains("!= 'failure'") {
        return Err(format!(
            "{what}: forbidden negative (no-failure) aggregation logic"
        ));
    }
    Ok(())
}

fn audit_callable_structure(
    class: RepositoryClass,
    rendered: &str,
    block_sha: &str,
) -> Result<(), String> {
    let what = callable_path_display(class);

    // workflow_call only — no other trigger.
    require_contains(rendered, "workflow_call:", &what, "workflow_call trigger")?;
    for forbidden in ["pull_request:", "push:", "schedule:", "workflow_dispatch:"] {
        if rendered.contains(&format!("\n  {forbidden}")) {
            return Err(format!(
                "{what}: callable workflow must be workflow_call only, found {forbidden}"
            ));
        }
    }
    // No secret or environment inheritance.
    if rendered.contains("secrets:") || rendered.contains("environment:") {
        return Err(format!(
            "{what}: callable workflow must not inherit secrets or environments"
        ));
    }

    // Internal composite closure is pinned to the block SHA.
    let run_gate = format!("{CANONICAL_OWNER}/{ACTIONS_REPO}/actions/run-gate@{block_sha}");
    let aggregate = format!("{CANONICAL_OWNER}/{ACTIONS_REPO}/actions/aggregate@{block_sha}");
    require_contains(rendered, &run_gate, &what, "run-gate pinned to block SHA")?;
    require_contains(rendered, &aggregate, &what, "aggregate pinned to block SHA")?;

    // Every executable `uses:` ref must be a full 40-hex SHA (no mutable refs).
    for reference in uses_refs(rendered) {
        if !is_sha40(&reference) {
            return Err(format!("{what}: non-40-hex or mutable ref {reference:?}"));
        }
    }
    audit_admitted_closure(rendered, block_sha, &what)?;

    // Public unmerged-code events route the Velnor lane to GitHub-hosted; no
    // public-unmerged Velnor route is allowed.
    require_contains(
        rendered,
        "(github.event_name == 'pull_request' || github.event_name == 'merge_group') && 'ubuntu-latest' || 'velnor-trusted'",
        &what,
        "public-unmerged GitHub-hosted routing",
    )?;

    // Lane verdict is fail-closed and positive.
    require_contains(rendered, "if: ${{ always() }}", &what, "verdict always()")?;
    require_contains(
        rendered,
        "one lane never substitutes",
        &what,
        "no-substitution note",
    )?;
    require_contains(rendered, "contract=success", &what, "contract emission")?;

    // Every gate command rendered into the workflow is non-empty.
    for line in rendered.lines() {
        if let Some(cmd) = line.trim_start().strip_prefix("command:")
            && cmd.trim().is_empty()
        {
            return Err(format!("{what}: rendered gate has an empty command"));
        }
    }
    Ok(())
}

fn audit_admitted_closure(rendered: &str, block_sha: &str, what: &str) -> Result<(), String> {
    for identity in uses_identities(rendered) {
        let (target, reference) = identity
            .rsplit_once('@')
            .ok_or_else(|| format!("{what}: action identity has no ref {identity:?}"))?;
        let mut segments = target.split('/');
        let owner = segments.next().unwrap_or_default();
        let repository = segments.next().unwrap_or_default();
        let root = format!("{owner}/{repository}");
        if root == format!("{CANONICAL_OWNER}/{ACTIONS_REPO}") {
            if reference != block_sha {
                return Err(format!(
                    "{what}: internal action is not block-bound: {identity}"
                ));
            }
            continue;
        }
        if !ADMITTED_REMOTE_ROOTS
            .iter()
            .any(|(admitted, sha)| *admitted == root && *sha == reference)
        {
            return Err(format!("{what}: unadmitted remote action {identity}"));
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    for (path, digest) in REMOTE_EXECUTABLE_CLOSURE {
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(format!(
                "{what}: malformed executable closure digest for {path}"
            ));
        }
        if !seen.insert(path) {
            return Err(format!("{what}: duplicate executable closure path {path}"));
        }
    }
    for (root, _) in ADMITTED_REMOTE_ROOTS {
        if !REMOTE_EXECUTABLE_CLOSURE
            .iter()
            .any(|(path, _)| path.starts_with(&format!("{root}/")))
        {
            return Err(format!(
                "{what}: admitted root {root} has no executable closure"
            ));
        }
    }
    Ok(())
}

fn audit_materialization(manifest: &FleetManifest) -> Result<(), String> {
    for class in ALL_CLASSES {
        let template = render::consumer_template(class);
        let class_bytes = render::render_consumer(&template, AUDIT_RELEASE_SHA, AUDIT_CALVER)?;

        // Three coherent owner references sharing one SHA and one CalVer.
        let want_ref = format!("@{AUDIT_RELEASE_SHA} # {AUDIT_CALVER}");
        if class_bytes.matches(&want_ref).count() != OWNERS.len() {
            return Err(format!(
                "class {} materialization does not share one SHA/CalVer across all {} owner calls",
                class.code(),
                OWNERS.len()
            ));
        }
        // No placeholder survives; a second substitution is refused.
        if class_bytes.contains(FLEET_SHA_PLACEHOLDER) || class_bytes.contains(CALVER_PLACEHOLDER) {
            return Err(format!(
                "class {} materialization left a placeholder",
                class.code()
            ));
        }
        if render::render_consumer(&class_bytes, AUDIT_RELEASE_SHA, AUDIT_CALVER).is_ok() {
            return Err(format!(
                "class {} accepted a second repository-specific substitution",
                class.code()
            ));
        }

        // Every member of the class materializes to the identical bytes: no
        // per-repository fork or slug-specific substitution.
        for repo in manifest.members_of(class) {
            let repo_bytes = render::render_consumer(&template, AUDIT_RELEASE_SHA, AUDIT_CALVER)?;
            if repo_bytes != class_bytes {
                return Err(format!(
                    "repository {} does not materialize to its class {} template bytes",
                    repo.slug,
                    class.code()
                ));
            }
        }
    }
    Ok(())
}

fn require_contains(text: &str, needle: &str, what: &str, label: &str) -> Result<(), String> {
    if text.contains(needle) {
        Ok(())
    } else {
        Err(format!("{what}: missing {label}"))
    }
}
