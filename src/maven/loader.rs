use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::Arc,
};

use anyhow::Context as _;

use camino::{Utf8Path, Utf8PathBuf};
use dashmap::DashMap;
use futures_util::{StreamExt, stream::FuturesUnordered};
use tokio::sync::watch;

use crate::{
    manifest::{PomDependency, PomScope, Scope, lock::Lock},
    maven::{
        ClientError, MAVEN_POM_SUFFIX, MavenRepositoryClient,
        pom::{Dependency, DependencyScope, Parent, Pom},
        xml::{XmlFile, XmlNode},
    },
    status::StatusHandle,
    types::{ArtifactCoordinates, ArtifactKey, ArtifactType, ArtifactVersion, ExclusionPattern},
};

type Cache<K, V> = DashMap<K, CacheEntry<V>>;
type CacheValue<V> = (Option<Arc<MavenRepositoryClient>>, Arc<V>);

type Rank = (usize, Vec<usize>);

#[derive(Clone)]
enum CacheEntry<V> {
    Ready(CacheValue<V>),
    Failed(Arc<anyhow::Error>),
    Loading(watch::Receiver<Option<CacheValue<V>>>),
}

#[derive(Clone)]
struct ArtifactSlot {
    rank: Rank,
    coord: ArtifactCoordinates,
    entry: CacheEntry<ResolvedArtifact>,
}

#[derive(Clone, Debug)]
pub struct ResolvedArtifact {
    pub coord: ArtifactCoordinates,
    pub source: String,
    pub artifact_path: Utf8PathBuf,
    pub artifact_type: ArtifactType,
    pub classifier: Option<String>,
    pub effective_scope: Scope,
    pub dependencies: BTreeMap<ArtifactCoordinates, PomDependency>,
}

impl ResolvedArtifact {
    pub fn key(&self) -> ArtifactKey {
        self.coord
            .key(&self.artifact_type, self.classifier.as_deref())
    }
}

#[derive(Clone)]
pub struct LoaderBranch {
    pub depth: usize,
    effective_scope: Scope,
    exclusions: BTreeSet<ExclusionPattern>,
    position: Vec<usize>,
}

impl LoaderBranch {
    pub fn new(exclusions: BTreeSet<ExclusionPattern>, position: usize, scope: Scope) -> Self {
        Self {
            depth: 0,
            effective_scope: scope,
            exclusions,
            position: vec![position],
        }
    }

    fn child(
        &self,
        index: usize,
        extra_exclusions: impl IntoIterator<Item = ExclusionPattern>,
        pom_scope: PomScope,
    ) -> Self {
        let mut exclusions = self.exclusions.clone();
        exclusions.extend(extra_exclusions);
        let mut position = self.position.clone();
        position.push(index);
        Self {
            depth: self.depth + 1,
            effective_scope: crate::manifest::mediate(self.effective_scope, pom_scope),
            exclusions,
            position,
        }
    }

    fn is_excluded(&self, coord: &ArtifactCoordinates) -> bool {
        self.exclusions.iter().any(|p| p.matches(coord))
    }
}

pub struct MavenLoader {
    super_pom: XmlFile,
    client: reqwest::Client,
    repos: Vec<Arc<MavenRepositoryClient>>,
    strategy: crate::manifest::RepoStrategy,

    channel: (watch::Sender<Option<()>>, watch::Receiver<Option<()>>),
    artifacts: Arc<DashMap<ArtifactKey, ArtifactSlot>>,

    metas: Cache<ArtifactCoordinates, Utf8PathBuf>,
    files: Cache<String, XmlFile>,
    pom_deps: Cache<ArtifactCoordinates, BTreeMap<ArtifactCoordinates, PomDependency>>,
}

