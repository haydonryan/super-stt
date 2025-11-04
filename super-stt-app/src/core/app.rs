// SPDX-License-Identifier: GPL-3.0-only

use crate::audio::{parse_audio_level_from_udp, parse_recording_state_from_udp};

use crate::daemon::client::{
    cancel_download, fetch_daemon_config, get_current_device, get_current_model,
    get_download_status, get_preview_typing, list_available_models, load_audio_themes, ping_daemon,
    send_record_command, set_and_test_audio_theme, set_device, set_model, set_preview_typing,
    test_daemon_connection,
};
use crate::state::{AudioTheme, ContextPage, DaemonStatus, MenuAction, Page, RecordingStatus};
use crate::ui::messages::Message;
use crate::ui::views;
use cosmic::app::context_drawer;
use cosmic::iced::Subscription;
use cosmic::prelude::*;
use cosmic::widget::{icon, menu, nav_bar};
use futures_util::SinkExt;
use log::{info, warn};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use super_stt_shared::UdpAuth;
use super_stt_shared::stt_model::STTModel;
use tokio::net::UdpSocket;
use tokio::time::Duration;

/// Model loading/switching state with operation locking
#[derive(Debug, Clone, PartialEq)]
pub enum ModelState {
    Ready,
    Loading,
    /// Model is switching - prevent concurrent operations
    Switching(STTModel), // Target model being switched to
}

/// Device switching state
#[derive(Debug, Clone, PartialEq)]
pub enum DeviceState {
    Ready,
    Switching,
}

/// Download state
#[derive(Debug, Clone, PartialEq)]
pub enum DownloadState {
    Idle,
    Active,
}

/// The application model stores app-specific state used to describe its interface and
/// drive its logic.
pub struct AppModel {
    /// Application state which is managed by the COSMIC runtime.
    core: cosmic::Core,
    /// Display a context drawer with the designated page if defined.
    context_page: ContextPage,
    /// Contains items assigned to the nav bar panel.
    nav: nav_bar::Model,

    // Super STT specific state
    /// Socket path for daemon communication
    pub socket_path: PathBuf,
    /// UDP port for audio streaming
    pub udp_port: u16,
    /// Current daemon connection status
    pub daemon_status: DaemonStatus,
    /// Current recording status
    pub recording_status: RecordingStatus,
    /// Latest transcription text
    pub transcription_text: String,
    /// Current audio level (0.0 to 1.0)
    pub audio_level: f32,
    /// Whether speech is currently detected
    pub is_speech_detected: bool,
    /// Available audio themes
    pub audio_themes: Vec<AudioTheme>,
    /// Currently selected audio theme
    pub selected_audio_theme: AudioTheme,
    /// UDP restart counter for subscriptions
    pub udp_restart_counter: u64,
    /// Last UDP data timestamp
    pub last_udp_data: std::time::Instant,

    // Model management state
    /// Available models from daemon
    pub available_models: Vec<STTModel>,
    /// Currently loaded model
    pub current_model: STTModel,
    /// The model we had before starting a download (to revert to on cancel)
    pub previous_model: STTModel,
    /// Model loading state
    pub model_state: ModelState,

    // Device management state
    /// Current device (cpu/cuda) from daemon
    pub current_device: String,
    /// Available devices from daemon
    pub available_devices: Vec<String>,
    /// Device switching state
    pub device_state: DeviceState,

    // Download progress state
    /// Current download progress if any download is active
    pub download_progress: Option<super_stt_shared::models::protocol::DownloadProgress>,
    /// Download state
    pub download_state: DownloadState,

    // Preview typing state
    /// Whether preview typing is enabled (beta feature)
    pub preview_typing_enabled: bool,
}

/// Create a COSMIC application from the app model
impl cosmic::Application for AppModel {
    /// The async executor that will be used to run your application's commands.
    type Executor = cosmic::executor::Default;

    /// Data that your application receives to its init method.
    type Flags = ();

    /// Messages which the application and its widgets will emit.
    type Message = Message;

    /// Unique identifier in RDNN (reverse domain name notation) format.
    const APP_ID: &'static str = "ai.menjivar.super-stt-app";

    fn core(&self) -> &cosmic::Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut cosmic::Core {
        &mut self.core
    }

