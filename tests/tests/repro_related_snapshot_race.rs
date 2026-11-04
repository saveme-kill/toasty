//! Reproduce the unlocked source/FK snapshot race in belongs-to updates.
//!
//! Each backend runs concurrent rebind and delete cases, with and without a
//! test-only `FOR UPDATE` on the snapshot. The unlocked cases assert the known
//! incorrect result: the old device changes even though the token is rebound or
//! deleted. Passing these tests confirms the race; it does not confirm a fix.
//!
//! Use empty, disposable databases with their default isolation levels:
//! PostgreSQL READ COMMITTED and MySQL/InnoDB REPEATABLE READ. These tests create
//! tables and delete their test rows, but leave the tables in place.
//!
//! Set `REPRO_POSTGRES_URL` and `REPRO_MYSQL_URL`, then run:
//!
//! ```text
//! cargo test -p tests --no-default-features --features postgresql,mysql \
//!   --test repro_related_snapshot_race -- --ignored --nocapture --test-threads=1
//! ```
//!
//! To run one backend, add `postgres_snapshot_race` or `mysql_snapshot_race`
//! before `--`; only that backend's URL is required.

#![cfg(all(feature = "postgresql", feature = "mysql"))]

use async_trait::async_trait;
use std::{
    borrow::Cow,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use toasty_core::{
    Result, Schema,
    driver::{Capability, ConnectContext, Connection, Driver, ExecResponse, Operation},
    schema::{
        db::{AppliedMigration, Migration},
        diff,
    },
    stmt,
};
use tokio::sync::Notify;

#[derive(Debug, toasty::Model)]
struct RaceDevice {
    #[key]
    id: i64,
    touches: i64,
}

#[derive(Debug, toasty::Model)]
struct RaceToken {
    #[key]
    id: i64,
    revoked: bool,
    #[index]
    device_id: i64,
    #[belongs_to(key = device_id, references = id)]
    device: toasty::Deferred<RaceDevice>,
}

#[derive(Debug, Default)]
struct Gate {
    armed: AtomicBool,
    lock: AtomicBool,
    captured: Notify,
    resume: Notify,
}

#[derive(Debug)]
struct GatedDriver {
    inner: Box<dyn Driver>,
    gate: Arc<Gate>,
}

#[async_trait]
impl Driver for GatedDriver {
    fn url(&self) -> Cow<'_, str> {
        self.inner.url()
    }

    fn capability(&self) -> &'static Capability {
        self.inner.capability()
    }

    async fn connect(&self, cx: &ConnectContext) -> Result<Box<dyn Connection>> {
        Ok(Box::new(GatedConnection {
            inner: self.inner.connect(cx).await?,
            gate: self.gate.clone(),
        }))
    }

    fn generate_migration(&self, schema: &diff::Schema<'_>) -> Migration {
        self.inner.generate_migration(schema)
    }

    async fn reset_db(&self) -> Result<()> {
        self.inner.reset_db().await
    }
}

#[derive(Debug)]
struct GatedConnection {
    inner: Box<dyn Connection>,
    gate: Arc<Gate>,
}

#[async_trait]
impl Connection for GatedConnection {
    async fn exec(
        &mut self,
        schema: &Arc<Schema>,
        mut operation: Operation,
    ) -> Result<ExecResponse> {
        let capture = matches!(&operation, Operation::QuerySql(op) if op.stmt.is_query())
            && self.gate.armed.swap(false, Ordering::SeqCst);
        if capture {
            let Operation::QuerySql(op) = &mut operation else {
                unreachable!()
            };
            let query = op.stmt.as_query_mut().unwrap();
            assert!(query.locks.is_empty());
            // Inject the lock only for the control cases, without changing
            // the production planner or either database driver's behavior.
            if self.gate.lock.load(Ordering::SeqCst) {
                query.locks.push(stmt::Lock::Update);
            }
            println!("A snapshot locks: {:?}", query.locks);
        }
        let mut response = self.inner.exec(schema, operation).await?;
        if capture {
            // Drain the real query before allowing B to write. Keeping a
            // duplicate preserves the result the engine consumes after A resumes.
            let snapshot = response.values.dup().await?.collect_as_value().await?;
            println!("A snapshot fully read: {snapshot:?}");
            self.gate.captured.notify_one();
            tokio::time::timeout(Duration::from_secs(15), self.gate.resume.notified())
                .await
                .expect("snapshot gate timed out");
        }
        Ok(response)
    }

    async fn push_schema(&mut self, schema: &Schema) -> Result<()> {
        self.inner.push_schema(schema).await
    }

    async fn applied_migrations(&mut self) -> Result<Vec<AppliedMigration>> {
        self.inner.applied_migrations().await
    }

    async fn apply_migration(&mut self, id: u64, name: &str, migration: &Migration) -> Result<()> {
        self.inner.apply_migration(id, name, migration).await
    }

    fn is_valid(&self) -> bool {
        self.inner.is_valid()
    }

    async fn ping(&mut self) -> Result<()> {
        self.inner.ping().await
    }
}

