use crate::auth::{BrowserSession, SessionProvider};
use crate::snapshot::{CourseSnapshot, SnapshotManifest, SnapshotStore, cache_key};
use anyhow::{Context, Result, anyhow, bail};
use reqwest::{Client, StatusCode};
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler, handler::server::wrapper::Parameters,
    model::*, schemars::JsonSchema, service::RequestContext, tool, tool_handler, tool_router,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap, future::Future, path::PathBuf, pin::Pin, sync::Arc, time::Duration,
};
use tokio::sync::Mutex;
use url::Url;

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, Default)]
struct ToolArgs {
    course_id: Option<u64>,
    assignment_id: Option<u64>,
    announcement_id: Option<u64>,
    attachment_id: Option<u64>,
    topic_id: Option<u64>,
    module_id: Option<u64>,
    quiz_id: Option<u64>,
    days_ahead: Option<u64>,
    start_date: Option<String>,
    end_date: Option<String>,
    limit: Option<u64>,
    unread_only: Option<bool>,
    query: Option<String>,
    path: Option<String>,
    section: Option<String>,
}

impl ToolArgs {
    fn as_map(&self) -> BTreeMap<String, Value> {
        serde_json::to_value(self)
            .ok()
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default()
            .into_iter()
            .filter(|(_, value)| !value.is_null())
            .collect()
    }
}

