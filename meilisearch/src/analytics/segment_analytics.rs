use std::any::TypeId;
use std::collections::{BTreeSet, BinaryHeap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use actix_web::http::header::USER_AGENT;
use actix_web::HttpRequest;
use byte_unit::Byte;
use index_scheduler::IndexScheduler;
use meilisearch_auth::{AuthController, AuthFilter};
use meilisearch_types::features::RuntimeTogglableFeatures;
use meilisearch_types::locales::Locale;
use meilisearch_types::InstanceUid;
use once_cell::sync::Lazy;
use regex::Regex;
use segment::message::{Identify, Track, User};
use segment::{AutoBatcher, Batcher, HttpClient};
use serde::Serialize;
use serde_json::{json, Value};
use sysinfo::{Disks, System};
use time::OffsetDateTime;
use tokio::select;
use tokio::sync::mpsc::{self, Receiver, Sender};
use uuid::Uuid;

use super::{config_user_id_path, Aggregate, AggregateMethod, MEILISEARCH_CONFIG_PATH};
use crate::option::{
    default_http_addr, IndexerOpts, LogMode, MaxMemory, MaxThreads, ScheduleSnapshot,
};
use crate::routes::{create_all_stats, Stats};
use crate::search::{
    FederatedSearch, SearchQuery, SearchQueryWithIndex, SearchResult, SimilarQuery, SimilarResult,
    DEFAULT_CROP_LENGTH, DEFAULT_CROP_MARKER, DEFAULT_HIGHLIGHT_POST_TAG,
    DEFAULT_HIGHLIGHT_PRE_TAG, DEFAULT_SEARCH_LIMIT, DEFAULT_SEMANTIC_RATIO,
};
use crate::{aggregate_methods, Opt};

const ANALYTICS_HEADER: &str = "X-Meilisearch-Client";

/// Write the instance-uid in the `data.ms` and in `~/.config/MeiliSearch/path-to-db-instance-uid`. Ignore the errors.
fn write_user_id(db_path: &Path, user_id: &InstanceUid) {
    let _ = fs::write(db_path.join("instance-uid"), user_id.to_string());
    if let Some((meilisearch_config_path, user_id_path)) =
        MEILISEARCH_CONFIG_PATH.as_ref().zip(config_user_id_path(db_path))
    {
        let _ = fs::create_dir_all(meilisearch_config_path);
        let _ = fs::write(user_id_path, user_id.to_string());
    }
}

const SEGMENT_API_KEY: &str = "P3FWhhEsJiEDCuEHpmcN9DHcK4hVfBvb";

pub fn extract_user_agents(request: &HttpRequest) -> HashSet<String> {
    request
        .headers()
        .get(ANALYTICS_HEADER)
        .or_else(|| request.headers().get(USER_AGENT))
        .and_then(|header| header.to_str().ok())
        .unwrap_or("unknown")
        .split(';')
        .map(str::trim)
        .map(ToString::to_string)
        .collect()
}

pub struct Message {
    // Since the type_id is solved statically we cannot retrieve it from the Box.
    // Thus we have to send it in the message directly.
    type_id: TypeId,
    // Same for the aggregate function.
    #[allow(clippy::type_complexity)]
    aggregator_function: fn(Box<dyn Aggregate>, Box<dyn Aggregate>) -> Option<Box<dyn Aggregate>>,
    event: Event,
}

pub struct Event {
    original: Box<dyn Aggregate>,
    timestamp: OffsetDateTime,
    user_agents: HashSet<String>,
    total: usize,
}

/// This function should always be called on the same type. If `this` and `other`
/// aren't the same type the function will do nothing and return `None`.
fn downcast_aggregate<ConcreteType: Aggregate>(
    old: Box<dyn Aggregate>,
    new: Box<dyn Aggregate>,
) -> Option<Box<dyn Aggregate>> {
    if old.is::<ConcreteType>() && new.is::<ConcreteType>() {
        // Both the two following lines cannot fail, but just to be sure we don't crash, we're still avoiding unwrapping
        let this = old.downcast::<ConcreteType>().ok()?;
        let other = new.downcast::<ConcreteType>().ok()?;
        Some(ConcreteType::aggregate(this, other))
    } else {
        None
    }
}

impl Message {
    pub fn new<T: Aggregate>(event: T, request: &HttpRequest) -> Self {
        Self {
            type_id: TypeId::of::<T>(),
            event: Event {
                original: Box::new(event),
                timestamp: OffsetDateTime::now_utc(),
                user_agents: extract_user_agents(request),
                total: 1,
            },
            aggregator_function: downcast_aggregate::<T>,
        }
    }
}

pub struct SegmentAnalytics {
    pub instance_uid: InstanceUid,
    pub user: User,
    pub sender: Sender<Message>,
}

impl SegmentAnalytics {
    #[allow(clippy::new_ret_no_self)]
    pub async fn new(
        opt: &Opt,
        index_scheduler: Arc<IndexScheduler>,
        auth_controller: Arc<AuthController>,
    ) -> Option<Arc<Self>> {
        let instance_uid = super::find_user_id(&opt.db_path);
        let first_time_run = instance_uid.is_none();
        let instance_uid = instance_uid.unwrap_or_else(Uuid::new_v4);
        write_user_id(&opt.db_path, &instance_uid);

        let client = reqwest::Client::builder().connect_timeout(Duration::from_secs(10)).build();

        // if reqwest throws an error we won't be able to send analytics
        if client.is_err() {
            return None;
        }

        let client =
            HttpClient::new(client.unwrap(), "https://telemetry.meilisearch.com".to_string());
        let user = User::UserId { user_id: instance_uid.to_string() };
        let mut batcher = AutoBatcher::new(client, Batcher::new(None), SEGMENT_API_KEY.to_string());

        // If Meilisearch is Launched for the first time:
        // 1. Send an event Launched associated to the user `total_launch`.
        // 2. Batch an event Launched with the real instance-id and send it in one hour.
        if first_time_run {
            let _ = batcher
                .push(Track {
                    user: User::UserId { user_id: "total_launch".to_string() },
                    event: "Launched".to_string(),
                    ..Default::default()
                })
                .await;
            let _ = batcher.flush().await;
            let _ = batcher
                .push(Track {
                    user: user.clone(),
                    event: "Launched".to_string(),
                    ..Default::default()
                })
                .await;
        }

        let (sender, inbox) = mpsc::channel(100); // How many analytics can we bufferize

        let segment = Box::new(Segment {
            inbox,
            user: user.clone(),
            opt: opt.clone(),
            batcher,
            events: HashMap::new(),
        });
        tokio::spawn(segment.run(index_scheduler.clone(), auth_controller.clone()));

        let this = Self { instance_uid, sender, user: user.clone() };

        Some(Arc::new(this))
    }
}

/// This structure represent the `infos` field we send in the analytics.
/// It's quite close to the `Opt` structure except all sensitive informations
/// have been simplified to a boolean.
/// It's send as-is in amplitude thus you should never update a name of the
/// struct without the approval of the PM.
#[derive(Debug, Clone, Serialize)]
struct Infos {
    env: String,
    experimental_contains_filter: bool,
    experimental_vector_store: bool,
    experimental_enable_metrics: bool,
    experimental_edit_documents_by_function: bool,
    experimental_search_queue_size: usize,
    experimental_drop_search_after: usize,
    experimental_nb_searches_per_core: usize,
    experimental_logs_mode: LogMode,
    experimental_replication_parameters: bool,
    experimental_enable_logs_route: bool,
    experimental_reduce_indexing_memory_usage: bool,
    experimental_max_number_of_batched_tasks: usize,
    gpu_enabled: bool,
    db_path: bool,
    import_dump: bool,
    dump_dir: bool,
    ignore_missing_dump: bool,
    ignore_dump_if_db_exists: bool,
    import_snapshot: bool,
    schedule_snapshot: Option<u64>,
    snapshot_dir: bool,
    ignore_missing_snapshot: bool,
    ignore_snapshot_if_db_exists: bool,
    http_addr: bool,
    http_payload_size_limit: Byte,
    task_queue_webhook: bool,
    task_webhook_authorization_header: bool,
    log_level: String,
    max_indexing_memory: MaxMemory,
    max_indexing_threads: MaxThreads,
    with_configuration_file: bool,
    ssl_auth_path: bool,
    ssl_cert_path: bool,
    ssl_key_path: bool,
    ssl_ocsp_path: bool,
    ssl_require_auth: bool,
    ssl_resumption: bool,
    ssl_tickets: bool,
}

impl Infos {
    pub fn new(options: Opt, features: RuntimeTogglableFeatures) -> Self {
        // We wants to decompose this whole struct by hand to be sure we don't forget
        // to add analytics when we add a field in the Opt.
        // Thus we must not insert `..` at the end.
        let Opt {
            db_path,
            experimental_contains_filter,
            experimental_enable_metrics,
            experimental_search_queue_size,
            experimental_drop_search_after,
            experimental_nb_searches_per_core,
            experimental_logs_mode,
            experimental_replication_parameters,
            experimental_enable_logs_route,
            experimental_reduce_indexing_memory_usage,
            experimental_max_number_of_batched_tasks,
            http_addr,
            master_key: _,
            env,
            task_webhook_url,
            task_webhook_authorization_header,
            max_index_size: _,
            max_task_db_size: _,
            http_payload_size_limit,
            ssl_cert_path,
            ssl_key_path,
            ssl_auth_path,
            ssl_ocsp_path,
            ssl_require_auth,
            ssl_resumption,
            ssl_tickets,
            import_snapshot,
            ignore_missing_snapshot,
            ignore_snapshot_if_db_exists,
            snapshot_dir,
            schedule_snapshot,
            import_dump,
            ignore_missing_dump,
            ignore_dump_if_db_exists,
            dump_dir,
            log_level,
            indexer_options,
            config_file_path,
            no_analytics: _,
        } = options;

        let schedule_snapshot = match schedule_snapshot {
            ScheduleSnapshot::Disabled => None,
            ScheduleSnapshot::Enabled(interval) => Some(interval),
        };

        let IndexerOpts { max_indexing_memory, max_indexing_threads, skip_index_budget: _ } =
            indexer_options;

        let RuntimeTogglableFeatures {
            vector_store,
            metrics,
            logs_route,
            edit_documents_by_function,
            contains_filter,
        } = features;

        // We're going to override every sensible information.
        // We consider information sensible if it contains a path, an address, or a key.
        Self {
            env,
            experimental_contains_filter: experimental_contains_filter | contains_filter,
            experimental_vector_store: vector_store,
            experimental_edit_documents_by_function: edit_documents_by_function,
            experimental_enable_metrics: experimental_enable_metrics | metrics,
            experimental_search_queue_size,
            experimental_drop_search_after: experimental_drop_search_after.into(),
            experimental_nb_searches_per_core: experimental_nb_searches_per_core.into(),
            experimental_logs_mode,
            experimental_replication_parameters,
            experimental_enable_logs_route: experimental_enable_logs_route | logs_route,
            experimental_reduce_indexing_memory_usage,
            gpu_enabled: meilisearch_types::milli::vector::is_cuda_enabled(),
            db_path: db_path != PathBuf::from("./data.ms"),
            import_dump: import_dump.is_some(),
            dump_dir: dump_dir != PathBuf::from("dumps/"),
            ignore_missing_dump,
            ignore_dump_if_db_exists,
            import_snapshot: import_snapshot.is_some(),
            schedule_snapshot,
            snapshot_dir: snapshot_dir != PathBuf::from("snapshots/"),
            ignore_missing_snapshot,
            ignore_snapshot_if_db_exists,
            http_addr: http_addr != default_http_addr(),
            http_payload_size_limit,
            experimental_max_number_of_batched_tasks,
            task_queue_webhook: task_webhook_url.is_some(),
            task_webhook_authorization_header: task_webhook_authorization_header.is_some(),
            log_level: log_level.to_string(),
            max_indexing_memory,
            max_indexing_threads,
            with_configuration_file: config_file_path.is_some(),
            ssl_auth_path: ssl_auth_path.is_some(),
            ssl_cert_path: ssl_cert_path.is_some(),
            ssl_key_path: ssl_key_path.is_some(),
            ssl_ocsp_path: ssl_ocsp_path.is_some(),
            ssl_require_auth,
            ssl_resumption,
            ssl_tickets,
        }
    }
}

pub struct Segment {
    inbox: Receiver<Message>,
    user: User,
    opt: Opt,
    batcher: AutoBatcher,
    events: HashMap<TypeId, Event>,
}

impl Segment {
    fn compute_traits(opt: &Opt, stats: Stats, features: RuntimeTogglableFeatures) -> Value {
        static FIRST_START_TIMESTAMP: Lazy<Instant> = Lazy::new(Instant::now);
        static SYSTEM: Lazy<Value> = Lazy::new(|| {
            let disks = Disks::new_with_refreshed_list();
            let mut sys = System::new_all();
            sys.refresh_all();
            let kernel_version = System::kernel_version()
                .and_then(|k| k.split_once('-').map(|(k, _)| k.to_string()));
            json!({
                    "distribution": System::name(),
                    "kernel_version": kernel_version,
                    "cores": sys.cpus().len(),
                    "ram_size": sys.total_memory(),
                    "disk_size": disks.iter().map(|disk| disk.total_space()).max(),
                    "server_provider": std::env::var("MEILI_SERVER_PROVIDER").ok(),
            })
        });
        let number_of_documents =
            stats.indexes.values().map(|index| index.number_of_documents).collect::<Vec<u64>>();

        json!({
            "start_since_days": FIRST_START_TIMESTAMP.elapsed().as_secs() / (60 * 60 * 24), // one day
            "system": *SYSTEM,
            "stats": {
                "database_size": stats.database_size,
                "indexes_number": stats.indexes.len(),
                "documents_number": number_of_documents,
            },
            "infos": Infos::new(opt.clone(), features),
        })
    }

    async fn run(
        mut self,
        index_scheduler: Arc<IndexScheduler>,
        auth_controller: Arc<AuthController>,
    ) {
        const INTERVAL: Duration = Duration::from_secs(60 * 60); // one hour
                                                                 // The first batch must be sent after one hour.
        let mut interval =
            tokio::time::interval_at(tokio::time::Instant::now() + INTERVAL, INTERVAL);

        loop {
            select! {
                _ = interval.tick() => {
                    self.tick(index_scheduler.clone(), auth_controller.clone()).await;
                },
                Some(msg) = self.inbox.recv() => {
                    self.handle_msg(msg);
               }
            }
        }
    }

    fn handle_msg(&mut self, Message { type_id, aggregator_function, event }: Message) {
        let new_event = match self.events.remove(&type_id) {
            Some(old) => {
                // The function should never fail since we retrieved the corresponding TypeId in the map. But in the unfortunate
                // case it could happens we're going to silently ignore the error
                let Some(original) = (aggregator_function)(old.original, event.original) else {
                    return;
                };
                Event {
                    original,
                    // We always want to return the FIRST timestamp ever encountered
                    timestamp: old.timestamp,
                    user_agents: old.user_agents.union(&event.user_agents).cloned().collect(),
                    total: old.total.saturating_add(event.total),
                }
            }
            None => event,
        };
        self.events.insert(type_id, new_event);
    }

    async fn tick(
        &mut self,
        index_scheduler: Arc<IndexScheduler>,
        auth_controller: Arc<AuthController>,
    ) {
        if let Ok(stats) = create_all_stats(
            index_scheduler.clone().into(),
            auth_controller.into(),
            &AuthFilter::default(),
        ) {
            // Replace the version number with the prototype name if any.
            let version = if let Some(prototype) = build_info::DescribeResult::from_build()
                .and_then(|describe| describe.as_prototype())
            {
                prototype
            } else {
                env!("CARGO_PKG_VERSION")
            };

            let _ = self
                .batcher
                .push(Identify {
                    context: Some(json!({
                        "app": {
                            "version": version.to_string(),
                        },
                    })),
                    user: self.user.clone(),
                    traits: Self::compute_traits(
                        &self.opt,
                        stats,
                        index_scheduler.features().runtime_features(),
                    ),
                    ..Default::default()
                })
                .await;
        }

        // We empty the list of events
        let events = std::mem::take(&mut self.events);

        for (_, event) in events {
            let Event { original, timestamp, user_agents, total } = event;
            let name = original.event_name();
            let mut properties = original.into_event();
            if properties["user-agent"].is_null() {
                properties["user-agent"] = json!(user_agents);
            };
            if properties["requests"]["total_received"].is_null() {
                properties["requests"]["total_received"] = total.into();
            };

            let _ = self
                .batcher
                .push(Track {
                    user: self.user.clone(),
                    event: name.to_string(),
                    properties,
                    timestamp: Some(timestamp),
                    ..Default::default()
                })
                .await;
        }

        let _ = self.batcher.flush().await;
    }
}

#[derive(Default)]
pub struct SearchAggregator<Method: AggregateMethod> {
    // requests
    total_received: usize,
    total_succeeded: usize,
    total_degraded: usize,
    total_used_negative_operator: usize,
    time_spent: BinaryHeap<usize>,

    // sort
    sort_with_geo_point: bool,
    // every time a request has a filter, this field must be incremented by the number of terms it contains
    sort_sum_of_criteria_terms: usize,
    // every time a request has a filter, this field must be incremented by one
    sort_total_number_of_criteria: usize,

    // distinct
    distinct: bool,

    // filter
    filter_with_geo_radius: bool,
    filter_with_geo_bounding_box: bool,
    // every time a request has a filter, this field must be incremented by the number of terms it contains
    filter_sum_of_criteria_terms: usize,
    // every time a request has a filter, this field must be incremented by one
    filter_total_number_of_criteria: usize,
    used_syntax: HashMap<String, usize>,

    // attributes_to_search_on
    // every time a search is done using attributes_to_search_on
    attributes_to_search_on_total_number_of_uses: usize,

    // q
    // The maximum number of terms in a q request
    max_terms_number: usize,

    // vector
    // The maximum number of floats in a vector request
    max_vector_size: usize,
    // Whether the semantic ratio passed to a hybrid search equals the default ratio.
    semantic_ratio: bool,
    hybrid: bool,
    retrieve_vectors: bool,

    // every time a search is done, we increment the counter linked to the used settings
    matching_strategy: HashMap<String, usize>,

    // List of the unique Locales passed as parameter
    locales: BTreeSet<Locale>,

    // pagination
    max_limit: usize,
    max_offset: usize,
    finite_pagination: usize,

    // formatting
    max_attributes_to_retrieve: usize,
    max_attributes_to_highlight: usize,
    highlight_pre_tag: bool,
    highlight_post_tag: bool,
    max_attributes_to_crop: usize,
    crop_marker: bool,
    show_matches_position: bool,
    crop_length: bool,

    // facets
    facets_sum_of_terms: usize,
    facets_total_number_of_facets: usize,

    // scoring
    show_ranking_score: bool,
    show_ranking_score_details: bool,
    ranking_score_threshold: bool,

    marker: std::marker::PhantomData<Method>,
}

impl<Method: AggregateMethod> SearchAggregator<Method> {
    #[allow(clippy::field_reassign_with_default)]
    pub fn from_query(query: &SearchQuery) -> Self {
        let SearchQuery {
            q,
            vector,
            offset,
            limit,
            page,
            hits_per_page,
            attributes_to_retrieve: _,
            retrieve_vectors,
            attributes_to_crop: _,
            crop_length,
            attributes_to_highlight: _,
            show_matches_position,
            show_ranking_score,
            show_ranking_score_details,
            filter,
            sort,
            distinct,
            facets: _,
            highlight_pre_tag,
            highlight_post_tag,
            crop_marker,
            matching_strategy,
            attributes_to_search_on,
            hybrid,
            ranking_score_threshold,
            locales,
        } = query;

        let mut ret = Self::default();

        ret.total_received = 1;

        if let Some(ref sort) = sort {
            ret.sort_total_number_of_criteria = 1;
            ret.sort_with_geo_point = sort.iter().any(|s| s.contains("_geoPoint("));
            ret.sort_sum_of_criteria_terms = sort.len();
        }

        ret.distinct = distinct.is_some();

        if let Some(ref filter) = filter {
            static RE: Lazy<Regex> = Lazy::new(|| Regex::new("AND | OR").unwrap());
            ret.filter_total_number_of_criteria = 1;

            let syntax = match filter {
                Value::String(_) => "string".to_string(),
                Value::Array(values) => {
                    if values.iter().map(|v| v.to_string()).any(|s| RE.is_match(&s)) {
                        "mixed".to_string()
                    } else {
                        "array".to_string()
                    }
                }
                _ => "none".to_string(),
            };
            // convert the string to a HashMap
            ret.used_syntax.insert(syntax, 1);

            let stringified_filters = filter.to_string();
            ret.filter_with_geo_radius = stringified_filters.contains("_geoRadius(");
            ret.filter_with_geo_bounding_box = stringified_filters.contains("_geoBoundingBox(");
            ret.filter_sum_of_criteria_terms = RE.split(&stringified_filters).count();
        }

        // attributes_to_search_on
        if attributes_to_search_on.is_some() {
            ret.attributes_to_search_on_total_number_of_uses = 1;
        }

        if let Some(ref q) = q {
            ret.max_terms_number = q.split_whitespace().count();
        }

        if let Some(ref vector) = vector {
            ret.max_vector_size = vector.len();
        }
        ret.retrieve_vectors |= retrieve_vectors;

        if query.is_finite_pagination() {
            let limit = hits_per_page.unwrap_or_else(DEFAULT_SEARCH_LIMIT);
            ret.max_limit = limit;
            ret.max_offset = page.unwrap_or(1).saturating_sub(1) * limit;
            ret.finite_pagination = 1;
        } else {
            ret.max_limit = *limit;
            ret.max_offset = *offset;
            ret.finite_pagination = 0;
        }

        ret.matching_strategy.insert(format!("{:?}", matching_strategy), 1);

        if let Some(locales) = locales {
            ret.locales = locales.iter().copied().collect();
        }

        ret.highlight_pre_tag = *highlight_pre_tag != DEFAULT_HIGHLIGHT_PRE_TAG();
        ret.highlight_post_tag = *highlight_post_tag != DEFAULT_HIGHLIGHT_POST_TAG();
        ret.crop_marker = *crop_marker != DEFAULT_CROP_MARKER();
        ret.crop_length = *crop_length != DEFAULT_CROP_LENGTH();
        ret.show_matches_position = *show_matches_position;

        ret.show_ranking_score = *show_ranking_score;
        ret.show_ranking_score_details = *show_ranking_score_details;
        ret.ranking_score_threshold = ranking_score_threshold.is_some();

        if let Some(hybrid) = hybrid {
            ret.semantic_ratio = hybrid.semantic_ratio != DEFAULT_SEMANTIC_RATIO();
            ret.hybrid = true;
        }

        ret
    }

    pub fn succeed(&mut self, result: &SearchResult) {
        let SearchResult {
            hits: _,
            query: _,
            processing_time_ms,
            hits_info: _,
            semantic_hit_count: _,
            facet_distribution: _,
            facet_stats: _,
            degraded,
            used_negative_operator,
        } = result;

        self.total_succeeded = self.total_succeeded.saturating_add(1);
        if *degraded {
            self.total_degraded = self.total_degraded.saturating_add(1);
        }
        if *used_negative_operator {
            self.total_used_negative_operator = self.total_used_negative_operator.saturating_add(1);
        }
        self.time_spent.push(*processing_time_ms as usize);
    }
}

aggregate_methods!(
    SearchGET => "Documents Searched GET",
    SearchPOST => "Documents Searched POST",
);

impl<Method: AggregateMethod> Aggregate for SearchAggregator<Method> {
    fn event_name(&self) -> &'static str {
        Method::event_name()
    }

    fn aggregate(mut self: Box<Self>, new: Box<Self>) -> Box<Self> {
        let Self {
            total_received,
            total_succeeded,
            mut time_spent,
            sort_with_geo_point,
            sort_sum_of_criteria_terms,
            sort_total_number_of_criteria,
            distinct,
            filter_with_geo_radius,
            filter_with_geo_bounding_box,
            filter_sum_of_criteria_terms,
            filter_total_number_of_criteria,
            used_syntax,
            attributes_to_search_on_total_number_of_uses,
            max_terms_number,
            max_vector_size,
            retrieve_vectors,
            matching_strategy,
            max_limit,
            max_offset,
            finite_pagination,
            max_attributes_to_retrieve,
            max_attributes_to_highlight,
            highlight_pre_tag,
            highlight_post_tag,
            max_attributes_to_crop,
            crop_marker,
            show_matches_position,
            crop_length,
            facets_sum_of_terms,
            facets_total_number_of_facets,
            show_ranking_score,
            show_ranking_score_details,
            semantic_ratio,
            hybrid,
            total_degraded,
            total_used_negative_operator,
            ranking_score_threshold,
            mut locales,
            marker: _,
        } = *new;

        // request
        self.total_received = self.total_received.saturating_add(total_received);
        self.total_succeeded = self.total_succeeded.saturating_add(total_succeeded);
        self.total_degraded = self.total_degraded.saturating_add(total_degraded);
        self.total_used_negative_operator =
            self.total_used_negative_operator.saturating_add(total_used_negative_operator);
        self.time_spent.append(&mut time_spent);

        // sort
        self.sort_with_geo_point |= sort_with_geo_point;
        self.sort_sum_of_criteria_terms =
            self.sort_sum_of_criteria_terms.saturating_add(sort_sum_of_criteria_terms);
        self.sort_total_number_of_criteria =
            self.sort_total_number_of_criteria.saturating_add(sort_total_number_of_criteria);

        // distinct
        self.distinct |= distinct;

        // filter
        self.filter_with_geo_radius |= filter_with_geo_radius;
        self.filter_with_geo_bounding_box |= filter_with_geo_bounding_box;
        self.filter_sum_of_criteria_terms =
            self.filter_sum_of_criteria_terms.saturating_add(filter_sum_of_criteria_terms);
        self.filter_total_number_of_criteria =
            self.filter_total_number_of_criteria.saturating_add(filter_total_number_of_criteria);
        for (key, value) in used_syntax.into_iter() {
            let used_syntax = self.used_syntax.entry(key).or_insert(0);
            *used_syntax = used_syntax.saturating_add(value);
        }

        // attributes_to_search_on
        self.attributes_to_search_on_total_number_of_uses = self
            .attributes_to_search_on_total_number_of_uses
            .saturating_add(attributes_to_search_on_total_number_of_uses);

        // q
        self.max_terms_number = self.max_terms_number.max(max_terms_number);

        // vector
        self.max_vector_size = self.max_vector_size.max(max_vector_size);
        self.retrieve_vectors |= retrieve_vectors;
        self.semantic_ratio |= semantic_ratio;
        self.hybrid |= hybrid;

        // pagination
        self.max_limit = self.max_limit.max(max_limit);
        self.max_offset = self.max_offset.max(max_offset);
        self.finite_pagination += finite_pagination;

        // formatting
        self.max_attributes_to_retrieve =
            self.max_attributes_to_retrieve.max(max_attributes_to_retrieve);
        self.max_attributes_to_highlight =
            self.max_attributes_to_highlight.max(max_attributes_to_highlight);
        self.highlight_pre_tag |= highlight_pre_tag;
        self.highlight_post_tag |= highlight_post_tag;
        self.max_attributes_to_crop = self.max_attributes_to_crop.max(max_attributes_to_crop);
        self.crop_marker |= crop_marker;
        self.show_matches_position |= show_matches_position;
        self.crop_length |= crop_length;

        // facets
        self.facets_sum_of_terms = self.facets_sum_of_terms.saturating_add(facets_sum_of_terms);
        self.facets_total_number_of_facets =
            self.facets_total_number_of_facets.saturating_add(facets_total_number_of_facets);

        // matching strategy
        for (key, value) in matching_strategy.into_iter() {
            let matching_strategy = self.matching_strategy.entry(key).or_insert(0);
            *matching_strategy = matching_strategy.saturating_add(value);
        }

        // scoring
        self.show_ranking_score |= show_ranking_score;
        self.show_ranking_score_details |= show_ranking_score_details;
        self.ranking_score_threshold |= ranking_score_threshold;

        // locales
        self.locales.append(&mut locales);

        self
    }

    fn into_event(self: Box<Self>) -> serde_json::Value {
        let Self {
            total_received,
            total_succeeded,
            time_spent,
            sort_with_geo_point,
            sort_sum_of_criteria_terms,
            sort_total_number_of_criteria,
            distinct,
            filter_with_geo_radius,
            filter_with_geo_bounding_box,
            filter_sum_of_criteria_terms,
            filter_total_number_of_criteria,
            used_syntax,
            attributes_to_search_on_total_number_of_uses,
            max_terms_number,
            max_vector_size,
            retrieve_vectors,
            matching_strategy,
            max_limit,
            max_offset,
            finite_pagination,
            max_attributes_to_retrieve,
            max_attributes_to_highlight,
            highlight_pre_tag,
            highlight_post_tag,
            max_attributes_to_crop,
            crop_marker,
            show_matches_position,
            crop_length,
            facets_sum_of_terms,
            facets_total_number_of_facets,
            show_ranking_score,
            show_ranking_score_details,
            semantic_ratio,
            hybrid,
            total_degraded,
            total_used_negative_operator,
            ranking_score_threshold,
            locales,
            marker: _,
        } = *self;

        // we get all the values in a sorted manner
        let time_spent = time_spent.into_sorted_vec();
        // the index of the 99th percentage of value
        let percentile_99th = time_spent.len() * 99 / 100;
        // We are only interested by the slowest value of the 99th fastest results
        let time_spent = time_spent.get(percentile_99th);

        json!({
            "requests": {
                "99th_response_time": time_spent.map(|t| format!("{:.2}", t)),
                "total_succeeded": total_succeeded,
                "total_failed": total_received.saturating_sub(total_succeeded), // just to be sure we never panics
                "total_received": total_received,
                "total_degraded": total_degraded,
                "total_used_negative_operator": total_used_negative_operator,
            },
            "sort": {
                "with_geoPoint": sort_with_geo_point,
                "avg_criteria_number": format!("{:.2}", sort_sum_of_criteria_terms as f64 / sort_total_number_of_criteria as f64),
            },
            "distinct": distinct,
            "filter": {
               "with_geoRadius": filter_with_geo_radius,
               "with_geoBoundingBox": filter_with_geo_bounding_box,
               "avg_criteria_number": format!("{:.2}", filter_sum_of_criteria_terms as f64 / filter_total_number_of_criteria as f64),
               "most_used_syntax": used_syntax.iter().max_by_key(|(_, v)| *v).map(|(k, _)| json!(k)).unwrap_or_else(|| json!(null)),
            },
            "attributes_to_search_on": {
               "total_number_of_uses": attributes_to_search_on_total_number_of_uses,
            },
            "q": {
               "max_terms_number": max_terms_number,
            },
            "vector": {
                "max_vector_size": max_vector_size,
                "retrieve_vectors": retrieve_vectors,
            },
            "hybrid": {
                "enabled": hybrid,
                "semantic_ratio": semantic_ratio,
            },
            "pagination": {
               "max_limit": max_limit,
               "max_offset": max_offset,
               "most_used_navigation": if finite_pagination > (total_received / 2) { "exhaustive" } else { "estimated" },
            },
            "formatting": {
                "max_attributes_to_retrieve": max_attributes_to_retrieve,
                "max_attributes_to_highlight": max_attributes_to_highlight,
                "highlight_pre_tag": highlight_pre_tag,
                "highlight_post_tag": highlight_post_tag,
                "max_attributes_to_crop": max_attributes_to_crop,
                "crop_marker": crop_marker,
                "show_matches_position": show_matches_position,
                "crop_length": crop_length,
            },
            "facets": {
                "avg_facets_number": format!("{:.2}", facets_sum_of_terms as f64 / facets_total_number_of_facets as f64),
            },
            "matching_strategy": {
                "most_used_strategy": matching_strategy.iter().max_by_key(|(_, v)| *v).map(|(k, _)| json!(k)).unwrap_or_else(|| json!(null)),
            },
            "locales": locales,
            "scoring": {
                "show_ranking_score": show_ranking_score,
                "show_ranking_score_details": show_ranking_score_details,
                "ranking_score_threshold": ranking_score_threshold,
            },
        })
    }
}

