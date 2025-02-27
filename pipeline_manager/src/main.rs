//! DBSP Pipeline Manager provides an HTTP API to catalog, compile, and execute
//! SQL programs.
//!
//! The API is currently single-tenant: there is no concept of users or
//! permissions.  Multi-tenancy can be implemented by creating a manager
//! instance per tenant, which enables better separation of concerns,
//! resource isolation and fault isolation compared to buiding multitenancy
//! into the manager.
//!
//! # Architecture
//!
//! * Project database.  Projects (including SQL source code), configs, and
//!   pipelines are stored in a Postgres database.  The database is the only
//!   state that is expected to survive across server restarts.  Intermediate
//!   artifacts stored in the file system (see below) can be safely deleted.
//!
//! * Compiler.  The compiler generates a binary crate for each project and adds
//!   it to a cargo workspace that also includes libraries that come with the
//!   SQL libraries.  This way, all precompiled dependencies of the main crate
//!   are reused across projects, thus speeding up compilation.
//!
//! * Runner.  The runner component is responsible for starting and killing
//!   compiled pipelines and for interacting with them at runtime.  It also
//!   registers each pipeline with Prometheus.

// TODOs:
// * Tests.
// * Support multi-node DBSP deployments (the current architecture assumes that
//   pipelines and the Prometheus server run on the same host as this server).
// * Proper UI.

use actix_files as fs;
use actix_files::NamedFile;
use actix_web::{
    delete,
    dev::{ServiceFactory, ServiceRequest},
    get,
    http::{
        header::{CacheControl, CacheDirective},
        Method,
    },
    middleware::Logger,
    patch, post, web,
    web::Data as WebData,
    App, Error as ActixError, HttpRequest, HttpResponse, HttpServer, Responder,
    Result as ActixResult,
};
use actix_web_static_files::ResourceFiles;
use anyhow::{Error as AnyError, Result as AnyResult};
use clap::Parser;
use env_logger::Env;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::{fs::read, sync::Mutex};
use utoipa::{openapi::OpenApi as OpenApiDoc, OpenApi, ToSchema};
use utoipa_swagger_ui::SwaggerUi;

mod compiler;
mod config;
mod db;
mod runner;

pub(crate) use compiler::{Compiler, ProjectStatus};
pub(crate) use config::ManagerConfig;
use db::{ConfigId, DBError, PipelineId, ProjectDB, ProjectId, Version};
use runner::{Runner, RunnerError};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Server configuration YAML file.
    #[arg(short, long)]
    config_file: Option<String>,

    /// [Developers only] serve static content from the specified directory.
    /// Allows modifying JavaScript without restarting the server.
    #[arg(short, long)]
    static_html: Option<String>,
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "DBSP Pipeline Manager API",
        description = r"API to catalog, compile, and execute SQL programs.

# API concepts

* *Project*.  A project is a SQL script with a non-unique name and a unique ID
  attached to it.  The client can add, remove, modify, and compile projects.
  Compilation includes running the SQL-to-DBSP compiler followed by the Rust
  compiler.

* *Configuration*.  A project can have multiple configurations associated with
  it.  Similar to projects, one can add, remove, and modify configs.

* *Pipeline*.  A pipeline is a running instance of a compiled project based on
  one of the configs.  Clients can start multiple pipelines for a project with
  the same or different configs.

# Concurrency

The API prevents race conditions due to multiple users accessing the same
project or configuration concurrently.  An example is user 1 modifying the project,
while user 2 is starting a pipeline for the same project.  The pipeline
may end up running the old or the new version, potentially leading to
unexpected behaviors.  The API prevents such situations by associating a
monotonically increasing version number with each project and configuration.
Every request to compile the project or start a pipeline must include project
id _and_ version number. If the version number isn't equal to the current
version in the database, this means that the last version of the project
observed by the user is outdated, so the request is rejected."
    ),
    paths(
        list_projects,
        project_code,
        project_status,
        new_project,
        update_project,
        compile_project,
        cancel_project,
        delete_project,
        new_config,
        update_config,
        delete_config,
        list_project_configs,
        new_pipeline,
        list_project_pipelines,
        pipeline_status,
        pipeline_metadata,
        pipeline_start,
        pipeline_pause,
        shutdown_pipeline,
        delete_pipeline,
    ),
    components(schemas(
        db::ProjectDescr,
        db::ConfigDescr,
        db::PipelineDescr,
        ProjectId,
        PipelineId,
        ConfigId,
        Version,
        ProjectStatus,
        ErrorResponse,
        ProjectCodeResponse,
        ProjectStatusResponse,
        NewProjectRequest,
        NewProjectResponse,
        UpdateProjectRequest,
        UpdateProjectResponse,
        CompileProjectRequest,
        CancelProjectRequest,
        NewConfigRequest,
        NewConfigResponse,
        UpdateConfigRequest,
        UpdateConfigResponse,
        NewPipelineRequest,
        NewPipelineResponse,
        ShutdownPipelineRequest,
    ),),
    tags(
        (name = "Project", description = "Manage projects"),
        (name = "Config", description = "Manage project configurations"),
        (name = "Pipeline", description = "Manage project pipelines"),
    ),
)]
pub struct ApiDoc;

