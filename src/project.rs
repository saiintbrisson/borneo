use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use camino::Utf8PathBuf;
use futures_util::StreamExt;

use crate::{
    cli::{BuildArgs, ProjectArgs},
    manifest::{
        Scope,
        self, Packaging,
        lock::{self, Checksum, Lock, LockArtifact},
    },
    maven::loader::{LoaderBranch, MavenLoader, ResolvedDependencies, verify_cached},
    types::ArtifactCoordinates,
    status,
};

pub struct Project {
    pub dir: PathBuf,
    pub out: PathBuf,
    source: PathBuf,
    resources: Option<PathBuf>,
    packaging: Packaging,
    pub class_path: BTreeMap<PathBuf, Scope>,
    pub manifest: Option<manifest::Manifest>,
}

impl Project {
    fn resolve_dir(project: &ProjectArgs) -> Result<PathBuf> {
        let dir = match &project.base {
            Some(base) => base
                .canonicalize()
                .context("failed to canonicalize base path")?,
            None => std::env::current_dir().context("failed to get current directory")?,
        };
        ensure!(
            dir.is_dir(),
            "base path is not a directory: {}",
            dir.display()
        );
        Ok(dir)
    }

    fn load_manifest(project: &ProjectArgs) -> Result<Option<manifest::Manifest>> {
        let manifest_path = project
            .manifest
            .as_deref()
            .unwrap_or("borneo.kdl".as_ref());
        if manifest_path.is_file() {
            let source = std::fs::read_to_string(manifest_path)
                .with_context(|| format!("failed to read manifest: {}", manifest_path.display()))?;
            let name = manifest_path.display().to_string();
            Ok(Some(
                manifest::Manifest::parse(&source, &name)
                    .map_err(|e| {
                        status::StatusHandle::get().fatal(format!("{e:?}"));
                        anyhow::anyhow!("")
                    })?,
            ))
        } else {
            Ok(None)
        }
    }

    pub fn from_project_args(project: &ProjectArgs) -> Result<Self> {
        let dir = Self::resolve_dir(project)?;
        let manifest = Self::load_manifest(project)?;

        let out = dir.join("build");
        let source = dir.join(
            manifest
                .as_ref()
                .map(|m| m.source.as_path())
                .unwrap_or(Path::new("src/main/java")),
        );
        let resources = Self::resolve_resources(&manifest, &dir)?;

        Ok(Self {
            dir: dir.clone(),
            out,
            source,
            resources,
            packaging: Packaging::default(),
            class_path: BTreeMap::from([(dir, Scope::Compile)]),
            manifest,
        })
    }

