use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use camino::Utf8PathBuf;
use futures_util::StreamExt;

use crate::{
    cli::ProjectArgs,
    java::jar::JarWriter,
    manifest::{
        self, Packaging, Scope,
        lock::{self, Checksum, Lock, LockArtifact},
    },
    maven::loader::{LoaderBranch, MavenLoader, ResolvedDependencies, verify_cached},
    status,
    types::ArtifactCoordinates,
};

const NATIVE_EXTENSIONS: &[&str] = &["dll", "so", "dylib"];

fn resolve_path(base: &Path, user: &Option<PathBuf>, default: impl AsRef<Path>) -> PathBuf {
    match user {
        Some(p) if p.is_absolute() => p.clone(),
        Some(p) => base.join(p),
        None => base.join(default),
    }
}

pub trait Compiler {
    fn name(&self) -> &str;
    fn source(&self) -> &Path;
    fn compile(
        &self,
        project: &Project,
        out: &Path,
        files: &[PathBuf],
    ) -> Result<std::process::Output>;
}

pub struct Project {
    pub dir: PathBuf,
    pub build_dir: PathBuf,
    pub out: Option<PathBuf>,
    pub entry: Option<String>,
    java: Option<crate::java::Java>,
    resources: Option<PathBuf>,
    packaging: Packaging,
    pub class_path: BTreeMap<PathBuf, Scope>,
    pub manifest: Option<manifest::Manifest>,
}

impl Project {
    pub fn java(&self) -> &crate::java::Java {
        self.java.as_ref().expect("java not initialized")
    }

    fn ensure_java(&mut self) -> Result<()> {
        if self.java.is_none() {
            self.java = Some(crate::java::Java::new()?);
        }
        Ok(())
    }

    pub fn new(
        project: &ProjectArgs,
        out: Option<&PathBuf>,
        packaging: Option<Packaging>,
        entry: Option<String>,
    ) -> Result<Self> {
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

        let manifest_path = resolve_path(&dir, &project.manifest, "borneo.kdl");
        let manifest = if manifest_path.is_file() {
            let source = std::fs::read_to_string(&manifest_path)
                .with_context(|| format!("failed to read manifest: {}", manifest_path.display()))?;
            let name = manifest_path.display().to_string();
            Some(manifest::Manifest::parse(&source, &name).map_err(|e| {
                status::StatusHandle::get().fatal(format!("{e:?}"));
                anyhow::anyhow!("")
            })?)
        } else {
            None
        };

        let build_dir = dir.join("build");

        let out = out
            .map(|o| resolve_path(&dir, &Some(o.clone()), ""))
            .or_else(|| {
                manifest
                    .as_ref()
                    .and_then(|m| m.build.output.clone())
                    .map(|o| dir.join(o))
            });

        let packaging = packaging.unwrap_or_else(|| {
            manifest
                .as_ref()
                .map(|m| m.build.packaging)
                .unwrap_or_default()
        });

        let resources = Self::resolve_resources(&manifest, &dir)?;

        Ok(Self {
            dir: dir.clone(),
            build_dir,
            out,
            entry,
            java: None,
            resources,
            packaging,
            class_path: BTreeMap::from([(dir, Scope::Compile)]),
            manifest,
        })
    }

    pub fn class_path_iter(&self) -> impl Iterator<Item = &PathBuf> {
        self.class_path.keys()
    }

    pub fn processor_path_iter(&self) -> impl Iterator<Item = &PathBuf> {
        self.class_path
            .iter()
            .filter(|(_, scope)| matches!(scope, Scope::Processor))
            .map(|(path, _)| path)
    }

    pub fn native_library_dirs(&self) -> BTreeSet<PathBuf> {
        self.class_path
            .keys()
            .filter(|p| {
                p.extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| NATIVE_EXTENSIONS.contains(&e))
            })
            .filter_map(|p| p.parent().map(|d| d.to_path_buf()))
            .collect()
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

