use std::{
    collections::BTreeMap,
    io::{self, IsTerminal},
    path::PathBuf,
    sync::{
        Arc,
        mpsc::{self, Receiver, Sender, TryRecvError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use ratatui::{
    crossterm::{
        event::{
            self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind,
            KeyModifiers,
        },
        execute,
    },
    layout::{Constraint, Layout, Rect},
    prelude::{Color, Frame, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, BorderType, Clear, Gauge, List, ListItem, ListState, Paragraph, Wrap},
};
use tokio::{runtime::Runtime, task::JoinSet, time::timeout};
use tui_input::{Input, InputRequest, backend::crossterm::EventHandler};

use crate::{
    Feedback, InstallOutcome, InstallStep, LICENSE, PreparedUpdate, Progress, Result, catalog,
    docks, install, prepare_install, version,
};

const ACCENT: Color = Color::Cyan;
const MIN_WIDTH: u16 = 44;
const MIN_HEIGHT: u16 = 14;

fn fits(area: Rect) -> bool {
    area.width >= MIN_WIDTH && area.height >= MIN_HEIGHT
}

fn runtime() -> io::Result<Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
}

struct Job<T> {
    messages: Receiver<T>,
    thread: JoinHandle<()>,
}

impl<T: Send + 'static> Job<T> {
    fn spawn(run: impl FnOnce(Sender<T>) + Send + 'static) -> io::Result<Self> {
        let (send, messages) = mpsc::channel();
        let thread = thread::Builder::new().spawn(move || run(send))?;
        Ok(Self { messages, thread })
    }
}

struct Device {
    label: String,
    key: String,
    serial: Option<SerialIdentity>,
    version: Option<std::result::Result<String, String>>,
    status: Option<DeviceStatus>,
}

type SerialIdentity = (u16, u16, String);
type CatalogSnapshot = std::result::Result<Arc<catalog::Catalogs>, String>;

enum DeviceStatus {
    Current,
    Available,
    ChooseModel,
    Unavailable,
    Unknown,
}

enum Probe {
    Devices(Vec<Device>),
    Version(String, std::result::Result<String, String>),
    Catalogs(CatalogSnapshot),
    Failed(String),
}

fn probe(send: Sender<Probe>) {
    let result: Result<()> = (|| {
        runtime()?.block_on(async {
            let connected = docks::list().await?;
            let devices = connected
                .iter()
                .map(|dock| Device {
                    label: dock.label.clone(),
                    key: dock.key.clone(),
                    serial: dock
                        .hid
                        .serial_number
                        .as_ref()
                        .filter(|serial| !serial.trim().is_empty())
                        .map(|serial| (dock.hid.vendor_id, dock.hid.product_id, serial.clone())),
                    version: None,
                    status: None,
                })
                .collect();
            let has_devices = !connected.is_empty();
            if send.send(Probe::Devices(devices)).is_err() {
                return Ok(());
            }
            let catalog_task = has_devices.then(|| {
                let send = send.clone();
                tokio::task::spawn_blocking(move || {
                    let keep_fetching = |_| true;
                    let result = catalog::Catalogs::fetch(&Feedback(Some(&keep_fetching)))
                        .map(Arc::new)
                        .map_err(|error| error.to_string());
                    let _ = send.send(Probe::Catalogs(result));
                })
            });
            let mut reads = JoinSet::new();
            for dock in connected {
                reads.spawn(async move {
                    let answer = match timeout(Duration::from_secs(5), version(&dock.hid)).await {
                        Ok(Ok(version)) if !version.is_empty() => Ok(version),
                        Ok(Ok(_)) => Err("The dock reported an empty firmware version.".into()),
                        Ok(Err(error)) => Err(error.to_string()),
                        Err(_) => Err("The dock did not answer the firmware version read.".into()),
                    };
                    (dock.key, answer)
                });
            }
            while let Some(result) = reads.join_next().await {
                let (key, version) = result?;
                if send.send(Probe::Version(key, version)).is_err() {
                    break;
                }
            }
            if let Some(task) = catalog_task {
                task.await?;
            }
            Ok(())
        })
    })();
    if let Err(error) = result {
        let _ = send.send(Probe::Failed(error.to_string()));
    }
}

enum Outcome {
    Ready(Box<PreparedUpdate>),
    Current,
    Unavailable,
    ChooseModel(Vec<String>),
    Installed(String),
    Rebooted(catalog::Update),
}

enum WorkerEvent {
    Progress(Progress),
    Done(std::result::Result<Outcome, String>),
}

fn update_job(
    run: impl FnOnce(&Feedback<'_>) -> Result<Outcome> + Send + 'static,
) -> io::Result<Job<WorkerEvent>> {
    Job::spawn(move |send| {
        let forward = |event| send.send(WorkerEvent::Progress(event)).is_ok();
        let outcome = run(&Feedback(Some(&forward))).map_err(|error| error.to_string());
        let _ = send.send(WorkerEvent::Done(outcome));
    })
}

enum Phase {
    Reading,
    EnterVersion,
    CheckingCatalogs,
    Preparing,
    Current,
    Unavailable,
    ChooseModel {
        models: Vec<String>,
        index: usize,
    },
    Ready {
        plan: Box<PreparedUpdate>,
        confirm: Option<bool>,
    },
    Installing(Option<InstallStep>),
    VerifyManually(catalog::Update),
    Success(String),
    Failed(String),
}

struct Detail {
    key: String,
    label: String,
    serial: Option<SerialIdentity>,
    hardware_changed: bool,
    version: Option<String>,
    phase: Phase,
    input: Input,
    notice: Option<String>,
    progress: Option<Progress>,
    started: Instant,
}

impl Detail {
    fn phase(&mut self, phase: Phase) {
        self.phase = phase;
        self.progress = None;
        self.started = Instant::now();
    }
}

#[derive(Default)]
struct App {
    devices: Vec<Device>,
    index: usize,
    scan: Option<Job<Probe>>,
    scan_error: Option<String>,
    catalogs: Option<CatalogSnapshot>,
    verified: BTreeMap<SerialIdentity, String>,
    detail: Option<Detail>,
    worker: Option<Job<WorkerEvent>>,
    flasher: Option<PathBuf>,
    license: Option<u16>,
}

impl App {
    fn rescan(&mut self) {
        if self.scan.is_none() {
            self.devices.clear();
            self.scan_error = None;
            self.catalogs = None;
            match Job::spawn(probe) {
                Ok(job) => self.scan = Some(job),
                Err(error) => self.scan_error = Some(error.to_string()),
            }
        }
    }

    fn read_scan(&mut self) {
        while let Some(scan) = &self.scan {
            match scan.messages.try_recv() {
                Ok(Probe::Devices(devices)) => {
                    self.index = self.index.min(devices.len().saturating_sub(1));
                    self.devices = devices;
                }
                Ok(Probe::Version(key, result)) => {
                    if let Some(dock) = self.devices.iter_mut().find(|dock| dock.key == key) {
                        dock.version = Some(result);
                    }
                }
                Ok(Probe::Catalogs(result)) => self.catalogs = Some(result),
                Ok(Probe::Failed(error)) => {
                    if let Some(detail) = &mut self.detail
                        && matches!(detail.phase, Phase::Reading)
                    {
                        detail.phase(Phase::Failed(error.clone()));
                    }
                    self.scan_error = Some(error);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => self.scan = None,
            }
        }
        if let Some(Ok(catalogs)) = &self.catalogs {
            for device in &mut self.devices {
                if device.status.is_none()
                    && let Some(Ok(version)) = &device.version
                {
                    device.status = Some(match catalogs.lookup(version, None) {
                        Ok(catalog::Lookup::Current) => DeviceStatus::Current,
                        Ok(catalog::Lookup::Available(_)) => DeviceStatus::Available,
                        Ok(catalog::Lookup::ModelRequired(_)) => DeviceStatus::ChooseModel,
                        Ok(catalog::Lookup::NoCompatibleUpdate) => DeviceStatus::Unavailable,
                        Err(_) => DeviceStatus::Unknown,
                    });
                }
            }
        }
        self.use_version();
        if matches!(
            self.detail.as_ref().map(|detail| &detail.phase),
            Some(Phase::CheckingCatalogs)
        ) && (self.catalogs.is_some() || self.scan.is_none())
        {
            self.prepare(None);
        }
    }

    fn open(&mut self) {
        if let Some(dock) = self.devices.get(self.index) {
            self.detail = Some(Detail {
                key: dock.key.clone(),
                label: dock.label.clone(),
                serial: dock.serial.clone(),
                hardware_changed: false,
                version: None,
                phase: Phase::Reading,
                input: Input::default(),
                notice: None,
                progress: None,
                started: Instant::now(),
            });
            self.use_version();
        }
    }

    fn refresh(&mut self) {
        if self.scan.is_some() {
            return;
        }
        if let Some(detail) = &mut self.detail
            && !matches!(detail.phase, Phase::VerifyManually(_))
        {
            self.worker = None;
            detail.version = None;
            detail.notice = None;
            detail.hardware_changed = false;
            detail.input.reset();
            detail.phase(Phase::Reading);
        }
        self.rescan();
    }

    fn use_version(&mut self) {
        let Some(detail) = &mut self.detail else {
            return;
        };
        if !matches!(detail.phase, Phase::Reading) {
            return;
        }
        let dock = self
            .devices
            .iter()
            .find(|dock| dock.key == detail.key)
            .or_else(|| {
                let serial = detail.serial.as_ref()?;
                let mut matches = self
                    .devices
                    .iter()
                    .filter(|dock| dock.serial.as_ref() == Some(serial));
                match (matches.next(), matches.next()) {
                    (Some(dock), None) => Some(dock),
                    _ => None,
                }
            });
        if let Some(dock) = dock {
            detail.key.clone_from(&dock.key);
            detail.label.clone_from(&dock.label);
        }
        let result = dock.and_then(|dock| dock.version.clone());
        match result {
            Some(Ok(version)) => {
                detail.version = Some(version);
                self.prepare(None);
            }
            Some(Err(error)) if !cfg!(target_os = "linux") => {
                detail.notice = Some(error);
                detail.phase(Phase::EnterVersion);
            }
            Some(Err(error)) => detail.phase(Phase::Failed(error)),
            None if self.scan.is_none() => detail.phase(Phase::Failed(
                self.scan_error.clone().unwrap_or_else(|| "Could not read the selected dock. Press R to refresh or Esc to choose another dock.".into())
            )),
            None => {}
        }
    }

    fn prepare(&mut self, model: Option<String>) {
        let Some(detail) = &mut self.detail else {
            return;
        };
        let Some(installed) = detail.version.clone() else {
            return;
        };
        let key = detail.key.clone();
        let flasher = self.flasher.clone();
        detail.notice = None;
        let catalogs = match &self.catalogs {
            Some(Ok(catalogs)) => Arc::clone(catalogs),
            Some(Err(error)) => {
                detail.phase(Phase::Failed(error.clone()));
                return;
            }
            None if self.scan.is_some() => {
                detail.phase(Phase::CheckingCatalogs);
                return;
            }
            None => {
                detail.phase(Phase::Failed(
                    "Could not load the firmware catalogs. Press R to refresh.".into(),
                ));
                return;
            }
        };
        detail.phase(Phase::Preparing);
        self.set_job(update_job(move |feedback| {
            match catalogs.lookup(&installed, model.as_deref())? {
                catalog::Lookup::Available(update) => runtime()?.block_on(async {
                    let dock = docks::select_by_key(&key).await?;
                    let plan =
                        prepare_install(dock, installed, update, flasher.as_deref(), feedback)
                            .await?;
                    Ok(Outcome::Ready(Box::new(plan)))
                }),
                catalog::Lookup::Current => Ok(Outcome::Current),
                catalog::Lookup::NoCompatibleUpdate => Ok(Outcome::Unavailable),
                catalog::Lookup::ModelRequired(models) => Ok(Outcome::ChooseModel(models)),
            }
        }));
    }

    fn set_job(&mut self, job: io::Result<Job<WorkerEvent>>) {
        match job {
            Ok(job) => self.worker = Some(job),
            Err(error) => {
                if let Some(detail) = &mut self.detail {
                    detail.phase(Phase::Failed(error.to_string()));
                }
            }
        }
    }

    fn read_worker(&mut self) {
        for _ in 0..256 {
            let Some(worker) = &self.worker else { break };
            let Some(detail) = &mut self.detail else {
                break;
            };
            match worker.messages.try_recv() {
                Ok(WorkerEvent::Progress(Progress::Install(step))) => {
                    if step == InstallStep::SwitchMode {
                        detail.hardware_changed = true;
                    }
                    detail.phase(Phase::Installing(Some(step)))
                }
                Ok(WorkerEvent::Progress(progress)) => detail.progress = Some(progress),
                Ok(WorkerEvent::Done(outcome)) => {
                    self.worker = None;
                    detail.input.reset();
                    detail.phase(match outcome {
                        Ok(Outcome::Ready(plan)) => Phase::Ready {
                            plan,
                            confirm: None,
                        },
                        Ok(Outcome::Current) => Phase::Current,
                        Ok(Outcome::Unavailable) => Phase::Unavailable,
                        Ok(Outcome::ChooseModel(models)) => Phase::ChooseModel { models, index: 0 },
                        Ok(Outcome::Installed(version)) => Phase::Success(version),
                        Ok(Outcome::Rebooted(update)) => Phase::VerifyManually(update),
                        Err(error) => Phase::Failed(error),
                    });
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.worker = None;
                    detail.phase(Phase::Failed("The updater stopped unexpectedly.".into()));
                }
            }
        }
    }

    fn enter(&mut self) {
        let Some(detail) = &mut self.detail else {
            self.open();
            return;
        };
        match &mut detail.phase {
            Phase::EnterVersion if !detail.input.value().trim().is_empty() => {
                detail.version = Some(detail.input.value().trim().to_owned());
                self.prepare(None);
            }
            Phase::ChooseModel { models, index } => {
                let model = models.get(*index).cloned();
                self.prepare(model);
            }
            Phase::Ready {
                confirm: confirm @ None,
                ..
            } => *confirm = Some(false),
            Phase::Ready {
                confirm: confirm @ Some(false),
                ..
            } => *confirm = None,
            Phase::Ready {
                confirm: Some(true),
                ..
            } => {
                let Phase::Ready { plan, .. } =
                    std::mem::replace(&mut detail.phase, Phase::Installing(None))
                else {
                    unreachable!()
                };
                detail.started = Instant::now();
                self.set_job(update_job(move |feedback| {
                    match runtime()?.block_on(install(&plan, feedback))? {
                        InstallOutcome::Verified(version) => Ok(Outcome::Installed(version)),
                        InstallOutcome::Rebooted => Ok(Outcome::Rebooted(plan.update)),
                    }
                }));
            }
            Phase::VerifyManually(update) if !detail.input.value().trim().is_empty() => {
                let reported = detail.input.value().trim();
                if update.matches_version(reported) {
                    if let Some(serial) = &detail.serial
                        && self
                            .devices
                            .iter()
                            .filter(|dock| dock.serial.as_ref() == Some(serial))
                            .count()
                            == 1
                    {
                        self.verified.insert(serial.clone(), reported.to_owned());
                    }
                    detail.notice = None;
                    detail.phase(Phase::Success(reported.to_owned()));
                } else {
                    detail.notice = Some("That version differs from the expected update. Check the version shown in Stream Dock.".into());
                }
            }
            _ => {}
        }
    }

    fn installing(&self) -> bool {
        matches!(
            self.detail.as_ref().map(|d| &d.phase),
            Some(Phase::Installing(_))
        )
    }

    fn handle_event(&mut self, event: Event, visible: bool) -> bool {
        if self.installing() {
            return false;
        }
        if let Event::Key(key) = event {
            if key.kind == KeyEventKind::Release {
                return false;
            }
            if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                return true;
            }
            if let Some(offset) = &mut self.license {
                match key.code {
                    KeyCode::Esc => self.license = None,
                    KeyCode::Up => *offset = offset.saturating_sub(1),
                    KeyCode::Down => *offset = offset.saturating_add(1),
                    KeyCode::PageUp => *offset = offset.saturating_sub(10),
                    KeyCode::PageDown => *offset = offset.saturating_add(10),
                    _ => {}
                }
                return false;
            }
            if key.code == KeyCode::Esc {
                if let Some(detail) = &mut self.detail {
                    if let Phase::Ready {
                        confirm: confirm @ Some(_),
                        ..
                    } = &mut detail.phase
                    {
                        *confirm = None;
                    } else {
                        let refresh = detail.hardware_changed;
                        self.detail = None;
                        self.worker = None;
                        if refresh {
                            self.scan = None;
                            self.rescan();
                        }
                    }
                    return false;
                }
                return true;
            }
            if !visible {
                return false;
            }
            let editing = matches!(
                self.detail.as_ref().map(|detail| &detail.phase),
                Some(Phase::EnterVersion | Phase::VerifyManually(_))
            );
            if key.kind == KeyEventKind::Press
                && matches!(key.code, KeyCode::Char('r' | 'R'))
                && (!editing || key.modifiers.contains(KeyModifiers::CONTROL))
            {
                self.refresh();
                return false;
            }
            if key.code == KeyCode::Enter && key.kind == KeyEventKind::Press {
                self.enter();
                return false;
            }
            match self.detail.as_mut().map(|detail| &mut detail.phase) {
                None => match key.code {
                    KeyCode::Up => self.index = self.index.saturating_sub(1),
                    KeyCode::Down => {
                        self.index = (self.index + 1).min(self.devices.len().saturating_sub(1))
                    }
                    KeyCode::Char('l') => self.license = Some(0),
                    KeyCode::Char('q') => return true,
                    _ => {}
                },
                Some(Phase::ChooseModel { models, index }) => match key.code {
                    KeyCode::Up => *index = index.saturating_sub(1),
                    KeyCode::Down => *index = (*index + 1).min(models.len().saturating_sub(1)),
                    _ => {}
                },
                Some(Phase::Ready {
                    confirm: Some(yes), ..
                }) => match key.code {
                    KeyCode::Left => *yes = true,
                    KeyCode::Right => *yes = false,
                    KeyCode::Tab | KeyCode::BackTab => *yes = !*yes,
                    _ => {}
                },
                _ => {}
            }
        }
        if visible
            && let Some(detail) = &mut self.detail
            && matches!(detail.phase, Phase::EnterVersion | Phase::VerifyManually(_))
        {
            if let Event::Paste(text) = &event {
                for ch in text
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .chars()
                    .filter(|ch| !ch.is_control())
                {
                    detail.input.handle(InputRequest::InsertChar(ch));
                }
            } else {
                detail.input.handle_event(&event);
            }
        }
        false
    }

    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        if !fits(area) {
            let notice = if self.installing() {
                "Update in progress. Keep the dock connected and enlarge the terminal."
            } else {
                "Enlarge the terminal to at least 44 × 14."
            };
            message(frame, area, notice);
            return;
        }
        let [header, body, footer] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .areas(area);
        frame.render_widget(
            Line::from(" Stream Dock Firmware Updater ")
                .fg(Color::Black)
                .bg(ACCENT)
                .bold(),
            header,
        );
        let help = if let Some(offset) = &mut self.license {
            let block = panel(" EUPL-1.2 ");
            let inner = block.inner(body);
            let text = Paragraph::new(LICENSE).wrap(Wrap { trim: false });
            let height = u16::try_from(text.line_count(inner.width)).unwrap_or(u16::MAX);
            *offset = (*offset).min(height.saturating_sub(inner.height));
            frame.render_widget(block, body);
            frame.render_widget(text.scroll((*offset, 0)), inner);
            "↑↓ / PgUp / PgDn Scroll   ·   Esc Back"
        } else if let Some(detail) = &self.detail {
            draw_detail(frame, body, detail)
        } else {
            self.draw_devices(frame, body);
            "↑↓ Select · Enter Open · R Refresh · L License · Esc Quit"
        };
        frame.render_widget(Line::from(help).fg(Color::Gray), footer);
    }

    fn draw_devices(&self, frame: &mut Frame, area: Rect) {
        let block = panel(" Select device ");
        if self.devices.is_empty() {
            let message = self
                .scan_error
                .as_deref()
                .unwrap_or(if self.scan.is_some() {
                    "Looking for connected Stream Docks…"
                } else {
                    "Connect your Stream Dock and press R to rescan."
                });
            frame.render_widget(
                Paragraph::new(message)
                    .block(block)
                    .wrap(Wrap { trim: true }),
                area,
            );
        } else {
            let width = block.inner(area).width.saturating_sub(2);
            let list = List::new(self.devices.iter().map(|dock| self.device_row(dock, width)))
                .block(block)
                .highlight_symbol("› ")
                .highlight_style(Style::new().bold());
            let mut state = ListState::default().with_selected(Some(self.index));
            frame.render_stateful_widget(list, area, &mut state);
        }
    }

    fn device_row<'a>(&'a self, dock: &'a Device, width: u16) -> ListItem<'a> {
        let verified = dock.serial.as_ref().and_then(|serial| {
            self.verified.get(serial).filter(|_| {
                self.devices
                    .iter()
                    .filter(|device| device.serial.as_ref() == Some(serial))
                    .count()
                    == 1
            })
        });
        let (status, color) = match (&dock.version, &dock.status) {
            (None, _) => ("Reading firmware", Color::Gray),
            (Some(Err(_)), _) if verified.is_some() => ("✓ Verified this session", Color::Green),
            (Some(Err(_)), _) if cfg!(target_os = "linux") => ("Version unreadable", Color::Yellow),
            (Some(Err(_)), _) => ("Version needed", Color::Yellow),
            (_, Some(DeviceStatus::Current)) => ("✓ Up to date", Color::Green),
            (_, Some(DeviceStatus::Available)) => ("↑ Update available", Color::Yellow),
            (_, Some(DeviceStatus::ChooseModel)) => ("Choose model", Color::Yellow),
            (_, Some(DeviceStatus::Unavailable)) => ("No compatible update", Color::Gray),
            (_, Some(DeviceStatus::Unknown)) => ("Could not match firmware", Color::Yellow),
            (_, None) if matches!(self.catalogs, Some(Err(_))) => {
                ("Catalogs unavailable", Color::Yellow)
            }
            (_, None) => ("Checking for updates", Color::Gray),
        };
        let version = match &dock.version {
            Some(Ok(version)) => Some(version.as_str()),
            Some(Err(_)) => verified.map(String::as_str),
            None => None,
        };
        let mut label = Line::from(vec![
            status.fg(color),
            Span::raw("  ·  "),
            Span::raw(&dock.label),
        ]);
        if let Some(version) = version {
            let firmware = version.gray();
            if label.width() + firmware.width() + 5 > usize::from(width) {
                return ListItem::new(vec![label, Line::from(vec![Span::raw("  "), firmware])]);
            }
            label.spans.extend([Span::raw("  ·  "), firmware]);
        }
        ListItem::new(label)
    }
}

fn panel(title: impl Into<Line<'static>>) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .title(title)
}

fn draw_detail(frame: &mut Frame, area: Rect, detail: &Detail) -> &'static str {
    let block = panel(format!(" {} ", detail.label));
    let [summary, content] =
        Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).areas(block.inner(area));
    frame.render_widget(block, area);
    if let Some(version) = detail.version.as_deref() {
        let summary_line = match &detail.phase {
            Phase::Current => Line::from(format!("{version} ✓")).green().bold(),
            Phase::Ready { plan, .. } => Line::from(vec![
                version.gray(),
                Span::raw("  →  "),
                plan.update.version().green().bold(),
            ]),
            Phase::Success(reported) => Line::from(reported.as_str()).green().bold(),
            _ => Line::from(version.gray()),
        };
        frame.render_widget(
            Paragraph::new(summary_line).wrap(Wrap { trim: true }),
            summary,
        );
    }
    match &detail.phase {
        Phase::Reading => message(frame, content, "Reading the dock’s installed firmware…"),
        Phase::EnterVersion => {
            let instructions = detail
                .notice
                .as_deref()
                .unwrap_or("Copy the installed version from Stream Dock → Settings → Device.");
            draw_input(frame, content, instructions, &detail.input);
            return "Enter Check · Ctrl+R Refresh · Esc Back";
        }
        Phase::CheckingCatalogs => message(frame, content, "Checking for updates…"),
        Phase::Preparing => draw_download(frame, content, detail),
        Phase::Current => message(frame, content, "Up to date"),
        Phase::Unavailable => message(
            frame,
            content,
            "No compatible upgrade is listed for this firmware.",
        ),
        Phase::ChooseModel { models, index } => {
            let list = List::new(models.iter().map(String::as_str))
                .block(panel(" Select your model "))
                .highlight_symbol("› ")
                .highlight_style(Style::new().fg(ACCENT).bold());
            let mut state = ListState::default().with_selected(Some(*index));
            frame.render_stateful_widget(list, content, &mut state);
            return "↑↓ Model · Enter Select · R Refresh · Esc Back";
        }
        Phase::Ready { confirm, .. } => {
            message(frame, content, Line::from("[ Update ]").fg(ACCENT).bold());
            if let Some(yes) = confirm {
                draw_confirm(frame, area, *yes);
                return "← → Choose · Enter Confirm · R Refresh · Esc Back";
            }
            return "Enter Update · R Refresh · Esc Back";
        }
        Phase::Installing(step) => {
            draw_install(frame, content, *step, detail.started.elapsed());
            return "Keep your dock connected";
        }
        Phase::VerifyManually(update) => {
            let text = format!(
                "The dock restarted. Read its version in Stream Dock → Settings → Device. Expected {}.\n{}",
                update.version(),
                detail
                    .notice
                    .as_deref()
                    .unwrap_or("Enter the version shown by the dock.")
            );
            draw_input(frame, content, &text, &detail.input);
            return "Enter Verify · Ctrl+R Refresh · Esc Back";
        }
        Phase::Success(_) => message(frame, content, Line::from("✓ SUCCESS").green().bold()),
        Phase::Failed(error) => message(frame, content, Line::from(error.as_str()).red()),
    }
    "R Refresh · Esc Back to devices"
}

