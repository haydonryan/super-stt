// SPDX-License-Identifier: GPL-3.0-only

use crate::{daemon::types::SuperSTTDaemon, output::preview::Typer};
use super_stt_shared::models::protocol::{Command, DaemonRequest, DaemonResponse};

impl SuperSTTDaemon {
    /// Main command handler - routes commands to appropriate handlers
    pub async fn handle_command(&self, request: DaemonRequest) -> DaemonResponse {
        // Track connection if client_id is present
        if let Some(client_id) = &request.client_id {
            self.update_client_connection(client_id.clone()).await;
        }

        let command = match Command::try_from(request) {
            Ok(cmd) => cmd,
            Err(e) => return DaemonResponse::error(&e),
        };

        match command {
            Command::Transcribe {
                audio_data,
                sample_rate,
                client_id,
            } => {
                self.handle_transcribe(audio_data, sample_rate, client_id)
                    .await
            }
            Command::Subscribe {
                event_types,
                client_info,
            } => self.handle_subscribe(event_types, client_info),
            Command::Unsubscribe => {
                DaemonResponse::error("Unsubscribe must be called on persistent connection")
            }
            Command::GetEvents {
                since_timestamp,
                event_types,
                limit,
            } => self.handle_get_events(since_timestamp, event_types, limit),
            Command::GetSubscriberInfo => self.handle_get_subscriber_info(),
            Command::Notify {
                event_type,
                client_id,
                data,
            } => self.handle_notify(event_type, client_id, data).await,
            Command::Ping { client_id } => self.handle_ping(client_id).await,
            Command::Status => self.handle_status().await,
            Command::StartRealTimeTranscription {
                client_id,
                sample_rate,
                language,
            } => {
                self.handle_start_realtime(client_id, sample_rate, language)
                    .await
            }
            Command::RealTimeAudioChunk {
                client_id,
                audio_data,
                sample_rate,
            } => {
                self.handle_realtime_audio(client_id, audio_data, sample_rate)
                    .await
            }
            Command::Record {
                write_mode,
                manual_stop,
            } => {
                if manual_stop {
                    // Toggle behaviour: if already recording, stop it and return immediately;
                    // the first caller will receive the transcription when it finishes.
                    let is_recording = *self.is_recording.read().await;
                    if is_recording {
                        let guard = self.manual_stop_tx.read().await;
                        if let Some(tx) = guard.as_ref() {
                            let _ = tx.send(());
                            log::info!("🛑 Manual stop triggered via second key press");
                        } else {
                            log::warn!("Manual stop requested but no stop channel found (recording not ready or already finishing)");
                        }
                        return DaemonResponse::success()
                            .with_message("Recording stop signal sent".to_string());
                    }
                }
                let mut typer = Typer::default();
                self.handle_record_internal(&mut typer, write_mode, manual_stop)
                    .await
            }
            Command::SetAudioTheme { theme } => self.handle_set_audio_theme(theme),
            Command::GetAudioTheme => self.handle_get_audio_theme(),
            Command::TestAudioTheme => self.handle_test_audio_theme().await,
            Command::SetModel { model } => self.handle_set_model(model).await,
            Command::GetModel => self.handle_get_model().await,
            Command::ListModels => self.handle_list_models(),
            Command::SetDevice { device } => self.handle_set_device(device).await,
            Command::GetDevice => self.handle_get_device().await,
            Command::GetConfig => self.handle_get_config().await,
            Command::CancelDownload => self.handle_cancel_download(),
            Command::GetDownloadStatus => self.handle_get_download_status(),
            Command::ListAudioThemes => self.handle_list_audio_themes(),
            Command::SetPreviewTyping { enabled } => self.handle_set_preview_typing(enabled).await,
            Command::GetPreviewTyping => self.handle_get_preview_typing(),
        }
    }

    /// Placeholder for real-time handlers - these need to be implemented
    pub async fn handle_start_realtime(
        &self,
        client_id: String,
        sample_rate: Option<u32>,
        language: Option<String>,
    ) -> DaemonResponse {
        match self
            .realtime_manager
            .start_session(client_id.clone(), sample_rate, language)
            .await
        {
            Ok(_receiver) => {
                log::info!("Started real-time transcription for client: {client_id}");
                DaemonResponse::success()
                    .with_client_id(client_id)
                    .with_message("Real-time transcription session started".to_string())
            }
            Err(e) => {
                log::error!("Failed to start real-time session: {e}");
                DaemonResponse::error(&format!("Failed to start real-time session: {e}"))
            }
        }
    }

    pub async fn handle_realtime_audio(
        &self,
        client_id: String,
        audio_data: Vec<f32>,
        sample_rate: u32,
    ) -> DaemonResponse {
        match self
            .realtime_manager
            .process_audio_chunk(&client_id, audio_data, sample_rate)
            .await
        {
            Ok(()) => DaemonResponse::success().with_message("Audio chunk processed".to_string()),
            Err(e) => {
                log::warn!("Failed to process audio chunk for {client_id}: {e}");
                DaemonResponse::error(&format!("Failed to process audio chunk: {e}"))
            }
        }
    }
}
