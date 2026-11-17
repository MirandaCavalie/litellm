use std::time::Duration;

use chrono::Utc;
use litellm_db::{Error, Pools, insert_spend_log_rows, within, write_spend_log_ledger};

use crate::{LedgerStore, SpendLogRow, ledger_request};

#[derive(Clone)]
pub struct PgLedgerStore {
    pools: Pools,
    named: bool,
    socket_timeout: Option<Duration>,
    transaction_timeout: Duration,
    max_depth: usize,
}

impl PgLedgerStore {
    pub fn new(
        pools: Pools,
        named: bool,
        socket_timeout: Option<Duration>,
        transaction_timeout: Duration,
        max_depth: usize,
    ) -> Self {
        Self {
            pools,
            named,
            socket_timeout,
            transaction_timeout,
            max_depth,
        }
    }

    async fn insert(&self, rows: &[SpendLogRow]) -> Result<usize, Error> {
        let request = ledger_request(rows, self.max_depth)?;
        let pool = self.pools.writer();
        let legacy = if request.legacy.is_empty() {
            0
        } else {
            within(
                self.socket_timeout,
                insert_spend_log_rows(pool, &request.legacy, Utc::now().naive_utc(), self.named),
            )
            .await?
            .len()
        };
        if request.ledger.is_empty() {
            return Ok(legacy);
        }
        let accepted = within(
            self.socket_timeout,
            write_spend_log_ledger(
                pool,
                &request.ledger,
                true,
                self.transaction_timeout,
                self.named,
            ),
        )
        .await?;
        Ok(legacy + accepted)
    }
}

impl LedgerStore for PgLedgerStore {
    async fn insert_with_deliveries(&self, rows: &[SpendLogRow]) -> Result<usize, Error> {
        let result = self
            .insert(rows)
            .await
            .map_err(|error| error.explaining_pool_timeout(self.pools.writer()));
        if let Err(error) = &result {
            self.pools.recycle_after(error);
        }
        result
    }
}
