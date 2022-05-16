use std::collections::HashSet;
use std::fmt;

use regex::RegexSet;
use sea_orm::entity::prelude::*;
use sea_orm::{sea_query, DbBackend, FromQueryResult, QuerySelect, Set, Statement};
use serde::Serialize;
use url::Url;

use super::indexed_document;
use crate::regex::{regex_for_domain, regex_for_prefix};
use shared::config::{Lens, Limit, UserSettings};

const MAX_RETRIES: u8 = 5;

#[derive(Debug, Clone, PartialEq, EnumIter, DeriveActiveEnum, Serialize)]
#[sea_orm(rs_type = "String", db_type = "String(Some(1))")]
pub enum CrawlStatus {
    #[sea_orm(string_value = "Queued")]
    Queued,
    #[sea_orm(string_value = "Processing")]
    Processing,
    #[sea_orm(string_value = "Completed")]
    Completed,
    #[sea_orm(string_value = "Failed")]
    Failed,
}

impl fmt::Display for CrawlStatus {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            CrawlStatus::Queued => write!(f, "Queued"),
            CrawlStatus::Processing => write!(f, "Processing"),
            CrawlStatus::Completed => write!(f, "Completed"),
            CrawlStatus::Failed => write!(f, "Failed"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, EnumIter, DeriveActiveEnum, Serialize)]
#[sea_orm(rs_type = "String", db_type = "String(Some(1))")]
pub enum CrawlType {
    #[sea_orm(string_value = "API")]
    Api,
    #[sea_orm(string_value = "Bootstrap")]
    Bootstrap,
    #[sea_orm(string_value = "Normal")]
    Normal,
}

impl Default for CrawlType {
    fn default() -> Self {
        CrawlType::Normal
    }
}

impl fmt::Display for CrawlType {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            CrawlType::Api => write!(f, "Api"),
            CrawlType::Bootstrap => write!(f, "Bootstrap"),
            CrawlType::Normal => write!(f, "Normal"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize)]
#[sea_orm(table_name = "crawl_queue")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    /// Domain/host of the URL to be crawled
    pub domain: String,
    /// URL to crawl
    #[sea_orm(unique)]
    pub url: String,
    /// Task status.
    pub status: CrawlStatus,
    /// Number of retries for this task.
    #[sea_orm(default_value = 0)]
    pub num_retries: u8,
    /// Crawl Type
    pub crawl_type: CrawlType,
    /// When this was first added to the crawl queue.
    pub created_at: DateTimeUtc,
    /// When this task was last updated.
    pub updated_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter)]
pub enum Relation {}

impl RelationTrait for Relation {
    fn def(&self) -> RelationDef {
        panic!("No RelationDef")
    }
}

impl ActiveModelBehavior for ActiveModel {
    fn new() -> Self {
        Self {
            crawl_type: Set(CrawlType::Normal),
            status: Set(CrawlStatus::Queued),
            created_at: Set(chrono::Utc::now()),
            updated_at: Set(chrono::Utc::now()),
            ..ActiveModelTrait::default()
        }
    }

    // Triggered before insert / update
    fn before_save(mut self, insert: bool) -> Result<Self, DbErr> {
        if !insert {
            self.updated_at = Set(chrono::Utc::now());
        }

        Ok(self)
    }
}

pub async fn reset_processing(db: &DatabaseConnection) {
    Entity::update_many()
        .col_expr(
            Column::Status,
            sea_query::Expr::value(sea_query::Value::String(Some(Box::new(
                CrawlStatus::Queued.to_string(),
            )))),
        )
        .filter(Column::Status.contains(&CrawlStatus::Processing.to_string()))
        .exec(db)
        .await
        .unwrap();
}

#[derive(FromQueryResult)]
struct CrawlQueueCount {
    count: i64,
}

pub async fn num_queued(
    db: &DatabaseConnection,
    status: CrawlStatus,
) -> anyhow::Result<u64, sea_orm::DbErr> {
    let res = Entity::find()
        .column_as(Column::Id.count(), "count")
        .filter(Column::Status.eq(status.to_string()))
        .into_model::<CrawlQueueCount>()
        .one(db)
        .await?;

    Ok(res.unwrap().count as u64)
}

