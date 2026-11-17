use std::sync::Arc;

use litellm_config::Config;
use litellm_config_load::{ConfigEnvironment, environment_values};
use litellm_db::Pools;
use litellm_db_resolve::ConfiguredDatabase;
use litellm_env::ProcessEnvironment;

use crate::Error;

pub(crate) async fn connect(config: &Config) -> Result<Option<Pools>, Error> {
    let environment = ConfigEnvironment::new(
        environment_values(&config.environment_variables)?,
        Arc::new(ProcessEnvironment),
    );
    let configured = match ConfiguredDatabase::read(&config.general_settings, &environment) {
        Ok(Some(configured)) => configured,
        Ok(None) => return Ok(None),
        Err(error @ litellm_db_resolve::Error::TokenAuth { .. }) => {
            tracing::warn!("{error}, so the gateway runs without a database pool");
            return Ok(None);
        }
        Err(error) => return Err(error.into()),
    };
    let database = configured.resolve().await?;
    for ignored in &database.ignored {
        tracing::warn!(
            "Database setting {} is ignored by the native pool: {}",
            ignored.setting,
            ignored.consequence
        );
    }
    Ok(Some(Pools::new(
        &database.writer,
        database.reader.as_ref(),
    )?))
}
