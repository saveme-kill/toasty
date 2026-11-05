//! Reproduce snapshot-key changes and nested relation writes in related updates.
//!
//! These opt-in tests assert correct behavior and fail while the bugs are present.
//! A rejected operation must return `unsupported_feature` and leave all rows
//! unchanged. A successful operation must apply every requested update without
//! leaving dangling foreign keys or writing rows when no source matches.
//!
//! SQLite uses a fresh in-memory database for each case:
//!
//! ```text
//! cargo test -p tests --test repro_related_update_edges -- --ignored --nocapture
//! ```
//!
//! For PostgreSQL and MySQL, set `REPRO_POSTGRES_URL` and `REPRO_MYSQL_URL` to
//! empty disposable databases, then run:
//!
//! ```text
//! cargo test -p tests --no-default-features --features postgresql,mysql \
//!   --test repro_related_update_edges -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Each backend can also run alone with its own feature and URL. The cases use
//! distinct tables, but leave their tables and rows in place. Use fresh databases
//! for every run. Unexpected errors, including a connection worker panic, fail
//! the tests rather than counting as a safe rejection.

#![cfg(any(feature = "sqlite", feature = "postgresql", feature = "mysql"))]

#[derive(Debug, toasty::Model)]
struct EdgeParent {
    #[key]
    id: i64,
    #[unique]
    code: String,
    touches: i64,
}

#[derive(Debug, toasty::Model)]
struct EdgeChild {
    #[key]
    id: i64,
    changed: bool,
    #[index]
    first_id: i64,
    #[belongs_to(key = first_id, references = id)]
    first: toasty::Deferred<EdgeParent>,
    #[index]
    second_code: String,
    #[belongs_to(key = second_code, references = code)]
    second: toasty::Deferred<EdgeParent>,
}

async fn cross_relation_keys(url: &str) -> toasty::Result<()> {
    let mut db = toasty::Db::builder()
        .models(toasty::models!(EdgeParent, EdgeChild))
        .connect(url)
        .await?;
    db.push_schema().await?;
    EdgeParent::create()
        .id(1)
        .code("old")
        .touches(0)
        .exec(&mut db)
        .await?;
    EdgeChild::create()
        .id(1)
        .changed(false)
        .first_id(1)
        .second_code("old")
        .exec(&mut db)
        .await?;

    let result = EdgeChild::update_by_id(1)
        .changed(true)
        .update_first(|parent| parent.code("new"))
        .update_second(|parent| parent.touches(1))
        .exec(&mut db)
        .await;
    println!("cross relation keys: result={result:?}");
    let rejected = unsupported_or_success(result)?;
    let parent = EdgeParent::get_by_id(&mut db, 1).await?;
    let child = EdgeChild::get_by_id(&mut db, 1).await?;
    println!("cross relation keys: parent={parent:?}, child={child:?}");
    assert_eq!(child.first_id, parent.id);
    if rejected {
        assert_eq!(parent.code, "old");
        assert_eq!(parent.touches, 0);
        assert!(!child.changed);
        assert_eq!(child.second_code, "old");
    } else {
        assert_eq!(parent.code, "new");
        assert!(child.changed);
        assert_eq!(
            parent.touches, 1,
            "the second related update must not silently disappear"
        );
        assert_eq!(
            child.second_code, parent.code,
            "the relation must remain valid"
        );
    }
    Ok(())
}

#[derive(Debug, toasty::Model)]
struct EdgeSelfRef {
    #[key]
    id: i64,
    #[unique]
    code: String,
    changed: bool,
    #[index]
    parent_code: String,
    #[belongs_to(key = parent_code, references = code)]
    parent: toasty::Deferred<EdgeSelfRef>,
}

