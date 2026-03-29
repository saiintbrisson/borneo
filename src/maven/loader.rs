use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque, hash_map},
    sync::Arc,
};

use anyhow::Context as _;

use camino::{Utf8Path, Utf8PathBuf};
use dashmap::DashMap;
use futures_util::{TryFutureExt, future::join_all};
use tokio::sync::watch;

use crate::{
    manifest::{PomScope, lock::Lock},
    maven::{
        MAVEN_POM_SUFFIX, MavenRepositoryClient,
        pom::{Dependency, DependencyScope, Parent, Pom},
        xml::{XmlFile, XmlNode},
    },
    status::StatusHandle,
    types::{ArtifactCoordinates, ArtifactKey, ArtifactVersion},
};

type Cache<K, V> = DashMap<K, CacheEntry<V>>;
type CacheValue<V> = (Option<Arc<MavenRepositoryClient>>, Arc<V>);

#[derive(Clone, Debug)]
enum CacheEntry<V> {
    Ready(CacheValue<V>),
    Loading(watch::Receiver<Option<CacheValue<V>>>),
}

#[derive(Clone, Debug)]
pub struct ResolvedArtifact {
    pub coord: ArtifactCoordinates,
    pub source: String,
    pub artifact_path: Utf8PathBuf,
    pub dependencies: BTreeMap<ArtifactCoordinates, PomScope>,
    pub depth: usize,
}

#[derive(Clone)]
pub struct LoaderBranch {
    depth: usize,
    exclusions: BTreeSet<ArtifactKey>,
}

impl LoaderBranch {
    pub fn new(exclusions: BTreeSet<ArtifactKey>) -> Self {
        Self {
            depth: 0,
            exclusions,
        }
    }

