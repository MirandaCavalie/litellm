use std::{collections::BTreeMap, path::PathBuf};

use litellm_config::{Config, GeneralSettings};
use litellm_db::{Endpoint, PoolLimits, SslMode};
use litellm_db_resolve::{ConfiguredDatabase, Database, Error};
use litellm_secret_value::SecretValue;
use rstest::rstest;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

const WRITER: &str = "postgresql://app:secret@db.internal/proxy";

fn general(yaml: &str) -> GeneralSettings {
    Config::from_yaml(yaml).unwrap().general_settings
}

fn environment(variables: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let variables: BTreeMap<String, String> = variables
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect();
    move |name: &str| variables.get(name).cloned()
}

fn read(yaml: &str, variables: &[(&str, &str)]) -> Result<Option<ConfiguredDatabase>, Error> {
    ConfiguredDatabase::read(&general(yaml), &environment(variables))
}

async fn resolve(yaml: &str, variables: &[(&str, &str)]) -> Database {
    read(yaml, variables)
        .unwrap()
        .unwrap()
        .resolve()
        .await
        .unwrap()
}

async fn resolve_url(url: &str) -> Database {
    resolve("model_list: []", &[("DATABASE_URL", url)]).await
}

fn endpoint() -> Endpoint {
    Endpoint {
        host: "db.internal".to_owned(),
        port: 5432,
        socket: None,
        username: "app".to_owned(),
        password: Some(SecretValue::new("secret")),
        database: "proxy".to_owned(),
        ssl_mode: SslMode::Prefer,
        ssl_root_cert: None,
        ssl_client: None,
        application_name: Some("litellm-native".to_owned()),
        options: Vec::new(),
        statement_cache_capacity: 100,
        pool: PoolLimits {
            max_connections: 10,
            acquire_timeout_seconds: Some(60.0),
            idle_timeout_seconds: Some(60.0),
            socket_timeout_seconds: None,
        },
    }
}

#[tokio::test]
async fn the_configured_url_wins_and_its_settings_override_what_the_url_pins() {
    let yaml = r#"
general_settings:
  database_url: "postgresql://app:p%40ss@DB.internal:6543/proxy?connection_limit=50&pool_timeout=5&schema=tenant&options=-c%20statement_timeout%3D5000"
  database_connection_pool_limit: 20
  database_socket_timeout: 7
  database_statement_timeout: 30
  database_lock_timeout: 2.5
"#;

    let database = resolve(
        yaml,
        &[("DATABASE_URL", "postgresql://other@elsewhere/other")],
    )
    .await;

    assert_eq!(
        database,
        Database {
            writer: Endpoint {
                port: 6543,
                password: Some(SecretValue::new("p@ss")),
                options: vec![
                    ("search_path".to_owned(), "\"tenant\"".to_owned()),
                    ("statement_timeout".to_owned(), "5000".to_owned()),
                    ("lock_timeout".to_owned(), "2500".to_owned()),
                ],
                pool: PoolLimits {
                    max_connections: 20,
                    socket_timeout_seconds: Some(7.0),
                    ..endpoint().pool
                },
                ..endpoint()
            },
            reader: None,
            ignored: Vec::new(),
        }
    );
}

#[tokio::test]
async fn discrete_variables_assemble_the_writer_and_a_root_cert_alone_verifies() {
    let database = resolve(
        "model_list: []",
        &[
            ("DATABASE_HOST", "db.internal"),
            ("DATABASE_PORT", "6543"),
            ("DATABASE_USERNAME", "app"),
            ("DATABASE_PASSWORD", "p@ss/word"),
            ("DATABASE_NAME", "proxy"),
            ("DATABASE_SCHEMA", "tenant"),
            ("DATABASE_SSLROOTCERT", "/etc/ssl/rds.pem"),
        ],
    )
    .await;

    assert_eq!(
        database.writer,
        Endpoint {
            port: 6543,
            password: Some(SecretValue::new("p@ss/word")),
            ssl_mode: SslMode::VerifyFull,
            ssl_root_cert: Some(PathBuf::from("/etc/ssl/rds.pem")),
            options: vec![("search_path".to_owned(), "\"tenant\"".to_owned())],
            ..endpoint()
        }
    );
}

