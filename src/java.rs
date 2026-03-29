use anyhow::{Context, Result, ensure};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(windows)]
pub const PATH_SEPARATOR: &str = ";";
#[cfg(not(windows))]
pub const PATH_SEPARATOR: &str = ":";

pub struct Java {
    home: PathBuf,
}

impl Java {
    pub fn new() -> Result<Self> {
        let home = PathBuf::from(
            std::env::var("JAVA_HOME").context("JAVA_HOME environment variable not set")?,
        );
        Ok(Self { home })
    }

    fn bin(&self, name: &str) -> PathBuf {
        self.home.join("bin").join(name)
    }

    pub fn javac<'a>(
        &self,
        base: &Path,
        out: &Path,
        class_path: impl Iterator<Item = &'a PathBuf>,
        files: &[PathBuf],
    ) -> Result<()> {
        let mut cmd = Command::new(self.bin("javac"));
        cmd.current_dir(base);

        cmd.arg("-d").arg(out);

        let class_path = class_path.map(|cp| cp.as_os_str()).collect::<Vec<_>>();
        let class_path = class_path.join(&OsString::from(PATH_SEPARATOR));

        if !class_path.is_empty() {
            cmd.arg("--class-path").arg(class_path);
        }

        cmd.args(files);
        run_cmd(&mut cmd, "javac")
    }

    pub fn extract_jar(&self, jar: &Path, dst: &Path) -> Result<()> {
        let mut cmd = Command::new(self.bin("jar"));
        cmd.current_dir(dst);
        cmd.arg("xf").arg(jar);
        run_cmd(&mut cmd, "jar xf")
    }

    pub fn jar(
        &self,
        base: &Path,
        out: &Path,
        jar_path: &Path,
        entry: Option<&str>,
    ) -> Result<()> {
        let mut cmd = Command::new(self.bin("jar"));
        cmd.current_dir(base);

        if let Some(entry) = entry {
            cmd.arg("cfe").arg(jar_path).arg(entry);
        } else {
            cmd.arg("cf").arg(jar_path);
        }

        cmd.arg("-C").arg(out).arg(".");
        run_cmd(&mut cmd, "jar")
    }

    pub fn run<'a>(
        &self,
        base: &Path,
        out: &Path,
        class_path: impl Iterator<Item = &'a PathBuf>,
        entry: &str,
        args: &[String],
    ) -> Result<()> {
        let mut cmd = Command::new(self.bin("java"));
        cmd.current_dir(base);

        let mut final_class_path = out.as_os_str().to_owned();

        let class_path = class_path.map(|cp| cp.as_os_str()).collect::<Vec<_>>();
        let class_path = class_path.join(&OsString::from(PATH_SEPARATOR));

        if !class_path.is_empty() {
            final_class_path.push(PATH_SEPARATOR);
            final_class_path.push(class_path);
        }

        cmd.arg("--class-path").arg(final_class_path);
        cmd.arg(entry);
        cmd.args(args);
        run_cmd(&mut cmd, "java")
    }

    pub fn run_jar(&self, base: &Path, jar_path: &Path, args: &[String]) -> Result<()> {
        let mut cmd = Command::new(self.bin("java"));
        cmd.current_dir(base);
        cmd.arg("-jar").arg(jar_path);
        cmd.args(args);
        run_cmd(&mut cmd, "java -jar")
    }
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