#[actix_web::main]
async fn main() -> AnyResult<()> {
    // Create env logger.
    env_logger::Builder::from_env(Env::default().default_filter_or("info")).init();

    let args = Args::try_parse()?;

    let config_file = &args
        .config_file
        .unwrap_or_else(|| "config.yaml".to_string());
    let config_yaml = read(config_file)
        .await
        .map_err(|e| AnyError::msg(format!("error reading config file '{config_file}': {e}")))?;
    let config_yaml = String::from_utf8_lossy(&config_yaml);
    let mut config: ManagerConfig = serde_yaml::from_str(&config_yaml)
        .map_err(|e| AnyError::msg(format!("error parsing config file '{config_file}': {e}")))?;

    if let Some(static_html) = &args.static_html {
        config.static_html = Some(static_html.clone());
    }
    let config = config.canonicalize().await?;

    run(config).await
}

struct ServerState {
    // Serialize DB access with a lock, so we don't need to deal with
    // transaction conflicts.  The server must avoid holding this lock
    // for a long time to avoid blocking concurrent requests.
    db: Arc<Mutex<ProjectDB>>,
    // Dropping this handle kills the compiler task.
    _compiler: Compiler,
    runner: Runner,
    config: ManagerConfig,
}

impl ServerState {
    async fn new(
        config: ManagerConfig,
        db: Arc<Mutex<ProjectDB>>,
        compiler: Compiler,
    ) -> AnyResult<Self> {
        let runner = Runner::new(db.clone(), &config).await?;

        Ok(Self {
            db,
            _compiler: compiler,
            runner,
            config,
        })
    }
}

async fn run(config: ManagerConfig) -> AnyResult<()> {
    let db = Arc::new(Mutex::new(ProjectDB::connect(&config).await?));
    let compiler = Compiler::new(&config, db.clone()).await?;

    // Since we don't trust any file system state after restart,
    // reset all projects to `ProjectStatus::None`, which will force
    // us to recompile projects before running them.
    db.lock().await.reset_project_status().await?;
    let bind_address = config.bind_address.clone();
    let bind_port = config.port;
    let state = WebData::new(ServerState::new(config, db, compiler).await?);
    let openapi = ApiDoc::openapi();

    HttpServer::new(move || {
        build_app(
            App::new().wrap(Logger::default()),
            state.clone(),
            openapi.clone(),
        )
    })
    .bind((bind_address, bind_port))?
    .run()
    .await?;

    Ok(())
}

// `static_files` magic.
include!(concat!(env!("OUT_DIR"), "/generated.rs"));

fn build_app<T>(app: App<T>, state: WebData<ServerState>, openapi: OpenApiDoc) -> App<T>
where
    T: ServiceFactory<ServiceRequest, Config = (), Error = ActixError, InitError = ()>,
{
    // Creates a dictionary of static files indexed by file name.
    let generated = generate();

    // Extract the contents of `index.html`, so we can serve it on the
    // root endpoint (`/`), while other files are served via the`/static`
    // endpoint.
    let index_data = match generated.get("index.html") {
        None => "<html><head><title>DBSP manager</title></head></html>"
            .as_bytes()
            .to_owned(),
        Some(resource) => resource.data.to_owned(),
    };

    let app = app
        .app_data(state.clone())
        .service(list_projects)
        .service(project_code)
        .service(project_status)
        .service(new_project)
        .service(update_project)
        .service(compile_project)
        .service(delete_project)
        .service(new_config)
        .service(update_config)
        .service(delete_config)
        .service(list_project_configs)
        .service(new_pipeline)
        .service(list_project_pipelines)
        .service(pipeline_status)
        .service(pipeline_metadata)
        .service(pipeline_start)
        .service(pipeline_pause)
        .service(shutdown_pipeline)
        .service(delete_pipeline)
        .service(SwaggerUi::new("/swagger-ui/{_:.*}").url("/api-doc/openapi.json", openapi));

    if let Some(static_html) = &state.config.static_html {
        // Serve static contents from the file system.
        app.route("/", web::get().to(index))
            .service(fs::Files::new("/static", static_html).show_files_listing())
    } else {
        // Serve static contents embedded in the program.
        app.route(
            "/",
            web::get().to(move || {
                let index_data = index_data.clone();
                async { HttpResponse::Ok().body(index_data) }
            }),
        )
        .service(ResourceFiles::new("/static", generated))
    }
}

async fn index() -> ActixResult<NamedFile> {
    Ok(NamedFile::open("static/index.html")?)
}

