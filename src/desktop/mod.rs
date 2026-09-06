//! The native desktop window.
//!
//! A single window over the local daemon for the four things a person does
//! most: paste a transcript and submit it, answer the question that comes
//! back, read or copy the refined prompt, and keep the Gemini key and the
//! enrolled folders current. Everything it shows comes from the same loopback
//! API the browser interface reads, so a case started here is visible there
//! and to the command line, and vice versa.
//!
//! The window is immediate-mode: every frame redraws from [`App`]'s state,
//! and every call to the daemon runs on a background runtime and reports back
//! through a channel. Nothing here blocks on the network or the credential
//! store, so the window stays responsive while a case runs.

mod client;
mod prompt_text;
mod transcript;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::PathBuf,
    sync::mpsc,
    time::{Duration, Instant},
};

use eframe::egui::{self, Color32, RichText};

use crate::{
    config::{Config, DataDir},
    domain::{
        Answer, AnswerSet, AnswerValue, CaseEventPayload, CaseId, CaseState, Destination,
        LocalExportDestination, Outcome, OverlordDestination, QuestionId, QuestionRequestId,
        RefinementRequest, RepositoryId, ResponseType, SchemaVersion, SourceRef, SourceSystem,
    },
    error::{AppError, Result},
    storage::{CaseDetail, CaseSummary},
};

pub use client::{Backend, DaemonMode, Health, Msg, Notifier, RepositoryView, INSTANCE};

/// Open the window and run it until it is closed.
///
/// Configuration is resolved exactly as the command line resolves it, so the
/// window and `refinery serve` always agree on the data directory, the port,
/// and the credential store.
pub fn run() -> Result<()> {
    let data_dir = DataDir::resolve()?;
    let config = Config::load_from(data_dir)?;
    let _logging = crate::diagnostics::logging::init(&config.data_dir, &config.settings.logging)?;
    tracing::debug!(
        version = env!("CARGO_PKG_VERSION"),
        data_dir = %config.data_dir.root().display(),
        "starting the desktop window"
    );
    crate::config::paths::ensure_private_dir(&config.data_dir.root().join("exports"))?;

    let notifier = Notifier::default();
    let (backend, rx) = Backend::start(config, notifier.clone())?;

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Refinery")
            .with_inner_size([1180.0, 800.0])
            .with_min_inner_size([860.0, 600.0]),
        centered: true,
        ..Default::default()
    };
    eframe::run_native(
        "Refinery",
        options,
        Box::new(move |cc| {
            notifier.attach(cc.egui_ctx.clone());
            Ok(Box::new(App::new(backend, rx)))
        }),
    )
    .map_err(|error| AppError::Internal(anyhow::anyhow!("the window could not open: {error}")))
}

/// Environment variable naming the tab the window opens on.
pub const START_TAB_ENV: &str = "REFINERY_DESKTOP_TAB";

/* Timing ------------------------------------------------------------------ */

const HEALTH_INTERVAL: Duration = Duration::from_secs(15);
const CASES_INTERVAL: Duration = Duration::from_secs(5);
const ACTIVE_CASE_INTERVAL: Duration = Duration::from_secs(1);
const IDLE_CASE_INTERVAL: Duration = Duration::from_secs(10);
const NOTICE_LIFETIME: Duration = Duration::from_secs(8);

/* State ------------------------------------------------------------------- */

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Compose,
    Cases,
    Repositories,
    Settings,
}

impl Tab {
    const ALL: [Tab; 4] = [Tab::Compose, Tab::Cases, Tab::Repositories, Tab::Settings];