#[derive(Default)]
pub struct MultiSearchAggregator {
    // requests
    total_received: usize,
    total_succeeded: usize,

    // sum of the number of distinct indexes in each single request, use with total_received to compute an avg
    total_distinct_index_count: usize,
    // number of queries with a single index, use with total_received to compute a proportion
    total_single_index: usize,

    // sum of the number of search queries in the requests, use with total_received to compute an average
    total_search_count: usize,

    // scoring
    show_ranking_score: bool,
    show_ranking_score_details: bool,

    // federation
    use_federation: bool,
}

impl MultiSearchAggregator {
    pub fn from_federated_search(federated_search: &FederatedSearch) -> Self {
        let use_federation = federated_search.federation.is_some();

        let distinct_indexes: HashSet<_> = federated_search
            .queries
            .iter()
            .map(|query| {
                let query = &query;
                // make sure we get a compilation error if a field gets added to / removed from SearchQueryWithIndex
                let SearchQueryWithIndex {
                    index_uid,
                    federation_options: _,
                    q: _,
                    vector: _,
                    offset: _,
                    limit: _,
                    page: _,
                    hits_per_page: _,
                    attributes_to_retrieve: _,
                    retrieve_vectors: _,
                    attributes_to_crop: _,
                    crop_length: _,
                    attributes_to_highlight: _,
                    show_ranking_score: _,
                    show_ranking_score_details: _,
                    show_matches_position: _,
                    filter: _,
                    sort: _,
                    distinct: _,
                    facets: _,
                    highlight_pre_tag: _,
                    highlight_post_tag: _,
                    crop_marker: _,
                    matching_strategy: _,
                    attributes_to_search_on: _,
                    hybrid: _,
                    ranking_score_threshold: _,
                    locales: _,
                } = query;

                index_uid.as_str()
            })
            .collect();

        let show_ranking_score =
            federated_search.queries.iter().any(|query| query.show_ranking_score);
        let show_ranking_score_details =
            federated_search.queries.iter().any(|query| query.show_ranking_score_details);

        Self {
            total_received: 1,
            total_succeeded: 0,
            total_distinct_index_count: distinct_indexes.len(),
            total_single_index: if distinct_indexes.len() == 1 { 1 } else { 0 },
            total_search_count: federated_search.queries.len(),
            show_ranking_score,
            show_ranking_score_details,
            use_federation,
        }
    }

