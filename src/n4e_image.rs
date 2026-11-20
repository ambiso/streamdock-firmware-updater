use std::{fs::File, io::Read, path::Path, str};

use flate2::Crc;

use crate::Result;

pub const USB_ID: &str = "6603:1007";
pub const PARTITION_TABLE: &str =
    "spi0.0:512k(spl),128k(env),128k(env_r),2m(os),1m(rodata),10m(data);";
const MAX_IMAGE: u64 = 128 * 1024 * 1024;
const META_SIZE: usize = 512;

pub struct Component<'a> {
    pub name: &'static str,
    pub partition: &'static str,
    pub address: u64,
    pub metadata: &'a [u8],
    pub data: &'a [u8],
    pub checksum: u32,
}

pub struct N4eImage<'a> {
    pub version: &'a str,
    pub info: Component<'a>,
    pub firmware: [Component<'a>; 2],
}

pub fn read_image(path: &Path) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(MAX_IMAGE + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_IMAGE {
        return Err("Firmware image exceeds 128 MiB.".into());
    }
    Ok(bytes)
}

impl<'a> N4eImage<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self> {
        let header = bytes
            .get(..2048)
            .ok_or("Incomplete ArtInChip image header.")?;
        if !header.starts_with(b"AIC.FW\0\0")
            || field(&header[8..72]) != b"d13x"
            || field(&header[72..136]) != b"HSV293V3-N4"
            || field(&header[200..264]) != b"spi-nor"
        {
            return Err("Expected a MiraBox N4E ArtInChip SPI NOR firmware image.".into());
        }
        let offset = word(header, 332) as usize;
        let length = word(header, 336) as usize;
        let entries = slice(bytes, offset, length).ok_or("Invalid firmware component table.")?;
        if offset < header.len() || !length.is_multiple_of(META_SIZE) {
            return Err("Invalid firmware component table.".into());
        }
        let info = component(bytes, entries, "image.info", "", 0, header.len())?;
        if info.data != header {
            return Err("Image information does not match the file header.".into());
        }
        let bootloader = component(bytes, entries, "image.target.spl", "spl", 0, 0x80000)?;
        if !bootloader
            .data
            .windows(PARTITION_TABLE.len())
            .any(|part| part == PARTITION_TABLE.as_bytes())
        {
            return Err("The image's bootloader uses an unsupported N4E partition layout.".into());
        }
        let os = component(bytes, entries, "image.target.os", "os", 0xc0000, 0x200000)?;
        let rodata = component(
            bytes,
            entries,
            "image.target.rodata",
            "rodata",
            0x2c0000,
            0x100000,
        )?;
        if !os.data.starts_with(b"\xd0\x0d\xfe\xed") {
            return Err("The image's OS component is missing its FIT header.".into());
        }
        let mut versions = os
            .data
            .split(|byte| !byte.is_ascii_graphic())
            .filter_map(|token| {
                let version = str::from_utf8(token).ok()?;
                is_n4e_version(version).then_some(version)
            });
        let version = versions
            .next()
            .ok_or("No N4E version found in the image's OS component.")?;
        if versions.any(|other| other != version) {
            return Err("Conflicting N4E versions found in the image's OS component.".into());
        }
        Ok(Self {
            version,
            info,
            firmware: [os, rodata],
        })
    }
}

pub fn is_n4e_version(version: &str) -> bool {
    let Some((family, suffix)) = version.rsplit_once('.').and_then(|(prefix, build)| {
        let (family, major) = prefix.rsplit_once('.')?;
        Some((family, (major, build)))
    }) else {
        return false;
    };
    matches!(family, "V3.Mbox_N4E" | "V3.Mbox_N4E_HJDF" | "V3.293N4E")
        && suffix.0.len() == 2
        && suffix.1.len() == 3
        && suffix
            .0
            .bytes()
            .chain(suffix.1.bytes())
            .all(|byte| byte.is_ascii_digit())
}

pub fn field(bytes: &[u8]) -> &[u8] {
    bytes.split(|byte| *byte == 0).next().unwrap_or_default()
}

pub fn word(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn slice(bytes: &[u8], start: usize, size: usize) -> Option<&[u8]> {
    bytes.get(start..start.checked_add(size)?)
}

fn component<'a>(
    bytes: &'a [u8],
    entries: &'a [u8],
    name: &'static str,
    partition: &'static str,
    address: u64,
    limit: usize,
) -> Result<Component<'a>> {
    let mut matches = entries
        .chunks_exact(META_SIZE)
        .filter(|meta| meta.starts_with(b"META\0\0\0\0") && field(&meta[8..72]) == name.as_bytes());
    let metadata = matches
        .next()
        .ok_or_else(|| format!("Missing {name} component."))?;
    if matches.next().is_some()
        || field(&metadata[72..136]) != partition.as_bytes()
        || metadata[72 + partition.len()..136]
            .iter()
            .any(|byte| *byte != 0)
    {
        return Err(format!("Unexpected destination or duplicate entry for {name}.").into());
    }
    let size = word(metadata, 140) as usize;
    let checksum = word(metadata, 144);
    let data = slice(bytes, word(metadata, 136) as usize, size)
        .filter(|data| !data.is_empty() && data.len() <= limit)
        .ok_or_else(|| format!("Invalid size or offset for {name}."))?;
    let mut crc = Crc::new();
    crc.update(data);
    if crc.sum() != checksum {
        return Err(format!("CRC mismatch for {name}.").into());
    }
    Ok(Component {
        name,
        partition,
        address,
        metadata,
        data,
        checksum,
    })
}

#[cfg(test)]
mod tests {
    use super::is_n4e_version;

    #[test]
    fn recognizes_n4e_vendor_versions() {
        assert!(is_n4e_version("V3.Mbox_N4E.02.014"));
        assert!(is_n4e_version("V3.Mbox_N4E_HJDF.02.018"));
        assert!(is_n4e_version("V3.293N4E.01.007"));
        assert!(!is_n4e_version("V3.Mbox_N4.02.014"));
        assert!(!is_n4e_version("V3.Mbox_N4E.02.014x"));
    }
}
