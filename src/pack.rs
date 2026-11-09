//! Build EdgeForge artifacts (OCI image layouts packed into a single tar).
//!
//! Output is byte-for-byte reproducible: entries are sorted, timestamps and
//! ownership are zeroed, so the same app directory always produces the same
//! manifest digest.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

use flate2::write::GzEncoder;
use flate2::Compression;
use serde_json::json;

use crate::error::{EdgeError, Result};
use crate::oci::{sha256_digest, MT_CONFIG, MT_INDEX, MT_LAYER_GZIP, MT_MANIFEST};
use crate::spec::AppSpec;

/// File name of the spec inside an app source directory.
pub const SPEC_FILE: &str = "edgeforge.json";

/// In-memory OCI image layout: `path -> bytes`.
#[derive(Debug, Clone)]
pub struct LayoutFiles {
    pub files: BTreeMap<String, Vec<u8>>,
    pub manifest_digest: String,
    pub config_digest: String,
    pub layer_digests: Vec<String>,
}

pub fn blob_key(digest: &str) -> String {
    format!(
        "blobs/sha256/{}",
        digest.strip_prefix("sha256:").unwrap_or(digest)
    )
}

fn descriptor(media_type: &str, bytes: &[u8]) -> serde_json::Value {
    json!({ "mediaType": media_type, "digest": sha256_digest(bytes), "size": bytes.len() })
}

impl LayoutFiles {
    /// Assemble a layout from raw config and layer blobs. Media types are
    /// parameters so tests can build well-formed but invalid artifacts.
    pub fn build(
        config: &[u8],
        config_media_type: &str,
        layers: &[(Vec<u8>, &str)],
        version: &str,
    ) -> LayoutFiles {
        let mut files = BTreeMap::new();
        files.insert(
            "oci-layout".to_string(),
            br#"{"imageLayoutVersion":"1.0.0"}"#.to_vec(),
        );

        let config_digest = sha256_digest(config);
        files.insert(blob_key(&config_digest), config.to_vec());

        let mut layer_descs = Vec::new();
        let mut layer_digests = Vec::new();
        for (bytes, mt) in layers {
            let d = sha256_digest(bytes);
            files.insert(blob_key(&d), bytes.clone());
            layer_descs.push(descriptor(mt, bytes));
            layer_digests.push(d);
        }

        let manifest = serde_json::to_vec(&json!({
            "schemaVersion": 2,
            "mediaType": MT_MANIFEST,
            "config": descriptor(config_media_type, config),
            "layers": layer_descs,
            "annotations": { "org.opencontainers.image.version": version },
        }))
        .expect("manifest serializes");
        let manifest_digest = sha256_digest(&manifest);
        files.insert(blob_key(&manifest_digest), manifest.clone());

        let index = serde_json::to_vec(&json!({
            "schemaVersion": 2,
            "mediaType": MT_INDEX,
            "manifests": [{
                "mediaType": MT_MANIFEST,
                "digest": manifest_digest,
                "size": manifest.len(),
                "annotations": { "org.opencontainers.image.ref.name": version },
            }],
        }))
        .expect("index serializes");
        files.insert("index.json".to_string(), index);

        LayoutFiles {
            files,
            manifest_digest,
            config_digest,
            layer_digests,
        }
    }

    /// Write the layout as a single uncompressed tar bundle.
    pub fn write_bundle(&self, out: &Path) -> Result<()> {
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut builder = tar::Builder::new(File::create(out)?);
        for (path, data) in &self.files {
            append_file(&mut builder, path, data, 0o644)?;
        }
        builder.into_inner()?.sync_all()?;
        Ok(())
    }
}

fn append_file<W: Write>(
    b: &mut tar::Builder<W>,
    path: &str,
    data: &[u8],
    mode: u32,
) -> Result<()> {
    let mut h = tar::Header::new_gnu();
    h.set_path(path)?;
    h.set_size(data.len() as u64);
    h.set_mode(mode);
    h.set_mtime(0);
    h.set_uid(0);
    h.set_gid(0);
    h.set_entry_type(tar::EntryType::Regular);
    h.set_cksum();
    b.append(&h, data)?;
    Ok(())
}

fn append_dir<W: Write>(b: &mut tar::Builder<W>, path: &str) -> Result<()> {
    let mut h = tar::Header::new_gnu();
    h.set_path(format!("{path}/"))?;
    h.set_size(0);
    h.set_mode(0o755);
    h.set_mtime(0);
    h.set_entry_type(tar::EntryType::Directory);
    h.set_cksum();
    b.append(&h, std::io::empty())?;
    Ok(())
}

/// tar+gzip every file under `dir` (except the spec file), sorted, with
/// normalised metadata.
pub fn layer_from_dir(dir: &Path) -> Result<Vec<u8>> {
    let mut builder = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
    add_tree(&mut builder, dir, dir)?;
    Ok(builder.into_inner()?.finish()?)
}

fn add_tree<W: Write>(b: &mut tar::Builder<W>, root: &Path, dir: &Path) -> Result<()> {
    let mut entries: Vec<_> = fs::read_dir(dir)?.collect::<std::io::Result<_>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .expect("walk stays under root")
            .to_string_lossy()
            .into_owned();
        if rel == SPEC_FILE {
            continue;
        }
        let meta = fs::symlink_metadata(&path)?;
        if meta.is_dir() {
            append_dir(b, &rel)?;
            add_tree(b, root, &path)?;
        } else if meta.is_file() {
            use std::os::unix::fs::PermissionsExt;
            let mode = if meta.permissions().mode() & 0o111 != 0 {
                0o755
            } else {
                0o644
            };
            append_file(b, &rel, &fs::read(&path)?, mode)?;
        } else {
            return Err(EdgeError::Invalid(format!(
                "{} is not a regular file or directory",
                path.display()
            )));
        }
    }
    Ok(())
}

pub fn load_spec(dir: &Path) -> Result<AppSpec> {
    let path = dir.join(SPEC_FILE);
    let bytes = fs::read(&path)
        .map_err(|e| EdgeError::Invalid(format!("cannot read {}: {e}", path.display())))?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// Package an app directory into an artifact at `out`.
pub fn pack_app(dir: &Path, spec: &AppSpec, out: &Path) -> Result<LayoutFiles> {
    spec.validate()?;
    let config = serde_json::to_vec_pretty(spec)?;
    let layer = layer_from_dir(dir)?;
    let layout = LayoutFiles::build(&config, MT_CONFIG, &[(layer, MT_LAYER_GZIP)], &spec.version);
    layout.write_bundle(out)?;
    Ok(layout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packing_is_reproducible() {
        let t = tempfile::tempdir().unwrap();
        let app = t.path().join("app");
        fs::create_dir_all(app.join("model")).unwrap();
        fs::write(app.join("run.sh"), "#!/bin/sh\nexit 0\n").unwrap();
        fs::write(app.join("model/weights.bin"), [1u8, 2, 3]).unwrap();
        let spec: AppSpec =
            serde_json::from_str(r#"{"name":"det","version":"1.0.0","entrypoint":["./run.sh"]}"#)
                .unwrap();
        let a = pack_app(&app, &spec, &t.path().join("a.tar")).unwrap();
        let b = pack_app(&app, &spec, &t.path().join("b.tar")).unwrap();
        assert_eq!(a.manifest_digest, b.manifest_digest);
        assert_eq!(
            fs::read(t.path().join("a.tar")).unwrap(),
            fs::read(t.path().join("b.tar")).unwrap()
        );
    }
}
