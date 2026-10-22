pub mod capture;
pub mod brand;
pub mod candidates;
pub mod devices;
pub mod icon;
pub mod process;
pub mod render;
pub mod thread;

use wasapi::{AudioClient, SharedModeEnginePeriod, WasapiError};

use crate::platform::StreamFacts;

/// The shortest period `IAudioClient3` accepts: the minimum raised to a whole multiple of the engine's
/// fundamental and clamped to the maximum. A non-multiple request is declined outright, which reads
/// exactly like a device that refuses low latency.
pub(super) fn lowest_latency_period(periods: &SharedModeEnginePeriod) -> u32 {
    let max = periods.max.max(1);
    let mut frames = periods.min.max(1);
    if periods.fundamental > 0 && frames % periods.fundamental != 0 {
        let rounded = frames + (periods.fundamental - frames % periods.fundamental);
        frames = if rounded > max { max } else { rounded };
    }
    frames
}

/// `AUDCLNT_E_DEVICE_INVALIDATED` and `AUDCLNT_E_RESOURCES_INVALIDATED`: the endpoint is still listed,
/// the stream under it is dead, and reopening the same id repairs it.
const INVALIDATED: [u32; 2] = [0x8889_0004, 0x8889_0026];

/// The HRESULT as an unsigned pattern, or `None` for an error that is not a COM code. No HRESULT travels
/// further than this module.
pub(super) fn error_code(error: &WasapiError) -> Option<u32> {
    match error {
        WasapiError::Windows(err) => {
            let code = err.code().0 as u32;
            (code != 0).then_some(code)
        }
        _ => None,
    }
}

/// Whether a failed stream died because the endpoint was reset, not because the device went away or the
/// format was refused. The code is returned so the caller can say which one it was.
pub(super) fn invalidated_endpoint(error: &WasapiError) -> Option<u32> {
    let code = error_code(error)?;
    INVALIDATED.contains(&code).then_some(code)
}

/// Turns "the low-latency call succeeded" into "the stream got a shorter period": a grant equal to the
/// device's own default cadence is not a win, and saying so is what keeps `lowLatency=true` honest.
pub(crate) fn grade(client: &AudioClient, facts: &mut StreamFacts) {
    let default_ms = client
        .get_device_period()
        .ok()
        .map(|(default_hns, _)| default_hns as f64 / 10_000.0);
    if !facts.low_latency_active {
        return;
    }
    match default_ms {
        Some(default_ms) if facts.granted_period_ms >= default_ms - 0.01 => {
            facts.low_latency_active = false;
            facts.low_latency_reason = format!(
                "IAudioClient3 granted {:.2} ms, which is not below the default period {:.2} ms",
                facts.granted_period_ms, default_ms
            );
        }
        Some(_) => facts.low_latency_reason.clear(),
        None => {
            facts.low_latency_active = false;
            facts.low_latency_reason =
                "device period unavailable, cannot grade the low-latency grant".to_string();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn periods(default: u32, fundamental: u32, min: u32, max: u32) -> SharedModeEnginePeriod {
        SharedModeEnginePeriod { default, fundamental, min, max }
    }

    fn com(code: u32) -> WasapiError {
        WasapiError::Windows(windows::core::Error::from_hresult(windows::core::HRESULT(code as i32)))
    }

    #[test]
    fn a_low_latency_period_is_rounded_to_the_engine_fundamental() {
        // VB-CABLE answers a minimum that is already one whole fundamental, so nothing moves.
        assert_eq!(lowest_latency_period(&periods(480, 128, 128, 480)), 128);
        // A minimum that is not a whole number of periods would be declined; ask for the next multiple.
        assert_eq!(lowest_latency_period(&periods(480, 128, 150, 960)), 256);
        // Rounding past the maximum falls back to the maximum instead of failing the attempt.
        assert_eq!(lowest_latency_period(&periods(480, 128, 400, 480)), 480);
        // A device that reports no fundamental is taken at its own minimum.
        assert_eq!(lowest_latency_period(&periods(480, 0, 100, 480)), 100);
    }

    #[test]
    fn only_the_two_invalidation_codes_ask_for_a_reconnect() {
        assert_eq!(invalidated_endpoint(&com(0x8889_0004)), Some(0x8889_0004));
        assert_eq!(invalidated_endpoint(&com(0x8889_0026)), Some(0x8889_0026));
        // A format refusal is not something to retry eight times, and neither is a lost event.
        assert_eq!(invalidated_endpoint(&com(0x8889_0008)), None);
        assert_eq!(invalidated_endpoint(&WasapiError::EventTimeout), None);
    }
}
