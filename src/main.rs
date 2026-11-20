#![cfg_attr(windows, windows_subsystem = "console")]

use std::{
    env,
    error::Error,
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Command, ExitCode},
    time::{Duration, Instant},
};

use async_hid::{AsyncHidWrite, Device as HidDevice};
use clap::{Args, CommandFactory, Parser, Subcommand};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::time::{sleep, timeout};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

mod catalog;
mod docks;
#[cfg(target_os = "linux")]
mod hid_updater;
#[cfg(target_os = "linux")]
mod n4e_image;
mod tui;

const UPDATER: &str = "33c3:6677";
const HID_UPDATER: &str = "33c3:8899";
const CHECK_VERSION: &str = "V3.293N4E.01.007";
const FLASHER_RELEASE: &str = "https://github.com/boa-z/artinchip-flash/releases/download/v0.2.0";
const LICENSE: &str = concat!(
    "Stream Dock Firmware Updater · EUPL-1.2\n",
    "Source code ",
    env!("CARGO_PKG_REPOSITORY"),
    "\n\n",
    include_str!("../LICENSE"),
);

/// Firmware updater for compatible MiraBox Stream Dock devices.
#[derive(Parser)]
#[command(
    name = "streamdock-fw",
    version,
    after_help = "Run without arguments in a terminal to open the interactive interface."
)]
struct Cli {
    #[command(subcommand)]
    action: Option<Action>,
}

#[derive(Subcommand)]
enum Action {
    /// List connected Stream Docks.
    Devices,
    /// Read firmware from the connected dock.
    Version(DockArgs),
    /// Find the latest compatible catalog update without accessing USB.
    Latest(CatalogArgs),
    /// Verify the downloads and the burn plan without accessing USB.
    Check {
        #[command(flatten)]
        catalog: CatalogArgs,
        /// Path to an installed artinchip-flash executable.
        #[arg(long, value_name = "PATH")]
        flasher: Option<PathBuf>,
    },
    /// Install compatible firmware from the catalog or a local N4E image.
    #[command(after_help = "On Windows, flash requires --from.")]
    Flash(FlashArgs),
    /// Enter USB updater mode without writing firmware.
    EnterUpdater(DockArgs),
    /// Open the interactive terminal interface.
    Tui {
        /// Path to an installed artinchip-flash executable.
        #[arg(long, value_name = "PATH")]
        flasher: Option<PathBuf>,
    },
    /// Print the embedded license and source repository.
    License,
}

#[derive(Args)]
struct DockArgs {
    #[arg(long, hide = true, conflicts_with = "device_key")]
    usb: Option<String>,
    #[arg(long, hide = true)]
    device_key: Option<String>,
}

#[derive(Args)]
struct CatalogArgs {
    /// Installed firmware version to look up in the catalogs.
    #[arg(long, value_name = "VERSION", default_value = CHECK_VERSION)]
    from: String,
    /// Exact catalog model name, required when models share a firmware family.
    #[arg(long, value_name = "NAME")]
    model: Option<String>,
}

#[derive(Args)]
struct FlashArgs {
    /// Installed firmware version shown by the dock.
    #[arg(long, value_name = "VERSION", required = cfg!(windows))]
    from: Option<String>,
    /// Exact catalog model name, required when models share a firmware family.
    #[arg(long, value_name = "NAME")]
    model: Option<String>,
    /// Local N4E firmware image to install on Linux, including older releases.
    #[arg(long, value_name = "IMAGE", conflicts_with = "model")]
    file: Option<PathBuf>,
    /// Validate the local file without accessing USB.
    #[arg(long, requires = "file")]
    dry_run: bool,
    /// Confirm installation of the local image for non-interactive use.
    #[arg(long, requires = "file", conflicts_with = "dry_run")]
    yes: bool,
    #[command(flatten)]
    target: DockArgs,
    /// Path to an installed artinchip-flash executable.
    #[arg(long, value_name = "PATH")]
    flasher: Option<PathBuf>,
}

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InstallStep {
    SwitchMode,
    Flash,
    Restart,
    Verify,
}

