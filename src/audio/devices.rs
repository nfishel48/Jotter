//! Device enumeration and selection.
//!
//! The important distinction here is *direction*. cpal decides whether to open
//! a plain capture stream or a loopback tap purely from whether the device
//! reports `supports_input()`, so a device's direction determines what you
//! actually end up recording. See [`super::capture::open_loopback`].

use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{Device, DeviceId};

use super::capture::CaptureError;

/// How the caller picked a device.
pub enum DeviceChoice {
    /// Use whichever device this module considers the best default.
    Default,
    /// Use a specific device, identified by its `DeviceId` string.
    Id(String),
}

/// A device plus the facts we care about when choosing one.
pub struct DeviceInfo {
    pub id: Option<String>,
    pub name: String,
    pub direction: cpal::DeviceDirection,
    pub supports_input: bool,
    pub supports_output: bool,
    pub interface: cpal::InterfaceType,
    pub device_type: cpal::DeviceType,
    pub is_default_input: bool,
    pub is_default_output: bool,
}

impl DeviceInfo {
    /// True if this device can be tapped for system audio.
    ///
    /// cpal only takes its loopback branch when the device reports no input
    /// support, so a duplex device silently records the microphone instead.
    pub fn can_loopback(&self) -> bool {
        self.supports_output && !self.supports_input
    }
}

fn describe(device: &Device, default_in: Option<&str>, default_out: Option<&str>) -> DeviceInfo {
    let id = device.id().ok().map(|i| i.to_string());
    let desc = device.description().ok();

    let name = desc
        .as_ref()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|| "<unknown>".to_string());

    DeviceInfo {
        is_default_input: id.is_some() && id.as_deref() == default_in,
        is_default_output: id.is_some() && id.as_deref() == default_out,
        direction: desc
            .as_ref()
            .map(|d| d.direction())
            .unwrap_or(cpal::DeviceDirection::Unknown),
        interface: desc
            .as_ref()
            .map(|d| d.interface_type())
            .unwrap_or(cpal::InterfaceType::Unknown),
        device_type: desc
            .as_ref()
            .map(|d| d.device_type())
            .unwrap_or(cpal::DeviceType::Unknown),
        supports_input: device.supports_input(),
        supports_output: device.supports_output(),
        id,
        name,
    }
}

/// Every device the default host knows about, paired with its description.
pub fn list_devices() -> Result<Vec<(Device, DeviceInfo)>, CaptureError> {
    let host = cpal::default_host();

    let default_in = host
        .default_input_device()
        .and_then(|d| d.id().ok())
        .map(|i| i.to_string());
    let default_out = host
        .default_output_device()
        .and_then(|d| d.id().ok())
        .map(|i| i.to_string());

    Ok(host
        .devices()?
        .map(|d| {
            let info = describe(&d, default_in.as_deref(), default_out.as_deref());
            (d, info)
        })
        .collect())
}

fn by_id(id: &str) -> Result<(Device, DeviceInfo), CaptureError> {
    let parsed: DeviceId = id
        .parse()
        .map_err(|_| CaptureError::NoSuchDevice(id.to_string()))?;

    let host = cpal::default_host();
    let device = host
        .device_by_id(&parsed)
        .ok_or_else(|| CaptureError::NoSuchDevice(id.to_string()))?;

    let info = describe(&device, None, None);
    Ok((device, info))
}

/// Resolve a microphone.
///
/// The default deliberately prefers a built-in microphone over the system
/// default. macOS switches Bluetooth headsets into a degraded call mode as soon
/// as their microphone is activated, which costs real transcription accuracy on
/// your own track — so when AirPods are the system default we'd rather record
/// the laptop mic and let the user override.
pub fn resolve_mic(choice: DeviceChoice) -> Result<(Device, DeviceInfo), CaptureError> {
    match choice {
        DeviceChoice::Id(id) => by_id(&id),
        DeviceChoice::Default => {
            let devices = list_devices()?;

            let built_in = devices
                .iter()
                .position(|(_, i)| i.supports_input && i.interface == cpal::InterfaceType::BuiltIn);
            let fallback = devices
                .iter()
                .position(|(_, i)| i.supports_input && i.is_default_input)
                .or_else(|| devices.iter().position(|(_, i)| i.supports_input));

            let idx = built_in.or(fallback).ok_or(CaptureError::NoInputDevice)?;

            let mut devices = devices;
            Ok(devices.swap_remove(idx))
        }
    }
}

/// Resolve a device to tap for system audio.
///
/// Prefers the default output device, since a tap only captures what is
/// actually routed to that device — tapping the laptop speakers while you
/// listen on AirPods yields silence. Falls back to any output-only device if
/// the default output is duplex and therefore untappable.
pub fn resolve_system(choice: DeviceChoice) -> Result<(Device, DeviceInfo), CaptureError> {
    match choice {
        DeviceChoice::Id(id) => by_id(&id),
        DeviceChoice::Default => {
            let devices = list_devices()?;

            let default_out = devices
                .iter()
                .position(|(_, i)| i.is_default_output && i.can_loopback());
            let any_out = devices.iter().position(|(_, i)| i.can_loopback());
            // Last resort: the default output even though it's duplex, so the
            // caller gets the duplex error rather than a confusing "no device".
            let duplex = devices.iter().position(|(_, i)| i.is_default_output);

            let idx = default_out
                .or(any_out)
                .or(duplex)
                .ok_or(CaptureError::NoOutputDevice)?;

            let mut devices = devices;
            Ok(devices.swap_remove(idx))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(supports_input: bool, supports_output: bool) -> DeviceInfo {
        DeviceInfo {
            id: Some("test".into()),
            name: "Test".into(),
            direction: cpal::DeviceDirection::Unknown,
            supports_input,
            supports_output,
            interface: cpal::InterfaceType::Unknown,
            device_type: cpal::DeviceType::Unknown,
            is_default_input: false,
            is_default_output: false,
        }
    }

    // The most important invariant in the codebase. cpal has no explicit
    // loopback API: it only taps system audio on a device reporting NO input.
    // Hand it a duplex device and it silently records the microphone into
    // system.wav instead — no error, discovered only at transcription time.
    #[test]
    fn only_output_only_devices_can_loopback() {
        assert!(info(false, true).can_loopback(), "output-only must tap");
        assert!(
            !info(true, true).can_loopback(),
            "duplex must NOT tap: cpal would record the mic into system.wav"
        );
        assert!(!info(true, false).can_loopback(), "input-only cannot tap");
        assert!(
            !info(false, false).can_loopback(),
            "inert device cannot tap"
        );
    }
}