    pub fn succeed(&mut self) {
        self.total_succeeded = self.total_succeeded.saturating_add(1);
    }
}

impl Aggregate for MultiSearchAggregator {
    fn event_name(&self) -> &'static str {
        "Documents Searched by Multi-Search POST"
    }

    /// Aggregate one [MultiSearchAggregator] into another.
    fn aggregate(self: Box<Self>, new: Box<Self>) -> Box<Self> {
        // write the aggregate in a way that will cause a compilation error if a field is added.

        // get ownership of self, replacing it by a default value.
        let this = *self;

        let total_received = this.total_received.saturating_add(new.total_received);
        let total_succeeded = this.total_succeeded.saturating_add(new.total_succeeded);
        let total_distinct_index_count =
            this.total_distinct_index_count.saturating_add(new.total_distinct_index_count);
        let total_single_index = this.total_single_index.saturating_add(new.total_single_index);
        let total_search_count = this.total_search_count.saturating_add(new.total_search_count);
        let show_ranking_score = this.show_ranking_score || new.show_ranking_score;
        let show_ranking_score_details =
            this.show_ranking_score_details || new.show_ranking_score_details;
        let use_federation = this.use_federation || new.use_federation;

        Box::new(Self {
            total_received,
            total_succeeded,
            total_distinct_index_count,
            total_single_index,
            total_search_count,
            show_ranking_score,
            show_ranking_score_details,
            use_federation,
        })
    }

    fn into_event(self: Box<Self>) -> serde_json::Value {
        let Self {
            total_received,
            total_succeeded,
            total_distinct_index_count,
            total_single_index,
            total_search_count,
            show_ranking_score,
            show_ranking_score_details,
            use_federation,
        } = *self;

        json!({
            "requests": {
                "total_succeeded": total_succeeded,
                "total_failed": total_received.saturating_sub(total_succeeded), // just to be sure we never panics
                "total_received": total_received,
            },
            "indexes": {
                "total_single_index": total_single_index,
                "total_distinct_index_count": total_distinct_index_count,
                "avg_distinct_index_count": (total_distinct_index_count as f64) / (total_received as f64), // not 0 else returned early
            },
            "searches": {
                "total_search_count": total_search_count,
                "avg_search_count": (total_search_count as f64) / (total_received as f64),
            },
            "scoring": {
                "show_ranking_score": show_ranking_score,
                "show_ranking_score_details": show_ranking_score_details,
            },
            "federation": {
                "use_federation": use_federation,
            }
        })
    }
}

