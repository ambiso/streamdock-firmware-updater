use std::{
    env,
    error::Error,
    fs::{self, File},
    io,
    path::{Path, PathBuf},
    process::{Command, ExitCode},
    time::{Duration, Instant},
};

use async_hid::{AsyncHidWrite, Device as HidDevice, HidBackend};
use futures_lite::StreamExt;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::time::sleep;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

mod catalog;

const UPDATER: &str = "33c3:6677";
const CHECK_VERSION: &str = "V3.293N4E.01.007";
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
        ("windows", "x86_64" | "aarch64") => Ok((
            "artinchip-flash-windows-x64.zip",
            "artinchip-flash-windows-x64.exe",
            "de2de5ce928a60e7e30ba9766af4a0eed6609a5b60e1b193073c591c091f2027",
        )),
        _ => {
            Err("No bundled flasher download is available for this platform. Supply an installed artinchip-flash using --flasher PATH.".into())
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

fn download(url: &str, limit: u64, redirects: u32) -> Result<Vec<u8>> {
    println!("Downloading {url}");
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .https_only(true)
        .max_redirects(redirects)
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_global(Some(Duration::from_secs(120)))
        .build()
        .into();
    let mut response = agent
        .get(url)
        .call()
        .map_err(|error| format!("{url}: {error}"))?;
    if !response.status().is_success() {
        return Err(format!("{url}: HTTP {}", response.status()).into());
    }
    Ok(response
        .body_mut()
        .with_config()
        .limit(limit)
        .read_to_vec()?)
}

fn verify_sha256(bytes: &[u8], expected: &str, source: &str) -> Result<()> {
    let sha256 = format!("{:x}", Sha256::digest(bytes));
    if !sha256.eq_ignore_ascii_case(expected) {
        return Err(format!("SHA-256 mismatch for {source}: got {sha256}").into());
    }
    Ok(())
}

struct Flasher {
    path: PathBuf,
    downloaded: bool,
}

fn prepare_flasher(dir: &Path, provided: Option<&Path>) -> Result<Flasher> {
    if let Some(path) = provided {
        return Ok(Flasher {
            path: path.canonicalize()?,
            downloaded: false,
        });
    }
    let (archive_name, member, sha256) = flasher_asset()?;
    let url = format!("{FLASHER_RELEASE}/{archive_name}");
    let bytes = download(&url, 32 * 1024 * 1024, 5)?;
    verify_sha256(&bytes, sha256, &url)?;

    let flasher = dir.join(format!("artinchip-flash{}", env::consts::EXE_SUFFIX));
    extract_flasher(&bytes, member, File::create(&flasher)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&flasher, fs::Permissions::from_mode(0o700))?;
    }
    Ok(Flasher {
        path: flasher,
        downloaded: true,
    })
}

#[cfg(unix)]
fn extract_flasher(bytes: &[u8], member: &str, mut output: File) -> Result<()> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
    for entry in archive.entries()? {
        let mut entry = entry?;
        if entry.path_bytes() == member.as_bytes() && entry.header().entry_type().is_file() {
            io::copy(&mut entry, &mut output)?;
            return Ok(());
        }
    }
    Err(format!("Flasher archive does not contain {member}.").into())
}

#[cfg(windows)]
fn extract_flasher(bytes: &[u8], member: &str, mut output: File) -> Result<()> {
    let mut archive = zip::ZipArchive::new(io::Cursor::new(bytes))?;
    io::copy(&mut archive.by_name(member)?, &mut output)?;
    Ok(())
}

fn prepare_update(
    dir: &Path,
    update: &catalog::Update,
    provided: Option<&Path>,
) -> Result<(Flasher, PathBuf)> {
    let image = dir.join("firmware.img");
    fs::write(&image, update.download()?)?;
    let flasher = prepare_flasher(dir, provided)?;
    run(
        flasher.command().args(["burn", "--plan"]).arg(&image),
        "validate burn plan",
    )?;
    Ok((flasher, image))
}

