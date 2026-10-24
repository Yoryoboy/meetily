use anyhow::Result;
use cpal::traits::{DeviceTrait, HostTrait};
use futures_util::{Stream, StreamExt};
use std::pin::Pin;
use std::task::{Context, Poll};

#[cfg(target_os = "macos")]
use super::core_audio::CoreAudioCapture;
#[cfg(target_os = "macos")]
use futures_channel::mpsc;
#[cfg(target_os = "macos")]
use log::info;

/// System audio capture using Core Audio on macOS and PulseAudio monitor sources on Linux.
pub struct SystemAudioCapture {
    _host: cpal::Host,
}

impl SystemAudioCapture {
    pub fn new() -> Result<Self> {
        let host = cpal::default_host();
        Ok(Self { _host: host })
    }

    pub fn list_system_devices() -> Result<Vec<String>> {
        #[cfg(target_os = "linux")]
        {
            return linux::list_monitor_sources();
        }

        let host = cpal::default_host();
        let devices = host
            .output_devices()
            .map_err(|e| anyhow::anyhow!("Failed to enumerate output devices: {}", e))?;

        let mut device_names = Vec::new();
        for device in devices {
            if let Ok(name) = device.name() {
                device_names.push(name);
            }
        }

        Ok(device_names)
    }

    pub fn start_system_audio_capture(&self) -> Result<SystemAudioStream> {
        #[cfg(target_os = "macos")]
        {
            info!("Starting Core Audio system capture (macOS)");
            // Use Core Audio tap for system audio capture
            let core_audio = CoreAudioCapture::new()?;
            let core_audio_stream = core_audio.stream()?;
            let sample_rate = core_audio_stream.sample_rate();

            // Convert CoreAudioStream to SystemAudioStream
            let (tx, rx) = mpsc::unbounded::<Vec<f32>>();
            let (drop_tx, drop_rx) = std::sync::mpsc::channel::<()>();

            // Spawn task to forward Core Audio samples
            tokio::spawn(async move {
                use futures_util::StreamExt;
                let mut stream = core_audio_stream;
                let mut buffer = Vec::new();
                let chunk_size = 1024;

                loop {
                    // Check if we should stop
                    if drop_rx.try_recv().is_ok() {
                        break;
                    }

                    // Poll the Core Audio stream
                    match stream.next().await {
                        Some(sample) => {
                            buffer.push(sample);
                            if buffer.len() >= chunk_size {
                                if tx.unbounded_send(buffer.clone()).is_err() {
                                    break;
                                }
                                buffer.clear();
                            }
                        }
                        None => break,
                    }
                }

                // Send any remaining samples
                if !buffer.is_empty() {
                    let _ = tx.unbounded_send(buffer);
                }
            });

            let receiver = rx.map(futures_util::stream::iter).flatten();

            info!("Core Audio system capture started successfully");

            Ok(SystemAudioStream {
                drop_tx,
                sample_rate,
                receiver: Box::pin(receiver),
            })
        }

        #[cfg(not(target_os = "macos"))]
        {
            #[cfg(target_os = "linux")]
            {
                return linux::start_monitor_capture(None);
            }

            anyhow::bail!("System audio capture is not implemented for this platform")
        }
    }

    #[cfg(target_os = "linux")]
    pub fn start_microphone_capture_for_device(
        &self,
        requested: &str,
    ) -> Result<SystemAudioStream> {
        linux::start_microphone_capture(Some(requested))
    }

    #[cfg(target_os = "linux")]
    pub fn list_microphone_devices() -> Result<Vec<String>> {
        linux::list_microphone_sources()
    }

    #[cfg(target_os = "linux")]
    pub fn start_system_audio_capture_for_device(
        &self,
        requested: &str,
    ) -> Result<SystemAudioStream> {
        linux::start_monitor_capture(Some(requested))
    }

