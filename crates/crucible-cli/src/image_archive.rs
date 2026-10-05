//! `docker save` archives of uploaded plugins' images (docs/plugins.md
//! §14.3): built once at registration, stored as a sealed blob (gzip), and
//! loaded by every scoring job instead of a rebuild.
//!
//! An archive is read without Docker: its `manifest.json` names the config
//! and the layers; the image id is the SHA-256 of the config, and every
//! layer is checked against its content address (OCI layout, `blobs/
//! sha256/<hex>`) or the config's `rootfs.diff_ids` (older layouts), so
//! the id pins the whole image.

use std::collections::HashMap;
use std::io::{Read, Write};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Largest image (uncompressed archive) a plugin may have.
pub const MAX_IMAGE_BYTES: u64 = 2 << 30;

/// A parsed archive: its files and what `manifest.json` says.
pub struct Archive {
    files: HashMap<String, (usize, usize)>,
    data: Vec<u8>,
    /// `sha256:<hex>` of the config.
    pub id: String,
    pub config: String,
    pub layers: Vec<String>,
}

#[derive(Deserialize)]
struct Entry {
    #[serde(rename = "Config")]
    config: String,
    #[serde(rename = "Layers", default)]
    layers: Vec<String>,
}

fn sha_hex(b: &[u8]) -> String {
    hex::encode(Sha256::digest(b))
}

fn octal(field: &[u8]) -> Result<usize> {
    let s = std::str::from_utf8(field).map_err(|_| anyhow!("bad tar header"))?;
    let s = s.trim_matches(|c: char| c == '\0' || c == ' ');
    if s.is_empty() {
        return Ok(0);
    }
    usize::from_str_radix(s, 8).map_err(|_| anyhow!("bad tar size"))
}

/// Regular files of a ustar archive: name -> (offset, len) into `data`.
fn tar_index(data: &[u8]) -> Result<HashMap<String, (usize, usize)>> {
    let mut out = HashMap::new();
    let mut pos = 0usize;
    let mut long_name: Option<String> = None;
    while pos + 512 <= data.len() {
        let h = &data[pos..pos + 512];
        if h.iter().all(|&b| b == 0) {
            break;
        }
        let size = octal(&h[124..136])?;
        let start = pos + 512;
        let end = start
            .checked_add(size)
            .filter(|&e| e <= data.len())
            .ok_or_else(|| anyhow!("truncated tar"))?;
        let cut = |f: &[u8]| -> String {
            let n = f.iter().position(|&b| b == 0).unwrap_or(f.len());
            String::from_utf8_lossy(&f[..n]).into_owned()
        };
        let mut name = cut(&h[0..100]);
        let prefix = cut(&h[345..500]);
        if &h[257..262] == b"ustar" && !prefix.is_empty() {
            name = format!("{prefix}/{name}");
        }
        match h[156] {
            b'L' => long_name = Some(cut(&data[start..end])),
            b'x' => {
                // PAX: only `path` matters here.
                let rec = String::from_utf8_lossy(&data[start..end]).into_owned();
                for line in rec.lines() {
                    if let Some((_, kv)) = line.split_once(' ')
                        && let Some(p) = kv.strip_prefix("path=")
                    {
                        long_name = Some(p.to_owned());
                    }
                }
            }
            b'0' | 0 => {
                let n = long_name.take().unwrap_or(name);
                out.insert(n.trim_start_matches("./").to_owned(), (start, size));
            }
            _ => long_name = None,
        }
        pos = start + size.div_ceil(512) * 512;
    }
    Ok(out)
}