    /// The tab the window opens on, from `REFINERY_DESKTOP_TAB` when set.
    ///
    /// A support thread or a screenshot script wants the window to open on a
    /// particular tab; the default is where a person starts a new case.
    fn initial() -> Self {
        match std::env::var(START_TAB_ENV)
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "cases" => Tab::Cases,
            "repositories" => Tab::Repositories,
            "settings" => Tab::Settings,
            _ => Tab::Compose,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Tab::Compose => "Compose",
            Tab::Cases => "Cases",
            Tab::Repositories => "Repositories",
            Tab::Settings => "Settings",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DestinationChoice {
    LocalExport,
    Overlord,
}

#[derive(Default)]
struct Compose {
    text: String,
    task_hint: String,
    repository: Option<RepositoryId>,
    destination: Option<DestinationChoice>,
    message_count: usize,
    busy: bool,
    error: Option<String>,
}

#[derive(Debug, Clone)]
enum AnswerDraft {
    Text(String),
    Choice(Option<String>),
    Choices(BTreeSet<String>),
}

#[derive(Default)]
struct Answers {
    request: Option<QuestionRequestId>,
    drafts: HashMap<QuestionId, AnswerDraft>,
    busy: bool,
    error: Option<String>,
}

#[derive(Default)]
struct Repositories {
    items: Vec<RepositoryView>,
    error: Option<String>,
    path_input: String,
    busy: bool,
}

#[derive(Default)]
struct KeyPanel {
    input: String,
    busy: bool,
    message: Option<(bool, String)>,
}

struct Notice {
    ok: bool,
    text: String,
    shown_at: Instant,
}

/// Something the interface asked for during a frame, applied after drawing so
/// no widget closure needs to hold the whole application mutably.
enum Action {
    SelectCase(CaseId),
    Submit,
    SubmitAnswers,
    Cancel(CaseId),
    Retry(CaseId),
    AddRepository(PathBuf),
    ForgetRepository(RepositoryId),
    StoreKey,
    TestKey,
    RemoveKey,
    Copy(String),
    SaveMarkdown(String, String),
    OpenInterface,
    Refresh,
}

/// The window.
pub struct App {
    backend: Backend,
    rx: mpsc::Receiver<Msg>,
    tab: Tab,
    health: Option<Health>,
    health_error: Option<String>,
    last_health: Option<Instant>,
    cases: Vec<CaseSummary>,
    cases_error: Option<String>,
    last_cases: Option<Instant>,
    selected: Option<CaseId>,
    detail: Option<CaseDetail>,
    detail_error: Option<String>,
    last_detail: Option<Instant>,
    detail_busy: bool,
    compose: Compose,
    answers: Answers,
    repositories: Repositories,
    key: KeyPanel,
    notice: Option<Notice>,
    actions: Vec<Action>,
}

impl App {
    fn new(backend: Backend, rx: mpsc::Receiver<Msg>) -> Self {
        backend.refresh_health();
        backend.refresh_repositories();
        backend.refresh_cases();
        let compose = Compose {
            destination: Some(DestinationChoice::LocalExport),
            ..Default::default()
        };
        Self {
            backend,
            rx,
            tab: Tab::initial(),
            health: None,
            health_error: None,
            last_health: Some(Instant::now()),
            cases: Vec::new(),
            cases_error: None,
            last_cases: Some(Instant::now()),
            selected: None,
            detail: None,
            detail_error: None,
            last_detail: None,
            detail_busy: false,
            compose,
            answers: Answers::default(),
            repositories: Repositories::default(),
            key: KeyPanel::default(),
            notice: None,
            actions: Vec::new(),
        }
    }

    fn notify(&mut self, ok: bool, text: impl Into<String>) {
        self.notice = Some(Notice {
            ok,
            text: text.into(),
            shown_at: Instant::now(),
        });
    }

    /* Messages from the daemon ---------------------------------------------- */

    fn drain_messages(&mut self) {
        while let Ok(message) = self.rx.try_recv() {
            self.handle(message);
        }
    }

    fn handle(&mut self, message: Msg) {
        match message {
            Msg::Health(Ok(health)) => {
                self.health = Some(health);
                self.health_error = None;
            }
            Msg::Health(Err(error)) => self.health_error = Some(error),
            Msg::Repositories(Ok(items)) => {
                self.repositories.items = items;
                self.repositories.error = None;
            }
            Msg::Repositories(Err(error)) => self.repositories.error = Some(error),
            Msg::RepositoryAdded(result) => {
                self.repositories.busy = false;
                match result {
                    Ok(repository) => {
                        self.notify(true, format!("Enrolled {}.", repository.root.display()));
                        self.repositories.path_input.clear();
                        self.backend.refresh_repositories();
                    }
                    Err(error) => self.repositories.error = Some(error),
                }
            }
            Msg::RepositoryForgotten(result) => {
                self.repositories.busy = false;
                match result {
                    Ok(id) => {
                        if self.compose.repository == Some(id) {
                            self.compose.repository = None;
                        }
                        self.notify(true, "Forgot the folder. Nothing on disk changed.");
                        self.backend.refresh_repositories();
                    }
                    Err(error) => self.repositories.error = Some(error),
                }
            }
            Msg::Cases(Ok(cases)) => {
                self.cases = cases;
                self.cases_error = None;
                // A window opened onto existing work shows the newest case
                // rather than an empty pane.
                if self.selected.is_none() {
                    if let Some(newest) = self.cases.first() {
                        self.select_case(newest.id);
                    }
                }
            }
            Msg::Cases(Err(error)) => self.cases_error = Some(error),
            Msg::Case(result) => {
                self.detail_busy = false;
                match result {
                    Ok(detail) => {
                        if Some(detail.id) == self.selected {
                            self.sync_answer_drafts(&detail);
                            self.detail = Some(*detail);
                            self.detail_error = None;
                        }
                    }
                    Err(error) => self.detail_error = Some(error),
                }
            }
            Msg::Submitted(result) => {
                self.compose.busy = false;
                match result {
                    Ok(id) => {
                        self.compose.text.clear();
                        self.compose.task_hint.clear();
                        self.compose.message_count = 0;
                        self.compose.error = None;
                        self.notify(true, "Submitted. Refinery is working on it.");
                        self.select_case(id);
                        self.tab = Tab::Cases;
                        self.backend.refresh_cases();
                    }
                    Err(error) => self.compose.error = Some(error),
                }
            }
            Msg::Answered(result) => {
                self.answers.busy = false;
                match result {
                    Ok(id) => {
                        self.notify(true, "Answers recorded. Refinery is continuing.");
                        self.answers.error = None;
                        self.reload_case(id);
                    }
                    Err(error) => self.answers.error = Some(error),
                }
            }
            Msg::Cancelled(result) => match result {
                Ok(id) => {
                    self.notify(true, "Cancelled.");
                    self.reload_case(id);
                    self.backend.refresh_cases();
                }
                Err(error) => self.notify(false, error),
            },
            Msg::RetryRequested(result) => match result {
                Ok(id) => {
                    self.notify(true, "Delivery retry requested.");
                    self.reload_case(id);
                }
                Err(error) => self.notify(false, error),
            },
            Msg::KeyStored(result) => {
                self.key.busy = false;
                self.key.input.clear();
                match result {
                    Ok(outcome) => {
                        self.key.message = Some(match outcome.verified {
                            Ok(summary) => (
                                true,
                                format!(
                                    "Key stored in the {}. Connected to {summary}.",
                                    outcome.backend
                                ),
                            ),
                            Err(error) => (
                                false,
                                format!(
                                    "Key stored in the {}, but the connection test failed: {error}",
                                    outcome.backend
                                ),
                            ),
                        });
                        self.backend.refresh_health();
                    }
                    Err(error) => {
                        self.key.message =
                            Some((false, format!("Could not store the key: {error}")));
                    }
                }
            }
            Msg::KeyTested(result) => {
                self.key.busy = false;
                self.key.message = Some(match result {
                    Ok(summary) => (true, format!("Connected to {summary}.")),
                    Err(error) => (false, format!("Connection test failed: {error}")),
                });
            }
            Msg::KeyRemoved(result) => {
                self.key.busy = false;
                self.key.message = Some(match result {
                    Ok(()) => (true, "The key was removed.".to_owned()),
                    Err(error) => (false, format!("Could not remove the key: {error}")),
                });
                self.backend.refresh_health();
            }
        }
    }

    fn select_case(&mut self, id: CaseId) {
        if self.selected != Some(id) {
            self.selected = Some(id);
            self.detail = None;
            self.detail_error = None;
            self.answers = Answers::default();
        }
        self.reload_case(id);
    }

    fn reload_case(&mut self, id: CaseId) {
        self.detail_busy = true;
        self.last_detail = Some(Instant::now());
        self.backend.load_case(id);
    }

    /// Keep answer drafts aligned with whichever question is pending now.
    fn sync_answer_drafts(&mut self, detail: &CaseDetail) {
        let Some(pending) = &detail.pending_question else {
            self.answers = Answers::default();
            return;
        };
        if self.answers.request == Some(pending.id) {
            return;
        }
        let mut drafts = HashMap::new();
        for question in &pending.questions {
            let draft = match question.response_type {
                ResponseType::FreeText => AnswerDraft::Text(String::new()),
                ResponseType::SingleChoice => AnswerDraft::Choice(None),
                ResponseType::MultipleChoice => AnswerDraft::Choices(BTreeSet::new()),
            };
            drafts.insert(question.id, draft);
        }
        self.answers = Answers {
            request: Some(pending.id),
            drafts,
            busy: false,
            error: None,
        };
    }

    /* Polling ----------------------------------------------------------------- */

    fn poll(&mut self) {
        let now = Instant::now();
        if self
            .last_health
            .is_none_or(|last| now.duration_since(last) >= HEALTH_INTERVAL)
        {
            self.last_health = Some(now);
            self.backend.refresh_health();
        }
        if self
            .last_cases
            .is_none_or(|last| now.duration_since(last) >= CASES_INTERVAL)
        {
            self.last_cases = Some(now);
            self.backend.refresh_cases();
        }
        if let Some(id) = self.selected {
            let interval = match &self.detail {
                Some(detail) if detail.state.is_terminal() => IDLE_CASE_INTERVAL,
                _ => ACTIVE_CASE_INTERVAL,
            };
            if !self.detail_busy
                && self
                    .last_detail
                    .is_none_or(|last| now.duration_since(last) >= interval)
            {
                self.reload_case(id);
            }
        }
        if let Some(notice) = &self.notice {
            if now.duration_since(notice.shown_at) >= NOTICE_LIFETIME {
                self.notice = None;
            }
        }
    }

    /* Actions ----------------------------------------------------------------- */

    fn apply_actions(&mut self, ctx: &egui::Context) {
        let actions = std::mem::take(&mut self.actions);
        for action in actions {
            match action {
                Action::SelectCase(id) => self.select_case(id),
                Action::Submit => self.submit(),
                Action::SubmitAnswers => self.submit_answers(),
                Action::Cancel(id) => self.backend.cancel(id),
                Action::Retry(id) => self.backend.retry_delivery(id),
                Action::AddRepository(path) => {
                    self.repositories.busy = true;
                    self.repositories.error = None;
                    self.backend.add_repository(path);
                }
                Action::ForgetRepository(id) => {
                    self.repositories.busy = true;
                    self.repositories.error = None;
                    self.backend.forget_repository(id);
                }
                Action::StoreKey => {
                    let key = self.key.input.trim().to_owned();
                    if key.is_empty() {
                        self.key.message = Some((false, "Paste a key first.".to_owned()));
                    } else {
                        self.key.busy = true;
                        self.key.message = None;
                        self.backend.store_api_key(key);
                    }
                }
                Action::TestKey => {
                    self.key.busy = true;
                    self.key.message = None;
                    self.backend.test_api_key();
                }
                Action::RemoveKey => {
                    self.key.busy = true;
                    self.key.message = None;
                    self.backend.remove_api_key();
                }
                Action::Copy(text) => {
                    ctx.copy_text(text);
                    self.notify(true, "Copied to the clipboard.");
                }
                Action::SaveMarkdown(name, text) => {
                    let chosen = rfd::FileDialog::new()
                        .set_title("Save the refined prompt")
                        .set_file_name(name)
                        .add_filter("Markdown", &["md"])
                        .save_file();
                    if let Some(path) = chosen {
                        match std::fs::write(&path, text) {
                            Ok(()) => self.notify(true, format!("Saved {}.", path.display())),
                            Err(error) => self.notify(
                                false,
                                format!("Could not write {}: {error}", path.display()),
                            ),
                        }
                    }
                }
                Action::OpenInterface => {
                    ctx.open_url(egui::OpenUrl::new_tab(self.backend.interface_url()));
                }
                Action::Refresh => {
                    self.backend.refresh_health();
                    self.backend.refresh_cases();
                    self.backend.refresh_repositories();
                    if let Some(id) = self.selected {
                        self.reload_case(id);
                    }
                }
            }
        }
    }

    fn submit(&mut self) {
        let Some(transcript) = transcript::parse(&self.compose.text) else {
            self.compose.error = Some("Paste a transcript or some feedback first.".to_owned());
            return;
        };
        let id = uuid::Uuid::new_v4();
        let request_id = format!("desktop:{id}");
        let destination = match self.compose.destination {
            Some(DestinationChoice::Overlord) => {
                match &self.backend.config().settings.overlord.base_url {
                    Some(base_url) => Destination::Overlord(OverlordDestination {
                        base_url: base_url.clone(),
                        mission_id: None,
                        objective_id: None,
                        bearer_token: None,
                    }),
                    None => {
                        self.compose.error =
                            Some("No Overlord destination is configured.".to_owned());
                        return;
                    }
                }
            }
            _ => Destination::LocalExport(LocalExportDestination {
                path: self
                    .backend
                    .export_dir()
                    .join(format!("{id}.json"))
                    .display()
                    .to_string(),
            }),
        };
        let task_hint = self.compose.task_hint.trim();
        let mut metadata = BTreeMap::new();
        metadata.insert("origin".to_owned(), INSTANCE.to_owned());
        let request = RefinementRequest {
            schema_version: SchemaVersion::CURRENT,
            request_id,
            idempotency_key: format!("desktop-{id}"),
            source: SourceRef {
                system: SourceSystem::LocalUi,
                instance: INSTANCE.to_owned(),
                callback: None,
            },
            transcript,
            task_hint: (!task_hint.is_empty()).then(|| task_hint.to_owned()),
            attachments: Vec::new(),
            repository: self.compose.repository,
            destination,
            metadata,
        };
        if let Err(report) = request.validate() {
            let issues: Vec<String> = report
                .issues
                .iter()
                .map(|issue| format!("{}: {}", issue.field, issue.message))
                .collect();
            self.compose.error = Some(issues.join("; "));
            return;
        }
        self.compose.busy = true;
        self.compose.error = None;
        self.backend.submit(request);
    }

    fn submit_answers(&mut self) {
        let Some(detail) = &self.detail else { return };
        let Some(pending) = &detail.pending_question else {
            return;
        };
        let now = chrono::Utc::now();
        let mut answers = Vec::new();
        let mut missing = Vec::new();
        for question in &pending.questions {
            let value = match self.answers.drafts.get(&question.id) {
                Some(AnswerDraft::Text(text)) if !text.trim().is_empty() => {
                    Some(AnswerValue::Text {
                        text: text.trim().to_owned(),
                    })
                }
                Some(AnswerDraft::Choice(Some(value))) => Some(AnswerValue::Choice {
                    value: value.clone(),
                }),
                Some(AnswerDraft::Choices(values)) if !values.is_empty() => {
                    Some(AnswerValue::Choices {
                        values: values.iter().cloned().collect(),
                    })
                }
                _ => None,
            };
            match value {
                Some(value) => answers.push(Answer {
                    question_id: question.id,
                    value,
                    answered_at: now,
                    answered_by: INSTANCE.to_owned(),
                }),
                None if question.required => missing.push(question.label.clone()),
                None => {}
            }
        }
        if !missing.is_empty() {
            self.answers.error = Some(format!("Still needed: {}", missing.join(", ")));
            return;
        }
        let set = AnswerSet {
            schema_version: SchemaVersion::CURRENT,
            case_id: detail.id,
            question_request_id: pending.id,
            answers,
        };
        self.answers.busy = true;
        self.answers.error = None;
        self.backend.answer(set);
    }
}

/* Drawing ----------------------------------------------------------------- */

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain_messages();
        self.poll();
        let ctx = ui.ctx().clone();

        egui::Panel::top("header").show(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading("Refinery");
                ui.add_space(12.0);
                for tab in Tab::ALL {
                    if ui.selectable_label(self.tab == tab, tab.label()).clicked() {
                        self.tab = tab;
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("Refresh").clicked() {
                        self.actions.push(Action::Refresh);
                    }
                    ui.add_space(8.0);
                    self.status_line(ui);
                });
            });
            ui.add_space(6.0);
        });

        if let Some(notice) = &self.notice {
            let (ok, text) = (notice.ok, notice.text.clone());
            egui::Panel::top("notice").show(ui, |ui| {
                ui.add_space(4.0);
                let color = if ok { GOOD } else { BAD };
                ui.colored_label(color, text);
                ui.add_space(4.0);
            });
        }

        if self.tab == Tab::Cases {
            egui::Panel::left("cases")
                .default_size(300.0)
                .min_size(220.0)
                .show(ui, |ui| self.case_list(ui));
        }

        egui::CentralPanel::default().show(ui, |ui| match self.tab {
            Tab::Compose => self.compose_tab(ui),
            Tab::Cases => self.case_tab(ui),
            Tab::Repositories => self.repositories_tab(ui),
            Tab::Settings => self.settings_tab(ui),
        });

        self.apply_actions(&ctx);
        ctx.request_repaint_after(Duration::from_secs(1));
    }
}