#[tokio::test]
async fn what_a_replica_url_pins_wins_over_the_configured_pool() {
    let yaml = "general_settings:\n  database_connection_pool_limit: 20";

    let database = resolve(
        yaml,
        &[
            ("DATABASE_URL", WRITER),
            (
                "DATABASE_URL_READ_REPLICA",
                "postgresql://app:secret@replica.internal/proxy?connection_limit=3&pool_timeout=0",
            ),
        ],
    )
    .await;

    assert_eq!(database.writer.pool.max_connections, 20);
    assert_eq!(
        database.reader,
        Some(Endpoint {
            host: "replica.internal".to_owned(),
            pool: PoolLimits {
                max_connections: 3,
                acquire_timeout_seconds: None,
                ..endpoint().pool
            },
            ..endpoint()
        })
    );
}

#[tokio::test]
async fn a_replica_host_takes_the_writer_fields_it_does_not_set() {
    let database = resolve(
        "model_list: []",
        &[
            ("DATABASE_HOST", "db.internal"),
            ("DATABASE_USER", "app"),
            ("DATABASE_PASSWORD", "secret"),
            ("DATABASE_NAME", "proxy"),
            ("DATABASE_HOST_READ_REPLICA", "replica.internal"),
            ("DATABASE_PASSWORD_READ_REPLICA", "other"),
        ],
    )
    .await;

    assert_eq!(database.writer, endpoint());
    assert_eq!(
        database.reader,
        Some(Endpoint {
            host: "replica.internal".to_owned(),
            password: Some(SecretValue::new("other")),
            ..endpoint()
        })
    );
}

#[rstest]
#[case::disabled("?sslmode=disable", SslMode::Disable, None)]
#[case::required("?sslmode=require", SslMode::Require, None)]
#[case::libpq_verify_ca("?sslmode=verify-ca", SslMode::VerifyFull, None)]
#[case::libpq_root_cert(
    "?sslmode=verify-full&sslrootcert=/ca.pem",
    SslMode::VerifyFull,
    Some("/ca.pem")
)]
#[case::prisma_strict(
    "?sslmode=require&sslaccept=strict&sslcert=/ca.pem",
    SslMode::VerifyFull,
    Some("/ca.pem")
)]
#[case::prisma_cert_without_strict("?sslmode=require&sslcert=/ca.pem", SslMode::Require, None)]
#[tokio::test]
async fn tls_settings_are_read_in_both_dialects(
    #[case] query: &str,
    #[case] ssl_mode: SslMode,
    #[case] root_cert: Option<&str>,
) {
    let database = resolve_url(&format!("{WRITER}{query}")).await;

    assert_eq!(database.writer.ssl_mode, ssl_mode);
    assert_eq!(database.writer.ssl_root_cert, root_cert.map(PathBuf::from));
}

#[rstest]
#[case::server_refuses(b'N', SslMode::Disable)]
#[case::server_offers(b'S', SslMode::VerifyFull)]
#[tokio::test]
async fn strict_tls_under_prefer_asks_the_server_once(
    #[case] answer: u8,
    #[case] ssl_mode: SslMode,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0u8; 8];
        stream.read_exact(&mut request).await.unwrap();
        stream.write_all(&[answer]).await.unwrap();
        request
    });

    let database = resolve_url(&format!(
        "postgresql://app:secret@127.0.0.1:{port}/proxy?sslaccept=strict"
    ))
    .await;

    assert_eq!(database.writer.ssl_mode, ssl_mode);
    assert_eq!(
        server.await.unwrap().as_slice(),
        [8_u32.to_be_bytes(), 80_877_103_u32.to_be_bytes()].concat()
    );
}

