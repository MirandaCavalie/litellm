#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{setting} is not a URL the database pool can read")]
    Url { setting: &'static str },
    #[error("{setting} uses unsupported scheme '{scheme}', the database must be PostgreSQL")]
    Scheme {
        setting: &'static str,
        scheme: String,
    },
    #[error("{setting} must name a host and a user")]
    Incomplete { setting: &'static str },
    #[error("invalid database setting {setting}: expected {expected}")]
    Invalid {
        setting: String,
        expected: &'static str,
    },
    #[error("{variable} asks for a minted database token, which the Rust gateway cannot mint yet")]
    TokenAuth { variable: &'static str },
}
