use super::Result;
use deadpool_postgres::{Client, Manager, ManagerConfig, Pool, RecyclingMethod};
pub use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::str::FromStr;
use tokio_postgres::NoTls;

use crate::migrations::{Migration, MIGRATIONS};

use crate::structs::{Parameters, RunID, RunRecord, RunTags, State};

pub struct Storage {
    pool: Pool,
}

impl Storage {
    pub async fn new(url: &str, max_connections: Option<usize>) -> Self {
        let tokio_config = tokio_postgres::Config::from_str(url).unwrap();
        let mgr_config = ManagerConfig {
            recycling_method: RecyclingMethod::Fast,
        };
        let manager = Manager::from_config(tokio_config, NoTls, mgr_config);
        let pool = Pool::builder(manager)
            .max_size(max_connections.unwrap_or(16))
            .build()
            .expect("Unable to build DB pool");
        // Attempt to connect before returning
        Storage { pool }
    }

    async fn get_client(&self) -> Client {
        self.pool.get().await.expect("Unable to create client")
    }

    async fn get_last_migration_id(&self, client: &Client) -> Result<i32> {
        let mut last_applied_migration: i32 = -1;
        if let Ok(rows) = client.query("SELECT max(id) from _migrations", &[]).await {
            if !rows.is_empty() && !rows[0].is_empty() {
                last_applied_migration = rows[0].try_get(0).unwrap_or(last_applied_migration);
            }
        } else {
            // Create the table
            client
                    .query("CREATE TABLE _migrations (id INT PRIMARY KEY, name varchar(255), applied TIMESTAMP default NOW())", &[])
                    .await
                    ?;
        }
        Ok(last_applied_migration)
    }

    pub async fn migrate_down(&self) -> Result<()> {
        let client = self.get_client().await;
        let last_applied_migration = self.get_last_migration_id(&client).await?;
        let mut migrations: Vec<Migration> = MIGRATIONS
            .iter()
            .take(usize::try_from(last_applied_migration + 1).unwrap_or(0))
            .cloned()
            .collect();

        migrations.reverse();

        for migration in migrations {
            client.query(migration.down, &[]).await?;
        }

        client.query("DELETE FROM _migrations", &[]).await?;
        Ok(())
    }

    pub async fn reset(&self) -> Result<()> {
        self.migrate_down().await?;
        self.migrate().await?;

        Ok(())
    }

    // migrations
    async fn migrate(&self) -> Result<()> {
        let client = self.get_client().await;
        // Apply outstanding migrations
        let last_applied_migration = self.get_last_migration_id(&client).await?;
        for (i, migration) in MIGRATIONS.iter().enumerate() {
            let id = i32::try_from(i).unwrap();
            if id > last_applied_migration {
                client.query(migration.up, &[]).await?;
                client
                    .query(
                        "INSERT INTO _migrations (id, name) VALUES ($1::INT, $2::TEXT)",
                        &[&id, &migration.name],
                    )
                    .await?;
            }
        }
        Ok(())
    }

    //
    // Auth
    //
    pub async fn auth_user(&self) {}
    pub async fn get_user(&self) {}
    pub async fn get_group(&self) {}

    //
    // Runs
    //

    // Query
    pub async fn create_run(&self, tags: &RunTags, parameters: &Parameters) -> Result<RunID> {
        let client = self.get_client().await;
        let rows = client
            .query(
                "INSERT INTO runs (tags, parameters) VALUES ($1::HSTORE, $2::HSTORE) RETURNING id",
                &[&tags, &parameters],
            )
            .await?;
        let rid: i64 = rows[0].try_get(0)?;

        client
            .query(
                "INSERT INTO state_changes (run_id, state) VALUES ($1::BIGINT, $2::STATE) RETURNING id",
                &[&rid, &State::Queued],
            )
            .await?;

        Ok(RunID::try_from(rid)?)
    }

    /*
    pub async fn add_tasks(&self, run_id: RunID, tasks: &TaskSet) -> Result<()> {
        let client = self.get_client();
        for (task_id, task) in tasks {
            client
                .query("INSERT INTO tasks (run_id, task_id, task_type, is_generator, max_retries) VALUES ($1::INT, $2::TEXT, $3::TEXT, $4::BOOLEAN, $5::INT)", &[&run_id, &task_id, &val])
                .await?;
        }
    }
    */
    pub async fn update_task(&self) {}
    pub async fn update_run_state(&self) {}
    pub async fn update_task_state(&self) {}
    pub async fn add_task_attempt(&self) {}

    // Update
    pub async fn get_runs(&self) {}
    pub async fn get_run(&self, run_id: RunID) -> Result<Option<RunRecord>> {
        let client = self.get_client().await;
        let run_id = i64::try_from(run_id).unwrap();
        let rows = client
            .query("SELECT * FROM runs WHERE id = $1::BIGINT", &[&run_id])
            .await?;

        if rows.is_empty() {
            return Ok(None);
        }

        Ok(Some(RunRecord {
            tags: rows[0].try_get("tags")?,
            parameters: rows[0].try_get("parameters")?,
            tasks: HashMap::new(),
            state_changes: Vec::new(),
        }))
    }
    pub async fn get_run_state(&self) {}
    pub async fn get_run_state_updates(&self) {}
    pub async fn get_task_summary(&self) {}
    pub async fn get_tasks(&self) {}
    pub async fn get_task(&self) {}
    pub async fn get_task_last_attempt(&self) {}
    pub async fn get_task_state(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn get_url() -> String {
        let user = users::get_user_by_uid(users::get_current_uid()).unwrap();
        format!(
            "postgres://{}@localhost:5432/daggyr_test",
            user.name().to_string_lossy()
        )
    }

    #[tokio::test]
    #[serial]
    async fn test_basic_storage() {
        let url = get_url();
        let storage = Storage::new(&url, None).await;
        storage.reset().await.unwrap();
    }

    #[tokio::test]
    #[serial]
    async fn test_storing_run() {
        let url = get_url();
        let storage = Storage::new(&url, None).await;

        // Create a run
        let tags = RunTags(HashMap::<String, String>::from([
            ("abc".to_owned(), "def".to_owned()),
            ("kea".to_owned(), "alsdkm".to_owned()),
        ]));
        let parameters = Parameters(HashMap::<String, Vec<String>>::from([
            (
                "asldkm".to_owned(),
                vec!["alskdfm".to_owned(), "asldkm".to_owned()],
            ),
            (
                "hehldkm".to_owned(),
                vec!["alskdfm".to_owned(), "hehldkm".to_owned()],
            ),
        ]));

        let run_id = storage.create_run(&tags, &parameters).await.unwrap();
        let run = storage.get_run(run_id).await.unwrap().unwrap();
        assert_eq!(run.tags, tags);
        assert_eq!(run.parameters, parameters);
    }
}