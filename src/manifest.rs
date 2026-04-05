#![allow(dead_code)]

use std::collections::BTreeSet;
use std::path::PathBuf;

use kdl::{KdlDocument, KdlNode, KdlValue};
use miette::{LabeledSpan, NamedSource};

use crate::types::{ArtifactCoordinates, ArtifactId, ArtifactKey, ArtifactVersion, GroupId};

pub mod lock;

pub struct Manifest {
    pub group: GroupId,
    pub artifact: ArtifactId,
    pub version: ArtifactVersion,

    pub description: Option<String>,
    pub author: Option<String>,

    pub entry: Option<String>,

    pub source: PathBuf,

    pub resources: PathBuf,

    pub java: JavaConfig,

    pub build: BuildConfig,

    pub test: TestConfig,

    pub repositories: Repositories,

    pub dependencies: Vec<Dependency>,
}

#[derive(Default, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Packaging {
    Dir,
    #[default]
    Jar,
}

#[derive(Default)]
pub struct JavaConfig {
    pub release: Option<u32>,
    pub compiler_args: Vec<String>,
}

pub struct TestConfig {
    pub source: PathBuf,
    pub resources: PathBuf,
    pub jvm_args: Vec<String>,
}

impl Default for TestConfig {
    fn default() -> Self {
        Self {
            source: PathBuf::from("src/test/java"),
            resources: PathBuf::from("src/test/resources"),
            jvm_args: Vec::new(),
        }
    }
}

#[derive(Default)]
pub struct BuildConfig {
    pub packaging: Packaging,
    pub output: Option<PathBuf>,
    pub shadow: bool,
    pub post_build: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ChecksumPolicy {
    #[default]
    Fail,
    Warn,
    Ignore,
}

pub struct RepoEntry {
    pub url: String,
    pub checksum_policy: ChecksumPolicy,
}

pub struct Repositories(pub Vec<RepoEntry>);

impl Default for Repositories {
    fn default() -> Self {
        Self(vec![RepoEntry {
            url: crate::maven::MAVEN_REPO.to_string(),
            checksum_policy: ChecksumPolicy::Fail,
        }])
    }
}

impl Repositories {
    pub fn entries(&self) -> &[RepoEntry] {
        &self.0
    }

    pub fn urls(&self) -> Vec<String> {
        self.0.iter().map(|e| e.url.clone()).collect()
    }
}

#[derive(Clone, Copy, Hash, PartialEq, Eq, Default)]
pub enum Scope {
    #[default]
    Compile,
    Runtime,
    Provided,
    Processor,
    Test,
}

impl Scope {
    fn rank(self) -> u8 {
        match self {
            Self::Compile => 4,
            Self::Runtime => 3,
            Self::Provided | Self::Processor => 2,
            Self::Test => 0,
        }
    }
}

impl Ord for Scope {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.rank().cmp(&other.rank())
    }
}

impl PartialOrd for Scope {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl std::fmt::Display for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Compile => f.write_str("compile"),
            Self::Runtime => f.write_str("runtime"),
            Self::Provided => f.write_str("provided"),
            Self::Processor => f.write_str("processor"),
            Self::Test => f.write_str("test"),
        }
    }
}

impl std::str::FromStr for Scope {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "compile" => Ok(Self::Compile),
            "runtime" => Ok(Self::Runtime),
            "provided" => Ok(Self::Provided),
            "processor" => Ok(Self::Processor),
            "test" => Ok(Self::Test),
            other => anyhow::bail!("unknown scope: {other}"),
        }
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub enum PomScope {
    Compile,
    Runtime,
}

impl std::fmt::Display for PomScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Compile => f.write_str("compile"),
            Self::Runtime => f.write_str("runtime"),
        }
    }
}

impl std::str::FromStr for PomScope {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "compile" => Ok(Self::Compile),
            "runtime" => Ok(Self::Runtime),
            other => anyhow::bail!("unknown pom scope: {other}"),
        }
    }
}