const GOOD: Color32 = Color32::from_rgb(0x2e, 0x9e, 0x5b);
const BAD: Color32 = Color32::from_rgb(0xd0, 0x3b, 0x3b);
const WARN: Color32 = Color32::from_rgb(0xd9, 0x8a, 0x1c);
const BUSY: Color32 = Color32::from_rgb(0x2f, 0x6f, 0xd6);

fn state_color(state: CaseState) -> Color32 {
    match state {
        CaseState::Completed | CaseState::Ready => GOOD,
        CaseState::Failed => BAD,
        CaseState::AwaitingAnswer => WARN,
        CaseState::Cancelled => Color32::GRAY,
        _ => BUSY,
    }
}

fn state_label(state: CaseState) -> &'static str {
    match state {
        CaseState::Received => "Received",
        CaseState::Preparing => "Preparing",
        CaseState::Running => "Running",
        CaseState::AwaitingAnswer => "Needs your answer",
        CaseState::Validating => "Validating",
        CaseState::Ready => "Ready",
        CaseState::Delivering => "Delivering",
        CaseState::Completed => "Completed",
        CaseState::Failed => "Failed",
        CaseState::Cancelled => "Cancelled",
    }
}

fn local_time(time: chrono::DateTime<chrono::Utc>) -> String {
    time.with_timezone(&chrono::Local)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

fn short_time(time: chrono::DateTime<chrono::Utc>) -> String {
    let local = time.with_timezone(&chrono::Local);
    if local.date_naive() == chrono::Local::now().date_naive() {
        local.format("%H:%M").to_string()
    } else {
        local.format("%b %d").to_string()
    }
}

/// A short title for a case in a list: the first line of its first message.
fn case_title(detail: Option<&CaseDetail>, summary: &CaseSummary) -> String {
    if let Some(detail) = detail {
        if let Some(output) = &detail.output {
            return output.prompt.title.clone();
        }
        if let Some(hint) = &detail.request.task_hint {
            return hint.clone();
        }
        if let Some(first) = detail.request.transcript.messages.first() {
            let line = first.content.lines().next().unwrap_or_default();
            return truncate(line, 60);
        }
    }
    let id = summary.id.to_string();
    format!("Case {}", &id[id.len().saturating_sub(6)..])
}

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_owned()
    } else {
        let mut out: String = text.chars().take(max_chars.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

fn read_only(ui: &mut egui::Ui, id: &str, text: &str, rows: usize) {
    let mut view: &str = text;
    ui.add(
        egui::TextEdit::multiline(&mut view)
            .id_salt(id)
            .desired_width(f32::INFINITY)
            .desired_rows(rows)
            .font(egui::TextStyle::Monospace),
    );
}

impl App {
    fn status_line(&self, ui: &mut egui::Ui) {
        match (&self.health, &self.health_error) {
            (Some(health), _) => {
                let key = if health.provider_credential {
                    ("Gemini key set", GOOD)
                } else {
                    ("No Gemini key", WARN)
                };
                ui.colored_label(key.1, key.0);
                ui.separator();
                let mode = match self.backend.mode() {
                    DaemonMode::Embedded => "service running in this window",
                    DaemonMode::External => "attached to the running service",
                };
                let color = if health.status == "ok" { GOOD } else { WARN };
                ui.colored_label(color, format!("{mode} · v{}", health.version));
            }
            (None, Some(error)) => {
                ui.colored_label(BAD, format!("service unreachable: {error}"));
            }
            (None, None) => {
                ui.weak("connecting…");
            }
        }
    }

    /* Compose ----------------------------------------------------------------- */

    fn compose_tab(&mut self, ui: &mut egui::Ui) {
        egui::Panel::right("compose_options")
            .resizable(false)
            .exact_size(320.0)
            .show(ui, |ui| {
                ui.add_space(4.0);
                ui.heading("Options");
                ui.add_space(8.0);

                ui.label("Task hint (optional)");
                ui.add(
                    egui::TextEdit::multiline(&mut self.compose.task_hint)
                        .desired_rows(3)
                        .desired_width(f32::INFINITY)
                        .hint_text("One line on what the prompt is for"),
                );
                ui.add_space(8.0);

                ui.label("Repository the agent may read");
                let selected_label = self
                    .compose
                    .repository
                    .and_then(|id| self.repositories.items.iter().find(|item| item.id == id))
                    .map(|item| item.label.clone())
                    .unwrap_or_else(|| "None".to_owned());
                egui::ComboBox::from_id_salt("compose_repository")
                    .width(300.0)
                    .selected_text(selected_label)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.compose.repository, None, "None");
                        for item in &self.repositories.items {
                            ui.selectable_value(
                                &mut self.compose.repository,
                                Some(item.id),
                                format!("{} — {}", item.label, item.root.display()),
                            );
                        }
                    });
                if self.repositories.items.is_empty() {
                    ui.weak("Enroll a folder under Repositories to ground the prompt in code.");
                }
                ui.add_space(8.0);

                ui.label("When the prompt is ready");
                ui.radio_value(
                    &mut self.compose.destination,
                    Some(DestinationChoice::LocalExport),
                    "Keep it here to copy or save",
                );
                let overlord = self.backend.config().settings.overlord.base_url.clone();
                match overlord {
                    Some(base_url) => {
                        ui.radio_value(
                            &mut self.compose.destination,
                            Some(DestinationChoice::Overlord),
                            format!("Also send it to Overlord at {base_url}"),
                        );
                    }
                    None => {
                        ui.add_enabled(
                            false,
                            egui::RadioButton::new(false, "Also send it to Overlord (not configured)"),
                        );
                    }
                }
                ui.add_space(12.0);

                let ready = !self.compose.busy && !self.compose.text.trim().is_empty();
                let label = if self.compose.busy {
                    "Submitting…"
                } else {
                    "Refine this"
                };
                if ui
                    .add_enabled(ready, egui::Button::new(RichText::new(label).strong()))
                    .clicked()
                {
                    self.actions.push(Action::Submit);
                }
                if let Some(error) = &self.compose.error {
                    ui.add_space(6.0);
                    ui.colored_label(BAD, error);
                }
                if self.health.as_ref().is_some_and(|health| !health.provider_credential) {
                    ui.add_space(6.0);
                    ui.colored_label(
                        WARN,
                        "No Gemini key is stored. The case will wait until one is set under Settings.",
                    );
                }
            });

        egui::CentralPanel::default().show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.heading("Transcript");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let chars = self.compose.text.chars().count();
                    let messages = self.compose.message_count;
                    ui.weak(format!(
                        "{chars} characters · {messages} message{}",
                        if messages == 1 { "" } else { "s" }
                    ));
                });
            });
            ui.weak(
                "Paste a conversation, a thread, or plain feedback. Lines starting with \
                 “User:”, “Assistant:”, or “System:” become separate messages.",
            );
            ui.add_space(6.0);
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let response = ui.add_sized(
                        ui.available_size(),
                        egui::TextEdit::multiline(&mut self.compose.text)
                            .id_salt("compose_text")
                            .desired_width(f32::INFINITY)
                            .hint_text("Paste the transcript here…"),
                    );
                    if response.changed() {
                        self.compose.message_count = transcript::parse(&self.compose.text)
                            .map(|transcript| transcript.messages.len())
                            .unwrap_or_default();
                    }
                });
        });
    }

    /* Cases ------------------------------------------------------------------- */

    fn case_list(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.heading("Cases");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("New").clicked() {
                    self.tab = Tab::Compose;
                }
            });
        });
        if let Some(error) = &self.cases_error {
            ui.colored_label(BAD, error);
        }
        if self.cases.is_empty() {
            ui.add_space(8.0);
            ui.weak("Nothing yet. Compose a transcript to start a case.");
            return;
        }
        ui.add_space(4.0);
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for summary in &self.cases {
                    let selected = self.selected == Some(summary.id);
                    let detail = self
                        .detail
                        .as_ref()
                        .filter(|detail| detail.id == summary.id);
                    let title = case_title(detail, summary);
                    let text = format!(
                        "{}\n{}  ·  {}",
                        truncate(&title, 48),
                        state_label(summary.state),
                        short_time(summary.updated_at)
                    );
                    let rich = RichText::new(text).color(if selected {
                        ui.visuals().strong_text_color()
                    } else {
                        state_color(summary.state)
                    });
                    if ui.selectable_label(selected, rich).clicked() {
                        self.actions.push(Action::SelectCase(summary.id));
                    }
                }
            });
    }

    fn case_tab(&mut self, ui: &mut egui::Ui) {
        let Some(selected) = self.selected else {
            ui.add_space(24.0);
            ui.vertical_centered(|ui| {
                ui.weak("Select a case on the left, or compose a new one.");
            });
            return;
        };
        if let Some(error) = &self.detail_error {
            ui.colored_label(BAD, error);
        }
        let Some(detail) = self.detail.clone() else {
            ui.weak(format!("Loading case {selected}…"));
            return;
        };

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.colored_label(
                        state_color(detail.state),
                        RichText::new(state_label(detail.state)).heading(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if !detail.state.is_terminal() && ui.button("Cancel case").clicked() {
                            self.actions.push(Action::Cancel(detail.id));
                        }
                    });
                });
                ui.weak(format!(
                    "Started {} · updated {} · {}",
                    local_time(detail.created_at),
                    local_time(detail.updated_at),
                    detail.request_id
                ));
                if let Some(repository) = detail.request.repository {
                    let label = self
                        .repositories
                        .items
                        .iter()
                        .find(|item| item.id == repository)
                        .map(|item| item.root.display().to_string())
                        .unwrap_or_else(|| repository.to_string());
                    ui.weak(format!("Repository: {label}"));
                }
                if let Some(hint) = &detail.request.task_hint {
                    ui.label(format!("Task hint: {hint}"));
                }
                ui.add_space(8.0);

                if let Some(pending) = &detail.pending_question {
                    self.question_form(ui, pending);
                    ui.add_space(12.0);
                }

                if let Some(output) = &detail.output {
                    self.output_section(ui, &detail, output);
                    ui.add_space(12.0);
                } else if detail.state.has_active_work() || detail.state == CaseState::Received {
                    ui.colored_label(
                        BUSY,
                        "Refinery is working. The prompt appears here when it is ready.",
                    );
                    ui.add_space(12.0);
                } else if detail.state == CaseState::Failed {
                    ui.colored_label(
                        BAD,
                        "The case failed before producing a prompt. The history below says why.",
                    );
                    ui.add_space(12.0);
                }

                if !detail.deliveries.is_empty() {
                    self.deliveries_section(ui, &detail);
                    ui.add_space(12.0);
                }

                egui::CollapsingHeader::new(format!("History ({} events)", detail.event_count))
                    .default_open(detail.output.is_none())
                    .show(ui, |ui| {
                        for event in &detail.events {
                            ui.horizontal_wrapped(|ui| {
                                ui.monospace(short_time(event.occurred_at));
                                ui.label(event_line(&event.payload));
                            });
                        }
                    });

                egui::CollapsingHeader::new(format!(
                    "Transcript ({} messages)",
                    detail.request.transcript.messages.len()
                ))
                .default_open(false)
                .show(ui, |ui| {
                    for message in &detail.request.transcript.messages {
                        ui.strong(format!("{:?}", message.role));
                        ui.label(&message.content);
                        ui.add_space(4.0);
                    }
                });
                ui.add_space(16.0);
            });
    }

    fn question_form(&mut self, ui: &mut egui::Ui, pending: &crate::domain::QuestionRequest) {
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.colored_label(WARN, RichText::new("Refinery needs an answer").strong());
            ui.add_space(4.0);
            ui.label(&pending.prompt);
            ui.add_space(8.0);
            for question in &pending.questions {
                let label = if question.required {
                    format!("{} *", question.label)
                } else {
                    question.label.clone()
                };
                ui.strong(label);
                if let Some(description) = &question.description {
                    ui.weak(description);
                }
                let Some(draft) = self.answers.drafts.get_mut(&question.id) else {
                    continue;
                };
                match draft {
                    AnswerDraft::Text(text) => {
                        ui.add(
                            egui::TextEdit::multiline(text)
                                .id_salt(question.id)
                                .desired_rows(3)
                                .desired_width(f32::INFINITY),
                        );
                    }
                    AnswerDraft::Choice(selected) => {
                        for choice in &question.choices {
                            let label = choice.label.as_deref().unwrap_or(&choice.value);
                            ui.radio_value(selected, Some(choice.value.clone()), label);
                        }
                    }
                    AnswerDraft::Choices(selected) => {
                        for choice in &question.choices {
                            let label = choice.label.as_deref().unwrap_or(&choice.value);
                            let mut checked = selected.contains(&choice.value);
                            if ui.checkbox(&mut checked, label).changed() {
                                if checked {
                                    selected.insert(choice.value.clone());
                                } else {
                                    selected.remove(&choice.value);
                                }
                            }
                        }
                    }
                }
                ui.add_space(8.0);
            }
            let label = if self.answers.busy {
                "Sending…"
            } else {
                "Send answers"
            };
            if ui
                .add_enabled(!self.answers.busy, egui::Button::new(label))
                .clicked()
            {
                self.actions.push(Action::SubmitAnswers);
            }
            if let Some(error) = &self.answers.error {
                ui.colored_label(BAD, error);
            }
        });
    }

    fn output_section(
        &mut self,
        ui: &mut egui::Ui,
        detail: &CaseDetail,
        output: &crate::storage::StoredOutput,
    ) {
        let markdown = prompt_text::markdown(&output.prompt);
        ui.horizontal(|ui| {
            ui.heading(&output.prompt.title);
        });
        if !output.valid {
            ui.colored_label(
                WARN,
                format!(
                    "This output did not pass validation: {}",
                    output.validation_issues.join("; ")
                ),
            );
        }
        ui.horizontal(|ui| {
            if ui.button("Copy prompt").clicked() {
                self.actions
                    .push(Action::Copy(output.prompt.prompt.clone()));
            }
            if ui.button("Copy everything as Markdown").clicked() {
                self.actions.push(Action::Copy(markdown.clone()));
            }
            if ui.button("Save as…").clicked() {
                let name = format!("{}.md", slug(&output.prompt.title));
                self.actions
                    .push(Action::SaveMarkdown(name, markdown.clone()));
            }
            ui.weak(format!("produced {}", local_time(output.created_at)));
        });
        ui.add_space(6.0);
        read_only(ui, &format!("output-{}", detail.id), &markdown, 18);
    }

    fn deliveries_section(&mut self, ui: &mut egui::Ui, detail: &CaseDetail) {
        ui.strong("Deliveries");
        let mut failed = false;
        for delivery in &detail.deliveries {
            let color = match delivery.status.as_str() {
                "succeeded" | "delivered" | "completed" => GOOD,
                "failed" => BAD,
                _ => BUSY,
            };
            ui.horizontal_wrapped(|ui| {
                ui.colored_label(color, &delivery.status);
                ui.label(format!(
                    "{} · attempt {} · {}",
                    delivery.destination_kind,
                    delivery.attempt,
                    local_time(delivery.updated_at)
                ));
            });
            if let Some(error) = &delivery.error {
                ui.colored_label(BAD, error);
            }
            failed |= delivery.status == "failed";
        }
        if let Destination::LocalExport(export) = &detail.request.destination {
            ui.weak(format!("Export file: {}", export.path));
        }
        if failed && detail.state.is_terminal() && ui.button("Retry delivery").clicked() {
            self.actions.push(Action::Retry(detail.id));
        }
    }

    /* Repositories ------------------------------------------------------------ */

    fn repositories_tab(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.heading("Enrolled folders");
        ui.weak(
            "Refinery reads an enrolled folder to ground a prompt in its code. \
             Reads are bounded by the installation's limits and never modify anything.",
        );
        ui.add_space(8.0);

        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.strong("Enroll a folder");
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.repositories.path_input)
                        .desired_width(460.0)
                        .hint_text("/path/to/a/repository"),
                );
                if ui.button("Choose…").clicked() {
                    if let Some(path) = rfd::FileDialog::new()
                        .set_title("Choose a folder to enroll")
                        .pick_folder()
                    {
                        self.repositories.path_input = path.display().to_string();
                    }
                }
                let ready =
                    !self.repositories.busy && !self.repositories.path_input.trim().is_empty();
                if ui.add_enabled(ready, egui::Button::new("Enroll")).clicked() {
                    let path = PathBuf::from(self.repositories.path_input.trim());
                    self.actions.push(Action::AddRepository(path));
                }
            });
            if let Some(error) = &self.repositories.error {
                ui.colored_label(BAD, error);
            }
        });
        ui.add_space(12.0);

        if self.repositories.items.is_empty() {
            ui.weak("No folders are enrolled yet.");
            return;
        }
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for item in &self.repositories.items {
                    egui::Frame::group(ui.style()).show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.vertical(|ui| {
                                ui.strong(&item.label);
                                ui.monospace(item.root.display().to_string());
                                ui.weak(if item.is_git_work_tree {
                                    "Git work tree"
                                } else {
                                    "Plain folder"
                                });
                            });
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .add_enabled(
                                            !self.repositories.busy,
                                            egui::Button::new("Forget"),
                                        )
                                        .clicked()
                                    {
                                        self.actions.push(Action::ForgetRepository(item.id));
                                    }
                                },
                            );
                        });
                    });
                }
            });
    }

    /* Settings ---------------------------------------------------------------- */

    fn settings_tab(&mut self, ui: &mut egui::Ui) {
        let config = self.backend.config();
        let provider = config.settings.provider.clone();
        let data_dir = config.data_dir.root().display().to_string();
        let listen = config.settings.api.bind_address().to_string();
        let overlord = config.settings.overlord.base_url.clone();

        ui.add_space(4.0);
        ui.heading("Gemini API key");
        ui.weak(
            "The key is kept in the operating system's credential store and never written \
             to a settings file or a log.",
        );
        ui.add_space(6.0);
        egui::Frame::group(ui.style()).show(ui, |ui| {
            match &self.health {
                Some(health) if health.provider_credential => {
                    ui.colored_label(GOOD, "A key is stored.");
                }
                Some(_) => {
                    ui.colored_label(WARN, "No key is stored.");
                }
                None => {
                    ui.weak("Checking…");
                }
            }
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.key.input)
                        .password(true)
                        .desired_width(420.0)
                        .hint_text("Paste a new key"),
                );
                let ready = !self.key.busy && !self.key.input.trim().is_empty();
                if ui
                    .add_enabled(ready, egui::Button::new("Save and test"))
                    .clicked()
                {
                    self.actions.push(Action::StoreKey);
                }
            });
            ui.horizontal(|ui| {
                let stored = self
                    .health
                    .as_ref()
                    .is_some_and(|health| health.provider_credential);
                if ui
                    .add_enabled(
                        !self.key.busy && stored,
                        egui::Button::new("Test stored key"),
                    )
                    .clicked()
                {
                    self.actions.push(Action::TestKey);
                }
                if ui
                    .add_enabled(!self.key.busy && stored, egui::Button::new("Remove key"))
                    .clicked()
                {
                    self.actions.push(Action::RemoveKey);
                }
                if self.key.busy {
                    ui.weak("Working…");
                }
            });
            if let Some((ok, message)) = &self.key.message {
                ui.colored_label(if *ok { GOOD } else { BAD }, message);
            }
        });
        ui.add_space(16.0);

        ui.heading("This installation");
        egui::Grid::new("installation")
            .num_columns(2)
            .spacing([16.0, 6.0])
            .show(ui, |ui| {
                ui.weak("Provider");
                ui.label(format!("{} · {}", provider.backend, provider.model));
                ui.end_row();
                ui.weak("Service");
                ui.label(match self.backend.mode() {
                    DaemonMode::Embedded => {
                        format!("running inside this window on {listen}")
                    }
                    DaemonMode::External => format!("attached to the service on {listen}"),
                });
                ui.end_row();
                ui.weak("Data directory");
                ui.monospace(&data_dir);
                ui.end_row();
                ui.weak("Overlord destination");
                ui.label(
                    overlord.unwrap_or_else(|| "not configured (run `refinery setup`)".into()),
                );
                ui.end_row();
                if let Some(health) = &self.health {
                    ui.weak("Cases");
                    let counts: Vec<String> = health
                        .cases_by_state
                        .iter()
                        .filter(|(_, count)| **count > 0)
                        .map(|(state, count)| format!("{count} {state}"))
                        .collect();
                    ui.label(if counts.is_empty() {
                        "none".to_owned()
                    } else {
                        counts.join(", ")
                    });
                    ui.end_row();
                    if !health.integrity_problems.is_empty() {
                        ui.weak("Integrity");
                        ui.colored_label(BAD, health.integrity_problems.join("; "));
                        ui.end_row();
                    }
                }
            });
        ui.add_space(12.0);
        if ui.button("Open the browser interface").clicked() {
            self.actions.push(Action::OpenInterface);
        }
        ui.weak("Logs, health, and the full case history are also in the browser interface.");
    }
}