impl Flasher {
    fn command(&self) -> Command {
        // The Nix package injects a loader for the downloaded generic Linux ELF.
        if self.downloaded
            && let (Some(loader), Some(libraries)) = (
                option_env!("AKP05_FLASHER_LOADER"),
                option_env!("AKP05_FLASHER_LIBRARY_PATH"),
            )
        {
            let mut command = Command::new(loader);
            command.args(["--library-path", libraries]).arg(&self.path);
            return command;
        }
        Command::new(&self.path)
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
    fn matches(&self, id: &str) -> bool {
        format!("{}:{}", self.vid, self.pid).eq_ignore_ascii_case(id)
    }

    fn same_port(&self, other: &Self) -> bool {
        !self.path.is_empty() && self.bus == other.bus && self.path == other.path
    }
}

fn usb_devices(flasher: &Flasher) -> Result<Vec<UsbDevice>> {
    #[derive(Deserialize)]
    struct DeviceList {
        devices: Vec<UsbDevice>,
    }
    let output = flasher.command().args(["usb-list", "--json"]).output()?;
    if !output.status.success() {
        return Err(format!(
            "Cannot list USB devices: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    Ok(serde_json::from_slice::<DeviceList>(&output.stdout)?.devices)
}

fn usb_id(device: &HidDevice) -> String {
    format!("{:04x}:{:04x}", device.vendor_id, device.product_id)
}

async fn device(selector: Option<&str>) -> Result<HidDevice> {
    let backend = HidBackend::default();
    let mut devices = backend.enumerate().await?.filter(|device| {
        device.usage_page == 0xffa0
            && device.usage_id == 1
            && selector.is_none_or(|selector| usb_id(device).eq_ignore_ascii_case(selector))
    });
    match (devices.next().await, devices.next().await) {
        (Some(device), None) => Ok(device),
        _ => Err(
            "Connect exactly one accessible Stream Dock, or select its USB ID with --usb VID:PID."
                .into(),
        ),
    }
}

async fn version(device: &HidDevice) -> Result<String> {
    if cfg!(windows) {
        return Err(
            "Windows HID does not expose the firmware report. Read its version in Stream Dock."
                .into(),
        );
    }
    let mut report = [0; 64];
    report[0] = 1;
    let len = device.read_feature_report(&mut report).await?;
    Ok(std::str::from_utf8(&report[..len])?
        .trim_end_matches('\0')
        .into())
}

async fn wait_for_usb(flasher: &Flasher, port: &UsbDevice, id: &str) -> Result<UsbDevice> {
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
    Err(format!("USB device {id} did not become accessible on port {}. Check the USB connection and permissions.", port.path).into())
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

async fn flash(options: &Options<'_>) -> Result<()> {
    let dock = device(options.usb).await?;
    let id = usb_id(&dock);
    if cfg!(windows) && options.usb.is_none() {
        return Err(format!(
            "Windows needs --usb {id} and --from VERSION to confirm the connected dock."
        )
        .into());
    }
    let installed = source_version(&dock, options.from).await?;
    let Some(update) = catalog::latest(&installed, options.model)? else {
        println!("No newer compatible ArtInChip firmware listed for {installed}.");
        return Ok(());
    };
    println!(
        "Updating {} ({id}) from {installed} to {}.",
        update.model,
        update.version()
    );

    let dir = tempfile::tempdir()?;
    let (flasher, image) = prepare_update(dir.path(), &update, options.flasher)?;
    let devices = usb_devices(&flasher)?;
    let mut ports = devices.iter().filter(|usb| usb.matches(&id));
    let (Some(port), None) = (ports.next(), ports.next()) else {
        return Err(format!("Connect exactly one USB device with ID {id}.").into());
    };
    if port.path.is_empty() || devices.iter().any(|d| d.matches(UPDATER)) {
        return Err(
            "Connect the dock directly and disconnect any other ArtInChip updater devices.".into(),
        );
    }
    if device(options.usb).await?.id != dock.id
        || source_version(&dock, options.from).await? != installed
    {
        return Err("The connected dock changed during preparation. Run flash again.".into());
    }

    println!("Entering updater mode. Keep the USB cable connected.");
    enter_updater(&dock).await?;

    let updater = wait_for_usb(&flasher, port, UPDATER).await?;
    let address = format!("{}:{}", updater.bus, updater.address);
    run(
        flasher
            .command()
            .args(["burn", "--device", &address])
            .arg(&image),
        "flash firmware",
    )?;

    wait_for_usb(&flasher, port, &id).await?;
    if cfg!(windows) {
        println!(
            "The dock rebooted. Check its updated firmware version in Stream Dock (catalog version {}).",
            update.version()
        );
        return Ok(());
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if let Ok(dock) = device(Some(&id)).await
            && let Ok(reported) = version(&dock).await
        {
            if !update.matches_version(&reported) {
                return Err(
                    format!("Device rebooted with unexpected firmware {reported:?}.").into(),
                );
            }
            println!("Verified firmware {reported} after reboot.");
            return Ok(());
        }
        sleep(Duration::from_millis(200)).await;
    }
    Err("The dock rebooted, but the firmware version could not be read. Check it in Stream Dock or run version again.".into())
}

#[derive(Default)]
struct Options<'a> {
    from: Option<&'a str>,
    model: Option<&'a str>,
    usb: Option<&'a str>,
    flasher: Option<&'a Path>,
}

impl<'a> Options<'a> {
    fn parse(args: &'a [String]) -> Result<Self> {
        let mut options = Self::default();
        let (pairs, remainder) = args.as_chunks::<2>();
        for [flag, value] in pairs {
            let (flag, value) = (flag.as_str(), value.as_str());
            match flag {
                "--from" if options.from.is_none() => options.from = Some(value),
                "--model" if options.model.is_none() => options.model = Some(value),
                "--usb" if options.usb.is_none() => options.usb = Some(value),
                "--flasher" if options.flasher.is_none() => {
                    options.flasher = Some(Path::new(value))
                }
                _ => return Err(format!("Unknown or repeated option {flag:?}.").into()),
            }
        }
        if !remainder.is_empty() {
            return Err("Each option needs a value.".into());
        }
        Ok(options)
    }
}

async fn execute() -> Result<()> {
    let args: Vec<_> = env::args().skip(1).collect();
    let command = args.first().map_or("--help", String::as_str);
    let options = Options::parse(args.get(1..).unwrap_or(&[]))?;
    match command {
        "check" | "latest" => {
            let installed = options.from.unwrap_or(CHECK_VERSION);
            let Some(update) = catalog::latest(installed, options.model)? else {
                println!("No newer compatible ArtInChip firmware listed for {installed}.");
                return Ok(());
            };
            println!("{}: {} → {}", update.model, installed, update.version());
            if command == "check" {
                let dir = tempfile::tempdir()?;
                prepare_update(dir.path(), &update, options.flasher)?;
                println!("Download verification and the offline burn plan passed.");
            }
        }
        "version" => {
            println!("{}", version(&device(options.usb).await?).await?);
        }
        "flash" => flash(&options).await?,
        "enter-updater" => {
            enter_updater(&device(options.usb).await?).await?;
            println!("Requested ArtInChip USB mode (33c3:6677). No firmware was written.");
        }
        "--help" | "-h" => println!(
            "Usage: akp05-updater COMMAND [--from VERSION] [--model NAME] [--usb VID:PID] [--flasher PATH]\n\n  version        Read firmware from the connected dock\n  latest         Find the latest compatible catalog update without accessing USB\n  check          Verify the downloads and the burn plan without accessing USB\n  flash          Install the latest compatible firmware on the connected dock\n  enter-updater  Enter USB updater mode without writing firmware\n\nOn Windows, flash requires --from and --usb. If a family is shared across models, supply --model."
        ),
        _ => return Err("Use akp05-updater --help for usage.".into()),
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