/// Pipeline manager error response.
#[derive(Serialize, ToSchema)]
pub(crate) struct ErrorResponse {
    #[schema(example = "Unknown project id 42.")]
    message: String,
}

impl ErrorResponse {
    pub(crate) fn new(message: &str) -> Self {
        Self {
            message: message.to_string(),
        }
    }
}

fn http_resp_from_error(error: &AnyError) -> HttpResponse {
    if let Some(db_error) = error.downcast_ref::<DBError>() {
        let message = db_error.to_string();
        match db_error {
            DBError::UnknownProject(_) => HttpResponse::NotFound(),
            DBError::OutdatedProjectVersion(_) => HttpResponse::Conflict(),
            DBError::UnknownConfig(_) => HttpResponse::NotFound(),
            DBError::UnknownPipeline(_) => HttpResponse::NotFound(),
        }
        .json(ErrorResponse::new(&message))
    } else if let Some(runner_error) = error.downcast_ref::<RunnerError>() {
        let message = runner_error.to_string();
        match runner_error {
            RunnerError::PipelineShutdown(_) => HttpResponse::Conflict(),
        }
        .json(ErrorResponse::new(&message))
    } else {
        HttpResponse::InternalServerError().json(ErrorResponse::new(&error.to_string()))
    }
}

fn parse_project_id_param(req: &HttpRequest) -> Result<ProjectId, HttpResponse> {
    match req.match_info().get("project_id") {
        None => Err(HttpResponse::BadRequest().body("missing project id argument")),
        Some(project_id) => {
            match project_id.parse::<i64>() {
                Err(e) => Err(HttpResponse::BadRequest()
                    .body(format!("invalid project id '{project_id}': {e}"))),
                Ok(project_id) => Ok(ProjectId(project_id)),
            }
        }
    }
}

fn parse_config_id_param(req: &HttpRequest) -> Result<ConfigId, HttpResponse> {
    match req.match_info().get("config_id") {
        None => Err(HttpResponse::BadRequest().body("missing config id argument")),
        Some(config_id) => match config_id.parse::<i64>() {
            Err(e) => Err(HttpResponse::BadRequest()
                .body(format!("invalid configuration id '{config_id}': {e}"))),
            Ok(config_id) => Ok(ConfigId(config_id)),
        },
    }
}

fn parse_pipeline_id_param(req: &HttpRequest) -> Result<PipelineId, HttpResponse> {
    match req.match_info().get("pipeline_id") {
        None => Err(HttpResponse::BadRequest().body("missing pipeline id argument")),
        Some(pipeline_id) => match pipeline_id.parse::<i64>() {
            Err(e) => Err(HttpResponse::BadRequest()
                .body(format!("invalid pipeline id '{pipeline_id}': {e}"))),
            Ok(pipeline_id) => Ok(PipelineId(pipeline_id)),
        },
    }
}

/// Enumerate the project database.
#[utoipa::path(
    responses(
        (status = OK, description = "List of projects retrieved successfully", body = [ProjectDescr]),
    ),
    tag = "Project"
)]
#[get("/projects")]
async fn list_projects(state: WebData<ServerState>) -> impl Responder {
    state
        .db
        .lock()
        .await
        .list_projects()
        .await
        .map(|projects| {
            HttpResponse::Ok()
                .insert_header(CacheControl(vec![CacheDirective::NoCache]))
                .json(projects)
        })
        .unwrap_or_else(|e| http_resp_from_error(&e))
}

/// Response to a project code request.
#[derive(Serialize, ToSchema)]
struct ProjectCodeResponse {
    /// Current project version.
    version: Version,
    /// Project code.
    code: String,
}

/// Returns the latest SQL source code of the project.
#[utoipa::path(
    responses(
        (status = OK, description = "Project code retrieved successfully.", body = ProjectCodeResponse),
        (status = BAD_REQUEST
            , description = "Missing or invalid `project_id` parameter."
            , body = ErrorResponse
            , example = json!(ErrorResponse::new("Missing 'project_id' parameter."))),
        (status = NOT_FOUND
            , description = "Specified `project_id` does not exist in the database."
            , body = ErrorResponse
            , example = json!(ErrorResponse::new("Unknown project id '42'"))),
    ),
    params(
        ("project_id" = i64, Path, description = "Unique project identifier")
    ),
    tag = "Project"
)]
#[get("/projects/{project_id}/code")]
async fn project_code(state: WebData<ServerState>, req: HttpRequest) -> impl Responder {
    let project_id = match parse_project_id_param(&req) {
        Err(e) => {
            return e;
        }
        Ok(project_id) => project_id,
    };

    state
        .db
        .lock()
        .await
        .project_code(project_id)
        .await
        .map(|(version, code)| {
            HttpResponse::Ok()
                .insert_header(CacheControl(vec![CacheDirective::NoCache]))
                .json(&ProjectCodeResponse { version, code })
        })
        .unwrap_or_else(|e| http_resp_from_error(&e))
}

