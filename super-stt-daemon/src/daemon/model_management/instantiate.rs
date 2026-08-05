// SPDX-License-Identifier: GPL-3.0-only
use crate::daemon::types::SuperSTTDaemon;
use crate::stt_models::ModelDefinition;
use crate::stt_models::backends::{self, DiscoveredBackend};
use crate::stt_models::transcribe::Transcribe;
use anyhow::{Result, anyhow, bail};
use super_stt_shared::models::provider::Provider;

impl SuperSTTDaemon {
    /// Build a running backend instance for `(name, provider, source)` plus its
    /// resolved definition. Central routing point for all model loading.
    ///
    /// # Errors
    /// Returns an error if no installed backend serves the model, the backend
    /// kind is unsupported in this build, or instantiation fails.
    pub async fn instantiate_backend(
        &self,
        name: &str,
        provider: &Provider,
        source: &str,
        device_pref: &str,
    ) -> Result<(Box<dyn Transcribe>, ModelDefinition)> {
        let (backend, def) = {
            let backends = self.backends.read().await;
            let (b, d) = backends::find_model(&backends, name, provider, source)
                .ok_or_else(|| anyhow!("no installed backend serves {name} via {provider}"))?;
            (b.clone(), d.clone())
        };

        let instance: Box<dyn Transcribe> = match backend.kind.as_str() {
            "wasm" => self.instantiate_wasm(&backend, &def).await?,
            "subprocess" => {
                self.instantiate_subprocess(&backend, name, device_pref)
                    .await?
            }
            other => bail!("backend {} declares unknown kind '{other}'", backend.source),
        };
        Ok((instance, def))
    }

    #[cfg(feature = "wasm-backends")]
    async fn instantiate_wasm(
        &self,
        backend: &DiscoveredBackend,
        def: &ModelDefinition,
    ) -> Result<Box<dyn Transcribe>> {
        use crate::stt_models::transcribe::ModelInfoData;
        let headers = self.backend_headers(backend).await?;
        let component = backend.dir.join(&backend.entrypoint);
        let info = ModelInfoData::new(
            def.name.clone(),
            def.provider.clone(),
            def.source.clone(),
            def.is_multilingual,
            def.is_online(),
            def.processing_interval,
        );
        // Websocket capability is a per-backend flag (every model the backend
        // serves shares it). Read it from the manifest so a ws-capable
        // component is linked against the realtime world.
        let websocket_capability =
            crate::stt_models::backends::manifest::Manifest::load(&backend.dir)?
                .capabilities
                .websocket;
        // Egress = the manifest-pinned `allowed_hosts` (SSRF-guarded) plus any
        // host the user authorized via the `base_url` option (SSRF exception).
        let user_allowed_hosts = self.base_url_egress_hosts(backend).await;
        let inst = crate::stt_models::wasm::WasmBackend::with_info(
            &component,
            backend.allowed_hosts.clone(),
            user_allowed_hosts,
            info,
            headers,
            websocket_capability,
            def.realtime,
        )?;
        Ok(Box::new(inst))
    }

    #[cfg(not(feature = "wasm-backends"))]
    async fn instantiate_wasm(
        &self,
        backend: &DiscoveredBackend,
        _def: &ModelDefinition,
    ) -> Result<Box<dyn Transcribe>> {
        bail!(
            "backend {} is a WASM backend, unsupported in this build (rebuild with --features wasm-backends)",
            backend.source
        )
    }