#[derive(Clone)]
pub struct BrightspaceServer {
    core: Arc<Core>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct ApiVersions {
    lp: String,
    le: String,
}

struct Core {
    base_url: Url,
    versions: Mutex<Option<ApiVersions>>,
    http: Client,
    auth: Arc<dyn SessionProvider>,
    refresh_lock: Mutex<()>,
    session_generation: std::sync::atomic::AtomicU64,
    snapshots: SnapshotStore,
    sync_lock: Mutex<()>,
}

type ToolFuture<'a> = Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>>;
type CustomHandler = for<'a> fn(&'a Core, &'a BTreeMap<String, Value>) -> ToolFuture<'a>;

#[derive(Clone, Copy)]
enum Operation {
    Json(&'static str),
    Text(&'static str),
    Custom(CustomHandler),
    Local,
}

struct ToolRoute {
    name: &'static str,
    operation: Operation,
    query: &'static [(&'static str, &'static str)],
}

const NO_QUERY: &[(&str, &str)] = &[];

impl BrightspaceServer {
    pub async fn new(base_url: Url, profile_dir: PathBuf, sync_dir: PathBuf) -> Result<Self> {
        let http = Client::builder()
            .timeout(Duration::from_secs(45))
            .user_agent("brightspace-mcp-rs/0.1.0")
            .build()?;
        let snapshots = SnapshotStore::new(sync_dir);
        let versions = snapshots
            .read_manifest()?
            .api_versions
            .and_then(|value| serde_json::from_value::<ApiVersions>(value).ok());
        let core = Arc::new(Core {
            auth: Arc::new(BrowserSession::new(base_url.clone(), profile_dir)),
            base_url,
            versions: Mutex::new(versions),
            http,
            refresh_lock: Mutex::new(()),
            session_generation: std::sync::atomic::AtomicU64::new(0),
            snapshots,
            sync_lock: Mutex::new(()),
        });
        Ok(Self { core })
    }

    pub async fn shutdown(&self) -> Result<()> {
        self.core.auth.shutdown().await
    }
}

// This macro keeps the upstream tool names stable while sharing argument
// validation and API transport behavior.
macro_rules! tools_impl {
    ($(($method:ident, $name:literal, $description:literal, $operation:expr, $query:expr)),+ $(,)?) => {
        #[tool_router]
        impl BrightspaceServer {
            $(
                #[tool(name = $name, description = $description)]
                async fn $method(&self, Parameters(args): Parameters<ToolArgs>) -> String {
                    match self.core.call_tool($name, args.as_map()).await {
                        Ok(value) => value,
                        Err(error) => format!("Brightspace error: {error:#}"),
                    }
                }
            )+
        }

        static TOOL_ROUTES: &[ToolRoute] = &[
            $(ToolRoute { name: $name, operation: $operation, query: $query }),+
        ];
    };
}

tools_impl! {
    (sync_courses, "sync_courses", "Sign in through the visible Microsoft browser and refresh local snapshots of all courses.", Operation::Local, NO_QUERY),
    (list_synced_courses, "list_synced_courses", "List courses and sync status from local files. Does not contact Brightspace.", Operation::Local, NO_QUERY),
    (read_synced_course, "read_synced_course", "Read a whole local course snapshot or one section. Does not contact Brightspace. Required: course_id; optional section.", Operation::Local, NO_QUERY),
    (check_auth, "check_auth", "Show the last local sync status. Run sync_courses to authenticate.", Operation::Local, NO_QUERY),
    (list_my_courses, "list_my_courses", "List courses from local snapshots. Run sync_courses to refresh.", Operation::Local, NO_QUERY),
    (clear_cache, "clear_cache", "Delete local course snapshots. Does not contact Brightspace.", Operation::Local, NO_QUERY),
    (get_diagnostics, "get_diagnostics", "Show server version, Brightspace host, and discovered API versions.", Operation::Custom(diagnostics), NO_QUERY),
    (get_my_grades, "get_my_grades", "Read grade items and the current user's grade values for a course. Required: course_id.", Operation::Custom(grades), NO_QUERY),
    (get_assignments, "get_assignments", "List Dropbox assignments for a course. Required: course_id.", Operation::Json("/d2l/api/le/{le}/{course_id}/dropbox/folders/"), NO_QUERY),
    (get_upcoming_due_dates, "get_upcoming_due_dates", "List assignments and quizzes due within days_ahead (default 14).", Operation::Custom(upcoming), NO_QUERY),
    (get_feedback, "get_feedback", "Read a user's submissions for an assignment. Required: course_id, assignment_id.", Operation::Json("/d2l/api/le/{le}/{course_id}/dropbox/folders/{assignment_id}/submissions/mysubmissions/"), NO_QUERY),
    (get_assignment_rubric, "get_assignment_rubric", "Read rubric information for an assignment. Required: course_id, assignment_id.", Operation::Json("/d2l/api/le/{le}/{course_id}/rubrics?objectType=Dropbox&objectId={assignment_id}"), NO_QUERY),
    (get_my_submissions, "get_my_submissions", "Read the current user's assignment submissions. Required: course_id, assignment_id.", Operation::Json("/d2l/api/le/{le}/{course_id}/dropbox/folders/{assignment_id}/submissions/mysubmissions/"), NO_QUERY),
    (get_roster, "get_roster", "Read course classlist. Required: course_id.", Operation::Json("/d2l/api/le/{le}/{course_id}/classlist/"), NO_QUERY),
    (get_classlist_emails, "get_classlist_emails", "Read course classlist including email fields. Required: course_id.", Operation::Json("/d2l/api/le/{le}/{course_id}/classlist/"), NO_QUERY),
    (get_syllabus, "get_syllabus", "Read the course overview/syllabus. Required: course_id.", Operation::Json("/d2l/api/le/{le}/{course_id}/overview"), NO_QUERY),
    (get_course_content, "get_course_content", "Read the course content table of contents. Required: course_id.", Operation::Json("/d2l/api/le/{le}/{course_id}/content/toc"), NO_QUERY),
    (get_announcements, "get_announcements", "Read course announcements. Required: course_id.", Operation::Json("/d2l/api/le/{le}/{course_id}/news/"), NO_QUERY),
    (get_announcement, "get_announcement", "Read one announcement. Required: course_id, announcement_id; optional attachment_id.", Operation::Custom(announcement), NO_QUERY),
    (get_discussions, "get_discussions", "Read course discussion forums and topics. Required: course_id.", Operation::Custom(discussions), NO_QUERY),
    (get_calendar_events, "get_calendar_events", "Read calendar events. Required: course_id; optional start_date and end_date.", Operation::Json("/d2l/api/le/{le}/{course_id}/calendar/events/"), &[("start_date", "startDateTime"), ("end_date", "endDateTime")]),
    (get_assignment_files, "get_assignment_files", "Read assignment attachments metadata. Required: course_id, assignment_id.", Operation::Json("/d2l/api/le/{le}/{course_id}/dropbox/folders/{assignment_id}/attachments/"), NO_QUERY),
    (get_topic_file, "get_topic_file", "Download/read a course content topic file. Required: course_id, topic_id.", Operation::Text("/d2l/api/le/{le}/{course_id}/content/topics/{topic_id}/file"), NO_QUERY),
    (get_module, "get_module", "Read one module's content structure. Required: course_id, module_id.", Operation::Json("/d2l/api/le/{le}/{course_id}/content/modules/{module_id}/structure/"), NO_QUERY),
    (get_course_file, "get_course_file", "Read a course file discovered and cached during sync. Required: course_id, path.", Operation::Local, NO_QUERY),
    (list_quizzes, "list_quizzes", "List course quizzes. Required: course_id.", Operation::Json("/d2l/api/le/{le}/{course_id}/quizzes/"), NO_QUERY),
    (get_quiz_attempts, "get_quiz_attempts", "Read attempts for a quiz. Required: course_id, quiz_id.", Operation::Json("/d2l/api/le/{le}/{course_id}/quizzes/{quiz_id}/attempts/"), NO_QUERY),
    (list_notifications, "list_notifications", "Read the current user's Brightspace activity feed.", Operation::Json("/d2l/api/lp/{lp}/feed/myFeed/"), &[("limit", "pageSize"), ("unread_only", "unreadOnly")]),
    (search_course, "search_course", "Search course content, announcements, and discussion metadata. Required: course_id, query.", Operation::Custom(search), NO_QUERY),
    (get_my_groups, "get_my_groups", "Read groups and group members for a course. Required: course_id.", Operation::Custom(groups), NO_QUERY),
    (get_audit_log, "get_audit_log", "Read local write audit history. This server has no write operations, so the history is empty.", Operation::Custom(audit_log), NO_QUERY),
}

#[tool_handler]
impl ServerHandler for BrightspaceServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_resources().build())
            .with_server_info(Implementation::new("brightspace-mcp-rs", env!("CARGO_PKG_VERSION")))
            .with_instructions("Course reads use local snapshots. Only sync_courses contacts Brightspace and may open a visible browser for Microsoft SSO and manual Authenticator approval. Keep that browser open for future syncs in this server process.")
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<ListResourcesResult, McpError> {
        Ok(ListResourcesResult {
            resources: Vec::new(),
            next_cursor: None,
            meta: None,
            ..Default::default()
        })
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<ListResourceTemplatesResult, McpError> {
        Ok(ListResourceTemplatesResult {
            resource_templates: vec![
                ResourceTemplate::new(
                    "brightspace://{courseId}/syllabus",
                    "Cached course syllabus",
                ),
                ResourceTemplate::new(
                    "brightspace://{courseId}/content/topics/{topicId}",
                    "Cached course content topic",
                ),
                ResourceTemplate::new(
                    "brightspace://{courseId}/assignments/{assignmentId}/files",
                    "Cached assignment files",
                ),
                ResourceTemplate::new(
                    "brightspace://{courseId}/announcements/{announcementId}",
                    "Cached announcement",
                ),
            ],
            next_cursor: None,
            meta: None,
            ..Default::default()
        })
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<ReadResourceResponse, McpError> {
        let value = self
            .core
            .read_resource(&request.uri)
            .await
            .map_err(|error| {
                McpError::resource_not_found(format!("resource read failed: {error:#}"), None)
            })?;
        Ok(ReadResourceResult::new(vec![ResourceContents::text(value, &request.uri)]).into())
    }
}

impl Core {
    async fn api_versions(&self) -> Result<ApiVersions> {
        let mut versions = self.versions.lock().await;
        if versions.is_none() {
            *versions = Some(discover_versions(&self.http, &self.base_url).await?);
        }
        versions
            .as_ref()
            .cloned()
            .ok_or_else(|| anyhow!("Brightspace API versions unavailable"))
    }

    async fn refresh_api_versions(&self) -> Result<ApiVersions> {
        let versions = discover_versions(&self.http, &self.base_url).await?;
        *self.versions.lock().await = Some(versions.clone());
        Ok(versions)
    }

    async fn get(&self, path: &str) -> Result<reqwest::Response> {
        if !(path.starts_with("/d2l/api/") || path.starts_with("/content/enforced/")) {
            bail!("refusing request outside Brightspace read API paths");
        }
        let url = self.base_url.join(path.trim_start_matches('/'))?;
        let generation = self
            .session_generation
            .load(std::sync::atomic::Ordering::Acquire);
        let cookie = self.auth.cookie(false).await?;
        let mut response = self
            .http
            .get(url.clone())
            .header(reqwest::header::COOKIE, cookie)
            .send()
            .await?;
        if response.status() == StatusCode::UNAUTHORIZED
            || response.status() == StatusCode::FORBIDDEN
        {
            let _guard = self.refresh_lock.lock().await;
            let refreshed_cookie = if self
                .session_generation
                .load(std::sync::atomic::Ordering::Acquire)
                == generation
            {
                self.auth.clear().await;
                let cookie = self.auth.cookie(true).await?;
                self.session_generation
                    .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                cookie
            } else {
                self.auth.cookie(false).await?
            };
            response = self
                .http
                .get(url)
                .header(reqwest::header::COOKIE, refreshed_cookie)
                .send()
                .await?;
        }
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            let safe_body = body.chars().take(800).collect::<String>();
            bail!("Brightspace returned HTTP {status}: {safe_body}");
        }
        Ok(response)
    }

    async fn get_json(&self, path: &str) -> Result<Value> {
        let response = self.get(path).await?;
        response
            .json()
            .await
            .context("Brightspace response was not valid JSON")
    }

    async fn call_tool(&self, name: &str, args: BTreeMap<String, Value>) -> Result<String> {
        match name {
            "sync_courses" => self.sync_courses().await,
            "list_synced_courses" => self.list_synced_courses().await,
            "list_my_courses" => self.read_cached_tool(name, &args).await,
            "read_synced_course" => self.read_synced_course(&args).await,
            "check_auth" => self.sync_status().await,
            "clear_cache" => {
                self.snapshots.clear()?;
                Ok("Local course snapshots deleted.".into())
            }
            "get_diagnostics" => pretty(json!({
                "server": "brightspace-mcp-rs",
                "brightspace_host": self.base_url.host_str(),
                "authentication": "only sync_courses accesses Brightspace",
                "local_sync_directory": "~/.brightspace-mcp-rs/synced-courses"
            })),
            "get_audit_log" => {
                Ok("No write operations are enabled; local write audit is empty.".into())
            }
            "search_course" => self.search_cached(&args).await,
            "get_course_file" => self.read_course_file(&args).await,
            _ => self.read_cached_tool(name, &args).await,
        }
    }

    async fn fetch_tool(&self, name: &str, args: BTreeMap<String, Value>) -> Result<String> {
        let route = TOOL_ROUTES
            .iter()
            .find(|route| route.name == name)
            .ok_or_else(|| anyhow!("unknown Brightspace sync operation: {name}"))?;
        match route.operation {
            Operation::Json(template) => {
                let path = self.render_path(template, &args, route.query).await?;
                self.get_json(&path).await.and_then(pretty)
            }
            Operation::Text(template) => {
                let path = self.render_path(template, &args, route.query).await?;
                let response = self.get(&path).await?;
                Ok(text_or_base64(&response.bytes().await?))
            }
            Operation::Custom(handler) => handler(self, &args).await,
            Operation::Local => {
                bail!("local tool `{name}` cannot be used as a Brightspace sync operation")
            }
        }
    }

    async fn sync_courses(&self) -> Result<String> {
        let _guard = self.sync_lock.lock().await;
        let synced_at = unix_seconds();
        let mut manifest = SnapshotManifest {
            synced_at: Some(synced_at),
            ..Default::default()
        };
        let versions = self.refresh_api_versions().await?;
        manifest.api_versions = Some(serde_json::to_value(&versions)?);
        for name in ["list_notifications"] {
            match self.fetch_tool(name, BTreeMap::new()).await {
                Ok(output) => {
                    manifest
                        .tools
                        .insert(cache_key(name, &BTreeMap::new())?, output_value(output));
                }
                Err(error) => {
                    manifest
                        .errors
                        .insert(name.to_owned(), format!("{error:#}"));
                }
            }
        }

        let enrollments = self
            .get_json(&format!(
                "/d2l/api/lp/{}/enrollments/myenrollments/",
                versions.lp
            ))
            .await?;
        manifest.tools.insert(
            cache_key("list_my_courses", &BTreeMap::new())?,
            enrollments.clone(),
        );
        let course_rows = array_values(&enrollments);
        let mut count = 0usize;
        let mut upcoming_courses = Vec::new();
        for enrollment in course_rows {
            let course = enrollment.get("OrgUnit").unwrap_or(enrollment);
            if course
                .get("Type")
                .and_then(|kind| kind.get("Code"))
                .and_then(Value::as_str)
                .is_some_and(|kind| !kind.eq_ignore_ascii_case("Course Offering"))
            {
                continue;
            }
            let Some(course_id) = number_field(course, &["OrgUnitId", "Id"]) else {
                continue;
            };
            let mut snapshot = CourseSnapshot {
                course: course.clone(),
                synced_at,
                tools: BTreeMap::new(),
                errors: BTreeMap::new(),
            };
            let base_tools = [
                "get_syllabus",
                "get_course_content",
                "get_announcements",
                "get_assignments",
                "get_my_grades",
                "get_roster",
                "get_classlist_emails",
                "get_discussions",
                "get_calendar_events",
                "list_quizzes",
                "get_my_groups",
            ];
            for name in base_tools {
                let args = course_args(course_id);
                match self.fetch_tool(name, args.clone()).await {
                    Ok(output) => {
                        snapshot
                            .tools
                            .insert(cache_key(name, &args)?, output_value(output));
                    }
                    Err(error) => {
                        snapshot
                            .errors
                            .insert(name.to_owned(), format!("{error:#}"));
                    }
                }
            }

            let assignments = cached_value(&snapshot, "get_assignments", course_id);
            for id in list_item_ids(assignments, &["FolderId", "AssignmentId", "Id"]) {
                for name in [
                    "get_feedback",
                    "get_my_submissions",
                    "get_assignment_rubric",
                    "get_assignment_files",
                ] {
                    let args = item_args(course_id, "assignment_id", id);
                    match self.fetch_tool(name, args.clone()).await {
                        Ok(output) => {
                            snapshot
                                .tools
                                .insert(cache_key(name, &args)?, output_value(output));
                        }
                        Err(error) => {
                            snapshot
                                .errors
                                .insert(format!("{name}/{id}"), format!("{error:#}"));
                        }
                    }
                }
            }

            let quizzes = cached_value(&snapshot, "list_quizzes", course_id);
            for id in list_item_ids(quizzes, &["QuizId", "Id"]) {
                let args = item_args(course_id, "quiz_id", id);
                match self.fetch_tool("get_quiz_attempts", args.clone()).await {
                    Ok(output) => {
                        snapshot
                            .tools
                            .insert(cache_key("get_quiz_attempts", &args)?, output_value(output));
                    }
                    Err(error) => {
                        snapshot
                            .errors
                            .insert(format!("get_quiz_attempts/{id}"), format!("{error:#}"));
                    }
                }
            }

            let announcements = cached_value(&snapshot, "get_announcements", course_id);
            for id in list_item_ids(announcements, &["AnnouncementId", "NewsItemId", "Id"]) {
                let args = item_args(course_id, "announcement_id", id);
                match self.fetch_tool("get_announcement", args.clone()).await {
                    Ok(output) => {
                        snapshot
                            .tools
                            .insert(cache_key("get_announcement", &args)?, output_value(output));
                    }
                    Err(error) => {
                        snapshot
                            .errors
                            .insert(format!("get_announcement/{id}"), format!("{error:#}"));
                    }
                }
            }

            let content = cached_value(&snapshot, "get_course_content", course_id).clone();
            for id in collect_numeric_fields(&content, &["TopicId"]) {
                let args = item_args(course_id, "topic_id", id);
                match self.fetch_tool("get_topic_file", args.clone()).await {
                    Ok(output) => {
                        snapshot
                            .tools
                            .insert(cache_key("get_topic_file", &args)?, output_value(output));
                    }
                    Err(error) => {
                        snapshot
                            .errors
                            .insert(format!("get_topic_file/{id}"), format!("{error:#}"));
                    }
                }
            }
            for id in collect_numeric_fields(&content, &["ModuleId"]) {
                let args = item_args(course_id, "module_id", id);
                match self.fetch_tool("get_module", args.clone()).await {
                    Ok(output) => {
                        snapshot
                            .tools
                            .insert(cache_key("get_module", &args)?, output_value(output));
                    }
                    Err(error) => {
                        snapshot
                            .errors
                            .insert(format!("get_module/{id}"), format!("{error:#}"));
                    }
                }
            }

            let file_paths = collect_course_file_paths(&snapshot.tools, course_id);
            for path in &file_paths {
                let args = BTreeMap::from([
                    ("course_id".to_owned(), json!(course_id)),
                    ("path".to_owned(), json!(path)),
                ]);
                match self.fetch_course_file(path).await {
                    Ok(output) => {
                        snapshot
                            .tools
                            .insert(cache_key("get_course_file", &args)?, output_value(output));
                    }
                    Err(error) => {
                        snapshot
                            .errors
                            .insert(format!("get_course_file/{path}"), format!("{error:#}"));
                    }
                }
            }

            self.snapshots.write_course(course_id, &snapshot)?;
            upcoming_courses.push(json!({
                "course_id": course_id,
                "assignments": cached_value(&snapshot, "get_assignments", course_id),
                "quizzes": cached_value(&snapshot, "list_quizzes", course_id),
            }));
            manifest.courses.push(json!({
                "course_id": course_id,
                "course": course,
                "synced_at": synced_at,
                "cached_tool_count": snapshot.tools.len(),
                "cached_course_file_count": file_paths.len(),
                "error_count": snapshot.errors.len(),
            }));
            count += 1;
        }
        manifest.tools.insert(
            cache_key("get_upcoming_due_dates", &BTreeMap::new())?,
            json!({"days_ahead": 14, "courses": upcoming_courses}),
        );
        self.snapshots.write_manifest(&manifest)?;
        pretty(json!({
            "synced_courses": count,
            "synced_at_unix_seconds": synced_at,
            "local_directory": "~/.brightspace-mcp-rs/synced-courses",
            "errors": manifest.errors,
            "browser_remains_open": true,
        }))
    }

    async fn read_cached_tool(&self, name: &str, args: &BTreeMap<String, Value>) -> Result<String> {
        let key = cache_key(name, args)?;
        if let Some(course_id) = optional_id(args, "course_id")? {
            let snapshot = self.snapshots.read_course(course_id)?;
            if let Some(value) = snapshot.tools.get(&key) {
                return render_cached(value);
            }
            let detail_id = [
                "assignment_id",
                "announcement_id",
                "topic_id",
                "module_id",
                "quiz_id",
            ]
            .iter()
            .find_map(|field| optional_id(args, field).ok().flatten());
            let file_error = args
                .get("path")
                .and_then(Value::as_str)
                .map(|path| format!("get_course_file/{path}"));
            let error = file_error
                .as_deref()
                .and_then(|key| snapshot.errors.get(key))
                .or_else(|| detail_id.and_then(|id| snapshot.errors.get(&format!("{name}/{id}"))))
                .or_else(|| snapshot.errors.get(name));
            if let Some(error) = error {
                bail!(
                    "last sync failed to fetch `{name}` for course {course_id}: {error}; run sync_courses to retry"
                );
            }
            bail!("`{name}` is not in the local snapshot for course {course_id}; run sync_courses");
        }
        let manifest = self.snapshots.read_manifest()?;
        manifest
            .tools
            .get(&key)
            .map(render_cached)
            .transpose()?
            .ok_or_else(|| anyhow!("`{name}` is not in the local snapshot; run sync_courses"))
    }

    async fn read_course_file(&self, args: &BTreeMap<String, Value>) -> Result<String> {
        let course_id = required(optional_id(args, "course_id")?, "course_id")?;
        let path = required_string(args, "path")?;
        let path = normalize_course_file_path(course_id, &path, &self.base_url)?;
        let normalized_args = BTreeMap::from([
            ("course_id".to_owned(), json!(course_id)),
            ("path".to_owned(), json!(path)),
        ]);
        self.read_cached_tool("get_course_file", &normalized_args)
            .await
    }

    async fn fetch_course_file(&self, path: &str) -> Result<String> {
        let response = self.get(path).await?;
        Ok(text_or_base64(&response.bytes().await?))
    }

    async fn list_synced_courses(&self) -> Result<String> {
        pretty(json!({
            "synced_at": self.snapshots.read_manifest()?.synced_at,
            "courses": self.snapshots.read_manifest()?.courses,
        }))
    }

    async fn sync_status(&self) -> Result<String> {
        let manifest = self.snapshots.read_manifest()?;
        pretty(json!({
            "authentication_status": "not checked; local reads do not sign in",
            "last_sync_unix_seconds": manifest.synced_at,
            "synced_course_count": manifest.courses.len(),
        }))
    }

    async fn read_synced_course(&self, args: &BTreeMap<String, Value>) -> Result<String> {
        let course_id = required(optional_id(args, "course_id")?, "course_id")?;
        let snapshot = self.snapshots.read_course(course_id)?;
        if let Some(section) = args.get("section").and_then(Value::as_str) {
            if let Some(value) = snapshot.tools.get(section) {
                return pretty(value.clone());
            }
            let matching: BTreeMap<_, _> = snapshot
                .tools
                .iter()
                .filter(|(key, _)| key.starts_with(&format!("{section}:")))
                .collect();
            if matching.is_empty() {
                bail!("section `{section}` is not in the local snapshot for course {course_id}");
            }
            return pretty(json!(matching));
        }
        pretty(serde_json::to_value(snapshot)?)
    }

    async fn search_cached(&self, args: &BTreeMap<String, Value>) -> Result<String> {
        let course_id = required(optional_id(args, "course_id")?, "course_id")?;
        let query = required_string(args, "query")?;
        let snapshot = self.snapshots.read_course(course_id)?;
        let mut hits = Vec::new();
        for (name, value) in &snapshot.tools {
            if name.starts_with("get_course_content:")
                || name.starts_with("get_announcements:")
                || name.starts_with("get_discussions:")
            {
                collect_matches(
                    value,
                    &query.to_lowercase(),
                    name.split(':').next().unwrap_or("course"),
                    &mut hits,
                );
            }
        }
        pretty(json!({"query": query, "course_id": course_id, "matches": hits}))
    }

    async fn render_path(
        &self,
        template: &str,
        args: &BTreeMap<String, Value>,
        query: &[(&str, &str)],
    ) -> Result<String> {
        let versions = self.api_versions().await?;
        let mut path = template
            .replace("{lp}", &versions.lp)
            .replace("{le}", &versions.le);
        for key in [
            "course_id",
            "assignment_id",
            "announcement_id",
            "attachment_id",
            "topic_id",
            "module_id",
            "quiz_id",
        ] {
            let placeholder = format!("{{{key}}}");
            if path.contains(&placeholder) {
                let id = required(optional_id(args, key)?, key)?;
                path = path.replace(&placeholder, &id.to_string());
            }
        }
        if !query.is_empty() {
            let mut serializer = url::form_urlencoded::Serializer::new(String::new());
            for (input, output) in query {
                if let Some(value) = args.get(*input) {
                    let encoded = match value {
                        Value::String(s) => s.clone(),
                        Value::Bool(b) => b.to_string(),
                        Value::Number(n) => n.to_string(),
                        _ => continue,
                    };
                    serializer.append_pair(output, &encoded);
                }
            }
            let encoded = serializer.finish();
            if !encoded.is_empty() {
                path.push('?');
                path.push_str(&encoded);
            }
        }
        Ok(path)
    }

    async fn groups(&self, course_id: u64) -> Result<String> {
        let versions = self.api_versions().await?;
        let lp = versions.lp;
        let categories = self
            .get_json(&format!("/d2l/api/lp/{lp}/{course_id}/groupcategories/"))
            .await?;
        let mut groups = Vec::new();
        for category in array_values(&categories) {
            if let Some(id) = number_field(category, &["GroupCategoryId", "Id"]) {
                let members = self
                    .get_json(&format!(
                        "/d2l/api/lp/{lp}/{course_id}/groupcategories/{id}/groups/"
                    ))
                    .await?;
                groups.push(json!({"category": category, "groups": members}));
            }
        }
        pretty(Value::Array(groups))
    }

    async fn upcoming(&self, args: BTreeMap<String, Value>) -> Result<String> {
        let versions = self.api_versions().await?;
        let days = args
            .get("days_ahead")
            .and_then(Value::as_u64)
            .unwrap_or(14)
            .clamp(1, 120);
        let courses = self
            .get_json(&format!(
                "/d2l/api/lp/{}/enrollments/myenrollments/",
                versions.lp
            ))
            .await?;
        let mut due = Vec::new();
        for item in array_values(&courses) {
            let Some(course_id) = course_id_from_enrollment(item) else {
                continue;
            };
            let assignments = self
                .get_json(&format!(
                    "/d2l/api/le/{}/{course_id}/dropbox/folders/",
                    versions.le
                ))
                .await?;
            let quizzes = self
                .get_json(&format!("/d2l/api/le/{}/{course_id}/quizzes/", versions.le))
                .await?;
            due.push(
                json!({"course_id": course_id, "assignments": assignments, "quizzes": quizzes}),
            );
        }
        pretty(json!({"days_ahead": days, "courses": due}))
    }

    async fn search(&self, course_id: u64, query: &str) -> Result<String> {
        let versions = self.api_versions().await?;
        let le = versions.le;
        let paths = [
            (
                "content",
                format!("/d2l/api/le/{le}/{course_id}/content/toc"),
            ),
            (
                "announcements",
                format!("/d2l/api/le/{le}/{course_id}/news/"),
            ),
            (
                "discussions",
                format!("/d2l/api/le/{le}/{course_id}/discussions/forums/"),
            ),
        ];
        let needle = query.to_lowercase();
        let mut hits = Vec::new();
        for (scope, path) in paths {
            let value = self.get_json(&path).await?;
            collect_matches(&value, &needle, scope, &mut hits);
        }
        pretty(json!({"query": query, "course_id": course_id, "matches": hits}))
    }

    async fn read_resource(&self, uri: &str) -> Result<String> {
        let (course_id, segments) = parse_brightspace_resource(uri)?;
        match segments.as_slice() {
            [syllabus] if syllabus == "syllabus" => {
                self.read_cached_tool("get_syllabus", &course_args(course_id))
                    .await
            }
            [content, topics, topic_id] if content == "content" && topics == "topics" => {
                let id = parse_id(topic_id, "topicId")?;
                self.read_cached_tool("get_topic_file", &item_args(course_id, "topic_id", id))
                    .await
            }
            [assignments, assignment_id, files]
                if assignments == "assignments" && files == "files" =>
            {
                let id = parse_id(assignment_id, "assignmentId")?;
                self.read_cached_tool(
                    "get_assignment_files",
                    &item_args(course_id, "assignment_id", id),
                )
                .await
            }
            [announcements, announcement_id] if announcements == "announcements" => {
                let id = parse_id(announcement_id, "announcementId")?;
                self.read_cached_tool(
                    "get_announcement",
                    &item_args(course_id, "announcement_id", id),
                )
                .await
            }
            _ => bail!("unsupported Brightspace resource URI"),
        }
    }
}

fn diagnostics<'a>(core: &'a Core, _args: &'a BTreeMap<String, Value>) -> ToolFuture<'a> {
    Box::pin(async move {
        pretty(json!({
            "server": "brightspace-mcp-rs",
            "base_url": core.base_url.origin().ascii_serialization(),
            "auth": "browser -> Microsoft SSO -> Microsoft Authenticator MFA",
            "course_snapshots": "local"
        }))
    })
}

fn audit_log<'a>(_core: &'a Core, _args: &'a BTreeMap<String, Value>) -> ToolFuture<'a> {
    Box::pin(async { Ok("No write operations are enabled; local write audit is empty.".into()) })
}

fn grades<'a>(core: &'a Core, args: &'a BTreeMap<String, Value>) -> ToolFuture<'a> {
    Box::pin(async move {
        let course_id = required(optional_id(args, "course_id")?, "course_id")?;
        let versions = core.api_versions().await?;
        let items = core
            .get_json(&format!("/d2l/api/le/{}/{course_id}/grades/", versions.le))
            .await?;
        let values = core
            .get_json(&format!(
                "/d2l/api/le/{}/{course_id}/grades/values/myGradeValues/",
                versions.le
            ))
            .await?;
        pretty(json!({"items": items, "values": values}))
    })
}

fn discussions<'a>(core: &'a Core, args: &'a BTreeMap<String, Value>) -> ToolFuture<'a> {
    Box::pin(async move {
        let course_id = required(optional_id(args, "course_id")?, "course_id")?;
        let versions = core.api_versions().await?;
        let lp = &versions.le;
        let forums = core
            .get_json(&format!("/d2l/api/le/{lp}/{course_id}/discussions/forums/"))
            .await?;
        let mut result = Vec::new();
        for forum in array_values(&forums) {
            if let Some(id) = number_field(forum, &["ForumId", "Id"]) {
                let topics = core
                    .get_json(&format!(
                        "/d2l/api/le/{lp}/{course_id}/discussions/forums/{id}/topics/"
                    ))
                    .await?;
                result.push(json!({"forum": forum, "topics": topics}));
            }
        }
        pretty(Value::Array(result))
    })
}

fn announcement<'a>(core: &'a Core, args: &'a BTreeMap<String, Value>) -> ToolFuture<'a> {
    Box::pin(async move {
        let course_id = required(optional_id(args, "course_id")?, "course_id")?;
        let announcement_id = required(optional_id(args, "announcement_id")?, "announcement_id")?;
        let versions = core.api_versions().await?;
        let path = match optional_id(args, "attachment_id")? {
            Some(file_id) => format!(
                "/d2l/api/le/{}/{course_id}/news/{announcement_id}/attachments/{file_id}",
                versions.le
            ),
            None => format!(
                "/d2l/api/le/{}/{course_id}/news/{announcement_id}",
                versions.le
            ),
        };
        let response = core.get(&path).await?;
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let bytes = response.bytes().await?;
        if content_type.contains("json") {
            pretty(serde_json::from_slice(&bytes)?)
        } else {
            Ok(text_or_base64(&bytes))
        }
    })
}

fn upcoming<'a>(core: &'a Core, args: &'a BTreeMap<String, Value>) -> ToolFuture<'a> {
    Box::pin(async move { core.upcoming(args.clone()).await })
}

fn search<'a>(core: &'a Core, args: &'a BTreeMap<String, Value>) -> ToolFuture<'a> {
    Box::pin(async move {
        let course_id = required(optional_id(args, "course_id")?, "course_id")?;
        core.search(course_id, &required_string(args, "query")?)
            .await
    })
}

fn groups<'a>(core: &'a Core, args: &'a BTreeMap<String, Value>) -> ToolFuture<'a> {
    Box::pin(async move {
        let course_id = required(optional_id(args, "course_id")?, "course_id")?;
        core.groups(course_id).await
    })
}

async fn discover_versions(http: &Client, base_url: &Url) -> Result<ApiVersions> {
    let url = base_url.join("d2l/api/versions/")?;
    let entries: Vec<Value> = http
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
        .context("Brightspace /d2l/api/versions/ returned invalid JSON")?;
    let version = |product: &str| {
        entries
            .iter()
            .find(|item| {
                item.get("ProductCode")
                    .and_then(Value::as_str)
                    .is_some_and(|v| v.eq_ignore_ascii_case(product))
            })
            .and_then(|item| item.get("LatestVersion"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    };
    Ok(ApiVersions {
        lp: version("lp")
            .ok_or_else(|| anyhow!("Brightspace did not advertise an LP API version"))?,
        le: version("le")
            .ok_or_else(|| anyhow!("Brightspace did not advertise an LE API version"))?,
    })
}

fn required(value: Option<u64>, name: &str) -> Result<u64> {
    value
        .filter(|v| *v > 0)
        .ok_or_else(|| anyhow!("required positive integer parameter `{name}` is missing"))
}

fn optional_id(args: &BTreeMap<String, Value>, key: &str) -> Result<Option<u64>> {
    args.get(key)
        .map(|v| {
            v.as_u64()
                .filter(|n| *n > 0)
                .ok_or_else(|| anyhow!("`{key}` must be a positive integer"))
        })
        .transpose()
}

fn required_string(args: &BTreeMap<String, Value>, key: &str) -> Result<String> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow!("required string parameter `{key}` is missing"))
}

fn pretty(value: Value) -> Result<String> {
    Ok(serde_json::to_string_pretty(&value)?)
}

fn array_values(value: &Value) -> &[Value] {
    value
        .as_array()
        .or_else(|| value.get("Items").and_then(Value::as_array))
        .map(Vec::as_slice)
        .unwrap_or_default()
}

fn unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn output_value(output: String) -> Value {
    serde_json::from_str(&output).unwrap_or(Value::String(output))
}

fn collect_course_file_paths(tools: &BTreeMap<String, Value>, course_id: u64) -> Vec<String> {
    fn visit(value: &Value, course_id: u64, paths: &mut std::collections::BTreeSet<String>) {
        match value {
            Value::String(text) => {
                let prefix = format!("/content/enforced/{course_id}-");
                let mut remaining = text.as_str();
                while let Some(start) = remaining.find(&prefix) {
                    let candidate = &remaining[start..];
                    let end = candidate
                        .find(|ch: char| {
                            ch.is_whitespace()
                                || matches!(ch, '\"' | '\'' | '<' | '>' | ')' | '?' | '#')
                        })
                        .unwrap_or(candidate.len());
                    let path = candidate[..end].replace("&amp;", "&");
                    if normalize_course_file_path(
                        course_id,
                        &path,
                        &Url::parse("https://brightspace.invalid").expect("static URL"),
                    )
                    .is_ok()
                    {
                        paths.insert(path);
                    }
                    remaining = &candidate[end.max(1)..];
                }
            }
            Value::Array(items) => items.iter().for_each(|item| visit(item, course_id, paths)),
            Value::Object(fields) => fields
                .values()
                .for_each(|item| visit(item, course_id, paths)),
            _ => {}
        }
    }
    let mut paths = std::collections::BTreeSet::new();
    for value in tools.values() {
        visit(value, course_id, &mut paths);
    }
    paths.into_iter().collect()
}

fn normalize_course_file_path(course_id: u64, input: &str, base_url: &Url) -> Result<String> {
    let url = if input.starts_with("https://") || input.starts_with("http://") {
        let url = Url::parse(input).context("course file URL is invalid")?;
        if url.origin() != base_url.origin() {
            bail!("course file URL must use the configured Brightspace host");
        }
        url
    } else {
        base_url.join(input)?
    };
    let path = url.path();
    let prefix = format!("/content/enforced/{course_id}-");
    if url.origin() != base_url.origin()
        || !path.starts_with(&prefix)
        || path.contains('\\')
        || path
            .split('/')
            .any(|segment| segment == "." || segment == "..")
        || path.to_ascii_lowercase().contains("%2e")
        || path.to_ascii_lowercase().contains("%2f")
        || path.to_ascii_lowercase().contains("%5c")
    {
        bail!("path must stay inside /content/enforced/{course_id}-… for this course");
    }
    Ok(path.to_owned())
}

#[cfg(test)]
mod course_file_tests {
    use super::{collect_course_file_paths, normalize_course_file_path};
    use serde_json::json;
    use std::collections::BTreeMap;
    use url::Url;

