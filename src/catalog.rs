use std::{collections::BTreeMap, io::Read};

use flate2::read::GzDecoder;
use md5::Digest;
use md5::Md5;
use serde::Deserialize;

use crate::{Result, download};

const CDN: &str = "https://cdn1.key123.vip/";
const DOWNLOAD_PREFIX: &str = "https://cdn1.key123.vip/StreamDock/firmware/download/";
const MANIFESTS: &[&str] = &[
    "StreamDock/firmware/firmware-version-check.json",
    "StreamDock/firmware/firmware-version-check-test.json",
];
const MAX_MANIFEST: u64 = 2 * 1024 * 1024;
const MAX_IMAGE: u64 = 128 * 1024 * 1024;
const TESTED_IMAGE: &str = "V3.Mbox_N4E_HJDF.02.018.img";
const TESTED_SHA256: &str = "a24c149bb44d9ee7db042bb521a9f1a53f9cb8750fc76fcc9ed0e6a83882ffb1";

#[derive(Deserialize)]
struct Catalog {
    #[serde(rename = "CurrentVersion")]
    models: BTreeMap<String, Model>,
}

#[derive(Deserialize)]
struct Model {
    #[serde(default)]
    variants: BTreeMap<String, Entry>,
    #[serde(flatten)]
    legacy: Entry,
}

#[derive(Clone, Default, Deserialize)]
struct Entry {
    #[serde(rename = "Version", default)]
    version: String,
    #[serde(rename = "MiniVersion")]
    minimum: Option<String>,
    #[serde(rename = "Prohibited", default)]
    prohibited: Vec<String>,
    #[serde(rename = "fileURL")]
    file_url: Option<String>,
    #[serde(rename = "downloadURL")]
    download_url: Option<String>,
    md5: Option<String>,
    #[serde(alias = "SHA256")]
    sha256: Option<String>,
}

impl Entry {
    fn image_url(&self) -> Option<&str> {
        let url = [self.file_url.as_deref(), self.download_url.as_deref()]
            .into_iter()
            .flatten()
            .find(|url| !url.is_empty())?;
        let filename = url.strip_prefix(DOWNLOAD_PREFIX)?;
        if filename.contains(['/', '\\', '?', '#']) || !filename.ends_with(".img") {
            return None;
        }
        Some(url)
    }

    fn allows(&self, installed: &str, number: (u32, u32)) -> Result<bool> {
        if self.prohibited.iter().any(|version| version == installed) {
            return Ok(false);
        }
        if let Some(minimum) = self
            .minimum
            .as_deref()
            .filter(|minimum| !minimum.is_empty())
        {
            let (_, minimum_number) = parse_version(minimum)
                .ok_or_else(|| format!("Unrecognized minimum firmware version {minimum:?}."))?;
            return Ok(number >= minimum_number);
        }
        Ok(true)
    }

