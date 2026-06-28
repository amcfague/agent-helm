use agent_helm::tui::{
    ToolLaunchSettings, TuiAction, TuiDetails, TuiInitialState, TuiSessionStatus,
    TuiWorkspaceContext,
};
use agent_helm::{
    config::AppConfig,
    controller::ApplicationController,
    error::Result,
    models::{
        CostFilter, CreateSession, DeleteMode, DeleteSessionRequest, ForkSessionRequest,
        ProjectSpec, SessionDeckStatus, StructuredEvent, now_ts,
    },
    runtime::TmuxRuntime,
};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;
use std::{collections::HashMap, process::ExitCode};

#[cfg(feature = "serve")]
use agent_helm::api;

#[derive(Debug, Parser)]
#[command(
    name = "agent-helm",
    version,
    about = "Local terminal-agent session control"
)]
struct Cli {
    #[arg(long, global = true, default_value = "default")]
    profile: String,
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Serialize)]
struct DiffText {
    text: String,
}

#[derive(Debug, Serialize)]
struct StructuredEventsSnapshot {
    mode: &'static str,
    events: Vec<StructuredEvent>,
}

#[derive(Debug, Subcommand)]
enum Command {
    Init,
    Add {
        path: String,
        #[arg(long)]
        agent: Option<String>,
        #[arg(long = "cmd")]
        command: Option<String>,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        group: Option<String>,
        #[arg(long)]
        worktree: Option<String>,
        #[arg(long)]
        carry_state: bool,
        #[arg(long)]
        sandbox: bool,
        #[arg(long)]
        prompt: Option<String>,
    },
    #[command(alias = "ls")]
    List {
        #[arg(long)]
        group: Option<String>,
        #[arg(long)]
        status: Option<String>,
    },
    Show {
        session: String,
    },
    Status {
        session: String,
    },
    Output {
        session: String,
        #[arg(long, default_value_t = 200)]
        limit: usize,
        #[arg(long)]
        ansi: bool,
    },
    Diff {
        session: String,
    },
    Attach {
        session: String,
    },
    Send {
        session: String,
        text: String,
    },
    Stop {
        session: String,
    },
    Restart {
        session: String,
    },
    Remove {
        session: String,
        #[arg(long)]
        purge: bool,
        #[arg(long)]
        cleanup_worktree: bool,
    },
    Fork {
        session: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        group: Option<String>,
        #[arg(long)]
        worktree: Option<String>,
        #[arg(long)]
        carry_state: bool,
        #[arg(long)]
        no_start: bool,
    },
    Search {
        query: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
    Project {
        #[command(subcommand)]
        command: ProjectCommand,
    },
    Workspace {
        #[command(subcommand)]
        command: WorkspaceCommand,
    },
    Group {
        #[command(subcommand)]
        command: GroupCommand,
    },
    Worktree {
        #[command(subcommand)]
        command: WorktreeCommand,
    },
    Mcp {
        #[command(subcommand)]
        command: AttachmentCommand,
    },
    Skill {
        #[command(subcommand)]
        command: AttachmentCommand,
    },
    Watcher {
        #[command(subcommand)]
        command: WatcherCommand,
    },
    Conductor {
        #[command(subcommand)]
        command: ConductorCommand,
    },
    #[command(alias = "cost")]
    Costs {
        #[command(subcommand)]
        command: Option<CostCommand>,
        #[command(flatten)]
        filter: CostFilterArgs,
    },
    Tui,
    #[cfg(feature = "serve")]
    Serve {
        #[arg(long)]
        listen: Option<String>,
        #[arg(long)]
        token: Option<String>,
        #[arg(long)]
        token_env: Option<String>,
        #[arg(long)]
        read_only: bool,
    },
}

#[derive(Debug, Subcommand)]
enum SessionCommand {
    Start {
        session: String,
    },
    Stop {
        session: String,
    },
    Restart {
        session: String,
    },
    #[command(alias = "ls")]
    List {
        #[arg(long)]
        group: Option<String>,
        #[arg(long)]
        status: Option<String>,
    },
    #[command(alias = "add")]
    Create {
        path: String,
        #[arg(long)]
        agent: Option<String>,
        #[arg(long = "cmd")]
        command: Option<String>,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        group: Option<String>,
        #[arg(long)]
        worktree: Option<String>,
        #[arg(long)]
        carry_state: bool,
        #[arg(long)]
        sandbox: bool,
        #[arg(long)]
        prompt: Option<String>,
    },
    #[command(alias = "rm")]
    Remove {
        session: String,
        #[arg(long)]
        purge: bool,
        #[arg(long)]
        cleanup_worktree: bool,
    },
    Fork {
        session: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        group: Option<String>,
        #[arg(long)]
        worktree: Option<String>,
        #[arg(long)]
        carry_state: bool,
        #[arg(long)]
        no_start: bool,
    },
    Attach {
        session: String,
    },
    Show {
        session: String,
    },
    Status {
        session: String,
    },
    StatusSnapshot {
        session: String,
    },
    Send {
        session: String,
        text: String,
    },
    Output {
        session: String,
        #[arg(long, default_value_t = 200)]
        limit: usize,
        #[arg(long)]
        ansi: bool,
    },
    Events {
        session: String,
        #[arg(long, default_value_t = 0)]
        since: i64,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    StructuredEvents {
        session: String,
        #[arg(long, default_value_t = 0)]
        since: i64,
        #[arg(long, default_value_t = 200)]
        limit: usize,
    },
    RecordEvent {
        session: String,
        #[arg(long, default_value = "agent_state")]
        kind: String,
        #[arg(long)]
        state: Option<String>,
        #[arg(long)]
        source: Option<String>,
        #[arg(long)]
        tool: Option<String>,
    },
    SyncState {
        session: String,
    },
    Materialization {
        session: String,
    },
    Diff {
        session: String,
    },
    Search {
        query: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
}

#[derive(Debug, Subcommand)]
enum ProjectCommand {
    Add {
        path: String,
        #[arg(long)]
        default_branch: Option<String>,
        #[arg(long)]
        trust: bool,
    },
    List,
    Show {
        project: String,
    },
    Trust {
        project: String,
    },
    Untrust {
        project: String,
    },
    Remove {
        project: String,
    },
}

#[derive(Debug, Subcommand)]
enum GroupCommand {
    List,
    Create {
        name: String,
        #[arg(long)]
        parent: Option<String>,
        #[arg(long, visible_alias = "default-working-directory")]
        default_project_path: Option<String>,
    },
    Update {
        group: String,
        #[arg(long, visible_alias = "default-working-directory")]
        default_project_path: Option<String>,
        #[arg(long, visible_alias = "clear-default-working-directory")]
        clear_default_project_path: bool,
        #[arg(long)]
        collapsed: Option<bool>,
    },
    #[command(alias = "remove", alias = "rm")]
    Delete {
        group: String,
        #[arg(long)]
        force: bool,
    },
    Move {
        session: String,
        group: String,
    },
}

#[derive(Debug, Subcommand)]
enum WorktreeCommand {
    Create {
        project: String,
        branch: String,
        #[arg(long)]
        carry_state: bool,
    },
    Show {
        worktree: String,
    },
    Finish {
        worktree: String,
    },
    Cleanup {
        project: String,
    },
    List {
        project: String,
    },
}

#[derive(Debug, Subcommand)]
enum WorkspaceCommand {
    Show { workspace: String },
    List { project: String },
}

#[derive(Debug, Subcommand)]
enum AttachmentCommand {
    List,
    Attach { session: String, id: String },
    AttachProject { project: String, id: String },
    AttachProfile { id: String },
    Detach { id: String },
    Sync,
}

#[derive(Debug, Args)]
struct CostFilterArgs {
    #[arg(long)]
    project: Option<String>,
    #[arg(long)]
    group: Option<String>,
    #[arg(long)]
    session: Option<String>,
    #[arg(long)]
    agent: Option<String>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long)]
    start_at: Option<i64>,
    #[arg(long)]
    end_at: Option<i64>,
}

#[derive(Debug, Subcommand)]
enum CostCommand {
    Events {
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    Record {
        session: String,
        amount_usd: f64,
        #[arg(long)]
        model: Option<String>,
        #[arg(long, default_value_t = 0)]
        input_tokens: i64,
        #[arg(long, default_value_t = 0)]
        output_tokens: i64,
        #[arg(long, default_value_t = 0)]
        total_tokens: i64,
        #[arg(long)]
        source: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum WatcherCommand {
    List {
        #[arg(long)]
        project: Option<String>,
    },
    Events {
        watcher: String,
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    Create {
        name: String,
        #[arg(long, default_value = "manual")]
        adapter: String,
        #[arg(long)]
        project: Option<String>,
        #[arg(long, default_value = "{}")]
        config: String,
    },
    Start {
        name: String,
    },
    Poll {
        watcher: String,
    },
    Ingest {
        watcher: String,
        payload: String,
        #[arg(long, default_value = "external")]
        source: String,
        #[arg(long, default_value = "event")]
        event_type: String,
        #[arg(long, default_value = "not_applicable")]
        signature_status: String,
    },
    PollAll,
    Stop {
        watcher: String,
    },
    #[command(alias = "delete", alias = "rm")]
    Remove {
        watcher: String,
    },
    Test {
        name: String,
    },
}

#[derive(Debug, Subcommand)]
enum ConductorCommand {
    Setup {
        session: String,
    },
    List,
    Start {
        conductor: String,
    },
    Heartbeat {
        conductor: String,
    },
    Stop {
        conductor: String,
    },
    #[command(alias = "delete", alias = "rm")]
    Remove {
        conductor: String,
    },
    Send {
        conductor: String,
        session: String,
        task: String,
    },
    Complete {
        assignment: String,
        #[arg(long, value_enum, default_value = "completed")]
        status: ConductorAssignmentStatusArg,
    },
    Fail {
        assignment: String,
    },
    Cancel {
        assignment: String,
    },
    Assignments {
        conductor: String,
    },
    Status {
        conductor: String,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ConductorAssignmentStatusArg {
    Completed,
    Failed,
    Cancelled,
}

impl ConductorAssignmentStatusArg {
    fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

#[cfg(feature = "serve")]
#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    match run_inner_async().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::from(1)
        }
    }
}

#[cfg(not(feature = "serve"))]
fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    match run_inner() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::from(1)
        }
    }
}

#[cfg(feature = "serve")]
async fn run_inner_async() -> Result<()> {
    let cli = Cli::parse();
    let config = AppConfig::load(&cli.profile)?;
    let controller = ApplicationController::new(config.clone(), TmuxRuntime, false)?;
    match cli.command.unwrap_or(Command::Tui) {
        Command::Serve {
            listen,
            token,
            token_env,
            read_only,
        } => {
            let read_only = read_only || config.web_read_only;
            let controller = ApplicationController::new(config, TmuxRuntime, read_only)?;
            let listen = listen.unwrap_or_else(|| controller.config.web_listen.clone());
            let token_env = token_env.unwrap_or_else(|| controller.config.web_token_env.clone());
            api::serve(controller, listen, token, token_env, read_only).await
        }
        command => run_command(command, cli.json, controller),
    }
}

#[cfg(not(feature = "serve"))]
fn run_inner() -> Result<()> {
    let cli = Cli::parse();
    let config = AppConfig::load(&cli.profile)?;
    let controller = ApplicationController::new(config, TmuxRuntime::default(), false)?;
    run_command(cli.command.unwrap_or(Command::Tui), cli.json, controller)
}

fn run_command(
    command: Command,
    json: bool,
    controller: ApplicationController<TmuxRuntime>,
) -> Result<()> {
    match command {
        Command::Init => {
            controller.init_profile()?;
            println!("{}", controller.config.data_dir.display());
        }
        Command::Add {
            path,
            agent,
            command,
            name,
            group,
            worktree,
            carry_state,
            sandbox,
            prompt,
        } => {
            create_session(
                &controller,
                json,
                CreateSession {
                    path,
                    agent: cli_config_default(agent),
                    command: command.unwrap_or_default(),
                    name: name.unwrap_or_default(),
                    group_name: cli_config_default(group),
                    worktree,
                    carry_state,
                    sandbox,
                    prompt,
                    parent_session_id: None,
                },
            )?;
        }
        Command::List { group, status } => print_session_list(&controller, json, group, status)?,
        Command::Show { session } => print_record(&controller.get_session(&session)?, json)?,
        Command::Status { session } => {
            print_json_or_debug(&controller.status_snapshot(&session)?, json)?
        }
        Command::Output {
            session,
            limit,
            ansi,
        } => print_output_page(controller.output(&session, limit, ansi)?, json)?,
        Command::Diff { session } => print_diff(controller.diff(&session)?, json)?,
        Command::Attach { session } => controller.attach(&session)?,
        Command::Send { session, text } => controller.send(&session, &text)?,
        Command::Stop { session } => print_record(&controller.stop(&session)?, json)?,
        Command::Restart { session } => print_record(&controller.restart(&session)?, json)?,
        Command::Remove {
            session,
            purge,
            cleanup_worktree,
        } => {
            let mode = if purge {
                DeleteMode::Purge
            } else if cleanup_worktree {
                DeleteMode::CleanupWorktree
            } else {
                DeleteMode::MetadataOnly
            };
            let result = controller.delete_session(DeleteSessionRequest {
                session_id: session,
                mode,
                reason: "cli remove".to_string(),
            })?;
            print_json_or_debug(&result, json)?;
        }
        Command::Fork {
            session,
            name,
            group,
            worktree,
            carry_state,
            no_start,
        } => {
            let result = controller.fork_session(ForkSessionRequest {
                parent_session_id: session,
                name,
                group_name: group,
                worktree_branch: worktree,
                carry_state,
                start_immediately: !no_start,
            })?;
            print_json_or_debug(&result, json)?;
        }
        Command::Search { query, limit } => print_search(&controller, &query, limit, json)?,
        Command::Session { command } => run_session_command(command, json, &controller)?,
        Command::Project { command } => match command {
            ProjectCommand::Add {
                path,
                default_branch,
                trust,
            } => {
                let project = controller.register_project(ProjectSpec {
                    profile: controller.config.profile.clone(),
                    root_path: path,
                    default_branch: default_branch.unwrap_or_default(),
                    trusted: trust,
                })?;
                print_json_or_debug(&project, json)?;
            }
            ProjectCommand::List => print_json_or_debug(&controller.list_projects()?, json)?,
            ProjectCommand::Show { project } => {
                print_json_or_debug(&controller.get_project(&project)?, json)?
            }
            ProjectCommand::Trust { project } => {
                print_json_or_debug(&controller.set_project_trust(&project, true)?, json)?
            }
            ProjectCommand::Untrust { project } => {
                print_json_or_debug(&controller.set_project_trust(&project, false)?, json)?
            }
            ProjectCommand::Remove { project } => controller.remove_project(&project)?,
        },
        Command::Workspace { command } => match command {
            WorkspaceCommand::Show { workspace } => {
                print_json_or_debug(&controller.get_workspace(&workspace)?, json)?
            }
            WorkspaceCommand::List { project } => {
                print_json_or_debug(&controller.list_workspaces(&project)?, json)?
            }
        },
        Command::Group { command } => match command {
            GroupCommand::List => print_json_or_debug(&controller.list_groups()?, json)?,
            GroupCommand::Create {
                name,
                parent,
                default_project_path,
            } => print_json_or_debug(
                &controller.create_group(name, parent, default_project_path)?,
                json,
            )?,
            GroupCommand::Update {
                group,
                default_project_path,
                clear_default_project_path,
                collapsed,
            } => print_json_or_debug(
                &controller.update_group(
                    &group,
                    default_project_path,
                    clear_default_project_path,
                    collapsed,
                )?,
                json,
            )?,
            GroupCommand::Delete { group, force } => {
                controller.delete_group(&group, force)?;
            }
            GroupCommand::Move { session, group } => {
                print_record(&controller.move_session_to_group(&session, group)?, json)?
            }
        },
        Command::Worktree { command } => match command {
            WorktreeCommand::Create {
                project,
                branch,
                carry_state,
            } => print_json_or_debug(
                &controller.create_project_worktree(&project, &branch, carry_state)?,
                json,
            )?,
            WorktreeCommand::Show { worktree } => {
                print_json_or_debug(&controller.get_worktree(&worktree)?, json)?
            }
            WorktreeCommand::Finish { worktree } => {
                print_json_or_debug(&controller.finish_worktree(&worktree)?, json)?
            }
            WorktreeCommand::Cleanup { project } => {
                print_json_or_debug(&controller.cleanup_worktrees(&project)?, json)?
            }
            WorktreeCommand::List { project } => {
                print_json_or_debug(&controller.list_worktrees(&project)?, json)?
            }
        },
        Command::Mcp { command } => match command {
            AttachmentCommand::List => {
                print_json_or_debug(&controller.list_mcp_attachments()?, json)?
            }
            AttachmentCommand::Attach { session, id } => {
                print_json_or_debug(&controller.attach_mcp(&session, id)?, json)?
            }
            AttachmentCommand::AttachProject { project, id } => {
                print_json_or_debug(&controller.attach_project_mcp(&project, id)?, json)?
            }
            AttachmentCommand::AttachProfile { id } => {
                print_json_or_debug(&controller.attach_profile_mcp(id)?, json)?
            }
            AttachmentCommand::Detach { id } => {
                print_json_or_debug(&controller.detach_mcp(&id)?, json)?
            }
            AttachmentCommand::Sync => print_json_or_debug(&controller.sync_mcp()?, json)?,
        },
        Command::Skill { command } => match command {
            AttachmentCommand::List => {
                print_json_or_debug(&controller.list_skill_attachments()?, json)?
            }
            AttachmentCommand::Attach { session, id } => {
                print_json_or_debug(&controller.attach_skill(&session, id)?, json)?
            }
            AttachmentCommand::AttachProject { project, id } => {
                print_json_or_debug(&controller.attach_project_skill(&project, id)?, json)?
            }
            AttachmentCommand::AttachProfile { id } => {
                print_json_or_debug(&controller.attach_profile_skill(id)?, json)?
            }
            AttachmentCommand::Detach { id } => {
                print_json_or_debug(&controller.detach_skill(&id)?, json)?
            }
            AttachmentCommand::Sync => print_json_or_debug(&controller.sync_skills()?, json)?,
        },
        Command::Watcher { command } => match command {
            WatcherCommand::List { project } => {
                if let Some(project) = project {
                    print_json_or_debug(&controller.list_project_watchers(&project)?, json)?
                } else {
                    print_json_or_debug(&controller.list_watchers()?, json)?
                }
            }
            WatcherCommand::Events {
                watcher,
                offset,
                limit,
            } => print_json_or_debug(
                &controller.list_watcher_events(&watcher, offset, limit)?,
                json,
            )?,
            WatcherCommand::Create {
                name,
                adapter,
                project,
                config,
            } => print_json_or_debug(
                &controller.create_watcher_config(name, adapter, project, config)?,
                json,
            )?,
            WatcherCommand::Start { name } => {
                print_json_or_debug(&controller.start_watcher(&name)?, json)?
            }
            WatcherCommand::Poll { watcher } => {
                print_json_or_debug(&controller.poll_watcher(&watcher)?, json)?
            }
            WatcherCommand::Ingest {
                watcher,
                payload,
                source,
                event_type,
                signature_status,
            } => print_json_or_debug(
                &controller.ingest_watcher_event(
                    &watcher,
                    source,
                    event_type,
                    payload,
                    signature_status,
                )?,
                json,
            )?,
            WatcherCommand::PollAll => {
                print_json_or_debug(&controller.poll_running_watchers()?, json)?
            }
            WatcherCommand::Test { name } => {
                print_json_or_debug(&controller.test_watcher(&name)?, json)?
            }
            WatcherCommand::Stop { watcher } => {
                print_json_or_debug(&controller.stop_watcher(&watcher)?, json)?
            }
            WatcherCommand::Remove { watcher } => {
                controller.delete_watcher(&watcher)?;
            }
        },
        Command::Conductor { command } => match command {
            ConductorCommand::Setup { session } => {
                print_json_or_debug(&controller.create_conductor(session)?, json)?
            }
            ConductorCommand::List => print_json_or_debug(&controller.list_conductors()?, json)?,
            ConductorCommand::Send {
                conductor,
                session,
                task,
            } => print_json_or_debug(&controller.send_conductor(&conductor, session, task)?, json)?,
            ConductorCommand::Complete { assignment, status } => print_json_or_debug(
                &controller.complete_conductor_assignment(&assignment, status.as_str())?,
                json,
            )?,
            ConductorCommand::Fail { assignment } => print_json_or_debug(
                &controller.complete_conductor_assignment(&assignment, "failed")?,
                json,
            )?,
            ConductorCommand::Cancel { assignment } => print_json_or_debug(
                &controller.complete_conductor_assignment(&assignment, "cancelled")?,
                json,
            )?,
            ConductorCommand::Assignments { conductor } => {
                print_json_or_debug(&controller.list_conductor_assignments(&conductor)?, json)?
            }
            ConductorCommand::Status { conductor } => {
                print_json_or_debug(&controller.get_conductor(&conductor)?, json)?;
            }
            ConductorCommand::Start { conductor } => {
                print_json_or_debug(&controller.start_conductor(&conductor)?, json)?
            }
            ConductorCommand::Heartbeat { conductor } => {
                print_json_or_debug(&controller.heartbeat_conductor(&conductor)?, json)?
            }
            ConductorCommand::Stop { conductor } => {
                print_json_or_debug(&controller.stop_conductor(&conductor)?, json)?
            }
            ConductorCommand::Remove { conductor } => {
                controller.delete_conductor(&conductor)?;
            }
        },
        Command::Costs { command, filter } => match command {
            Some(CostCommand::Record {
                session,
                amount_usd,
                model,
                input_tokens,
                output_tokens,
                total_tokens,
                source,
            }) => {
                let mut payload = serde_json::Map::new();
                if let Some(model) = model {
                    payload.insert("model".to_string(), serde_json::json!(model));
                }
                if let Some(source) = source {
                    payload.insert("source".to_string(), serde_json::json!(source));
                }
                payload.insert("input_tokens".to_string(), serde_json::json!(input_tokens));
                payload.insert(
                    "output_tokens".to_string(),
                    serde_json::json!(output_tokens),
                );
                payload.insert("total_tokens".to_string(), serde_json::json!(total_tokens));
                print_json_or_debug(
                    &controller.record_cost(
                        &session,
                        amount_usd,
                        serde_json::Value::Object(payload),
                    )?,
                    json,
                )?;
            }
            Some(CostCommand::Events { offset, limit }) => {
                let now = now_ts();
                let events = controller.cost_events(
                    CostFilter {
                        profile: controller.config.profile.clone(),
                        project_id: filter.project,
                        group_name: filter.group,
                        session_id: filter.session,
                        agent: filter.agent,
                        model: filter.model,
                        start_at: filter.start_at.unwrap_or(0),
                        end_at: filter.end_at.unwrap_or(now),
                    },
                    offset,
                    limit,
                )?;
                print_json_or_debug(&events, json)?;
            }
            None => {
                let now = now_ts();
                let summary = controller.cost_summary(CostFilter {
                    profile: controller.config.profile.clone(),
                    project_id: filter.project,
                    group_name: filter.group,
                    session_id: filter.session,
                    agent: filter.agent,
                    model: filter.model,
                    start_at: filter.start_at.unwrap_or(0),
                    end_at: filter.end_at.unwrap_or(now),
                })?;
                print_json_or_debug(&summary, json)?;
            }
        },
        Command::Tui => {
            let sessions = controller.list_sessions()?;
            let groups = controller.list_groups()?;
            let status_controller = controller.clone();
            let details_controller = controller.clone();
            let mut action_controller = controller.clone();
            let headroom_metrics = agent_helm::tui::HeadroomMetrics::load(
                &controller.config.headroom_proxy_savings_path,
            );
            let agent_choices = controller.config.installed_tool_names();
            let tool_settings = controller
                .config
                .tools
                .iter()
                .map(|(name, tool)| ToolLaunchSettings::from_profile(name.clone(), tool))
                .collect::<Vec<_>>();
            agent_helm::tui::run_tui(
                TuiInitialState {
                    sessions,
                    groups,
                    default_agent: controller.config.default_agent.clone(),
                    headroom_metrics,
                    agent_choices,
                    tool_settings,
                },
                move |session_ids| {
                    let mut statuses = Vec::new();
                    for session_id in session_ids {
                        if let Ok(snapshot) = status_controller.status_snapshot(session_id) {
                            statuses.push((
                                session_id.clone(),
                                TuiSessionStatus {
                                    deck_status: snapshot.deck_status,
                                    activity: snapshot.activity,
                                },
                            ));
                        }
                    }
                    Ok(statuses)
                },
                move |session_id| {
                    let snapshot = details_controller.status_snapshot(session_id).ok();
                    let deck_status = snapshot
                        .as_ref()
                        .map(|snapshot| snapshot.deck_status)
                        .unwrap_or(SessionDeckStatus::Errored);
                    let workspace = snapshot.as_ref().and_then(|snapshot| {
                        let workspace = details_controller
                            .get_workspace(&snapshot.session.workspace_id)
                            .ok()?;
                        let worktree = snapshot
                            .session
                            .worktree_id
                            .as_deref()
                            .and_then(|id| details_controller.get_worktree(id).ok());
                        Some(TuiWorkspaceContext {
                            workspace_id: workspace.id,
                            workspace_path: workspace.path,
                            worktree_id: worktree.as_ref().map(|worktree| worktree.id.clone()),
                            worktree_path: worktree.as_ref().map(|worktree| worktree.path.clone()),
                            worktree_branch: worktree
                                .as_ref()
                                .map(|worktree| worktree.branch.clone()),
                        })
                    });
                    let output = if matches!(
                        deck_status,
                        SessionDeckStatus::Running | SessionDeckStatus::Starting
                    ) {
                        String::new()
                    } else {
                        details_controller
                            .output(session_id, 200, false)
                            .map(|page| page.text)
                            .unwrap_or_else(|err| format!("output failed: {err}"))
                    };
                    Ok(TuiDetails {
                        deck_status,
                        activity: snapshot.and_then(|snapshot| snapshot.activity),
                        output,
                        workspace,
                    })
                },
                move |action| {
                    match action {
                        TuiAction::Create(request) => {
                            action_controller.create_session(request)?;
                        }
                        TuiAction::Attach(session) => {
                            action_controller.attach(&session)?;
                        }
                        TuiAction::Stop(session) => {
                            action_controller.stop(&session)?;
                        }
                        TuiAction::Restart(session) => {
                            action_controller.restart(&session)?;
                        }
                        TuiAction::Fork(request) => {
                            action_controller.fork_session(request)?;
                        }
                        TuiAction::Search { query, limit } => {
                            let response = action_controller.search_sessions(&query, limit)?;
                            let mut sessions = Vec::new();
                            for result in response.results {
                                let session_id = result.session_id;
                                if sessions.iter().any(
                                    |session: &agent_helm::models::SessionRecord| {
                                        session.id.as_str() == session_id.as_str()
                                    },
                                ) {
                                    continue;
                                }
                                if let Ok(session) = action_controller.get_session(&session_id) {
                                    sessions.push(session);
                                }
                            }
                            return Ok(sessions);
                        }
                        TuiAction::CreateGroup {
                            name,
                            default_project_path,
                        } => {
                            action_controller.create_group(
                                name,
                                None,
                                Some(default_project_path),
                            )?;
                        }
                        TuiAction::Remove { session_id, mode } => {
                            action_controller.delete_session(DeleteSessionRequest {
                                session_id,
                                mode,
                                reason: "tui remove".to_string(),
                            })?;
                        }
                        TuiAction::Refresh => {}
                        TuiAction::SetGroupCollapsed {
                            group_name,
                            collapsed,
                        } => {
                            action_controller.update_group(
                                &group_name,
                                None,
                                false,
                                Some(collapsed),
                            )?;
                        }
                        TuiAction::MoveToGroup {
                            session_id,
                            group_name,
                        } => {
                            action_controller.move_session_to_group(&session_id, group_name)?;
                        }
                        TuiAction::SaveToolSettings(settings) => {
                            let tools = settings
                                .into_iter()
                                .map(|setting| (setting.name.clone(), setting.to_profile()))
                                .collect::<HashMap<_, _>>();
                            action_controller.config.save_tool_settings(tools)?;
                        }
                    }
                    action_controller.list_sessions()
                },
            )?;
        }
        #[cfg(feature = "serve")]
        Command::Serve { .. } => unreachable!("serve is handled before sync commands"),
    }
    Ok(())
}

fn create_session(
    controller: &ApplicationController<TmuxRuntime>,
    json: bool,
    request: CreateSession,
) -> Result<()> {
    let session = controller.create_session(request)?;
    print_record(&session, json)
}

fn print_session_list(
    controller: &ApplicationController<TmuxRuntime>,
    json: bool,
    group: Option<String>,
    status: Option<String>,
) -> Result<()> {
    let status = status.as_deref().map(normalize_status_filter);
    let mut sessions = Vec::new();
    for session in controller.list_sessions()? {
        if group
            .as_deref()
            .is_some_and(|group| session.group_name != group)
        {
            continue;
        }
        if let Some(status) = status.as_deref()
            && !cli_list_status_matches(controller, &session, status)?
        {
            continue;
        }
        sessions.push(session);
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&sessions)?);
    } else {
        for session in sessions {
            println!(
                "{}\t{}\t{}\t{}\t{}",
                session.id,
                session.status.as_str(),
                session.group_name,
                session.agent,
                session.name
            );
        }
    }
    Ok(())
}