    #[test]
    fn sync_discovers_only_files_belonging_to_course() {
        let tools = BTreeMap::from([
            (
                "topic".to_owned(),
                json!("<a href=\"/content/enforced/42-secure/lecture%20one.pdf\">PDF</a>"),
            ),
            (
                "module".to_owned(),
                json!({"Description":"/content/enforced/43-other/file.pdf /content/enforced/42-secure/slides.pptx"}),
            ),
        ]);
        assert_eq!(
            collect_course_file_paths(&tools, 42),
            vec![
                "/content/enforced/42-secure/lecture%20one.pdf",
                "/content/enforced/42-secure/slides.pptx"
            ]
        );
    }

    #[test]
    fn course_file_paths_must_be_same_host_and_course_scoped() {
        let base = Url::parse("https://lms.example.edu").expect("base URL");
        assert_eq!(
            normalize_course_file_path(
                42,
                "https://lms.example.edu/content/enforced/42-secure/file.pdf?download=1",
                &base
            )
            .expect("same-host path"),
            "/content/enforced/42-secure/file.pdf"
        );
        for path in [
            "/content/enforced/43-other/file.pdf",
            "/content/enforced/42-secure/../43-other/file.pdf",
            "/content/enforced/42-secure/%2e%2e/file.pdf",
            "https://attacker.example/content/enforced/42-secure/file.pdf",
        ] {
            assert!(
                normalize_course_file_path(42, path, &base).is_err(),
                "{path}"
            );
        }
    }
}

fn render_cached(value: &Value) -> Result<String> {
    match value {
        Value::String(text) => Ok(text.clone()),
        _ => pretty(value.clone()),
    }
}

fn course_args(course_id: u64) -> BTreeMap<String, Value> {
    BTreeMap::from([("course_id".to_owned(), json!(course_id))])
}

fn item_args(course_id: u64, field: &str, id: u64) -> BTreeMap<String, Value> {
    BTreeMap::from([
        ("course_id".to_owned(), json!(course_id)),
        (field.to_owned(), json!(id)),
    ])
}

fn cached_value<'a>(snapshot: &'a CourseSnapshot, name: &str, course_id: u64) -> &'a Value {
    let key = cache_key(name, &course_args(course_id)).expect("serializing simple cache key");
    snapshot.tools.get(&key).unwrap_or(&Value::Null)
}