async fn reproduce(url: &str, inner: Box<dyn Driver>, isolation_sql: &str) -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("toasty::query=debug")
        .with_ansi(false)
        .try_init();
    let gate = Arc::new(Gate::default());
    let mut db_a = toasty::Db::builder()
        .models(toasty::models!(RaceDevice, RaceToken))
        .max_pool_size(1)
        .build(GatedDriver {
            inner,
            gate: gate.clone(),
        })
        .await?;
    let mut db_b = toasty::Db::builder()
        .models(toasty::models!(RaceDevice, RaceToken))
        .max_pool_size(1)
        .connect(url)
        .await?;
    db_a.push_schema().await?;
    println!(
        "Database isolation: {:?}",
        toasty::sql::query(isolation_sql).exec(&mut db_a).await?
    );

    for delete in [false, true] {
        for locked_control in [false, true] {
            RaceDevice::create()
                .id(1)
                .touches(0)
                .exec(&mut db_a)
                .await?;
            RaceDevice::create()
                .id(2)
                .touches(0)
                .exec(&mut db_a)
                .await?;
            RaceToken::create()
                .id(1)
                .revoked(false)
                .device_id(1)
                .exec(&mut db_a)
                .await?;
            println!("CASE delete={delete}, test_only_lock={locked_control}");
            gate.lock.store(locked_control, Ordering::SeqCst);
            gate.armed.store(true, Ordering::SeqCst);
            let a = tokio::spawn(async move {
                let result = RaceToken::filter(
                    RaceToken::fields()
                        .id()
                        .eq(1)
                        .and(RaceToken::fields().revoked().eq(false)),
                )
                .update()
                .revoked(true)
                .update_device(|device| device.touches(1))
                .exec(&mut db_a)
                .await;
                (db_a, result)
            });
            tokio::time::timeout(Duration::from_secs(10), gate.captured.notified())
                .await
                .expect("A did not read snapshot");
            let started = Arc::new(Notify::new());
            let started_b = started.clone();
            let mut b = tokio::spawn(async move {
                started_b.notify_one();
                let query = RaceToken::filter(
                    RaceToken::fields()
                        .id()
                        .eq(1)
                        .and(RaceToken::fields().revoked().eq(false)),
                );
                if delete {
                    query.delete().exec(&mut db_b).await?;
                } else {
                    query.update().device_id(2).exec(&mut db_b).await?;
                }
                Ok::<_, toasty::Error>(db_b)
            });
            started.notified().await;
            if locked_control {
                assert!(
                    tokio::time::timeout(Duration::from_millis(500), &mut b)
                        .await
                        .is_err(),
                    "B must wait for A's row lock"
                );
                println!("B is pending while A holds snapshot row lock");
                gate.resume.notify_one();
                db_b = tokio::time::timeout(Duration::from_secs(10), b)
                    .await
                    .unwrap()
                    .unwrap()?;
            } else {
                db_b = tokio::time::timeout(Duration::from_secs(10), b)
                    .await
                    .expect("B must complete while A is paused")
                    .unwrap()?;
                println!("B committed while A remained paused after snapshot");
                gate.resume.notify_one();
            }
            let (returned_a, result) = tokio::time::timeout(Duration::from_secs(10), a)
                .await
                .unwrap()
                .unwrap();
            db_a = returned_a;
            println!("A API result: {result:?}");
            result?;
            let d1 = RaceDevice::get_by_id(&mut db_a, 1).await?;
            let d2 = RaceDevice::get_by_id(&mut db_a, 2).await?;
            let tokens = RaceToken::all().exec(&mut db_a).await?;
            println!(
                "FINAL tokens={tokens:?}, D1.touches={}, D2.touches={}",
                d1.touches, d2.touches
            );
            assert_eq!(d1.touches, 1);
            assert_eq!(d2.touches, 0);
            if locked_control {
                assert_eq!(tokens.len(), 1);
                assert!(tokens[0].revoked);
                assert_eq!(tokens[0].device_id, 1);
            } else if delete {
                assert!(tokens.is_empty());
            } else {
                assert_eq!(tokens.len(), 1);
                assert!(tokens[0].revoked);
                assert_eq!(tokens[0].device_id, 2);
            }
            RaceToken::all().delete().exec(&mut db_a).await?;
            RaceDevice::all().delete().exec(&mut db_a).await?;
        }
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires an empty disposable PostgreSQL database; asserts a known race"]
async fn postgres_snapshot_race() -> Result<()> {
    let url = std::env::var("REPRO_POSTGRES_URL").expect("REPRO_POSTGRES_URL required");
    let driver = toasty_driver_postgresql::PostgreSQL::new(url.clone())?;
    reproduce(&url, Box::new(driver), "SHOW transaction_isolation").await
}

#[tokio::test]
#[ignore = "requires an empty disposable MySQL database; asserts a known race"]
async fn mysql_snapshot_race() -> Result<()> {
    let url = std::env::var("REPRO_MYSQL_URL").expect("REPRO_MYSQL_URL required");
    let driver = toasty_driver_mysql::MySQL::new(url.as_str())?;
    reproduce(&url, Box::new(driver), "SELECT @@transaction_isolation").await
}
