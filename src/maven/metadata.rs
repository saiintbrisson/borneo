#![allow(dead_code)]

use serde::Deserialize;

use crate::types::{ArtifactId, ArtifactVersion, GroupId};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactMetadata {
    pub group_id: GroupId,
    pub artifact_id: ArtifactId,

    pub versioning: ArtifactVersioning,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactVersioning {
    pub versions: Option<ArtifactVersioningEntry>,
    pub snapshot: Option<ArtifactVersioningSnapshot>,
    pub last_updated: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactVersioningSnapshot {
    pub timestamp: String,
    pub build_number: String,
}

#[derive(Debug, Deserialize)]
pub struct ArtifactVersioningEntry {
    #[serde(rename = "version")]
    pub versions: Vec<ArtifactVersion>,
}