    pub fn from_build_args(build: &BuildArgs) -> Result<Self> {
        let dir = Self::resolve_dir(&build.project_args)?;
        let manifest = Self::load_manifest(&build.project_args)?;

        let out = build
            .out
            .as_ref()
            .map(|o| o.to_path_buf())
            .or_else(|| manifest.as_ref().and_then(|m| m.build.output.clone()).map(|o| dir.join(o)))
            .unwrap_or_else(|| dir.join("build"));

        if out.extension().is_some_and(|ext| ext == "jar") {
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("failed to create output directory: {}", parent.display())
                })?;
            }
        } else {
            std::fs::create_dir_all(&out)
                .with_context(|| format!("failed to create output directory: {}", out.display()))?;
        }

        let packaging = build.packaging.unwrap_or_else(|| {
            if let Some(m) = &manifest {
                if m.build.output.as_ref().is_some_and(|o| o.extension().is_some_and(|ext| ext == "jar")) {
                    return Packaging::Jar;
                }
                m.build.packaging
            } else if out.extension().is_some_and(|ext| ext == "jar") {
                Packaging::Jar
            } else {
                Packaging::default()
            }
        });

        let source = dir.join(
            manifest
                .as_ref()
                .map(|m| m.source.as_path())
                .unwrap_or(Path::new("src/main/java")),
        );
        ensure!(
            source.is_dir(),
            "source directory does not exist: {}",
            source.display()
        );

        let resources = Self::resolve_resources(&manifest, &dir)?;

        Ok(Self {
            dir: dir.clone(),
            out,
            source,
            resources,
            packaging,
            class_path: BTreeMap::from([(dir, Scope::Compile)]),
            manifest,
        })
    }

    pub fn class_path_iter(&self) -> impl Iterator<Item = &PathBuf> {
        self.class_path.keys()
    }

    fn resolve_resources(
        manifest: &Option<manifest::Manifest>,
        dir: &Path,
    ) -> Result<Option<PathBuf>> {
        let resources = dir.join(
            manifest
                .as_ref()
                .map(|m| m.resources.as_path())
                .unwrap_or(Path::new("src/main/resources")),
        );
        if resources.is_dir() {
            Ok(Some(resources))
        } else if manifest
            .as_ref()
            .is_some_and(|m| m.resources != Path::new("src/main/resources"))
        {
            anyhow::bail!(
                "resources directory does not exist: {}",
                resources.display()
            );
        } else {
            Ok(None)
        }
    }

    pub async fn build(&mut self) -> Result<Option<PathBuf>> {
        let mut files = Vec::with_capacity(1);

        for entry in walkdir::WalkDir::new(&self.source) {
            let entry = entry.context("failed to walk source directory")?;
            if !entry.file_type().is_file() {
                continue;
            }

            files.push(
                entry
                    .into_path()
                    .canonicalize()
                    .context("failed to canonicalize source file")?,
            );
        }

        self.resolve_dependencies().await?;
        self.class_path.insert(self.source.clone(), Scope::Compile);

        let status = status::StatusHandle::get();
        let java = crate::java::Java::new()?;

        match self.packaging {
            Packaging::Dir => {
                status.begin("compile", format!("compiling {} source files", files.len()));
                java.javac(&self.dir, &self.out, self.class_path_iter(), &files)?;
                status.end("compile");
                status.log(format!("compiled {} source files", files.len()));
                if let Some(resources) = &self.resources {
                    self.class_path.insert(resources.clone(), Scope::Compile);
                }
                Ok(None)
            }
            Packaging::Jar => {
                let shadow = self
                    .manifest
                    .as_ref()
                    .is_some_and(|m| m.build.shadow);

                let build_dir = self.dir.join("build");
                let classes_dir = build_dir.join("classes");
                let jar_path = if self.out.extension().is_some_and(|ext| ext == "jar") {
                    self.out.clone()
                } else {
                    let suffix = if shadow { "-all.jar" } else { ".jar" };
                    let jar_name = self
                        .manifest
                        .as_ref()
                        .map(|m| format!("{}-{}{suffix}", m.artifact, m.version))
                        .unwrap_or_else(|| format!("output{suffix}"));
                    self.out.join(jar_name)
                };

                if classes_dir.exists() {
                    std::fs::remove_dir_all(&classes_dir)
                        .context("failed to clean classes directory")?;
                }
                std::fs::create_dir_all(&classes_dir)
                    .context("failed to create classes directory")?;

                status.begin("compile", format!("compiling {} source files", files.len()));
                java.javac(&self.dir, &classes_dir, self.class_path_iter(), &files)?;
                status.end("compile");
                status.log(format!("compiled {} source files", files.len()));

                if let Some(resources) = &self.resources {
                    copy_dir_contents(resources, &classes_dir)?;
                }

                if shadow {
                    status.begin("shadow", "bundling dependencies");
                    for (path, scope) in &self.class_path {
                        if matches!(scope, Scope::Compile | Scope::Runtime)
                            && path.extension().is_some_and(|ext| ext == "jar")
                        {
                            unpack_jar(&java, path, &classes_dir)?;
                        }
                    }
                    status.end("shadow");
                    status.log("bundled dependencies into shadow jar");
                }

                if jar_path.exists() {
                    std::fs::remove_file(&jar_path).ok();
                    std::fs::remove_dir_all(&jar_path).ok();
                }

                let rel_jar = jar_path.strip_prefix(&self.dir).unwrap_or(&jar_path);
                status.begin("package", format!("packaging {}", rel_jar.display()));
                let entry = self.manifest.as_ref().and_then(|m| m.entry.as_deref());
                java.jar(&self.dir, &classes_dir, &jar_path, entry)?;
                status.end("package");
                status.log(format!("packaged {}", rel_jar.display()));

                if let Some(post_build) = self.manifest.as_ref().and_then(|m| m.build.post_build.as_deref()) {
                    status.begin("post-build", format!("running: {post_build}"));
                    let output = run_post_build(&self.dir, post_build, &jar_path)?;
                    status.end("post-build");
                    status.log(format!("post-build: {post_build}"));
                    status.output(output.stdout);
                    status.output(output.stderr);
                }

                Ok(Some(jar_path))
            }
        }
    }

    pub fn clean(&self) -> Result<()> {
        if self.out.exists() {
            std::fs::remove_dir_all(&self.out).with_context(|| {
                format!("failed to remove build directory: {}", self.out.display())
            })?;
            eprintln!(
                "cleaned {}",
                self.out
                    .strip_prefix(&self.dir)
                    .unwrap_or(&self.out)
                    .display()
            );
        }
        Ok(())
    }

    async fn resolve_dependencies(&mut self) -> Result<()> {
        let Some(manifest) = &self.manifest else {
            return Ok(());
        };

        let cache_dir = self.dir.join("build").join("cache");
        std::fs::create_dir_all(&cache_dir).context("failed to create cache directory")?;

        let lock_path = self.dir.join("borneo.lock");
        let prev_lock = read_lock(&lock_path)?;

        let mut local_artifacts = BTreeSet::new();
        for dep in &manifest.dependencies {
            if let manifest::DependencySource::Path(path) = &dep.source {
                let abs = self.dir.join(path);
                ensure!(
                    abs.exists(),
                    "local dependency not found: {}",
                    abs.display()
                );
                let bytes = std::fs::read(&abs)
                    .with_context(|| format!("failed to read local dep: {}", abs.display()))?;
                let hash = <sha2::Sha256 as sha2::Digest>::digest(&bytes);
                local_artifacts.insert(lock::LocalArtifact {
                    path: path.display().to_string(),
                    checksum: Checksum::provided(hash.to_vec()),
                });
                self.class_path.insert(abs, dep.scope);
            }
        }

        let repo_urls = manifest.repositories.urls();
        let resolved = resolve_artifacts(manifest, &prev_lock, &repo_urls).await;
        let mut lock = download_and_lock(
            &mut self.class_path,
            manifest,
            &resolved,
            &prev_lock,
            &cache_dir,
            &repo_urls,
        )
        .await?;
        lock.local = local_artifacts;

        write_lock(&lock_path, &lock)?;
        Ok(())
    }
}

