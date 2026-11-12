use cot::auth::PasswordHash;
use cot::common_types::Password;
use cot::db::{Auto, Database, LimitedString, Model};

// ---------------------------------------------------------------------------
// User model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
#[cot::db::model]
pub struct User {
    #[model(primary_key)]
    id: Auto<i64>,
    #[model(unique)]
    username: LimitedString<255>,
    password: Option<String>,
    email: Option<String>,
    display_name: Option<String>,
    avatar_url: Option<String>,
    role: LimitedString<32>,
    is_active: bool,
}

// ---------------------------------------------------------------------------
// User helper methods
// ---------------------------------------------------------------------------

impl User {
    /// Get a user by primary key.
    pub async fn get_by_id(db: &Database, user_id: i64) -> cot::db::Result<Option<Self>> {
        Self::get_by_primary_key(db, Auto::Fixed(user_id)).await
    }

    /// Create a new user and insert it into the database.
    pub async fn create(
        db: &Database,
        username: &str,
        email: Option<&str>,
        display_name: Option<&str>,
        password: &str,
        role: &str,
    ) -> cot::db::Result<Self> {
        let hash = PasswordHash::from_password(&Password::new(password));
        let mut user = Self {
            id: Auto::auto(),
            username: LimitedString::new(username).unwrap(),
            password: Some(hash.into_string()),
            email: email.map(str::to_owned),
            display_name: display_name.map(str::to_owned),
            avatar_url: None,
            role: LimitedString::new(role).unwrap(),
            is_active: true,
        };
        user.insert(db).await?;
        Ok(user)
    }

    /// Update an existing user. If `new_password` is `Some`, the password hash
    /// is replaced; otherwise the existing hash is kept.
    pub async fn update_fields(
        &mut self,
        db: &Database,
        username: &str,
        email: Option<&str>,
        display_name: Option<&str>,
        new_password: Option<&str>,
        role: &str,
    ) -> cot::db::Result<()> {
        self.username = LimitedString::new(username).unwrap();
        self.email = email.map(str::to_owned);
        self.display_name = display_name.map(str::to_owned);
        if let Some(pw) = new_password {
            self.password = Some(PasswordHash::from_password(&Password::new(pw)).into_string());
        }
        self.role = LimitedString::new(role).unwrap();
        self.save(db).await
    }

    /// Look up a user by username.
    pub async fn get_by_username(db: &Database, username: &str) -> cot::db::Result<Option<Self>> {
        let Ok(username) = LimitedString::<255>::new(username) else {
            return Ok(None);
        };
        cot::db::query!(User, $username == username).get(db).await
    }

    /// Count all users in the database.
    pub async fn count_all(db: &Database) -> cot::db::Result<u64> {
        Self::objects().count(db).await
    }

    /// Return a reference to the password hash, if set.
    pub fn password_ref(&self) -> Option<PasswordHash> {
        self.password
            .as_ref()
            .and_then(|hash| PasswordHash::new(hash.clone()).ok())
    }

    /// Parse the stored role code into a `Role`, defaulting to `User`.
    pub fn role(&self) -> crate::auth::Role {
        crate::auth::Role::from_code(&self.role).unwrap_or(crate::auth::Role::User)
    }

    /// Delete this user by primary key.
    pub async fn delete_by_id(db: &Database, user_id: i64) -> cot::db::Result<()> {
        cot::db::query!(User, $id == Auto::Fixed(user_id))
            .delete(db)
            .await?;
        Ok(())
    }

    // Accessor helpers for templates
    pub fn id_val(&self) -> i64 {
        self.id.unwrap()
    }
    pub fn username_str(&self) -> &str {
        &self.username
    }
    pub fn display_name_str(&self) -> String {
        self.display_name
            .as_ref()
            .map(|d| d.to_string())
            .unwrap_or_default()
    }
    pub fn is_active(&self) -> bool {
        self.is_active
    }

