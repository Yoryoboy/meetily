use crate::audio::devices::configuration::{AudioDevice, DeviceType};
use anyhow::Result;

/// Configure Linux audio devices using ALSA/PulseAudio
pub fn configure_linux_audio(_host: &cpal::Host) -> Result<Vec<AudioDevice>> {
    let mut devices = Vec::new();

    // Linux capture is opened through PulseAudio/PipeWire so both sources share
    // the server clock and the fixed 48 kHz pipeline format.
    for name in crate::audio::capture::list_microphone_devices()? {
        devices.push(AudioDevice::new(name, DeviceType::Input));
    }

    // System audio is captured from native PulseAudio monitor sources, not ALSA outputs.
    for name in crate::audio::capture::list_system_audio_devices()? {
        devices.push(AudioDevice::new(name, DeviceType::Output));
    }

    Ok(devices)
}