    async fn compile_main(&mut self) -> Result<PathBuf> {
        self.ensure_java()?;

        if let Some(manifest) = &self.manifest
            && let Some(required) = manifest.java.release
        {
            let actual = self.java().major_version();
            ensure!(
                actual.is_some_and(|v| v >= required),
                "project requires Java {required} but JAVA_HOME provides {}",
                actual.map_or("unknown".into(), |v| v.to_string()),
            );
        }

        let compilers = self.build_compilers().await?;
        ensure!(!compilers.is_empty(), "no source directories found");

        self.resolve_dependencies().await?;

        let classes_dir = self.build_dir.join("classes");
        if classes_dir.exists() {
            std::fs::remove_dir_all(&classes_dir).context("failed to clean classes directory")?;
        }
        std::fs::create_dir_all(&classes_dir).context("failed to create classes directory")?;

        for compiler in &compilers {
            self.class_path
                .insert(compiler.source().to_path_buf(), Scope::Compile);
        }

        let status = status::StatusHandle::get();
        for compiler in &compilers {
            let files = collect_source_files(compiler.source())?;
            if files.is_empty() {
                continue;
            }
            let file_count = files.len();
            let name = compiler.name();
            let output = status.task(
                name,
                format!("compiling {file_count} source files"),
                format!("compiled {file_count} source files"),
                || compiler.compile(self, &classes_dir, &files),
            )?;
            flush_output(&output);
        }

        if let Some(resources) = &self.resources {
            copy_dir_contents(resources, &classes_dir)?;
        }

        Ok(classes_dir)
    }

    async fn build_compilers(&self) -> Result<Vec<Box<dyn Compiler>>> {
        let mut compilers: Vec<Box<dyn Compiler>> = Vec::new();

        let Some(manifest) = &self.manifest else {
            compilers.push(Box::new(crate::java::JavaCompiler::new(
                self.dir.join("src/main/java"),
                Vec::new(),
            )));
            return Ok(compilers);
        };

        if let Some(kotlin_config) = &manifest.kotlin {
            let kotlin_source = self.dir.join(&kotlin_config.compiler.source);
            ensure!(
                kotlin_source.is_dir(),
                "kotlin source directory does not exist: {}",
                kotlin_source.display()
            );
            let kotlin =
                crate::kotlin::Kotlin::new(kotlin_config.version.as_deref(), &self.build_dir)
                    .await?;
            compilers.push(Box::new(crate::kotlin::KotlinCompiler::new(
                kotlin,
                kotlin_source,
                kotlin_config.compiler.compiler_args.clone(),
            )));
        } else {
            let default_kotlin = self.dir.join("src/main/kotlin");
            if default_kotlin.is_dir() {
                status::StatusHandle::get().log(
                    "warning: src/main/kotlin exists but kotlin is not declared in the manifest",
                );
            }
        }

        let java_source = self.dir.join(&manifest.java.compiler.source);
        if manifest.kotlin.is_some() {
            if java_source.is_dir() {
                compilers.push(Box::new(crate::java::JavaCompiler::new(
                    java_source,
                    manifest.java.compiler.compiler_args.clone(),
                )));
            }
        } else {
            ensure!(
                java_source.is_dir(),
                "source directory does not exist: {}",
                java_source.display()
            );
            compilers.push(Box::new(crate::java::JavaCompiler::new(
                java_source,
                manifest.java.compiler.compiler_args.clone(),
            )));
        }

        Ok(compilers)
    }

    pub async fn sync(&mut self) -> Result<()> {
        if let Some(manifest) = &self.manifest
            && let Some(kotlin_config) = &manifest.kotlin
        {
            crate::kotlin::Kotlin::new(kotlin_config.version.as_deref(), &self.build_dir).await?;
        }

        self.resolve_dependencies().await
    }