/// Response to a project status request.
#[derive(Serialize, ToSchema)]
struct ProjectStatusResponse {
    /// Current project version.
    version: Version,
    /// Project compilation status.
    status: ProjectStatus,
}

/// Returns current project version and compilation status.
#[utoipa::path(
    responses(
        (status = OK, description = "Project status retrieved successfully.", body = ProjectStatusResponse),
        (status = BAD_REQUEST
            , description = "Missing or invalid `project_id` parameter."
            , body = ErrorResponse
            , example = json!(ErrorResponse::new("Missing 'project_id' parameter."))),
        (status = NOT_FOUND
            , description = "Specified `project_id` does not exist in the database."
            , body = ErrorResponse
            , example = json!(ErrorResponse::new("Unknown project id '42'"))),
    ),
    params(
        ("project_id" = i64, Path, description = "Unique project identifier")
    ),
    tag = "Project"
)]
#[get("/projects/{project_id}")]
async fn project_status(state: WebData<ServerState>, req: HttpRequest) -> impl Responder {
    let project_id = match parse_project_id_param(&req) {
        Err(e) => {
            return e;
        }
        Ok(project_id) => project_id,
    };

    state
        .db
        .lock()
        .await
        .get_project(project_id)
        .await
        .map(|descr| {
            HttpResponse::Ok()
                .insert_header(CacheControl(vec![CacheDirective::NoCache]))
                .json(&ProjectStatusResponse {
                    version: descr.version,
                    status: descr.status,
                })
        })
        .unwrap_or_else(|e| http_resp_from_error(&e))
}

/// Request to create a new DBSP project.
#[derive(Deserialize, ToSchema)]
struct NewProjectRequest {
    /// Project name.
    #[schema(example = "Example project")]
    name: String,
    /// SQL code of the project.
    #[schema(example = "CREATE TABLE Example(name varchar);")]
    code: String,
}

/// Response to a new project request.
#[derive(Serialize, ToSchema)]
struct NewProjectResponse {
    /// Id of the newly created project.
    #[schema(example = 42)]
    project_id: ProjectId,
    /// Initial project version (this field is always set to 1).
    #[schema(example = 1)]
    version: Version,
}

/// Create a new project.
#[utoipa::path(
    request_body = NewProjectRequest,
    responses(
        (status = CREATED, description = "Project created successfully", body = NewProjectResponse),
    ),
    tag = "Project"
)]
#[post("/projects")]
async fn new_project(
    state: WebData<ServerState>,
    request: web::Json<NewProjectRequest>,
) -> impl Responder {
    state
        .db
        .lock()
        .await
        .new_project(&request.name, &request.code)
        .await
        .map(|(project_id, version)| {
            HttpResponse::Created()
                .insert_header(CacheControl(vec![CacheDirective::NoCache]))
                .json(&NewProjectResponse {
                    project_id,
                    version,
                })
        })
        .unwrap_or_else(|e| http_resp_from_error(&e))
}

/// Update project request.
#[derive(Deserialize, ToSchema)]
struct UpdateProjectRequest {
    /// Id of the project.
    project_id: ProjectId,
    /// New name for the project.
    name: String,
    /// New SQL code for the project or `None` to keep existing project
    /// code unmodified.
    code: Option<String>,
}

/// Response to an project update request.
#[derive(Serialize, ToSchema)]
struct UpdateProjectResponse {
    /// New project version.  Equals the previous version if project code
    /// doesn't change or previous version +1 if it does.
    version: Version,
}

/// Change project code and/or name.
///
/// If project code changes, any ongoing compilation gets cancelled,
/// project status is reset to `None`, and project version
/// is incremented by 1.  Changing project name only doesn't affect its
/// version or the compilation process.
#[utoipa::path(
    request_body = UpdateProjectRequest,
    responses(
        (status = OK, description = "Project updated successfully.", body = UpdateProjectResponse),
        (status = NOT_FOUND
            , description = "Specified `project_id` does not exist in the database."
            , body = ErrorResponse
            , example = json!(ErrorResponse::new("Unknown project id '42'"))),
    ),
    tag = "Project"
)]
#[patch("/projects")]
async fn update_project(
    state: WebData<ServerState>,
    request: web::Json<UpdateProjectRequest>,
) -> impl Responder {
    state
        .db
        .lock()
        .await
        .update_project(request.project_id, &request.name, &request.code)
        .await
        .map(|version| {
            HttpResponse::Ok()
                .insert_header(CacheControl(vec![CacheDirective::NoCache]))
                .json(&UpdateProjectResponse { version })
        })
        .unwrap_or_else(|e| http_resp_from_error(&e))
}

