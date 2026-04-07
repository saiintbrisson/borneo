use std::collections::{BTreeMap, BTreeSet};

use anyhow::Context;
use camino::Utf8PathBuf;
use kdl::{KdlDocument, KdlEntry, KdlNode, KdlValue};

use crate::manifest::{PomScope, Scope};
use crate::types::{ArtifactCoordinates, ArtifactType, ExclusionKey};

pub struct Lock {
    pub version: String,
    pub repositories: BTreeSet<String>,
    pub artifacts: BTreeSet<LockArtifact>,
    pub local: BTreeSet<LocalArtifact>,
}

#[derive(Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct LocalArtifact {
    pub path: String,
    pub checksum: Checksum,
}

#[derive(Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct LockArtifact {
    pub coord: ArtifactCoordinates,
    pub classifier: Option<String>,
    pub artifact_type: ArtifactType,
    pub source: String,
    pub artifact_path: Utf8PathBuf,
    pub checksum: Checksum,
    pub effective_scope: Scope,
    pub depth: usize,
    pub position: Vec<usize>,
    pub dependencies: BTreeMap<ArtifactCoordinates, PomScope>,
    pub exclusions: BTreeSet<ExclusionKey>,
}

#[derive(Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct Checksum(Vec<u8>);

impl Checksum {
    pub fn provided(digest: Vec<u8>) -> Self {
        Self(digest)
    }

    pub fn digest(&self) -> &[u8] {
        &self.0
    }
}

impl std::fmt::Display for Checksum {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&hex::encode(&self.0))
    }
}

impl std::str::FromStr for Checksum {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(hex::decode(s).context("invalid hex in checksum")?))
    }
}