    /// Initializes the application with any given flags and startup commands.
    fn init(
        core: cosmic::Core,
        _flags: Self::Flags,
    ) -> (Self, Task<cosmic::Action<Self::Message>>) {
        // Create a nav bar with Super STT specific pages
        let mut nav = nav_bar::Model::default();

        nav.insert()
            .text("Settings")
            .data::<Page>(Page::Settings)
            .icon(icon::from_name("preferences-system-symbolic"))
            .activate();

        nav.insert()
            .text("Testing")
            .data::<Page>(Page::Testing)
            .icon(icon::from_name("view-grid-symbolic"));

        nav.insert()
            .text("Connection")
            .data::<Page>(Page::Connection)
            .icon(icon::from_name("help-about-symbolic"));

        // Construct the app model with the runtime's core.
        let mut app = AppModel {
            core,
            context_page: ContextPage::default(),
            nav,
            // Initialize Super STT state using proper socket path
            socket_path: super_stt_shared::validation::get_secure_socket_path(),
            udp_port: 8765,
            daemon_status: DaemonStatus::Disconnected,
            recording_status: RecordingStatus::Idle,
            transcription_text: String::new(),
            audio_level: 0.0,
            is_speech_detected: false,
            audio_themes: Vec::new(),
            selected_audio_theme: AudioTheme::default(),
            udp_restart_counter: 0,
            last_udp_data: std::time::Instant::now(),

            // Initialize model state
            available_models: Vec::new(),
            current_model: STTModel::default(), // Use default model before loading from daemon
            previous_model: STTModel::default(), // Use default model before loading from daemon
            model_state: ModelState::Loading,   // We're loading the initial model state

            // Initialize device state
            current_device: String::new(), // Empty until loaded from daemon
            available_devices: vec!["cpu".to_string()], // Default until loaded from daemon
            device_state: DeviceState::Ready,

            // Initialize download state
            download_progress: None,
            download_state: DownloadState::Idle,

            // Initialize preview typing state (disabled by default as beta feature)
            preview_typing_enabled: false,
        };

        // Create startup commands
        let title_command = app.update_title();

        // Load audio themes on startup (always available)
        let load_themes = Task::perform(load_audio_themes(app.socket_path.clone()), |themes| {
            cosmic::Action::App(Message::AudioThemesLoaded(themes))
        });

        // Try to ping the daemon on startup
        let initial_ping = Task::perform(ping_daemon(app.socket_path.clone()), |result| {
            cosmic::Action::App(match result {
                Ok(_) => Message::DaemonConnected,
                Err(e) => Message::DaemonError(e),
            })
        });

        // Load models on startup
        let load_models = Task::perform(
            async move {
                // Small delay to let daemon connection establish
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            },
            |()| cosmic::Action::App(Message::LoadModels),
        );

        (
            app,
            Task::batch([title_command, load_themes, initial_ping, load_models]),
        )
    }

    /// Elements to pack at the start of the header bar.
    fn header_start(&self) -> Vec<Element<'_, Self::Message>> {
        let menu_bar = menu::bar(vec![menu::Tree::with_children(
            menu::root("View").apply(Element::from),
            menu::items(
                &HashMap::new(),
                vec![menu::Item::Button("About", None, MenuAction::About)],
            ),
        )]);

