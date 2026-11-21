use std::io::{self, IsTerminal, Write};

use async_hid::{Device as HidDevice, HidBackend};
use futures_lite::StreamExt;
use sha2::{Digest, Sha256};

use crate::{DockArgs, Result};

pub struct Dock {
    pub hid: HidDevice,
    pub label: String,
    pub key: String,
    pub usb_address: Option<(u8, u8)>,
}

impl Dock {
    pub fn usb_id(&self) -> String {
        format!("{:04x}:{:04x}", self.hid.vendor_id, self.hid.product_id)
    }
}

pub async fn list() -> Result<Vec<Dock>> {
    let backend = HidBackend::default();
    let mut docks: Vec<_> = backend
        .enumerate()
        .await?
        .filter(|hid| hid.usage_page == 0xffa0 && hid.usage_id == 1)
        .map(|hid| {
            let usb_address = usb_address(&hid);
            let name = model_name(hid.vendor_id, hid.product_id).unwrap_or_else(|| {
                let name = hid.name.trim();
                if name.is_empty() { "Stream Dock" } else { name }
            });
            let label = match &hid.serial_number {
                Some(serial) if !serial.is_empty() => format!("{name} · serial {serial}"),
                _ => name.to_owned(),
            }
            .chars()
            .filter(|ch| !ch.is_control())
            .collect();
            // Pass the selected HID instance to the worker without using a list index
            // that could point to a different device after a USB connection changes.
            let identity = format!(
                "{:?}:{:04x}:{:04x}:{:?}:{usb_address:?}",
                hid.id, hid.vendor_id, hid.product_id, hid.serial_number
            );
            let key = format!("{:x}", Sha256::digest(identity));
            Dock {
                hid,
                label,
                key,
                usb_address,
            }
        })
        .collect()
        .await;
    docks.sort_unstable_by(|a, b| (&a.label, &a.key).cmp(&(&b.label, &b.key)));
    docks.dedup_by(|a, b| a.key == b.key);
    Ok(docks)
}

pub async fn select(target: &DockArgs) -> Result<Dock> {
    if let Some(key) = &target.device_key {
        return select_by_key(key).await;
    }
    let mut docks = list().await?;
    if let Some(usb) = &target.usb {
        docks.retain(|dock| dock.usb_id().eq_ignore_ascii_case(usb));
    }
    match docks.len() {
        0 => Err("No accessible Stream Dock found. Connect your dock and check its USB permissions.".into()),
        1 => Ok(docks.remove(0)),
        _ if io::stdin().is_terminal() && io::stdout().is_terminal() => {
            let dock = choose(docks)?;
            select_by_key(&dock.key).await
        }
        _ => Err("Several Stream Docks are connected. Open the interactive updater to choose one, or disconnect the other docks.".into()),
    }
}

pub async fn select_by_key(key: &str) -> Result<Dock> {
    list()
        .await?
        .into_iter()
        .find(|dock| dock.key == key)
        .ok_or_else(|| {
            "The selected dock is no longer available. Rescan and choose it again.".into()
        })
}

fn choose(mut docks: Vec<Dock>) -> Result<Dock> {
    for (index, dock) in docks.iter().enumerate() {
        println!("  {}. {}", index + 1, dock.label);
    }
    loop {
        print!("Choose a dock by number, or press Enter to cancel. ");
        io::stdout().flush()?;
        let mut choice = String::new();
        io::stdin().read_line(&mut choice)?;
        let choice = choice.trim();
        if choice.is_empty() {
            return Err("Device selection cancelled.".into());
        }
        if let Ok(index) = choice.parse::<usize>()
            && (1..=docks.len()).contains(&index)
        {
            return Ok(docks.remove(index - 1));
        }
    }
}

#[cfg(target_os = "linux")]
fn usb_address(hid: &HidDevice) -> Option<(u8, u8)> {
    use std::{fs, path::Path};

    let async_hid::DeviceId::DevPath(path) = &hid.id else {
        return None;
    };
    let sysfs_path = Path::new("/sys/class/hidraw")
        .join(path.file_name()?)
        .join("device");
    let device = fs::canonicalize(sysfs_path).ok()?;
    device.ancestors().find_map(|parent| {
        let hex =
            |name| u16::from_str_radix(fs::read_to_string(parent.join(name)).ok()?.trim(), 16).ok();
        if (hex("idVendor")?, hex("idProduct")?) != (hid.vendor_id, hid.product_id) {
            return None;
        }
        let bus = fs::read_to_string(parent.join("busnum"))
            .ok()?
            .trim()
            .parse()
            .ok()?;
        let address = fs::read_to_string(parent.join("devnum"))
            .ok()?
            .trim()
            .parse()
            .ok()?;
        Some((bus, address))
    })
}

#[cfg(not(target_os = "linux"))]
fn usb_address(_: &HidDevice) -> Option<(u8, u8)> {
    None
}

