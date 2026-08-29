//! Fleet data model and validation.
//!
//! This module owns the parsed, validated fleet: the 28-member repository
//! manifest and the five class contracts. Gates are data: each class declares an
//! ordered gate list in `fleet/classes.toml`, and each member may schedule a
//! subset of its class's gates in `fleet/repositories.toml`. Parsing is
//! fail-closed — an invalid slug, a non-40-hex SHA, a duplicate or unclassified
//! member, a wrong per-class count, a class with no gates, a duplicate or
//! malformed gate name, an empty gate command, a member referencing a gate its
//! class does not declare, or an implicit gate applicability all reject before
//! any output is accepted.

use std::collections::BTreeSet;
use std::path::Path;

use serde::Deserialize;

use crate::RepositoryClass;

/// The three recognized fleet organizations, in canonical rendering order.
pub const OWNERS: [&str; 3] = ["jackin-project", "tailrocks", "ChainArgos"];

/// The conventional first gate: dependency installation that every other gate
/// builds on. A class that declares it must declare it first.
pub const INSTALL_GATE: &str = "install";

/// Required member count per class, in canonical [`crate::ALL_CLASSES`] order:
/// 19 code, 1 native, 5 tap, 2 apt, 1 fixture (total 28).
pub const REQUIRED_COUNTS: [(RepositoryClass, usize); 5] = [
    (RepositoryClass::Code, 19),
    (RepositoryClass::Native, 1),
    (RepositoryClass::Tap, 5),
    (RepositoryClass::Apt, 2),
    (RepositoryClass::Fixture, 1),
];

/// One lane selector. `Velnor` is the safe default; `Both` reports each lane
/// independently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    /// The self-hosted Velnor lane (default).
    Velnor,
    /// The GitHub-hosted lane.
    GitHub,
    /// Both lanes, reported independently.
    Both,
}

impl Lane {
    /// Parse a lane selector from its lowercase token.
    #[must_use]
    pub fn from_token(token: &str) -> Option<Lane> {
        match token {
            "velnor" => Some(Lane::Velnor),
            "github" => Some(Lane::GitHub),
            "both" => Some(Lane::Both),
            _ => None,
        }
    }

    /// The lowercase selector token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Lane::Velnor => "velnor",
            Lane::GitHub => "github",
            Lane::Both => "both",
        }
    }
}

/// Resolve a lane selector to the concrete execution lanes it runs. `Both`
/// expands to the Velnor lane then the GitHub lane; each runs independently and a
/// failure on one is never substituted by the other.
#[must_use]
pub fn resolve_lanes(selector: Lane) -> Vec<Lane> {
    match selector {
        Lane::Velnor => vec![Lane::Velnor],
        Lane::GitHub => vec![Lane::GitHub],
        Lane::Both => vec![Lane::Velnor, Lane::GitHub],
    }
}

/// Whether a gate is lane-portable or platform-only. Explicit for every gate;
/// there is no implicit default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applicability {
    /// Runs on both the Velnor lane and the GitHub-hosted lane.
    Both,
    /// Platform-only: runs only on the GitHub-hosted lane. Not applicable on the
    /// Velnor lane and never misreported there as an equivalent Velnor check.
    Github,
}

impl Applicability {
    fn from_token(token: &str) -> Result<Applicability, String> {
        match token {
            "both" => Ok(Applicability::Both),
            "github" => Ok(Applicability::Github),
            other => Err(format!(
                "invalid applicability {other:?} (expected \"both\" or \"github\")"
            )),
        }
    }

    /// Whether this gate is applicable on the given concrete lane.
    #[must_use]
    pub fn applies_on(self, lane: Lane) -> bool {
        match (self, lane) {
            (Applicability::Both, _) => true,
            (Applicability::Github, Lane::GitHub) => true,
            (Applicability::Github, _) => false,
        }
    }
}

/// One named CI gate with its exact command and explicit applicability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gate {
    /// Gate name (unique within its class, e.g. install, build, test).
    pub name: String,
    /// Exact non-empty command executed by the `run-gate` building block.
    pub command: String,
    /// Explicit lane applicability.
    pub applicability: Applicability,
}

impl Gate {
    /// The mise task this gate's command invokes, if the command is exactly a
    /// `mise run <task>` (or `mise task <task>`) invocation. Other commands are
    /// opaque to the task graph and are identified by their command bytes.
    #[must_use]
    pub fn mise_task(&self) -> Option<String> {
        mise_task_of_command(&self.command)
    }
}

