use serde::Deserialize;

use crate::types::{ArtifactId, ArtifactVersion, ExclusionPattern, GroupId};

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pom {
    pub dependency_management: Option<DependencyManagement>,

    #[serde(default)]
    pub dependencies: Vec<Dependency>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Parent {
    pub group_id: GroupId,
    pub artifact_id: ArtifactId,
    pub version: Option<ArtifactVersion>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DependencyManagement {
    pub dependencies: Vec<Dependency>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Dependency {
    pub group_id: GroupId,
    pub artifact_id: ArtifactId,
    pub version: Option<ArtifactVersion>,

    #[serde(default)]
    pub scope: DependencyScope,

    #[serde(default = "default_dependency_type")]
    pub r#type: String,

    #[serde(default)]
    pub classifier: Option<String>,

    #[serde(default)]
    pub optional: bool,

    #[serde(default)]
    pub exclusions: Vec<Exclusion>,
}

fn default_dependency_type() -> String {
    "jar".to_string()
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Exclusion {
    pub group_id: GroupId,
    pub artifact_id: ArtifactId,
}

impl Exclusion {
    pub fn to_pattern(&self) -> anyhow::Result<ExclusionPattern> {
        format!("{}:{}", self.group_id.as_str(), self.artifact_id.as_str()).parse()
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