aggregate_methods!(
    SimilarPOST => "Similar POST",
    SimilarGET => "Similar GET",
);

#[derive(Default)]
pub struct SimilarAggregator<Method: AggregateMethod> {
    // requests
    total_received: usize,
    total_succeeded: usize,
    time_spent: BinaryHeap<usize>,

    // filter
    filter_with_geo_radius: bool,
    filter_with_geo_bounding_box: bool,
    // every time a request has a filter, this field must be incremented by the number of terms it contains
    filter_sum_of_criteria_terms: usize,
    // every time a request has a filter, this field must be incremented by one
    filter_total_number_of_criteria: usize,
    used_syntax: HashMap<String, usize>,

    // Whether a non-default embedder was specified
    retrieve_vectors: bool,

    // pagination
    max_limit: usize,
    max_offset: usize,

    // formatting
    max_attributes_to_retrieve: usize,

    // scoring
    show_ranking_score: bool,
    show_ranking_score_details: bool,
    ranking_score_threshold: bool,

    marker: std::marker::PhantomData<Method>,
}

impl<Method: AggregateMethod> SimilarAggregator<Method> {
    #[allow(clippy::field_reassign_with_default)]
    pub fn from_query(query: &SimilarQuery) -> Self {
        let SimilarQuery {
            id: _,
            embedder: _,
            offset,
            limit,
            attributes_to_retrieve: _,
            retrieve_vectors,
            show_ranking_score,
            show_ranking_score_details,
            filter,
            ranking_score_threshold,
        } = query;

        let mut ret = Self::default();

        ret.total_received = 1;

        if let Some(ref filter) = filter {
            static RE: Lazy<Regex> = Lazy::new(|| Regex::new("AND | OR").unwrap());
            ret.filter_total_number_of_criteria = 1;

            let syntax = match filter {
                Value::String(_) => "string".to_string(),
                Value::Array(values) => {
                    if values.iter().map(|v| v.to_string()).any(|s| RE.is_match(&s)) {
                        "mixed".to_string()
                    } else {
                        "array".to_string()
                    }
                }
                _ => "none".to_string(),
            };
            // convert the string to a HashMap
            ret.used_syntax.insert(syntax, 1);

            let stringified_filters = filter.to_string();
            ret.filter_with_geo_radius = stringified_filters.contains("_geoRadius(");
            ret.filter_with_geo_bounding_box = stringified_filters.contains("_geoBoundingBox(");
            ret.filter_sum_of_criteria_terms = RE.split(&stringified_filters).count();
        }

        ret.max_limit = *limit;
        ret.max_offset = *offset;

        ret.show_ranking_score = *show_ranking_score;
        ret.show_ranking_score_details = *show_ranking_score_details;
        ret.ranking_score_threshold = ranking_score_threshold.is_some();

        ret.retrieve_vectors = *retrieve_vectors;

        ret
    }

