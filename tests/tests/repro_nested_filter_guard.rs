//! Reproduce a nested relation create hidden in a related update's filter.
//!
//! The opt-in assertions require a missing source to leave all tables empty.
//! SQLite uses an in-memory database. PostgreSQL and MySQL require empty,
//! disposable databases through `REPRO_POSTGRES_URL` and `REPRO_MYSQL_URL`.
//! Tables and rows are left in place; use fresh databases for each run.
//!
//! ```text
//! cargo test -p tests --features postgresql,mysql \
//!   --test repro_nested_filter_guard -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Enable only the desired backend features to run fewer databases.

#![cfg(any(feature = "sqlite", feature = "postgresql", feature = "mysql"))]

use toasty::stmt::IntoExpr;

#[derive(Debug, toasty::Model)]
struct FilterPerson {
    #[key]
    #[auto]
    id: uuid::Uuid,
    name: String,
}

#[derive(Debug, toasty::Embed)]
struct FilterMembership {
    person_id: uuid::Uuid,
    #[belongs_to(key = person_id)]
    person: toasty::Deferred<FilterPerson>,
}

#[derive(Debug, toasty::Model)]
struct FilterDevice {
    #[key]
    id: i64,
    name: String,
    membership: FilterMembership,
}

#[derive(Debug, toasty::Model)]
struct FilterToken {
    #[key]
    id: i64,
    #[index]
    device_id: i64,
    #[belongs_to(key = device_id, references = id)]
    device: toasty::Deferred<FilterDevice>,
}

async fn missing_source_does_not_create_relation_from_child_filter(
    url: &str,
) -> toasty::Result<()> {
    let mut db = toasty::Db::builder()
        .models(toasty::models!(FilterToken, FilterDevice, FilterPerson))
        .connect(url)
        .await?;
    db.push_schema().await?;

    let person: toasty::stmt::Expr<FilterPerson> = FilterPerson::create()
        .name("orphan-from-filter")
        .into_expr();
    let membership =
        toasty::stmt::Expr::<FilterMembership>::from_untyped(toasty_core::stmt::Expr::record([
            toasty_core::stmt::Expr::null(),
            person.into(),
        ]));
    let result = FilterToken::update_by_id(1)
        .update_device(|_| {
            FilterDevice::filter(FilterDevice::fields().membership().eq(membership))
                .update()
                .name("new")
        })
        .exec(&mut db)
        .await;
    println!("nested child filter result={result:?}");
    if let Err(error) = result {
        assert!(
            error.is_unsupported_feature(),
            "unexpected error: {error:?}"
        );
    }
    let people = FilterPerson::all().exec(&mut db).await?;
    println!("nested child filter people={people:?}");
    assert!(FilterToken::all().exec(&mut db).await?.is_empty());
    assert!(FilterDevice::all().exec(&mut db).await?.is_empty());
    assert!(
        people.is_empty(),
        "a missing source must not insert via a child filter"
    );
    Ok(())
}

#[cfg(feature = "sqlite")]
#[tokio::test]
#[ignore = "reproduces a known nested-write bug"]
async fn sqlite_nested_filter() -> toasty::Result<()> {
    missing_source_does_not_create_relation_from_child_filter("sqlite::memory:").await
}

#[cfg(feature = "postgresql")]
#[tokio::test]
#[ignore = "requires an empty disposable PostgreSQL database"]
async fn postgres_nested_filter() -> toasty::Result<()> {
    let url = std::env::var("REPRO_POSTGRES_URL").expect("REPRO_POSTGRES_URL required");
    missing_source_does_not_create_relation_from_child_filter(&url).await
}

#[cfg(feature = "mysql")]
#[tokio::test]
#[ignore = "requires an empty disposable MySQL database"]
async fn mysql_nested_filter() -> toasty::Result<()> {
    let url = std::env::var("REPRO_MYSQL_URL").expect("REPRO_MYSQL_URL required");
    missing_source_does_not_create_relation_from_child_filter(&url).await
}
