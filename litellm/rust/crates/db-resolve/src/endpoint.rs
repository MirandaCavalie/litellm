use std::{collections::BTreeMap, iter, path::PathBuf};

use litellm_db::{Endpoint, PoolLimits, SslMode};
use litellm_python_compat::number;

use crate::{Error, Ignored, location::Location, settings::PoolSettings};

const PRISMA_APPLICATION_NAME: &str = "litellm-prisma";
const NATIVE_APPLICATION_NAME: &str = "litellm-native";
const DEFAULT_SCHEMA: &str = "public";
const DEFAULT_STATEMENT_CACHE_SIZE: usize = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Precedence {
    Configured,
    Pinned,
}

struct Tls<'a> {
    mode: &'a str,
    strict: bool,
    root_cert: Option<&'a str>,
}

impl<'a> Tls<'a> {
    fn read(params: &'a BTreeMap<String, String>) -> Self {
        let mode = param(params, "sslmode").unwrap_or("prefer");
        let verifies = matches!(mode, "verify-ca" | "verify-full");
        let libpq = verifies || param(params, "sslrootcert").is_some();
        Self {
            mode: if verifies { "require" } else { mode },
            strict: param(params, "sslaccept").or(libpq.then_some("strict")) == Some("strict"),
            root_cert: param(params, "sslcert").or_else(|| param(params, "sslrootcert")),
        }
    }

    fn needs_probe(&self) -> bool {
        self.mode == "prefer" && self.strict
    }

    fn ssl(&self, tls_offered: bool) -> (SslMode, Option<PathBuf>) {
        if self.mode == "disable" || (self.needs_probe() && !tls_offered) {
            return (SslMode::Disable, None);
        }
        if self.strict {
            return (SslMode::VerifyFull, self.root_cert.map(PathBuf::from));
        }
        match self.mode {
            "require" => (SslMode::Require, None),
            _ => (SslMode::Prefer, None),
        }
    }
}

pub(crate) fn needs_tls_probe(location: &Location) -> bool {
    Tls::read(&location.params).needs_probe() && !location.params.contains_key("host")
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
    let max_connections = match (precedence, param(params, "connection_limit")) {
        (Precedence::Pinned, Some(text)) => text
            .parse::<u32>()
            .ok()
            .filter(|limit| *limit > 0)
            .ok_or_else(|| invalid(location, "connection_limit", "a positive integer"))?,
        _ => pool.connection_limit,
    };
    let unnamed = pool.unnamed_statements || param(params, "pgbouncer") == Some("true");
    let statement_cache_capacity = match (unnamed, param(params, "statement_cache_size")) {
        (true, _) => 0,
        (false, Some(text)) => text
            .parse::<usize>()
            .map_err(|_| invalid(location, "statement_cache_size", "a non-negative integer"))?,
        (false, None) => DEFAULT_STATEMENT_CACHE_SIZE,
    };
    let client_identity = (param(params, "sslidentity").is_some() && ssl_mode != SslMode::Disable)
        .then(|| Ignored {
            setting: format!("{} ?sslidentity", location.source),
            consequence: "the native pool sends no client certificate, so a server that requires one refuses its connections",
        });
    let ignored = unsent
        .into_iter()
        .map(|token| Ignored {
            setting: format!("{} options {token}", location.source),
            consequence: "only -c name=value settings can be sent at startup",
        })
        .chain(client_identity)
        .collect();
    let endpoint = Endpoint {
        host: location.host.clone(),
        port: location.port,
        socket: param(params, "host").map(PathBuf::from),
        username: location.username.clone(),
        password: location.password.clone(),
        database: location.database.clone(),
        ssl_mode,
        ssl_root_cert,
        ssl_client: None,
        application_name: Some(match param(params, "application_name") {
            Some(name) if name != PRISMA_APPLICATION_NAME => name.to_owned(),
            _ => NATIVE_APPLICATION_NAME.to_owned(),
        }),
        options,
        statement_cache_capacity,
        pool: PoolLimits {
            max_connections,
            acquire_timeout_seconds: seconds(
                location,
                "pool_timeout",
                Some(pool.pool_timeout),
                precedence,
            )?,
            idle_timeout_seconds: seconds(
                location,
                "max_idle_connection_lifetime",
                Some(pool.idle_lifetime),
                Precedence::Pinned,
            )?,
            socket_timeout_seconds: seconds(
                location,
                "socket_timeout",
                pool.socket_timeout,
                precedence,
            )?,
        },
    };
    Ok((endpoint, ignored))
}

fn param<'a>(params: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    params.get(key).map(String::as_str)
}

fn invalid(location: &Location, key: &str, expected: &'static str) -> Error {
    Error::Invalid {
        setting: format!("{} ?{key}", location.source),
        expected,
    }
}

fn seconds(
    location: &Location,
    key: &str,
    configured: Option<f64>,
    precedence: Precedence,
) -> Result<Option<f64>, Error> {
    let pinned = || {
        param(&location.params, key)
            .map(|text| {
                number::float(text).ok_or_else(|| invalid(location, key, "a number of seconds"))
            })
            .transpose()
    };
    let value = match (precedence, configured) {
        (Precedence::Configured, Some(value)) => Some(value),
        (Precedence::Configured, None) => pinned()?,
        (Precedence::Pinned, configured) => pinned()?.or(configured),
    };
    Ok(value.filter(|seconds| *seconds != 0.0))
}

fn session_options(
    params: &BTreeMap<String, String>,
    timeouts: &[(&'static str, i64)],
) -> (Vec<(String, String)>, Vec<String>) {
    let search_path = param(params, "schema")
        .filter(|schema| *schema != DEFAULT_SCHEMA)
        .map(|schema| ("search_path".to_owned(), format!("\"{schema}\"")));
    let pinned = param(params, "options").unwrap_or_default();
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