fn slug(title: &str) -> String {
    let mut out = String::new();
    for ch in title.chars() {
        if ch.is_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "refined-prompt".to_owned()
    } else {
        truncate(trimmed, 60)
    }
}

/// One line of history for a person, from an event's payload.
fn event_line(payload: &CaseEventPayload) -> String {
    match payload {
        CaseEventPayload::CaseReceived {
            source_system,
            has_repository,
            ..
        } => format!(
            "Received from {source_system}{}",
            if *has_repository {
                " with a repository"
            } else {
                ""
            }
        ),
        CaseEventPayload::StateChanged { to, .. } => state_label(*to).to_owned(),
        CaseEventPayload::AttachmentImported { media_type, .. } => {
            format!("Imported a {media_type} attachment")
        }
        CaseEventPayload::AttachmentStateChanged { .. } => "Attachment state changed".to_owned(),
        CaseEventPayload::RepositoryToolInvoked {
            tool,
            result_bytes,
            outcome,
            ..
        } => format!(
            "Read the repository with {tool} ({result_bytes} bytes, {})",
            outcome_word(*outcome)
        ),
        CaseEventPayload::ProviderCallCompleted {
            step,
            usage,
            outcome,
            ..
        } => {
            let tokens = usage
                .as_ref()
                .and_then(|usage| usage.total_tokens)
                .map(|total| format!(", {total} tokens"))
                .unwrap_or_default();
            format!("Model call {step} {}{tokens}", outcome_word(*outcome))
        }
        CaseEventPayload::QuestionRequested { question_count, .. } => {
            format!("Asked {question_count} question(s)")
        }
        CaseEventPayload::QuestionForwarded {
            target, outcome, ..
        } => format!("Question sent to {target}: {}", outcome_word(*outcome)),
        CaseEventPayload::AnswersRecorded { answer_count, .. } => {
            format!("Recorded {answer_count} answer(s)")
        }
        CaseEventPayload::OutputRecorded { valid, issues, .. } => {
            if *valid {
                "Prompt produced and validated".to_owned()
            } else {
                format!("Prompt produced but invalid: {}", issues.join("; "))
            }
        }
        CaseEventPayload::DeliveryAttempted {
            destination,
            attempt,
            outcome,
            ..
        } => format!(
            "Delivery to {destination} attempt {attempt} {}",
            outcome_word(*outcome)
        ),
        CaseEventPayload::Note { message } => message.clone(),
    }
}

fn outcome_word(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Succeeded => "succeeded",
        Outcome::Failed => "failed",
        Outcome::Denied => "was denied",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_are_file_name_safe() {
        assert_eq!(
            slug("Resumable importer: phase 1!"),
            "resumable-importer-phase-1"
        );
        assert_eq!(slug("   "), "refined-prompt");
    }

    #[test]
    fn truncation_counts_characters_not_bytes() {
        assert_eq!(truncate("héllo wörld", 5), "héll…");
        assert_eq!(truncate("short", 10), "short");
    }
}