fn message<'a>(frame: &mut Frame, area: Rect, text: impl Into<Line<'a>>) {
    frame.render_widget(Paragraph::new(text.into()).wrap(Wrap { trim: true }), area);
}

fn draw_input(frame: &mut Frame, area: Rect, instructions: &str, input: &Input) {
    let [hint, field] = Layout::vertical([Constraint::Min(3), Constraint::Length(3)]).areas(area);
    frame.render_widget(Paragraph::new(instructions).wrap(Wrap { trim: true }), hint);
    let block = panel(" Firmware version ");
    let inner = block.inner(field);
    let width = usize::from(inner.width.saturating_sub(1));
    let scroll = input.visual_scroll(width);
    let offset = input.visual_cursor().saturating_sub(scroll).min(width) as u16;
    frame.render_widget(
        Paragraph::new(input.value())
            .block(block)
            .scroll((0, scroll as u16)),
        field,
    );
    if inner.width > 0 && inner.height > 0 {
        frame.set_cursor_position((inner.x + offset, inner.y));
    }
}

fn draw_download(frame: &mut Frame, area: Rect, detail: &Detail) {
    match &detail.progress {
        Some(Progress::Download { name, bytes, total }) => {
            let title = if name.contains("firmware-version-check") {
                "Checking for updates"
            } else if name.contains("artinchip-flash") {
                "Downloading flasher"
            } else {
                "Downloading firmware"
            };
            let [heading, bar, _] = Layout::vertical([
                Constraint::Length(2),
                Constraint::Length(2),
                Constraint::Min(0),
            ])
            .areas(area);
            message(frame, heading, title);
            if let Some(total) = total.filter(|total| *total > 0) {
                let fraction = (*bytes as f64 / total as f64).clamp(0.0, 1.0);
                frame.render_widget(Gauge::default().gauge_style(ACCENT).ratio(fraction), bar);
            } else {
                let age = detail.started.elapsed();
                let width = usize::from(bar.width.min(24));
                let lit = (age.as_millis() / 100) as usize % width.max(1);
                let pulse: String = (0..width)
                    .map(|cell| if cell == lit { '━' } else { '─' })
                    .collect();
                frame.render_widget(
                    Paragraph::new(vec![
                        Line::from(pulse).fg(ACCENT),
                        Line::from(format!(
                            "{} KiB received · {}s",
                            bytes / 1024,
                            age.as_secs()
                        )),
                    ]),
                    bar,
                );
            }
        }
        Some(Progress::Validating) => {
            message(frame, area, "Verifying the firmware image and burn plan…")
        }
        _ => message(frame, area, "Checking for updates…"),
    }
}