pub fn mediate(parent_scope: Scope, pom_scope: PomScope) -> Scope {
    match (parent_scope, pom_scope) {
        (Scope::Compile, PomScope::Compile) => Scope::Compile,
        (Scope::Compile, PomScope::Runtime) => Scope::Runtime,
        (Scope::Provided, _) => Scope::Provided,
        (Scope::Processor, _) => Scope::Processor,
        (Scope::Runtime, _) => Scope::Runtime,
        (Scope::Test, _) => Scope::Test,
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct ArtifactType(pub(crate) String);

impl Default for ArtifactType {
    fn default() -> Self {
        Self("jar".into())
    }
}

impl ArtifactType {
    pub fn extension(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ArtifactType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

pub struct Dependency {
    pub scope: Scope,
    pub artifact_type: ArtifactType,
    pub source: DependencySource,
    pub exclusions: BTreeSet<ArtifactKey>,
}

pub enum DependencySource {
    Id(ArtifactCoordinates),
    Path(PathBuf),
}

impl Dependency {
    pub fn coord(&self) -> Option<&ArtifactCoordinates> {
        match &self.source {
            DependencySource::Id(coord) => Some(coord),
            DependencySource::Path(_) => None,
        }
    }
}

impl Manifest {
    pub fn parse(source: &str, name: &str) -> miette::Result<Self> {
        let doc: KdlDocument = source.parse()?;
        let src = NamedSource::new(name, source.to_string());

        let group = GroupId::new(require_string_arg(&doc, "group", &src)?);
        let artifact = ArtifactId::new(require_string_arg(&doc, "artifact", &src)?);
        let version_str = require_string_arg(&doc, "version", &src)?;
        let version = ArtifactVersion::new(version_str).map_err(|e| {
            let span = doc.get("version").unwrap().span();
            miette::miette!(labels = vec![LabeledSpan::at(span, "here")], "{e}")
                .with_source_code(src.clone())
        })?;

        let description = optional_string_arg(&doc, "description");
        let author = optional_string_arg(&doc, "author");
        let entry = optional_string_arg(&doc, "entry");

        let source_path = optional_string_arg(&doc, "source")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("src/main/java"));
        let resources = optional_string_arg(&doc, "resources")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("src/main/resources"));

        let java = parse_java_config(&doc, &src)?;
        let test = parse_test_config(&doc);
        let build = parse_build_config(&doc, &src)?;
        let repositories = parse_repositories(&doc);
        let dependencies = parse_dependencies(&doc, &src)?;

        Ok(Manifest {
            group,
            artifact,
            version,
            description,
            author,
            entry,
            source: source_path,
            resources,
            java,
            build,
            test,
            repositories,
            dependencies,
        })
    }

    pub fn dependency_coords(&self) -> BTreeSet<ArtifactCoordinates> {
        self.dependencies
            .iter()
            .filter_map(|d| d.coord().cloned())
            .collect()
    }
}

fn require_string_arg(
    doc: &KdlDocument,
    name: &str,
    src: &NamedSource<String>,
) -> miette::Result<String> {
    let node = doc.get(name).ok_or_else(|| {
        miette::miette!(
            labels = vec![LabeledSpan::at(doc.span(), "in this document")],
            "missing required field: {name}"
        )
        .with_source_code(src.clone())
    })?;

    let val = node.entry(0).ok_or_else(|| {
        miette::miette!(
            labels = vec![LabeledSpan::at(node.span(), "this node has no value")],
            "{name} requires a value"
        )
        .with_source_code(src.clone())
    })?;

    match val.value() {
        KdlValue::String(s) => Ok(s.clone()),
        _ => Err(miette::miette!(
            labels = vec![LabeledSpan::at(val.span(), "expected a string")],
            "{name} must be a string"
        )
        .with_source_code(src.clone())),
    }
}

fn optional_string_arg(doc: &KdlDocument, name: &str) -> Option<String> {
    doc.get_arg(name).and_then(|v| match v {
        KdlValue::String(s) => Some(s.clone()),
        _ => None,
    })
}

fn parse_kdl_bool(val: &KdlValue) -> Option<bool> {
    match val {
        KdlValue::Bool(b) => Some(*b),
        KdlValue::String(s) if s == "true" => Some(true),
        KdlValue::String(s) if s == "false" => Some(false),
        _ => None,
    }
}

fn parse_checksum_policy(node: &KdlNode) -> ChecksumPolicy {
    node.entry("checksum-policy")
        .and_then(|e| match e.value() {
            KdlValue::String(s) => match s.as_str() {
                "fail" => Some(ChecksumPolicy::Fail),
                "warn" => Some(ChecksumPolicy::Warn),
                "ignore" => Some(ChecksumPolicy::Ignore),
                _ => None,
            },
            _ => None,
        })
        .unwrap_or_default()
}

fn parse_repositories(doc: &KdlDocument) -> Repositories {
    let Some(node) = doc.get("repositories") else {
        return Repositories::default();
    };
    let Some(children) = node.children() else {
        return Repositories::default();
    };

    let mut entries = Vec::new();
    let mut has_central = false;
    for node in children.nodes() {
        let enabled = node
            .entry("enabled")
            .and_then(|e| parse_kdl_bool(e.value()))
            .unwrap_or(true);

        if node.name().value() == "central" {
            has_central = true;
            if enabled {
                entries.push(RepoEntry {
                    url: crate::maven::MAVEN_REPO.to_string(),
                    checksum_policy: parse_checksum_policy(node),
                });
            }
        } else if enabled {
            entries.push(RepoEntry {
                url: node.name().value().to_string(),
                checksum_policy: parse_checksum_policy(node),
            });
        }
    }

    if !has_central {
        entries.insert(
            0,
            RepoEntry {
                url: crate::maven::MAVEN_REPO.to_string(),
                checksum_policy: ChecksumPolicy::Fail,
            },
        );
    }

    Repositories(entries)
}

fn parse_test_config(doc: &KdlDocument) -> TestConfig {
    let Some(node) = doc.get("test") else {
        return TestConfig::default();
    };

    let source = optional_string_arg_node(node, "source")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("src/test/java"));
    let resources = optional_string_arg_node(node, "resources")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("src/test/resources"));

    let jvm_args = node
        .children()
        .into_iter()
        .flat_map(|c| c.nodes().iter().filter(|n| n.name().value() == "jvm-args"))
        .filter_map(|n| n.entry(0))
        .filter_map(|e| match e.value() {
            KdlValue::String(s) => Some(s.clone()),
            _ => None,
        })
        .collect();

    TestConfig {
        source,
        resources,
        jvm_args,
    }
}