fn run_session_command(
    command: SessionCommand,
    json: bool,
    controller: &ApplicationController<TmuxRuntime>,
) -> Result<()> {
    match command {
        SessionCommand::Start { session } | SessionCommand::Restart { session } => {
            print_record(&controller.restart(&session)?, json)?
        }
        SessionCommand::Stop { session } => print_record(&controller.stop(&session)?, json)?,
        SessionCommand::List { group, status } => {
            print_session_list(controller, json, group, status)?
        }
        SessionCommand::Create {
            path,
            agent,
            command,
            name,
            group,
            worktree,
            carry_state,
            sandbox,
            prompt,
        } => create_session(
            controller,
            json,
            CreateSession {
                path,
                agent: cli_config_default(agent),
                command: command.unwrap_or_default(),
                name: name.unwrap_or_default(),
                group_name: cli_config_default(group),
                worktree,
                carry_state,
                sandbox,
                prompt,
                parent_session_id: None,
            },
        )?,
        SessionCommand::Remove {
            session,
            purge,
            cleanup_worktree,
        } => {
            let mode = if purge {
                DeleteMode::Purge
            } else if cleanup_worktree {
                DeleteMode::CleanupWorktree
            } else {
                DeleteMode::MetadataOnly
            };
            let result = controller.delete_session(DeleteSessionRequest {
                session_id: session,
                mode,
                reason: "cli session remove".to_string(),
            })?;
            print_json_or_debug(&result, json)?;
        }
        SessionCommand::Fork {
            session,
            name,
            group,
            worktree,
            carry_state,
            no_start,
        } => {
            let result = controller.fork_session(ForkSessionRequest {
                parent_session_id: session,
                name,
                group_name: group,
                worktree_branch: worktree,
                carry_state,
                start_immediately: !no_start,
            })?;
            print_json_or_debug(&result, json)?;
        }
        SessionCommand::Attach { session } => controller.attach(&session)?,
        SessionCommand::Show { session } => print_record(&controller.get_session(&session)?, json)?,
        SessionCommand::Status { session } => {
            print_json_or_debug(&controller.status_snapshot(&session)?, json)?
        }
        SessionCommand::StatusSnapshot { session } => {
            print_json_or_debug(&controller.status_snapshot(&session)?, json)?
        }
        SessionCommand::Send { session, text } => controller.send(&session, &text)?,
        SessionCommand::Output {
            session,
            limit,
            ansi,
        } => print_output_page(controller.output(&session, limit, ansi)?, json)?,
        SessionCommand::Events {
            session,
            since,
            limit,
        } => print_json_or_debug(&controller.events(&session, since, limit)?, json)?,
        SessionCommand::StructuredEvents {
            session,
            since,
            limit,
        } => print_json_or_debug(
            &StructuredEventsSnapshot {
                mode: "snapshot",
                events: controller.structured_events(&session, since, limit)?,
            },
            json,
        )?,
        SessionCommand::RecordEvent {
            session,
            kind,
            state,
            source,
            tool,
        } => {
            let mut payload = serde_json::Map::new();
            if let Some(state) = state {
                payload.insert("state".to_string(), serde_json::json!(state));
            }
            if let Some(source) = source {
                payload.insert("source".to_string(), serde_json::json!(source));
            } else {
                payload.insert("source".to_string(), serde_json::json!("cli"));
            }
            if let Some(tool) = tool {
                payload.insert("tool".to_string(), serde_json::json!(tool));
            }
            print_json_or_debug(
                &controller.record_session_event(
                    &session,
                    &kind,
                    serde_json::Value::Object(payload),
                )?,
                json,
            )?;
        }
        SessionCommand::SyncState { session } => {
            print_json_or_debug(&controller.sync_agent_state(&session)?, json)?
        }
        SessionCommand::Materialization { session } => {
            print_json_or_debug(&controller.session_materialization(&session)?, json)?
        }
        SessionCommand::Diff { session } => print_diff(controller.diff(&session)?, json)?,
        SessionCommand::Search { query, limit } => print_search(controller, &query, limit, json)?,
    }
    Ok(())
}

