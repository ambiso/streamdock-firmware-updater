use std::{
    env,
    error::Error,
    fs::{self, File},
    io,
    path::{Path, PathBuf},
    process::{Command, ExitCode},
    time::{Duration, Instant},
};

use async_hid::AsyncHidWrite;
use mirajazz::{
    device::{DeviceQuery, list_devices},
    types::HidDevice,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::time::sleep;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

const N4E: (&str, &str) = ("6603", "1007");
const UPDATER: (&str, &str) = ("33c3", "6677");
const NEW_VERSION: &str = "V3.Mbox_N4E_HJDF.02.018";

const FIRMWARE_URL: &str =
    "https://cdn1.key123.vip/StreamDock/firmware/download/V3.Mbox_N4E_HJDF.02.018.img";
const FIRMWARE_SHA256: &str = "a24c149bb44d9ee7db042bb521a9f1a53f9cb8750fc76fcc9ed0e6a83882ffb1";
const FLASHER_RELEASE: &str = "https://github.com/boa-z/artinchip-flash/releases/download/v0.2.0";

fn flasher_asset() -> Result<(&'static str, &'static str, &'static str)> {
    match (env::consts::OS, env::consts::ARCH) {
        ("linux", "x86_64") => Ok((
            "artinchip-flash-linux-x64.tar.gz",
            "./artinchip-flash-linux-x64",
            "6f140836d40b4bb64fe952403eaf60bcb65e4caf32a2071477e98bba41e7c201",
        )),
        ("macos", "aarch64") => Ok((
            "artinchip-flash-macos-arm64.tar.gz",
            "./artinchip-flash-macos-arm64",
            "20f6f841dc91ba9dd4fb811ed3e08865431aac951d391905d379e047d2e0dc11",
        )),
        ("windows", "x86_64") => Ok((
            "artinchip-flash-windows-x64.zip",
            "artinchip-flash-windows-x64.exe",
            "de2de5ce928a60e7e30ba9766af4a0eed6609a5b60e1b193073c591c091f2027",
        )),
        _ => {
            Err("Supported platforms are x86-64 Linux and Windows, and Apple Silicon macOS.".into())
        }
    }
}

fn run(command: &mut Command, action: &str) -> Result<()> {
    let status = command
        .status()
        .map_err(|error| format!("{action}: {error}"))?;
    if !status.success() {
        return Err(format!("{action} failed ({status})").into());
    }
    Ok(())
}

fn download(url: &str, destination: &Path, expected_sha256: &str) -> Result<()> {
    println!("Downloading {url}");
    run(
        Command::new("curl")
            .args([
                "--fail",
                "--silent",
                "--show-error",
                "--location",
                "--proto",
                "=https",
                "--proto-redir",
                "=https",
                "--retry",
                "3",
                "--connect-timeout",
                "10",
                "--max-time",
                "120",
                "--output",
            ])
            .arg(destination)
            .arg(url),
        "download",
    )?;
    let sha256 = format!("{:x}", Sha256::digest(fs::read(destination)?));
    if sha256 != expected_sha256 {
        return Err(format!("SHA-256 mismatch for {url}: got {sha256}").into());
    }
    Ok(())
}

fn prepare_flasher(dir: &Path) -> Result<(PathBuf, PathBuf)> {
    let image = dir.join("n4e-02.018.img");
    let (archive_name, member, sha256) = flasher_asset()?;
    let archive = dir.join(archive_name);
    download(FIRMWARE_URL, &image, FIRMWARE_SHA256)?;
    download(
        &format!("{FLASHER_RELEASE}/{archive_name}"),
        &archive,
        sha256,
    )?;

    // Extract only the CLI to a file we created ourselves.
    let flasher = dir.join(format!("artinchip-flash{}", env::consts::EXE_SUFFIX));
    run(
        Command::new("tar")
            .arg("-xOf")
            .arg(archive)
            .arg(member)
            .stdout(File::create(&flasher)?),
        "extract flasher CLI",
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&flasher, fs::Permissions::from_mode(0o700))?;
    }
    run(
        flasher_command(&flasher)
            .args(["burn", "--plan"])
            .arg(&image),
        "validate burn plan",
    )?;
    Ok((flasher, image))
}

