use crate::prelude::*;
use jiff::Timestamp;

#[derive(Debug, toasty::Model)]
struct Device {
    #[key]
    #[auto]
    id: uuid::Uuid,
    #[unique]
    name: String,
    #[auto]
    updated_at: Timestamp,
    #[default(0)]
    touches: i64,
}

#[derive(Debug, toasty::Model)]
struct Token {
    #[key]
    #[auto]
    id: uuid::Uuid,
    #[unique]
    access_token_id: uuid::Uuid,
    is_revoked: bool,
    #[index]
    device_id: Option<uuid::Uuid>,
    #[belongs_to(key = device_id, references = id)]
    device: toasty::Deferred<Option<Device>>,
}

async fn create_device(db: &mut toasty::Db, name: &str) -> Result<Device> {
    Device::create()
        .name(name)
        .updated_at(Timestamp::UNIX_EPOCH)
        .exec(db)
        .await
}

async fn create_token(db: &mut toasty::Db, device: &Device) -> Result<Token> {
    Token::create()
        .access_token_id(uuid::Uuid::new_v4())
        .is_revoked(false)
        .device(device)
        .exec(db)
        .await
}

#[driver_test(requires(sql))]
pub async fn update_record_and_touch_belongs_to(test: &mut Test) -> Result<()> {
    let mut db = test.setup_db(models!(Token, Device)).await;
    let device = create_device(&mut db, "phone").await?;
    let other = create_device(&mut db, "tablet").await?;
    let token = create_token(&mut db, &device).await?;

    Token::update_by_access_token_id(token.access_token_id)
        .is_revoked(true)
        .update_device(|device| device)
        .exec(&mut db)
        .await?;

    let token = Token::get_by_id(&mut db, token.id).await?;
    assert!(token.is_revoked);
    assert_eq!(token.device_id, Some(device.id));
    assert!(token.device.is_unloaded());
    assert!(Device::get_by_id(&mut db, device.id).await?.updated_at > device.updated_at);
    assert_eq!(
        Device::get_by_id(&mut db, other.id).await?.updated_at,
        other.updated_at
    );
    Ok(())
}

#[driver_test(requires(sql))]
pub async fn update_belongs_to_preserves_source_selection(test: &mut Test) -> Result<()> {
    let mut db = test.setup_db(models!(Token, Device)).await;
    let device = create_device(&mut db, "phone").await?;
    let other = create_device(&mut db, "tablet").await?;
    let first = create_token(&mut db, &device).await?;
    let second = create_token(&mut db, &device).await?;
    let unrelated = create_token(&mut db, &other).await?;

    Token::filter(Token::fields().device().name().eq("phone"))
        .update()
        .is_revoked(true)
        .update_device(|device| device.name("changed").touches(toasty::stmt::increment()))
        .exec(&mut db)
        .await?;

    assert!(Token::get_by_id(&mut db, first.id).await?.is_revoked);
    assert!(Token::get_by_id(&mut db, second.id).await?.is_revoked);
    assert!(!Token::get_by_id(&mut db, unrelated.id).await?.is_revoked);
    let loaded = Device::get_by_id(&mut db, device.id).await?;
    assert_eq!(loaded.name, "changed");
    assert_eq!(loaded.touches, 1);
    assert_eq!(
        Device::get_by_id(&mut db, other.id).await?.updated_at,
        other.updated_at
    );
    Ok(())
}

#[driver_test(requires(sql))]
pub async fn update_belongs_to_failure_leaves_source_unchanged(test: &mut Test) -> Result<()> {
    let mut db = test.setup_db(models!(Token, Device)).await;
    let device = create_device(&mut db, "phone").await?;
    create_device(&mut db, "taken").await?;
    let token = create_token(&mut db, &device).await?;

    assert_err!(
        Token::update_by_id(token.id)
            .is_revoked(true)
            .update_device(|device| device.name("taken"))
            .exec(&mut db)
            .await
    );

    assert!(!Token::get_by_id(&mut db, token.id).await?.is_revoked);
    let loaded = Device::get_by_id(&mut db, device.id).await?;
    assert_eq!(loaded.name, device.name);
    assert_eq!(loaded.updated_at, device.updated_at);
    Ok(())
}

#[driver_test(requires(sql))]
pub async fn update_belongs_to_rejects_referenced_key_changes(test: &mut Test) -> Result<()> {
    let mut db = test.setup_db(models!(Token, Device)).await;
    let device = create_device(&mut db, "phone").await?;
    let token = create_token(&mut db, &device).await?;

    let error = Token::update_by_id(token.id)
        .is_revoked(true)
        .update_device(|device| device.id(uuid::Uuid::new_v4()))
        .exec(&mut db)
        .await
        .unwrap_err();
    assert!(error.is_unsupported_feature());
    assert!(!Token::get_by_id(&mut db, token.id).await?.is_revoked);
    assert_eq!(
        Device::get_by_id(&mut db, device.id).await?.updated_at,
        device.updated_at
    );
    Ok(())
}