enum Progress {
    Download {
        name: String,
        bytes: u64,
        total: Option<u64>,
    },
    Validating,
    Install(InstallStep),
}

struct Feedback<'a>(Option<&'a dyn Fn(Progress) -> bool>);

impl Feedback<'_> {
    fn emit(&self, event: Progress) -> bool {
        if let Some(emit) = self.0 {
            return emit(event);
        }
        match event {
            Progress::Download { name, bytes: 0, .. } => println!("Downloading {name}"),
            Progress::Install(InstallStep::SwitchMode) => {
                println!("Entering updater mode. Keep the USB cable connected.");
            }
            _ => {}
        }
        true
    }
}

fn run(command: &mut Command, action: &str, feedback: &Feedback<'_>) -> Result<()> {
    if feedback.0.is_some() {
        let output = command
            .output()
            .map_err(|error| format!("{action}: {error}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let detail = stderr
                .lines()
                .rev()
                .chain(stdout.lines().rev())
                .find(|line| !line.trim().is_empty());
            let error = format!("{action} failed ({})", output.status);
            return Err(detail
                .map_or_else(
                    || error.clone(),
                    |detail| format!("{error}\n{}", detail.trim()),
                )
                .into());
        }
        return Ok(());
    }
    let status = command
        .status()
        .map_err(|error| format!("{action}: {error}"))?;
    if !status.success() {
        return Err(format!("{action} failed ({status})").into());
    }
    Ok(())
}

fn download(url: &str, limit: u64, redirects: u32, feedback: &Feedback<'_>) -> Result<Vec<u8>> {
    let name = url.to_owned();
    if !feedback.emit(Progress::Download {
        name: name.clone(),
        bytes: 0,
        total: None,
    }) {
        return Err("Download cancelled.".into());
    }
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
    let body = response.body_mut();
    let total = body.content_length();
    let mut reader = body.with_config().limit(limit).reader();
    let mut data = Vec::new();
    let mut chunk = [0; 64 * 1024];
    loop {
        let size = reader.read(&mut chunk)?;
        if size == 0 {
            return Ok(data);
        }
        data.extend_from_slice(&chunk[..size]);
        if feedback.0.is_some()
            && !feedback.emit(Progress::Download {
                name: name.clone(),
                bytes: data.len() as u64,
                total,
            })
        {
            return Err("Download cancelled.".into());
        }
    }
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

fn prepare_flasher(
    dir: &Path,
    provided: Option<&Path>,
    feedback: &Feedback<'_>,
) -> Result<Flasher> {
    if let Some(path) = provided {
        return Ok(Flasher {
            path: path.canonicalize()?,
            downloaded: false,
        });
    }
    let (archive_name, member, sha256) = flasher_asset()?;
    let url = format!("{FLASHER_RELEASE}/{archive_name}");
    let bytes = download(&url, 32 * 1024 * 1024, 5, feedback)?;
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
    feedback: &Feedback<'_>,
) -> Result<(Flasher, PathBuf)> {
    let image = dir.join("firmware.img");
    fs::write(&image, update.download(feedback)?)?;
    let flasher = prepare_flasher(dir, provided, feedback)?;
    feedback.emit(Progress::Validating);
    run(
        flasher.command().args(["burn", "--plan"]).arg(&image),
        "validate burn plan",
        feedback,
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

#[derive(Clone, Deserialize)]
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

fn usb_port<'a>(
    devices: &'a [UsbDevice],
    id: &str,
    address: Option<(u8, u8)>,
) -> Option<&'a UsbDevice> {
    let mut matching = devices.iter().filter(|usb| {
        usb.matches(id) && address.is_none_or(|address| address == (usb.bus, usb.address))
    });
    match (matching.next(), matching.next()) {
        (Some(port), None) => Some(port),
        _ => None,
    }
}

async fn version(device: &HidDevice) -> Result<String> {
    // Windows reports a zero feature-report buffer length for these docks.
    // See https://github.com/4ndv/mirajazz/issues/10.
    if cfg!(windows) {
        return Err(
            "Read the installed firmware version in Stream Dock → Settings → Device on Windows."
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
    let deadline = Instant::now() + Duration::from_secs(90);
    while Instant::now() < deadline {
        let devices = usb_devices(flasher)?;
        if let Some(device) = devices
            .into_iter()
            .find(|d| d.matches(id) && d.same_port(port))
        {
            return Ok(device);
        }
        sleep(Duration::from_millis(300)).await;
    }
    Err(format!("USB device {id} did not become accessible on port {}. Check the USB connection and permissions.", port.path).into())
}

enum Updater {
    Bulk(UsbDevice),
    #[cfg(target_os = "linux")]
    Hid(hid_updater::HidUpdater),
}

async fn wait_for_updater(flasher: &Flasher, port: &UsbDevice, allow_hid: bool) -> Result<Updater> {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut hid_seen = false;
    while Instant::now() < deadline {
        if let Some(updater) = usb_devices(flasher)?.into_iter().find(|device| {
            device.same_port(port) && (device.matches(UPDATER) || device.matches(HID_UPDATER))
        }) {
            if updater.matches(UPDATER) {
                let node = format!("/dev/bus/usb/{:03}/{:03}", updater.bus, updater.address);
                if !cfg!(target_os = "linux")
                    || fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(node)
                        .is_ok()
                {
                    return Ok(Updater::Bulk(updater));
                }
            } else {
                hid_seen = true;
            }
        }
        #[cfg(target_os = "linux")]
        if let Some(address) = docks::hid_updater_address(port) {
            hid_seen = true;
            if allow_hid && let Some(updater) = docks::open_hid_updater(address).await? {
                return Ok(Updater::Hid(updater));
            }
        }
        if hid_seen && (!allow_hid || !cfg!(target_os = "linux")) {
            return Err("This dock entered HID updater mode. HID flashing is currently available for MiraBox N4E images on Linux. Reconnect the dock to return to normal mode.".into());
        }
        sleep(Duration::from_millis(300)).await;
    }
    let id = if hid_seen { HID_UPDATER } else { UPDATER };
    Err(format!("USB updater {id} did not become accessible on port {}. Check the USB connection and permissions, then reconnect the dock before retrying.", port.path).into())
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
    let reported = timeout(Duration::from_secs(5), version(device))
        .await
        .unwrap_or_else(|_| Err("Firmware version read timed out.".into()));
    match (reported, supplied) {
        (Ok(reported), Some(supplied)) if reported != supplied => {
            Err(format!("The device reports {reported}, but --from specified {supplied}.").into())
        }
        (Ok(reported), _) => Ok(reported),
        (Err(_), Some(supplied)) if !cfg!(target_os = "linux") => Ok(supplied.to_owned()),
        (Err(error), _) => Err(error),
    }
}

struct PreparedUpdate {
    flash: PreparedFlash,
    update: catalog::Update,
}

struct PreparedFlash {
    dock: docks::Dock,
    installed: String,
    _directory: tempfile::TempDir,
    flasher: Flasher,
    image: PathBuf,
    port: UsbDevice,
}

enum InstallOutcome {
    Verified(String),
    Rebooted,
}

async fn locate_dock(dock: &docks::Dock, flasher: &Flasher) -> Result<(docks::Dock, UsbDevice)> {
    let id = dock.usb_id();
    let mut connected = docks::list().await?;
    let Some(index) = connected.iter().position(|current| current.key == dock.key) else {
        return Err("The selected dock changed. Rescan and select it again.".into());
    };
    if dock.usb_address.is_none()
        && connected
            .iter()
            .filter(|current| current.usb_id() == id)
            .count()
            > 1
    {
        return Err("Disconnect the other dock of the same model and retry so the selected dock’s USB port can be identified.".into());
    }
    let devices = usb_devices(flasher)?;
    let Some(port) = usb_port(&devices, &id, dock.usb_address) else {
        return Err("Cannot identify the selected dock’s USB port. Connect it directly by USB and disconnect other docks of the same model.".into());
    };
    if port.path.is_empty()
        || devices
            .iter()
            .any(|d| d.matches(UPDATER) || d.matches(HID_UPDATER))
    {
        return Err(
            "Connect the dock directly and disconnect any other ArtInChip updater devices.".into(),
        );
    }
    Ok((connected.swap_remove(index), port.clone()))
}

async fn prepare_install(
    dock: docks::Dock,
    installed: String,
    update: catalog::Update,
    provided: Option<&Path>,
    feedback: &Feedback<'_>,
) -> Result<PreparedUpdate> {
    let directory = tempfile::tempdir()?;
    let (flasher, image) = prepare_update(directory.path(), &update, provided, feedback)?;
    let (dock, port) = locate_dock(&dock, &flasher).await?;
    let flash = PreparedFlash {
        dock,
        installed,
        _directory: directory,
        flasher,
        image,
        port,
    };
    Ok(PreparedUpdate { flash, update })
}

async fn install(prepared: &PreparedUpdate, feedback: &Feedback<'_>) -> Result<InstallOutcome> {
    install_image(
        &prepared.flash,
        |reported| prepared.update.matches_version(reported),
        false,
        feedback,
    )
    .await
}

async fn install_image(
    prepared: &PreparedFlash,
    matches_version: impl Fn(&str) -> bool,
    local: bool,
    feedback: &Feedback<'_>,
) -> Result<InstallOutcome> {
    let PreparedFlash {
        dock,
        installed,
        flasher,
        image,
        port,
        ..
    } = prepared;
    let id = dock.usb_id();
    #[cfg(not(target_os = "linux"))]
    let allow_hid = false;
    #[cfg(target_os = "linux")]
    let allow_hid = id == n4e_image::USB_ID && n4e_image::is_n4e_version(installed);
    #[cfg(target_os = "linux")]
    let _port_lock = allow_hid.then(|| lock_port(port)).transpose()?;

    let (current, current_port) = locate_dock(dock, flasher).await?;
    if !current_port.same_port(port)
        || current_port.address != port.address
        || source_version(&current.hid, Some(installed)).await? != *installed
    {
        return Err("The selected dock changed. Rescan and prepare the update again.".into());
    }

    // Check an N4E image before changing USB modes. Catalog upgrades retain
    // their bulk-flasher fallback if this image is outside the HID scope.
    #[cfg(target_os = "linux")]
    let hid_bytes = if allow_hid {
        Some(n4e_image::read_image(image)?)
    } else {
        None
    };
    #[cfg(target_os = "linux")]
    let hid_image = hid_bytes
        .as_deref()
        .map(n4e_image::N4eImage::parse)
        .transpose();
    feedback.emit(Progress::Install(InstallStep::SwitchMode));
    enter_updater(&current.hid).await?;

    match wait_for_updater(flasher, port, allow_hid).await? {
        Updater::Bulk(_) if local => {
            return Err("Local N4E images require HID updater mode (33c3:8899). Reconnect the dock to return to normal mode.".into());
        }
        Updater::Bulk(updater) => {
            let address = format!("{}:{}", updater.bus, updater.address);
            feedback.emit(Progress::Install(InstallStep::Flash));
            run(
                flasher
                    .command()
                    .args(["burn", "--device", &address])
                    .arg(image),
                "flash firmware",
                feedback,
            )?;
            feedback.emit(Progress::Install(InstallStep::Restart));
        }
        #[cfg(target_os = "linux")]
        Updater::Hid(mut updater) => {
            let hid_image = hid_image?.ok_or("Missing N4E image.")?;
            if !matches_version(hid_image.version) {
                return Err(format!(
                    "HID image version {:?} differs from the expected firmware.",
                    hid_image.version
                )
                .into());
            }
            updater.check_layout().await?;
            feedback.emit(Progress::Install(InstallStep::Flash));
            updater
                .burn(&hid_image, |component| {
                    if feedback.0.is_none() {
                        println!(
                            "Writing {} ({} bytes)",
                            component.name,
                            component.data.len()
                        );
                    }
                })
                .await
                .map_err(|error| format!("HID flash verification failed: {error}"))?;
            feedback.emit(Progress::Install(InstallStep::Restart));
            // A successful reset can disconnect before the final reply.
            if updater.restart().await.is_err() && feedback.0.is_none() {
                println!("Flash readback passed. Reconnect the dock if it does not restart.");
            }
        }
    }

    let restarted = wait_for_usb(flasher, port, &id).await?;
    feedback.emit(Progress::Install(InstallStep::Verify));
    if cfg!(windows) {
        return Ok(InstallOutcome::Rebooted);
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if let Ok(connected) = docks::list().await {
            let mut matching = connected.iter().filter(|current| {
                current.usb_id() == id
                    && match current.usb_address {
                        Some(address) => address == (restarted.bus, restarted.address),
                        None => dock.usb_address.is_none(),
                    }
            });
            if let (Some(current), None) = (matching.next(), matching.next())
                && let Ok(Ok(reported)) =
                    timeout(Duration::from_secs(3), version(&current.hid)).await
            {
                if !matches_version(&reported) {
                    return Err(
                        format!("Device rebooted with unexpected firmware {reported:?}.").into(),
                    );
                }
                return Ok(InstallOutcome::Verified(reported));
            }
        }
        sleep(Duration::from_millis(200)).await;
    }
    Err("The dock rebooted, but the firmware version could not be read. Check it in Stream Dock or run version again.".into())
}

#[cfg(target_os = "linux")]
fn lock_port(port: &UsbDevice) -> Result<File> {
    let runtime = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(env::temp_dir);
    let key = Sha256::digest(port.path.as_bytes());
    let file = File::options()
        .write(true)
        .create(true)
        .truncate(false)
        .open(runtime.join(format!("streamdock-fw-{}-{key:x}.lock", port.bus)))?;
    file.try_lock()
        .map_err(|error| format!("Could not lock the selected USB port for flashing: {error}"))?;
    Ok(file)
}

async fn flash(options: FlashArgs) -> Result<()> {
    if let Some(path) = options.file.as_deref() {
        return flash_file(&options, path).await;
    }
    let dock = docks::select(&options.target).await?;
    let installed = source_version(&dock.hid, options.from.as_deref()).await?;
    let Some(update) = catalog::latest(&installed, options.model.as_deref())? else {
        println!("No newer compatible ArtInChip firmware listed for {installed}.");
        return Ok(());
    };
    println!(
        "Updating {} ({}) from {installed} to {}.",
        update.model,
        dock.usb_id(),
        update.version()
    );
    let feedback = Feedback(None);
    let prepared = prepare_install(
        dock,
        installed,
        update,
        options.flasher.as_deref(),
        &feedback,
    )
    .await?;
    match install(&prepared, &feedback).await? {
        InstallOutcome::Verified(reported) => {
            println!("Verified firmware {reported} after reboot.")
        }
        InstallOutcome::Rebooted => println!(
            "The dock rebooted. Check its updated firmware version in Stream Dock (catalog version {}).",
            prepared.update.version()
        ),
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
async fn flash_file(_: &FlashArgs, _: &Path) -> Result<()> {
    Err("Installing a local N4E image is currently supported on Linux.".into())
}

#[cfg(target_os = "linux")]
async fn flash_file(options: &FlashArgs, path: &Path) -> Result<()> {
    use std::io::IsTerminal;

    let bytes = n4e_image::read_image(path)?;
    let image = n4e_image::N4eImage::parse(&bytes)?;
    println!(
        "MiraBox N4E firmware {} ({})",
        image.version,
        path.display()
    );
    for component in &image.firmware {
        println!(
            "  {}: {} bytes, CRC {:08x}",
            component.partition,
            component.data.len(),
            component.checksum
        );
    }
    if options.dry_run {
        return Ok(());
    }
    let dock = docks::select(&options.target).await?;
    let installed = source_version(&dock.hid, options.from.as_deref()).await?;
    if dock.usb_id() != n4e_image::USB_ID || !n4e_image::is_n4e_version(&installed) {
        return Err("The selected dock did not identify itself as a MiraBox N4E.".into());
    }
    println!("{}: {installed} → {}", dock.label, image.version);
    if !options.yes {
        if !io::stdin().is_terminal() {
            return Err("Use --yes to confirm a local flash in a non-interactive session.".into());
        }
        print!("Flash the local firmware? This operation may brick your device. [y/N] ");
        io::stdout().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            println!("Flash cancelled.");
            return Ok(());
        }
    }
    let feedback = Feedback(None);
    let directory = tempfile::tempdir()?;
    let staged = directory.path().join("firmware.img");
    fs::write(&staged, &bytes)?;
    let flasher = prepare_flasher(directory.path(), options.flasher.as_deref(), &feedback)?;
    let (dock, port) = locate_dock(&dock, &flasher).await?;
    let prepared = PreparedFlash {
        dock,
        installed,
        _directory: directory,
        flasher,
        image: staged,
        port,
    };
    if let InstallOutcome::Verified(reported) = install_image(
        &prepared,
        |reported| reported == image.version,
        true,
        &feedback,
    )
    .await?
    {
        println!("Verified firmware {reported} after reboot.");
    }
    Ok(())
}

fn catalog_update(options: CatalogArgs) -> Result<Option<catalog::Update>> {
    let installed = options.from;
    let update = catalog::latest(&installed, options.model.as_deref())?;
    if let Some(update) = &update {
        println!("{}: {} → {}", update.model, installed, update.version());
    } else {
        println!("No newer compatible ArtInChip firmware listed for {installed}.");
    }
    Ok(update)
}

async fn execute(cli: Cli) -> Result<()> {
    match cli.action {
        Some(Action::Devices) => {
            let docks = docks::list().await?;
            if docks.is_empty() {
                println!("No accessible Stream Dock found.");
            }
            for dock in docks {
                println!("{}", dock.label);
            }
        }
        Some(Action::Version(options)) => {
            println!("{}", version(&docks::select(&options).await?.hid).await?);
        }
        Some(Action::Latest(options)) => {
            catalog_update(options)?;
        }
        Some(Action::Check { catalog, flasher }) => {
            if let Some(update) = catalog_update(catalog)? {
                let dir = tempfile::tempdir()?;
                prepare_update(dir.path(), &update, flasher.as_deref(), &Feedback(None))?;
                println!("Download verification and the offline burn plan passed.");
            }
        }
        Some(Action::Flash(options)) => flash(options).await?,
        Some(Action::EnterUpdater(options)) => {
            enter_updater(&docks::select(&options).await?.hid).await?;
            println!("Requested ArtInChip updater mode. No firmware was written.");
        }
        Some(Action::License) => io::stdout().write_all(LICENSE.as_bytes())?,
        None if !tui::has_terminal() => Cli::command().print_help()?,
        Some(Action::Tui { flasher }) => tui::run(flasher)?,
        None => tui::run(None)?,
    }
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match execute(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("streamdock-fw: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{UsbDevice, usb_port};

    #[test]
    fn duplicate_models_require_the_selected_physical_address() {
        let first = UsbDevice {
            bus: 1,
            address: 4,
            path: "1-2".into(),
            vid: "6603".into(),
            pid: "1007".into(),
        };
        let second = UsbDevice {
            bus: 2,
            address: 4,
            path: "2-3".into(),
            vid: "6603".into(),
            pid: "1007".into(),
        };
        let devices = [first, second];
        assert!(usb_port(&devices, "6603:1007", None).is_none());
        assert_eq!(
            usb_port(&devices, "6603:1007", Some((2, 4))).unwrap().path,
            "2-3"
        );
        assert!(usb_port(&devices[..1], "6603:1007", Some((2, 4))).is_none());
    }
}