#[rstest]
#[case::url_names_pgbouncer("?pgbouncer=true", &[], 0)]
#[case::variable_disables_prepared_statements("", &[("DATABASE_DISABLE_PREPARED_STATEMENTS", "true")], 0)]
#[case::url_sizes_the_cache("?statement_cache_size=25", &[], 25)]
#[case::default("", &[], 100)]
#[tokio::test]
async fn the_statement_cache_follows_the_url_and_the_environment(
    #[case] query: &str,
    #[case] variables: &[(&str, &str)],
    #[case] capacity: usize,
) {
    let url = format!("{WRITER}{query}");
    let variables: Vec<(&str, &str)> = variables
        .iter()
        .copied()
        .chain([("DATABASE_URL", url.as_str())])
        .collect();

    let database = resolve("model_list: []", &variables).await;

    assert_eq!(database.writer.statement_cache_capacity, capacity);
}

#[tokio::test]
async fn the_config_can_disable_prepared_statements() {
    let yaml = "general_settings:\n  database_disable_prepared_statements: true";

    let database = resolve(yaml, &[("DATABASE_URL", WRITER)]).await;

    assert_eq!(database.writer.statement_cache_capacity, 0);
}

#[rstest]
#[case::operator_named("?application_name=billing", "billing")]
#[case::prisma_default("?application_name=litellm-prisma", "litellm-native")]
#[tokio::test]
async fn only_an_operator_application_name_is_kept(#[case] query: &str, #[case] expected: &str) {
    let database = resolve_url(&format!("{WRITER}{query}")).await;

    assert_eq!(database.writer.application_name.as_deref(), Some(expected));
}

#[tokio::test]
async fn settings_the_pool_cannot_carry_are_reported() {
    let database = resolve_url(&format!(
        "{WRITER}?sslidentity=/client.p12&options=-c%20lock_timeout%3D10%20-x"
    ))
    .await;

    assert_eq!(
        database.writer.options,
        vec![("lock_timeout".to_owned(), "10".to_owned())]
    );
    assert_eq!(database.writer.ssl_client, None);
    assert_eq!(
        database
            .ignored
            .iter()
            .map(|ignored| ignored.setting.as_str())
            .collect::<Vec<_>>(),
        ["DATABASE_URL options -x", "DATABASE_URL ?sslidentity"]
    );
}

#[test]
fn nothing_configured_is_no_database() {
    assert!(matches!(read("model_list: []", &[]), Ok(None)));
}

#[rstest]
#[case::iam("IAM_TOKEN_DB_AUTH")]
#[case::entra("AZURE_POSTGRESQL_AUTH")]
fn token_auth_is_refused_by_name(#[case] flag: &'static str) {
    let refused = read(
        "model_list: []",
        &[("DATABASE_URL", WRITER), (flag, "True")],
    );

    assert!(matches!(refused, Err(Error::TokenAuth { variable }) if variable == flag));
}

#[test]
fn an_unsupported_scheme_is_refused_without_echoing_the_url() {
    let refused = read(
        "model_list: []",
        &[("DATABASE_URL", "mysql://root:hunter2@db.internal/proxy")],
    )
    .unwrap_err();

    assert!(matches!(&refused, Error::Scheme { scheme, .. } if scheme == "mysql"));
    assert!(!refused.to_string().contains("hunter2"));
}

#[tokio::test]
async fn a_replica_limit_that_is_not_a_positive_integer_is_refused() {
    let configured = read(
        "model_list: []",
        &[
            ("DATABASE_URL", WRITER),
            (
                "DATABASE_URL_READ_REPLICA",
                "postgresql://app@replica.internal/proxy?connection_limit=0",
            ),
        ],
    )
    .unwrap()
    .unwrap();

    let refused = configured.resolve().await.unwrap_err();

    assert!(matches!(refused, Error::Invalid { .. }));
}