fn collect_numeric_fields(value: &Value, names: &[&str]) -> Vec<u64> {
    fn visit(value: &Value, names: &[&str], found: &mut std::collections::BTreeSet<u64>) {
        match value {
            Value::Array(items) => items.iter().for_each(|item| visit(item, names, found)),
            Value::Object(object) => {
                for name in names {
                    if let Some(id) = object
                        .get(*name)
                        .and_then(|value| value.as_u64().or_else(|| value.as_str()?.parse().ok()))
                        .filter(|id| *id > 0)
                    {
                        found.insert(id);
                    }
                }
                object.values().for_each(|child| visit(child, names, found));
            }
            _ => {}
        }
    }
    let mut found = std::collections::BTreeSet::new();
    visit(value, names, &mut found);
    found.into_iter().collect()
}

fn list_item_ids(value: &Value, names: &[&str]) -> Vec<u64> {
    array_values(value)
        .iter()
        .filter_map(|item| number_field(item, names))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn number_field(value: &Value, names: &[&str]) -> Option<u64> {
    names.iter().find_map(|name| {
        value
            .get(name)
            .and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()))
    })
}

fn course_id_from_enrollment(value: &Value) -> Option<u64> {
    value
        .get("OrgUnit")
        .and_then(|course| number_field(course, &["OrgUnitId", "Id"]))
        .or_else(|| number_field(value, &["OrgUnitId", "Id"]))
}

