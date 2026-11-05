//! Reproduce missing validation of a mutating query in a related update filter.
//!
//! Wrapping an insert in a query reaches planning instead of being rejected,
//! which can panic the connection worker and surface as `connection_lost`.
//! The assertions require either a clean no-op or `unsupported_feature`, with
//! all tables left empty. PostgreSQL and MySQL need disposable databases in
//! `REPRO_POSTGRES_URL` and `REPRO_MYSQL_URL`; use a fresh database for each run.
//!
//! ```text
//! cargo test -p tests --features postgresql,mysql \
//!   --test repro_nested_query_guard -- --ignored --nocapture --test-threads=1
//! ```

#![cfg(any(feature = "sqlite", feature = "postgresql", feature = "mysql"))]

use toasty::stmt::IntoExpr;
use toasty_core::stmt;

#[derive(Debug, toasty::Model)]
struct QueryGuardPerson {
    #[key]
    #[auto]
    id: uuid::Uuid,
    name: String,
}

#[derive(Debug, toasty::Model)]
struct QueryGuardDevice {
    #[key]
    id: i64,
    name: String,
}

#[derive(Debug, toasty::Model)]
struct QueryGuardToken {
    #[key]
    id: i64,
    #[index]
    device_id: i64,
    #[belongs_to(key = device_id, references = id)]
    device: toasty::Deferred<QueryGuardDevice>,
}

async fn missing_source_does_not_create_from_mutating_query(url: &str) -> toasty::Result<()> {
    let mut db = toasty::Db::builder()
        .models(toasty::models!(
            QueryGuardToken,
            QueryGuardDevice,
            QueryGuardPerson
        ))
        .connect(url)
        .await?;
    db.push_schema().await?;

    let person: toasty::stmt::Expr<QueryGuardPerson> = QueryGuardPerson::create()
        .name("orphan-from-query")
        .into_expr();
    let expression: stmt::Expr = person.into();
    let stmt::Statement::Insert(mut insert) = *expression.into_stmt().stmt else {
        unreachable!()
    };
    let name: toasty::stmt::Expr<String> = QueryGuardPerson::fields().name().into_expr();
    insert.returning = Some(stmt::Returning::Project(name.into()));
    let mut query = stmt::Query::new(insert);
    query.single = true;
    let name = toasty::stmt::Expr::<String>::from_untyped(stmt::Expr::stmt(query));
    let result = QueryGuardToken::update_by_id(1)
        .update_device(|_| {
            QueryGuardDevice::filter(QueryGuardDevice::fields().name().eq(name))
                .update()
                .name("new")
        })
        .exec(&mut db)
        .await;
    println!("nested mutating query result={result:?}");
    if let Err(error) = result {
        assert!(
            error.is_unsupported_feature(),
            "unexpected error: {error:?}"
        );
    }
    let people = QueryGuardPerson::all().exec(&mut db).await?;
    println!("nested mutating query people={people:?}");
    assert!(QueryGuardToken::all().exec(&mut db).await?.is_empty());
    assert!(QueryGuardDevice::all().exec(&mut db).await?.is_empty());
    assert!(
        people.is_empty(),
        "a missing source must not insert via a mutating query"
    );
    Ok(())
}

#[cfg(feature = "sqlite")]
#[tokio::test]
#[ignore = "reproduces missing nested mutation query validation"]
async fn sqlite_nested_mutating_query() -> toasty::Result<()> {
    missing_source_does_not_create_from_mutating_query("sqlite::memory:").await
}

#[cfg(feature = "postgresql")]
#[tokio::test]
#[ignore = "requires an empty disposable PostgreSQL database"]
async fn postgres_nested_mutating_query() -> toasty::Result<()> {
    let url = std::env::var("REPRO_POSTGRES_URL").expect("REPRO_POSTGRES_URL required");
    missing_source_does_not_create_from_mutating_query(&url).await
}

#[cfg(feature = "mysql")]
#[tokio::test]
#[ignore = "requires an empty disposable MySQL database"]
async fn mysql_nested_mutating_query() -> toasty::Result<()> {
    let url = std::env::var("REPRO_MYSQL_URL").expect("REPRO_MYSQL_URL required");
    missing_source_does_not_create_from_mutating_query(&url).await
}