        vec![menu_bar.into()]
    }

    /// Enables the COSMIC application to create a nav bar with this model.
    fn nav_model(&self) -> Option<&nav_bar::Model> {
        // Only show navigation when daemon is connected
        if self.daemon_status == DaemonStatus::Connected {
            Some(&self.nav)
        } else {
            None
        }
    }

    /// Display a context drawer if the context page is requested.
    fn context_drawer(&self) -> Option<context_drawer::ContextDrawer<'_, Self::Message>> {
        if !self.core.window.show_context {
            return None;
        }

        Some(match self.context_page {
            ContextPage::About => context_drawer::context_drawer(
                views::about::page(),
                Message::ToggleContextPage(ContextPage::About),
            )
            .title("About"),
        })
    }

    /// Describes the interface based on the current state of the application model.
    ///
    /// Application events will be processed through the view. Any messages emitted by
    /// events received by widgets will be passed to the update method.
    fn view(&self) -> Element<'_, Self::Message> {
        // Force Connection page when daemon is not connected
        if self.daemon_status != DaemonStatus::Connected {
            return views::connection::page(
                &self.daemon_status,
                self.socket_path.to_string_lossy().to_string(),
                self.udp_port,
            );
        }

        // When connected, show normal navigation
        let active_page = self
            .nav
            .data::<Page>(self.nav.active())
            .unwrap_or(&Page::Settings);

        match active_page {
            Page::Settings => views::settings::page(
                &self.audio_themes,
                &self.selected_audio_theme,
                &self.available_models,
                &self.current_model,
                self.download_progress.as_ref(),
                self.download_state == DownloadState::Active,
                &self.current_device,
                &self.available_devices,
                self.device_state == DeviceState::Switching,
                self.preview_typing_enabled,
            ),
            Page::Testing => views::testing::page(
                &self.recording_status,
                &self.transcription_text,
                self.audio_level,
                self.is_speech_detected,
            ),
            Page::Connection => views::connection::page(
                &self.daemon_status,
                self.socket_path.to_string_lossy().to_string(),
                self.udp_port,
            ),
        }
    }

    /// Register subscriptions for this application.
    ///
    /// Subscriptions are long-running async tasks running in the background which
    /// emit messages to the application through a channel. They are started at the
    /// beginning of the application, and persist through its lifetime.
    fn subscription(&self) -> Subscription<Self::Message> {
        // Connection monitoring constants
        const PING_INTERVAL_SECS: u64 = 5;

        Subscription::batch(vec![
            // UDP audio level streaming subscription with restart capability
            Subscription::run_with_id(
                self.udp_restart_counter,
                cosmic::iced::stream::channel(100, |mut channel| async move {
                    let socket = match UdpSocket::bind("127.0.0.1:0").await {
                        Ok(socket) => Arc::new(socket),
                        Err(e) => {
                            warn!("Failed to bind UDP socket: {e}");
                            futures_util::future::pending().await
                        }
                    };

                    // Register with daemon using authentication (use 'applet' to get continuous audio data like the applet)
                    let auth = match UdpAuth::new() {
                        Ok(auth) => auth,
                        Err(e) => {
                            warn!("Failed to initialize UDP authentication: {e}");
                            return;
                        }
                    };

                    let registration_msg = match auth.create_auth_message("applet") {
                        Ok(msg) => msg,
                        Err(e) => {
                            warn!("Failed to create authenticated registration message: {e}");
                            return;
                        }
                    };

                    if let Err(e) = socket
                        .send_to(registration_msg.as_bytes(), "127.0.0.1:8765")
                        .await
                    {
                        warn!("Failed to register with daemon: {e}");
                        return;
                    }

                    // Wait for registration confirmation
                    let mut reg_buffer = [0u8; 1024];
                    match tokio::time::timeout(
                        Duration::from_secs(5),
                        socket.recv_from(&mut reg_buffer),
                    )
                    .await
                    {
                        Ok(Ok((len, addr))) => {
                            let response = String::from_utf8_lossy(&reg_buffer[0..len]);
                            if response.starts_with("REGISTERED:") {
                                info!(
                                    "Successfully registered with daemon: {response} from {addr}"
                                );
                            } else {
                                warn!("Unexpected registration response: {response} from {addr}");
                            }
                        }
                        Ok(Err(e)) => {
                            warn!("Failed to receive registration response: {e}");
                        }
                        Err(_) => {
                            warn!("Registration response timeout");
                        }
                    }

                    // Periodic pings are handled by the existing PingTimeout subscription

                    let mut buffer = [0u8; 1024];
                    loop {
                        match socket.recv_from(&mut buffer).await {
                            Ok((len, addr)) => {
                                // Validate source address - only accept from localhost
                                if addr.ip() != std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
                                {
                                    warn!("Rejected UDP packet from unauthorized source: {addr}");
                                    continue;
                                }

                                // Validate packet size
                                if !(1..=1024).contains(&len) {
                                    warn!("Rejected UDP packet with invalid size: {len}");
                                    continue;
                                }

                                let data = buffer[..len].to_vec();
                                if channel.send(Message::UdpDataReceived(data)).await.is_err() {
                                    break;
                                }
                            }
                            Err(e) => {
                                warn!("UDP receive error: {e}");
                                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                            }
                        }
                    }
                }),
            ),
            // Periodic connection monitoring
            cosmic::iced::time::every(std::time::Duration::from_secs(PING_INTERVAL_SECS))
                .map(|_| Message::PingTimeout),
            // Periodic download progress check
            cosmic::iced::time::every(std::time::Duration::from_secs(2))
                .map(|_| Message::CheckDownloadStatus),
        ])
    }

    /// Handles messages emitted by the application and its widgets.
    ///
    /// Tasks may be returned for asynchronous execution of code in the background
    /// on the application's async runtime.
    #[allow(clippy::too_many_lines)]
    fn update(&mut self, message: Self::Message) -> Task<cosmic::Action<Self::Message>> {
        // Try daemon-related messages first
        if matches!(
            message,
            Message::ConnectToDaemon
                | Message::DaemonConnectionResult(_)
                | Message::DaemonConnected
                | Message::DaemonConfigReceived(_)
                | Message::DaemonError(_)
                | Message::RetryConnection
                | Message::RefreshDaemonStatus
                | Message::PingTimeout
        ) {
            return self.handle_daemon_messages(message);
        }

        // Try model-related messages
        if matches!(
            message,
            Message::LoadModels
                | Message::ModelSelected(_)
                | Message::ModelsLoaded { .. }
                | Message::AvailableModelsLoaded(_)
                | Message::CurrentModelLoaded(_)
                | Message::ModelChanged(_)
                | Message::ModelError(_)
        ) {
            return self.handle_model_messages(message);
        }

        // Try device-related messages
        if matches!(
            message,
            Message::DeviceSelected(_)
                | Message::DeviceLoaded(_)
                | Message::DeviceInfoLoaded(_, _)
                | Message::DeviceError(_)
        ) {
            return self.handle_device_messages(message);
        }

        // Try download-related messages
        if matches!(
            message,
            Message::DownloadProgressUpdate(_)
                | Message::CancelDownload
                | Message::DownloadCompleted(_)
                | Message::DownloadCancelled(_)
                | Message::DownloadError { .. }
                | Message::CheckDownloadStatus
                | Message::NoDownloadInProgress
        ) {
            return self.handle_download_messages(message);
        }

        // Try preview typing-related messages
        if matches!(
            message,
            Message::PreviewTypingToggled(_)
                | Message::PreviewTypingSettingLoaded(_)
                | Message::PreviewTypingError(_)
        ) {
            return self.handle_preview_typing_messages(message);
        }

        match message {
            // Original template messages
            Message::OpenRepositoryUrl => {
                _ = open::that_detached(views::about::REPOSITORY);
            }

            Message::ToggleContextPage(context_page) => {
                if self.context_page == context_page {
                    self.core.window.show_context = !self.core.window.show_context;
                } else {
                    self.context_page = context_page;
                    self.core.window.show_context = true;
                }
            }

            Message::LaunchUrl(url) => match open::that_detached(&url) {
                Ok(()) => {}
                Err(err) => {
                    eprintln!("failed to open {url:?}: {err}");
                }
            },

            // Super STT specific messages
            Message::StartRecording => {
                // Prevent starting recording if already recording
                if matches!(self.recording_status, RecordingStatus::Recording) {
                    return Task::none();
                }

                self.recording_status = RecordingStatus::Recording;
                return Task::perform(send_record_command(self.socket_path.clone()), |result| {
                    match result {
                        Ok(transcription) => {
                            cosmic::Action::App(Message::TranscriptionReceived(transcription))
                        }
                        Err(e) => cosmic::Action::App(Message::TranscriptionReceived(format!(
                            "Error: {e}"
                        ))),
                    }
                });
            }

            Message::StopRecording => {
                self.recording_status = RecordingStatus::Idle;
                self.audio_level = 0.0;
            }

            Message::TranscriptionReceived(text) => {
                self.transcription_text = text;
                self.recording_status = RecordingStatus::Idle;
                self.audio_level = 0.0;
            }

            Message::AudioLevelUpdate { level, is_speech } => {
                self.audio_level = level;
                self.is_speech_detected = is_speech;
            }

            Message::AudioThemeSelected(theme) => {
                self.selected_audio_theme = theme;
                // Audio theme preference is now saved by the daemon automatically
                return Task::perform(
                    set_and_test_audio_theme(self.socket_path.clone(), theme),
                    |result| match result {
                        Ok(_) => cosmic::Action::App(Message::DaemonConnected),
                        Err(e) => cosmic::Action::App(Message::DaemonError(e)),
                    },
                );
            }

            Message::SetAudioTheme(theme) => {
                self.selected_audio_theme = theme;
                // Audio theme preference is now saved by the daemon automatically
            }

            Message::AudioThemesLoaded(themes) => {
                self.audio_themes = themes;
            }

            Message::UdpDataReceived(data) => {
                self.last_udp_data = std::time::Instant::now();

                // Try parsing as recording state first (like the applet)
                if let Some(state) = parse_recording_state_from_udp(&data) {
                    self.recording_status = state;
                } else {
                    let audio_data = parse_audio_level_from_udp(&data);
                    // Always update audio level regardless of recording state
                    self.audio_level = audio_data.level;
                    self.is_speech_detected = audio_data.is_speech;
                }
            }

            Message::RecordingStateChanged(state) => {
                self.recording_status = state;
            }

            // Handled by helper methods
            _ => {}
        }
        Task::none()
    }

    /// Called when a nav item is selected.
    fn on_nav_select(&mut self, id: nav_bar::Id) -> Task<cosmic::Action<Self::Message>> {
        // Activate the page in the model.
        self.nav.activate(id);

        self.update_title()
    }
}