/// Request to queue a project for compilation.
#[derive(Deserialize, ToSchema)]
struct CompileProjectRequest {
    /// Project id.
    project_id: ProjectId,
    /// Latest project version known to the client.
    version: Version,
}

/// Queue project for compilation.
///
/// The client should poll the `/project_status` endpoint
/// for compilation results.
#[utoipa::path(
    request_body = CompileProjectRequest,
    responses(
        (status = ACCEPTED, description = "Compilation request submitted."),
        (status = NOT_FOUND
            , description = "Specified `project_id` does not exist in the database."
            , body = ErrorResponse
            , example = json!(ErrorResponse::new("Unknown project id '42'"))),
        (status = CONFLICT
            , description = "Project version specified in the request doesn't match the latest project version in the database."
            , body = ErrorResponse
            , example = json!(ErrorResponse::new("Outdated project version '{version}'"))),
    ),
    tag = "Project"
)]
#[post("/projects/compile")]
async fn compile_project(
    state: WebData<ServerState>,
    request: web::Json<CompileProjectRequest>,
) -> impl Responder {
    state
        .db
        .lock()
        .await
        .set_project_pending(request.project_id, request.version)
        .await
        .map(|_| HttpResponse::Accepted().finish())
        .unwrap_or_else(|e| http_resp_from_error(&e))
}

/// Request to cancel ongoing project compilation.
#[derive(Deserialize, ToSchema)]
struct CancelProjectRequest {
    /// Project id.
    project_id: ProjectId,
    /// Latest project version known to the client.
    version: Version,
}

/// Cancel outstanding compilation request.
///
/// The client should poll the `/project_status` endpoint
/// to determine when the cancelation request completes.
#[utoipa::path(
    request_body = CancelProjectRequest,
    responses(
        (status = ACCEPTED, description = "Cancelation request submitted."),
        (status = NOT_FOUND
            , description = "Specified `project_id` does not exist in the database."
            , body = ErrorResponse
            , example = json!(ErrorResponse::new("Unknown project id '42'"))),
        (status = CONFLICT
            , description = "Project version specified in the request doesn't match the latest project version in the database."
            , body = ErrorResponse
            , example = json!(ErrorResponse::new("Outdated project version '{3}'"))),
    ),
    tag = "Project"
)]
#[delete("/projects/compile")]
async fn cancel_project(
    state: WebData<ServerState>,
    request: web::Json<CancelProjectRequest>,
) -> impl Responder {
    state
        .db
        .lock()
        .await
        .cancel_project(request.project_id, request.version)
        .await
        .map(|_| HttpResponse::Accepted().finish())
        .unwrap_or_else(|e| http_resp_from_error(&e))
}

/// Delete a project.
#[utoipa::path(
    responses(
        (status = OK, description = "Project successfully deleted."),
        (status = NOT_FOUND
            , description = "Specified `project_id` does not exist in the database."
            , body = ErrorResponse
            , example = json!(ErrorResponse::new("Unknown project id '42'"))),
        (status = BAD_REQUEST
            , description = "The project has one or more running pipelines."
            , body = ErrorResponse
            , example = json!(ErrorResponse::new("Cannot delete a project while some of its pipelines are running"))),
    ),
    params(
        ("project_id" = i64, Path, description = "Unique project identifier")
    ),
    tag = "Project"
)]
#[delete("/projects/{project_id}")]
async fn delete_project(state: WebData<ServerState>, req: HttpRequest) -> impl Responder {
    let project_id = match parse_project_id_param(&req) {
        Err(e) => {
            return e;
        }
        Ok(project_id) => project_id,
    };

    let db = state.db.lock().await;

    match db.list_project_pipelines(project_id).await {
        Ok(pipelines) => {
            if pipelines.iter().any(|pipeline| !pipeline.killed) {
                return HttpResponse::BadRequest()
                    .body("Cannot delete a project while some of its pipelines are running.");
            }
        }
        Err(e) => return http_resp_from_error(&e),
    }

    db.delete_project(project_id)
        .await
        .map(|_| HttpResponse::Ok().finish())
        .unwrap_or_else(|e| http_resp_from_error(&e))
}

/// Request to create a new project configuration.
#[derive(Deserialize, ToSchema)]
struct NewConfigRequest {
    /// Project to create config for.
    project_id: ProjectId,
    /// Config name.
    name: String,
    /// YAML code for the config.
    config: String,
}

/// Response to a config creation request.
#[derive(Serialize, ToSchema)]
struct NewConfigResponse {
    /// Id of the newly created config.
    config_id: ConfigId,
    /// Initial config version (this field is always set to 1).
    version: Version,
}