impl Archive {
    /// Parse and verify an uncompressed `docker save` archive.
    pub fn parse(data: Vec<u8>) -> Result<Archive> {
        if data.len() as u64 > MAX_IMAGE_BYTES {
            bail!(
                "the image is larger than {} MB",
                MAX_IMAGE_BYTES >> 20
            );
        }
        let files = tar_index(&data)?;
        let get = |n: &str| -> Result<&[u8]> {
            let (o, l) = files
                .get(n)
                .ok_or_else(|| anyhow!("image archive: {n} missing"))?;
            Ok(&data[*o..*o + *l])
        };
        let entries: Vec<Entry> =
            serde_json::from_slice(get("manifest.json")?).context("image archive: manifest.json")?;
        let [e] = entries.as_slice() else {
            bail!("image archive: expected exactly one image");
        };
        let config = get(&e.config)?;
        let id = format!("sha256:{}", sha_hex(config));
        let cfg: serde_json::Value = serde_json::from_slice(config).context("image config")?;
        let diff_ids: Vec<&str> = cfg["rootfs"]["diff_ids"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        if diff_ids.len() != e.layers.len() {
            bail!("image archive: layers do not match the config");
        }
        for (l, diff) in e.layers.iter().zip(&diff_ids) {
            let got = sha_hex(get(l)?);
            let addressed = l
                .strip_prefix("blobs/sha256/")
                .filter(|h| crucible_core::blob::is_sha256_hex(h));
            let ok = match addressed {
                Some(h) => h == got,
                None => diff.strip_prefix("sha256:") == Some(got.as_str()),
            };
            if !ok {
                bail!("image archive: layer {l} does not match its digest");
            }
        }
        if let Some(h) = e.config.strip_prefix("blobs/sha256/")
            && format!("sha256:{h}") != id
        {
            bail!("image archive: config does not match its digest");
        }
        Ok(Archive {
            config: e.config.clone(),
            layers: e.layers.clone(),
            files,
            data,
            id,
        })
    }

    pub fn file(&self, name: &str) -> Option<&[u8]> {
        self.files.get(name).map(|(o, l)| &self.data[*o..*o + *l])
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.data
    }
}

pub fn gzip(data: &[u8]) -> Result<Vec<u8>> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    e.write_all(data)?;
    Ok(e.finish()?)
}

/// Gunzip, refusing more than [`MAX_IMAGE_BYTES`].
pub fn gunzip(data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(data)
        .take(MAX_IMAGE_BYTES + 1)
        .read_to_end(&mut out)
        .context("image archive: not gzip")?;
    if out.len() as u64 > MAX_IMAGE_BYTES {
        bail!("the image is larger than {} MB", MAX_IMAGE_BYTES >> 20);
    }
    Ok(out)
}

/// The media type of a layer blob, from its first bytes.
pub fn layer_media_type(b: &[u8]) -> &'static str {
    if b.starts_with(&[0x1f, 0x8b]) {
        "application/vnd.oci.image.layer.v1.tar+gzip"
    } else if b.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        "application/vnd.oci.image.layer.v1.tar+zstd"
    } else {
        "application/vnd.oci.image.layer.v1.tar"
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    fn tar_entry(out: &mut Vec<u8>, name: &str, body: &[u8]) {
        let mut h = [0u8; 512];
        h[..name.len()].copy_from_slice(name.as_bytes());
        h[100..107].copy_from_slice(b"0000644");
        let size = format!("{:011o}", body.len());
        h[124..135].copy_from_slice(size.as_bytes());
        h[156] = b'0';
        h[257..262].copy_from_slice(b"ustar");
        out.extend_from_slice(&h);
        out.extend_from_slice(body);
        out.resize(out.len().div_ceil(512) * 512, 0);
    }

    /// A minimal OCI-layout `docker save` archive with one layer.
    pub fn sample(layer: &[u8]) -> (Vec<u8>, String) {
        let lh = sha_hex(layer);
        let cfg = format!(
            r#"{{"architecture":"amd64","os":"linux","rootfs":{{"type":"layers","diff_ids":["sha256:{lh}"]}}}}"#
        );
        let ch = sha_hex(cfg.as_bytes());
        let manifest = format!(
            r#"[{{"Config":"blobs/sha256/{ch}","RepoTags":["x:run"],"Layers":["blobs/sha256/{lh}"]}}]"#
        );
        let mut t = Vec::new();
        tar_entry(&mut t, &format!("blobs/sha256/{ch}"), cfg.as_bytes());
        tar_entry(&mut t, &format!("blobs/sha256/{lh}"), layer);
        tar_entry(&mut t, "manifest.json", manifest.as_bytes());
        t.extend_from_slice(&[0u8; 1024]);
        (t, format!("sha256:{ch}"))
    }

    #[test]
    fn parses_and_verifies() {
        let (t, id) = sample(b"layer bytes");
        let a = Archive::parse(t.clone()).unwrap();
        assert_eq!(a.id, id);
        assert_eq!(a.layers.len(), 1);
        assert_eq!(gunzip(&gzip(&t).unwrap()).unwrap(), t);
        // A layer that does not match its address is refused.
        let mut bad = t.clone();
        let i = bad.windows(11).position(|w| w == b"layer bytes").unwrap();
        bad[i] = b'L';
        assert!(Archive::parse(bad).is_err());
        assert!(Archive::parse(b"not a tar".to_vec()).is_err());
    }
}