    pub async fn build(&mut self) -> Result<Option<PathBuf>> {
        let classes_dir = self.compile_main().await?;

        match self.packaging {
            Packaging::Dir => {
                if let Some(resources) = &self.resources {
                    self.class_path.insert(resources.clone(), Scope::Compile);
                }
                Ok(None)
            }
            Packaging::Jar => {
                let shadow = self.manifest.as_ref().and_then(|m| m.build.shadow.as_ref());

                let base_name = self
                    .manifest
                    .as_ref()
                    .map(|m| format!("{}-{}", m.artifact, m.version))
                    .unwrap_or_else(|| "output".into());

                let slim_jar = self.build_dir.join(format!("{base_name}.jar"));

                let resolve_out = |default_name: String| -> PathBuf {
                    match &self.out {
                        Some(o) if o.extension().is_some_and(|ext| ext == "jar") => o.clone(),
                        Some(o) => o.join(default_name),
                        None => self.build_dir.join(default_name),
                    }
                };

                let final_jar = if shadow.is_some() {
                    resolve_out(format!("{base_name}-all.jar"))
                } else {
                    resolve_out(format!("{base_name}.jar"))
                };

                let status = status::StatusHandle::get();

                if slim_jar.exists() {
                    std::fs::remove_file(&slim_jar).ok();
                }
                if shadow.is_some() && final_jar.exists() {
                    std::fs::remove_file(&final_jar).ok();
                }

                let entry = self
                    .entry
                    .as_deref()
                    .or(self.manifest.as_ref().and_then(|m| m.entry.as_deref()));
                let manifest_entries = self
                    .manifest
                    .as_ref()
                    .map(|m| m.build.manifest_entries.as_slice())
                    .unwrap_or_default();
                let manifest_file = if manifest_entries.is_empty() {
                    None
                } else {
                    let path = self.build_dir.join("MANIFEST.MF");
                    let mut contents = String::from("Manifest-Version: 1.0\n");
                    for (k, v) in manifest_entries {
                        contents.push_str(&format!("{k}: {v}\n"));
                    }
                    contents.push('\n');
                    std::fs::write(&path, contents).context("failed to write MANIFEST.MF")?;
                    Some(path)
                };

                let rel_slim = slim_jar.strip_prefix(&self.dir).unwrap_or(&slim_jar);
                let output = status.task(
                    "package",
                    format!("packaging {}", rel_slim.display()),
                    format!("packaged {}", rel_slim.display()),
                    || {
                        self.java().jar(
                            &self.dir,
                            &classes_dir,
                            &slim_jar,
                            entry,
                            manifest_file.as_deref(),
                        )
                    },
                )?;
                flush_output(&output);

                if let Some(shadow_config) = shadow {
                    let rel_final = final_jar.strip_prefix(&self.dir).unwrap_or(&final_jar);
                    status.task(
                        "shadow",
                        format!("bundling {}", rel_final.display()),
                        format!("bundled {}", rel_final.display()),
                        || {
                            let mut writer = JarWriter::new(&final_jar);
                            writer.copy_jar_contents(&slim_jar, &Default::default());
                            for (path, scope) in &self.class_path {
                                if matches!(scope, Scope::Compile | Scope::Runtime)
                                    && path.extension().is_some_and(|ext| ext == "jar")
                                {
                                    writer.copy_jar_contents(path, &shadow_config.exclusions);
                                }
                            }
                            writer.flush();
                            Ok(())
                        },
                    )?;
                }

                if let Some(post_build) = self
                    .manifest
                    .as_ref()
                    .and_then(|m| m.build.post_build.as_deref())
                {
                    let output = status.task(
                        "post-build",
                        format!("running: {post_build}"),
                        format!("post-build: {post_build}"),
                        || run_post_build(&self.dir, post_build, &final_jar),
                    )?;
                    status.stdout(output.stdout);
                    status.stderr(output.stderr);
                }

                Ok(Some(final_jar))
            }
        }
    }

    pub fn clean(&self, purge: bool) -> Result<()> {
        if purge {
            return self.purge_libraries();
        }
        if self.build_dir.exists() {
            std::fs::remove_dir_all(&self.build_dir).with_context(|| {
                format!(
                    "failed to remove build directory: {}",
                    self.build_dir.display()
                )
            })?;
            status::StatusHandle::get().log(format!(
                "cleaned {}",
                self.build_dir
                    .strip_prefix(&self.dir)
                    .unwrap_or(&self.build_dir)
                    .display()
            ));
        }
        Ok(())
    }