async fn resolve_artifacts(
    manifest: &manifest::Manifest,
    prev_lock: &Option<Lock>,
    repo_urls: &[String],
) -> ResolvedDependencies {
    let loader = MavenLoader::new(repo_urls);

    if let Some(lock) = prev_lock {
        loader.seed_from_lock(lock, &manifest.dependencies, repo_urls);
    }

    let status = status::StatusHandle::get();
    if !manifest.dependencies.is_empty() {
        status.log(format!(
            "resolving {} direct dependencies",
            manifest.dependencies.len()
        ));
    }

    for dep in &manifest.dependencies {
        let Some(coord) = dep.coord() else {
            continue;
        };
        loader
            .clone()
            .spawn_load_artifact(coord.clone(), LoaderBranch::new(dep.exclusions.clone()));
    }

    let resolved = loader.into_resolved().await;

    if !manifest.dependencies.is_empty() {
        status.log(format!(
            "resolved {} artifacts ({} from lock)",
            resolved.artifacts.len(),
            prev_lock.as_ref().map_or(0, |l| l.artifacts.len()),
        ));
    }

    resolved
}

fn compute_effective_scopes(
    manifest_deps: &[manifest::Dependency],
    resolved: &ResolvedDependencies,
) -> BTreeMap<ArtifactCoordinates, Scope> {
    use std::collections::HashMap;

    let by_coord: HashMap<_, _> = resolved
        .artifacts
        .iter()
        .map(|a| (&a.coord, a.as_ref()))
        .collect();

    let mut result = BTreeMap::new();
    let mut queue = VecDeque::new();

    for dep in manifest_deps {
        if let Some(coord) = dep.coord() {
            queue.push_back((coord.clone(), dep.scope));
        }
    }

    while let Some((coord, effective_scope)) = queue.pop_front() {
        match result.entry(coord.clone()) {
            std::collections::btree_map::Entry::Occupied(mut e) => {
                if effective_scope <= *e.get() {
                    continue;
                }
                e.insert(effective_scope);
            }
            std::collections::btree_map::Entry::Vacant(e) => {
                e.insert(effective_scope);
            }
        }

        if let Some(artifact) = by_coord.get(&coord) {
            for (child_coord, pom_scope) in &artifact.dependencies {
                let child_scope = manifest::mediate(effective_scope, *pom_scope);
                queue.push_back((child_coord.clone(), child_scope));
            }
        }
    }

    result
}

