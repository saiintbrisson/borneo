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
    manifest::{ArtifactType, PomScope, lock::Lock},
    maven::{
        ClientError, MAVEN_POM_SUFFIX, MavenRepositoryClient,
        pom::{Dependency, DependencyScope, Parent, Pom},
        xml::{XmlFile, XmlNode},
    },
    status::StatusHandle,
    types::{ArtifactCoordinates, ArtifactKey, ArtifactVersion},
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
    entry: CacheEntry<ResolvedArtifact>,
}

#[derive(Clone, Debug)]
pub struct ResolvedArtifact {
    pub coord: ArtifactCoordinates,
    pub source: String,
    pub artifact_path: Utf8PathBuf,
    pub artifact_type: ArtifactType,
    pub dependencies: BTreeMap<ArtifactCoordinates, PomScope>,
}

#[derive(Clone)]
pub struct LoaderBranch {
    pub depth: usize,
    exclusions: BTreeSet<ArtifactKey>,
    position: Vec<usize>,
}

impl LoaderBranch {
    pub fn new(exclusions: BTreeSet<ArtifactKey>, position: usize) -> Self {
        Self {
            depth: 0,
            exclusions,
            position: vec![position],
        }
    }

    fn child(&self, index: usize, extra_exclusions: impl IntoIterator<Item = ArtifactKey>) -> Self {
        let mut exclusions = self.exclusions.clone();
        exclusions.extend(extra_exclusions);
        let mut position = self.position.clone();
        position.push(index);
        Self {
            depth: self.depth + 1,
            exclusions,
            position,
        }
    }

    fn is_excluded(&self, key: &ArtifactKey) -> bool {
        self.exclusions.contains(key)
    }
}

pub struct MavenLoader {
    super_pom: XmlFile,
    client: reqwest::Client,
    repos: Vec<Arc<MavenRepositoryClient>>,

    channel: (watch::Sender<Option<()>>, watch::Receiver<Option<()>>),
    artifacts: Arc<DashMap<ArtifactKey, ArtifactSlot>>,

    metas: Cache<ArtifactCoordinates, Utf8PathBuf>,
    files: Cache<String, XmlFile>,
}

