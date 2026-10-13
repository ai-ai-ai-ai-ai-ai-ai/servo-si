mod greetings;

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use codex::config::{ConfigBuilder, ConfigOverrides};
use codex_core_api::{
    Arg0DispatchPaths, AskForApproval, AuthManager, CodexAppsToolsCache,
    CodexHomeUserInstructionsProvider, Config, ConfigLoadOptions, EnvironmentManager,
    ExecServerRuntimeOptions, Feature, PermissionProfile, SessionSource, StartIfIdleSubmission,
    StartThreadOptions, ThreadManager, TurnInputRequest, UserInput, bootstrap_auth_config,
    build_models_manager, empty_extension_registry, init_state_db,
    load_config_toml_with_layer_stack, local_agent_graph_store_from_state_db,
    passthrough_image_store, resolve_installation_id, thread_store_from_config,
};
use thiserror::Error;

use greetings::Greetings;

const PROMPT: &str = "Create exactly two subagents using spawn_agent. Give each this task: \
    'Reply with exactly Hi! and do nothing else. Do not create subagents or use tools.' \
    Start both before waiting. Wait until both have completed, then finish. \
    Do not close the subagents, create additional agents, or do any other work.";

#[derive(Debug, Parser)]
#[command(about = "Start a Codex parent agent and ask two subagents to say hi.")]
struct Args {
    /// Override the model from your Codex configuration.
    #[arg(long)]
    model: Option<String>,

    /// Maximum time to wait for the parent and its two subagents.
    #[arg(long, default_value_t = 120, value_parser = clap::value_parser!(u64).range(1..))]
    timeout_secs: u64,
}

