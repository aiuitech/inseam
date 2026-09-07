//! Planning a runtime plugin install: the uploaded file set becomes a
//! checked [`InstallPlan`] — confined relative paths, exactly one artifact,
//! its manifest beside it — before anything touches disk. Parse, don't
//! validate: once a plan exists, writing it is branch-free.

use std::path::{Component, Path, PathBuf};

use inseam_seams::SeamError;
use inseam_seams::operations::{
    FileBytes, InstallPluginRequest, PLUGIN_FILES_MAX, PLUGIN_UPLOAD_BYTES_MAX, PluginFile,
};

/// One file to write, its path relative to the install directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlannedFile {
    pub relative: PathBuf,
    pub bytes: FileBytes,
}

/// A checked upload: what lands in the plugin directory and which file is
/// the artifact the composition entry points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InstallPlan {
    pub files: Vec<PlannedFile>,
    /// The `.wasm` among `files`, relative to the install directory.
    pub artifact: PathBuf,
}

pub(crate) fn plan(request: InstallPluginRequest) -> Result<InstallPlan, SeamError> {
    if request.files.is_empty() {
        return Err(SeamError::Refused(
            "a plugin upload carries no files".to_string(),
        ));
    }
    if request.files.len() > PLUGIN_FILES_MAX {
        return Err(SeamError::Refused(format!(
            "a plugin upload may carry at most {PLUGIN_FILES_MAX} files; this one has {}",
            request.files.len()
        )));
    }
    let total: u64 = request
        .files
        .iter()
        .map(|file| u64::try_from(file.bytes.0.len()).expect("a file's length fits u64"))
        .sum();
    if total > PLUGIN_UPLOAD_BYTES_MAX {
        return Err(SeamError::Refused(format!(
            "a plugin upload may carry at most {PLUGIN_UPLOAD_BYTES_MAX} bytes; this one has {total}"
        )));
    }
    let mut files: Vec<PlannedFile> = Vec::with_capacity(request.files.len());
    for file in request.files {
        let relative = confined_path(&file)?;
        if files.iter().any(|planned| planned.relative == relative) {
            return Err(SeamError::Refused(format!(
                "the upload names `{}` twice",
                relative.display()
            )));
        }
        files.push(PlannedFile {
            relative,
            bytes: file.bytes,
        });
    }
    let artifact = the_one_artifact(&files)?;
    let manifest = artifact.with_extension("manifest.toml");
    if !files.iter().any(|planned| planned.relative == manifest) {
        return Err(SeamError::Refused(format!(
            "the upload has no manifest `{}` beside the artifact `{}`",
            manifest.display(),
            artifact.display()
        )));
    }
    Ok(InstallPlan { files, artifact })
}

/// A relative path made only of plain components: nothing absolute, no
/// `..`, no `.`, no prefix — it cannot leave the install directory.
fn confined_path(file: &PluginFile) -> Result<PathBuf, SeamError> {
    let path = Path::new(&file.path);
    if file.path.is_empty() {
        return Err(SeamError::Refused(
            "a plugin file has an empty path".to_string(),
        ));
    }
    let confined = path
        .components()
        .all(|component| matches!(component, Component::Normal(_)));
    if !confined {
        return Err(SeamError::Refused(format!(
            "plugin file path `{}` must be relative and stay inside the plugin directory",
            file.path
        )));
    }
    Ok(path.to_path_buf())
}

/// Exactly one `.wasm` in the set: none is nothing to mount, several is
/// ambiguous about what the entry should point at.
fn the_one_artifact(files: &[PlannedFile]) -> Result<PathBuf, SeamError> {
    let artifacts: Vec<&PathBuf> = files
        .iter()
        .map(|planned| &planned.relative)
        .filter(|relative| {
            relative
                .extension()
                .is_some_and(|extension| extension == "wasm")
        })
        .collect();
    match artifacts.as_slice() {
        [one] => Ok((*one).clone()),
        [] => Err(SeamError::Refused(
            "the upload has no `.wasm` artifact".to_string(),
        )),
        several => Err(SeamError::Refused(format!(
            "the upload has {} `.wasm` artifacts; a plugin directory holds one",
            several.len()
        ))),
    }
}

