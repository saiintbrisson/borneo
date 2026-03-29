use std::str::FromStr;

use anyhow::ensure;
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

#[derive(Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct ArtifactKey(GroupId, ArtifactId);

impl ArtifactKey {
    pub fn new(group_id: GroupId, artifact_id: ArtifactId) -> Self {
        Self(group_id, artifact_id)
    }
}

impl FromStr for ArtifactKey {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (gid, aid) = s
            .split_once(':')
            .ok_or_else(|| anyhow::anyhow!("missing ':' in artifact key"))?;
        Ok(Self(
            GroupId(gid.replace('/', ".")),
            ArtifactId(aid.to_string()),
        ))
    }
}

impl std::fmt::Display for ArtifactKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.0.0, self.1.0)
    }
}

impl std::fmt::Debug for ArtifactKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ArtifactKey({}:{})", self.0.0, self.1.0)
    }
}

impl<'de> serde::Deserialize<'de> for ArtifactKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

impl serde::Serialize for ArtifactKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
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

    pub fn key(&self) -> ArtifactKey {
        ArtifactKey(self.0.clone(), self.1.clone())
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
