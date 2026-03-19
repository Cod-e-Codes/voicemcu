use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;
use serde::{Deserialize, Serialize};

use voicemcu_common::audio::VAD_RMS_THRESHOLD;
use voicemcu_common::jitter::DEFAULT_JITTER_DEPTH;
use voicemcu_common::protocol::DEFAULT_BITRATE;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Parser)]
#[command(name = "voicemcu-server", about = "MCU voice chat server")]
pub struct Cli {
    /// Print default configuration as TOML and exit
    #[arg(long)]
    pub dump_config: bool,

    /// Path to TOML configuration file
    #[arg(long)]
    pub config: Option<PathBuf>,

    /// Bind address (e.g. 0.0.0.0:4433)
    #[arg(long)]
    pub bind: Option<SocketAddr>,

    /// TLS certificate file (PEM). If the file does not exist, a new
    /// self-signed certificate is generated and saved here.
    #[arg(long)]
    pub cert_file: Option<PathBuf>,

    /// TLS private key file (PEM). Used together with --cert-file.
    #[arg(long)]
    pub key_file: Option<PathBuf>,

    /// Opus bitrate in bits per second
    #[arg(long)]
    pub bitrate: Option<i32>,

    /// Maximum clients per room
    #[arg(long)]
    pub max_room_size: Option<usize>,

    /// Jitter buffer depth in 20 ms frames
    #[arg(long)]
    pub jitter_depth: Option<usize>,

    /// VAD RMS threshold (0.0 - 1.0)
    #[arg(long)]
    pub vad_threshold: Option<f32>,

    /// Empty room cleanup interval in seconds
    #[arg(long)]
    pub cleanup_interval: Option<u64>,

    /// Signaling commands per second per client
    #[arg(long)]
    pub signal_rate: Option<u32>,

    /// Signaling burst capacity per client
    #[arg(long)]
    pub signal_burst: Option<u32>,

    /// Connections per second per IP
    #[arg(long)]
    pub connect_rate_per_ip: Option<u32>,

    /// Connection burst capacity per IP
    #[arg(long)]
    pub connect_burst_per_ip: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub bind: SocketAddr,
    pub cert_file: Option<String>,
    pub key_file: Option<String>,
    pub bitrate: i32,
    pub max_room_size: usize,
    pub jitter_depth: usize,
    pub vad_threshold: f32,
    pub cleanup_interval_secs: u64,
    pub max_display_name: usize,
    pub max_room_code: usize,
    pub datagram_buffer: usize,
    pub signal_rate: u32,
    pub signal_burst: u32,
    pub connect_rate_per_ip: u32,
    pub connect_burst_per_ip: u32,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:4433".parse().expect("valid default"),
            cert_file: None,
            key_file: None,
            bitrate: DEFAULT_BITRATE,
            max_room_size: 64,
            jitter_depth: DEFAULT_JITTER_DEPTH,
            vad_threshold: VAD_RMS_THRESHOLD,
            cleanup_interval_secs: 30,
            max_display_name: 64,
            max_room_code: 128,
            datagram_buffer: 65_536,
            signal_rate: 10,
            signal_burst: 20,
            connect_rate_per_ip: 5,
            connect_burst_per_ip: 10,
        }
    }
}

impl ServerConfig {
    pub fn load(cli: &Cli) -> Result<Self, BoxError> {
        let mut config = if let Some(ref path) = cli.config {
            let contents = std::fs::read_to_string(path)?;
            toml::from_str(&contents)?
        } else {
            Self::default()
        };

        if let Some(bind) = cli.bind {
            config.bind = bind;
        }
        if let Some(ref cert_file) = cli.cert_file {
            config.cert_file = Some(cert_file.display().to_string());
        }
        if let Some(ref key_file) = cli.key_file {
            config.key_file = Some(key_file.display().to_string());
        }
        if let Some(bitrate) = cli.bitrate {
            config.bitrate = bitrate;
        }
        if let Some(max_room_size) = cli.max_room_size {
            config.max_room_size = max_room_size;
        }
        if let Some(jitter_depth) = cli.jitter_depth {
            config.jitter_depth = jitter_depth;
        }
        if let Some(vad_threshold) = cli.vad_threshold {
            config.vad_threshold = vad_threshold;
        }
        if let Some(cleanup_interval) = cli.cleanup_interval {
            config.cleanup_interval_secs = cleanup_interval;
        }
        if let Some(v) = cli.signal_rate {
            config.signal_rate = v;
        }
        if let Some(v) = cli.signal_burst {
            config.signal_burst = v;
        }
        if let Some(v) = cli.connect_rate_per_ip {
            config.connect_rate_per_ip = v;
        }
        if let Some(v) = cli.connect_burst_per_ip {
            config.connect_burst_per_ip = v;
        }

        config.validate()?;
        Ok(config)
    }