fn flasher_command(flasher: &Path) -> Command {
    // The Nix package injects the loader and library path for the upstream ELF.
    match (
        option_env!("AKP05_FLASHER_LOADER"),
        option_env!("AKP05_FLASHER_LIBRARY_PATH"),
    ) {
        (Some(loader), Some(libraries)) => {
            let mut command = Command::new(loader);
            command.args(["--library-path", libraries]).arg(flasher);
            command
        }
        _ => Command::new(flasher),
    }
}

#[derive(Deserialize)]
struct UsbDevice {
    bus: u8,
    address: u8,
    path: String,
    vid: String,
    pid: String,
}

impl UsbDevice {
    fn matches(&self, id: (&str, &str)) -> bool {
        (self.vid.as_str(), self.pid.as_str()) == id
    }

    fn same_port(&self, other: &Self) -> bool {
        !self.path.is_empty() && self.bus == other.bus && self.path == other.path
    }
}

fn usb_devices(flasher: &Path) -> Result<Vec<UsbDevice>> {
    #[derive(Deserialize)]
    struct DeviceList {
        devices: Vec<UsbDevice>,
    }
    let output = flasher_command(flasher)
        .args(["usb-list", "--json"])
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "Cannot list USB devices: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    Ok(serde_json::from_slice::<DeviceList>(&output.stdout)?.devices)
}

async fn n4e() -> Result<HidDevice> {
    let mut devices = list_devices(&[DeviceQuery::new(0xffa0, 1, 0x6603, 0x1007)])
        .await?
        .into_iter();
    match (devices.next(), devices.next()) {
        (Some(device), None) => Ok(device),
        _ => Err("Expected one accessible MiraBox N4E HID interface (6603:1007).".into()),
    }
}

async fn version(device: &HidDevice) -> Result<String> {
    if cfg!(windows) {
        return Err("Windows HID does not expose this device's firmware report. Read its version in Stream Dock.".into());
    }
    let mut report = [0; 64];
    report[0] = 1;
    let len = device.read_feature_report(&mut report).await?;
    Ok(std::str::from_utf8(&report[..len])?
        .trim_end_matches('\0')
        .into())
}

fn is_supported_source(version: &str) -> bool {
    // Match the vendor catalog's V3.293N4E variant and its minimum version.
    let Some((major, build)) = version
        .strip_prefix("V3.293N4E.")
        .and_then(|v| v.split_once('.'))
    else {
        return false;
    };
    if major.len() != 2 || build.len() != 3 {
        return false;
    }
    match (major.parse::<u16>(), build.parse::<u16>()) {
        (Ok(major), Ok(build)) => ((0, 4)..(2, 18)).contains(&(major, build)),
        _ => false,
    }
}

async fn wait_for_usb(flasher: &Path, port: &UsbDevice, id: (&str, &str)) -> Result<UsbDevice> {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        let devices = usb_devices(flasher)?;
        if let Some(device) = devices
            .into_iter()
            .find(|d| d.matches(id) && d.same_port(port))
        {
            if cfg!(target_os = "linux") && id == UPDATER {
                // The kernel can report the device before udev or Nook grants access.
                let node = format!("/dev/bus/usb/{:03}/{:03}", device.bus, device.address);
                if fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(node)
                    .is_err()
                {
                    sleep(Duration::from_millis(300)).await;
                    continue;
                }
            }
            return Ok(device);
        }
        sleep(Duration::from_millis(300)).await;
    }
    Err(format!("USB device {}:{} did not become accessible on port {}. Check the USB connection and permissions.", id.0, id.1, port.path).into())
}

async fn enter_updater(device: &HidDevice) -> Result<()> {
    let (_, mut writer) = device.open().await?;
    let command = b"\0CRT\0\0APPNEW";
    let mut report = [0; 1025];
    report[..command.len()].copy_from_slice(command);
    writer.write_output_report(&report).await?;
    Ok(())
}

