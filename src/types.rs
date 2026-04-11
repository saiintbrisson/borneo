use std::str::FromStr;

use anyhow::{Context, ensure};
use globset::{Glob, GlobMatcher};
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize, Hash, PartialEq, Eq, PartialOrd, Ord)]
#[serde(transparent)]
pub struct GroupId(String);

impl GroupId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn to_path(&self) -> String {
        self.0.replace('.', "/")
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for GroupId {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.replace('/', ".")))
    }
}

#[derive(Clone, Debug, Deserialize, Hash, PartialEq, Eq, PartialOrd, Ord)]
#[serde(transparent)]
pub struct ArtifactId(String);

impl FromStr for ArtifactId {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.to_string()))
    }
}

impl ArtifactId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ArtifactId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Debug, Deserialize, Hash, PartialEq, Eq, PartialOrd, Ord)]
#[serde(transparent)]
pub struct ArtifactVersion(String);

impl ArtifactVersion {
    pub fn new(version: impl ToString) -> anyhow::Result<Self> {
        let s = version.to_string();
        ensure!(!s.contains(':'), "colons not allowed in version");
        Ok(Self(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ArtifactVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A pattern that matches a Maven coordinate by groupId and artifactId.
#[derive(Clone)]
pub struct ExclusionPattern {
    raw: String,
    group: GlobMatcher,
    artifact: GlobMatcher,
}

impl ExclusionPattern {
    pub fn matches(&self, coord: &ArtifactCoordinates) -> bool {
        self.group.is_match(coord.group_id().as_str())
            && self.artifact.is_match(coord.artifact_id().as_str())
    }
}

impl FromStr for ExclusionPattern {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (gid, aid) = s
            .split_once(':')
            .ok_or_else(|| anyhow::anyhow!("missing ':' in exclusion pattern: {s}"))?;
        let gid = gid.replace('/', ".");
        let group = Glob::new(&gid)
            .with_context(|| format!("invalid group glob in exclusion: {gid}"))?
            .compile_matcher();
        let artifact = Glob::new(aid)
            .with_context(|| format!("invalid artifact glob in exclusion: {aid}"))?
            .compile_matcher();
        Ok(Self {
            raw: format!("{gid}:{aid}"),
            group,
            artifact,
        })
    }
}

impl std::fmt::Display for ExclusionPattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.raw)
    }
}

impl std::fmt::Debug for ExclusionPattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ExclusionPattern({})", self.raw)
    }
}

impl PartialEq for ExclusionPattern {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw
    }
}

impl Eq for ExclusionPattern {}

impl PartialOrd for ExclusionPattern {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ExclusionPattern {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.raw.cmp(&other.raw)
    }
}

impl std::hash::Hash for ExclusionPattern {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.raw.hash(state);
    }
}

impl<'de> serde::Deserialize<'de> for ExclusionPattern {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

impl serde::Serialize for ExclusionPattern {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

#[derive(Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct ArtifactKey {
    group_id: GroupId,
    artifact_id: ArtifactId,
    artifact_type: ArtifactType,
    classifier: Option<String>,
}

impl std::fmt::Display for ArtifactKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}:{}:{}",
            self.group_id.0, self.artifact_id.0, self.artifact_type
        )?;
        if let Some(c) = &self.classifier {
            write!(f, ":{c}")?;
        }
        Ok(())
    }
}

impl std::fmt::Debug for ArtifactKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ArtifactKey({self})")
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct ArtifactType(String);

impl ArtifactType {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn extension(&self) -> &str {
        match self.0.as_str() {
            "test-jar" | "maven-plugin" | "ejb" | "ejb-client" | "java-source" | "javadoc" => "jar",
            other => other,
        }
    }

    pub fn implied_classifier(&self) -> Option<&str> {
        match self.0.as_str() {
            "test-jar" => Some("tests"),
            "ejb-client" => Some("client"),
            "java-source" => Some("sources"),
            "javadoc" => Some("javadoc"),
            _ => None,
        }
    }
}

impl Default for ArtifactType {
    fn default() -> Self {
        Self("jar".into())
    }
}

impl std::fmt::Display for ArtifactType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct ArtifactCoordinates(GroupId, ArtifactId, ArtifactVersion);

impl ArtifactCoordinates {
    pub(crate) fn new(
        group_id: GroupId,
        artifact_id: ArtifactId,
        version: ArtifactVersion,
    ) -> Self {
        Self(group_id, artifact_id, version)
    }

    pub fn group_id(&self) -> &GroupId {
        &self.0
    }

    pub fn artifact_id(&self) -> &ArtifactId {
        &self.1
    }

    pub fn version(&self) -> &ArtifactVersion {
        &self.2
    }

    pub fn key(&self, artifact_type: &ArtifactType, classifier: Option<&str>) -> ArtifactKey {
        ArtifactKey {
            group_id: self.0.clone(),
            artifact_id: self.1.clone(),
            artifact_type: artifact_type.clone(),
            classifier: classifier.map(|s| s.to_string()),
        }
    }
}

impl FromStr for ArtifactCoordinates {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (gid, rest) = s
            .split_once(':')
            .ok_or_else(|| anyhow::anyhow!("missing ':' in artifact coordinates"))?;
        let (aid, v) = rest
            .rsplit_once(':')
            .ok_or_else(|| anyhow::anyhow!("missing second ':' in artifact coordinates"))?;
        Ok(Self(
            GroupId(gid.replace('/', ".")),
            ArtifactId(aid.to_string()),
            ArtifactVersion(v.to_string()),
        ))
    }
}

impl std::fmt::Debug for ArtifactCoordinates {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ArtifactCoordinates")
            .field(&format_args!("{}:{}:{}", self.0.0, self.1.0, self.2.0))
            .finish()
    }
}

impl std::fmt::Display for ArtifactCoordinates {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("{}:{}:{}", self.0.0, self.1.0, self.2.0))
    }
}

impl<'de> serde::Deserialize<'de> for ArtifactCoordinates {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

impl serde::Serialize for ArtifactCoordinates {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}