    /// Create a user without a password (for OIDC-only accounts).
    pub async fn create_oidc(
        db: &Database,
        username: &str,
        email: Option<&str>,
        display_name: Option<&str>,
        role: &str,
    ) -> cot::db::Result<Self> {
        let mut user = Self {
            id: Auto::auto(),
            username: LimitedString::new(username).unwrap(),
            password: None,
            email: email.map(str::to_owned),
            display_name: display_name.map(str::to_owned),
            avatar_url: None,
            role: LimitedString::new(role).unwrap(),
            is_active: true,
        };
        user.insert(db).await?;
        Ok(user)
    }

    /// Update the user's role and persist the change.
    pub async fn update_role(&mut self, db: &Database, role: &str) -> cot::db::Result<()> {
        self.role = LimitedString::new(role).unwrap();
        self.save(db).await
    }

    /// Find a user by email address.
    pub async fn get_by_email(db: &Database, email: &str) -> cot::db::Result<Option<Self>> {
        cot::db::query!(User, $email == Some(email.to_owned()))
            .get(db)
            .await
    }
}

// ---------------------------------------------------------------------------
// OidcLink model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
#[cot::db::model]
pub struct OidcLink {
    #[model(primary_key)]
    id: Auto<i64>,
    user_id: i64,
    issuer: LimitedString<255>,
    sub: LimitedString<255>,
    email: Option<String>,
    name: Option<String>,
    avatar_url: Option<String>,
}

// ---------------------------------------------------------------------------
// OidcLink helper methods
// ---------------------------------------------------------------------------

impl OidcLink {
    /// Find an OIDC link by issuer + subject.
    pub async fn find_by_issuer_sub(
        db: &Database,
        issuer: &str,
        sub: &str,
    ) -> cot::db::Result<Option<Self>> {
        let Ok(issuer) = LimitedString::<255>::new(issuer) else {
            return Ok(None);
        };
        let Ok(sub) = LimitedString::<255>::new(sub) else {
            return Ok(None);
        };
        cot::db::query!(OidcLink, $issuer == issuer && $sub == sub)
            .get(db)
            .await
    }

    /// Create a new OIDC link for a user.
    pub async fn create_link(
        db: &Database,
        user_id: i64,
        issuer: &str,
        sub: &str,
        email: Option<&str>,
        name: Option<&str>,
    ) -> cot::db::Result<Self> {
        let mut link = Self {
            id: Auto::auto(),
            user_id,
            issuer: LimitedString::new(issuer).unwrap(),
            sub: LimitedString::new(sub).unwrap(),
            email: email.map(str::to_owned),
            name: name.map(str::to_owned),
            avatar_url: None,
        };
        link.insert(db).await?;
        Ok(link)
    }

    /// Update cached claims (email, name) on an existing link.
    pub async fn update_claims(
        &mut self,
        db: &Database,
        email: Option<&str>,
        name: Option<&str>,
    ) -> cot::db::Result<()> {
        self.email = email.map(str::to_owned);
        self.name = name.map(str::to_owned);
        self.save(db).await
    }

    /// Delete this OIDC link by primary key.
    pub async fn delete(self, db: &Database) -> cot::db::Result<()> {
        let link_id = self.id;
        cot::db::query!(OidcLink, $id == link_id).delete(db).await?;
        Ok(())
    }

    /// Accessor for the linked user ID.
    pub fn user_id(&self) -> i64 {
        self.user_id
    }
}

// ---------------------------------------------------------------------------
// Migrations
// ---------------------------------------------------------------------------

pub mod db_migrations {
    use cot::auth::PasswordHash;
    use cot::db::migrations::{self, Field, Operation, SyncDynMigration};
    use cot::db::{DatabaseField, Identifier, LimitedString};

    // -- M0003: create furumusic__user -------------------------------------

    #[derive(Debug, Copy, Clone)]
    pub struct M0003CreateUser;

