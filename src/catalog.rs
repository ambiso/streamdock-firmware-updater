use std::{collections::BTreeMap, io::Read};

use flate2::read::GzDecoder;
use md5::Digest;
use md5::Md5;
use serde::Deserialize;

use crate::{Feedback, Result, download};

const CDN: &str = "https://cdn1.key123.vip/";
const DOWNLOAD_PREFIX: &str = "https://cdn1.key123.vip/StreamDock/firmware/download/";
const MIRABOX_DOWNLOAD_PREFIX: &str = "https://cdn.mirabox.net/StreamDock/firmware/download/";
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
        let filename = url
            .strip_prefix(DOWNLOAD_PREFIX)
            .or_else(|| url.strip_prefix(MIRABOX_DOWNLOAD_PREFIX))?;
        // The vendor uses both literal and escaped spaces in filenames.
        // Accept one filename component and leave other escapes rejected.
        let filename = filename.replace("%20", " ");
        let stem = filename.strip_suffix(".img")?;
        if stem.is_empty()
            || !stem.bytes().all(|byte| {
                byte.is_ascii_alphanumeric()
                    || matches!(byte, b' ' | b'.' | b'_' | b'-' | b'(' | b')' | b'+')
            })
        {
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

    pub fn download(&self, feedback: &Feedback<'_>) -> Result<Vec<u8>> {
        let url = self
            .entry
            .image_url()
            .ok_or("Invalid firmware image URL.")?;
        let md5 = self.entry.md5.as_deref();
        let sha256 = self.entry.sha256.as_deref();
        if md5.is_none() && sha256.is_none() {
            return Err("The catalog does not provide a firmware checksum.".into());
        }

        let image = download(&url.replace(' ', "%20"), MAX_IMAGE, 0, feedback)?;
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

pub enum Lookup {
    Available(Update),
    Current,
    NoCompatibleUpdate,
    ModelRequired(Vec<String>),
}

impl Lookup {
    fn into_update(self, installed: &str) -> Result<Option<Update>> {
        match self {
            Self::Available(update) => Ok(Some(update)),
            Self::Current | Self::NoCompatibleUpdate => Ok(None),
            Self::ModelRequired(models) => Err(format!(
                "Firmware {installed} matches several models: {}. Use --model with the exact catalog name.",
                models.join(", ")
            ).into()),
        }
    }
}

pub fn latest(installed: &str, model: Option<&str>) -> Result<Option<Update>> {
    Catalogs::fetch(&Feedback(None))?
        .lookup(installed, model)?
        .into_update(installed)
}

pub struct Catalogs(Vec<Catalog>);

impl Catalogs {
    pub fn fetch(feedback: &Feedback<'_>) -> Result<Self> {
        let catalogs = MANIFESTS
            .iter()
            .map(|path| {
                let bytes = download(&format!("{CDN}{path}"), MAX_MANIFEST, 0, feedback)?;
                parse_catalog(&bytes).map_err(|error| format!("{path}: {error}").into())
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self(catalogs))
    }

    pub fn lookup(&self, installed: &str, model: Option<&str>) -> Result<Lookup> {
        select_update(&self.0, installed, model)
    }
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

fn select_update(catalogs: &[Catalog], installed: &str, model: Option<&str>) -> Result<Lookup> {
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
        return Ok(Lookup::ModelRequired(
            matches.keys().map(|name| (*name).to_owned()).collect(),
        ));
    }
    let Some((model, models)) = matches.into_iter().next() else {
        return Err(format!(
            "No catalog entry matches firmware {installed:?} and model {model:?}."
        )
        .into());
    };
    let mut candidates = BTreeMap::<_, Vec<_>>::new();
    let mut latest_listed = None;
    for (variant, entry) in models.iter().flat_map(|model| model.entries()) {
        if variant != family {
            continue;
        }
        if let Some((_, number)) = parse_version(&entry.version) {
            latest_listed = latest_listed.max(Some(number));
            if number > installed_number && entry.image_url().is_some() {
                candidates.entry(number).or_default().push(entry);
            }
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
        return Ok(Lookup::Available(Update {
            model: model.to_owned(),
            entry: chosen.clone(),
            alternate_versions,
        }));
    }
    Ok(if latest_listed == Some(installed_number) {
        Lookup::Current
    } else {
        Lookup::NoCompatibleUpdate
    })
}

#[cfg(test)]
mod tests {
    use super::{Catalog, Lookup};
    use crate::Result;
    use serde_json::json;

    const URL: &str = "https://cdn1.key123.vip/StreamDock/firmware/download/dock.img";

    #[test]
    fn accepts_the_current_vendor_image_host() {
        let catalog = serde_json::from_value::<Catalog>(json!({
            "CurrentVersion": {
                "MBox-N4 E": {
                    "variants": {
                        "V3.Mbox_N4E": {
                            "Version": "V3.Mbox_N4E.02.018",
                            "fileURL": "https://cdn.mirabox.net/StreamDock/firmware/download/V3.Mbox_N4E_HJDF.02.018.img"
                        }
                    }
                }
            }
        }))
        .unwrap();
        let update = select_update(&[catalog], "V3.Mbox_N4E.02.014", None)
            .unwrap()
            .unwrap();
        assert_eq!(update.version(), "V3.Mbox_N4E.02.018");
    }

    #[test]
    fn accepts_only_a_single_firmware_filename_under_the_download_prefix() {
        for filename in ["A dock.img", "A%20dock.img", "V3.Dock.02.018.img"] {
            let url = format!("{}{filename}", super::MIRABOX_DOWNLOAD_PREFIX);
            let entry = super::Entry {
                file_url: Some(url.clone()),
                ..Default::default()
            };
            assert_eq!(entry.image_url(), Some(url.as_str()));
        }
        for filename in [
            "../dock.img",
            "subdir/dock.img",
            r"subdir\dock.img",
            "subdir%2Fdock.img",
            "subdir%5Cdock.img",
            "subdir%252Fdock.img",
            "dock?file=other.img",
            "dock#other.img",
        ] {
            let entry = super::Entry {
                file_url: Some(format!("{}{filename}", super::MIRABOX_DOWNLOAD_PREFIX)),
                ..Default::default()
            };
            assert_eq!(entry.image_url(), None, "{filename}");
        }
    }

    fn select_update(
        catalogs: &[Catalog],
        installed: &str,
        model: Option<&str>,
    ) -> Result<Option<super::Update>> {
        super::select_update(catalogs, installed, model)?.into_update(installed)
    }

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
        assert!(matches!(
            super::select_update(&catalogs, "V3.Dock.02.010", None).unwrap(),
            Lookup::Current
        ));
        assert!(matches!(
            super::select_update(&catalogs, "V3.Dock.02.011", None).unwrap(),
            Lookup::NoCompatibleUpdate
        ));
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