fn cli_config_default(value: Option<String>) -> String {
    value.unwrap_or_default()
}

fn print_output_page(page: agent_helm::models::OutputPage, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(&page)?);
    } else {
        print!("{}", page.text);
    }
    Ok(())
}

fn print_diff(text: String, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(&DiffText { text })?);
    } else {
        print!("{text}");
    }
    Ok(())
}

fn print_search(
    controller: &ApplicationController<TmuxRuntime>,
    query: &str,
    limit: usize,
    json: bool,
) -> Result<()> {
    let response = controller.search_sessions(query, limit)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&response)?);
    } else {
        for result in response.results {
            println!(
                "{}\t{}\t{}\t{}\t{}",
                result.session_id, result.group_name, result.agent, result.source, result.snippet
            );
        }
    }
    Ok(())
}

fn normalize_status_filter(status: &str) -> String {
    status.trim().to_ascii_lowercase()
}

fn cli_list_status_matches(
    controller: &ApplicationController<TmuxRuntime>,
    session: &agent_helm::models::SessionRecord,
    status: &str,
) -> Result<bool> {
    if session_status_matches_filter(session.status.as_str(), None, status) {
        return Ok(true);
    }
    let snapshot = controller.status_snapshot(&session.id)?;
    Ok(session_status_matches_filter(
        session.status.as_str(),
        Some(snapshot.deck_status),
        status,
    ))
}