fn gen_priority_values(items: &[String], is_prefix: bool) -> String {
    if items.is_empty() {
        "(\"\", 0)".to_string()
    } else {
        items
            .iter()
            .map(|item| {
                let item = if is_prefix {
                    // Wildcards not supported in prefixes
                    item.to_owned() + "%"
                } else {
                    // TODO: Should probably sanitize this...
                    item.replace('*', "%")
                };

                format!("(\"{}\", 1)", item)
            })
            .collect::<Vec<String>>()
            .join(",")
    }
}

fn gen_priority_sql(p_domains: &str, p_prefixes: &str, user_settings: UserSettings) -> Statement {
    Statement::from_sql_and_values(
        DbBackend::Sqlite,
        &format!(
            r#"WITH
                p_domain(domain, priority) AS (values {}),
                p_prefix(prefix, priority) AS (values {}), {}"#,
            p_domains,
            p_prefixes,
            include_str!("sql/dequeue.sqlx")
        ),
        vec![
            user_settings.domain_crawl_limit.value().into(),
            user_settings.inflight_domain_limit.value().into(),
        ],
    )
}

/// Get the next url in the crawl queue
pub async fn dequeue(
    db: &DatabaseConnection,
    user_settings: UserSettings,
    // Prioritized domains
    p_domains: &[String],
    // Prioritized prefixes
    p_prefixes: &[String],
) -> anyhow::Result<Option<Model>, sea_orm::DbErr> {
    // Check for inflight limits
    if let Limit::Finite(inflight_crawl_limit) = user_settings.inflight_crawl_limit {
        // How many do we have in progress?
        let num_in_progress = Entity::find()
            .filter(Column::Status.eq(CrawlStatus::Processing.to_string()))
            .count(db)
            .await? as u32;

        if num_in_progress >= inflight_crawl_limit {
            return Ok(None);
        }
    }

    // Prioritize any bootstrapping tasks first.
    let entity = Entity::find()
        .filter(Column::Status.eq(CrawlStatus::Queued.to_string()))
        .filter(Column::CrawlType.eq(CrawlType::Bootstrap.to_string()))
        .one(db)
        .await?;

    if let Some(task) = entity {
        return Ok(Some(task));
    }

    // List of domains to prioritize when dequeuing tasks
    // For example, we'll pull domains that make up with lenses before
    // general crawling.
    let prioritized_domains = gen_priority_values(p_domains, false);
    let prioritized_prefixes = gen_priority_values(p_prefixes, true);

    let entity = Entity::find().from_raw_sql(gen_priority_sql(
        &prioritized_domains,
        &prioritized_prefixes,
        user_settings,
    ));

    return entity.one(db).await;
}

/// Add url to the crawl queue
#[derive(PartialEq)]
pub enum SkipReason {
    Invalid,
    Blocked,
    Duplicate,
}

#[derive(Default)]
pub struct EnqueueSettings {
    pub skip_blocklist: bool,
    pub skip_lenses: bool,
    pub crawl_type: CrawlType,
}