#[driver_test(requires(sql))]
pub async fn update_belongs_to_skips_missing_source_and_null_relation(
    test: &mut Test,
) -> Result<()> {
    let mut db = test.setup_db(models!(Token, Device)).await;
    let device = create_device(&mut db, "phone").await?;
    let token = Token::create()
        .access_token_id(uuid::Uuid::new_v4())
        .is_revoked(false)
        .exec(&mut db)
        .await?;

    for id in [uuid::Uuid::new_v4(), token.id] {
        Token::update_by_id(id)
            .is_revoked(true)
            .update_device(|device| device.name("changed"))
            .exec(&mut db)
            .await?;
    }

    assert!(Token::get_by_id(&mut db, token.id).await?.is_revoked);
    let loaded = Device::get_by_id(&mut db, device.id).await?;
    assert_eq!(loaded.name, device.name);
    assert_eq!(loaded.updated_at, device.updated_at);
    Ok(())
}

#[driver_test(requires(sql))]
pub async fn update_belongs_to_rolls_back_on_source_failure(test: &mut Test) -> Result<()> {
    let mut db = test.setup_db(models!(Token, Device)).await;
    let device = create_device(&mut db, "phone").await?;
    let token = create_token(&mut db, &device).await?;
    let other = create_token(&mut db, &device).await?;

    assert_err!(
        Token::update_by_id(token.id)
            .access_token_id(other.access_token_id)
            .update_device(|device| device.name("changed"))
            .exec(&mut db)
            .await
    );

    let loaded = Device::get_by_id(&mut db, device.id).await?;
    assert_eq!(loaded.name, device.name);
    assert_eq!(loaded.updated_at, device.updated_at);
    assert_eq!(
        Token::get_by_id(&mut db, token.id).await?.access_token_id,
        token.access_token_id
    );
    Ok(())
}

#[driver_test(requires(sql))]
pub async fn update_belongs_to_from_instance(test: &mut Test) -> Result<()> {
    let mut db = test.setup_db(models!(Token, Device)).await;
    let device = create_device(&mut db, "phone").await?;
    let mut token = create_token(&mut db, &device).await?;

    token
        .update()
        .is_revoked(true)
        .update_device(|device| device.name("changed"))
        .exec(&mut db)
        .await?;

    assert!(token.is_revoked);
    assert_eq!(token.device_id, Some(device.id));
    assert!(token.device.is_unloaded());
    assert_eq!(token.device().exec(&mut db).await?.unwrap().name, "changed");
    Ok(())
}

#[driver_test(requires(sql))]
pub async fn update_eager_belongs_to_with_composite_keys(test: &mut Test) -> Result<()> {
    #[derive(Debug, toasty::Model)]
    #[key(tenant, id)]
    struct Parent {
        tenant: String,
        id: i64,
        name: String,
    }

    #[derive(Debug, toasty::Model)]
    #[key(tenant, id)]
    #[index(parent_tenant, parent_id)]
    struct Child {
        tenant: String,
        id: i64,
        name: String,
        parent_tenant: String,
        parent_id: i64,
        #[belongs_to(key = [parent_tenant, parent_id], references = [tenant, id])]
        parent: Parent,
    }

    let mut db = test.setup_db(models!(Parent, Child)).await;
    let parent = Parent::create()
        .tenant("a")
        .id(1)
        .name("old")
        .exec(&mut db)
        .await?;
    let other = Parent::create()
        .tenant("b")
        .id(1)
        .name("other")
        .exec(&mut db)
        .await?;
    let mut child = Child::create()
        .tenant("a")
        .id(2)
        .name("old")
        .parent(&parent)
        .exec(&mut db)
        .await?;
    Child::create()
        .tenant("b")
        .id(2)
        .name("other")
        .parent(&other)
        .exec(&mut db)
        .await?;

    child
        .update()
        .name("new")
        .update_parent(|parent| parent.name("new"))
        .exec(&mut db)
        .await?;

    assert_eq!(child.name, "new");
    assert_eq!(child.parent.name, "new");
    child
        .update()
        .update_parent(|parent| parent.name("again"))
        .exec(&mut db)
        .await?;
    assert_eq!(child.parent.name, "again");
    let error = child
        .update()
        .name("invalid")
        .update_parent(|parent| parent)
        .exec(&mut db)
        .await
        .unwrap_err();
    assert!(error.is_invalid_statement());
    assert_eq!(child.name, "new");
    assert_eq!(
        Parent::get_by_tenant_and_id(&mut db, "b", 1).await?.name,
        "other"
    );
    assert_eq!(
        Child::get_by_tenant_and_id(&mut db, "b", 2).await?.name,
        "other"
    );
    Ok(())
}

#[driver_test(requires(sql))]
pub async fn update_belongs_to_without_source_assignments(test: &mut Test) -> Result<()> {
    let mut db = test.setup_db(models!(Token, Device)).await;
    let device = create_device(&mut db, "phone").await?;
    let token = create_token(&mut db, &device).await?;

    Token::update_by_id(token.id)
        .update_device(|device| device.name("changed"))
        .exec(&mut db)
        .await?;

    assert!(!Token::get_by_id(&mut db, token.id).await?.is_revoked);
    assert_eq!(Device::get_by_id(&mut db, device.id).await?.name, "changed");
    Ok(())
}