impl Lock {
    pub fn parse(source: &str) -> anyhow::Result<Self> {
        let doc: KdlDocument = source.parse().context("failed to parse lock file")?;

        let version = doc
            .get_arg("version")
            .and_then(|v| match v {
                KdlValue::String(s) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "1".to_string());

        let mut repositories = BTreeSet::new();
        if let Some(repos_node) = doc.get("repositories")
            && let Some(children) = repos_node.children()
        {
            for node in children.nodes() {
                repositories.insert(node.name().value().to_string());
            }
        }

        let mut artifacts = BTreeSet::new();
        let mut local = BTreeSet::new();
        for node in doc.nodes() {
            match node.name().value() {
                "artifact" => artifacts.insert(parse_lock_artifact(node)?),
                "local" => local.insert(parse_local_artifact(node)?),
                _ => continue,
            };
        }

        Ok(Lock {
            version,
            repositories,
            artifacts,
            local,
        })
    }

    pub fn to_kdl(&self) -> String {
        let mut doc = KdlDocument::new();

        let mut version_node = KdlNode::new("version");
        version_node
            .entries_mut()
            .push(KdlEntry::new(KdlValue::String(self.version.clone())));
        doc.nodes_mut().push(version_node);

        if !self.repositories.is_empty() {
            let mut repos_node = KdlNode::new("repositories");
            let mut children = KdlDocument::new();
            for url in &self.repositories {
                children.nodes_mut().push(KdlNode::new(url.as_str()));
            }
            repos_node.set_children(children);
            doc.nodes_mut().push(repos_node);
        }

        for artifact in &self.artifacts {
            doc.nodes_mut().push(artifact.to_kdl_node());
        }

        for local in &self.local {
            let mut node = KdlNode::new("local");
            push_prop(&mut node, "path", &local.path);
            push_prop(&mut node, "checksum", &local.checksum.to_string());
            doc.nodes_mut().push(node);
        }

        doc.autoformat();
        doc.to_string()
    }
}

fn parse_lock_artifact(node: &KdlNode) -> anyhow::Result<LockArtifact> {
    let coord_str = node
        .entry(0)
        .and_then(|e| match e.value() {
            KdlValue::String(s) => Some(s.as_str()),
            _ => None,
        })
        .context("artifact node missing coord")?;
    let coord: ArtifactCoordinates = coord_str.parse()?;

    let source = node_prop_str(node, "source").context("artifact missing source")?;
    let artifact_path =
        Utf8PathBuf::from(node_prop_str(node, "path").context("artifact missing path")?);
    let checksum: Checksum = node_prop_str(node, "checksum")
        .context("artifact missing checksum")?
        .parse()?;

    let classifier = node_prop_str(node, "classifier");
    let artifact_type = node_prop_str(node, "type")
        .map(ArtifactType::new)
        .unwrap_or_default();

    let effective_scope: Scope = node_prop_str(node, "scope")
        .and_then(|s| s.parse().ok())
        .unwrap_or(Scope::Compile);

    let depth = node
        .entry("depth")
        .and_then(|e| match e.value() {
            KdlValue::Integer(n) => Some(*n as usize),
            _ => None,
        })
        .unwrap_or(0);

    let position = node_prop_str(node, "position")
        .map(|s| {
            s.split('.')
                .filter(|p| !p.is_empty())
                .filter_map(|p| p.parse().ok())
                .collect()
        })
        .unwrap_or_default();

    let mut dependencies = BTreeMap::new();
    let mut exclusions = BTreeSet::new();

    if let Some(children) = node.children() {
        for child in children.nodes() {
            let val = child
                .entry(0)
                .and_then(|e| match e.value() {
                    KdlValue::String(s) => Some(s.as_str()),
                    _ => None,
                })
                .with_context(|| format!("{} missing value", child.name().value()))?;

            match child.name().value() {
                "compile" => {
                    dependencies.insert(val.parse()?, PomScope::Compile);
                }
                "runtime" => {
                    dependencies.insert(val.parse()?, PomScope::Runtime);
                }
                "exclude" => {
                    exclusions.insert(val.parse()?);
                }
                other => anyhow::bail!("unexpected node in artifact: {other}"),
            }
        }
    }

    Ok(LockArtifact {
        coord,
        classifier,
        artifact_type,
        source,
        artifact_path,
        checksum,
        effective_scope,
        depth,
        position,
        dependencies,
        exclusions,
    })
}

fn parse_local_artifact(node: &KdlNode) -> anyhow::Result<LocalArtifact> {
    let path = node_prop_str(node, "path").context("local node missing path")?;
    let checksum: Checksum = node_prop_str(node, "checksum")
        .context("local node missing checksum")?
        .parse()?;
    Ok(LocalArtifact { path, checksum })
}

fn node_prop_str(node: &KdlNode, key: &str) -> Option<String> {
    node.entry(key).and_then(|e| match e.value() {
        KdlValue::String(s) => Some(s.clone()),
        _ => None,
    })
}

impl LockArtifact {
    fn to_kdl_node(&self) -> KdlNode {
        let mut node = KdlNode::new("artifact");
        node.entries_mut()
            .push(KdlEntry::new(KdlValue::String(self.coord.to_string())));

        push_prop(&mut node, "source", &self.source);
        push_prop(&mut node, "path", self.artifact_path.as_str());
        push_prop(&mut node, "checksum", &self.checksum.to_string());
        push_prop(&mut node, "scope", &self.effective_scope.to_string());

        let mut depth_entry = KdlEntry::new(KdlValue::Integer(self.depth as i128));
        depth_entry.set_name(Some(kdl::KdlIdentifier::from("depth")));
        node.entries_mut().push(depth_entry);

        if !self.position.is_empty() {
            let pos_str = self
                .position
                .iter()
                .map(|p| p.to_string())
                .collect::<Vec<_>>()
                .join(".");
            push_prop(&mut node, "position", &pos_str);
        }

        if let Some(c) = &self.classifier {
            push_prop(&mut node, "classifier", c);
        }
        if self.artifact_type.extension() != "jar" {
            push_prop(&mut node, "type", self.artifact_type.extension());
        }

        let has_children = !self.dependencies.is_empty() || !self.exclusions.is_empty();
        if has_children {
            let mut children = KdlDocument::new();
            for (coord, pom_scope) in &self.dependencies {
                let mut dep_node = KdlNode::new(pom_scope.to_string().as_str());
                dep_node
                    .entries_mut()
                    .push(KdlEntry::new(KdlValue::String(coord.to_string())));
                children.nodes_mut().push(dep_node);
            }
            for excl in &self.exclusions {
                let mut excl_node = KdlNode::new("exclude");
                excl_node
                    .entries_mut()
                    .push(KdlEntry::new(KdlValue::String(excl.to_string())));
                children.nodes_mut().push(excl_node);
            }
            node.set_children(children);
        }

        node
    }
}

fn push_prop(node: &mut KdlNode, key: &str, value: &str) {
    let mut entry = KdlEntry::new(KdlValue::String(value.to_string()));
    entry.set_name(Some(kdl::KdlIdentifier::from(key)));
    node.entries_mut().push(entry);
}
