use thiserror::Error;

#[derive(Error, Debug)]
pub enum VoiceError {
    #[error("opus codec error: {0}")]
    Codec(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

impl From<audiopus::Error> for VoiceError {
    fn from(e: audiopus::Error) -> Self {
        VoiceError::Codec(format!("{e:?}"))
    }
}

impl From<postcard::Error> for VoiceError {
    fn from(e: postcard::Error) -> Self {
        VoiceError::Protocol(format!("{e}"))
    }
}
