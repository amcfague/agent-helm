use agent_helm::tui::{TuiAction, TuiDetails};
use agent_helm::{
    config::AppConfig,
    controller::ApplicationController,
    error::Result,
    models::{
        ArchiveSessionRequest, CostFilter, CreateSession, DeleteMode, DeleteSessionRequest,
        ForkSessionRequest, ProjectSpec, now_ts,
    },
    runtime::TmuxRuntime,
};
use clap::{Parser, Subcommand};
use serde::Serialize;
use std::process::ExitCode;

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

#[derive(Debug, Subcommand)]
enum Command {
    Init,
    Add {
        path: String,
        #[arg(long, default_value = "shell")]
        agent: String,
        #[arg(long = "cmd")]
        command: Option<String>,
        #[arg(long)]
        name: Option<String>,
        #[arg(long, default_value = "default")]
        group: String,
        #[arg(long)]
        worktree: Option<String>,
        #[arg(long)]
        sandbox: bool,
        #[arg(long)]
        prompt: Option<String>,
    },
    #[command(alias = "ls")]
    List {
        #[arg(long)]
        all: bool,
        #[arg(long)]
        archived: bool,
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
    Archive {
        session: String,
        #[arg(long, default_value = "cli")]
        by: String,
        #[arg(long, default_value = "")]
        reason: String,
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
    },
    Project {
        #[command(subcommand)]
        command: ProjectCommand,
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
    Costs,
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
enum ProjectCommand {
    Add {
        path: String,
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
    Move { session: String, group: String },
}

#[derive(Debug, Subcommand)]
enum WorktreeCommand {
    Create { project: String, branch: String },
    Finish { worktree: String },
    Cleanup { project: String },
    List { project: String },
}

#[derive(Debug, Subcommand)]
enum AttachmentCommand {
    List,
    Attach { session: String, id: String },
    Detach { id: String },
    Sync,
}

#[derive(Debug, Subcommand)]
enum WatcherCommand {
    List,
    Start { name: String },
    Stop { watcher: String },
    Test { name: String },
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
    Stop {
        conductor: String,
    },
    Send {
        conductor: String,
        session: String,
        task: String,
    },
    Status {
        conductor: String,
    },
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
            sandbox,
            prompt,
        } => {
            let session = controller.create_session(CreateSession {
                path,
                agent,
                command: command.unwrap_or_default(),
                name: name.unwrap_or_default(),
                group_name: group,
                worktree,
                carry_state: false,
                sandbox,
                prompt,
                parent_session_id: None,
            })?;
            print_record(&session, json)?;
        }
        Command::List {
            all,
            archived,
            group,
            status,
        } => {
            let sessions = controller
                .list_sessions(all || archived)?
                .into_iter()
                .filter(|session| !archived || session.archived)
                .filter(|session| {
                    group
                        .as_deref()
                        .is_none_or(|group| session.group_name == group)
                })
                .filter(|session| {
                    status
                        .as_deref()
                        .is_none_or(|status| session.status.as_str() == status)
                })
                .collect::<Vec<_>>();
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
        }
        Command::Show { session } => print_record(&controller.get_session(&session)?, json)?,
        Command::Status { session } => print_record(&controller.status(&session)?, json)?,
        Command::Output {
            session,
            limit,
            ansi,
        } => print!("{}", controller.output(&session, limit, ansi)?.text),
        Command::Diff { session } => print!("{}", controller.diff(&session)?),
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
        Command::Archive {
            session,
            by,
            reason,
        } => {
            let result = controller.archive_session(ArchiveSessionRequest {
                session_id: session,
                archived_by: by,
                reason,
                stop_if_running: true,
            })?;
            print_json_or_debug(&result, json)?;
        }
        Command::Fork {
            session,
            name,
            group,
            worktree,
            carry_state,
        } => {
            let result = controller.fork_session(ForkSessionRequest {
                parent_session_id: session,
                name,
                group_name: group,
                worktree_branch: worktree,
                carry_state,
                start_immediately: true,
            })?;
            print_json_or_debug(&result, json)?;
        }
        Command::Project { command } => match command {
            ProjectCommand::Add { path, trust } => {
                let project = controller.register_project(ProjectSpec {
                    profile: controller.config.profile.clone(),
                    root_path: path,
                    default_branch: String::new(),
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
        Command::Group { command } => match command {
            GroupCommand::List => print_json_or_debug(&controller.list_groups()?, json)?,
            GroupCommand::Move { session, group } => {
                print_record(&controller.move_session_to_group(&session, group)?, json)?
            }
        },
        Command::Worktree { command } => match command {
            WorktreeCommand::Create { project, branch } => print_json_or_debug(
                &controller.create_project_worktree(&project, &branch)?,
                json,
            )?,
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
            AttachmentCommand::Detach { id } => {
                print_json_or_debug(&controller.detach_skill(&id)?, json)?
            }
            AttachmentCommand::Sync => print_json_or_debug(&controller.sync_skills()?, json)?,
        },
        Command::Watcher { command } => match command {
            WatcherCommand::List => print_json_or_debug(&controller.list_watchers()?, json)?,
            WatcherCommand::Start { name } => {
                print_json_or_debug(&controller.start_watcher(&name)?, json)?
            }
            WatcherCommand::Test { name } => {
                print_json_or_debug(&controller.test_watcher(&name)?, json)?
            }
            WatcherCommand::Stop { watcher } => {
                print_json_or_debug(&controller.stop_watcher(&watcher)?, json)?
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
            ConductorCommand::Status { conductor } => {
                print_json_or_debug(&controller.get_conductor(&conductor)?, json)?;
            }
            ConductorCommand::Start { conductor } => {
                print_json_or_debug(&controller.start_conductor(&conductor)?, json)?
            }
            ConductorCommand::Stop { conductor } => {
                print_json_or_debug(&controller.stop_conductor(&conductor)?, json)?
            }
        },
        Command::Costs => {
            let now = now_ts();
            let summary = controller.cost_summary(CostFilter {
                profile: controller.config.profile.clone(),
                project_id: None,
                group_name: None,
                session_id: None,
                agent: None,
                model: None,
                start_at: 0,
                end_at: now,
                include_archived: true,
            })?;
            print_json_or_debug(&summary, json)?;
        }
        Command::Tui => {
            let sessions = controller.list_sessions(false)?;
            let details_controller = controller.clone();
            let action_controller = controller.clone();
            agent_helm::tui::run_tui(
                sessions,
                move |session_id| {
                    Ok(TuiDetails {
                        output: details_controller
                            .output(session_id, 200, false)
                            .map(|page| page.text)
                            .unwrap_or_else(|err| format!("output failed: {err}")),
                        diff: details_controller
                            .diff(session_id)
                            .unwrap_or_else(|err| format!("diff failed: {err}")),
                        structured: match details_controller.structured_events(session_id, 0, 50) {
                            Ok(events) if events.is_empty() => String::new(),
                            Ok(events) => serde_json::to_string_pretty(&events)?,
                            Err(err) => format!("structured events failed: {err}"),
                        },
                    })
                },
                move |action| {
                    match action {
                        TuiAction::Create(request) => {
                            action_controller.create_session(request)?;
                        }
                        TuiAction::Stop(session) => {
                            action_controller.stop(&session)?;
                        }
                        TuiAction::Restart(session) => {
                            action_controller.restart(&session)?;
                        }
                        TuiAction::Fork(session) => {
                            action_controller.fork_session(ForkSessionRequest {
                                parent_session_id: session,
                                name: None,
                                group_name: None,
                                worktree_branch: None,
                                carry_state: false,
                                start_immediately: true,
                            })?;
                        }
                        TuiAction::Send { session_id, text } => {
                            action_controller.send(&session_id, &text)?;
                        }
                    }
                    action_controller.list_sessions(false)
                },
            )?;
        }
        #[cfg(feature = "serve")]
        Command::Serve { .. } => unreachable!("serve is handled before sync commands"),
    }
    Ok(())
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
