use thiserror::Error;

#[derive(Debug, Error)]
pub enum GatewayError {
    #[error("control-plane HTTP error: {0}")]
    ControlPlane(String),
    #[error("invalid input: {0}")]
    Invalid(String),
    #[error("task timeout after {seconds}s")]
    TaskTimeout { seconds: u64 },
    #[error("task failed: {0}")]
    TaskFailed(String),
    #[error("unsupported feature: {0}")]
    Unsupported(&'static str),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type GatewayResult<T> = Result<T, GatewayError>;
