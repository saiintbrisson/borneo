#![allow(dead_code)]

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use camino::Utf8PathBuf;
use serde::{Deserialize, Deserializer};

use crate::types::{ArtifactId, ArtifactKey, ArtifactVersion, GroupId};

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pom {
    pub parent: Option<Parent>,

    pub group_id: Option<GroupId>,
    pub artifact_id: ArtifactId,
    pub version: Option<ArtifactVersion>,

    pub name: Option<String>,
    pub description: Option<String>,
    pub url: Option<String>,

    #[serde(default)]
    pub properties: HashMap<String, String>,

    pub dependency_management: Option<DependencyManagement>,

    #[serde(default)]
    pub dependencies: Vec<Dependency>,
}

impl Pom {
    pub fn group_id(&self) -> &GroupId {
        self.group_id
            .as_ref()
            .or_else(|| Some(&self.parent.as_ref()?.group_id))
            .expect("POM must have group_id")
    }

    pub fn version(&self) -> &ArtifactVersion {
        self.version
            .as_ref()
            .or_else(|| self.parent.as_ref()?.version.as_ref())
            .expect("POM must have version")
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Parent {
    pub group_id: GroupId,
    pub artifact_id: ArtifactId,
    pub version: Option<ArtifactVersion>,
    pub relative_path: Option<Utf8PathBuf>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DependencyManagement {
    pub dependencies: Vec<Dependency>,
}

impl Pom {
    pub fn to_jar_path(&self, suffix: Option<&str>) -> PathBuf {
        let path = &format!(
            "{}-{}{}.jar",
            self.artifact_id.as_str(),
            self.version().as_str(),
            suffix.unwrap_or("")
        );
        let path: &Path = path.as_ref();
        path.to_path_buf()
    }
}

fn unwrap_dependencies<'de, D>(deserializer: D) -> Result<Vec<Dependency>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    struct Dependencies {
        #[serde(default)]
        dependency: Vec<Dependency>,
    }
    Ok(Dependencies::deserialize(deserializer)?.dependency)
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Dependency {
    pub group_id: GroupId,
    pub artifact_id: ArtifactId,
    pub version: Option<ArtifactVersion>,

    #[serde(default)]
    pub r#type: DependencyType,

    #[serde(default)]
    pub scope: DependencyScope,

    pub system_path: Option<String>,

    #[serde(default)]
    pub exclusions: Vec<Exclusion>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Exclusion {
    pub group_id: GroupId,
    pub artifact_id: ArtifactId,
}

impl Exclusion {
    pub fn to_key(&self) -> ArtifactKey {
        ArtifactKey::new(self.group_id.clone(), self.artifact_id.clone())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct DependencyType(String);

impl Default for DependencyType {
    fn default() -> Self {
        Self("jar".into())
    }
}

impl DependencyType {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DependencyScope {
    #[default]
    Compile,
    Provided,
    Runtime,
    Test,
    System,
    Import,
}
