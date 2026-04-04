use std::future::ready;

use camino::Utf8Path;
use futures_util::{StreamExt, TryFutureExt, future::join, stream::FuturesOrdered};

pub mod loader;
pub mod metadata;
pub mod pom;
pub mod xml;

use reqwest::header::CONTENT_TYPE;

use crate::{
    maven::xml::XmlFile,
    types::{ArtifactId, ArtifactVersion, GroupId},
};

pub const MAVEN_REPO: &str = "https://repo1.maven.org/maven2";
pub const MAVEN_METADATA_FILE: &str = "maven-metadata.xml";
pub const MAVEN_POM_SUFFIX: &str = "pom";

#[derive(Clone, Debug, Default)]
pub struct MavenRepositoryClient {
    client: reqwest::Client,
    base: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("request failed: {0}")]
    Reqwest(#[from] reqwest::Error),
    #[error("header {0:?} is missing from response")]
    MissingHeader(reqwest::header::HeaderName),
    #[error("invalid content type, expected {0:?}, got {1:?}")]
    InvalidContentType(String, reqwest::header::HeaderValue),
    #[error("checksum does not exist for url {0:?}")]
    ChecksumNotFound(String),
    #[error("checksum for url {0:?} failed with method {1}")]
    ChecksumFailed(String, &'static str),
    #[error("server returned invalid hex for url {0:?}: {1}")]
    InvalidChecksum(String, hex::FromHexError),
    #[error("failed to parse metadata: {0}")]
    ParseError(String),
    #[error("io error: {0}")]
    IoError(#[from] std::io::Error),
}

pub struct Asset {
    sha256: Vec<u8>,
}

pub struct Content {
    content: bytes::Bytes,
}

impl MavenRepositoryClient {
    pub fn with_client(client: reqwest::Client, base: String) -> Self {
        Self { client, base }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    async fn fetch_digest(&self, url: &str) -> Result<Digest, ClientError> {
        let digests = DIGESTS
            .iter()
            .map(|method| async {
                let resp = self
                    .client
                    .get(format!("{url}.{}", method.name))
                    .send()
                    .await?
                    .error_for_status()?;
                let val = resp.text().await?;
                Result::<_, ClientError>::Ok(Digest {
                    name: method.name,
                    state: (method.state)(),
                    reference: hex::decode(&val)
                        .map_err(|err| ClientError::InvalidChecksum(url.to_string(), err))?,
                })
            })
            .collect::<FuturesOrdered<_>>()
            .filter(|res| ready(res.is_ok()))
            .next()
            .await;

        Ok(digests
            .ok_or_else(|| ClientError::ChecksumNotFound(url.to_string()))?
            .expect("is_ok called"))
    }

    pub async fn artifact_metadata(
        &self,
        gid: &GroupId,
        aid: &ArtifactId,
        version: Option<&ArtifactVersion>,
    ) -> Result<metadata::ArtifactMetadata, ClientError> {
        let resp = self
            .client
            .get(format!(
                "{}/{}/{}/{}{MAVEN_METADATA_FILE}",
                self.base,
                gid.to_path(),
                aid.as_str(),
                if let Some(version) = version {
                    format!("{}/", version.as_str())
                } else {
                    String::new()
                }
            ))
            .send()
            .await?
            .error_for_status()?;
        let ty = resp
            .headers()
            .get(CONTENT_TYPE)
            .ok_or_else(|| ClientError::MissingHeader(CONTENT_TYPE))?;
        if !matches!(ty.as_bytes(), b"text/xml" | b"application/xml") {
            return Err(ClientError::InvalidContentType(
                "text/xml or application/xml".to_string(),
                ty.clone(),
            ));
        }

        let txt = resp.text().await?;

        quick_xml::de::from_str(&txt).map_err(|e| ClientError::ParseError(e.to_string()))
    }

    async fn execute_request(
        &self,
        path: &str,
        accepted_mimes: &'static [&[u8]],
        status_key: Option<&str>,
    ) -> Result<Content, ClientError> {
        use std::sync::atomic::{AtomicU32, Ordering};

        let attempt = AtomicU32::new(0);
        let backoff = backoff::ExponentialBackoffBuilder::new()
            .with_initial_interval(std::time::Duration::from_millis(200))
            .with_multiplier(3.0)
            .with_max_elapsed_time(Some(std::time::Duration::from_secs(30)))
            .build();

        backoff::future::retry(backoff, || async {
            let n = attempt.fetch_add(1, Ordering::Relaxed);
            let timeout = match n {
                0 => std::time::Duration::from_secs(5),
                1 => std::time::Duration::from_secs(10),
                _ => std::time::Duration::from_secs(20),
            };

            self.try_execute_request(path, accepted_mimes, timeout)
                .await
                .map_err(|e| match &e {
                    ClientError::Reqwest(_) | ClientError::IoError(_) => {
                        let status = crate::status::StatusHandle::get();
                        if let Some(key) = status_key {
                            status.update(key, format!("retrying {key} ({e})"));
                        }
                        backoff::Error::transient(e)
                    }
                    _ => backoff::Error::permanent(e),
                })
        })
        .await
    }

    async fn try_execute_request(
        &self,
        path: &str,
        accepted_mimes: &'static [&[u8]],
        timeout: std::time::Duration,
    ) -> Result<Content, ClientError> {
        let url = format!("{}/{path}", self.base);
        let req = self
            .client
            .get(&url)
            .timeout(timeout)
            .send()
            .map_err(ClientError::from);

        let (resp, digest) = join(req, self.fetch_digest(&url)).await;
        let resp = resp?.error_for_status()?;

        let ty = resp
            .headers()
            .get(CONTENT_TYPE)
            .ok_or_else(|| ClientError::MissingHeader(CONTENT_TYPE))?;

        if !accepted_mimes.is_empty() && !accepted_mimes.contains(&ty.as_bytes()) {
            return Err(ClientError::InvalidContentType(
                path.to_string(),
                ty.clone(),
            ));
        }

        let content = resp.bytes().await?;

        match digest {
            Err(ClientError::ChecksumNotFound(_)) => Ok(Content { content }),
            digest => {
                let digest = digest?;
                let name = digest.name;
                digest
                    .check(&content)
                    .map_err(|_| ClientError::ChecksumFailed(url.clone(), name))?;
                Ok(Content { content })
            }
        }
    }

    pub async fn fetch_xml(
        &self,
        path: &str,
        status_key: Option<&str>,
    ) -> Result<XmlFile, ClientError> {
        const ACCEPTED_MIMES: &[&[u8]] = &[
            b"text/xml",
            b"application/xml",
            b"application/x-maven-pom+xml",
        ];

        let content = self.execute_request(path, ACCEPTED_MIMES, status_key).await?;
        let txt = String::from_utf8(content.content.to_vec())
            .map_err(|e| ClientError::ParseError(e.to_string()))?;

        tokio::fs::write(
            Utf8Path::new("build/cache/").join(Utf8Path::new(path).file_name().unwrap()),
            &txt,
        )
        .await
        .unwrap();

        XmlFile::from_str(&txt).map_err(|e| ClientError::ParseError(e.to_string()))
    }

    pub async fn download_asset(
        &self,
        path: &str,
        out: &Utf8Path,
        status_key: Option<&str>,
    ) -> Result<Asset, ClientError> {
        let content = self.execute_request(path, &[], status_key).await?;
        let sha256 = <sha2::Sha256 as sha2::Digest>::digest(&content.content).to_vec();
        tokio::fs::write(out, &content.content).await?;

        Ok(Asset { sha256 })
    }
}

struct DigestMethod {
    name: &'static str,
    state: fn() -> DigestState,
}

macro_rules! decl_digest {
    ($(($name:ident, $mod:ident, $ty:ident)),+) => {
        const DIGESTS: &[DigestMethod] = &[
            $(
                DigestMethod {
                    name: stringify!($name),
                    state: {
                        fn $name() -> DigestState {
                            DigestState::$ty($mod::$ty::default())
                        }
                        $name
                    },
                }
            ),+
        ];
        pub enum DigestState {
            $($ty($mod::$ty)),+
        }

        impl DigestState {
            pub fn update(&mut self, data: &[u8]) {
                match self {
                    $(Self::$ty(state) => <$mod::$ty as $mod::Digest>::update(state, data)),+
                }
            }

            pub fn finish(self) -> Vec<u8> {
                match self {
                    $(Self::$ty(state) => <$mod::$ty as $mod::Digest>::finalize(state).to_vec()),+
                }
            }
        }

    };
}

decl_digest![
    (sha512, sha2, Sha512),
    (sha256, sha2, Sha256),
    (sha1, sha1, Sha1),
    (md5, md5, Md5)
];

struct Digest {
    name: &'static str,
    state: DigestState,
    reference: Vec<u8>,
}

impl Digest {
    fn check(mut self, data: &[u8]) -> Result<Vec<u8>, ()> {
        self.update(data);

        let digest = self.state.finish();
        if digest == self.reference {
            Ok(digest)
        } else {
            Err(())
        }
    }

    fn update(&mut self, data: &[u8]) {
        self.state.update(data);
    }
}