/// Resolve the task name targeted by a `mise run <task>` / `mise task <task>`
/// command, skipping short flag tokens. Returns `None` for any other command.
fn mise_task_of_command(command: &str) -> Option<String> {
    let mut tokens = command.split_whitespace();
    if tokens.next()? != "mise" {
        return None;
    }
    match tokens.next()? {
        "run" | "task" | "r" | "t" => {}
        _ => return None,
    }
    tokens
        .find(|token| !token.starts_with('-'))
        .filter(|token| !token.is_empty())
        .map(str::to_string)
}

/// The shared contract for one repository class.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassContract {
    /// The class this contract governs.
    pub class: RepositoryClass,
    /// The ordered gates this class declares (one or more; unique names).
    pub gates: Vec<Gate>,
    /// Whether the class carries any genuinely platform-only gate.
    pub platform_only: bool,
    /// GitHub-hosted runner for the platform lane, present iff one is required.
    pub platform_runner: Option<String>,
    /// Stable check name for the platform lane, present iff one is required.
    pub platform_name: Option<String>,
    /// Job timeout for the platform lane in minutes; absent means the default.
    pub platform_timeout_minutes: Option<u32>,
}

impl ClassContract {
    /// The gates that are applicable on the given concrete lane.
    #[must_use]
    pub fn applicable_gates(&self, lane: Lane) -> Vec<&Gate> {
        self.gates
            .iter()
            .filter(|g| g.applicability.applies_on(lane))
            .collect()
    }

    /// The gate with the given name, if this class declares one.
    #[must_use]
    pub fn gate(&self, name: &str) -> Option<&Gate> {
        self.gates.iter().find(|gate| gate.name == name)
    }
}

/// One fleet member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repository {
    /// The `owner/repo` slug.
    pub slug: String,
    /// The owning organization (the slug's owner segment).
    pub organization: String,
    /// The normalized class.
    pub class: RepositoryClass,
    /// The immutable 40-hex default-branch baseline commit.
    pub baseline_sha: String,
    /// The gates this member schedules, in class declaration order. Members that
    /// declare no subset schedule every gate their class declares.
    pub gates: Vec<String>,
}

impl Repository {
    /// The repository name (the slug's repo segment).
    #[must_use]
    pub fn name(&self) -> &str {
        self.slug
            .split_once('/')
            .map_or(self.slug.as_str(), |(_, name)| name)
    }
}

/// The fully validated fleet: 28 repositories and five class contracts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetManifest {
    repositories: Vec<Repository>,
    classes: Vec<ClassContract>,
}

impl FleetManifest {
    /// Load and validate the fleet from `fleet/classes.toml` and
    /// `fleet/repositories.toml` under `root`.
    ///
    /// # Errors
    ///
    /// Returns `Err` for any I/O failure, TOML syntax error, or contract
    /// violation (invalid slug/SHA, duplicate or unclassified member, wrong
    /// count, class with no gates, duplicate or malformed gate name, empty gate
    /// command, member gate not declared by its class, or implicit/invalid
    /// applicability).
    pub fn load(root: &Path) -> Result<FleetManifest, String> {
        let classes = load_classes(&root.join("fleet").join("classes.toml"))?;
        let repositories =
            load_repositories(&root.join("fleet").join("repositories.toml"), &classes)?;
        Ok(FleetManifest {
            repositories,
            classes,
        })
    }

    /// All 28 repositories, in declaration order.
    #[must_use]
    pub fn repositories(&self) -> &[Repository] {
        &self.repositories
    }

    /// The five class contracts, in canonical [`crate::ALL_CLASSES`] order.
    #[must_use]
    pub fn classes(&self) -> &[ClassContract] {
        &self.classes
    }

    /// The contract for one class.
    #[must_use]
    pub fn class(&self, class: RepositoryClass) -> &ClassContract {
        self.classes
            .iter()
            .find(|c| c.class == class)
            .expect("every class present after validation")
    }

    /// The members of one class, in declaration order.
    #[must_use]
    pub fn members_of(&self, class: RepositoryClass) -> Vec<&Repository> {
        self.repositories
            .iter()
            .filter(|r| r.class == class)
            .collect()
    }

    /// The gates one member schedules, in class declaration order. Members that
    /// declare no subset schedule every gate their class declares.
    #[must_use]
    pub fn scheduled_gates(&self, repository: &Repository) -> Vec<&Gate> {
        let contract = self.class(repository.class);
        repository
            .gates
            .iter()
            .filter_map(|name| contract.gate(name))
            .collect()
    }
}