fn parse_java_config(doc: &KdlDocument, src: &NamedSource<String>) -> miette::Result<JavaConfig> {
    let Some(node) = doc.get("java") else {
        return Ok(JavaConfig::default());
    };
    let Some(children) = node.children() else {
        return Ok(JavaConfig::default());
    };

    let release = if let Some(val) = children.get_arg("release") {
        match val {
            KdlValue::Integer(n) => Some(*n as u32),
            KdlValue::String(s) => Some(s.parse::<u32>().map_err(|_| {
                let span = children.get("release").unwrap().span();
                miette::miette!(
                    labels = vec![LabeledSpan::at(span, "expected an integer")],
                    "invalid java release version: {s}"
                )
                .with_source_code(src.clone())
            })?),
            _ => None,
        }
    } else {
        None
    };

    let compiler_args = children
        .nodes()
        .iter()
        .filter(|n| n.name().value() == "compiler-args")
        .filter_map(|n| n.entry(0))
        .filter_map(|e| match e.value() {
            KdlValue::String(s) => Some(s.clone()),
            _ => None,
        })
        .collect();

    Ok(JavaConfig {
        release,
        compiler_args,
    })
}

fn parse_build_config(doc: &KdlDocument, src: &NamedSource<String>) -> miette::Result<BuildConfig> {
    let Some(node) = doc.get("build") else {
        return Ok(BuildConfig::default());
    };

    let output = optional_string_arg_node(node, "output").map(PathBuf::from);
    let shadow = node
        .children()
        .and_then(|c| c.get_arg("shadow"))
        .and_then(parse_kdl_bool)
        .unwrap_or(false);
    let post_build = optional_string_arg_node(node, "post-build");

    let packaging = match optional_string_arg_node(node, "packaging").as_deref() {
        Some("jar") | None => Packaging::Jar,
        Some("dir") => Packaging::Dir,
        Some(other) => {
            let span = node.entry("packaging").unwrap().span();
            return Err(miette::miette!(
                labels = vec![LabeledSpan::at(span, "expected \"jar\" or \"dir\"")],
                "unknown packaging type: {other}"
            )
            .with_source_code(src.clone()));
        }
    };

    Ok(BuildConfig {
        packaging,
        output,
        shadow,
        post_build,
    })
}

