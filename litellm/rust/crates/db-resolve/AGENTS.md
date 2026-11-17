# Database settings for a Rust host

`ConfiguredDatabase::read` turns `general_settings` and the variables a host hands it, the process environment under the config's `environment_variables`, into the writer and read replica `litellm_db::Endpoint`s, and `resolve` asks the server about TLS where it has to. Only a Rust host calls it. The Python host resolves its own endpoint in `litellm/proxy/db/native_database.py` from the URL Prisma connects with, so a rule changed there changes here too

- The writer comes from `general_settings.database_url`, then `DATABASE_URL`, then `DATABASE_HOST`, `DATABASE_PORT`, `DATABASE_USER` or `DATABASE_USERNAME`, `DATABASE_PASSWORD`, `DATABASE_NAME` and `DATABASE_SCHEMA`. The read replica comes from `DATABASE_URL_READ_REPLICA`, then `DATABASE_HOST_READ_REPLICA` with each `*_READ_REPLICA` field falling back to the writer's variable. No writer means no database, which is `Ok(None)`
- No setting name is spelled twice. `Fields` in `src/location.rs` is read with envy from the variables with `DATABASE_` stripped, once as they are and once with `_READ_REPLICA` stripped too, so a field is its variable. `Params` in `src/params.rs` is the url query, so a field is its parameter. The `general_settings` keys are typed fields of `litellm_config::GeneralSettings`. An error or an ignored setting takes its name from the field through `stringify!`. envy matches names in any case, so only a name in exactly upper case is read, like the proxy
- An `os.environ/` reference in the config is already resolved by `config-load` before this crate sees it, so nothing here strips the prefix
- Precedence follows `proxy_cli.py`. On the writer the configured pool size, pool timeout and socket timeout replace what the URL pins. On the replica what the URL pins wins. `max_idle_connection_lifetime` on a URL wins on both
- TLS is read in both dialects. libpq's `verify-ca`, `verify-full` and `sslrootcert` mean the same as Prisma's `sslmode=require` with `sslaccept=strict` and `sslcert`. `sslmode=prefer` with strict verification asks the server once with a Postgres `SSLRequest` and pins `verify-full` or `disable`, and a server that cannot be reached counts as offering TLS
- An error names the setting it came from and never the URL, because the URL carries the password
- A setting the pool cannot carry is returned in `Database::ignored` for the host to log. Only a malformed value, a non-PostgreSQL scheme or a token auth toggle fails

## Differences from the Python host on purpose

| Setting | Python | Here |
| --- | --- | --- |
| `IAM_TOKEN_DB_AUTH`, `AZURE_POSTGRESQL_AUTH` | Mints a token and refreshes it | `Error::TokenAuth`, since nothing in Rust mints one yet. The gateway logs it and runs without a pool |
| `sslidentity`, `sslpassword` | PKCS#12 handed over as PEM | Ignored with a warning, no client certificate is sent |
| `sslrootcert` holding several roots | Pinned down to the server's root, since Prisma takes one certificate | Passed as it is, since rustls trusts every certificate in the file |
| Configured statement, lock and idle-in-transaction timeouts | Appended to the `options` text unless a regex finds the setting there | Added unless the parsed `options` already set it, so a quoted setting still wins and an unreadable `options` still gets the timeouts |
| `options` that shlex cannot split | Drops every session setting, the schema's `search_path` included | Keeps `search_path` and the configured timeouts, and reports the `options` text as ignored |
| `DATABASE_DISABLE_PREPARED_STATEMENTS` with `pgbouncer=false` on the URL | The URL wins | Statements stay unnamed |
| `database_extra_connection_params`, `DATABASE_MAX_IDLE_CONNECTION_LIFETIME`, `connect_timeout` | Read | Not read |
| A URL with a user and no host, such as `postgresql://app@/db?host=/socket` | Connects through the socket | `Error::Url`, since the `url` crate refuses credentials without a host. Write `localhost` as the host |
| A replica `connection_limit` that is not a positive integer | Left for Prisma to judge | `Error::Invalid` |