    fn same_image(&self, other: &Self) -> bool {
        self.image_url() == other.image_url()
            && match (&self.md5, &other.md5) {
                (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
                (None, None) => true,
                _ => false,
            }
            && match (&self.sha256, &other.sha256) {
                (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
                _ => true,
            }
    }
}

impl Model {
    fn entries(&self) -> impl Iterator<Item = (&str, &Entry)> {
        let legacy = parse_version(&self.legacy.version).map(|(family, _)| (family, &self.legacy));
        self.variants
            .iter()
            .map(|(key, entry)| (key.as_str(), entry))
            .chain(legacy)
    }
}

fn parse_version(version: &str) -> Option<(&str, (u32, u32))> {
    let (prefix, build) = version.rsplit_once('.')?;
    let (family, major) = prefix.rsplit_once(['.', '_'])?;
    if family.is_empty() {
        return None;
    }
    Some((family, (major.parse().ok()?, build.parse().ok()?)))
}

pub struct Update {
    pub model: String,
    entry: Entry,
    alternate_versions: Vec<String>,
}

impl Update {
    pub fn version(&self) -> &str {
        &self.entry.version
    }

    pub fn matches_version(&self, reported: &str) -> bool {
        self.version() == reported
            || self
                .alternate_versions
                .iter()
                .any(|version| version == reported)
    }

    pub fn download(&self) -> Result<Vec<u8>> {
        let url = self
            .entry
            .image_url()
            .ok_or("Invalid firmware image URL.")?;
        let md5 = self.entry.md5.as_deref();
        let sha256 = self.entry.sha256.as_deref();
        if md5.is_none() && sha256.is_none() {
            return Err("The catalog does not provide a firmware checksum.".into());
        }

        let image = download(&url.replace(' ', "%20"), MAX_IMAGE, 0)?;
        if let Some(expected) = md5 {
            let actual = format!("{:x}", Md5::digest(&image));
            if !actual.eq_ignore_ascii_case(expected) {
                return Err(
                    format!("Firmware MD5 mismatch: expected {expected}, got {actual}.").into(),
                );
            }
        }
        if let Some(expected) = sha256 {
            crate::verify_sha256(&image, expected, url)?;
        }
        if url.ends_with(TESTED_IMAGE) {
            crate::verify_sha256(&image, TESTED_SHA256, url)?;
        }
        if !image.starts_with(b"AIC.FW\0\0") {
            return Err("The selected image is not in the ArtInChip AIC.FW format.".into());
        }
        Ok(image)
    }
}

pub fn latest(installed: &str, model: Option<&str>) -> Result<Option<Update>> {
    let catalogs = MANIFESTS
        .iter()
        .map(|path| {
            let bytes = download(&format!("{CDN}{path}"), MAX_MANIFEST, 0)?;
            parse_catalog(&bytes).map_err(|error| format!("{path}: {error}").into())
        })
        .collect::<Result<Vec<_>>>()?;
    select_update(&catalogs, installed, model)
}

fn parse_catalog(bytes: &[u8]) -> Result<Catalog> {
    if bytes.starts_with(&[0x1f, 0x8b]) {
        // The test catalog can be gzip-compressed without an HTTP encoding header.
        let mut decoded = Vec::new();
        GzDecoder::new(bytes)
            .take(MAX_MANIFEST + 1)
            .read_to_end(&mut decoded)?;
        if decoded.len() as u64 > MAX_MANIFEST {
            return Err("The decompressed catalog exceeds the size limit.".into());
        }
        return Ok(serde_json::from_slice(&decoded)?);
    }
    Ok(serde_json::from_slice(bytes)?)
}

fn select_update(
    catalogs: &[Catalog],
    installed: &str,
    model: Option<&str>,
) -> Result<Option<Update>> {
    let (family, installed_number) = parse_version(installed)
        .ok_or_else(|| format!("Unrecognized installed firmware version {installed:?}."))?;
    let mut matches = BTreeMap::<&str, Vec<&Model>>::new();
    for catalog in catalogs {
        for (key, entry) in &catalog.models {
            if model.is_none_or(|model| key == model)
                && entry.entries().any(|(variant, _)| variant == family)
            {
                matches.entry(key).or_default().push(entry);
            }
        }
    }
    if matches.len() > 1 {
        let models = matches.keys().copied().collect::<Vec<_>>().join(", ");
        return Err(format!("Firmware family {family} matches several models: {models}. Use --model with the exact catalog name.").into());
    }
    let Some((model, models)) = matches.into_iter().next() else {
        return Err(format!(
            "No catalog entry matches firmware {installed:?} and model {model:?}."
        )
        .into());
    };
    let mut candidates = BTreeMap::<_, Vec<_>>::new();
    for (variant, entry) in models.iter().flat_map(|model| model.entries()) {
        if variant != family || entry.image_url().is_none() {
            continue;
        }
        if let Some((_, number)) = parse_version(&entry.version)
            && number > installed_number
        {
            candidates.entry(number).or_default().push(entry);
        }
    }
    'versions: for (number, entries) in candidates.into_iter().rev() {
        for entry in &entries {
            if !entry.allows(installed, installed_number)? {
                continue 'versions;
            }
        }
        let chosen = entries
            .iter()
            .copied()
            .max_by_key(|entry| entry.sha256.is_some())
            .expect("each candidate version has at least one catalog entry");
        if entries.iter().any(|entry| !chosen.same_image(entry)) {
            return Err(format!(
                "The catalogs disagree about the image for {}.",
                chosen.version
            )
            .into());
        }
        let alternate_versions = models
            .iter()
            .flat_map(|model| model.entries())
            .filter(|(_, entry)| {
                chosen.same_image(entry)
                    && parse_version(&entry.version).is_some_and(|(_, version)| version == number)
            })
            .map(|(_, entry)| entry.version.clone())
            .collect();
        return Ok(Some(Update {
            model: model.to_owned(),
            entry: chosen.clone(),
            alternate_versions,
        }));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::{Catalog, select_update};
    use serde_json::json;

    const URL: &str = "https://cdn1.key123.vip/StreamDock/firmware/download/dock.img";

    #[test]
    fn picks_the_latest_allowed_numeric_version() {
        let legacy = serde_json::from_value::<Catalog>(json!({
            "CurrentVersion": {
                "Dock": {"Version": "V3.Dock.02.009", "fileURL": URL}
            }
        }))
        .unwrap();
        let variants = serde_json::from_value::<Catalog>(json!({
            "CurrentVersion": {
                "Dock": {
                    "variants": {
                        "V3.Dock": {
                            "Version": "V3.Dock.02.010",
                            "MiniVersion": "V3.Dock.01.002",
                            "Prohibited": ["V3.Dock.01.003"],
                            "fileURL": URL
                        }
                    }
                }
            }
        }))
        .unwrap();
        let duplicate = serde_json::from_value::<Catalog>(json!({
            "CurrentVersion": {
                "Dock": {"Version": "V3.Dock.02.010", "fileURL": URL}
            }
        }))
        .unwrap();
        let catalogs = [legacy, variants, duplicate];

        for (installed, expected) in [
            ("V3.Dock.01.001", "V3.Dock.02.009"),
            ("V3.Dock.01.003", "V3.Dock.02.009"),
            ("V3.Dock.01.004", "V3.Dock.02.010"),
        ] {
            let update = select_update(&catalogs, installed, None).unwrap().unwrap();
            assert_eq!(update.version(), expected);
        }
        assert!(
            select_update(&catalogs, "V3.Dock.02.011", None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn shared_firmware_family_requires_an_exact_model() {
        let catalogs = [serde_json::from_value::<Catalog>(json!({
            "CurrentVersion": {
                "First": {"Version": "V3.Dock.02.009", "fileURL": URL},
                "Second": {"Version": "V3.Dock.02.010", "fileURL": URL}
            }
        }))
        .unwrap()];

        assert!(select_update(&catalogs, "V3.Dock.01.001", None).is_err());
        let update = select_update(&catalogs, "V3.Dock.01.001", Some("First"))
            .unwrap()
            .unwrap();
        assert_eq!(update.version(), "V3.Dock.02.009");
    }

    #[test]
    fn accepts_a_reported_alias_only_for_the_same_image() {
        let catalogs = [serde_json::from_value::<Catalog>(json!({
            "CurrentVersion": {
                "Dock": {
                    "variants": {
                        "V3.Dock": {"Version": "V3.Dock.02.018", "fileURL": URL, "md5": "abc"},
                        "V3.New": {"Version": "V3.New.02.018", "fileURL": URL, "md5": "abc"},
                        "V3.Other": {"Version": "V3.Other.02.018", "fileURL": URL, "md5": "def"}
                    }
                }
            }
        }))
        .unwrap()];

        let update = select_update(&catalogs, "V3.Dock.01.001", None)
            .unwrap()
            .unwrap();
        assert!(update.matches_version("V3.New.02.018"));
        assert!(!update.matches_version("V3.Other.02.018"));
    }
}