impl AppModel {
    /// Handle daemon connection messages
    #[allow(clippy::too_many_lines)]
    fn handle_daemon_messages(&mut self, message: Message) -> Task<cosmic::Action<Message>> {
        match message {
            Message::ConnectToDaemon => {
                self.daemon_status = DaemonStatus::Connecting;
                Task::perform(test_daemon_connection(self.socket_path.clone()), |result| {
                    cosmic::Action::App(Message::DaemonConnectionResult(result))
                })
            }

            Message::DaemonConnectionResult(result) => {
                match result {
                    Ok(()) => {
                        self.daemon_status = DaemonStatus::Connected;
                    }
                    Err(e) => {
                        self.daemon_status = DaemonStatus::Error(e);
                    }
                }
                Task::none()
            }

            Message::DaemonConnected => {
                // Only switch to Settings page if we're transitioning from disconnected to connected
                let was_disconnected = self.daemon_status != DaemonStatus::Connected;

                self.daemon_status = DaemonStatus::Connected;
                // Clear potentially stuck switching states on reconnect
                self.device_state = DeviceState::Ready;
                self.model_state = ModelState::Ready;
                // Restart UDP subscription when daemon reconnects
                self.udp_restart_counter += 1;
                info!(
                    "Daemon connected, restarting UDP subscription (counter: {})",
                    self.udp_restart_counter
                );

                // Only switch to Settings page on initial connection, not on periodic pings
                if was_disconnected {
                    // Find the Settings page ID and activate it
                    let mut settings_entity = None;
                    for entity in self.nav.iter() {
                        if matches!(self.nav.data::<Page>(entity), Some(Page::Settings)) {
                            settings_entity = Some(entity);
                            break;
                        }
                    }
                    if let Some(entity) = settings_entity {
                        self.nav.activate(entity);
                    }
                }

                // Fetch daemon configuration to sync settings
                let socket_path = self.socket_path.clone();
                Task::perform(fetch_daemon_config(socket_path), |result| match result {
                    Ok(config) => cosmic::Action::App(Message::DaemonConfigReceived(config)),
                    Err(err) => {
                        warn!("Failed to fetch daemon config: {err}");
                        // Still load models even if config fetch fails
                        cosmic::Action::App(Message::LoadModels)
                    }
                })
            }

            Message::DaemonConfigReceived(config) => {
                // Parse daemon configuration and sync audio theme settings
                if let Some(audio_config) = config
                    .get("audio")
                    .and_then(|audio| audio.get("theme"))
                    .and_then(|theme| theme.as_str())
                {
                    let daemon_audio_theme = audio_config.parse::<AudioTheme>().unwrap_or_default();
                    self.selected_audio_theme = daemon_audio_theme;
                } else {
                    warn!("No audio theme found in daemon configuration");
                }

                // Load models and preview typing setting
                Task::batch([
                    Task::perform(
                        async move {
                            // Small delay to let daemon fully initialize
                            tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                        },
                        |()| cosmic::Action::App(Message::LoadModels),
                    ),
                    // Load preview typing setting from daemon
                    Task::perform(get_preview_typing(self.socket_path.clone()), |result| {
                        match result {
                            Ok(enabled) => {
                                cosmic::Action::App(Message::PreviewTypingSettingLoaded(enabled))
                            }
                            Err(e) => {
                                log::warn!("Failed to load preview typing setting: {e}");
                                // Continue with default (false) - don't show error to user on startup
                                cosmic::Action::App(Message::PreviewTypingSettingLoaded(false))
                            }
                        }
                    }),
                ])
            }

            Message::DaemonError(err) => {
                self.daemon_status = DaemonStatus::Error(err);
                Task::perform(
                    async {
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    },
                    |()| cosmic::Action::App(Message::RetryConnection),
                )
            }

            Message::RetryConnection => {
                self.daemon_status = DaemonStatus::Connecting;
                Task::perform(ping_daemon(self.socket_path.clone()), |result| {
                    cosmic::Action::App(match result {
                        Ok(_) => Message::DaemonConnected,
                        Err(e) => Message::DaemonError(e),
                    })
                })
            }

            Message::RefreshDaemonStatus => {
                Task::perform(test_daemon_connection(self.socket_path.clone()), |result| {
                    cosmic::Action::App(Message::DaemonConnectionResult(result))
                })
            }

            Message::PingTimeout => {
                if self.daemon_status == DaemonStatus::Connected {
                    Task::perform(ping_daemon(self.socket_path.clone()), |result| {
                        cosmic::Action::App(match result {
                            Ok(_) => Message::DaemonConnected,
                            Err(e) => Message::DaemonError(e),
                        })
                    })
                } else {
                    Task::none()
                }
            }

            _ => Task::none(),
        }
    }