    pub fn cert_path(&self) -> Option<PathBuf> {
        self.cert_file.as_ref().map(PathBuf::from)
    }

    pub fn key_path(&self) -> Option<PathBuf> {
        self.key_file.as_ref().map(PathBuf::from)
    }

    pub fn validate(&self) -> Result<(), BoxError> {
        if self.bitrate < 6_000 || self.bitrate > 510_000 {
            return Err("bitrate must be between 6000 and 510000".into());
        }
        if self.max_room_size == 0 {
            return Err("max_room_size must be > 0".into());
        }
        if self.jitter_depth == 0 || self.jitter_depth > 32 {
            return Err("jitter_depth must be between 1 and 32".into());
        }
        if !(0.0..=1.0).contains(&self.vad_threshold) {
            return Err("vad_threshold must be between 0.0 and 1.0".into());
        }
        if self.cleanup_interval_secs == 0 {
            return Err("cleanup_interval_secs must be > 0".into());
        }
        if self.max_display_name == 0 {
            return Err("max_display_name must be > 0".into());
        }
        if self.max_room_code == 0 {
            return Err("max_room_code must be > 0".into());
        }
        if self.signal_rate == 0 {
            return Err("signal_rate must be > 0".into());
        }
        if self.signal_burst == 0 {
            return Err("signal_burst must be > 0".into());
        }
        if self.connect_rate_per_ip == 0 {
            return Err("connect_rate_per_ip must be > 0".into());
        }
        if self.connect_burst_per_ip == 0 {
            return Err("connect_burst_per_ip must be > 0".into());
        }
        match (&self.cert_file, &self.key_file) {
            (Some(_), None) | (None, Some(_)) => {
                return Err("cert_file and key_file must both be set or both be omitted".into());
            }
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid() {
        ServerConfig::default()
            .validate()
            .expect("default should be valid");
    }

    #[test]
    fn parse_full_toml() {
        let toml_str = r#"
            bind = "127.0.0.1:5000"
            cert_file = "cert.pem"
            key_file = "key.pem"
            bitrate = 64000
            max_room_size = 32
            jitter_depth = 6
            vad_threshold = 0.005
            cleanup_interval_secs = 60
            max_display_name = 32
            max_room_code = 64
            datagram_buffer = 131072
            signal_rate = 15
            signal_burst = 30
            connect_rate_per_ip = 8
            connect_burst_per_ip = 16
        "#;
        let config: ServerConfig = toml::from_str(toml_str).expect("parse");
        assert_eq!(config.bind, "127.0.0.1:5000".parse::<SocketAddr>().unwrap());
        assert_eq!(config.cert_file.as_deref(), Some("cert.pem"));
        assert_eq!(config.key_file.as_deref(), Some("key.pem"));
        assert_eq!(config.bitrate, 64000);
        assert_eq!(config.max_room_size, 32);
        assert_eq!(config.jitter_depth, 6);
        assert!((config.vad_threshold - 0.005).abs() < 1e-6);
        assert_eq!(config.cleanup_interval_secs, 60);
        assert_eq!(config.max_display_name, 32);
        assert_eq!(config.max_room_code, 64);
        assert_eq!(config.datagram_buffer, 131072);
        assert_eq!(config.signal_rate, 15);
        assert_eq!(config.signal_burst, 30);
        assert_eq!(config.connect_rate_per_ip, 8);
        assert_eq!(config.connect_burst_per_ip, 16);
    }

    #[test]
    fn partial_toml_uses_defaults() {
        let toml_str = r#"
            bitrate = 32000
        "#;
        let config: ServerConfig = toml::from_str(toml_str).expect("parse");
        assert_eq!(config.bitrate, 32000);
        let defaults = ServerConfig::default();
        assert_eq!(config.bind, defaults.bind);
        assert_eq!(config.max_room_size, defaults.max_room_size);
        assert_eq!(config.jitter_depth, defaults.jitter_depth);
        assert!(config.cert_file.is_none());
        assert!(config.key_file.is_none());
    }

    #[test]
    fn empty_toml_gives_defaults() {
        let config: ServerConfig = toml::from_str("").expect("parse");
        let defaults = ServerConfig::default();
        assert_eq!(config.bitrate, defaults.bitrate);
        assert_eq!(config.bind, defaults.bind);
    }

    #[test]
    fn validation_rejects_low_bitrate() {
        let c = ServerConfig {
            bitrate: 100,
            ..ServerConfig::default()
        };
        assert!(c.validate().is_err());
    }

    #[test]
    fn validation_rejects_high_bitrate() {
        let c = ServerConfig {
            bitrate: 600_000,
            ..ServerConfig::default()
        };
        assert!(c.validate().is_err());
    }

    #[test]
    fn validation_rejects_zero_room_size() {
        let c = ServerConfig {
            max_room_size: 0,
            ..ServerConfig::default()
        };
        assert!(c.validate().is_err());
    }

    #[test]
    fn validation_rejects_zero_jitter_depth() {
        let c = ServerConfig {
            jitter_depth: 0,
            ..ServerConfig::default()
        };
        assert!(c.validate().is_err());
    }

    #[test]
    fn validation_rejects_excessive_jitter_depth() {
        let c = ServerConfig {
            jitter_depth: 33,
            ..ServerConfig::default()
        };
        assert!(c.validate().is_err());
    }

    #[test]
    fn validation_rejects_negative_vad() {
        let c = ServerConfig {
            vad_threshold: -0.1,
            ..ServerConfig::default()
        };
        assert!(c.validate().is_err());
    }

    #[test]
    fn validation_rejects_high_vad() {
        let c = ServerConfig {
            vad_threshold: 1.5,
            ..ServerConfig::default()
        };
        assert!(c.validate().is_err());
    }

    #[test]
    fn validation_accepts_boundary_vad() {
        let c0 = ServerConfig {
            vad_threshold: 0.0,
            ..ServerConfig::default()
        };
        assert!(c0.validate().is_ok());

        let c1 = ServerConfig {
            vad_threshold: 1.0,
            ..ServerConfig::default()
        };
        assert!(c1.validate().is_ok());
    }

    #[test]
    fn default_serializes_to_valid_toml() {
        let toml_str = toml::to_string_pretty(&ServerConfig::default()).expect("serialize");
        let _: ServerConfig = toml::from_str(&toml_str).expect("round-trip");
    }

    #[test]
    fn validation_rejects_cert_without_key() {
        let c = ServerConfig {
            cert_file: Some("cert.pem".into()),
            key_file: None,
            ..ServerConfig::default()
        };
        assert!(c.validate().is_err());
    }

    #[test]
    fn validation_rejects_key_without_cert() {
        let c = ServerConfig {
            cert_file: None,
            key_file: Some("key.pem".into()),
            ..ServerConfig::default()
        };
        assert!(c.validate().is_err());
    }

    #[test]
    fn validation_accepts_both_cert_and_key() {
        let c = ServerConfig {
            cert_file: Some("cert.pem".into()),
            key_file: Some("key.pem".into()),
            ..ServerConfig::default()
        };
        assert!(c.validate().is_ok());
    }

    #[test]
    fn validation_accepts_neither_cert_nor_key() {
        let c = ServerConfig::default();
        assert!(c.cert_file.is_none());
        assert!(c.key_file.is_none());
        assert!(c.validate().is_ok());
    }

    #[test]
    fn validation_rejects_zero_signal_rate() {
        let c = ServerConfig {
            signal_rate: 0,
            ..ServerConfig::default()
        };
        assert!(c.validate().is_err());
    }

    #[test]
    fn validation_rejects_zero_signal_burst() {
        let c = ServerConfig {
            signal_burst: 0,
            ..ServerConfig::default()
        };
        assert!(c.validate().is_err());
    }

    #[test]
    fn validation_rejects_zero_connect_rate() {
        let c = ServerConfig {
            connect_rate_per_ip: 0,
            ..ServerConfig::default()
        };
        assert!(c.validate().is_err());
    }

    #[test]
    fn validation_rejects_zero_connect_burst() {
        let c = ServerConfig {
            connect_burst_per_ip: 0,
            ..ServerConfig::default()
        };
        assert!(c.validate().is_err());
    }

    #[test]
    fn parse_rate_limit_toml() {
        let toml_str = r#"
            signal_rate = 20
            signal_burst = 40
            connect_rate_per_ip = 10
            connect_burst_per_ip = 20
        "#;
        let config: ServerConfig = toml::from_str(toml_str).expect("parse");
        assert_eq!(config.signal_rate, 20);
        assert_eq!(config.signal_burst, 40);
        assert_eq!(config.connect_rate_per_ip, 10);
        assert_eq!(config.connect_burst_per_ip, 20);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn cert_path_helpers() {
        let mut c = ServerConfig::default();
        assert!(c.cert_path().is_none());
        assert!(c.key_path().is_none());

        c.cert_file = Some("/tmp/cert.pem".into());
        c.key_file = Some("/tmp/key.pem".into());
        assert_eq!(c.cert_path().unwrap(), PathBuf::from("/tmp/cert.pem"));
        assert_eq!(c.key_path().unwrap(), PathBuf::from("/tmp/key.pem"));
    }
}
