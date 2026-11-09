//! OCI image-layout artifacts and their verification.
//!
//! An EdgeForge artifact is a plain tar of an [OCI image layout]:
//!
//! ```text
//! oci-layout
//! index.json                    -> manifest descriptor
//! blobs/sha256/<manifest>       -> config + layer descriptors
//! blobs/sha256/<config>         -> AppSpec JSON
//! blobs/sha256/<layer>          -> tar+gzip of the app files
//! ```
//!
//! Every blob is checked against the size *and* SHA-256 digest in the
//! descriptor that references it, starting from `index.json`. A single
//! flipped byte anywhere in the chain causes the artifact to be rejected
//! before anything is written to an app slot.
//!
//! [OCI image layout]: https://github.com/opencontainers/image-spec/blob/main/image-layout.md

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{EdgeError, Result};
use crate::fsutil;
use crate::spec::AppSpec;

pub const MT_INDEX: &str = "application/vnd.oci.image.index.v1+json";
pub const MT_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
pub const MT_CONFIG: &str = "application/vnd.edgeforge.app.config.v1+json";
pub const MT_LAYER_GZIP: &str = "application/vnd.oci.image.layer.v1.tar+gzip";

/// JSON documents (index, manifest, config) larger than this are rejected.
const MAX_JSON_BLOB: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Descriptor {
    pub media_type: String,
    pub digest: String,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Index {
    pub schema_version: u32,
    #[serde(default)]
    pub media_type: Option<String>,
    pub manifests: Vec<Descriptor>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub schema_version: u32,
    #[serde(default)]
    pub media_type: Option<String>,
    pub config: Descriptor,
    pub layers: Vec<Descriptor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageLayout {
    pub image_layout_version: String,
}

/// An artifact whose whole descriptor chain has been verified.
#[derive(Debug, Clone)]
pub struct VerifiedImage {
    pub spec: AppSpec,
    pub manifest_digest: String,
    pub layers: Vec<PathBuf>,
}

pub fn sha256_digest(bytes: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
}

/// Validate `sha256:<64 lowercase hex>` and return the hex part.
pub fn digest_hex(digest: &str) -> Result<&str> {
    let hex = digest
        .strip_prefix("sha256:")
        .ok_or_else(|| EdgeError::Verify(format!("unsupported digest algorithm in '{digest}'")))?;
    if hex.len() != 64 || !hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return Err(EdgeError::Verify(format!("malformed digest '{digest}'")));
    }
    Ok(hex)
}

pub fn blob_path(layout: &Path, digest: &str) -> Result<PathBuf> {
    Ok(layout
        .join("blobs")
        .join("sha256")
        .join(digest_hex(digest)?))
}

fn open_blob(layout: &Path, desc: &Descriptor) -> Result<(PathBuf, File)> {
    let path = blob_path(layout, &desc.digest)?;
    let file = File::open(&path).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => EdgeError::Verify(format!("missing blob {}", desc.digest)),
        _ => EdgeError::Io(e),
    })?;
    let actual = file.metadata()?.len();
    if actual != desc.size {
        return Err(EdgeError::Verify(format!(
            "size mismatch for {}: descriptor says {} bytes, blob has {}",
            desc.digest, desc.size, actual
        )));
    }
    Ok((path, file))
}

fn check_digest(desc: &Descriptor, computed: String) -> Result<()> {
    if computed != desc.digest {
        return Err(EdgeError::Verify(format!(
            "digest mismatch: expected {}, computed {}",
            desc.digest, computed
        )));
    }
    Ok(())
}

/// Stream a blob through SHA-256 and compare against its descriptor.
pub fn verify_blob(layout: &Path, desc: &Descriptor) -> Result<PathBuf> {
    let (path, mut file) = open_blob(layout, desc)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    check_digest(desc, format!("sha256:{}", hex::encode(hasher.finalize())))?;
    Ok(path)
}