#[cfg(target_os = "linux")]
pub async fn open_hid_updater(address: (u8, u8)) -> Result<Option<crate::hid_updater::HidUpdater>> {
    let backend = HidBackend::default();
    let mut candidates = backend.enumerate().await?.filter(|device| {
        (device.vendor_id, device.product_id) == (0x33c3, 0x8899)
            && usb_address(device) == Some(address)
    });
    let Some(device) = candidates.next().await else {
        return Ok(None);
    };
    if candidates.next().await.is_some() {
        return Err("Several HID updater interfaces were found on the selected port.".into());
    }
    Ok(device
        .open()
        .await
        .ok()
        .map(crate::hid_updater::HidUpdater::new))
}

// Display names from the OpenDeck AKP03, AKP05, AKP153, 293 V3, M18 and XL
// plugins and unambiguous entries in StreamDock.exe's VpidToUUID datasheet.
// Firmware compatibility is resolved through the MiraBox catalog.
fn model_name(vid: u16, pid: u16) -> Option<&'static str> {
    Some(match (vid, pid) {
        (0x0200, 0x2000) => "Redragon Skyrider SS-551",
        (0x0200, 0x3001) => "Redragon SS552",
        (0x0300, 0x1001) => "AJAZZ AKP03",
        (0x0300, 0x1002) => "AJAZZ AKP03E",
        (0x0300, 0x1003) => "AJAZZ AKP03R",
        (0x0300, 0x1010 | 0x3010) => "AJAZZ AKP153E",
        (0x0300, 0x1020 | 0x3011) => "AJAZZ AKP153R",
        (0x0300, 0x3002) => "AJAZZ AKP03E (rev. 2)",
        (0x0300, 0x3003) => "AJAZZ AKP03R (rev. 2)",
        (0x0300, 0x3004) => "AJAZZ AKP05E",
        (0x0300, 0x3005) => "AJAZZ AKP05R",
        (0x0300, 0x3006) => "AJAZZ AKP05",
        (0x0300, 0x3007) => "AJAZZ N1E",
        (0x0300, 0x3008) => "AJAZZ N1R",
        (0x0300, 0x3009) => "AJAZZ N1",
        (0x0300, 0x3013) => "AJAZZ AKP05E Pro",
        (0x0300, 0x3014) => "AJAZZ AKP05CN Pro",
        (0x0300, 0x6674) => "AJAZZ AKP153",
        (0x0400, 0x1000) => "MonstarGear TS115",
        (0x0500, 0x1001) => "TMICE ESC",
        (0x0600, 0x1000) => "Womier D15",
        (0x0a00, 0x1001) => "Rise Mode RMV01",
        (0x0b00, 0x1000 | 0x1005) => "Mars Gaming MSD-ONE",
        (0x0b00, 0x1001) => "Mars Gaming MSD-TWO",
        (0x0b00, 0x1003) => "Mars Gaming MSD-Pro",
        (0x0c00, 0x1000) => "MadDog GK150K",
        (0x1500, 0x3001) => "Soomfon Stream Controller SE",
        (0x1500, 0x3002) => "Soomfon CN003",
        (0x1500, 0x3003) => "Soomfon Stream Controller",
        (0x1500, 0x3005) => "Streonor S15",
        (0x1500, 0x3007) => "Streonor S10 Pro",
        (0x1500, 0x3008) => "Streonor S32",
        (0x3142, 0x0007) => "Stream Dock D6",
        (0x3142, 0x0060) => "Stream Dock D6 Pro",
        (0x3142, 0x006b) => "Stream Dock D6D",
        (0x3142, 0x00b8) => "Stream Dock D8",
        (0x5548, 0x1000) => "VSDInside M18",
        (0x5548, 0x1001) => "TreasLin N3",
        (0x5548, 0x1002) => "VSDInside N1",
        (0x5548, 0x1004) => "VSDInside N4",
        (0x5548, 0x1008) => "MiraBox N4 Pro",
        (0x5548, 0x1012) => "Stream Dock M18 V3",
        (0x5548, 0x1021) => "MiraBox N4 Pro E",
        (0x5548, 0x1023) => "VSDInside N4 Pro",
        (0x5548, 0x1028) => "MiraBox Stream Dock XL",
        (0x5548, 0x1031) => "MiraBox Stream Dock XL E",
        (0x5548, 0x1032) => "Stream Dock M3 E",
        (0x5548, 0x1034) => "VSDInside Stream Dock XL",
        (0x5548, 0x1036) => "Stream Dock XL Mini",
        (0x5548, 0x1037 | 0x1040) => "Stream Dock XL Mini W",
        (0x5548, 0x1038) => "VSDInside M3",
        (0x5548, 0x6670) => "MiraBox HSV293S",
        (0x6602, 0x1000 | 0x1002) | (0x6603, 0x1002 | 0x1003) => "MiraBox N3",
        (0x6602, 0x1001) => "MiraBox N4",
        (0x6603, 0x1000) => "MiraBox N1E",
        (0x6603, 0x1005 | 0x1014) => "MiraBox HSV293S V3",
        (0x6603, 0x1006) => "MiraBox 293 V3",
        (0x6603, 0x1007) => "MiraBox N4E",
        (0x6603, 0x1009) => "MiraBox M18",
        (0x6603, 0x1011) => "MiraBox N1",
        (0x6603, 0x1012) => "MiraBox M18 E",
        _ => return None,
    })
}