    fn child(&self, extra_exclusions: impl IntoIterator<Item = ArtifactKey>) -> Self {
        let mut exclusions = self.exclusions.clone();
        exclusions.extend(extra_exclusions);
        Self {
            depth: self.depth + 1,
            exclusions,
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
    loaded: Arc<Cache<ArtifactCoordinates, ResolvedArtifact>>,
    /// Tracks which exclusion sets have been applied per coord to avoid
    /// re-entering the same artifact with the same (or more restrictive) exclusions.
    seen_branches: DashMap<ArtifactCoordinates, Vec<BTreeSet<ArtifactKey>>>,

    metas: Cache<ArtifactCoordinates, Utf8PathBuf>,
    files: Cache<String, XmlFile>,
}

impl MavenLoader {
    pub fn new(urls: &[String]) -> Arc<Self> {
        let super_pom = XmlFile::from_str(include_str!("./pom-4.1.0.xml"))
            .expect("built-in super POM is valid");
        let client = reqwest::Client::new();

        let all_repos: Vec<_> = urls
            .iter()
            .map(|url| {
                Arc::new(MavenRepositoryClient::with_client(
                    client.clone(),
                    url.clone(),
                ))
            })
            .collect();

        Arc::new(Self {
            super_pom,
            repos: all_repos,
            client,

            channel: watch::channel(None),
            loaded: Default::default(),
            seen_branches: Default::default(),

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

        let mut depths: HashMap<_, _> = HashMap::new();
        let mut queue: VecDeque<(ArtifactCoordinates, usize, BTreeSet<ArtifactKey>)> =
            VecDeque::new();

        for dep in deps {
            let Some(coord) = dep.coord() else { continue };
            let Some(lock_artifact) = by_coord.get(coord) else {
                continue;
            };

            if lock_artifact.exclusions != dep.exclusions {
                continue;
            }

            queue.push_back((coord.clone(), 0, dep.exclusions.clone()));
        }

        while let Some((coord, depth, exclusions)) = queue.pop_front() {
            match depths.entry(coord.clone()) {
                hash_map::Entry::Occupied(e) if *e.get() <= depth => continue,
                hash_map::Entry::Occupied(mut e) => {
                    e.insert(depth);
                }
                hash_map::Entry::Vacant(e) => {
                    e.insert(depth);
                }
            }

            if let Some(lock_artifact) = by_coord.get(&coord) {
                for dep_coord in lock_artifact.dependencies.keys() {
                    if !exclusions.contains(&dep_coord.key()) {
                        queue.push_back((dep_coord.clone(), depth + 1, exclusions.clone()));
                    }
                }
            }
        }

        // Mark all seeded coords so load_artifact skips the merge for them.
        // Seeded entries already have correct dep lists from the lock.
        // Using empty exclusions (most permissive) so any branch is dominated.
        let empty = BTreeSet::new();
        for coord in depths.keys() {
            self.mark_branch(coord, &empty);
        }

        let mut repos = HashMap::<_, Arc<MavenRepositoryClient>>::new();

        for (coord, depth) in depths {
            let Some(lock_artifact) = by_coord.get(&coord) else {
                continue;
            };

            let repo = repos
                .entry(lock_artifact.source.clone())
                .or_insert_with(|| {
                    Arc::new(MavenRepositoryClient::with_client(
                        self.client.clone(),
                        lock_artifact.source.clone(),
                    ))
                })
                .clone();

            let resolved = Arc::new(ResolvedArtifact {
                coord: coord.clone(),
                source: lock_artifact.source.clone(),
                artifact_path: lock_artifact.artifact_path.clone(),
                dependencies: lock_artifact.dependencies.clone(),
                depth,
            });

            self.loaded
                .insert(coord, CacheEntry::Ready((Some(repo), resolved)));
        }
    }

    fn mark_branch(&self, coord: &ArtifactCoordinates, exclusions: &BTreeSet<ArtifactKey>) -> bool {
        let mut entry = self.seen_branches.entry(coord.clone()).or_default();
        let dominated = entry.iter().any(|seen| seen.is_subset(exclusions));
        if dominated {
            return true;
        }
        entry.retain(|seen| !exclusions.is_subset(seen));
        entry.push(exclusions.clone());
        false
    }

    async fn load_artifact(
        self: Arc<Self>,
        coord: ArtifactCoordinates,
        branch: LoaderBranch,
    ) -> anyhow::Result<()> {
        let tx = match self.loaded.entry(coord.clone()) {
            dashmap::Entry::Occupied(entry) => {
                if let CacheEntry::Loading(rx) = entry.get() {
                    let mut rx = rx.clone();
                    drop(entry);
                    let _ = rx.wait_for(|v| v.is_some()).await;
                } else {
                    drop(entry);
                }

                if self.mark_branch(&coord, &branch.exclusions) {
                    return Ok(());
                }

                if let Some(entry) = self.loaded.get(&coord)
                    && let CacheEntry::Ready((_, artifact)) = &*entry {
                        let existing_deps = artifact.dependencies.clone();
                        let artifact_path = artifact.artifact_path.clone();
                        drop(entry);

                        self.merge_branch_deps(&coord, &artifact_path, &existing_deps, &branch)
                            .await?;
                    }

                return Ok(());
            }
            dashmap::Entry::Vacant(entry) => {
                self.mark_branch(&coord, &branch.exclusions);
                let (tx, rx) = watch::channel(None);
                entry.insert(CacheEntry::Loading(rx));
                tx
            }
        };

        StatusHandle::get().resolving(&coord);

        let (repo, artifact_path) = self.resolve_coord(&coord).await?;
        let (repo, pom) = self.fetch_pom(&repo, &artifact_path).await?;

        let repo = repo.context("no repo found")?;
        let source = repo.base().to_string();

        let dep_coords = self.resolve_pom_deps(&coord, &pom, &branch).await?;

        let artifact = (
            Some(repo),
            Arc::new(ResolvedArtifact {
                source,
                coord: coord.clone(),
                artifact_path: artifact_path.as_ref().clone(),
                dependencies: dep_coords,
                depth: branch.depth,
            }),
        );

        StatusHandle::get().resolved(&coord);

        self.loaded
            .insert(coord, CacheEntry::Ready(artifact.clone()));
        let _ = tx.send(Some(artifact));
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
            .with_context(|| format!("failed to parse POM for {coord}"))?;

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

        for dep in &pom.dependencies {
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

            let child = branch.child(dep.exclusions.iter().map(|e| e.to_key()));
            self.clone().spawn_load_artifact(coord, child);
        }

        Ok(dep_coords)
    }

    async fn merge_branch_deps(
        self: &Arc<Self>,
        coord: &ArtifactCoordinates,
        artifact_path: &Utf8Path,
        existing_deps: &BTreeMap<ArtifactCoordinates, PomScope>,
        branch: &LoaderBranch,
    ) -> anyhow::Result<()> {
        let (_, pom) = self.fetch_pom(&None, artifact_path).await?;
        let new_deps = self.resolve_pom_deps(coord, &pom, branch).await?;

        let added: BTreeMap<_, _> = new_deps
            .into_iter()
            .filter(|(k, _)| !existing_deps.contains_key(k))
            .collect();
        if added.is_empty() {
            return Ok(());
        }

        if let Some(mut entry) = self.loaded.get_mut(coord)
            && let CacheEntry::Ready((_, ref mut artifact)) = *entry {
                let merged = Arc::make_mut(artifact);
                merged.dependencies.extend(added);
            }

        Ok(())
    }

    pub fn spawn_load_artifact(self: Arc<Self>, coord: ArtifactCoordinates, branch: LoaderBranch) {
        tokio::spawn(async move {
            if let Err(e) = self.load_artifact(coord.clone(), branch).await {
                StatusHandle::get().error(coord, e);
            }
        });
    }

    pub async fn into_resolved(self: Arc<Self>) -> ResolvedDependencies {
        let mut rx = self.channel.1.clone();
        let loaded = self.loaded.clone();
        drop(self);

        let _ = rx.changed().await;

        let mut deduped = HashMap::<_, Arc<ResolvedArtifact>>::new();
        for entry in loaded.iter() {
            let val = match entry.value() {
                CacheEntry::Ready((_, val)) => val.clone(),
                _ => unreachable!(),
            };

            match deduped.entry(entry.key().key()) {
                hash_map::Entry::Occupied(mut existing) if existing.get().depth < val.depth => {
                    existing.insert(val);
                }
                hash_map::Entry::Vacant(vacant) => {
                    vacant.insert(val);
                }
                _ => {}
            }
        }

        ResolvedDependencies {
            artifacts: deduped.into_values().collect(),
            loaded,
        }
    }
}

pub struct ResolvedDependencies {
    pub artifacts: Vec<Arc<ResolvedArtifact>>,
    loaded: Arc<Cache<ArtifactCoordinates, ResolvedArtifact>>,
}

impl ResolvedDependencies {
    pub async fn download_jar(
        &self,
        coord: &ArtifactCoordinates,
        out: &Utf8Path,
    ) -> anyhow::Result<Vec<u8>> {
        let entry = self
            .loaded
            .get(coord)
            .expect("coord must be resolved first");
        let (repo, artifact) = match &*entry {
            CacheEntry::Ready(val) => val.clone(),
            _ => unreachable!(),
        };

        let repo = repo.expect("resolved artifact must have a source repo");
        let jar_path = artifact.artifact_path.with_added_extension("jar");
        let asset = repo
            .download_asset(jar_path.as_str(), out)
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
                CacheEntry::Loading(rx) => {
                    let mut rx = rx.clone();
                    drop(entry);

                    let val = rx
                        .wait_for(|v| v.is_some())
                        .await
                        .map_err(|_| anyhow::anyhow!("cache loader dropped without producing a value"))?;
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

                    let futs = self.repos.iter().map(|repo| {
                        repo.artifact_metadata(
                            coord.group_id(),
                            coord.artifact_id(),
                            Some(coord.version()),
                        )
                        .map_ok(|meta| (repo.clone(), meta))
                    });
                    let mut results = join_all(futs).await;
                    results.sort_unstable_by_key(Result::is_ok);

                    let (repo, meta) = results
                        .pop()
                        .context("no repository responded")?
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
    ) -> anyhow::Result<CacheValue<XmlFile>> {
        self.get_or_load(&self.files, path.to_string(), async {
            let path = path.with_added_extension(MAVEN_POM_SUFFIX);

            let mut results = if let Some(hint) = repo_hint {
                vec![
                    hint.fetch_xml(path.as_str())
                        .await
                        .map(|res| (hint.clone(), res)),
                ]
            } else {
                join_all(self.repos.iter().map(|repo| {
                    repo.fetch_xml(path.as_str())
                        .map_ok(|res| (repo.clone(), res))
                }))
                .await
            };
            results.sort_unstable_by_key(Result::is_ok);

            let (repo, mut xml) = results
                .pop()
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
                let (_, parent) = Box::pin(self.fetch_pom(&repo, &path)).await?;

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
                let (_, import) = Box::pin(self.fetch_pom(&repo, &path)).await?;

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