/// Create a new project configuration.
#[utoipa::path(
    request_body = NewConfigRequest,
    responses(
        (status = OK, description = "Configuration successfully created.", body = NewConfigResponse),
        (status = NOT_FOUND
            , description = "Specified `project_id` does not exist in the database."
            , body = ErrorResponse
            , example = json!(ErrorResponse::new("Unknown project id '42'"))),
    ),
    tag = "Config"
)]
#[post("/configs")]
async fn new_config(
    state: WebData<ServerState>,
    request: web::Json<NewConfigRequest>,
) -> impl Responder {
    state
        .db
        .lock()
        .await
        .new_config(request.project_id, &request.name, &request.config)
        .await
        .map(|(config_id, version)| {
            HttpResponse::Ok()
                .insert_header(CacheControl(vec![CacheDirective::NoCache]))
                .json(&NewConfigResponse { config_id, version })
        })
        .unwrap_or_else(|e| http_resp_from_error(&e))
}

/// Request to update an existing project configuration.
#[derive(Deserialize, ToSchema)]
struct UpdateConfigRequest {
    /// Config id.
    config_id: ConfigId,
    /// New config name.
    name: String,
    /// New config YAML. If absent, existing YAML will be kept unmodified.
    config: Option<String>,
}

/// Response to a config update request.
#[derive(Serialize, ToSchema)]
struct UpdateConfigResponse {
    /// New config version.  Equals the previous version +1.
    version: Version,
}

/// Update existing project configuration.
///
/// Updates project config name and, optionally, code.
/// On success, increments config version by 1.
#[utoipa::path(
    request_body = UpdateConfigRequest,
    responses(
        (status = OK, description = "Configuration successfully updated.", body = UpdateConfigResponse),
        (status = NOT_FOUND
            , description = "Specified `config_id` does not exist in the database."
            , body = ErrorResponse
            , example = json!(ErrorResponse::new("Unknown config id '5'"))),
    ),
    tag = "Config"
)]
#[patch("/configs")]
async fn update_config(
    state: WebData<ServerState>,
    request: web::Json<UpdateConfigRequest>,
) -> impl Responder {
    state
        .db
        .lock()
        .await
        .update_config(request.config_id, &request.name, &request.config)
        .await
        .map(|version| {
            HttpResponse::Ok()
                .insert_header(CacheControl(vec![CacheDirective::NoCache]))
                .json(&UpdateConfigResponse { version })
        })
        .unwrap_or_else(|e| http_resp_from_error(&e))
}

/// Delete existing project configuration.
#[utoipa::path(
    responses(
        (status = OK, description = "Configuration successfully deleted."),
        (status = NOT_FOUND
            , description = "Specified `config_id` does not exist in the database."
            , body = ErrorResponse
            , example = json!(ErrorResponse::new("Unknown config id '5'"))),
    ),
    tag = "Config"
)]
#[delete("/configs/{config_id}")]
async fn delete_config(state: WebData<ServerState>, req: HttpRequest) -> impl Responder {
    let config_id = match parse_config_id_param(&req) {
        Err(e) => {
            return e;
        }
        Ok(config_id) => config_id,
    };

    state
        .db
        .lock()
        .await
        .delete_config(config_id)
        .await
        .map(|_| HttpResponse::Ok().finish())
        .unwrap_or_else(|e| http_resp_from_error(&e))
}

/// List project configurations.
#[utoipa::path(
    responses(
        (status = OK, description = "Project config list retrieved successfully.", body = [ConfigDescr]),
        (status = NOT_FOUND
            , description = "Specified `project_id` does not exist in the database."
            , body = ErrorResponse
            , example = json!(ErrorResponse::new("Unknown project id '42'"))),
    ),
    params(
        ("project_id" = i64, Path, description = "Unique project identifier")
    ),
    tag = "Config"
)]
#[get("/projects/{project_id}/configs")]
async fn list_project_configs(state: WebData<ServerState>, req: HttpRequest) -> impl Responder {
    let project_id = match parse_project_id_param(&req) {
        Err(e) => {
            return e;
        }
        Ok(project_id) => project_id,
    };

    state
        .db
        .lock()
        .await
        .list_project_configs(project_id)
        .await
        .map(|configs| {
            HttpResponse::Ok()
                .insert_header(CacheControl(vec![CacheDirective::NoCache]))
                .json(configs)
        })
        .unwrap_or_else(|e| http_resp_from_error(&e))
}

/// Request to create a new pipeline.
#[derive(Deserialize, ToSchema)]
pub(self) struct NewPipelineRequest {
    /// Project id to create pipeline for.
    project_id: ProjectId,
    /// Latest project version known to the client.
    project_version: Version,
    /// Project config to run the pipeline with.
    config_id: ConfigId,
    /// Latest config version known to the client.
    config_version: Version,
}

/// Response to a pipeline creation request.
#[derive(Serialize, ToSchema)]
struct NewPipelineResponse {
    /// Unique id assigned to the new pipeline.
    pipeline_id: PipelineId,
    /// TCP port that the pipeline process listens on.
    port: u16,
}