async fn source_version(device: &HidDevice, supplied: Option<&str>) -> Result<String> {
    match (version(device).await, supplied) {
        (Ok(reported), Some(supplied)) if reported != supplied => {
            Err(format!("The device reports {reported}, but --from specified {supplied}.").into())
        }
        (Ok(reported), _) => Ok(reported),
        (Err(_), Some(supplied)) if !cfg!(target_os = "linux") => Ok(supplied.to_owned()),
        (Err(error), _) => Err(error),
    }
}

async fn flash(from: Option<&str>) -> Result<()> {
    let device = n4e().await?;
    let installed = source_version(&device, from).await?;
    if installed == NEW_VERSION || installed == "V3.293N4E.02.018" {
        println!("Already running {installed}.");
        return Ok(());
    }
    if !is_supported_source(&installed) {
        return Err(format!(
            "Unsupported firmware {installed:?}. Expected V3.293N4E.00.004 through 02.017."
        )
        .into());
    }
    println!("Updating {installed} to {NEW_VERSION}.");

    let dir = tempfile::tempdir()?;
    let (flasher, image) = prepare_flasher(dir.path())?;
    let devices = usb_devices(&flasher)?;
    let mut n4es = devices.iter().filter(|d| d.matches(N4E));
    let (Some(port), None) = (n4es.next(), n4es.next()) else {
        return Err("Connect exactly one MiraBox N4E (6603:1007).".into());
    };
    if port.path.is_empty() || devices.iter().any(|d| d.matches(UPDATER)) {
        return Err(
            "Connect the N4E directly and disconnect any other ArtInChip updater devices.".into(),
        );
    }
    if n4e().await?.id != device.id || source_version(&device, from).await? != installed {
        return Err("The connected N4E changed during preparation. Run flash again.".into());
    }

    println!("Entering updater mode. Keep the USB cable connected.");
    enter_updater(&device).await?;

    let updater = wait_for_usb(&flasher, port, UPDATER).await?;
    let address = format!("{}:{}", updater.bus, updater.address);
    run(
        flasher_command(&flasher)
            .args(["burn", "--device", &address])
            .arg(&image),
        "flash firmware",
    )?;

    wait_for_usb(&flasher, port, N4E).await?;
    if cfg!(windows) {
        println!("The N4E rebooted. Check for {NEW_VERSION} in Stream Dock.");
        return Ok(());
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if let Ok(device) = n4e().await
            && let Ok(reported) = version(&device).await
        {
            if reported != NEW_VERSION {
                return Err(
                    format!("Device rebooted with unexpected firmware {reported:?}.").into(),
                );
            }
            println!("Verified firmware {reported} after reboot.");
            return Ok(());
        }
        sleep(Duration::from_millis(200)).await;
    }
    Err("The N4E rebooted, but the firmware version could not be read. Check it in Stream Dock or run version again.".into())
}

async fn execute() -> Result<()> {
    flasher_asset()?;
    let args: Vec<_> = env::args().skip(1).collect();
    let args: Vec<_> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["check"] => {
            let dir = tempfile::tempdir()?;
            prepare_flasher(dir.path())?;
            println!("Both SHA-256 checks and the offline burn plan passed.");
        }
        ["version"] => {
            println!("{}", version(&n4e().await?).await?);
        }
        ["flash"] => flash(None).await?,
        ["flash", "--from", version] => flash(Some(version)).await?,
        ["enter-updater"] => {
            enter_updater(&n4e().await?).await?;
            println!("Requested ArtInChip USB mode (33c3:6677). No firmware was written.");
        }
        [] | ["--help" | "-h"] => println!(
            "Usage: akp05-updater <version|check|flash|enter-updater>\n\n  version                Read the connected N4E's firmware version\n  check                  Verify the downloads and burn plan without accessing USB\n  flash                  Install firmware 02.018 on the connected N4E\n  flash --from VERSION   Confirm the version manually when Windows HID cannot read it\n  enter-updater          Enter USB updater mode without writing firmware"
        ),
        _ => return Err(io::Error::other("Use akp05-updater --help for usage.").into()),
    }
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match execute().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("akp05-updater: {error}");
            ExitCode::FAILURE
        }
    }
}