async fn self_relation_primary_key(url: &str) -> toasty::Result<()> {
    let mut db = toasty::Db::builder()
        .models(toasty::models!(EdgeSelfRef))
        .connect(url)
        .await?;
    db.push_schema().await?;
    EdgeSelfRef::create()
        .id(1)
        .code("one")
        .parent_code("one")
        .changed(false)
        .exec(&mut db)
        .await?;

    let result = EdgeSelfRef::update_by_id(1)
        .changed(true)
        .update_parent(|parent| parent.id(2))
        .exec(&mut db)
        .await;
    println!("self relation primary key: result={result:?}");
    let rejected = unsupported_or_success(result)?;
    let rows = EdgeSelfRef::all().exec(&mut db).await?;
    println!("self relation primary key: rows={rows:?}");
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.code, "one");
    assert_eq!(row.parent_code, row.code);
    if rejected {
        assert_eq!(row.id, 1);
        assert!(!row.changed);
    } else {
        assert_eq!(row.id, 2);
        assert!(row.changed, "the source update must not silently disappear");
    }
    Ok(())
}

#[derive(Debug, toasty::Model)]
struct EdgePerson {
    #[key]
    #[auto]
    id: uuid::Uuid,
    name: String,
}

#[derive(Debug, toasty::Embed)]
struct EdgeMembership {
    person_id: uuid::Uuid,
    #[belongs_to(key = person_id)]
    person: toasty::Deferred<EdgePerson>,
}

#[derive(Debug, toasty::Model)]
struct EdgeDevice {
    #[key]
    id: i64,
    membership: EdgeMembership,
}

#[derive(Debug, toasty::Model)]
struct EdgeToken {
    #[key]
    id: i64,
    #[index]
    device_id: i64,
    #[belongs_to(key = device_id, references = id)]
    device: toasty::Deferred<EdgeDevice>,
}

async fn nested_write_without_source(url: &str) -> toasty::Result<()> {
    use toasty::stmt::IntoExpr;

    let mut db = toasty::Db::builder()
        .models(toasty::models!(EdgeToken, EdgeDevice, EdgePerson))
        .connect(url)
        .await?;
    db.push_schema().await?;

    // The public expression constructor can carry a nested relation create,
    // as in the existing embedded-relation integration tests.
    let person: toasty::stmt::Expr<EdgePerson> = EdgePerson::create().name("orphan").into_expr();
    let membership =
        toasty::stmt::Expr::<EdgeMembership>::from_untyped(toasty_core::stmt::Expr::record([
            toasty_core::stmt::Expr::null(),
            person.into(),
        ]));
    let result = EdgeToken::update_by_id(1)
        .update_device(|device| device.membership(membership))
        .exec(&mut db)
        .await;
    println!("nested write without source: result={result:?}");
    unsupported_or_success(result)?;
    let people = EdgePerson::all().exec(&mut db).await?;
    println!("nested write without source: people={people:?}");
    assert!(EdgeToken::all().exec(&mut db).await?.is_empty());
    assert!(EdgeDevice::all().exec(&mut db).await?.is_empty());
    assert!(
        people.is_empty(),
        "a missing source must not insert nested relation rows"
    );
    Ok(())
}

fn unsupported_or_success(result: toasty::Result<()>) -> toasty::Result<bool> {
    match result {
        Ok(()) => Ok(false),
        Err(error) if error.is_unsupported_feature() => Ok(true),
        Err(error) => Err(error),
    }
}

macro_rules! backend_tests {
    ($backend:ident, $url:expr) => {
        mod $backend {
            #[tokio::test]
            #[ignore = "reproduces a known bug; requires a fresh database"]
            async fn cross_relation_keys() -> toasty::Result<()> {
                super::cross_relation_keys(&$url).await
            }

            #[tokio::test]
            #[ignore = "reproduces a known bug; requires a fresh database"]
            async fn self_relation_primary_key() -> toasty::Result<()> {
                super::self_relation_primary_key(&$url).await
            }

            #[tokio::test]
            #[ignore = "reproduces a known bug; requires a fresh database"]
            async fn nested_write_without_source() -> toasty::Result<()> {
                super::nested_write_without_source(&$url).await
            }
        }
    };
}

#[cfg(feature = "sqlite")]
backend_tests!(sqlite, "sqlite::memory:".to_owned());
#[cfg(feature = "postgresql")]
backend_tests!(
    postgresql,
    std::env::var("REPRO_POSTGRES_URL").expect("REPRO_POSTGRES_URL required")
);
#[cfg(feature = "mysql")]
backend_tests!(
    mysql,
    std::env::var("REPRO_MYSQL_URL").expect("REPRO_MYSQL_URL required")
);