    /// Handle device management messages
    fn handle_device_messages(&mut self, message: Message) -> Task<cosmic::Action<Message>> {
        match message {
            Message::DeviceSelected(device) => {
                if device != self.current_device && self.device_state == DeviceState::Ready {
                    self.device_state = DeviceState::Switching;

                    info!("Switching to device: {device}");
                    Task::perform(set_device(self.socket_path.clone(), device), |result| {
                        match result {
                            Ok(()) => cosmic::Action::App(Message::LoadModels), // Reload models after device change
                            Err(e) => cosmic::Action::App(Message::DeviceError(e)),
                        }
                    })
                } else if self.device_state == DeviceState::Switching {
                    warn!("Device switch already in progress - ignoring");
                    Task::none()
                } else {
                    Task::none()
                }
            }

            Message::DeviceLoaded(device) => {
                self.current_device = device;
                self.device_state = DeviceState::Ready;
                Task::none()
            }

            Message::DeviceInfoLoaded(device, available_devices) => {
                self.current_device.clone_from(&device);
                self.available_devices.clone_from(&available_devices);
                self.device_state = DeviceState::Ready;
                Task::none()
            }

            Message::DeviceError(err) => {
                self.device_state = DeviceState::Ready;
                self.transcription_text = format!("Device Error: {err}");
                Task::none()
            }

            _ => Task::none(),
        }
    }