    #[cfg(feature = "subprocess-backends")]
    async fn instantiate_subprocess(
        &self,
        backend: &DiscoveredBackend,
        name: &str,
        device_pref: &str,
    ) -> Result<Box<dyn Transcribe>> {
        // Count the files we'll provision so the tracker's denominator is
        // accurate from the first broadcast. Each `[[models.files]]` entry is
        // one file. Empty-files models (cloud-only) skip the tracker entirely —
        // there is nothing to download.
        let manifest = crate::stt_models::backends::manifest::Manifest::load(&backend.dir)?;
        let total_files = manifest
            .models
            .iter()
            .find(|m| m.name == name)
            .map_or(0, |m| m.files.len());

        let tracker = if total_files == 0 {
            None
        } else {
            let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let t = std::sync::Arc::new(
                crate::download_progress::DownloadProgressTracker::new(
                    name.to_string(),
                    total_files,
                    cancelled,
                )
                .with_event_bus(std::sync::Arc::clone(&self.events)),
            );
            // Register so `GET /download_status` returns this tracker and the
            // settings app's progress card lights up. A previous tracker (from
            // a failed load) is cleared first — the manager rejects parallel
            // downloads, but a leftover entry would block this one.
            self.download_manager.clear_download();
            if let Err(e) = self
                .download_manager
                .start_download(std::sync::Arc::clone(&t))
            {
                log::warn!("could not register download tracker: {e}");
            }
            // Emit the initial state immediately so the UI shows "0%" rather
            // than nothing while the first chunk lands.
            t.broadcast_progress();
            Some(t)
        };

        let result = crate::stt_models::subprocess::SubprocessBackend::spawn(
            &backend.dir,
            name,
            device_pref,
            tracker.as_ref(),
        )
        .await;

        // Whatever happened (success, error, cancel), the tracker has done
        // its job — mark the terminal status and clear the manager so the
        // UI's progress card collapses and the next load can register.
        if let Some(t) = &tracker {
            match &result {
                Ok(_) => t.mark_completed(),
                Err(e) => t.mark_error(&format!("{e:#}")),
            }
            t.broadcast_progress();
            self.download_manager.clear_download();
        }

        Ok(Box::new(result?))
    }

    #[cfg(not(feature = "subprocess-backends"))]
    async fn instantiate_subprocess(
        &self,
        backend: &DiscoveredBackend,
        _name: &str,
        _device_pref: &str,
    ) -> Result<Box<dyn Transcribe>> {
        bail!(
            "backend {} is a subprocess backend, unsupported in this build (rebuild with --features subprocess-backends)",
            backend.source
        )
    }

    /// Form `x-stt-secret-*` / `x-stt-option-*` headers for a WASM backend.
    ///
    /// Secrets come solely from the generic per-backend keyring store
    /// (`backend:<source>:<name>`) written by the settings app — there is no
    /// legacy `<provider>-api-key` fallback, so the key must be set for this
    /// specific backend. Options use the config override if set, else the
    /// manifest default. A required secret that resolves to nothing is an error.
    #[cfg(feature = "wasm-backends")]
    async fn backend_headers(&self, backend: &DiscoveredBackend) -> Result<Vec<(String, String)>> {
        let mut headers = Vec::new();
        for secret in &backend.secrets {
            let value = crate::keyring::get_backend_secret_async(
                backend.source.clone(),
                secret.name.clone(),
            )
            .await
            .map_err(|e| anyhow!(e))?
            .filter(|v| !v.is_empty());
            match value {
                Some(v) => headers.push((format!("x-stt-secret-{}", secret.name), v)),
                // Safety-net error: the settings UI is expected to surface this
                // requirement *before* the user can request a model load. If
                // that pre-flight is bypassed (a UI bug, or a non-UI client),
                // the daemon is the final guard — keep the message short and
                // user-facing rather than naming internals (`secret name`,
                // `backend source`), since the caller already chose this
                // backend.
                None if secret.required => bail!(
                    "{} must be set.",
                    secret.label.as_deref().unwrap_or(&secret.name)
                ),
                None => {}
            }
        }
        for opt in &backend.options {
            if let Some(v) = self.resolved_backend_option(backend, opt).await {
                headers.push((format!("x-stt-option-{}", opt.name), v));
            }
        }
        Ok(headers)
    }

    /// The effective value of a backend option: the user's config override if
    /// set, else the manifest default. The single source of truth for option
    /// resolution — shared by header injection ([`Self::backend_headers`]) and
    /// egress allowlist derivation ([`Self::base_url_egress_hosts`]).
    #[cfg(feature = "wasm-backends")]
    async fn resolved_backend_option(
        &self,
        backend: &DiscoveredBackend,
        opt: &backends::manifest::Opt,
    ) -> Option<String> {
        self.config
            .read()
            .await
            .backend_option(&backend.source, &opt.name)
            .map(str::to_string)
            .or_else(|| opt.default.as_ref().map(ToString::to_string))
    }