    fn purge_libraries(&self) -> Result<()> {
        let libraries_dir = self.build_dir.join("libraries");
        if !libraries_dir.is_dir() {
            return Ok(());
        }

        let lock_path = self.dir.join("borneo.lock");
        let lock = read_lock(&lock_path)?;

        let locked_files: BTreeSet<_> = lock
            .as_ref()
            .map(|l| {
                l.artifacts
                    .iter()
                    .map(|a| {
                        let ext = a.artifact_type.extension();
                        let classifier_suffix = a
                            .classifier
                            .as_deref()
                            .map(|c| format!("-{c}"))
                            .unwrap_or_default();
                        format!(
                            "{}-{}-{}{classifier_suffix}.{ext}",
                            a.coord.group_id().as_str(),
                            a.coord.artifact_id().as_str(),
                            a.coord.version().as_str(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();

        let mut removed = 0usize;
        for entry in
            std::fs::read_dir(&libraries_dir).context("failed to read libraries directory")?
        {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !locked_files.contains(name.as_ref()) {
                std::fs::remove_file(entry.path())?;
                removed += 1;
            }
        }

        if removed > 0 {
            status::StatusHandle::get()
                .log(format!("purged {removed} stale artifacts from libraries"));
        }
        Ok(())
    }

    pub async fn test(&mut self, cmd: &crate::cli::TestCommand) -> Result<()> {
        ensure!(
            self.manifest.is_some(),
            "test requires a borneo.kdl manifest"
        );

        let classes_dir = self.compile_main().await?;
        let manifest = self.manifest.as_ref().unwrap();

        const STANDALONE_PREFIX: &str = "org.junit.platform-junit-platform-console-standalone-";

        let standalone_jar = self
            .class_path
            .iter()
            .find(|(path, scope)| {
                matches!(scope, Scope::Test)
                    && path
                        .file_name()
                        .and_then(|f| f.to_str())
                        .is_some_and(|f| f.starts_with(STANDALONE_PREFIX))
            })
            .map(|(path, _)| path.clone())
            .context(
                "test requires org.junit.platform:junit-platform-console-standalone as a test dependency",
            )?;

        let standalone_major: u32 = standalone_jar
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.strip_prefix(STANDALONE_PREFIX))
            .and_then(|v| v.split('.').next())
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);

        let test_classes_dir = self.build_dir.join("test-classes");
        if test_classes_dir.exists() {
            std::fs::remove_dir_all(&test_classes_dir)
                .context("failed to clean test-classes directory")?;
        }
        std::fs::create_dir_all(&test_classes_dir)
            .context("failed to create test-classes directory")?;

        let compiler_args = manifest.java.compiler.compiler_args.clone();
        let test_source = self.dir.join(&manifest.java.compiler.test_source);
        ensure!(
            test_source.is_dir(),
            "test source directory does not exist: {}",
            test_source.display()
        );

        let mut test_files = Vec::new();
        for entry in walkdir::WalkDir::new(&test_source) {
            let entry = entry.context("failed to walk test source directory")?;
            if entry.file_type().is_file() {
                test_files.push(
                    entry
                        .into_path()
                        .canonicalize()
                        .context("failed to canonicalize test file")?,
                );
            }
        }

        let status = status::StatusHandle::get();
        let mut test_cp: Vec<_> = self.class_path_iter().cloned().collect();
        test_cp.push(classes_dir.clone());

        let test_file_count = test_files.len();
        let output = status.task(
            "compile-tests",
            format!("compiling {test_file_count} test files"),
            format!("compiled {test_file_count} test files"),
            || {
                self.java().javac(
                    &self.dir,
                    &test_classes_dir,
                    test_cp.iter(),
                    self.processor_path_iter(),
                    &test_files,
                    &compiler_args,
                )
            },
        )?;
        flush_output(&output);

        let test_resources = self.dir.join(&manifest.test.resources);
        if test_resources.is_dir() {
            copy_dir_contents(&test_resources, &test_classes_dir)?;
        }

        let mut run_cp = vec![classes_dir, test_classes_dir.clone()];
        for path in self.class_path.keys() {
            if *path != standalone_jar {
                run_cp.push(path.clone());
            }
        }

        let mut filter_args = Vec::new();
        if let Some(class) = &cmd.class {
            filter_args.push(format!("--select-class={class}"));
        }
        if let Some(method) = &cmd.method {
            filter_args.push(format!("--select-method={method}"));
        }
        if let Some(tag) = &cmd.tag {
            filter_args.push(format!("--include-tag={tag}"));
        }
        if let Some(tag) = &cmd.exclude_tag {
            filter_args.push(format!("--exclude-tag={tag}"));
        }

        status.log("running tests");
        self.java().run_tests(
            &self.dir,
            &standalone_jar,
            standalone_major,
            run_cp.iter(),
            &test_classes_dir,
            &manifest.test.jvm_args,
            &filter_args,
        )?;

        Ok(())
    }

    async fn resolve_dependencies(&mut self) -> Result<()> {
        let Some(manifest) = &self.manifest else {
            return Ok(());
        };

        let libraries_dir = self.build_dir.join("libraries");
        std::fs::create_dir_all(&libraries_dir).context("failed to create libraries directory")?;

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

        let repo_entries = manifest.repositories.entries();
        let repo_urls = manifest.repositories.urls();
        let strategy = manifest.repositories.strategy;
        let resolved =
            resolve_artifacts(manifest, &prev_lock, repo_entries, &repo_urls, strategy).await?;
        let mut lock = download_and_lock(
            &mut self.class_path,
            manifest,
            &resolved,
            &prev_lock,
            &libraries_dir,
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
    repo_entries: &[manifest::RepoEntry],
    repo_urls: &[String],
    strategy: manifest::RepoStrategy,
) -> Result<ResolvedDependencies> {
    let loader = MavenLoader::new(repo_entries, strategy);

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

    for (i, dep) in manifest.dependencies.iter().enumerate() {
        let Some(coord) = dep.coord() else {
            continue;
        };
        loader.clone().spawn_load_artifact(
            coord.clone(),
            dep.artifact_type.clone(),
            dep.classifier.clone(),
            LoaderBranch::new(dep.exclusions.clone(), i),
        );
    }

    let resolved = loader.into_resolved().await?;
    status.clear();

    if !manifest.dependencies.is_empty() {
        status.log(format!(
            "resolved {} artifacts ({} from lock)",
            resolved.artifacts.len(),
            prev_lock.as_ref().map_or(0, |l| l.artifacts.len()),
        ));
    }

    Ok(resolved)
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
    libraries_dir: &Path,
    repo_urls: &[String],
) -> Result<Lock> {
    let manifest_deps = &manifest.dependencies;
    let effective_scopes = compute_effective_scopes(manifest_deps, resolved);
    let status = status::StatusHandle::get();

    let mut lock_artifacts = BTreeSet::new();
    let mut to_download = Vec::new();

    for artifact in &resolved.artifacts {
        let ext = artifact.artifact_type.extension();
        let classifier_suffix = artifact
            .classifier
            .as_deref()
            .map(|c| format!("-{c}"))
            .unwrap_or_default();
        let file_name = format!(
            "{}-{}-{}{classifier_suffix}.{ext}",
            artifact.coord.group_id().as_str(),
            artifact.coord.artifact_id().as_str(),
            artifact.coord.version().as_str(),
        );
        let out = libraries_dir.join(&file_name);

        let expected_digest = prev_lock.as_ref().and_then(|lock| {
            lock.artifacts
                .iter()
                .find(|a| a.coord == artifact.coord && a.classifier == artifact.classifier)
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
            && verify_cached(&out, digest)
        {
            let (rank, _, _) = resolved.slot_map.get(&artifact.key()).unwrap();
            lock_artifacts.insert(LockArtifact {
                coord: artifact.coord.clone(),
                classifier: artifact.classifier.clone(),
                artifact_type: artifact.artifact_type.clone(),
                source: artifact.source.clone(),
                artifact_path: artifact.artifact_path.clone(),
                checksum: Checksum::provided(digest.clone()),
                effective_scope: scope,
                depth: rank.0,
                position: rank.1.clone(),
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
            let sha256 = resolved.download_artifact(artifact, &out).await?;

            status::StatusHandle::get().downloaded(&artifact.coord);

            Ok((artifact.key(), sha256))
        }))
        .buffer_unordered(8)
        .collect()
        .await;

    for result in results {
        let (key, sha256) = result?;
        let (artifact, out, exclusions, scope) = to_download
            .iter()
            .find(|(a, _, _, _)| a.key() == key)
            .context("download result does not match any queued artifact")?;
        let (rank, _, _) = resolved.slot_map.get(&key).unwrap();
        lock_artifacts.insert(LockArtifact {
            coord: artifact.coord.clone(),
            classifier: artifact.classifier.clone(),
            artifact_type: artifact.artifact_type.clone(),
            source: artifact.source.clone(),
            artifact_path: artifact.artifact_path.clone(),
            checksum: Checksum::provided(sha256),
            effective_scope: *scope,
            depth: rank.0,
            position: rank.1.clone(),
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

fn collect_source_files(source: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in walkdir::WalkDir::new(source) {
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
    Ok(files)
}

fn flush_output(output: &std::process::Output) {
    let status = status::StatusHandle::get();
    status.stdout(output.stdout.clone());
    status.stderr(output.stderr.clone());
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

fn run_post_build(dir: &Path, command: &str, jar_path: &Path) -> Result<std::process::Output> {
    let shell = if cfg!(windows) { "cmd" } else { "sh" };
    let flag = if cfg!(windows) { "/C" } else { "-c" };

    let output = std::process::Command::new(shell)
        .arg(flag)
        .arg(command)
        .current_dir(dir)
        .env(
            "BORNEO_BUILD_OUTPUT",
            jar_path
                .canonicalize()
                .unwrap_or_else(|_| jar_path.to_path_buf()),
        )
        .output()
        .with_context(|| format!("failed to run post-build: {command}"))?;

    ensure!(
        output.status.success(),
        "post-build command failed with {}",
        output.status
    );
    Ok(output)
}