/// Read a small JSON blob, verify it, and return its bytes.
pub fn read_verified(layout: &Path, desc: &Descriptor) -> Result<Vec<u8>> {
    if desc.size > MAX_JSON_BLOB {
        return Err(EdgeError::Verify(format!(
            "blob {} is {} bytes, limit for metadata is {}",
            desc.digest, desc.size, MAX_JSON_BLOB
        )));
    }
    let (_, mut file) = open_blob(layout, desc)?;
    let mut bytes = Vec::with_capacity(desc.size as usize);
    file.read_to_end(&mut bytes)?;
    check_digest(desc, sha256_digest(&bytes))?;
    Ok(bytes)
}

fn read_layout_json<T: serde::de::DeserializeOwned>(layout: &Path, name: &str) -> Result<T> {
    let bytes = fs::read(layout.join(name)).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => EdgeError::Verify(format!("missing {name}")),
        _ => EdgeError::Io(e),
    })?;
    serde_json::from_slice(&bytes).map_err(|e| EdgeError::Verify(format!("malformed {name}: {e}")))
}

/// Verify the full chain `index.json -> manifest -> config + layers`.
pub fn verify_layout(layout: &Path) -> Result<VerifiedImage> {
    let marker: ImageLayout = read_layout_json(layout, "oci-layout")?;
    if marker.image_layout_version != "1.0.0" {
        return Err(EdgeError::Verify(format!(
            "unsupported imageLayoutVersion {}",
            marker.image_layout_version
        )));
    }

    let index: Index = read_layout_json(layout, "index.json")?;
    if index.schema_version != 2 {
        return Err(EdgeError::Verify(format!(
            "unsupported index schemaVersion {}",
            index.schema_version
        )));
    }
    let mdesc = index
        .manifests
        .iter()
        .find(|d| d.media_type == MT_MANIFEST)
        .ok_or_else(|| EdgeError::Verify("index.json references no image manifest".into()))?;

    let manifest: Manifest = serde_json::from_slice(&read_verified(layout, mdesc)?)
        .map_err(|e| EdgeError::Verify(format!("malformed manifest: {e}")))?;
    if manifest.schema_version != 2 {
        return Err(EdgeError::Verify(format!(
            "unsupported manifest schemaVersion {}",
            manifest.schema_version
        )));
    }
    if manifest.config.media_type != MT_CONFIG {
        return Err(EdgeError::Verify(format!(
            "unsupported config media type '{}', expected '{}'",
            manifest.config.media_type, MT_CONFIG
        )));
    }

    let spec: AppSpec = serde_json::from_slice(&read_verified(layout, &manifest.config)?)
        .map_err(|e| EdgeError::Verify(format!("malformed app config: {e}")))?;
    spec.validate()?;

    if manifest.layers.is_empty() {
        return Err(EdgeError::Verify("manifest has no layers".into()));
    }
    let mut layers = Vec::with_capacity(manifest.layers.len());
    for layer in &manifest.layers {
        if layer.media_type != MT_LAYER_GZIP {
            return Err(EdgeError::Verify(format!(
                "unsupported layer media type '{}'",
                layer.media_type
            )));
        }
        layers.push(verify_blob(layout, layer)?);
    }

    Ok(VerifiedImage {
        spec,
        manifest_digest: mdesc.digest.clone(),
        layers,
    })
}

/// Unpack verified layers, in order, into `dest`.
pub fn unpack_layers(image: &VerifiedImage, dest: &Path) -> Result<u64> {
    let mut files = 0;
    for layer in &image.layers {
        let gz = flate2::read::GzDecoder::new(File::open(layer)?);
        files += fsutil::safe_unpack(gz, dest)?;
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_format_is_strict() {
        let good = sha256_digest(b"edge");
        assert!(digest_hex(&good).is_ok());
        assert!(digest_hex("md5:abc").is_err());
        assert!(digest_hex("sha256:ABC").is_err());
        assert!(digest_hex(&format!("sha256:{}", "g".repeat(64))).is_err());
        assert!(digest_hex(&format!("sha256:../../{}", "a".repeat(58))).is_err());
    }

    #[test]
    fn known_sha256_vector() {
        assert_eq!(
            sha256_digest(b"abc"),
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