pub async fn enqueue_all(
    db: &DatabaseConnection,
    urls: &[String],
    lenses: &[Lens],
    settings: &UserSettings,
    overrides: &EnqueueSettings,
) -> anyhow::Result<(), sea_orm::DbErr> {
    let mut allow_list: Vec<String> = Vec::new();
    for lens in lenses {
        // Build regex from domain
        for domain in lens.domains.iter() {
            allow_list.push(regex_for_domain(domain));
        }

        // Build regex from url rules
        for prefix in lens.urls.iter() {
            allow_list.push(regex_for_prefix(prefix));
        }
    }

    let allow_list = RegexSet::new(allow_list).unwrap();
    let block_list: HashSet<String> = HashSet::from_iter(settings.block_list.iter().cloned());

    // Ignore invalid URLs
    let urls: Vec<String> = urls
        .iter()
        .filter_map(|url| {
            if let Ok(mut parsed) = Url::parse(url) {
                // Always ignore fragments, otherwise crawling
                // https://wikipedia.org/Rust#Blah would be considered different than
                // https://wikipedia.org/Rust
                parsed.set_fragment(None);

                // Ignore URLs w/ no domain/host strings
                let domain = parsed.host_str()?;
                let normalized = parsed.to_string();

                // Ignore domains on blacklist
                if !overrides.skip_blocklist && block_list.contains(&domain.to_string()) {
                    return None;
                }

                // Check lense rules?
                if !overrides.skip_lenses
                    // Should we crawl external links?
                    && !settings.crawl_external_links
                    // Only allow crawls specified in our lenses
                    && !allow_list.is_match(&normalized)
                {
                    return None;
                }

                Some(parsed.as_str().to_string())
            } else {
                None
            }
        })
        .collect();

    // Ignore urls already in queue
    let is_queued: HashSet<String> = Entity::find()
        .filter(Column::Url.is_in(urls.clone()))
        .all(db)
        .await?
        .iter()
        .map(|f| f.url.to_string())
        .collect();

    // Igore urls already indexed
    let is_indexed: HashSet<String> = indexed_document::Entity::find()
        .filter(indexed_document::Column::Url.is_in(urls.clone()))
        .all(db)
        .await?
        .iter()
        .map(|x| x.url.to_string())
        .collect();

    let to_add: Vec<ActiveModel> = urls
        .into_iter()
        .filter(|url| !is_queued.contains(url) && !is_indexed.contains(url))
        .map(|url| {
            let parsed = Url::parse(&url).unwrap();
            let domain = parsed.host_str().unwrap();

            ActiveModel {
                domain: Set(domain.to_string()),
                crawl_type: Set(overrides.crawl_type.clone()),
                url: Set(url),
                ..Default::default()
            }
        })
        .collect();

    if to_add.is_empty() {
        return Ok(());
    }

    match Entity::insert_many(to_add).exec(db).await {
        Ok(_) => {}
        Err(e) => log::error!("insert_many error: {:?}", e),
    }

    Ok(())
}

pub async fn mark_done(
    db: &DatabaseConnection,
    id: i64,
    status: CrawlStatus,
) -> anyhow::Result<()> {
    let crawl = Entity::find_by_id(id).one(db).await?.unwrap();
    let mut updated: ActiveModel = crawl.clone().into();

    // Bump up number of retries if this failed
    if status == CrawlStatus::Failed && crawl.num_retries <= MAX_RETRIES {
        updated.num_retries = Set(crawl.num_retries + 1);
        // Queue again
        updated.status = Set(CrawlStatus::Queued);
    } else {
        updated.status = Set(status);
    }

    updated.update(db).await?;

    Ok(())
}

#[cfg(test)]
mod test {
    use sea_orm::prelude::*;
    use sea_orm::{ActiveModelTrait, Set};
    use url::Url;

    use crate::models::{crawl_queue, indexed_document};
    use crate::test::setup_test_db;
    use shared::config::{Limit, UserSettings};

    use super::{gen_priority_sql, gen_priority_values, EnqueueSettings};

    #[tokio::test]
    async fn test_insert() {
        let db = setup_test_db().await;

        let url = "oldschool.runescape.wiki/";
        let crawl = crawl_queue::ActiveModel {
            domain: Set("oldschool.runescape.wiki".to_string()),
            url: Set(url.to_owned()),
            ..Default::default()
        };
        crawl.insert(&db).await.expect("Unable to insert");

        let query = crawl_queue::Entity::find()
            .filter(crawl_queue::Column::Url.eq(url.to_string()))
            .one(&db)
            .await
            .expect("Unable to run query");

        assert!(query.is_some());

        let res = query.unwrap();
        assert_eq!(res.url, url);
    }