fn draw_confirm(frame: &mut Frame, area: Rect, yes: bool) {
    let width = area.width.min(58);
    let height = area.height.min(8);
    let popup = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    let block = panel(" Confirm update ");
    let inner = block.inner(popup);
    frame.render_widget(Clear, popup);
    frame.render_widget(block, popup);
    let choices = Line::from(vec![
        if yes {
            "› YES ‹".red().bold().reversed()
        } else {
            "  YES  ".red().bold()
        },
        Span::raw("      "),
        if yes {
            "  NO  ".green().bold()
        } else {
            "› NO ‹".green().bold().reversed()
        },
    ]);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from("FLASH NEW FIRMWARE?").bold(),
            Line::from("THIS OPERATION MAY BRICK YOUR DEVICE.")
                .red()
                .bold(),
            Line::default(),
            choices,
        ])
        .centered()
        .wrap(Wrap { trim: true }),
        inner,
    );
}

fn draw_install(frame: &mut Frame, area: Rect, active: Option<InstallStep>, elapsed: Duration) {
    let steps = [
        (InstallStep::SwitchMode, "Switch into firmware mode", 8),
        (InstallStep::Flash, "Flash firmware", 45),
        (InstallStep::Restart, "Wait for the dock to restart", 20),
        (InstallStep::Verify, "Verify the new firmware version", 8),
    ];
    let mut done = active.is_some();
    let mut lines = Vec::new();
    for (step, label, slow_after) in steps {
        if active == Some(step) {
            done = false;
            lines.push(
                Line::from(format!("› {label}… {}s", elapsed.as_secs()))
                    .fg(ACCENT)
                    .bold(),
            );
            if elapsed.as_secs() >= slow_after {
                lines.push(Line::from("  This is taking longer than usual…").yellow());
            }
        } else if done {
            lines.push(Line::from(format!("✓ {label}")).green());
        } else {
            lines.push(Line::from(format!("· {label}")).gray());
        }
    }
    if active.is_none() {
        lines.insert(
            0,
            Line::from(format!(
                "Rechecking the selected dock… {}s",
                elapsed.as_secs()
            )),
        );
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), area);
}