impl MavenLoader {
    pub fn new(repos: &[crate::manifest::RepoEntry]) -> Arc<Self> {
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

            channel: watch::channel(None),
            artifacts: Default::default(),

            metas: Default::default(),
            files: Default::default(),
        })
    }

    pub fn seed_from_lock(
        &self,
        lock: &Lock,
        deps: &[crate::manifest::Dependency],
        repo_urls: &[String],
    ) {
        let current_repos: BTreeSet<_> = repo_urls.iter().cloned().collect();
        if lock.repositories != current_repos {
            return;
        }

        let by_coord: HashMap<_, _> = lock.artifacts.iter().map(|a| (&a.coord, a)).collect();

        let manifest_coords: BTreeSet<_> = deps.iter().filter_map(|d| d.coord()).collect();

        let mut invalidated_keys = BTreeSet::new();

        for la in &lock.artifacts {
            if la.depth == 0 && !manifest_coords.contains(&la.coord) {
                invalidated_keys.insert(la.coord.key());
            }
        }

        for dep in deps {
            let Some(coord) = dep.coord() else { continue };
            let lock_match = by_coord.get(coord);
            let changed = match lock_match {
                None => true,
                Some(la) => la.exclusions != dep.exclusions,
            };
            if changed {
                invalidated_keys.insert(coord.key());
            }
        }

        if !invalidated_keys.is_empty() {
            let mut reverse_deps: HashMap<ArtifactKey, Vec<ArtifactKey>> = HashMap::new();
            for la in &lock.artifacts {
                for dep_coord in la.dependencies.keys() {
                    reverse_deps
                        .entry(dep_coord.key())
                        .or_default()
                        .push(la.coord.key());
                }
            }

            let mut queue = std::collections::VecDeque::from_iter(invalidated_keys.iter().cloned());
            while let Some(key) = queue.pop_front() {
                if let Some(dependents) = reverse_deps.get(&key) {
                    for dep_key in dependents {
                        if invalidated_keys.insert(dep_key.clone()) {
                            queue.push_back(dep_key.clone());
                        }
                    }
                }
            }
        }

        let mut reachable = BTreeSet::new();
        let mut queue = std::collections::VecDeque::new();

        for dep in deps {
            let Some(coord) = dep.coord() else { continue };
            if invalidated_keys.contains(&coord.key()) {
                continue;
            }
            let Some(lock_artifact) = by_coord.get(coord) else {
                continue;
            };
            if lock_artifact.exclusions != dep.exclusions {
                continue;
            }
            queue.push_back(coord.clone());
        }

        while let Some(coord) = queue.pop_front() {
            if !reachable.insert(coord.clone()) {
                continue;
            }
            if invalidated_keys.contains(&coord.key()) {
                continue;
            }
            if let Some(lock_artifact) = by_coord.get(&coord) {
                for dep_coord in lock_artifact.dependencies.keys() {
                    queue.push_back(dep_coord.clone());
                }
            }
        }

        let mut repos = HashMap::new();

        for lock_artifact in lock
            .artifacts
            .iter()
            .filter(|a| reachable.contains(&a.coord))
        {
            let repo = repos
                .entry(lock_artifact.source.clone())
                .or_insert_with(|| {
                    Arc::new(MavenRepositoryClient::with_client(
                        self.client.clone(),
                        lock_artifact.source.clone(),
                        Default::default(),
                    ))
                })
                .clone();

            let rank: Rank = (lock_artifact.depth, lock_artifact.position.clone());

            let resolved = Arc::new(ResolvedArtifact {
                coord: lock_artifact.coord.clone(),
                source: lock_artifact.source.clone(),
                artifact_path: lock_artifact.artifact_path.clone(),
                artifact_type: lock_artifact.artifact_type.clone(),
                dependencies: lock_artifact.dependencies.clone(),
            });

            self.artifacts.insert(
                lock_artifact.coord.key(),
                ArtifactSlot {
                    rank,
                    entry: CacheEntry::Ready((Some(repo), resolved)),
                },
            );
        }
    }

    async fn load_artifact(
        self: Arc<Self>,
        coord: ArtifactCoordinates,
        branch: LoaderBranch,
    ) -> anyhow::Result<()> {
        let key = coord.key();
        let rank: Rank = (branch.depth, branch.position.clone());

        let tx = match self.artifacts.entry(key.clone()) {
            dashmap::Entry::Occupied(e) if e.get().rank <= rank => return Ok(()),
            dashmap::Entry::Occupied(mut e) => {
                let (tx, rx) = watch::channel(None);
                e.insert(ArtifactSlot {
                    rank: rank.clone(),
                    entry: CacheEntry::Loading(rx),
                });
                tx
            }
            dashmap::Entry::Vacant(e) => {
                let (tx, rx) = watch::channel(None);
                e.insert(ArtifactSlot {
                    rank: rank.clone(),
                    entry: CacheEntry::Loading(rx),
                });
                tx
            }
        };

        StatusHandle::get().resolving(&coord);

        let (repo, artifact_path) = self.resolve_coord(&coord).await?;
        let coord_str = coord.to_string();
        let (repo, pom) = self
            .fetch_pom(&repo, &artifact_path, Some(&coord_str))
            .await?;

        let repo = repo.context("no repo found")?;
        let source = repo.base().to_string();

        let dep_coords = self.resolve_pom_deps(&coord, &pom, &branch).await?;

        let artifact = Arc::new(ResolvedArtifact {
            source,
            coord: coord.clone(),
            artifact_path: artifact_path.as_ref().clone(),
            artifact_type: Default::default(),
            dependencies: dep_coords,
        });

        StatusHandle::get().resolved(&coord);

        let entry = (Some(repo), artifact);
        self.artifacts.insert(
            key,
            ArtifactSlot {
                rank,
                entry: CacheEntry::Ready(entry.clone()),
            },
        );

        let _ = tx.send(Some(entry));

        Ok(())
    }

    async fn resolve_pom_deps(
        self: &Arc<Self>,
        coord: &ArtifactCoordinates,
        pom: &Arc<XmlFile>,
        branch: &LoaderBranch,
    ) -> anyhow::Result<BTreeMap<ArtifactCoordinates, PomScope>> {
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

        let mut dep_coords = BTreeMap::new();

        for (i, dep) in pom.dependencies.iter().enumerate() {
            let pom_scope = match dep.scope {
                DependencyScope::Compile => PomScope::Compile,
                DependencyScope::Runtime => PomScope::Runtime,
                _ => continue,
            };

            let key = ArtifactKey::new(dep.group_id.clone(), dep.artifact_id.clone());
            if branch.is_excluded(&key) {
                continue;
            }

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

            dep_coords.insert(coord.clone(), pom_scope);

            let child = branch.child(i, dep.exclusions.iter().map(|e| e.to_key()));
            self.clone().spawn_load_artifact(coord, child);
        }

        Ok(dep_coords)
    }

    pub fn spawn_load_artifact(self: Arc<Self>, coord: ArtifactCoordinates, branch: LoaderBranch) {
        let artifacts = self.artifacts.clone();
        let rank: Rank = (branch.depth, branch.position.clone());
        tokio::spawn(async move {
            if let Err(e) = self.load_artifact(coord.clone(), branch).await {
                StatusHandle::get().end(coord.to_string());
                let key = coord.key();
                let err = Arc::new(e);
                match artifacts.entry(key) {
                    dashmap::Entry::Occupied(existing)
                        if matches!(existing.get().entry, CacheEntry::Ready(_))
                            && existing.get().rank <= rank => {}
                    dashmap::Entry::Occupied(mut existing) => {
                        existing.insert(ArtifactSlot {
                            rank,
                            entry: CacheEntry::Failed(err),
                        });
                    }
                    dashmap::Entry::Vacant(vacant) => {
                        vacant.insert(ArtifactSlot {
                            rank,
                            entry: CacheEntry::Failed(err),
                        });
                    }
                }
            }
        });
    }

    pub async fn into_resolved(self: Arc<Self>) -> anyhow::Result<ResolvedDependencies> {
        let mut rx = self.channel.1.clone();
        let artifacts = self.artifacts.clone();
        drop(self);

        let _ = rx.changed().await;

        let map = Arc::try_unwrap(artifacts).unwrap_or_else(|arc| (*arc).clone());

        let mut result = Vec::new();
        let mut slot_map = HashMap::new();

        for (key, slot) in map.into_iter() {
            match slot.entry {
                CacheEntry::Ready((repo, artifact)) => {
                    let repo =
                        repo.context(format!("resolved artifact {} must have a source repo", key))?;
                    result.push(artifact.clone());
                    slot_map.insert(key, (slot.rank, repo, artifact));
                }
                CacheEntry::Failed(error) => anyhow::bail!("failed to resolve {key}: {error}"),
                CacheEntry::Loading(_) => continue,
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
    pub(crate) slot_map:
        HashMap<ArtifactKey, (Rank, Arc<MavenRepositoryClient>, Arc<ResolvedArtifact>)>,
}

impl ResolvedDependencies {
    pub async fn download_artifact(
        &self,
        coord: &ArtifactCoordinates,
        ext: &str,
        out: &Utf8Path,
    ) -> anyhow::Result<Vec<u8>> {
        let (_, repo, artifact) = self
            .slot_map
            .get(&coord.key())
            .context("coord must be resolved first")?;
        let path = artifact.artifact_path.with_added_extension(ext);
        let key = format!("dl:{coord}");
        let asset = repo
            .download_asset(path.as_str(), out, Some(&key))
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

                    let (repo, meta) = race_repos(&self.repos, |repo| {
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
                    race_repos(&self.repos, |repo| {
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
