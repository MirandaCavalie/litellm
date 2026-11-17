use std::collections::BTreeMap;

use litellm_secret_value::SecretValue;
use percent_encoding::percent_decode_str;
use serde::Deserialize;
use url::{Host, Url};

use crate::{
    Error, Expected,
    params::{Params, SslModeText},
};

const PREFIX: &str = "DATABASE_";
const REPLICA_SUFFIX: &str = "_READ_REPLICA";
const DEFAULT_PORT: u16 = 5432;
const DEFAULT_DATABASE: &str = "postgres";

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Location {
    pub source: &'static str,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: Option<SecretValue>,
    pub database: String,
    pub params: Params,
}

impl Location {
    fn from_url(source: &'static str, text: &str) -> Result<Self, Error> {
        let url = Url::parse(text).map_err(|_| Error::Url { setting: source })?;
        if !matches!(url.scheme(), "postgresql" | "postgres") {
            return Err(Error::Scheme {
                setting: source,
                scheme: url.scheme().to_owned(),
            });
        }
        let params = Params::from_url(source, &url)?;
        let host = match url.host() {
            Some(Host::Domain(name)) => name.to_lowercase(),
            Some(Host::Ipv4(address)) => address.to_string(),
            Some(Host::Ipv6(address)) => address.to_string(),
            None => String::new(),
        };
        let username = decoded(url.username());
        if (host.is_empty() && params.host.is_none()) || username.is_empty() {
            return Err(Error::Incomplete { setting: source });
        }
        let database = decoded(url.path().trim_start_matches('/'));
        Ok(Self {
            source,
            host: if host.is_empty() {
                "localhost".to_owned()
            } else {
                host
            },
            port: url.port().unwrap_or(DEFAULT_PORT),
            username,
            password: url
                .password()
                .map(decoded)
                .filter(|password| !password.is_empty())
                .map(SecretValue::new),
            database: if database.is_empty() {
                DEFAULT_DATABASE.to_owned()
            } else {
                database
            },
            params,
        })
    }
}

fn decoded(text: &str) -> String {
    percent_decode_str(text).decode_utf8_lossy().into_owned()
}

/// Each field is the `DATABASE_<FIELD>` variable, and `DATABASE_<FIELD>_READ_REPLICA` on the replica.
#[derive(Deserialize)]
struct Fields {
    url: Option<String>,
    host: Option<String>,
    port: Option<String>,
    user: Option<String>,
    username: Option<String>,
    password: Option<String>,
    name: Option<String>,
    schema: Option<String>,
    sslmode: Option<String>,
    sslrootcert: Option<String>,
}

impl Fields {
    fn writer(variables: &BTreeMap<String, String>) -> Result<Self, Error> {
        Self::read(variables, "")
    }

    fn replica(variables: &BTreeMap<String, String>) -> Result<Self, Error> {
        let own = Self::read(variables, REPLICA_SUFFIX)?;
        let writer = Self::writer(variables)?;
        Ok(Self {
            url: own.url,
            host: own.host,
            port: own.port.or(writer.port),
            user: own
                .user
                .or(own.username)
                .or(writer.user)
                .or(writer.username),
            username: None,
            password: own.password.or(writer.password),
            name: own.name.or(writer.name),
            schema: own.schema.or(writer.schema),
            sslmode: writer.sslmode,
            sslrootcert: writer.sslrootcert,
        })
    }

    // envy matches names in any case, and the proxy reads these exact names
    fn read(variables: &BTreeMap<String, String>, suffix: &str) -> Result<Self, Error> {
        let named = variables
            .iter()
            .filter(|(name, value)| {
                !value.is_empty() && !name.contains(|letter: char| letter.is_ascii_lowercase())
            })
            .filter_map(|(name, value)| {
                let field = name.strip_prefix(PREFIX)?.strip_suffix(suffix)?;
                Some((field.to_owned(), value.clone()))
            });
        Ok(envy::from_iter(named)?)
    }

    fn location(self, source: &'static str) -> Result<Option<Location>, Error> {
        let (Some(host), Some(username), Some(database)) =
            (self.host, self.user.or(self.username), self.name)
        else {
            return Ok(None);
        };
        let port = match self.port {
            Some(text) => text.trim().parse().map_err(|_| Error::Invalid {
                setting: format!("{source} port"),
                expected: Expected::Port,
            })?,
            None => DEFAULT_PORT,
        };
        let verified: &'static str = SslModeText::VerifyFull.into();
        let sslmode = self
            .sslmode
            .or_else(|| self.sslrootcert.as_ref().map(|_| verified.to_owned()));
        Ok(Some(Location {
            source,
            host: host.to_lowercase(),
            port,
            username,
            password: self.password.map(SecretValue::new),
            database,
            params: Params {
                schema: self.schema,
                sslmode,
                sslrootcert: self.sslrootcert,
                ..Params::default()
            },
        }))
    }
}

pub(crate) fn writer(
    configured: Option<&SecretValue>,
    variables: &BTreeMap<String, String>,
) -> Result<Option<Location>, Error> {
    let configured = configured
        .map(SecretValue::expose)
        .filter(|url| !url.is_empty());
    if let Some(url) = configured {
        return Location::from_url("general_settings.database_url", url).map(Some);
    }
    let fields = Fields::writer(variables)?;
    if let Some(url) = &fields.url {
        return Location::from_url("DATABASE_URL", url).map(Some);
    }
    fields.location("DATABASE_HOST")
}

pub(crate) fn reader(variables: &BTreeMap<String, String>) -> Result<Option<Location>, Error> {
    let fields = Fields::replica(variables)?;
    if let Some(url) = &fields.url {
        return Location::from_url("DATABASE_URL_READ_REPLICA", url).map(Some);
    }
    fields.location("DATABASE_HOST_READ_REPLICA")
}
