use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "sessions")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub name: String,
    pub branch: String,
    pub copilot_uuid: String,
    pub source_repo: String,
    pub worktree_path: String,
    pub backend: String,
    pub codespace_name: Option<String>,
    pub remote_workdir: Option<String>,
    pub github_login: Option<String>,
    pub status: String,
    pub last_used_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