/// Whether `value` is exactly 40 lowercase hexadecimal characters.
#[must_use]
pub fn is_sha40(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

fn valid_slug_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RepositoriesFile {
    #[serde(default)]
    repository: Vec<RepoEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RepoEntry {
    slug: String,
    class: String,
    baseline_sha: String,
    /// Optional subset of the class gates this member schedules. Absent (or
    /// empty) means every gate the class declares.
    #[serde(default)]
    gates: Option<Vec<String>>,
}

fn load_repositories(path: &Path, classes: &[ClassContract]) -> Result<Vec<Repository>, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let file: RepositoriesFile =
        toml::from_str(&text).map_err(|e| format!("parsing {}: {e}", path.display()))?;

    let mut repositories = Vec::with_capacity(file.repository.len());
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for entry in file.repository {
        let (owner, name) = entry
            .slug
            .split_once('/')
            .ok_or_else(|| format!("invalid slug {:?}: expected owner/repo", entry.slug))?;
        if !valid_slug_segment(owner) || !valid_slug_segment(name) || name.contains('/') {
            return Err(format!("invalid slug {:?}", entry.slug));
        }
        if !is_sha40(&entry.baseline_sha) {
            return Err(format!(
                "member {:?} baseline_sha {:?} is not 40 lowercase hex",
                entry.slug, entry.baseline_sha
            ));
        }
        let class = parse_class(&entry.class).ok_or_else(|| {
            format!(
                "member {:?} has unknown class {:?}",
                entry.slug, entry.class
            )
        })?;
        if !seen.insert(entry.slug.clone()) {
            return Err(format!("duplicate member {:?}", entry.slug));
        }
        let contract = classes
            .iter()
            .find(|c| c.class == class)
            .expect("every class present after load_classes");
        let gates = resolve_member_gates(&entry, contract)?;
        repositories.push(Repository {
            slug: entry.slug.clone(),
            organization: owner.to_string(),
            class,
            baseline_sha: entry.baseline_sha,
            gates,
        });
    }

    for &(class, want) in &REQUIRED_COUNTS {
        let have = repositories.iter().filter(|r| r.class == class).count();
        if have != want {
            return Err(format!(
                "class {} has {have} members, expected {want}",
                class.code()
            ));
        }
    }
    let total: usize = REQUIRED_COUNTS.iter().map(|&(_, n)| n).sum();
    if repositories.len() != total {
        return Err(format!(
            "fleet has {} members, expected {total}",
            repositories.len()
        ));
    }

    // Reject any organization outside the three recognized owners.
    for repo in &repositories {
        if !OWNERS.contains(&repo.organization.as_str()) {
            return Err(format!(
                "member {:?} has unknown organization {:?}",
                repo.slug, repo.organization
            ));
        }
    }

    Ok(repositories)
}

fn parse_class(token: &str) -> Option<RepositoryClass> {
    crate::ALL_CLASSES
        .iter()
        .copied()
        .find(|c| c.code() == token)
}

/// Validate a member's declared gate subset against its class contract and
/// canonicalize it to class declaration order. An absent or empty declaration
/// schedules every gate the class declares.
fn resolve_member_gates(
    entry: &RepoEntry,
    contract: &ClassContract,
) -> Result<Vec<String>, String> {
    let Some(declared) = entry.gates.as_ref() else {
        return Ok(contract.gates.iter().map(|g| g.name.clone()).collect());
    };
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for gate in declared {
        if !seen.insert(gate.as_str()) {
            return Err(format!(
                "member {:?} schedules gate {:?} more than once",
                entry.slug, gate
            ));
        }
        if contract.gate(gate).is_none() {
            return Err(format!(
                "member {:?} schedules gate {:?} which class {} does not declare",
                entry.slug,
                gate,
                contract.class.code()
            ));
        }
    }
    Ok(contract
        .gates
        .iter()
        .map(|g| g.name.clone())
        .filter(|name| seen.contains(name.as_str()))
        .collect())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClassesFile {
    code: ClassEntry,
    native: ClassEntry,
    tap: ClassEntry,
    apt: ClassEntry,
    fixture: ClassEntry,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClassEntry {
    platform_only: bool,
    #[serde(default)]
    platform_runner: Option<String>,
    #[serde(default)]
    platform_name: Option<String>,
    #[serde(default)]
    platform_timeout_minutes: Option<u32>,
    /// The ordered gate list this class declares (`[[<class>.gates]]`). Defaults
    /// to empty so an omitted list is rejected by load_classes with an
    /// actionable message instead of a serde "missing field" parse error.
    #[serde(default)]
    gates: Vec<GateEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GateEntry {
    name: String,
    command: String,
    applicability: String,
}

/// Gate names become rendered step names and fleet task-graph identities: keep
/// them short, lowercase, and hyphenated.
fn valid_gate_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(b) if b.is_ascii_lowercase())
        && name.len() <= 40
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn load_classes(path: &Path) -> Result<Vec<ClassContract>, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let file: ClassesFile =
        toml::from_str(&text).map_err(|e| format!("parsing {}: {e}", path.display()))?;

    let entries = [
        (RepositoryClass::Code, file.code),
        (RepositoryClass::Native, file.native),
        (RepositoryClass::Tap, file.tap),
        (RepositoryClass::Apt, file.apt),
        (RepositoryClass::Fixture, file.fixture),
    ];

    let mut classes = Vec::with_capacity(entries.len());
    for (class, entry) in entries {
        if entry.gates.is_empty() {
            return Err(format!(
                "class {} declares no gates; at least one is required",
                class.code()
            ));
        }
        let mut gates = Vec::with_capacity(entry.gates.len());
        let mut platform_seen = false;
        let mut names: BTreeSet<String> = BTreeSet::new();
        for spec in entry.gates {
            if !valid_gate_name(&spec.name) {
                return Err(format!(
                    "class {} has invalid gate name {:?} (expected lowercase/hyphen, first letter a-z)",
                    class.code(),
                    spec.name
                ));
            }
            if !names.insert(spec.name.clone()) {
                return Err(format!(
                    "class {} declares gate {:?} more than once",
                    class.code(),
                    spec.name
                ));
            }
            if spec.name == INSTALL_GATE && !gates.is_empty() {
                return Err(format!(
                    "class {} must declare the {INSTALL_GATE:?} gate first",
                    class.code()
                ));
            }
            if spec.command.trim().is_empty() {
                return Err(format!(
                    "class {} gate {:?} has an empty command",
                    class.code(),
                    spec.name
                ));
            }
            let applicability = Applicability::from_token(&spec.applicability)
                .map_err(|e| format!("class {} gate {:?}: {e}", class.code(), spec.name))?;
            if applicability == Applicability::Github {
                platform_seen = true;
            }
            gates.push(Gate {
                name: spec.name,
                command: spec.command,
                applicability,
            });
        }
        if entry.platform_only != platform_seen {
            return Err(format!(
                "class {} declares platform_only={} but {} a platform-only gate",
                class.code(),
                entry.platform_only,
                if platform_seen { "has" } else { "has no" }
            ));
        }
        let (platform_runner, platform_name) = match (
            entry.platform_only,
            entry.platform_runner,
            entry.platform_name,
        ) {
            (false, None, None) => (None, None),
            (true, Some(runner), Some(name)) => {
                let valid_runner = runner.starts_with("macos-")
                    && runner.bytes().all(|byte| {
                        byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'
                    });
                if !valid_runner {
                    return Err(format!(
                        "class {} has invalid platform runner {runner:?}",
                        class.code()
                    ));
                }
                let valid_name = !name.is_empty()
                    && name.len() <= 80
                    && name.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || byte == b' ' || byte == b'-' || byte == b'_'
                    });
                if !valid_name {
                    return Err(format!(
                        "class {} has invalid platform name {name:?}",
                        class.code()
                    ));
                }
                (Some(runner), Some(name))
            }
            _ => {
                return Err(format!(
                    "class {} must declare platform_runner and platform_name iff platform_only is true",
                    class.code()
                ));
            }
        };
        if let Some(timeout) = entry.platform_timeout_minutes {
            if !entry.platform_only {
                return Err(format!(
                    "class {} declares platform_timeout_minutes but has no platform-only gate",
                    class.code()
                ));
            }
            if !(5..=120).contains(&timeout) {
                return Err(format!(
                    "class {} has platform_timeout_minutes {timeout} outside 5..=120",
                    class.code()
                ));
            }
        }
        classes.push(ClassContract {
            class,
            gates,
            platform_only: entry.platform_only,
            platform_runner,
            platform_name,
            platform_timeout_minutes: entry.platform_timeout_minutes,
        });
    }

    Ok(classes)
}
