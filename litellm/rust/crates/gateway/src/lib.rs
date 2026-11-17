mod db;
mod error;
mod health;
mod license;
pub mod middleware;
mod redis;
pub mod screen;
mod state;
pub use error::Error;
pub use license::build_license;
pub use redis::RedisClients;
pub use state::AppState;

use std::{path::Path, sync::Arc};

use axum::Router;

use litellm_config::Config;
use litellm_config_load::{ConfigEnvironment, environment_values};
use litellm_env::{Lookup, ProcessEnvironment};
use litellm_gateway_auth::Auth;
use litellm_gateway_inference::{Gateway, ModelRouter};
use litellm_http::{
    ClientVariant, HttpClientPool, HttpSettings, HttpSettingsLayer, Resolution, SslVerify,
    media::PublicDnsResolver,
};
use litellm_inference::resources::CoreResources;
use litellm_model_catalog::{BUNDLED_REGISTRY, Catalog};
use litellm_secrets::{OidcResolver, SecretManagerState, SecretResolver};

/// The config at `path`, its `os.environ/` references read from the process environment.
pub async fn load_config(path: impl AsRef<Path>) -> Result<Config, Error> {
    let pool = HttpClientPool::new(Arc::new(PublicDnsResolver));
    let lookup = |name: &str| std::env::var(name).ok();
    let settings = HttpSettings::from_layers([HttpSettingsLayer::from_environment(&lookup)]);
    let client = pool.client(&Resolution::from(&settings).config, ClientVariant::Provider)?;
    let secrets = environment_secrets(Arc::new(ProcessEnvironment), client);
    Ok(litellm_config_load::load(path, secrets).await?)
}

fn environment_secrets(
    environment: Arc<dyn Lookup + Send + Sync>,
    client: litellm_http::Client,
) -> SecretResolver {
    SecretResolver::new_python_compatible(
        Arc::new(SecretManagerState::default()),
        environment,
        OidcResolver::new(client),
    )
}

pub fn build_inference(config: &Config) -> Result<Arc<Gateway>, Error> {
    let pool = Arc::new(HttpClientPool::new(Arc::new(PublicDnsResolver)));
    let environment = Arc::new(ConfigEnvironment::new(
        environment_values(&config.environment_variables)?,
        Arc::new(ProcessEnvironment),
    ));
    let lookup = |name: &str| environment.get(name);
    let catalog = Catalog::parse(BUNDLED_REGISTRY)?.value;
    let router = ModelRouter::from_config(config, &catalog);
    let settings = &config.litellm_settings;
    let http_settings = HttpSettings::from_layers([
        HttpSettingsLayer::from_environment(&lookup),
        HttpSettingsLayer {
            ssl_verify: settings.ssl_verify.as_ref().map(|value| match value {
                litellm_config::Spelled::Value(true) => SslVerify::Enabled,
                litellm_config::Spelled::Value(false) => SslVerify::Disabled,
                litellm_config::Spelled::Text(value) => SslVerify::parse(value),
            }),
            ssl_certificate: settings.ssl_certificate.as_ref().map(Into::into),
            ssl_security_level: settings.ssl_security_level.clone(),
            ssl_ecdh_curve: settings.ssl_ecdh_curve.clone(),
            force_ipv4: settings.force_ipv4,
            http2: settings.http2,
            aiohttp_trust_env: settings.aiohttp_trust_env,
            disable_aiohttp_trust_env: settings.disable_aiohttp_trust_env,
            disable_aiohttp_transport: settings.disable_aiohttp_transport,
            ..Default::default()
        },
    ]);
    let http = Resolution::from(&http_settings).config;
    let client = pool.client(&http, ClientVariant::Provider)?;
    let secrets = Arc::new(environment_secrets(environment.clone(), client));
    let resources = CoreResources::new(pool);
    Ok(Arc::new(Gateway::new(resources, http, secrets, router)?))
}

pub fn install_crypto_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
}

pub(crate) async fn build_redis(
    config: &Config,
    pool: &HttpClientPool,
    http: &litellm_http::HttpClientConfig,
) -> Result<RedisClients, Error> {
    let environment = Arc::new(environment_values(&config.environment_variables)?);
    let lookup: litellm_redis_connect::SharedEnvironment = Arc::new(move |name: &str| {
        environment
            .get(name)
            .map(|value| value.expose().to_owned())
            .or_else(|| std::env::var(name).ok())
    });
    let resources = redis::Resources {
        http: pool.client(http, ClientVariant::Provider)?,
        lookup: lookup.clone(),
    };
    redis::connect(config, &*lookup, &resources).await
}

pub fn router(state: AppState, config: &Config) -> Router {
    let auth = Auth::from_config(config);
    let inference = litellm_gateway_inference::router(state.inference.clone()).route_layer(
        axum::middleware::from_fn_with_state(auth, litellm_gateway_auth::authenticate),
    );
    middleware::inference(inference).merge(health::router(state))
}
