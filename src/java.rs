use anyhow::{Context, Result, ensure};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

const STREAM_ENCODING_MIN_VERSION: u32 = 18;

#[cfg(windows)]
pub const PATH_SEPARATOR: &str = ";";
#[cfg(not(windows))]
pub const PATH_SEPARATOR: &str = ":";

pub struct Java {
    home: PathBuf,
    major_version: Option<u32>,
}

impl Java {
    pub fn new() -> Result<Self> {
        let home = PathBuf::from(
            std::env::var("JAVA_HOME").context("JAVA_HOME environment variable not set")?,
        );
        let major_version = read_java_version(&home).and_then(|v| parse_major_version(&v));
        Ok(Self {
            home,
            major_version,
        })
    }

    fn bin(&self, name: &str) -> PathBuf {
        self.home.join("bin").join(name)
    }

    pub fn major_version(&self) -> Option<u32> {
        self.major_version
    }

    fn apply_library_path(cmd: &mut Command, dirs: &std::collections::BTreeSet<PathBuf>) {
        if dirs.is_empty() {
            return;
        }
        let joined = dirs
            .iter()
            .map(|d| d.as_os_str())
            .collect::<Vec<_>>()
            .join(&OsString::from(PATH_SEPARATOR));
        cmd.arg(format!("-Djava.library.path={}", joined.to_string_lossy()));
    }

    fn encoding_flags(&self) -> Vec<String> {
        let mut flags = vec!["-Dfile.encoding=UTF-8".into()];
        if self
            .major_version
            .is_some_and(|v| v >= STREAM_ENCODING_MIN_VERSION)
        {
            flags.push("-Dstdout.encoding=UTF-8".into());
            flags.push("-Dstderr.encoding=UTF-8".into());
        }
        flags
    }

    pub fn javac<'a>(
        &self,
        base: &Path,
        out: &Path,
        class_path: impl Iterator<Item = &'a PathBuf>,
        processor_path: impl Iterator<Item = &'a PathBuf>,
        files: &[PathBuf],
        extra_args: &[String],
    ) -> Result<std::process::Output> {
        let mut cmd = Command::new(self.bin("javac"));
        cmd.current_dir(base);
        cmd.arg("-encoding").arg("UTF-8");

        cmd.arg("-d").arg(out);

        let class_path: Vec<_> = class_path.map(|cp| cp.as_os_str()).collect();
        let class_path = class_path.join(&OsString::from(PATH_SEPARATOR));

        if !class_path.is_empty() {
            cmd.arg("-cp").arg(class_path);
        }

        let proc_path: Vec<_> = processor_path.map(|p| p.as_os_str()).collect();
        let proc_path = proc_path.join(&OsString::from(PATH_SEPARATOR));

        if !proc_path.is_empty() {
            cmd.arg("-processorpath").arg(proc_path);
        }

        cmd.args(extra_args);
        cmd.args(files);
        capture_cmd(&mut cmd, "javac")
    }

    pub fn extract_jar(&self, jar: &Path, dst: &Path) -> Result<std::process::Output> {
        let mut cmd = Command::new(self.bin("jar"));
        cmd.current_dir(dst);
        cmd.arg("xf").arg(jar);
        capture_cmd(&mut cmd, "jar xf")
    }

    pub fn jar(
        &self,
        base: &Path,
        out: &Path,
        jar_path: &Path,
        entry: Option<&str>,
    ) -> Result<std::process::Output> {
        let mut cmd = Command::new(self.bin("jar"));
        cmd.current_dir(base);

        if let Some(entry) = entry {
            cmd.arg("cfe").arg(jar_path).arg(entry);
        } else {
            cmd.arg("cf").arg(jar_path);
        }

        cmd.arg("-C").arg(out).arg(".");
        capture_cmd(&mut cmd, "jar")
    }

    pub fn run<'a>(
        &self,
        base: &Path,
        out: &Path,
        class_path: impl Iterator<Item = &'a PathBuf>,
        entry: &str,
        native_dirs: &std::collections::BTreeSet<PathBuf>,
        args: &[String],
    ) -> Result<()> {
        let mut cmd = Command::new(self.bin("java"));
        cmd.current_dir(base);
        cmd.args(self.encoding_flags());
        Self::apply_library_path(&mut cmd, native_dirs);

        let mut final_class_path = out.as_os_str().to_owned();

        let class_path: Vec<_> = class_path.map(|cp| cp.as_os_str()).collect();
        let class_path = class_path.join(&OsString::from(PATH_SEPARATOR));

        if !class_path.is_empty() {
            final_class_path.push(PATH_SEPARATOR);
            final_class_path.push(class_path);
        }

        cmd.arg("-cp").arg(final_class_path);
        cmd.arg(entry);
        cmd.args(args);
        run_cmd(&mut cmd, "java")
    }

    pub fn run_tests<'a>(
        &self,
        base: &Path,
        standalone_jar: &Path,
        class_path: impl Iterator<Item = &'a PathBuf>,
        scan_path: &Path,
        jvm_args: &[String],
        filter_args: &[String],
    ) -> Result<()> {
        let mut cmd = Command::new(self.bin("java"));
        cmd.current_dir(base);
        cmd.args(self.encoding_flags());
        cmd.args(jvm_args);
        cmd.arg("-jar").arg(standalone_jar);

        cmd.arg("execute");

        let cp: Vec<_> = class_path.map(|p| p.as_os_str()).collect();
        let cp = cp.join(&OsString::from(PATH_SEPARATOR));
        if !cp.is_empty() {
            cmd.arg("--class-path").arg(cp);
        }

        cmd.arg("--scan-class-path").arg(scan_path);
        cmd.args(filter_args);

        run_cmd(&mut cmd, "junit")
    }

    pub fn run_jar(
        &self,
        base: &Path,
        jar_path: &Path,
        native_dirs: &std::collections::BTreeSet<PathBuf>,
        args: &[String],
    ) -> Result<()> {
        let mut cmd = Command::new(self.bin("java"));
        cmd.current_dir(base);
        cmd.args(self.encoding_flags());
        Self::apply_library_path(&mut cmd, native_dirs);
        cmd.arg("-jar").arg(jar_path);
        cmd.args(args);
        run_cmd(&mut cmd, "java -jar")
    }
}

pub fn read_java_version(home: &Path) -> Option<String> {
    let release = std::fs::read_to_string(home.join("release")).ok()?;
    release
        .lines()
        .find_map(|l| l.strip_prefix("JAVA_VERSION="))
        .map(|v| v.trim_matches('"').to_string())
}

fn parse_major_version(version: &str) -> Option<u32> {
    version.split('.').next()?.parse().ok()
}

fn run_cmd(cmd: &mut Command, name: &str) -> Result<()> {
    let status = cmd
        .spawn()
        .with_context(|| format!("failed to run {name}"))?
        .wait()
        .with_context(|| format!("failed to wait for {name}"))?;
    ensure!(status.success(), "{name} exited with {status}");
    Ok(())
}

pub fn capture_cmd(cmd: &mut Command, name: &str) -> Result<std::process::Output> {
    let output = cmd
        .output()
        .with_context(|| format!("failed to run {name}"))?;
    if !output.status.success() {
        let status = crate::status::StatusHandle::get();
        status.output(output.stdout);
        status.output(output.stderr);
        anyhow::bail!("{name} exited with {}", output.status);
    }
    Ok(output)
}