/// Launch a new pipeline.
///
/// Create a new pipeline for the specified project and configuration.
/// This is a synchronous endpoint, which sends a response once
/// the pipeline has been initialized.
#[utoipa::path(
    request_body = NewPipelineRequest,
    responses(
        (status = OK, description = "Pipeline successfully created.", body = NewPipelineResponse),
        (status = NOT_FOUND
            , description = "Specified `project_id` or `config_id` does not exist in the database."
            , body = ErrorResponse
            , example = json!(ErrorResponse::new("Unknown config id '5'"))),
        (status = CONFLICT
            , description = "Project or config version in the request doesn't match the latest version in the database."
            , body = ErrorResponse
            , example = json!(ErrorResponse::new("Outdated project version '{3}'"))),
        (status = BAD_REQUEST
            , description = "`config_id` refers to a config that does not belong to `project_id`."
            , body = ErrorResponse
            , example = json!(ErrorResponse::new("Config '9' does not belong to project '15'"))),
        (status = INTERNAL_SERVER_ERROR
            , description = "Pipeline process failed to initialize."
            , body = ErrorResponse
            , example = json!(ErrorResponse::new("Failed to run 'project42': permission denied"))),
    ),
    tag = "Pipeline"
)]
#[post("/pipelines")]
async fn new_pipeline(
    state: WebData<ServerState>,
    request: web::Json<NewPipelineRequest>,
) -> impl Responder {
    state
        .runner
        .run_pipeline(&request)
        .await
        .unwrap_or_else(|e| http_resp_from_error(&e))
}

/// List pipelines associated with a project.
#[utoipa::path(
    responses(
        (status = OK, description = "Project pipeline list retrieved successfully.", body = [PipelineDescr]),
        (status = NOT_FOUND
            , description = "Specified `project_id` does not exist in the database."
            , body = ErrorResponse
            , example = json!(ErrorResponse::new("Unknown project id '42'"))),
    ),
    params(
        ("project_id" = i64, Path, description = "Unique project identifier")
    ),
    tag = "Pipeline"
)]
#[get("/projects/{project_id}/pipelines")]
async fn list_project_pipelines(state: WebData<ServerState>, req: HttpRequest) -> impl Responder {
    let project_id = match parse_project_id_param(&req) {
        Err(e) => {
            return e;
        }
        Ok(project_id) => project_id,
    };

    state
        .db
        .lock()
        .await
        .list_project_pipelines(project_id)
        .await
        .map(|pipelines| {
            HttpResponse::Ok()
                .insert_header(CacheControl(vec![CacheDirective::NoCache]))
                .json(pipelines)
        })
        .unwrap_or_else(|e| http_resp_from_error(&e))
}

/// Retrieve pipeline status and performance counters.
#[utoipa::path(
    responses(
        // TODO: figure out how to specify that response contains arbitrary JSON,
        // or, better yet, implement `ToSchema` for `ControllerStatus`, which is the
        // actual type returned by this endpoint.
        (status = OK, description = "Pipeline status retrieved successfully.", body = String),
        (status = NOT_FOUND
            , description = "Specified `pipeline_id` does not exist in the database."
            , body = ErrorResponse
            , example = json!(ErrorResponse::new("Unknown pipeline id '13'"))),
    ),
    params(
        ("pipeline_id" = i64, Path, description = "Unique pipeline identifier")
    ),
    tag = "Pipeline"
)]
#[get("/pipelines/{pipeline_id}/status")]
async fn pipeline_status(state: WebData<ServerState>, req: HttpRequest) -> impl Responder {
    let pipeline_id = match parse_pipeline_id_param(&req) {
        Err(e) => {
            return e;
        }
        Ok(pipeline_id) => pipeline_id,
    };

    state
        .runner
        .forward_to_pipeline(pipeline_id, Method::GET, "status")
        .await
        .unwrap_or_else(|e| http_resp_from_error(&e))
}

/// Retrieve pipeline metadata.
#[utoipa::path(
    responses(
        (status = OK, description = "Pipeline metadata retrieved successfully.", body = String),
        (status = NOT_FOUND
            , description = "Specified `pipeline_id` does not exist in the database."
            , body = ErrorResponse
            , example = json!(ErrorResponse::new("Unknown pipeline id '13'"))),
    ),
    params(
        ("pipeline_id" = i64, Path, description = "Unique pipeline identifier")
    ),
    tag = "Pipeline"
)]
#[get("/pipelines/{pipeline_id}/metadata")]
async fn pipeline_metadata(state: WebData<ServerState>, req: HttpRequest) -> impl Responder {
    let pipeline_id = match parse_pipeline_id_param(&req) {
        Err(e) => {
            return e;
        }
        Ok(pipeline_id) => pipeline_id,
    };

    state
        .runner
        .forward_to_pipeline(pipeline_id, Method::GET, "metadata")
        .await
        .unwrap_or_else(|e| http_resp_from_error(&e))
}

