#![cfg_attr(windows, windows_subsystem = "console")]

use std::{
    env,
    error::Error,
    fs::File,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::ExitCode,
    time::{Duration, Instant},
};

use artinchip_flash::{BurnEvent, FirmwareImage, UpgDevice, device::format_plan_lines};
use async_hid::{AsyncHidWrite, Device as HidDevice};
use clap::{Args, CommandFactory, Parser, Subcommand};
use sha2::{Digest, Sha256};
use tokio::time::{sleep, timeout};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

mod bulk_usb;
mod catalog;
mod docks;
#[cfg(target_os = "linux")]
mod hid_updater;
#[cfg(target_os = "linux")]
mod n4e_image;
mod tui;

use bulk_usb::{BulkTransport, UPDATER, UsbDevice};
const HID_UPDATER: &str = "33c3:8899";
const CHECK_VERSION: &str = "V3.293N4E.01.007";
const LICENSE: &str = concat!(
    "Stream Dock Firmware Updater · EUPL-1.2\n",
    "Source code ",
    env!("CARGO_PKG_REPOSITORY"),
    "\n\n",
    include_str!("../LICENSE"),
    "\n\nArtInChip firmware SDK · Apache-2.0\nhttps://github.com/boa-z/artinchip-flash\n\n",
    include_str!("../licenses/artinchip-flash.txt"),
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
    Check(CatalogArgs),
    /// Install compatible firmware from the catalog or a local N4E image.
    #[command(after_help = "On Windows, flash requires --from.")]
    Flash(FlashArgs),
    /// Enter USB updater mode without writing firmware.
    EnterUpdater(DockArgs),
    /// Open the interactive terminal interface.
    Tui,
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

fn prepare_update(update: &catalog::Update, feedback: &Feedback<'_>) -> Result<Vec<u8>> {
    let bytes = update.download(feedback)?;
    feedback.emit(Progress::Validating);
    let image = FirmwareImage::from_bytes(bytes.clone())?;
    let flash = image.prepare(image.default_options())?;
    if feedback.0.is_none() {
        for line in format_plan_lines(flash.plan()) {
            println!("{line}");
        }
    }
    Ok(bytes)
}

fn usb_port<'a>(
    devices: &'a [UsbDevice],
    id: &str,
    address: Option<(u8, u8)>,
) -> Option<&'a UsbDevice> {
    let mut matching = devices.iter().filter(|usb| {
        usb.matches(id)
            && address.is_none_or(|address| {
                usb.bus.parse().ok().map(|bus| (bus, usb.address)) == Some(address)
            })
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

async fn wait_for_usb(port: &UsbDevice, id: &str) -> Result<UsbDevice> {
    let deadline = Instant::now() + Duration::from_secs(90);
    while Instant::now() < deadline {
        let devices = bulk_usb::devices()?;
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

async fn wait_for_updater(port: &UsbDevice, allow_hid: bool) -> Result<Updater> {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut hid_seen = false;
    while Instant::now() < deadline {
        if let Some(updater) = bulk_usb::devices()?.into_iter().find(|device| {
            device.same_port(port) && (device.matches(UPDATER) || device.matches(HID_UPDATER))
        }) {
            if updater.matches(UPDATER) {
                #[cfg(target_os = "linux")]
                {
                    // The kernel lists the device before udev or Nook grants access.
                    let node = format!("/dev/bus/usb/{}/{:03}", updater.bus, updater.address);
                    if File::options().read(true).write(true).open(node).is_err() {
                        sleep(Duration::from_millis(300)).await;
                        continue;
                    }
                }
                return Ok(Updater::Bulk(updater));
            }
            hid_seen = true;
            if !allow_hid {
                return Err("This dock entered HID updater mode. HID flashing is currently available for MiraBox N4E images on Linux. Reconnect the dock to return to normal mode.".into());
            }
            #[cfg(target_os = "linux")]
            if let Some(address) = updater.linux_address()
                && let Some(device) = docks::open_hid_updater(address).await?
            {
                return Ok(Updater::Hid(device));
            }
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
    image: Vec<u8>,
    port: UsbDevice,
}

enum InstallOutcome {
    Verified(String),
    Rebooted,
}

async fn locate_dock(dock: &docks::Dock) -> Result<(docks::Dock, UsbDevice)> {
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
    let devices = bulk_usb::devices()?;
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
    feedback: &Feedback<'_>,
) -> Result<PreparedUpdate> {
    let image = prepare_update(&update, feedback)?;
    let (dock, port) = locate_dock(&dock).await?;
    let flash = PreparedFlash {
        dock,
        installed,
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
        image,
        port,
        ..
    } = prepared;
    let id = dock.usb_id();
    #[cfg(not(target_os = "linux"))]
    let allow_hid = false;
    #[cfg(target_os = "linux")]
    let allow_hid = id == n4e_image::USB_ID && n4e_image::is_n4e_version(installed);
    let _port_lock = lock_port(port)?;

    let (current, current_port) = locate_dock(dock).await?;
    if !current_port.same_port(port)
        || current_port.address != port.address
        || source_version(&current.hid, Some(installed)).await? != *installed
    {
        return Err("The selected dock changed. Rescan and prepare the update again.".into());
    }

    // Check an N4E image before changing USB modes. Catalog upgrades retain
    // their bulk-flasher fallback if this image is outside the HID scope.
    #[cfg(target_os = "linux")]
    let hid_image = allow_hid
        .then(|| n4e_image::N4eImage::parse(image))
        .transpose();
    feedback.emit(Progress::Install(InstallStep::SwitchMode));
    enter_updater(&current.hid).await?;

    match wait_for_updater(port, allow_hid).await? {
        Updater::Bulk(_) if local => {
            return Err("Local N4E images require HID updater mode (33c3:8899). Reconnect the dock to return to normal mode.".into());
        }
        Updater::Bulk(updater) => {
            let bulk_image = FirmwareImage::from_bytes(image.clone())?;
            let mut options = bulk_image.default_options();
            options.reset_after_burn = false;
            let flash = bulk_image.prepare(options)?;
            let mut device = UpgDevice::new(BulkTransport::open(updater)?);
            feedback.emit(Progress::Install(InstallStep::Flash));
            let print_progress = feedback.0.is_none();
            flash.execute(
                &mut device,
                Some(&mut |event| {
                    if print_progress {
                        match event {
                            BurnEvent::Stage(message) => println!("{message}"),
                            BurnEvent::ComponentStarted { name, size, .. } => {
                                println!("Writing {name} ({size} bytes)")
                            }
                            _ => {}
                        }
                    }
                }),
            )?;
            feedback.emit(Progress::Install(InstallStep::Restart));
            // The reset can disconnect the device before it sends the final reply.
            let _ = device.reset();
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

    let restarted = wait_for_usb(port, &id).await?;
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
                        Some((bus, address)) => {
                            restarted.bus.parse() == Ok(bus) && restarted.address == address
                        }
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

fn lock_port(port: &UsbDevice) -> Result<File> {
    let runtime = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(env::temp_dir);
    let key = Sha256::digest(format!("{}:{}", port.bus, port.path).as_bytes());
    let file = File::options()
        .write(true)
        .create(true)
        .truncate(false)
        .open(runtime.join(format!("streamdock-fw-{key:x}.lock")))?;
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
    let prepared = prepare_install(dock, installed, update, &feedback).await?;
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
    let (dock, port) = locate_dock(&dock).await?;
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
    let expected = image.version.to_owned();
    let prepared = PreparedFlash {
        dock,
        installed,
        image: bytes,
        port,
    };
    if let InstallOutcome::Verified(reported) =
        install_image(&prepared, |reported| reported == expected, true, &feedback).await?
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
        Some(Action::Check(catalog)) => {
            if let Some(update) = catalog_update(catalog)? {
                prepare_update(&update, &Feedback(None))?;
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
        Some(Action::Tui) | None => tui::run()?,
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
            bus: "001".into(),
            address: 4,
            path: "1-2".into(),
            vid: 0x6603,
            pid: 0x1007,
        };
        let second = UsbDevice {
            bus: "002".into(),
            address: 4,
            path: "2-3".into(),
            vid: 0x6603,
            pid: 0x1007,
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
