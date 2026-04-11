use std::{
    collections::{BTreeSet, HashSet},
    fs::File,
    io::BufReader,
    path::{Component, Path, PathBuf},
};

use zip::{ZipArchive, ZipWriter, write::FileOptions};

use crate::types::{ArtifactCoordinates, ExclusionPattern};

pub struct JarWriter {
    writer: ZipWriter<File>,
    seen: HashSet<PathBuf>,
}

impl JarWriter {
    pub fn new(output: &Path) -> Self {
        let file = File::create(output).unwrap();
        let writer = ZipWriter::new(file);
        Self {
            writer,
            seen: HashSet::new(),
        }
    }

    pub fn copy_jar_contents(
        &mut self,
        jar: &Path,
        coord: Option<&ArtifactCoordinates>,
        excluded: &BTreeSet<ExclusionPattern>,
    ) {
        if is_excluded(coord, excluded) {
            return;
        }
        let file = File::open(jar).unwrap();
        let reader = BufReader::new(file);
        let mut archive = ZipArchive::new(reader).unwrap();

        for i in 0..archive.len() {
            let file = archive.by_index_raw(i).unwrap();
            let Some(name) = file.enclosed_name() else {
                panic!("invalid file name: {}", file.name());
            };

            let name = clean(&name);

            if !self.seen.insert(name.clone()) {
                continue;
            }

            if file.is_dir() {
                self.writer
                    .add_directory_from_path(name, FileOptions::DEFAULT)
                    .unwrap();
            } else {
                self.writer
                    .raw_copy_file_to_path(file, &name)
                    .expect("failed to copy file to final jar");
            }
        }
    }

    pub fn flush(self) {
        self.writer.finish().unwrap();
    }
}

fn is_excluded(coord: Option<&ArtifactCoordinates>, excluded: &BTreeSet<ExclusionPattern>) -> bool {
    let Some(coord) = coord else {
        return false;
    };
    excluded.iter().any(|pattern| pattern.matches(coord))
}

fn clean(path: &Path) -> PathBuf {
    let mut resolved = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(c) => resolved.push(c),
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            _ => resolved.push(component),
        }
    }
    resolved
}