    /// Handle download progress messages
    #[allow(clippy::too_many_lines)]
    fn handle_download_messages(&mut self, message: Message) -> Task<cosmic::Action<Message>> {
        match message {
            Message::DownloadProgressUpdate(progress) => {
                // We have an actual download in progress
                self.download_progress = Some(progress.clone());
                self.download_state = if progress.status == "downloading" {
                    DownloadState::Active
                } else {
                    DownloadState::Idle
                };

                // If download is completed, reload models
                if progress.status == "completed" {
                    self.download_state = DownloadState::Idle;
                    self.download_progress = None;
                    Task::perform(
                        async move {
                            // Small delay to let daemon finish processing
                            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                        },
                        |()| cosmic::Action::App(Message::LoadModels),
                    )
                } else if progress.status == "cancelled" || progress.status == "error" {
                    self.download_state = DownloadState::Idle;
                    Task::none()
                } else {
                    Task::none()
                }
            }

            Message::CancelDownload => Task::perform(
                cancel_download(self.socket_path.clone()),
                |result| match result {
                    Ok(_) => cosmic::Action::App(Message::DownloadCancelled(String::new())),
                    Err(e) => cosmic::Action::App(Message::DownloadError {
                        model: String::new(),
                        error: e,
                    }),
                },
            ),

            Message::DownloadCompleted(model_name) => {
                info!("Model {model_name} finished downloading");
                self.download_progress = None;
                self.download_state = DownloadState::Idle;
                // Reload models to reflect the newly downloaded model
                Task::perform(
                    async move {
                        // Small delay to let daemon finish processing
                        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                    },
                    |()| cosmic::Action::App(Message::LoadModels),
                )
            }

            Message::DownloadCancelled(model_name) => {
                info!("Model {model_name} download was cancelled");
                self.download_progress = None;
                self.download_state = DownloadState::Idle;

                // Revert to previous model
                Task::perform(
                    set_model(self.socket_path.clone(), self.previous_model),
                    |result| {
                        match result {
                            Ok(_) => cosmic::Action::App(Message::LoadModels), // Reload to get updated current model
                            Err(e) => cosmic::Action::App(Message::ModelError(e)),
                        }
                    },
                )
            }

            Message::DownloadError { model, error } => {
                warn!("Download error for model {model}: {error}");
                self.download_progress = None;
                self.download_state = DownloadState::Idle;
                self.transcription_text = format!("Download Error: {error}");

                // Revert to previous model
                Task::perform(
                    set_model(self.socket_path.clone(), self.previous_model),
                    |result| {
                        match result {
                            Ok(_) => cosmic::Action::App(Message::LoadModels), // Reload to get updated current model
                            Err(e) => cosmic::Action::App(Message::ModelError(e)),
                        }
                    },
                )
            }

            Message::CheckDownloadStatus => {
                // Check download status if model is loading, switching, or we're tracking a download
                if matches!(
                    self.model_state,
                    ModelState::Loading | ModelState::Switching(_)
                ) || self.download_state == DownloadState::Active
                {
                    Task::perform(get_download_status(self.socket_path.clone()), |result| {
                        match result {
                            Ok(Some(progress)) => {
                                // Download is actually happening
                                cosmic::Action::App(Message::DownloadProgressUpdate(progress))
                            }
                            Ok(None) => {
                                // No download in progress, model must have loaded from cache
                                cosmic::Action::App(Message::NoDownloadInProgress)
                            }
                            Err(_) => {
                                // Failed to get status, assume no download
                                cosmic::Action::App(Message::NoDownloadInProgress)
                            }
                        }
                    })
                } else {
                    Task::none()
                }
            }

            Message::NoDownloadInProgress => {
                // Clear download state since there's no active download
                self.download_progress = None;
                self.download_state = DownloadState::Idle;
                // Reload models to ensure we have the current state
                Task::perform(
                    async move {
                        // Small delay to ensure model is fully loaded
                        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                    },
                    |()| cosmic::Action::App(Message::LoadModels),
                )
            }

            _ => Task::none(),
        }
    }