#[derive(Debug, Error)]
enum LaceError {
    #[error("{step}: {message}")]
    Codex { step: &'static str, message: String },
    #[error("{0}")]
    Greeting(String),
    #[error("timed out waiting for the agents")]
    Timeout,
    #[error("interrupted")]
    Interrupted,
}

fn failure<E: std::fmt::Display>(step: &'static str) -> impl FnOnce(E) -> LaceError {
    move |error| LaceError::Codex {
        step,
        message: error.to_string(),
    }
}

fn main() -> ExitCode {
    // Codex's filesystem and sandbox helpers re-enter this binary before CLI parsing.
    let helpers = codex_arg0::arg0_dispatch();
    let paths = helpers
        .as_ref()
        .map(|guard| guard.paths().clone())
        .unwrap_or_default();
    match run_main(Args::parse(), paths) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("lace: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run_main(args: Args, paths: Arg0DispatchPaths) -> Result<(), LaceError> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        // Match Codex's worker stack size for its deeply nested async operations.
        .thread_stack_size(16 * 1024 * 1024)
        .enable_all()
        .build()
        .map_err(failure("create async runtime"))?;
    runtime.block_on(async move {
        tokio::spawn(run(args, paths))
            .await
            .map_err(failure("run Codex task"))?
    })
}

async fn load_config(args: &Args, paths: Arg0DispatchPaths) -> Result<Config, LaceError> {
    let mut config = ConfigBuilder::default()
        .harness_overrides(ConfigOverrides {
            model: args.model.clone(),
            codex_self_exe: Some(
                std::env::current_exe().map_err(failure("locate lace executable"))?,
            ),
            codex_linux_sandbox_exe: paths.codex_linux_sandbox_exe,
            permission_profile: Some(PermissionProfile::read_only()),
            approval_policy: Some(AskForApproval::Never),
            ..Default::default()
        })
        .build()
        .await
        .map_err(failure("load Codex configuration"))?;

    // Codex's development version otherwise replaces the configured keyring with file auth.
    let bootstrap = load_config_toml_with_layer_stack(
        config.codex_home.as_path(),
        Some(&config.cwd),
        Vec::new(),
        ConfigLoadOptions::default(),
    )
    .await
    .map_err(failure("load authentication configuration"))?;
    let auth = bootstrap_auth_config(config.codex_home.as_path(), &bootstrap)
        .map_err(failure("resolve authentication configuration"))?;
    config.cli_auth_credentials_store_mode = auth.auth_credentials_store_mode;
    config.application_auth_route_config = Some(auth.auth_route_config);
    config
        .features
        .set_enabled(
            Feature::SecretAuthStorage,
            auth.keyring_backend_kind == codex_core_api::AuthKeyringBackendKind::Secrets,
        )
        .map_err(failure("configure authentication"))?;

    config.agents_enabled = true;
    config.agent_max_threads = Some(2);
    config.agent_max_depth = 1;
    // V2 counts the parent as one of its concurrent threads.
    config.multi_agent_v2.max_concurrent_threads_per_session = 3;
    config
        .features
        .enable(Feature::Collab)
        .map_err(failure("enable subagents"))?;
    Ok(config)
}

async fn run(args: Args, paths: Arg0DispatchPaths) -> Result<(), LaceError> {
    let config = load_config(&args, paths).await?;
    let state_db = init_state_db(&config).await;
    let auth = AuthManager::shared_from_config(&config, false)
        .await
        .map_err(failure("load Codex login (run `codex login` if needed)"))?;
    let environments = EnvironmentManager::from_codex_home(
        config.codex_home.clone(),
        Some(
            ExecServerRuntimeOptions::from_optional_paths(
                config.codex_self_exe.clone(),
                config.codex_linux_sandbox_exe.clone(),
            )
            .map_err(failure("configure Codex runtime paths"))?,
        ),
        config.http_client_factory(),
    )
    .await
    .map_err(failure("initialize Codex environment"))?;
    let installation_id = resolve_installation_id(&config.codex_home)
        .await
        .map_err(failure("resolve Codex installation"))?;
    let manager = ThreadManager::new(
        &config,
        Arc::clone(&auth),
        build_models_manager(&config, auth),
        CodexAppsToolsCache::default(),
        SessionSource::Exec,
        Arc::new(environments),
        empty_extension_registry(),
        Arc::new(CodexHomeUserInstructionsProvider::new(
            config.codex_home.clone(),
        )),
        None,
        passthrough_image_store(),
        thread_store_from_config(&config, state_db.clone()),
        local_agent_graph_store_from_state_db(state_db.as_ref()),
        installation_id,
        None,
        None,
    );

    let result = tokio::select! {
        result = tokio::time::timeout(Duration::from_secs(args.timeout_secs), greet(&manager, config)) => {
            result.map_err(|_| LaceError::Timeout).and_then(|result| result)
        }
        result = tokio::signal::ctrl_c() => {
            result.map_err(failure("listen for Ctrl-C")).and(Err(LaceError::Interrupted))
        }
    };
    let shutdown = manager
        .shutdown_all_threads_bounded(Duration::from_secs(10))
        .await;
    result?;
    if !shutdown.timed_out.is_empty() || !shutdown.submit_failed.is_empty() {
        return Err(LaceError::Greeting(
            "some Codex threads could not shut down".into(),
        ));
    }
    Ok(())
}

async fn greet(manager: &ThreadManager, config: Config) -> Result<(), LaceError> {
    let parent = manager
        .start_thread(StartThreadOptions::new(config))
        .await
        .map_err(failure("start parent thread"))?;
    println!("Parent: {}", parent.thread_id);
    let submission = parent
        .thread
        .start_turn_if_idle(TurnInputRequest::user_input(vec![UserInput::Text {
            text: PROMPT.into(),
            text_elements: Vec::new(),
        }]))
        .await
        .map_err(failure("start parent turn"))?;
    if let StartIfIdleSubmission::NotSubmitted { reason } = submission {
        return Err(LaceError::Greeting(format!(
            "parent turn was not submitted: {reason:?}"
        )));
    }

    let mut greetings = Greetings::new(parent.thread_id);
    loop {
        let event = parent
            .thread
            .next_event()
            .await
            .map_err(failure("read parent event"))?;
        if greetings.observe(event.msg)? {
            // Read actual child outcomes, including V2 replies delivered via the mailbox.
            for id in greetings.child_ids() {
                let child = manager
                    .get_thread(id)
                    .await
                    .map_err(failure("read subagent"))?;
                if child.config_snapshot().await.parent_thread_id != Some(parent.thread_id) {
                    return Err(LaceError::Greeting(format!(
                        "thread {id} is not a child of the parent"
                    )));
                }
                greetings.record_status(id, child.agent_status().await)?;
            }
            for (index, (id, reply)) in greetings.finish()?.into_iter().enumerate() {
                println!("Subagent {} ({id}): {reply}", index + 1);
            }
            return Ok(());
        }
    }
}
