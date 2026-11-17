use std::collections::BTreeMap;

use litellm_env::Lookup;
use litellm_secret_value::SecretValue;
use percent_encoding::percent_decode_str;
use url::{Host, Url};

use crate::Error;

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
    pub params: BTreeMap<String, String>,
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
        let params: BTreeMap<String, String> = url
            .query_pairs()
            .filter(|(_, value)| !value.is_empty())
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        let host = match url.host() {
            Some(Host::Domain(name)) => name.to_lowercase(),
            Some(Host::Ipv4(address)) => address.to_string(),
            Some(Host::Ipv6(address)) => address.to_string(),
            None => String::new(),
        };
        let username = decoded(url.username());
        if (host.is_empty() && !params.contains_key("host")) || username.is_empty() {
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

struct Fields {
    host: Option<String>,
    port: Option<String>,
    user: Option<String>,
    password: Option<String>,
    name: Option<String>,
    schema: Option<String>,
}

impl Fields {
    fn writer(lookup: &dyn Lookup) -> Self {
        Self {
            host: lookup.truthy("DATABASE_HOST"),
            port: lookup.truthy("DATABASE_PORT"),
            user: lookup
                .truthy("DATABASE_USER")
                .or_else(|| lookup.truthy("DATABASE_USERNAME")),
            password: lookup.truthy("DATABASE_PASSWORD"),
            name: lookup.truthy("DATABASE_NAME"),
            schema: lookup.truthy("DATABASE_SCHEMA"),
        }
    }

    fn reader(lookup: &dyn Lookup) -> Self {
        let writer = Self::writer(lookup);
        Self {
            host: lookup.truthy("DATABASE_HOST_READ_REPLICA"),
            port: lookup.truthy("DATABASE_PORT_READ_REPLICA").or(writer.port),
            user: lookup
                .truthy("DATABASE_USER_READ_REPLICA")
                .or_else(|| lookup.truthy("DATABASE_USERNAME_READ_REPLICA"))
                .or(writer.user),
            password: lookup
                .truthy("DATABASE_PASSWORD_READ_REPLICA")
                .or(writer.password),
            name: lookup.truthy("DATABASE_NAME_READ_REPLICA").or(writer.name),
            schema: lookup
                .truthy("DATABASE_SCHEMA_READ_REPLICA")
                .or(writer.schema),
        }
    }

    fn location(
        self,
        source: &'static str,
        lookup: &dyn Lookup,
    ) -> Result<Option<Location>, Error> {
        let (Some(host), Some(username), Some(database)) = (self.host, self.user, self.name) else {
            return Ok(None);
        };
        let port = match self.port {
            Some(text) => text.trim().parse().map_err(|_| Error::Invalid {
                setting: format!("{source} port"),
                expected: "a port number",
            })?,
            None => DEFAULT_PORT,
        };
        let root_cert = lookup.truthy("DATABASE_SSLROOTCERT");
        let ssl_mode = lookup
            .truthy("DATABASE_SSLMODE")
            .or_else(|| root_cert.as_ref().map(|_| "verify-full".to_owned()));
        let params = [
            ("schema", self.schema),
            ("sslmode", ssl_mode),
            ("sslrootcert", root_cert),
        ]
        .into_iter()
        .filter_map(|(key, value)| Some((key.to_owned(), value?)))
        .collect();
        Ok(Some(Location {
            source,
            host: host.to_lowercase(),
            port,
            username,
            password: self.password.map(SecretValue::new),
            database,
            params,
        }))
    }
}

pub(crate) fn writer(
    configured: Option<&SecretValue>,
    lookup: &dyn Lookup,
) -> Result<Option<Location>, Error> {
    let configured = configured
        .map(SecretValue::expose)
        .filter(|url| !url.is_empty());
    if let Some(url) = configured {
        return Location::from_url("general_settings.database_url", url).map(Some);
    }
    if let Some(url) = lookup.truthy("DATABASE_URL") {
        return Location::from_url("DATABASE_URL", &url).map(Some);
    }
    Fields::writer(lookup).location("DATABASE_HOST", lookup)
}

pub(crate) fn reader(lookup: &dyn Lookup) -> Result<Option<Location>, Error> {
    if let Some(url) = lookup.truthy("DATABASE_URL_READ_REPLICA") {
        return Location::from_url("DATABASE_URL_READ_REPLICA", &url).map(Some);
    }
    Fields::reader(lookup).location("DATABASE_HOST_READ_REPLICA", lookup)
}