    /// Handle model management messages
    #[allow(clippy::too_many_lines)]
    fn handle_model_messages(&mut self, message: Message) -> Task<cosmic::Action<Message>> {
        match message {
            Message::LoadModels => Task::batch([
                Task::perform(
                    list_available_models(self.socket_path.clone()),
                    |result| match result {
                        Ok(models) => cosmic::Action::App(Message::AvailableModelsLoaded(models)),
                        Err(e) => cosmic::Action::App(Message::ModelError(e)),
                    },
                ),
                Task::perform(
                    get_current_model(self.socket_path.clone()),
                    |result| match result {
                        Ok(model) => cosmic::Action::App(Message::CurrentModelLoaded(model)),
                        Err(e) => cosmic::Action::App(Message::ModelError(e)),
                    },
                ),
                Task::perform(
                    get_current_device(self.socket_path.clone()),
                    |result| match result {
                        Ok((device, available_devices)) => cosmic::Action::App(
                            Message::DeviceInfoLoaded(device, available_devices),
                        ),
                        Err(e) => cosmic::Action::App(Message::DeviceError(e)),
                    },
                ),
            ]),

            Message::ModelSelected(model) => {
                if model == self.current_model {
                    Task::none()
                } else {
                    // Atomic state check and transition to prevent race conditions
                    match &self.model_state {
                        ModelState::Switching(_) => {
                            warn!("Model switch already in progress - ignoring concurrent request");
                            return Task::none();
                        }
                        ModelState::Loading => {
                            warn!("Model loading in progress - ignoring switch request");
                            return Task::none();
                        }
                        ModelState::Ready => {
                            // Proceed with state transition
                        }
                    }

                    // Prevent model switching if a download is already active
                    if self.download_state == DownloadState::Active
                        && self.download_progress.is_some()
                    {
                        warn!("Attempted to switch models during active download - ignoring");
                        return Task::none();
                    }

                    // Atomic state transition: Reserve the operation
                    self.model_state = ModelState::Switching(model);

                    // Save the current model as previous (to revert to on cancel)
                    self.previous_model = self.current_model;

                    // Don't assume download is needed - let CheckDownloadStatus determine that
                    self.download_state = DownloadState::Idle;
                    self.download_progress = None;

                    Task::batch([
                        Task::perform(set_model(self.socket_path.clone(), model), |result| {
                            match result {
                                Ok(_) => cosmic::Action::App(Message::LoadModels), // Reload to get updated current model
                                Err(e) => cosmic::Action::App(Message::ModelError(e)),
                            }
                        }),
                        // Check download status immediately to see if download is needed
                        Task::perform(
                            async move {
                                // Small delay to allow daemon to start download if needed
                                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                            },
                            |()| cosmic::Action::App(Message::CheckDownloadStatus),
                        ),
                    ])
                }
            }

            Message::ModelsLoaded { current, available } => {
                self.available_models = available;
                self.current_model = current;

                // Properly handle state transition from Switching to Ready
                match &self.model_state {
                    ModelState::Switching(target_model) => {
                        if current == *target_model {
                            info!("Model switch to {target_model} completed successfully");
                            self.model_state = ModelState::Ready;
                        } else {
                            warn!("Model switch failed: expected {target_model}, got {current}");
                            self.model_state = ModelState::Ready;
                        }
                    }
                    _ => {
                        self.model_state = ModelState::Ready;
                    }
                }

                // If we were tracking a download and models loaded successfully, clear download state
                if self.download_state == DownloadState::Active
                    && self.download_progress.is_some()
                    && let Some(progress) = &self.download_progress
                    && progress.status == "completed"
                {
                    self.download_progress = None;
                    self.download_state = DownloadState::Idle;
                }

                Task::none()
            }

            Message::AvailableModelsLoaded(models) => {
                self.available_models = models;
                Task::none()
            }

            Message::CurrentModelLoaded(model) => {
                self.current_model = model;
                self.model_state = ModelState::Ready;
                // Clear download state since model is loaded
                if self.download_state == DownloadState::Idle {
                    self.download_progress = None;
                }
                Task::none()
            }

            Message::ModelChanged(model) => {
                self.current_model = model;
                self.model_state = ModelState::Ready;
                Task::none()
            }

            Message::ModelError(err) => {
                // Handle error from model switching - reset to Ready and revert if needed
                match &self.model_state {
                    ModelState::Switching(target_model) => {
                        warn!("Model switch to {target_model} failed: {err}");
                        self.model_state = ModelState::Ready;
                        // Don't automatically revert - let user try again
                    }
                    _ => {
                        self.model_state = ModelState::Ready;
                    }
                }

                // Display sanitized error message to user
                let sanitized_error = err
                    .replace(&std::env::var("HOME").unwrap_or_default(), "$HOME")
                    .chars()
                    .take(200)
                    .collect::<String>();
                self.transcription_text = format!("Model Error: {sanitized_error}");
                Task::none()
            }

            _ => Task::none(),
        }
    }