    pub fn check_system_audio_permissions() -> bool {
        // Check if we can enumerate audio devices
        match cpal::default_host().output_devices() {
            Ok(_) => true,
            Err(_) => false,
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::SystemAudioStream;
    use anyhow::{Context, Result};
    use futures_channel::mpsc;
    use futures_util::StreamExt;
    use libpulse_binding::sample::{Format, Spec};
    use libpulse_binding::stream::Direction;
    use libpulse_simple_binding::Simple;
    use log::{info, warn};
    use std::process::Command;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use std::thread;

    pub const SOURCE_PREFIX: &str = "PulseAudio monitor: ";
    pub const MIC_SOURCE_PREFIX: &str = "PulseAudio microphone: ";
    const SAMPLE_RATE: u32 = 48_000;
    const MONITOR_CHANNELS: u8 = 2;
    const MICROPHONE_CHANNELS: u8 = 1;
    const CHUNK_SAMPLES: usize = 1024;

    fn decode_f32le(bytes: &[u8]) -> Vec<f32> {
        bytes
            .chunks_exact(std::mem::size_of::<f32>())
            .map(|chunk| f32::from_le_bytes(chunk.try_into().unwrap()))
            .collect()
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct MonitorSource {
        name: String,
        description: String,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct PulseSource {
        name: String,
        description: String,
        is_monitor: bool,
    }

    fn pactl(args: &[&str]) -> Result<String> {
        let output = Command::new("pactl")
            .args(args)
            .output()
            .context("failed to execute pactl")?;
        if !output.status.success() {
            anyhow::bail!(
                "pactl {:?} failed: {}",
                args,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn parse_short_sources(text: &str) -> Vec<MonitorSource> {
        text.lines()
            .filter_map(|line| {
                let fields: Vec<_> = line.split('\t').collect();
                let name = fields.get(1)?.trim();
                if !name.ends_with(".monitor") {
                    return None;
                }
                let description = name.trim_end_matches(".monitor").replace('_', " ");
                Some(MonitorSource {
                    name: name.to_string(),
                    description,
                })
            })
            .collect()
    }

    fn parse_sources(text: &str) -> Vec<PulseSource> {
        text.lines()
            .filter_map(|line| {
                let fields: Vec<_> = line.split('\t').collect();
                let name = fields.get(1)?.trim();
                if name.is_empty() {
                    return None;
                }
                Some(PulseSource {
                    name: name.to_string(),
                    description: name.replace('_', " "),
                    is_monitor: name.ends_with(".monitor"),
                })
            })
            .collect()
    }

    fn parse_microphone_sources(text: &str) -> Vec<PulseSource> {
        parse_sources(text)
            .into_iter()
            .filter(|source| !source.is_monitor)
            .collect()
    }

    fn selected_microphone_source(
        requested: Option<&str>,
        sources: &[PulseSource],
    ) -> Result<String> {
        let requested = requested.map(|value| {
            let value = value.trim().trim_end_matches("(input)").trim();
            value
                .strip_prefix(MIC_SOURCE_PREFIX)
                .and_then(|value| {
                    value
                        .rsplit_once('[')
                        .map(|(_, name)| name.trim_end_matches(']').trim())
                })
                .or_else(|| {
                    value
                        .strip_prefix("PulseAudio microphone [")
                        .and_then(|v| v.strip_suffix(']'))
                })
                .unwrap_or(value)
        });
        if let Some(name) = requested.filter(|value| !value.is_empty()) {
            if let Some(source) = sources.iter().find(|source| source.name == name) {
                return Ok(source.name.clone());
            }
            if let Some(source) = sources.iter().find(|source| source.description == name) {
                return Ok(source.name.clone());
            }
            warn!(
                "Saved Linux microphone '{}' is unavailable; falling back to the default source",
                name
            );
        }
        let default_source = pactl(&["get-default-source"])
            .unwrap_or_default()
            .trim()
            .to_string();
        sources
            .iter()
            .find(|source| source.name == default_source)
            .or_else(|| sources.first())
            .map(|source| source.name.clone())
            .context("no PulseAudio microphone sources are available")
    }

    fn selected_source(requested: Option<&str>, sources: &[MonitorSource]) -> Result<String> {
        let default_sink = pactl(&["get-default-sink"])
            .unwrap_or_default()
            .trim()
            .to_string();
        let default_monitor = format!("{}.monitor", default_sink);
        let requested = requested.map(|s| s.trim().trim_end_matches("(output)").trim());
        let requested_name = requested.and_then(|value| {
            value
                .strip_prefix(SOURCE_PREFIX)
                .and_then(|value| {
                    value
                        .rsplit_once('[')
                        .map(|(_, name)| name.trim_end_matches(']').trim())
                })
                .or_else(|| {
                    value
                        .strip_prefix("PulseAudio monitor [")
                        .and_then(|v| v.strip_suffix(']'))
                })
        });

        if let Some(name) = requested_name.or(requested.filter(|s| s.ends_with(".monitor"))) {
            if sources.iter().any(|source| source.name == name) {
                return Ok(name.to_string());
            }
            warn!("Saved Linux system audio device '{}' is unavailable; falling back to the default sink monitor", value_for_log(requested));
        }
        if sources.iter().any(|source| source.name == default_monitor) {
            info!(
                "Using default sink monitor '{}' for Linux system audio",
                default_monitor
            );
            return Ok(default_monitor);
        }
        sources
            .first()
            .map(|source| source.name.clone())
            .context("no PulseAudio monitor sources are available")
    }

    fn value_for_log(value: Option<&str>) -> &str {
        value.unwrap_or("<default>")
    }

    pub fn list_monitor_sources() -> Result<Vec<String>> {
        let mut sources = parse_short_sources(&pactl(&["list", "short", "sources"])?);
        let default_monitor = format!(
            "{}.monitor",
            pactl(&["get-default-sink"]).unwrap_or_default().trim()
        );
        sources.sort_by_key(|source| source.name != default_monitor);
        Ok(sources
            .into_iter()
            .map(|source| format!("{}{} [{}]", SOURCE_PREFIX, source.description, source.name))
            .collect())
    }

    pub fn list_microphone_sources() -> Result<Vec<String>> {
        let default_source = pactl(&["get-default-source"])
            .unwrap_or_default()
            .trim()
            .to_string();
        let mut sources = parse_microphone_sources(&pactl(&["list", "short", "sources"])?);
        sources.sort_by_key(|source| source.name != default_source);
        Ok(sources
            .into_iter()
            .map(|source| {
                format!(
                    "{}{} [{}]",
                    MIC_SOURCE_PREFIX, source.description, source.name
                )
            })
            .collect())
    }

    pub fn start_monitor_capture(requested: Option<&str>) -> Result<SystemAudioStream> {
        let sources = parse_short_sources(&pactl(&["list", "short", "sources"])?);
        let source = selected_source(requested, &sources)?;
        let sample_spec = Spec {
            format: Format::F32le,
            rate: SAMPLE_RATE,
            channels: MONITOR_CHANNELS,
        };
        let (tx, rx) = mpsc::unbounded::<Vec<f32>>();
        let stopped = Arc::new(AtomicBool::new(false));
        let stopped_thread = stopped.clone();
        let source_for_thread = source.clone();
        thread::Builder::new()
            .name("meetily-pulse-monitor".into())
            .spawn(move || {
                let mut pulse = match Simple::new(
                    None,
                    "Meetily",
                    Direction::Record,
                    Some(&source_for_thread),
                    "System audio monitor",
                    &sample_spec,
                    None,
                    None,
                ) {
                    Ok(pulse) => pulse,
                    Err(error) => {
                        warn!(
                            "Failed to open PulseAudio monitor '{}': {}",
                            source_for_thread, error
                        );
                        return;
                    }
                };
                let mut bytes =
                    vec![
                        0u8;
                        CHUNK_SAMPLES * MONITOR_CHANNELS as usize * std::mem::size_of::<f32>()
                    ];
                info!(
                    "Capturing Linux system audio from PulseAudio monitor '{}'",
                    source_for_thread
                );
                while !stopped_thread.load(Ordering::Relaxed) {
                    if let Err(error) = pulse.read(&mut bytes) {
                        warn!("PulseAudio monitor read failed: {}", error);
                        break;
                    }
                    let samples = decode_f32le(&bytes);
                    if tx.unbounded_send(samples).is_err() {
                        break;
                    }
                }
            })
            .context("failed to start PulseAudio capture thread")?;

        let receiver = rx.map(futures_util::stream::iter).flatten();
        Ok(SystemAudioStream {
            drop_tx: stopped,
            sample_rate: SAMPLE_RATE,
            receiver: Box::pin(receiver),
        })
    }

    pub fn start_microphone_capture(requested: Option<&str>) -> Result<SystemAudioStream> {
        let sources = parse_microphone_sources(&pactl(&["list", "short", "sources"])?);
        let source = selected_microphone_source(requested, &sources)?;
        let sample_spec = Spec {
            format: Format::F32le,
            rate: SAMPLE_RATE,
            channels: MICROPHONE_CHANNELS,
        };
        let (tx, rx) = mpsc::unbounded::<Vec<f32>>();
        let stopped = Arc::new(AtomicBool::new(false));
        let stopped_thread = stopped.clone();
        let source_for_thread = source.clone();
        thread::Builder::new()
            .name("meetily-pulse-microphone".into())
            .spawn(move || {
                let mut pulse = match Simple::new(
                    None,
                    "Meetily",
                    Direction::Record,
                    Some(&source_for_thread),
                    "Microphone",
                    &sample_spec,
                    None,
                    None,
                ) {
                    Ok(pulse) => pulse,
                    Err(error) => {
                        warn!(
                            "Failed to open PulseAudio microphone '{}': {}",
                            source_for_thread, error
                        );
                        return;
                    }
                };
                let mut bytes = vec![0u8; CHUNK_SAMPLES * std::mem::size_of::<f32>()];
                info!(
                    "Capturing Linux microphone from PulseAudio source '{}' at {} Hz mono F32LE",
                    source_for_thread, SAMPLE_RATE
                );
                while !stopped_thread.load(Ordering::Relaxed) {
                    if let Err(error) = pulse.read(&mut bytes) {
                        warn!("PulseAudio microphone read failed: {}", error);
                        break;
                    }
                    let samples = decode_f32le(&bytes);
                    if tx.unbounded_send(samples).is_err() {
                        break;
                    }
                }
                info!("PulseAudio microphone capture stopped");
            })
            .context("failed to start PulseAudio microphone capture thread")?;
        let receiver = rx.map(futures_util::stream::iter).flatten();
        Ok(SystemAudioStream {
            drop_tx: stopped,
            sample_rate: SAMPLE_RATE,
            receiver: Box::pin(receiver),
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn parses_only_monitor_sources() {
            let sources = parse_short_sources("94\talsa_output.usb.analog-stereo.monitor\tPipeWire\n95\talsa_input.usb.mono\tPipeWire\n");
            assert_eq!(sources.len(), 1);
            assert_eq!(sources[0].name, "alsa_output.usb.analog-stereo.monitor");
        }
        #[test]
        fn selected_source_falls_back_to_default_monitor() {
            let sources = vec![MonitorSource {
                name: "sink.monitor".into(),
                description: "Sink".into(),
            }];
            assert_eq!(
                selected_source(Some("hdmi:CARD=NVidia,DEV=2 (output)"), &sources).unwrap(),
                "sink.monitor"
            );
        }

        #[test]
        fn parses_and_filters_real_microphone_sources() {
            let sources = parse_microphone_sources(
                "1\talsa_output.usb.monitor\tMonitor\n2\talsa_input.usb.mic\tMic\n",
            );
            assert_eq!(sources.len(), 1);
            assert_eq!(sources[0].name, "alsa_input.usb.mic");
        }

        #[test]
        fn selects_microphone_by_legacy_name() {
            let sources = vec![PulseSource {
                name: "alsa_input.usb.mic".into(),
                description: "alsa input.usb.mic".into(),
                is_monitor: false,
            }];
            assert_eq!(
                selected_microphone_source(Some("alsa_input.usb.mic (input)"), &sources).unwrap(),
                "alsa_input.usb.mic"
            );
        }

        #[test]
        fn fixed_pulse_chunk_decodes_to_fixed_sample_count() {
            let mut bytes = Vec::with_capacity(CHUNK_SAMPLES * 4);
            for sample in 0..CHUNK_SAMPLES {
                let sample = sample as f32;
                bytes.extend_from_slice(&sample.to_le_bytes());
            }
            let samples = decode_f32le(&bytes);
            assert_eq!(samples.len(), CHUNK_SAMPLES);
            assert_eq!(samples[CHUNK_SAMPLES - 1], (CHUNK_SAMPLES - 1) as f32);
        }
    }
}

pub struct SystemAudioStream {
    #[cfg(target_os = "linux")]
    drop_tx: std::sync::Arc<std::sync::atomic::AtomicBool>,
    #[cfg(not(target_os = "linux"))]
    drop_tx: std::sync::mpsc::Sender<()>,
    sample_rate: u32,
    receiver: Pin<Box<dyn Stream<Item = f32> + Send + Sync>>,
}

impl Drop for SystemAudioStream {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        self.drop_tx
            .store(true, std::sync::atomic::Ordering::Relaxed);
        #[cfg(not(target_os = "linux"))]
        let _ = self.drop_tx.send(());
    }
}

impl Stream for SystemAudioStream {
    type Item = f32;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.receiver.as_mut().poll_next_unpin(cx)
    }
}

impl SystemAudioStream {
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
}

/// Public interface for system audio capture
pub async fn start_system_audio_capture() -> Result<SystemAudioStream> {
    let capture = SystemAudioCapture::new()?;
    capture.start_system_audio_capture()
}

pub fn list_system_audio_devices() -> Result<Vec<String>> {
    SystemAudioCapture::list_system_devices()
}

#[cfg(target_os = "linux")]
pub fn list_microphone_devices() -> Result<Vec<String>> {
    SystemAudioCapture::list_microphone_devices()
}

pub fn check_system_audio_permissions() -> bool {
    SystemAudioCapture::check_system_audio_permissions()
}