fn text_or_base64(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        Err(_) => format!(
            "Binary file ({} bytes), base64: {}",
            bytes.len(),
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes)
        ),
    }
}

fn parse_brightspace_resource(uri: &str) -> Result<(u64, Vec<String>)> {
    let url = Url::parse(uri).context("invalid resource URI")?;
    if url.scheme() != "brightspace" {
        bail!("resource URI must use brightspace://");
    }
    let course = url
        .host_str()
        .ok_or_else(|| anyhow!("resource URI has no course id"))?;
    let course_id = parse_id(course, "courseId")?;
    let segments = url
        .path_segments()
        .map(|parts| {
            parts
                .filter(|s| !s.is_empty())
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default();
    Ok((course_id, segments))
}

fn parse_id(value: &str, name: &str) -> Result<u64> {
    let id = value
        .parse::<u64>()
        .with_context(|| format!("{name} must be numeric"))?;
    if id == 0 {
        bail!("{name} must be positive");
    }
    Ok(id)
}

fn collect_matches(value: &Value, query: &str, scope: &str, hits: &mut Vec<Value>) {
    match value {
        Value::Array(items) => {
            for item in items {
                collect_matches(item, query, scope, hits);
            }
        }
        Value::Object(object) => {
            let rendered = serde_json::to_string(value).unwrap_or_default();
            if rendered.to_lowercase().contains(query) {
                hits.push(json!({"scope": scope, "item": value}));
            } else {
                for child in object.values() {
                    collect_matches(child, query, scope, hits);
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Core, TOOL_ROUTES, item_args, parse_brightspace_resource, parse_id, required_string,
    };
    use crate::auth::{SessionFuture, SessionProvider};
    use crate::snapshot::{CourseSnapshot, SnapshotStore, cache_key};
    use anyhow::Result;
    use reqwest::Client;
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::Mutex as AsyncMutex,
    };
    use url::Url;

    struct FakeSession {
        cookie: Mutex<&'static str>,
        forced_logins: AtomicUsize,
    }

    impl SessionProvider for FakeSession {
        fn cookie(&self, force_login: bool) -> SessionFuture<'_, Result<String>> {
            Box::pin(async move {
                if force_login {
                    self.forced_logins.fetch_add(1, Ordering::SeqCst);
                    *self.cookie.lock().expect("cookie lock") = "fresh=1";
                }
                Ok(self.cookie.lock().expect("cookie lock").to_string())
            })
        }

        fn clear(&self) -> SessionFuture<'_, ()> {
            Box::pin(async move {
                *self.cookie.lock().expect("cookie lock") = "expired=1";
            })
        }

        fn shutdown(&self) -> SessionFuture<'_, Result<()>> {
            Box::pin(async { Ok(()) })
        }
    }

    #[test]
    fn parses_all_four_resource_uri_shapes() {
        for uri in [
            "brightspace://42/syllabus",
            "brightspace://42/content/topics/10",
            "brightspace://42/assignments/20/files",
            "brightspace://42/announcements/30",
        ] {
            assert_eq!(
                parse_brightspace_resource(uri).expect("valid resource").0,
                42
            );
        }
    }

    #[test]
    fn rejects_non_positive_ids_and_missing_strings() {
        assert!(parse_id("0", "courseId").is_err());
        assert!(parse_id("../10", "courseId").is_err());
        assert!(
            required_string(&BTreeMap::from([("path".to_owned(), json!(" "))]), "path").is_err()
        );
    }

    #[test]
    fn registry_exposes_upstream_read_tools_and_local_sync_tools_only() {
        assert_eq!(TOOL_ROUTES.len(), 31);
        assert!(
            TOOL_ROUTES
                .iter()
                .all(|tool| !tool.name.contains("submit") && !tool.name.contains("post_"))
        );
    }

    #[tokio::test]
    async fn local_course_reads_do_not_request_authentication() {
        let session = Arc::new(FakeSession {
            cookie: Mutex::new("unused=1"),
            forced_logins: AtomicUsize::new(0),
        });
        let root =
            std::env::temp_dir().join(format!("brightspace-local-read-{}", std::process::id()));
        let store = SnapshotStore::new(root.clone());
        let args = BTreeMap::from([("course_id".to_owned(), json!(42))]);
        let tools = BTreeMap::from([
            (
                cache_key("get_syllabus", &args).expect("syllabus key"),
                json!({"Text":"cached"}),
            ),
            (
                cache_key("get_topic_file", &item_args(42, "topic_id", 10)).expect("topic key"),
                json!("cached topic"),
            ),
            (
                cache_key("get_assignment_files", &item_args(42, "assignment_id", 20))
                    .expect("assignment key"),
                json!([{"FileName":"work.pdf"}]),
            ),
            (
                cache_key("get_announcement", &item_args(42, "announcement_id", 30))
                    .expect("announcement key"),
                json!({"Title":"cached news"}),
            ),
        ]);
        store
            .write_course(
                42,
                &CourseSnapshot {
                    course: json!({"Id":42}),
                    synced_at: 7,
                    tools,
                    errors: BTreeMap::new(),
                },
            )
            .expect("write cached course");
        let core = Core {
            base_url: Url::parse("https://school.example/").expect("base URL"),
            versions: tokio::sync::Mutex::new(None),
            http: Client::new(),
            auth: session.clone(),
            refresh_lock: AsyncMutex::new(()),
            session_generation: std::sync::atomic::AtomicU64::new(0),
            snapshots: store,
            sync_lock: AsyncMutex::new(()),
        };
        assert!(
            core.call_tool("get_syllabus", args)
                .await
                .expect("local read")
                .contains("cached")
        );
        for (uri, expected) in [
            ("brightspace://42/syllabus", "cached"),
            ("brightspace://42/content/topics/10", "cached topic"),
            ("brightspace://42/assignments/20/files", "work.pdf"),
            ("brightspace://42/announcements/30", "cached news"),
        ] {
            assert!(
                core.read_resource(uri)
                    .await
                    .expect("cached resource")
                    .contains(expected)
            );
        }
        assert_eq!(session.forced_logins.load(Ordering::SeqCst), 0);
        core.snapshots.clear().expect("clean snapshots");
    }

    #[tokio::test]
    async fn retries_a_read_once_after_authorization_expires() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock D2L");
        let address = listener.local_addr().expect("mock address");
        let server = tokio::spawn(async move {
            let mut seen = Vec::new();
            for (status, body) in [
                ("401 Unauthorized", "expired"),
                ("200 OK", "{\"Identifier\":\"1\"}"),
            ] {
                let (mut stream, _) = listener.accept().await.expect("accept request");
                let mut request = Vec::new();
                let mut chunk = [0u8; 1024];
                loop {
                    let count = stream.read(&mut chunk).await.expect("read request");
                    if count == 0 {
                        break;
                    }
                    request.extend_from_slice(&chunk[..count]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                seen.push(String::from_utf8_lossy(&request).to_ascii_lowercase());
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("write response");
            }
            seen
        });

        let session = Arc::new(FakeSession {
            cookie: Mutex::new("expired=1"),
            forced_logins: AtomicUsize::new(0),
        });
        let core = Core {
            base_url: Url::parse(&format!("http://{address}/")).expect("base URL"),
            versions: tokio::sync::Mutex::new(None),
            http: Client::new(),
            auth: session.clone(),
            refresh_lock: AsyncMutex::new(()),
            session_generation: std::sync::atomic::AtomicU64::new(0),
            snapshots: SnapshotStore::new(std::env::temp_dir().join("brightspace-mcp-http-test")),
            sync_lock: AsyncMutex::new(()),
        };

        assert_eq!(
            core.get("/d2l/api/lp/1.0/users/whoami")
                .await
                .expect("retried response")
                .status(),
            200
        );
        let requests = server.await.expect("mock server task");
        assert!(requests[0].contains("cookie: expired=1"));
        assert!(requests[1].contains("cookie: fresh=1"));
        assert_eq!(session.forced_logins.load(Ordering::SeqCst), 1);
    }
}
