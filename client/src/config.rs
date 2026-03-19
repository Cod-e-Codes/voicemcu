use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;
use serde::{Deserialize, Serialize};

use voicemcu_common::protocol::DEFAULT_BITRATE;

use crate::audio_io::DEFAULT_RING_BUFFER_FRAMES;
use crate::tui::DEFAULT_MAX_EVENTS;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Parser)]
#[command(name = "voicemcu-client", about = "MCU voice chat client")]
pub struct Cli {
    /// Server address (e.g. 127.0.0.1:4433)
    #[arg(required_unless_present = "list_devices")]
    pub server: Option<SocketAddr>,

    /// Room code
    #[arg(required_unless_present = "list_devices")]
    pub room: Option<String>,

    /// Display name
    #[arg(required_unless_present = "list_devices")]
    pub name: Option<String>,

    /// List available audio devices and exit
    #[arg(long)]
    pub list_devices: bool,

    /// Path to TOML configuration file
    #[arg(long)]
    pub config: Option<PathBuf>,

    /// Pin server certificate by SHA-256 fingerprint
    #[arg(long)]
    pub cert_hash: Option<String>,

    /// Skip all certificate verification (insecure)
    #[arg(long)]
    pub danger_skip_verify: bool,

    /// Send 440 Hz sine wave instead of mic input
    #[arg(long)]
    pub test_tone: bool,

    /// Log file path (default: voicemcu.log)
    #[arg(long)]
    pub log_file: Option<String>,

    /// Opus upstream bitrate in bits per second
    #[arg(long)]
    pub bitrate: Option<i32>,

    /// Ring buffer size in 20 ms frames
    #[arg(long)]
    pub ring_buffer_frames: Option<usize>,

    /// Maximum events in TUI log
    #[arg(long)]
    pub max_events: Option<usize>,

    /// Input (microphone) device name
    #[arg(long)]
    pub input_device: Option<String>,

    /// Output (speaker) device name
    #[arg(long)]
    pub output_device: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ClientConfig {
    pub log_file: String,
    pub bitrate: i32,
    pub ring_buffer_frames: usize,
    pub max_events: usize,
    pub input_device: Option<String>,
    pub output_device: Option<String>,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            log_file: "voicemcu.log".into(),
            bitrate: DEFAULT_BITRATE,
            ring_buffer_frames: DEFAULT_RING_BUFFER_FRAMES,
            max_events: DEFAULT_MAX_EVENTS,
            input_device: None,
            output_device: None,
        }
    }
}

impl ClientConfig {
    pub fn load(cli: &Cli) -> Result<Self, BoxError> {
        let mut config = if let Some(ref path) = cli.config {
            let contents = std::fs::read_to_string(path)?;
            toml::from_str(&contents)?
        } else {
            Self::default()
        };

        if let Some(ref log_file) = cli.log_file {
            config.log_file = log_file.clone();
        }
        if let Some(bitrate) = cli.bitrate {
            config.bitrate = bitrate;
        }
        if let Some(ring_buffer_frames) = cli.ring_buffer_frames {
            config.ring_buffer_frames = ring_buffer_frames;
        }
        if let Some(max_events) = cli.max_events {
            config.max_events = max_events;
        }
        if let Some(ref d) = cli.input_device {
            config.input_device = Some(d.clone());
        }
        if let Some(ref d) = cli.output_device {
            config.output_device = Some(d.clone());
        }

        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), BoxError> {
        if self.bitrate < 6_000 || self.bitrate > 510_000 {
            return Err("bitrate must be between 6000 and 510000".into());
        }
        if self.ring_buffer_frames == 0 || self.ring_buffer_frames > 100 {
            return Err("ring_buffer_frames must be between 1 and 100".into());
        }
        if self.max_events == 0 {
            return Err("max_events must be > 0".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid() {
        ClientConfig::default()
            .validate()
            .expect("default should be valid");
    }

    #[test]
    fn parse_full_toml() {
        let toml_str = r#"
            log_file = "/tmp/voice.log"
            bitrate = 64000
            ring_buffer_frames = 20
            max_events = 500
        "#;
        let config: ClientConfig = toml::from_str(toml_str).expect("parse");
        assert_eq!(config.log_file, "/tmp/voice.log");
        assert_eq!(config.bitrate, 64000);
        assert_eq!(config.ring_buffer_frames, 20);
        assert_eq!(config.max_events, 500);
    }

    #[test]
    fn partial_toml_uses_defaults() {
        let toml_str = r#"
            bitrate = 32000
        "#;
        let config: ClientConfig = toml::from_str(toml_str).expect("parse");
        assert_eq!(config.bitrate, 32000);
        let defaults = ClientConfig::default();
        assert_eq!(config.log_file, defaults.log_file);
        assert_eq!(config.ring_buffer_frames, defaults.ring_buffer_frames);
        assert_eq!(config.max_events, defaults.max_events);
    }

    #[test]
    fn empty_toml_gives_defaults() {
        let config: ClientConfig = toml::from_str("").expect("parse");
        let defaults = ClientConfig::default();
        assert_eq!(config.bitrate, defaults.bitrate);
        assert_eq!(config.log_file, defaults.log_file);
    }

    #[test]
    fn validation_rejects_low_bitrate() {
        let c = ClientConfig {
            bitrate: 100,
            ..ClientConfig::default()
        };
        assert!(c.validate().is_err());
    }

    #[test]
    fn validation_rejects_zero_ring_buffer() {
        let c = ClientConfig {
            ring_buffer_frames: 0,
            ..ClientConfig::default()
        };
        assert!(c.validate().is_err());
    }

    #[test]
    fn validation_rejects_excessive_ring_buffer() {
        let c = ClientConfig {
            ring_buffer_frames: 101,
            ..ClientConfig::default()
        };
        assert!(c.validate().is_err());
    }

    #[test]
    fn validation_rejects_zero_max_events() {
        let c = ClientConfig {
            max_events: 0,
            ..ClientConfig::default()
        };
        assert!(c.validate().is_err());
    }

    #[test]
    fn default_serializes_to_valid_toml() {
        let toml_str = toml::to_string_pretty(&ClientConfig::default()).expect("serialize");
        let _: ClientConfig = toml::from_str(&toml_str).expect("round-trip");
    }
}