fn session_status_matches_filter(
    lifecycle_status: &str,
    deck_status: Option<SessionDeckStatus>,
    status: &str,
) -> bool {
    let status = normalize_status_filter(status);
    lifecycle_status == status
        || deck_status.is_some_and(|deck_status| deck_status.as_str() == status)
}

fn print_record(record: &agent_helm::models::SessionRecord, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(record)?);
    } else {
        println!(
            "{}\t{}\t{}\t{}",
            record.id,
            record.status.as_str(),
            record.agent,
            record.name
        );
    }
    Ok(())
}

fn print_json_or_debug<T>(value: &T, json: bool) -> Result<()>
where
    T: Serialize + std::fmt::Debug,
{
    if json {
        println!("{}", serde_json::to_string_pretty(value)?);
    } else {
        println!("{value:?}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_list_alias_parses() {
        let cli = Cli::try_parse_from([
            "agent-helm",
            "session",
            "ls",
            "--group",
            "work/api",
            "--status",
            "waiting",
        ])
        .unwrap();

        match cli.command {
            Some(Command::Session {
                command: SessionCommand::List { group, status },
            }) => {
                assert_eq!(group.as_deref(), Some("work/api"));
                assert_eq!(status.as_deref(), Some("waiting"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn list_status_filter_matches_lifecycle_status() {
        assert!(session_status_matches_filter("running", None, "running"));
        assert!(session_status_matches_filter("running", None, " Running "));
    }

    #[test]
    fn list_status_filter_matches_deck_status() {
        assert!(session_status_matches_filter(
            "running",
            Some(SessionDeckStatus::Waiting),
            "waiting",
        ));
        assert!(session_status_matches_filter(
            "running",
            Some(SessionDeckStatus::Queued),
            "queued",
        ));
    }

    #[test]
    fn list_status_filter_rejects_unmatched_deck_status() {
        assert!(!session_status_matches_filter(
            "running",
            Some(SessionDeckStatus::Idle),
            "waiting",
        ));
    }
}
