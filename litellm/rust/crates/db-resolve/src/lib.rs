mod endpoint;
mod error;
mod location;
mod probe;
mod settings;

use litellm_config::GeneralSettings;
use litellm_db::Endpoint;
use litellm_env::Lookup;

pub use error::Error;

use endpoint::Precedence;
use location::Location;
use settings::PoolSettings;

const TOKEN_AUTH_FLAGS: [&str; 2] = ["IAM_TOKEN_DB_AUTH", "AZURE_POSTGRESQL_AUTH"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ignored {
    pub setting: String,
    pub consequence: &'static str,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Database {
    pub writer: Endpoint,
    pub reader: Option<Endpoint>,
    pub ignored: Vec<Ignored>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ConfiguredDatabase {
    writer: Location,
    reader: Option<Location>,
    pool: PoolSettings,
}

impl ConfiguredDatabase {
    pub fn read(general: &GeneralSettings, lookup: &dyn Lookup) -> Result<Option<Self>, Error> {
        for variable in TOKEN_AUTH_FLAGS {
            if settings::flag(variable, lookup.get(variable).as_deref())? {
                return Err(Error::TokenAuth { variable });
            }
        }
        let Some(writer) = location::writer(general.database_url.as_ref(), lookup)? else {
            return Ok(None);
        };
        Ok(Some(Self {
            writer,
            reader: location::reader(lookup)?,
            pool: PoolSettings::read(general, lookup)?,
        }))
    }

    pub async fn resolve(&self) -> Result<Database, Error> {
        let (writer, writer_ignored) = self.side(&self.writer, Precedence::Configured).await?;
        let (reader, reader_ignored) = match &self.reader {
            Some(location) => {
                let (endpoint, ignored) = self.side(location, Precedence::Pinned).await?;
                (Some(endpoint), ignored)
            }
            None => (None, Vec::new()),
        };
        Ok(Database {
            writer,
            reader,
            ignored: writer_ignored.into_iter().chain(reader_ignored).collect(),
        })
    }

    async fn side(
        &self,
        location: &Location,
        precedence: Precedence,
    ) -> Result<(Endpoint, Vec<Ignored>), Error> {
        let tls_offered = !endpoint::needs_tls_probe(location)
            || probe::server_offers_tls(&location.host, location.port).await;
        endpoint::endpoint(location, &self.pool, precedence, tls_offered)
    }
}