    pub fn succeed(&mut self, result: &SimilarResult) {
        let SimilarResult { id: _, hits: _, processing_time_ms, hits_info: _ } = result;

        self.total_succeeded = self.total_succeeded.saturating_add(1);

        self.time_spent.push(*processing_time_ms as usize);
    }
}

impl<Method: AggregateMethod> Aggregate for SimilarAggregator<Method> {
    fn event_name(&self) -> &'static str {
        Method::event_name()
    }

    /// Aggregate one [SimilarAggregator] into another.
    fn aggregate(mut self: Box<Self>, new: Box<Self>) -> Box<Self> {
        let Self {
            total_received,
            total_succeeded,
            mut time_spent,
            filter_with_geo_radius,
            filter_with_geo_bounding_box,
            filter_sum_of_criteria_terms,
            filter_total_number_of_criteria,
            used_syntax,
            max_limit,
            max_offset,
            max_attributes_to_retrieve,
            show_ranking_score,
            show_ranking_score_details,
            ranking_score_threshold,
            retrieve_vectors,
            marker: _,
        } = *new;

        // request
        self.total_received = self.total_received.saturating_add(total_received);
        self.total_succeeded = self.total_succeeded.saturating_add(total_succeeded);
        self.time_spent.append(&mut time_spent);

        // filter
        self.filter_with_geo_radius |= filter_with_geo_radius;
        self.filter_with_geo_bounding_box |= filter_with_geo_bounding_box;
        self.filter_sum_of_criteria_terms =
            self.filter_sum_of_criteria_terms.saturating_add(filter_sum_of_criteria_terms);
        self.filter_total_number_of_criteria =
            self.filter_total_number_of_criteria.saturating_add(filter_total_number_of_criteria);
        for (key, value) in used_syntax.into_iter() {
            let used_syntax = self.used_syntax.entry(key).or_insert(0);
            *used_syntax = used_syntax.saturating_add(value);
        }

        self.retrieve_vectors |= retrieve_vectors;

        // pagination
        self.max_limit = self.max_limit.max(max_limit);
        self.max_offset = self.max_offset.max(max_offset);

        // formatting
        self.max_attributes_to_retrieve =
            self.max_attributes_to_retrieve.max(max_attributes_to_retrieve);

        // scoring
        self.show_ranking_score |= show_ranking_score;
        self.show_ranking_score_details |= show_ranking_score_details;
        self.ranking_score_threshold |= ranking_score_threshold;

        self
    }

    fn into_event(self: Box<Self>) -> serde_json::Value {
        let Self {
            total_received,
            total_succeeded,
            time_spent,
            filter_with_geo_radius,
            filter_with_geo_bounding_box,
            filter_sum_of_criteria_terms,
            filter_total_number_of_criteria,
            used_syntax,
            max_limit,
            max_offset,
            max_attributes_to_retrieve,
            show_ranking_score,
            show_ranking_score_details,
            ranking_score_threshold,
            retrieve_vectors,
            marker: _,
        } = *self;

        // we get all the values in a sorted manner
        let time_spent = time_spent.into_sorted_vec();
        // the index of the 99th percentage of value
        let percentile_99th = time_spent.len() * 99 / 100;
        // We are only interested by the slowest value of the 99th fastest results
        let time_spent = time_spent.get(percentile_99th);

        json!({
            "requests": {
                "99th_response_time": time_spent.map(|t| format!("{:.2}", t)),
                "total_succeeded": total_succeeded,
                "total_failed": total_received.saturating_sub(total_succeeded), // just to be sure we never panics
                "total_received": total_received,
            },
            "filter": {
               "with_geoRadius": filter_with_geo_radius,
               "with_geoBoundingBox": filter_with_geo_bounding_box,
               "avg_criteria_number": format!("{:.2}", filter_sum_of_criteria_terms as f64 / filter_total_number_of_criteria as f64),
               "most_used_syntax": used_syntax.iter().max_by_key(|(_, v)| *v).map(|(k, _)| json!(k)).unwrap_or_else(|| json!(null)),
            },
            "vector": {
                "retrieve_vectors": retrieve_vectors,
            },
            "pagination": {
               "max_limit": max_limit,
               "max_offset": max_offset,
            },
            "formatting": {
                "max_attributes_to_retrieve": max_attributes_to_retrieve,
            },
            "scoring": {
                "show_ranking_score": show_ranking_score,
                "show_ranking_score_details": show_ranking_score_details,
                "ranking_score_threshold": ranking_score_threshold,
            }
        })
    }
}