    /// Hosts the *user* authorized via a `base_url` option, derived from its
    /// effective value (config override → manifest default).
    ///
    /// `base_url` is the documented convention for a backend's configurable
    /// endpoint (`docs/protocol/backend/config.md`); any backend declaring an
    /// option with that name gets the SSRF exception for its host. A backend
    /// cannot self-authorize these — options are set by the user only — so the
    /// host is exempt from the egress SSRF guard (it may be loopback/private,
    /// e.g. a local gateway). Unparseable or unset values contribute nothing.
    #[cfg(feature = "wasm-backends")]
    async fn base_url_egress_hosts(&self, backend: &DiscoveredBackend) -> Vec<String> {
        let Some(opt) = backend.options.iter().find(|o| o.name == "base_url") else {
            return Vec::new();
        };
        let Some(value) = self.resolved_backend_option(backend, opt).await else {
            return Vec::new();
        };
        base_url_host(&value).into_iter().collect()
    }
}

/// Extract the bare host (or `host[:port]`) from a base URL, mirroring the
/// origin-form assumption the OpenAI-style WASM backends make: `base_url` is a
/// scheme + authority, not a full-path URL. A bare host (no scheme) is treated
/// as a host. Returns `None` when nothing parseable remains.
#[cfg(feature = "wasm-backends")]
fn base_url_host(base_url: &str) -> Option<String> {
    let rest = base_url
        .strip_prefix("https://")
        .or_else(|| base_url.strip_prefix("http://"))
        .unwrap_or(base_url);
    let rest = rest.trim_end_matches('/');
    let host = match rest.find('/') {
        Some(i) => &rest[..i],
        None => rest,
    };
    if host.is_empty() {
        return None;
    }
    Some(host.to_string())
}

#[cfg(all(test, feature = "wasm-backends"))]
mod tests {
    use super::base_url_host;

    #[test]
    fn base_url_host_extracts_authority() {
        assert_eq!(
            base_url_host("https://api.openai.com"),
            Some("api.openai.com".to_string())
        );
        assert_eq!(
            base_url_host("https://api.openai.com/"),
            Some("api.openai.com".to_string())
        );
        assert_eq!(
            base_url_host("http://gw.example.com:8080"),
            Some("gw.example.com:8080".to_string())
        );
        // A bare host (no scheme) is treated as a host.
        assert_eq!(
            base_url_host("gw.example.com"),
            Some("gw.example.com".to_string())
        );
        // Any path after the authority is dropped — the backends assume origin form.
        assert_eq!(
            base_url_host("https://gw.example.com/v1/audio"),
            Some("gw.example.com".to_string())
        );
    }

    #[test]
    fn base_url_host_rejects_unparseable() {
        assert_eq!(base_url_host(""), None);
        assert_eq!(base_url_host("https://"), None);
        assert_eq!(base_url_host("/"), None);
        assert_eq!(base_url_host("https:///path"), None);
    }

    /// The user-set `base_url` option's host feeds the egress allowlist:
    /// manifest default when unset, the override (port included) when set, and
    /// nothing for a backend that declares no such option.
    #[tokio::test]
    async fn base_url_egress_hosts_resolves_override_or_default() {
        use crate::daemon::types::test_daemon;
        use crate::stt_models::backends::DiscoveredBackend;
        use crate::stt_models::backends::manifest::{Opt, OptionDefault, OptionType};

        let daemon = test_daemon().await;
        let source = "github.com/super-stt/openai";
        let backend = DiscoveredBackend {
            dir: std::path::PathBuf::from("/tmp/openai"),
            source: source.to_string(),
            name: "OpenAI".to_string(),
            kind: "wasm".to_string(),
            entrypoint: "openai.wasm".to_string(),
            allowed_hosts: vec!["api.openai.com".to_string()],
            secrets: vec![],
            options: vec![Opt {
                name: "base_url".to_string(),
                label: Some("API base URL".to_string()),
                description: "Base URL".to_string(),
                r#type: Some(OptionType::String),
                default: Some(OptionDefault::String("https://api.openai.com".to_string())),
                required: false,
            }],
            models: vec![],
        };

        // No override → the manifest default's host.
        assert_eq!(
            daemon.base_url_egress_hosts(&backend).await,
            vec!["api.openai.com"]
        );

        // Config override pointing at a local gateway → that host, port kept.
        daemon
            .config
            .write()
            .await
            .backends
            .options
            .entry(source.to_string())
            .or_default()
            .insert("base_url".to_string(), "http://localhost:8080".to_string());
        assert_eq!(
            daemon.base_url_egress_hosts(&backend).await,
            vec!["localhost:8080"]
        );

        // A backend declaring no `base_url` option contributes nothing.
        let no_base = DiscoveredBackend {
            options: vec![],
            ..backend
        };
        assert!(daemon.base_url_egress_hosts(&no_base).await.is_empty());
    }
}