async fn download_and_lock(
    class_path: &mut BTreeMap<PathBuf, Scope>,
    manifest: &manifest::Manifest,
    resolved: &ResolvedDependencies,
    prev_lock: &Option<Lock>,
    cache_dir: &Path,
    repo_urls: &[String],
) -> Result<Lock> {
    let manifest_deps = &manifest.dependencies;
    let effective_scopes = compute_effective_scopes(manifest_deps, resolved);
    let status = status::StatusHandle::get();

    let mut lock_artifacts = BTreeSet::new();
    let mut to_download = Vec::new();

    for artifact in &resolved.artifacts {
        let jar_name = format!(
            "{}-{}-{}.jar",
            artifact.coord.group_id().as_str(),
            artifact.coord.artifact_id().as_str(),
            artifact.coord.version().as_str(),
        );
        let out = cache_dir.join(&jar_name);

        let expected_digest = prev_lock.as_ref().and_then(|lock| {
            lock.artifacts
                .iter()
                .find(|a| a.coord == artifact.coord)
                .map(|a| a.checksum.digest().to_vec())
        });

        let exclusions = manifest_deps
            .iter()
            .find(|d| d.coord() == Some(&artifact.coord))
            .map(|d| d.exclusions.clone())
            .unwrap_or_default();
        let scope = effective_scopes
            .get(&artifact.coord)
            .copied()
            .unwrap_or(Scope::Compile);

        if let Some(digest) = &expected_digest
            && verify_cached(&out, digest) {
                lock_artifacts.insert(LockArtifact {
                    coord: artifact.coord.clone(),
                    classifier: None,
                    artifact_type: None,
                    source: artifact.source.clone(),
                    artifact_path: artifact.artifact_path.clone(),
                    checksum: Checksum::provided(digest.clone()),
                    effective_scope: scope,
                    dependencies: artifact.dependencies.clone(),
                    exclusions,
                });
                class_path.insert(out, scope);
                continue;
            }

        to_download.push((artifact.clone(), out, exclusions, scope));
    }

    if to_download.is_empty() {
        return Ok(Lock {
            version: "1".to_string(),
            repositories: repo_urls.iter().cloned().collect(),
            artifacts: lock_artifacts,
            local: BTreeSet::new(),
        });
    }

    for (artifact, _, _, _) in &to_download {
        status.downloading(&artifact.coord);
    }

    let results: Vec<anyhow::Result<_>> =
        futures_util::stream::iter(to_download.iter().map(|(artifact, out, _, _)| async {
            let out = Utf8PathBuf::from(out.to_string_lossy().to_string());
            let sha256 = resolved.download_jar(&artifact.coord, &out).await?;

            status::StatusHandle::get().downloaded(&artifact.coord);

            Ok((artifact.coord.clone(), sha256))
        }))
        .buffer_unordered(4)
        .collect()
        .await;

    for result in results {
        let (coord, sha256) = result?;
        let (artifact, out, exclusions, scope) = to_download
            .iter()
            .find(|(a, _, _, _)| a.coord == coord)
            .context("download result does not match any queued artifact")?;
        lock_artifacts.insert(LockArtifact {
            coord,
            classifier: None,
            artifact_type: None,
            source: artifact.source.clone(),
            artifact_path: artifact.artifact_path.clone(),
            checksum: Checksum::provided(sha256),
            effective_scope: *scope,
            dependencies: artifact.dependencies.clone(),
            exclusions: exclusions.clone(),
        });
        class_path.insert(out.clone(), *scope);
    }

    status.log(format!("downloaded {} artifacts", to_download.len()));

    Ok(Lock {
        version: "1".to_string(),
        repositories: repo_urls.iter().cloned().collect(),
        artifacts: lock_artifacts,
        local: BTreeSet::new(),
    })
}

fn read_lock(path: &Path) -> Result<Option<Lock>> {
    if path.is_file() {
        let source = std::fs::read_to_string(path).context("failed to read lock file")?;
        Ok(Some(Lock::parse(&source)?))
    } else {
        Ok(None)
    }
}

fn write_lock(path: &Path, lock: &Lock) -> Result<()> {
    std::fs::write(path, lock.to_kdl()).context("failed to write lock file")
}

fn copy_dir_contents(src: &Path, dst: &Path) -> Result<()> {
    for entry in walkdir::WalkDir::new(src) {
        let entry = entry.context("failed to walk resources directory")?;
        let rel = entry.path().strip_prefix(src).unwrap();
        let target = dst.join(rel);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

fn unpack_jar(java: &crate::java::Java, jar: &Path, dst: &Path) -> Result<()> {
    java.extract_jar(jar, dst)
        .with_context(|| format!("failed to unpack {}", jar.display()))
}

fn run_post_build(dir: &Path, command: &str, jar_path: &Path) -> Result<std::process::Output> {
    let shell = if cfg!(windows) { "cmd" } else { "sh" };
    let flag = if cfg!(windows) { "/C" } else { "-c" };

    let output = std::process::Command::new(shell)
        .arg(flag)
        .arg(command)
        .current_dir(dir)
        .env("BORNEO_BUILD_OUTPUT", jar_path.canonicalize().unwrap_or_else(|_| jar_path.to_path_buf()))
        .output()
        .with_context(|| format!("failed to run post-build: {command}"))?;

    ensure!(
        output.status.success(),
        "post-build command failed with {}",
        output.status
    );
    Ok(output)
}