    impl migrations::Migration for M0003CreateUser {
        const APP_NAME: &'static str = "furumusic";
        const MIGRATION_NAME: &'static str = "m_0003_create_user";
        const DEPENDENCIES: &'static [migrations::MigrationDependency] =
            &[migrations::MigrationDependency::migration(
                "furumusic",
                "m_0002_rename_config_table",
            )];
        const OPERATIONS: &'static [Operation] = &[Operation::create_model()
            .table_name(Identifier::new("furumusic__user"))
            .fields(&[
                Field::new(Identifier::new("id"), <i64 as DatabaseField>::TYPE)
                    .primary_key()
                    .auto(),
                Field::new(
                    Identifier::new("username"),
                    <LimitedString<255> as DatabaseField>::TYPE,
                )
                .unique(),
                Field::new(
                    Identifier::new("password"),
                    <PasswordHash as DatabaseField>::TYPE,
                )
                .set_null(true),
                Field::new(
                    Identifier::new("email"),
                    <LimitedString<255> as DatabaseField>::TYPE,
                )
                .set_null(true),
                Field::new(
                    Identifier::new("display_name"),
                    <LimitedString<255> as DatabaseField>::TYPE,
                )
                .set_null(true),
                Field::new(
                    Identifier::new("avatar_url"),
                    <String as DatabaseField>::TYPE,
                )
                .set_null(true),
                Field::new(
                    Identifier::new("role"),
                    <LimitedString<32> as DatabaseField>::TYPE,
                ),
                Field::new(Identifier::new("is_active"), <bool as DatabaseField>::TYPE),
            ])
            .build()];
    }

    // -- M0004: create furumusic__oidc_link --------------------------------

    #[derive(Debug, Copy, Clone)]
    pub struct M0004CreateOidcLink;

    impl migrations::Migration for M0004CreateOidcLink {
        const APP_NAME: &'static str = "furumusic";
        const MIGRATION_NAME: &'static str = "m_0004_create_oidc_link";
        const DEPENDENCIES: &'static [migrations::MigrationDependency] =
            &[migrations::MigrationDependency::migration(
                "furumusic",
                "m_0003_create_user",
            )];
        const OPERATIONS: &'static [Operation] = &[Operation::create_model()
            .table_name(Identifier::new("furumusic__oidc_link"))
            .fields(&[
                Field::new(Identifier::new("id"), <i64 as DatabaseField>::TYPE)
                    .primary_key()
                    .auto(),
                Field::new(Identifier::new("user_id"), <i64 as DatabaseField>::TYPE),
                Field::new(
                    Identifier::new("issuer"),
                    <LimitedString<255> as DatabaseField>::TYPE,
                ),
                Field::new(
                    Identifier::new("sub"),
                    <LimitedString<255> as DatabaseField>::TYPE,
                ),
                Field::new(
                    Identifier::new("email"),
                    <LimitedString<255> as DatabaseField>::TYPE,
                )
                .set_null(true),
                Field::new(
                    Identifier::new("name"),
                    <LimitedString<255> as DatabaseField>::TYPE,
                )
                .set_null(true),
                Field::new(
                    Identifier::new("avatar_url"),
                    <String as DatabaseField>::TYPE,
                )
                .set_null(true),
            ])
            .build()];
    }

    // -- M0005: indexes on furumusic__oidc_link ----------------------------

    #[cot::db::migrations::migration_op]
    async fn create_oidc_link_indexes(
        ctx: migrations::MigrationContext<'_>,
    ) -> cot::db::Result<()> {
        ctx.db
            .raw(
                "CREATE UNIQUE INDEX idx_oidc_link_issuer_sub \
                     ON furumusic__oidc_link (issuer, sub)",
            )
            .await?;
        ctx.db
            .raw(
                "CREATE INDEX idx_oidc_link_user_id \
                     ON furumusic__oidc_link (user_id)",
            )
            .await?;
        Ok(())
    }

    #[derive(Debug, Copy, Clone)]
    pub struct M0005OidcLinkIndexes;

    impl migrations::Migration for M0005OidcLinkIndexes {
        const APP_NAME: &'static str = "furumusic";
        const MIGRATION_NAME: &'static str = "m_0005_oidc_link_indexes";
        const DEPENDENCIES: &'static [migrations::MigrationDependency] =
            &[migrations::MigrationDependency::migration(
                "furumusic",
                "m_0004_create_oidc_link",
            )];
        const OPERATIONS: &'static [Operation] =
            &[Operation::custom(create_oidc_link_indexes).build()];
    }

    pub const MIGRATIONS: &[&SyncDynMigration] = &[
        &M0003CreateUser,
        &M0004CreateOidcLink,
        &M0005OidcLinkIndexes,
    ];
}