impl MavenLoader {
    pub fn new(
        repos: &[crate::manifest::RepoEntry],
        strategy: crate::manifest::RepoStrategy,
    ) -> Arc<Self> {
        let super_pom = XmlFile::from_str(include_str!("./pom-4.1.0.xml"))
            .expect("built-in super POM is valid");
        let client = reqwest::Client::builder()
            .user_agent(format!("borneo/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("failed to build HTTP client");

        let all_repos: Vec<_> = repos
            .iter()
            .map(|entry| {
                Arc::new(MavenRepositoryClient::with_client(
                    client.clone(),
                    entry.url.clone(),
                    entry.checksum_policy,
                ))
            })
            .collect();

        Arc::new(Self {
            super_pom,
            repos: all_repos,
            client,
            strategy,

            channel: watch::channel(None),
            artifacts: Default::default(),

            metas: Default::default(),
            files: Default::default(),
            pom_deps: Default::default(),
        })
    }

    pub fn seed_from_lock(&self, lock: &Lock) {
        let mut repos = HashMap::new();

        for la in &lock.artifacts {
            let repo = repos
                .entry(la.source.clone())
                .or_insert_with(|| {
                    Arc::new(MavenRepositoryClient::with_client(
                        self.client.clone(),
                        la.source.clone(),
                        Default::default(),
                    ))
                })
                .clone();

            self.metas.insert(
                la.coord.clone(),
                CacheEntry::Ready((Some(repo.clone()), Arc::new(la.artifact_path.clone()))),
            );

            self.pom_deps.insert(
                la.coord.clone(),
                CacheEntry::Ready((Some(repo), Arc::new(la.dependencies.clone()))),
            );
        }
    }

    async fn load_artifact(
        self: &Arc<Self>,
        coord: ArtifactCoordinates,
        artifact_type: ArtifactType,
        classifier: Option<String>,
        branch: LoaderBranch,
    ) -> anyhow::Result<()> {
        let classifier =
            classifier.or_else(|| artifact_type.implied_classifier().map(|s| s.to_string()));
        let key = coord.key(&artifact_type, classifier.as_deref());
        let rank: Rank = (branch.depth, branch.position.clone());

        let tx = match self.artifacts.entry(key.clone()) {
            dashmap::Entry::Occupied(e) if e.get().coord == coord => return Ok(()),
            dashmap::Entry::Occupied(e) if e.get().rank <= rank => return Ok(()),
            dashmap::Entry::Occupied(mut e) => {
                let (tx, rx) = watch::channel(None);
                e.insert(ArtifactSlot {
                    rank: rank.clone(),
                    coord: coord.clone(),
                    entry: CacheEntry::Loading(rx),
                });
                tx
            }
            dashmap::Entry::Vacant(e) => {
                let (tx, rx) = watch::channel(None);
                e.insert(ArtifactSlot {
                    rank: rank.clone(),
                    coord: coord.clone(),
                    entry: CacheEntry::Loading(rx),
                });
                tx
            }
        };

        let (repo, dep_coords) = self.resolve_pom_deps(&coord, &branch).await?;
        let artifact_path = self.resolve_coord(&coord).await?.1;

        let repo = repo.context("no repo found")?;
        let source = repo.base().to_string();

        let artifact = Arc::new(ResolvedArtifact {
            source,
            coord: coord.clone(),
            artifact_path: artifact_path.as_ref().clone(),
            artifact_type,
            classifier,
            effective_scope: branch.effective_scope,
            dependencies: dep_coords.as_ref().clone(),
        });

        let entry = (Some(repo), artifact);
        let slot = ArtifactSlot {
            rank: rank.clone(),
            coord: coord.clone(),
            entry: CacheEntry::Ready(entry.clone()),
        };
        let _ = tx.send(Some(entry));

        match self.artifacts.entry(key) {
            dashmap::Entry::Occupied(mut entry) if rank <= entry.get().rank => {
                entry.insert(slot);
            }
            dashmap::Entry::Vacant(entry) => {
                entry.insert(slot);
            }
            _ => {}
        }

        Ok(())
    }

    fn extract_pom_deps(
        coord: &ArtifactCoordinates,
        pom: &XmlFile,
    ) -> anyhow::Result<BTreeMap<ArtifactCoordinates, PomDependency>> {
        let mut pom: Pom = pom
            .read_as()
            .map_err(|e| anyhow::anyhow!("failed to parse POM for {coord}: {e}"))?;

        if let Some(dependency_management) = &pom.dependency_management
            && pom.dependencies.iter().any(|dep| dep.version.is_none())
        {
            let depm = dependency_management.clone();
            for dep in depm.dependencies {
                let found = pom.dependencies.iter_mut().find(|o| {
                    o.version.is_none()
                        && o.group_id == dep.group_id
                        && o.artifact_id == dep.artifact_id
                });

                if let Some(found) = found {
                    found.version = dep.version;
                }
            }
        }

        let mut deps = BTreeMap::new();

        for dep in &pom.dependencies {
            if dep.optional {
                continue;
            }

            let scope = match dep.scope {
                DependencyScope::Compile => PomScope::Compile,
                DependencyScope::Runtime => PomScope::Runtime,
                _ => continue,
            };

            let artifact_type = ArtifactType::new(&dep.r#type);
            let classifier = dep
                .classifier
                .clone()
                .or_else(|| artifact_type.implied_classifier().map(|s| s.to_string()));

            let coord = ArtifactCoordinates::new(
                dep.group_id.clone(),
                dep.artifact_id.clone(),
                dep.version.clone().with_context(|| {
                    format!(
                        "{}:{} of {coord} has no version",
                        dep.group_id.as_str(),
                        dep.artifact_id.as_str()
                    )
                })?,
            );

            let exclusions = dep
                .exclusions
                .iter()
                .map(|e| e.to_pattern())
                .collect::<anyhow::Result<_>>()?;

            deps.insert(
                coord,
                PomDependency {
                    scope,
                    artifact_type,
                    classifier,
                    exclusions,
                },
            );
        }

        Ok(deps)
    }

    async fn resolve_pom_deps(
        self: &Arc<Self>,
        coord: &ArtifactCoordinates,
        branch: &LoaderBranch,
    ) -> anyhow::Result<CacheValue<BTreeMap<ArtifactCoordinates, PomDependency>>> {
        let entry = self
            .get_or_load(&self.pom_deps, coord.clone(), async {
                StatusHandle::get().resolving(coord);

                let (repo, artifact_path) = self.resolve_coord(coord).await?;

                let (repo, pom) = self
                    .fetch_pom(&repo, &artifact_path, Some(&coord.to_string()))
                    .await?;

                let deps = Self::extract_pom_deps(coord, &pom)?;

                StatusHandle::get().resolved(coord);
                Ok((repo, Arc::new(deps)))
            })
            .await?;

        for (i, (dep_coord, pom_dep)) in entry.1.iter().enumerate() {
            if branch.is_excluded(dep_coord) {
                continue;
            }

            let child = branch.child(i, pom_dep.exclusions.iter().cloned(), pom_dep.scope);
            self.clone().spawn_load_artifact(
                dep_coord.clone(),
                pom_dep.artifact_type.clone(),
                pom_dep.classifier.clone(),
                child,
            );
        }

        Ok(entry)
    }

    pub fn spawn_load_artifact(
        self: Arc<Self>,
        coord: ArtifactCoordinates,
        artifact_type: ArtifactType,
        classifier: Option<String>,
        branch: LoaderBranch,
    ) {
        let artifacts = self.artifacts.clone();
        let rank: Rank = (branch.depth, branch.position.clone());
        tokio::spawn(async move {
            let load_artifact = self
                .load_artifact(
                    coord.clone(),
                    artifact_type.clone(),
                    classifier.clone(),
                    branch,
                )
                .await;

            if let Err(e) = load_artifact {
                StatusHandle::get().resolved(&coord);

                let key = coord.key(&artifact_type, classifier.as_deref());
                let err = Arc::new(e);
                match artifacts.entry(key) {
                    dashmap::Entry::Occupied(existing)
                        if matches!(existing.get().entry, CacheEntry::Ready(_))
                            && existing.get().rank <= rank => {}
                    dashmap::Entry::Occupied(mut existing) => {
                        existing.insert(ArtifactSlot {
                            rank,
                            coord,
                            entry: CacheEntry::Failed(err),
                        });
                    }
                    dashmap::Entry::Vacant(vacant) => {
                        vacant.insert(ArtifactSlot {
                            rank,
                            coord,
                            entry: CacheEntry::Failed(err),
                        });
                    }
                }
            }

            // DO NOT REMOVE: we want to guarantee Self's channel is dropped as late possible
            drop(self);
        });
    }

    pub async fn into_resolved(self: Arc<Self>) -> anyhow::Result<ResolvedDependencies> {
        let tx = self.channel.0.clone();
        let artifacts = self.artifacts.clone();
        drop(self);

        tx.closed().await;

        let map = Arc::try_unwrap(artifacts).unwrap_or_else(|arc| (*arc).clone());

        let mut result = Vec::new();
        let mut slot_map = HashMap::new();

        for (key, slot) in map.into_iter() {
            match slot.entry {
                CacheEntry::Ready((repo, artifact)) => {
                    let repo =
                        repo.context(format!("resolved artifact {} must have a source repo", key))?;
                    result.push(artifact.clone());
                    slot_map.insert(key, (repo, artifact));
                }
                CacheEntry::Failed(error) => anyhow::bail!("failed to resolve {key}: {error:?}"),
                CacheEntry::Loading(_) => {
                    anyhow::bail!("artifact {key} did not finish processing, please file a issue")
                }
            }
        }

        Ok(ResolvedDependencies {
            artifacts: result,
            slot_map,
        })
    }
}

pub struct ResolvedDependencies {
    pub artifacts: Vec<Arc<ResolvedArtifact>>,
    pub(crate) slot_map: HashMap<ArtifactKey, (Arc<MavenRepositoryClient>, Arc<ResolvedArtifact>)>,
}

impl ResolvedDependencies {
    pub fn from_lock(lock: &Lock, client: &reqwest::Client) -> Self {
        let mut repos: HashMap<_, _> = HashMap::new();
        let mut artifacts = Vec::new();
        let mut slot_map = HashMap::new();

        for la in &lock.artifacts {
            let repo = repos
                .entry(la.source.clone())
                .or_insert_with(|| {
                    Arc::new(MavenRepositoryClient::with_client(
                        client.clone(),
                        la.source.clone(),
                        Default::default(),
                    ))
                })
                .clone();

            let artifact = Arc::new(ResolvedArtifact {
                coord: la.coord.clone(),
                source: la.source.clone(),
                artifact_path: la.artifact_path.clone(),
                artifact_type: la.artifact_type.clone(),
                classifier: la.classifier.clone(),
                effective_scope: la.effective_scope,
                dependencies: la.dependencies.clone(),
            });

            let key = artifact.key();
            artifacts.push(artifact.clone());
            slot_map.insert(key, (repo, artifact));
        }

        Self {
            artifacts,
            slot_map,
        }
    }

    pub async fn download_artifact(
        &self,
        artifact: &ResolvedArtifact,
        out: &Utf8Path,
    ) -> anyhow::Result<Vec<u8>> {
        let key = artifact.key();
        let (repo, _) = self
            .slot_map
            .get(&key)
            .context("artifact must be resolved first")?;
        let ext = artifact.artifact_type.extension();
        let path = if let Some(classifier) = &artifact.classifier {
            let base = artifact.artifact_path.as_str();
            Utf8PathBuf::from(format!("{base}-{classifier}.{ext}"))
        } else {
            artifact.artifact_path.with_added_extension(ext)
        };
        let coord = &artifact.coord;
        let dl_key = format!("dl:{coord}");
        let asset = repo
            .download_asset(path.as_str(), out, Some(&dl_key))
            .await
            .with_context(|| format!("failed to download {coord}"))?;

        Ok(asset.sha256)
    }
}

pub fn verify_cached(path: &std::path::Path, expected: &[u8]) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    let hash = <sha2::Sha256 as sha2::Digest>::digest(&bytes);
    hash.as_slice() == expected
}

impl MavenLoader {
    async fn get_or_load<K, V, F>(
        &self,
        map: &DashMap<K, CacheEntry<V>>,
        key: K,
        loader: F,
    ) -> anyhow::Result<CacheValue<V>>
    where
        K: Clone + std::fmt::Debug + std::hash::Hash + Eq,
        F: Future<Output = anyhow::Result<CacheValue<V>>>,
    {
        match map.entry(key.clone()) {
            dashmap::Entry::Occupied(entry) => match entry.get() {
                CacheEntry::Ready(v) => Ok(v.clone()),
                CacheEntry::Failed(error) => Err(anyhow::anyhow!("{error}")),
                CacheEntry::Loading(rx) => {
                    let mut rx = rx.clone();
                    drop(entry);

                    let val = rx.wait_for(|v| v.is_some()).await.map_err(|_| {
                        anyhow::anyhow!("cache loader dropped without producing a value")
                    })?;
                    val.clone().context("cache entry resolved to None")
                }
            },
            dashmap::Entry::Vacant(entry) => {
                let (tx, rx) = watch::channel(None);
                entry.insert(CacheEntry::Loading(rx));

                let v = loader.await?;
                map.insert(key, CacheEntry::Ready(v.clone()));
                let _ = tx.send(Some(v.clone()));

                Ok(v)
            }
        }
    }

    async fn resolve_coord(
        &self,
        coord: &ArtifactCoordinates,
    ) -> anyhow::Result<CacheValue<Utf8PathBuf>> {
        self.get_or_load(&self.metas, coord.clone(), async {
            let (repo, version) =
                if let Some(version) = coord.version().as_str().strip_suffix("-SNAPSHOT") {
                    let version =
                        ArtifactVersion::new(version).expect("stripped version cannot contain ':'");

                    let (repo, meta) = search_repos(&self.repos, self.strategy, |repo| {
                        let repo = repo.clone();
                        async move {
                            let meta = repo
                                .artifact_metadata(
                                    coord.group_id(),
                                    coord.artifact_id(),
                                    Some(coord.version()),
                                )
                                .await?;
                            Ok::<_, ClientError>((repo, meta))
                        }
                    })
                    .await
                    .with_context(|| format!("failed to resolve metadata for {coord}"))?;

                    let snapshot = meta
                        .versioning
                        .snapshot
                        .with_context(|| format!("no snapshot info in metadata for {coord}"))?;

                    (
                        Some(repo),
                        Cow::Owned(format!(
                            "{}-{}-{}",
                            version.as_str(),
                            snapshot.timestamp,
                            snapshot.build_number
                        )),
                    )
                } else {
                    (None, Cow::Borrowed(coord.version().as_str()))
                };

            let url = format!(
                "{gid}/{aid}/{cv}/{aid}-{version}",
                gid = coord.group_id().to_path(),
                aid = coord.artifact_id().as_str(),
                cv = coord.version().as_str(),
            );

            let path = Utf8PathBuf::from(url);

            Ok((repo, Arc::new(path)))
        })
        .await
    }

    async fn fetch_pom(
        &self,
        repo_hint: &Option<Arc<MavenRepositoryClient>>,
        path: &Utf8Path,
        status_key: Option<&str>,
    ) -> anyhow::Result<CacheValue<XmlFile>> {
        self.get_or_load(&self.files, path.to_string(), async {
            let path = path.with_added_extension(MAVEN_POM_SUFFIX);

            let results = if let Some(hint) = repo_hint {
                vec![
                    hint.fetch_xml(path.as_str(), status_key)
                        .await
                        .map(|res| (hint.clone(), res)),
                ]
            } else {
                vec![
                    search_repos(&self.repos, self.strategy, |repo| {
                        let repo = repo.clone();
                        let path = path.clone();
                        async move {
                            let res = repo.fetch_xml(path.as_str(), status_key).await?;
                            Ok((repo, res))
                        }
                    })
                    .await,
                ]
            };

            let (repo, mut xml) = results
                .into_iter()
                .find(|r| r.is_ok())
                .context("no repositories configured")?
                .with_context(|| format!("failed to fetch POM {path}"))?;
            xml.replace_templates(&Default::default());

            if let Some(Ok(parent)) = xml.get("parent").map(XmlNode::read_as::<Parent>) {
                let coord = ArtifactCoordinates::new(
                    parent.group_id,
                    parent.artifact_id,
                    parent.version.context("parent POM must have version")?,
                );
                let (repo, path) = self.resolve_coord(&coord).await?;
                let (_, parent) = Box::pin(self.fetch_pom(&repo, &path, status_key)).await?;

                xml.merge_pom(&parent);
                xml.replace_templates(&Default::default());
            }

            let imports = xml
                .get("dependencyManagement/dependencies")
                .map(XmlNode::read_as::<Vec<Dependency>>)
                .and_then(Result::ok)
                .into_iter()
                .flat_map(|node: Vec<Dependency>| {
                    node.into_iter()
                        .filter(|dep| dep.scope == DependencyScope::Import)
                });

            for import in imports {
                let coord = ArtifactCoordinates::new(
                    import.group_id,
                    import.artifact_id,
                    import.version.context("import must have version")?,
                );
                let (repo, path) = self.resolve_coord(&coord).await?;
                let (_, import) = Box::pin(self.fetch_pom(&repo, &path, status_key)).await?;

                let Some(bom_dependencies) = import.get("dependencyManagement/dependencies") else {
                    continue;
                };

                let Some(xml_dependencies) = xml.get_mut("dependencyManagement/dependencies")
                else {
                    continue;
                };

                xml_dependencies.merge_node(bom_dependencies);
                xml.replace_templates(&Default::default());
            }

            xml.merge_pom(&self.super_pom);
            xml.replace_templates(&Default::default());

            Ok((Some(repo), Arc::new(xml)))
        })
        .await
    }
}

async fn race_repos<T, E, F, Fut>(repos: &[Arc<MavenRepositoryClient>], f: F) -> Result<T, E>
where
    F: Fn(&Arc<MavenRepositoryClient>) -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
{
    let n = repos.len();
    let mut futs: FuturesUnordered<_> = repos
        .iter()
        .enumerate()
        .map(|(i, repo)| {
            let fut = f(repo);
            async move { (i, fut.await) }
        })
        .collect();

    let mut best_ok: Option<(usize, Result<T, E>)> = None;
    let mut last_err: Option<Result<T, E>> = None;
    let mut seen = vec![false; n];

    while let Some((i, result)) = futs.next().await {
        seen[i] = true;

        if result.is_ok() {
            match &best_ok {
                Some((best_i, _)) if *best_i <= i => {}
                _ => best_ok = Some((i, result)),
            }
        } else {
            last_err = Some(result);
        }

        if let Some((best_i, _)) = &best_ok
            && seen[0..=*best_i].iter().all(|s| *s)
        {
            break;
        }
    }

    if let Some((_, result)) = best_ok {
        result
    } else if let Some(err) = last_err {
        err
    } else {
        panic!("no repositories configured")
    }
}

async fn sequential_repos<T, E, F, Fut>(repos: &[Arc<MavenRepositoryClient>], f: F) -> Result<T, E>
where
    F: Fn(&Arc<MavenRepositoryClient>) -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
{
    let mut last_err = None;
    for repo in repos {
        match f(repo).await {
            Ok(val) => return Ok(val),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.expect("no repositories configured"))
}

async fn search_repos<T, E, F, Fut>(
    repos: &[Arc<MavenRepositoryClient>],
    strategy: crate::manifest::RepoStrategy,
    f: F,
) -> Result<T, E>
where
    F: Fn(&Arc<MavenRepositoryClient>) -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
{
    match strategy {
        crate::manifest::RepoStrategy::Race => race_repos(repos, f).await,
        crate::manifest::RepoStrategy::Sequential => sequential_repos(repos, f).await,
    }
}
