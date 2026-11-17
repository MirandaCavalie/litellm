use std::{iter, path::PathBuf};

use litellm_db::{Endpoint, PoolLimits, SslMode};
use litellm_python_compat::number;

use crate::{
    Error, Expected, Ignored,
    location::Location,
    params::{Params, SslModeText},
    settings::PoolSettings,
};

const PRISMA_APPLICATION_NAME: &str = "litellm-prisma";
const NATIVE_APPLICATION_NAME: &str = "litellm-native";
const DEFAULT_SCHEMA: &str = "public";
const DEFAULT_STATEMENT_CACHE_SIZE: usize = 100;
const STRICT: &str = "strict";
const ENABLED: &str = "true";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Precedence {
    Configured,
    Pinned,
}

struct Pinned<'a> {
    key: &'static str,
    text: Option<&'a str>,
}

macro_rules! pinned {
    ($params:expr, $field:ident) => {
        Pinned {
            key: stringify!($field),
            text: $params.$field.as_deref(),
        }
    };
}

struct Tls<'a> {
    mode: SslModeText,
    strict: bool,
    root_cert: Option<&'a str>,
}

impl<'a> Tls<'a> {
    fn read(params: &'a Params) -> Self {
        let mode = params.ssl_mode();
        let verifies = matches!(mode, SslModeText::VerifyCa | SslModeText::VerifyFull);
        let libpq = verifies || params.sslrootcert.is_some();
        Self {
            mode: if verifies { SslModeText::Require } else { mode },
            strict: params
                .sslaccept
                .as_deref()
                .map_or(libpq, |accept| accept == STRICT),
            root_cert: params.sslcert.as_deref().or(params.sslrootcert.as_deref()),
        }
    }

    fn needs_probe(&self) -> bool {
        self.mode == SslModeText::Prefer && self.strict
    }

    fn ssl(&self, tls_offered: bool) -> (SslMode, Option<PathBuf>) {
        if self.mode == SslModeText::Disable || (self.needs_probe() && !tls_offered) {
            return (SslMode::Disable, None);
        }
        if self.strict {
            return (SslMode::VerifyFull, self.root_cert.map(PathBuf::from));
        }
        match self.mode {
            SslModeText::Require => (SslMode::Require, None),
            SslModeText::Disable
            | SslModeText::Prefer
            | SslModeText::VerifyCa
            | SslModeText::VerifyFull
            | SslModeText::Unknown => (SslMode::Prefer, None),
        }
    }
}

pub(crate) fn needs_tls_probe(location: &Location) -> bool {
    Tls::read(&location.params).needs_probe() && location.params.host.is_none()
}

pub(crate) fn endpoint(
    location: &Location,
    pool: &PoolSettings,
    precedence: Precedence,
    tls_offered: bool,
) -> Result<(Endpoint, Vec<Ignored>), Error> {
    let params = &location.params;
    let (ssl_mode, ssl_root_cert) = Tls::read(params).ssl(tls_offered);
    let (options, unsent) = session_options(params, &pool.session_timeouts);
    let connection_limit = pinned!(params, connection_limit);
    let max_connections = match (precedence, connection_limit.text) {
        (Precedence::Pinned, Some(text)) => text
            .parse::<u32>()
            .ok()
            .filter(|limit| *limit > 0)
            .ok_or_else(|| invalid(location, connection_limit.key, Expected::PositiveInteger))?,
        _ => pool.connection_limit,
    };
    let unnamed = pool.unnamed_statements || params.pgbouncer.as_deref() == Some(ENABLED);
    let cache_size = pinned!(params, statement_cache_size);
    let statement_cache_capacity = match (unnamed, cache_size.text) {
        (true, _) => 0,
        (false, Some(text)) => text
            .parse::<usize>()
            .map_err(|_| invalid(location, cache_size.key, Expected::NonNegativeInteger))?,
        (false, None) => DEFAULT_STATEMENT_CACHE_SIZE,
    };
    let identity = pinned!(params, sslidentity);
    let client_identity = (identity.text.is_some() && ssl_mode != SslMode::Disable).then(|| Ignored {
        setting: format!("{} ?{}", location.source, identity.key),
        consequence: "the native pool sends no client certificate, so a server that requires one refuses its connections",
    });
    let options_key = pinned!(params, options).key;
    let ignored = unsent
        .into_iter()
        .map(|token| Ignored {
            setting: format!("{} {options_key} {token}", location.source),
            consequence: "only -c name=value settings can be sent at startup",
        })
        .chain(client_identity)
        .collect();
    let endpoint = Endpoint {
        host: location.host.clone(),
        port: location.port,
        socket: params.host.as_deref().map(PathBuf::from),
        username: location.username.clone(),
        password: location.password.clone(),
        database: location.database.clone(),
        ssl_mode,
        ssl_root_cert,
        ssl_client: None,
        application_name: Some(match params.application_name.as_deref() {
            Some(name) if name != PRISMA_APPLICATION_NAME => name.to_owned(),
            _ => NATIVE_APPLICATION_NAME.to_owned(),
        }),
        options,
        statement_cache_capacity,
        pool: PoolLimits {
            max_connections,
            acquire_timeout_seconds: seconds(
                location,
                pinned!(params, pool_timeout),
                Some(pool.pool_timeout),
                precedence,
            )?,
            idle_timeout_seconds: seconds(
                location,
                pinned!(params, max_idle_connection_lifetime),
                Some(pool.idle_lifetime),
                Precedence::Pinned,
            )?,
            socket_timeout_seconds: seconds(
                location,
                pinned!(params, socket_timeout),
                pool.socket_timeout,
                precedence,
            )?,
        },
    };
    Ok((endpoint, ignored))
}

