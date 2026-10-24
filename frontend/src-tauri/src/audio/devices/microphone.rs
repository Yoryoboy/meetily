use anyhow::{anyhow, Result};
use cpal::traits::{HostTrait, DeviceTrait};
use log::{info, warn};

use super::configuration::{AudioDevice, DeviceType};

/// Get the default input (microphone) device for the system
pub fn default_input_device() -> Result<AudioDevice> {
    // Linux lists microphones from PulseAudio (default source first). Return
    // that listed name instead of cpal's generic "default", otherwise the
    // device monitor never finds the recording mic and reports it disconnected.
    #[cfg(target_os = "linux")]
    if let Some(name) = pick_listed_default(crate::audio::capture::list_microphone_devices()) {
        return Ok(AudioDevice::new(name, DeviceType::Input));
    }

    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or_else(|| anyhow!("No default input device found"))?;
    Ok(AudioDevice::new(device.name()?, DeviceType::Input))
}

/// Find the built-in microphone device (wired, stable, consistent sample rate)
///
/// Searches for MacBook/built-in microphone patterns to find the hardware
/// microphone instead of Bluetooth devices. This is useful for:
/// - Avoiding Bluetooth variable sample rate issues
/// - Getting stable wired audio for recording
/// - Fallback when Bluetooth device is default but unreliable
///
/// Returns None if no built-in microphone found
pub fn find_builtin_input_device() -> Result<Option<AudioDevice>> {
    let host = cpal::default_host();

    // Built-in microphone name patterns (platform-specific)
    let builtin_patterns = [
        // macOS patterns
        "macbook",
        "built-in microphone",
        "internal microphone",
        // Windows patterns
        "microphone array",
        "realtek",
        "conexant",
        // Linux patterns
        "hda intel",
        "built-in audio",
    ];

    // Search all input devices for built-in pattern matches
    for device in host.input_devices()? {
        if let Ok(name) = device.name() {
            let name_lower = name.to_lowercase();

            // Check if this is a built-in device
            for pattern in &builtin_patterns {
                if name_lower.contains(pattern) {
                    // Additional filter: exclude Bluetooth/wireless devices
                    if name_lower.contains("bluetooth") ||
                       name_lower.contains("airpods") ||
                       name_lower.contains("wireless") {
                        continue; // Skip Bluetooth devices
                    }

                    info!("🎤 Found built-in microphone: '{}'", name);
                    return Ok(Some(AudioDevice::new(name, DeviceType::Input)));
                }
            }
        }
    }

    warn!("⚠️ No built-in microphone found (searched {} patterns)", builtin_patterns.len());
    Ok(None)
}
/// First listed microphone, i.e. the platform's default source when the
/// listing puts it first (Linux PulseAudio does).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn pick_listed_default(listed: Result<Vec<String>>) -> Option<String> {
    listed.ok()?.into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listed_default_is_the_first_listed_microphone() {
        let listed = vec![
            "PulseAudio microphone: usb mic [alsa_input.usb-mic]".to_string(),
            "PulseAudio microphone: webcam [alsa_input.webcam]".to_string(),
        ];
        assert_eq!(
            pick_listed_default(Ok(listed)),
            Some("PulseAudio microphone: usb mic [alsa_input.usb-mic]".to_string())
        );
    }

    #[test]
    fn listed_default_falls_back_when_listing_is_empty_or_fails() {
        assert_eq!(pick_listed_default(Ok(vec![])), None);
        assert_eq!(pick_listed_default(Err(anyhow!("pactl missing"))), None);
    }

    /// The recording mic name must be one the device monitor can find,
    /// otherwise it reports a false disconnect every poll.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "requires a running PulseAudio/PipeWire server"]
    async fn linux_default_microphone_is_in_the_monitored_device_list() {
        let default = default_input_device().expect("default microphone");
        let listed = crate::audio::devices::list_audio_devices().await.expect("device list");
        assert!(
            listed.iter().any(|d| d.name == default.name),
            "default mic '{}' not in device list",
            default.name
        );
    }
}