    #[test]
    fn test_priority_sql() {
        let settings = UserSettings::default();
        let p_domains = gen_priority_values(&["en.wikipedia.org".to_string()], false);
        let p_prefixes =
            gen_priority_values(&["https://roll20.net/compendium/dnd5e".to_string()], true);

        let sql = gen_priority_sql(&p_domains, &p_prefixes, settings);
        assert_eq!(
            sql.to_string(),
            "WITH\n                p_domain(domain, priority) AS (values (\"en.wikipedia.org\", 1)),\n                p_prefix(prefix, priority) AS (values (\"https://roll20.net/compendium/dnd5e%\", 1)), indexed AS (\n    SELECT\n        domain,\n        count(*) as count\n    FROM indexed_document\n    GROUP BY domain\n),\ninflight AS (\n    SELECT\n        domain,\n        count(*) as count\n    FROM crawl_queue\n    WHERE status = \"Processing\"\n    GROUP BY domain\n)\nSELECT\n    cq.*\nFROM crawl_queue cq\nLEFT JOIN p_domain ON cq.domain like p_domain.domain\nLEFT JOIN p_prefix ON cq.url like p_prefix.prefix\nLEFT JOIN indexed ON indexed.domain = cq.domain\nLEFT JOIN inflight ON inflight.domain = cq.domain\nWHERE\n    COALESCE(indexed.count, 0) < 10000 AND\n    COALESCE(inflight.count, 0) < 2 AND\n    status = \"Queued\"\nORDER BY\n    p_prefix.priority DESC,\n    p_domain.priority DESC,\n    cq.updated_at ASC"
        );
    }

    #[tokio::test]
    async fn test_enqueue() {
        let settings = UserSettings::default();
        let db = setup_test_db().await;
        let url = vec!["https://oldschool.runescape.wiki/".into()];

        let overrides = EnqueueSettings {
            skip_lenses: true,
            ..Default::default()
        };
        crawl_queue::enqueue_all(&db, &url, &[], &settings, &overrides)
            .await
            .unwrap();

        let crawl = crawl_queue::Entity::find()
            .filter(crawl_queue::Column::Url.eq(url[0].to_string()))
            .all(&db)
            .await
            .unwrap();

        assert_eq!(crawl.len(), 1);
    }

    #[tokio::test]
    async fn test_dequeue() {
        let settings = UserSettings::default();
        let db = setup_test_db().await;
        let url = vec!["https://oldschool.runescape.wiki/".into()];
        let prioritized = vec![];

        let overrides = EnqueueSettings {
            skip_lenses: true,
            ..Default::default()
        };

        crawl_queue::enqueue_all(&db, &url, &[], &settings, &overrides)
            .await
            .unwrap();

        let queue = crawl_queue::dequeue(&db, settings, &prioritized, &[])
            .await
            .unwrap();

        assert!(queue.is_some());
        assert_eq!(queue.unwrap().url, url[0]);
    }

    #[tokio::test]
    async fn test_dequeue_with_limit() {
        let settings = UserSettings {
            domain_crawl_limit: Limit::Finite(2),
            ..Default::default()
        };
        let db = setup_test_db().await;
        let url: Vec<String> = vec!["https://oldschool.runescape.wiki/".into()];
        let parsed = Url::parse(&url[0]).unwrap();
        let prioritized = vec![];
        let overrides = EnqueueSettings {
            skip_lenses: true,
            ..Default::default()
        };

        crawl_queue::enqueue_all(&db, &url, &[], &settings, &overrides)
            .await
            .unwrap();
        let doc = indexed_document::ActiveModel {
            domain: Set(parsed.host_str().unwrap().to_string()),
            url: Set(url[0].clone()),
            doc_id: Set("docid".to_string()),
            ..Default::default()
        };
        doc.save(&db).await.unwrap();
        let queue = crawl_queue::dequeue(&db, settings, &prioritized, &[])
            .await
            .unwrap();
        assert!(queue.is_some());

        let settings = UserSettings {
            domain_crawl_limit: Limit::Finite(1),
            ..Default::default()
        };
        let queue = crawl_queue::dequeue(&db, settings, &prioritized, &[])
            .await
            .unwrap();
        assert!(queue.is_none());
    }
}