    /// Handle preview typing messages
    fn handle_preview_typing_messages(
        &mut self,
        message: Message,
    ) -> Task<cosmic::Action<Message>> {
        match message {
            Message::PreviewTypingToggled(enabled) => {
                self.preview_typing_enabled = enabled;
                Task::perform(
                    set_preview_typing(self.socket_path.clone(), enabled),
                    move |result| match result {
                        Ok(()) => cosmic::Action::App(Message::PreviewTypingSettingLoaded(enabled)),
                        Err(e) => cosmic::Action::App(Message::PreviewTypingError(e)),
                    },
                )
            }

            Message::PreviewTypingSettingLoaded(enabled) => {
                self.preview_typing_enabled = enabled;
                Task::none()
            }

            Message::PreviewTypingError(err) => {
                // Log error and show it to user in transcription text
                log::warn!("Preview typing error: {err}");
                self.transcription_text = format!("Preview Typing Error: {err}");
                Task::none()
            }

            _ => Task::none(),
        }
    }

    /// Updates the header and window titles.
    pub fn update_title(&mut self) -> Task<cosmic::Action<Message>> {
        let mut window_title = "Super STT".to_string();

        if let Some(page) = self.nav.text(self.nav.active()) {
            window_title.push_str(" — ");
            window_title.push_str(page);
        }

        if let Some(id) = self.core.main_window_id() {
            self.set_window_title(window_title, id)
        } else {
            Task::none()
        }
    }
}

impl menu::action::MenuAction for MenuAction {
    type Message = Message;

    fn message(&self) -> Self::Message {
        match self {
            MenuAction::About => Message::ToggleContextPage(ContextPage::About),
        }
    }
}