/// Start pipeline.
#[utoipa::path(
    responses(
        (status = OK, description = "Pipeline started."),
        (status = NOT_FOUND
            , description = "Specified `pipeline_id` does not exist in the database."
            , body = ErrorResponse
            , example = json!(ErrorResponse::new("Unknown pipeline id '13'"))),
    ),
    params(
        ("pipeline_id" = i64, Path, description = "Unique pipeline identifier")
    ),
    tag = "Pipeline"
)]
#[post("/pipelines/{pipeline_id}/start")]
async fn pipeline_start(state: WebData<ServerState>, req: HttpRequest) -> impl Responder {
    let pipeline_id = match parse_pipeline_id_param(&req) {
        Err(e) => {
            return e;
        }
        Ok(pipeline_id) => pipeline_id,
    };

    state
        .runner
        .forward_to_pipeline(pipeline_id, Method::GET, "start")
        .await
        .unwrap_or_else(|e| http_resp_from_error(&e))
}

/// Pause pipeline.
#[utoipa::path(
    responses(
        (status = OK, description = "Pipeline paused."),
        (status = NOT_FOUND
            , description = "Specified `pipeline_id` does not exist in the database."
            , body = ErrorResponse
            , example = json!(ErrorResponse::new("Unknown pipeline id '13'"))),
    ),
    params(
        ("pipeline_id" = i64, Path, description = "Unique pipeline identifier")
    ),
    tag = "Pipeline"
)]
#[post("/pipelines/{pipeline_id}/pause")]
async fn pipeline_pause(state: WebData<ServerState>, req: HttpRequest) -> impl Responder {
    let pipeline_id = match parse_pipeline_id_param(&req) {
        Err(e) => {
            return e;
        }
        Ok(pipeline_id) => pipeline_id,
    };

    state
        .runner
        .forward_to_pipeline(pipeline_id, Method::GET, "pause")
        .await
        .unwrap_or_else(|e| http_resp_from_error(&e))
}

/// Request to terminate a running project pipeline.
#[derive(Deserialize, ToSchema)]
pub(self) struct ShutdownPipelineRequest {
    /// Pipeline id to terminate.
    pipeline_id: PipelineId,
}

/// Terminate the execution of a pipeline.
///
/// Sends a termination request to the pipeline process.
/// Returns immediately, without waiting for the pipeline
/// to terminate (which can take several seconds).
///
/// The pipeline is not deleted from the database, but its
/// `killed` flag is set to `true`.
#[utoipa::path(
    request_body = ShutdownPipelineRequest,
    responses(
        (status = OK
            , description = "Pipeline successfully terminated."
            , body = String
            , example = json!("Pipeline successfully terminated")
            , example = json!("Pipeline already shut down")),
        (status = NOT_FOUND
            , description = "Specified `pipeline_id` does not exist in the database."
            , body = ErrorResponse
            , example = json!(ErrorResponse::new("Unknown pipeline id '64'"))),
        (status = INTERNAL_SERVER_ERROR
            , description = "Request failed."
            , body = ErrorResponse
            , example = json!(ErrorResponse::new("Failed to shut down the pipeline; response from pipeline controller: ..."))),
    ),
    tag = "Pipeline"
)]
#[post("/pipelines/shutdown")]
async fn shutdown_pipeline(
    state: WebData<ServerState>,
    request: web::Json<ShutdownPipelineRequest>,
) -> impl Responder {
    state
        .runner
        .shutdown_pipeline(request.pipeline_id)
        .await
        .unwrap_or_else(|e| http_resp_from_error(&e))
}

/// Terminate and delete a pipeline.
///
/// Shut down the pipeline if it is still running and delete it from
/// the database.
#[utoipa::path(
    responses(
        (status = OK
            , description = "Pipeline successfully deleted."
            , body = String
            , example = json!("Pipeline successfully deleted")),
        (status = NOT_FOUND
            , description = "Specified `pipeline_id` does not exist in the database."
            , body = ErrorResponse
            , example = json!(ErrorResponse::new("Unknown pipeline id '64'"))),
        (status = INTERNAL_SERVER_ERROR
            , description = "Request failed."
            , body = ErrorResponse
            , example = json!(ErrorResponse::new("Failed to shut down the pipeline; response from pipeline controller: ..."))),
    ),
    params(
        ("pipeline_id" = i64, Path, description = "Unique pipeline identifier")
    ),
    tag = "Pipeline"
)]
#[delete("/pipelines/{pipeline_id}")]
async fn delete_pipeline(state: WebData<ServerState>, req: HttpRequest) -> impl Responder {
    let pipeline_id = match parse_pipeline_id_param(&req) {
        Err(e) => {
            return e;
        }
        Ok(pipeline_id) => pipeline_id,
    };

    state
        .runner
        .delete_pipeline(pipeline_id)
        .await
        .unwrap_or_else(|e| http_resp_from_error(&e))
}
