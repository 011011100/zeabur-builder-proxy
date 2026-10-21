//! `zeabur-builder`: the controller binary.

mod controller;
mod decide;
mod output;
mod remote;
mod tls;

use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

/// Zeabur Builder: turns Git commits into container images with `BuildKit`.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[expect(clippy::large_enum_variant, reason = "parsed once at start")]
#[derive(Debug, Subcommand)]
enum Command {
    /// Print the custom resource definitions as a Kubernetes List, for `kubectl apply -f -`.
    Crds,
    /// Run the controller.
    Controller(ControllerArgs),
}

#[derive(Debug, Args)]
struct ControllerArgs {
    /// The controller's namespace, where it keeps the client certificate and registry credentials.
    #[arg(long, env = "POD_NAMESPACE")]
    namespace: String,
    /// The registry for `Build`s without `spec.output.image`, such as `registry.internal:5000`.
    #[arg(long, env = "ZEABUR_BUILDER_DEFAULT_REGISTRY")]
    default_registry: String,
    /// A `kubernetes.io/dockerconfigjson` Secret in the controller's namespace to push with.
    #[arg(long, env = "ZEABUR_BUILDER_REGISTRY_AUTH_SECRET")]
    registry_auth_secret: Option<String>,
    /// The image of the Job's `fetch` and `build` containers.
    #[arg(long, env = "ZEABUR_BUILDER_JOB_IMAGE")]
    job_image: String,
    /// The headless Service of the buildkitd `StatefulSet`.
    #[arg(long, default_value = "buildkitd")]
    buildkitd_service: String,
    /// buildkitd's namespace. Default: the controller's.
    #[arg(long)]
    buildkitd_namespace: Option<String>,
    /// Selects the buildkitd Pods.
    #[arg(long, default_value = "app.kubernetes.io/name=buildkitd")]
    buildkitd_selector: String,
    /// Builds one buildkitd instance runs at once.
    #[arg(long, default_value_t = 4)]
    buildkitd_slots: u32,
    /// The Secret with buildkitd's server certificate, created on first start.
    #[arg(long, default_value = "buildkitd-tls")]
    buildkitd_tls_secret: String,
    /// The Secret with the build Jobs' client certificate, created on first start.
    #[arg(long, default_value = "buildkitd-client-tls")]
    buildkitd_client_secret: String,
    /// How long a build may run, in seconds.
    #[arg(long, default_value_t = 3600)]
    job_deadline: i64,
    /// How long a finished Job is kept, in seconds, for log collectors.
    #[arg(long, default_value_t = 600)]
    job_ttl: i32,
}

impl ControllerArgs {
    fn settings(self) -> controller::Settings {
        let buildkitd_namespace = self
            .buildkitd_namespace
            .unwrap_or_else(|| self.namespace.clone());
        controller::Settings {
            tls: tls::Locations {
                service: self.buildkitd_service,
                buildkitd_namespace,
                server_secret: self.buildkitd_tls_secret,
                controller_namespace: self.namespace,
                client_secret: self.buildkitd_client_secret,
            },
            buildkitd_selector: self.buildkitd_selector,
            buildkitd_slots: self.buildkitd_slots,
            default_registry: self.default_registry,
            registry_auth_secret: self.registry_auth_secret,
            job_image: self.job_image,
            job_deadline_seconds: self.job_deadline,
            job_ttl_seconds: self.job_ttl,
        }
    }
}

async fn run_controller(args: ControllerArgs) -> Result<(), String> {
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .map_err(|_provider| "a TLS provider is already installed".to_owned())?;
    tracing_subscriber::fmt().json().with_target(false).init();
    let client = kube::Client::try_default()
        .await
        .map_err(|e| format!("cannot reach Kubernetes: {e}"))?;
    controller::run(client, args.settings()).await
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Crds => zeabur_builder_api::definitions_json()
            .map(Some)
            .map_err(|e| e.to_string()),
        Command::Controller(args) => tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())
            .and_then(|runtime| runtime.block_on(run_controller(args)))
            .map(|()| None),
    };
    match result {
        Ok(Some(text)) => output::stdout_line(&text),
        Ok(None) => ExitCode::SUCCESS,
        Err(message) => {
            output::stderr_line(&format!("error: {message}"));
            ExitCode::FAILURE
        }
    }
}
