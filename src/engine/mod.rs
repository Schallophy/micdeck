//! The mixer core: rings, per-line readers with adaptive resampling, one bus per output line, and the
//! meters the UI reads. Nothing in here touches a device, so all of it is testable without audio hardware.

pub mod clock;
pub mod meter;
pub mod mixer;
pub mod reader;
pub mod ring;
pub mod status;
pub mod topology;

#[cfg(test)]
mod tests;

pub use clock::{Clock, SystemClock};
pub use meter::MeterState;
pub use mixer::Mixer;
pub use reader::{BufferStats, SourceReader};
pub use ring::SourceRing;
pub use status::{EngineStatus, Identity};
pub use topology::{Engine, MIC_FEED_ID, MAX_GAIN};