fn optional_string_arg_node(node: &KdlNode, name: &str) -> Option<String> {
    node.children()?.get_arg(name).and_then(|v| match v {
        KdlValue::String(s) => Some(s.clone()),
        _ => None,
    })
}

fn parse_dependencies(
    doc: &KdlDocument,
    src: &NamedSource<String>,
) -> miette::Result<Vec<Dependency>> {
    let Some(node) = doc.get("dependencies") else {
        return Ok(Vec::new());
    };
    let Some(children) = node.children() else {
        return Ok(Vec::new());
    };

    let mut deps = Vec::new();
    for node in children.nodes() {
        deps.push(parse_dependency(node, src)?);
    }
    Ok(deps)
}

fn parse_dependency(node: &KdlNode, src: &NamedSource<String>) -> miette::Result<Dependency> {
    let scope = match node.name().value() {
        "compile" => Scope::Compile,
        "runtime" => Scope::Runtime,
        "provided" => Scope::Provided,
        "processor" => Scope::Processor,
        "test" => Scope::Test,
        other => {
            return Err(miette::miette!(
                labels = vec![LabeledSpan::at(node.name().span(), "unknown scope")],
                "unknown dependency scope: {other} (expected compile, runtime, provided, processor, or test)"
            )
            .with_source_code(src.clone()));
        }
    };

    let source = if let Some(path_entry) = node.entry("path") {
        match path_entry.value() {
            KdlValue::String(s) => DependencySource::Path(PathBuf::from(s)),
            _ => {
                return Err(miette::miette!(
                    labels = vec![LabeledSpan::at(path_entry.span(), "expected a string")],
                    "path must be a string"
                )
                .with_source_code(src.clone()));
            }
        }
    } else if let Some(entry) = node.entry(0) {
        match entry.value() {
            KdlValue::String(s) => {
                let coord: ArtifactCoordinates = s.parse().map_err(|e: anyhow::Error| {
                    miette::miette!(
                        labels = vec![LabeledSpan::at(entry.span(), "invalid coordinates")],
                        "{e}"
                    )
                    .with_source_code(src.clone())
                })?;
                DependencySource::Id(coord)
            }
            _ => {
                return Err(miette::miette!(
                    labels = vec![LabeledSpan::at(entry.span(), "expected a string")],
                    "dependency id must be a string"
                )
                .with_source_code(src.clone()));
            }
        }
    } else {
        return Err(miette::miette!(
            labels = vec![LabeledSpan::at(node.span(), "missing id or path")],
            "dependency must have an id (\"G:A:V\") or path property"
        )
        .with_source_code(src.clone()));
    };

    let mut exclusions = BTreeSet::new();
    if let Some(children) = node.children() {
        for child in children.nodes() {
            if child.name().value() != "exclude" {
                return Err(miette::miette!(
                    labels = vec![LabeledSpan::at(child.name().span(), "expected \"exclude\"")],
                    "unexpected node in dependency block: {}",
                    child.name().value()
                )
                .with_source_code(src.clone()));
            }

            let entry = child.entry(0).ok_or_else(|| {
                miette::miette!(
                    labels = vec![LabeledSpan::at(child.span(), "missing value")],
                    "exclude requires a \"G:A\" value"
                )
                .with_source_code(src.clone())
            })?;

            match entry.value() {
                KdlValue::String(s) => {
                    let key: ArtifactKey = s.parse().map_err(|e: anyhow::Error| {
                        miette::miette!(
                            labels = vec![LabeledSpan::at(entry.span(), "invalid artifact key")],
                            "{e}"
                        )
                        .with_source_code(src.clone())
                    })?;
                    exclusions.insert(key);
                }
                _ => {
                    return Err(miette::miette!(
                        labels = vec![LabeledSpan::at(entry.span(), "expected a string")],
                        "exclude value must be a string"
                    )
                    .with_source_code(src.clone()));
                }
            }
        }
    }

    let artifact_type = node
        .entry("type")
        .and_then(|e| match e.value() {
            KdlValue::String(s) => Some(ArtifactType(s.clone())),
            _ => None,
        })
        .unwrap_or_default();

    Ok(Dependency {
        scope,
        artifact_type,
        source,
        exclusions,
    })
}