fn invalid(location: &Location, key: &str, expected: Expected) -> Error {
    Error::Invalid {
        setting: format!("{} ?{key}", location.source),
        expected,
    }
}

fn seconds(
    location: &Location,
    pinned: Pinned<'_>,
    configured: Option<f64>,
    precedence: Precedence,
) -> Result<Option<f64>, Error> {
    let read = || {
        pinned
            .text
            .map(|text| {
                number::float(text).ok_or_else(|| invalid(location, pinned.key, Expected::Seconds))
            })
            .transpose()
    };
    let value = match (precedence, configured) {
        (Precedence::Configured, Some(value)) => Some(value),
        (Precedence::Configured, None) => read()?,
        (Precedence::Pinned, configured) => read()?.or(configured),
    };
    Ok(value.filter(|seconds| *seconds != 0.0))
}

fn session_options(
    params: &Params,
    timeouts: &[(&'static str, i64)],
) -> (Vec<(String, String)>, Vec<String>) {
    let search_path = params
        .schema
        .as_deref()
        .filter(|schema| *schema != DEFAULT_SCHEMA)
        .map(|schema| ("search_path".to_owned(), format!("\"{schema}\"")));
    let pinned = params.options.as_deref().unwrap_or_default();
    let (settings, unsent): (Vec<(String, String)>, Vec<String>) = match shlex::split(pinned) {
        Some(tokens) => {
            let folded = fold(&tokens);
            (
                folded.iter().filter_map(|token| setting(token)).collect(),
                folded
                    .iter()
                    .filter(|token| setting(token).is_none())
                    .cloned()
                    .collect(),
            )
        }
        None => (Vec::new(), vec![pinned.to_owned()]),
    };
    let configured: Vec<(String, String)> = timeouts
        .iter()
        .filter(|timeout| !settings.iter().any(|(name, _)| name.as_str() == timeout.0))
        .map(|(name, milliseconds)| ((*name).to_owned(), milliseconds.to_string()))
        .collect();
    let options = search_path
        .into_iter()
        .chain(settings)
        .chain(configured)
        .collect();
    (options, unsent)
}

fn fold(tokens: &[String]) -> Vec<String> {
    let previous = iter::once(None).chain(tokens.iter().map(Some));
    let following = tokens.iter().skip(1).map(Some).chain(iter::once(None));
    tokens
        .iter()
        .zip(previous)
        .zip(following)
        .filter(|((_, previous), _)| previous.is_none_or(|token| token.as_str() != "-c"))
        .map(|((token, _), following)| match following {
            Some(next) if token.as_str() == "-c" => format!("-c{next}"),
            _ => token.clone(),
        })
        .collect()
}

fn setting(token: &str) -> Option<(String, String)> {
    let rest = token
        .strip_prefix("-c")
        .or_else(|| token.strip_prefix("--"))?;
    let (name, value) = rest.split_once('=')?;
    Some((name.to_owned(), value.to_owned()))
}