pub fn has_terminal() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

pub fn run(flasher: Option<PathBuf>) -> io::Result<()> {
    if !has_terminal() {
        return Err(io::Error::other(
            "Open streamdock-fw tui in an interactive terminal.",
        ));
    }
    let mut app = App {
        flasher,
        ..App::default()
    };
    let result = (|| {
        let mut terminal = ratatui::try_init()?;
        let _ = execute!(terminal.backend_mut(), EnableBracketedPaste);
        app.rescan();
        loop {
            app.read_scan();
            app.read_worker();
            let visible = fits(terminal.draw(|frame| app.draw(frame))?.area);
            if event::poll(Duration::from_millis(80))? && app.handle_event(event::read()?, visible)
            {
                return Ok(());
            }
        }
    })();
    let _ = execute!(io::stdout(), DisableBracketedPaste);
    let restored = ratatui::try_restore();
    if app.installing()
        && let Some(worker) = app.worker.take()
    {
        let _ = worker.thread.join();
    }
    result.and(restored)
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend, prelude::Color};
    use std::time::Duration;

    use super::{App, Device, DeviceStatus, InstallStep, draw_confirm, draw_install};

    #[test]
    fn picker_shows_installed_versions_and_distinguishes_current_from_available() {
        let devices = [
            ("First dock", "V3.Dock.02.010", DeviceStatus::Current),
            ("Second dock", "V3.Dock.01.007", DeviceStatus::Available),
        ]
        .into_iter()
        .map(|(label, version, status)| Device {
            label: label.into(),
            key: label.into(),
            serial: None,
            version: Some(Ok(version.into())),
            status: Some(status),
        })
        .collect();
        let app = App {
            devices,
            ..App::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(44, 14)).unwrap();
        terminal
            .draw(|frame| app.draw_devices(frame, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rendered: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
        assert!(rendered.contains("✓ Up to date"));
        assert!(rendered.contains("↑ Update available"));
        assert!(rendered.contains("V3.Dock.02.010"));
        assert!(rendered.contains("V3.Dock.01.007"));
        assert!(
            buffer
                .content
                .iter()
                .any(|cell| cell.symbol() == "✓" && cell.fg == Color::Green)
        );
    }

    #[test]
    fn confirmation_highlights_no_and_keeps_yes_and_no_distinct() {
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal
            .draw(|frame| draw_confirm(frame, frame.area(), false))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rendered: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
        assert!(rendered.contains("FLASH NEW FIRMWARE?"));
        assert!(rendered.contains("THIS OPERATION MAY BRICK YOUR DEVICE."));
        assert!(rendered.contains("› NO ‹"));
        assert!(
            buffer
                .content
                .iter()
                .any(|cell| cell.symbol() == "Y" && cell.fg == Color::Red)
        );
        assert!(
            buffer
                .content
                .iter()
                .any(|cell| cell.symbol() == "N" && cell.fg == Color::Green)
        );
    }

    #[test]
    fn slow_restart_still_shows_elapsed_time_and_completed_steps() {
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal
            .draw(|frame| {
                draw_install(
                    frame,
                    frame.area(),
                    Some(InstallStep::Restart),
                    Duration::from_secs(23),
                )
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rendered: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
        assert!(rendered.contains("✓ Switch into firmware mode"));
        assert!(rendered.contains("✓ Flash firmware"));
        assert!(rendered.contains("Wait for the dock to restart… 23s"));
        assert!(rendered.contains("This is taking longer than usual"));
    }
}