/// Write the plan under `directory` and return the artifact's absolute
/// path — what the composition entry's `wasm:` ref names.
pub(crate) fn write(directory: &Path, plan: &InstallPlan) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(directory)?;
    for file in &plan.files {
        let target = directory.join(&file.relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&target, &file.bytes.0)?;
    }
    let artifact = directory.join(&plan.artifact);
    assert!(artifact.is_file(), "the plan's artifact was written");
    assert!(
        artifact.with_extension("manifest.toml").is_file(),
        "the plan's manifest was written beside the artifact"
    );
    Ok(artifact)
}

#[cfg(test)]
mod tests {
    use inseam_seams::operations::PluginId;

    use super::*;

    fn file(path: &str, bytes: &[u8]) -> PluginFile {
        PluginFile {
            path: path.to_string(),
            bytes: FileBytes(bytes.to_vec()),
        }
    }

    fn request(files: Vec<PluginFile>) -> InstallPluginRequest {
        InstallPluginRequest {
            id: PluginId::new("demo").expect("valid id"),
            files,
            config: toml::Table::new(),
        }
    }

    #[test]
    fn a_registry_shaped_directory_plans() {
        let plan = plan(request(vec![
            file("demo.wasm", b"\0asm"),
            file("demo.manifest.toml", b"name = \"demo\""),
            file("demo.checks.toml", b""),
            file("fixtures/pixel.png", b"png"),
        ]))
        .expect("plans");
        assert_eq!(plan.artifact, PathBuf::from("demo.wasm"));
        assert_eq!(plan.files.len(), 4);
        assert_eq!(plan.files[3].relative, PathBuf::from("fixtures/pixel.png"));
    }

    #[test]
    fn refuses_malformed_uploads() {
        let cases: Vec<(&str, Vec<PluginFile>)> = vec![
            ("no files", Vec::new()),
            ("no artifact", vec![file("demo.manifest.toml", b"")]),
            (
                "two artifacts",
                vec![
                    file("a.wasm", b""),
                    file("a.manifest.toml", b""),
                    file("b.wasm", b""),
                ],
            ),
            ("no manifest", vec![file("demo.wasm", b"")]),
            (
                "absolute path",
                vec![
                    file("/etc/demo.wasm", b""),
                    file("/etc/demo.manifest.toml", b""),
                ],
            ),
            (
                "parent escape",
                vec![
                    file("demo.wasm", b""),
                    file("demo.manifest.toml", b""),
                    file("../outside.txt", b""),
                ],
            ),
            (
                "duplicate path",
                vec![
                    file("demo.wasm", b""),
                    file("demo.manifest.toml", b""),
                    file("demo.manifest.toml", b""),
                ],
            ),
            (
                "empty path",
                vec![
                    file("demo.wasm", b""),
                    file("demo.manifest.toml", b""),
                    file("", b""),
                ],
            ),
        ];
        for (name, files) in cases {
            let outcome = plan(request(files));
            assert!(
                matches!(outcome, Err(SeamError::Refused(_))),
                "{name} should be refused, got {outcome:?}"
            );
        }
    }

    #[test]
    fn refuses_more_files_or_bytes_than_the_bounds() {
        let mut many = vec![file("demo.wasm", b""), file("demo.manifest.toml", b"")];
        for index in 0..PLUGIN_FILES_MAX {
            many.push(file(&format!("fixtures/{index}"), b""));
        }
        assert!(matches!(plan(request(many)), Err(SeamError::Refused(_))));

        let big = vec![0u8; usize::try_from(PLUGIN_UPLOAD_BYTES_MAX + 1).expect("fits")];
        let oversized = vec![file("demo.wasm", &big), file("demo.manifest.toml", b"")];
        assert!(matches!(
            plan(request(oversized)),
            Err(SeamError::Refused(_))
        ));
    }

    #[test]
    fn writing_a_plan_lays_the_directory_out_as_uploaded() {
        let directory = tempfile::tempdir().expect("tempdir");
        let plan = plan(request(vec![
            file("demo.wasm", b"\0asm"),
            file("demo.manifest.toml", b"name = \"demo\""),
            file("fixtures/pixel.png", b"png"),
        ]))
        .expect("plans");
        let target = directory.path().join("plugins").join("demo");
        let artifact = write(&target, &plan).expect("writes");
        assert_eq!(artifact, target.join("demo.wasm"));
        assert_eq!(
            std::fs::read(target.join("fixtures/pixel.png")).expect("fixture"),
            b"png"
        );
        assert_eq!(
            std::fs::read_to_string(target.join("demo.manifest.toml")).expect("manifest"),
            "name = \"demo\""
        );
    }
}
