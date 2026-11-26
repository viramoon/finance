use finance_api::{ApiDoc, AppState, config::Settings, router};
use std::{collections::BTreeMap, error::Error, net::Ipv4Addr, path::PathBuf};
use utoipa::OpenApi;
#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args == ["export-openapi"] {
        let mut spec = ApiDoc::openapi();
        // No package license has been declared. An empty License Object is invalid OpenAPI.
        if spec
            .info
            .license
            .as_ref()
            .is_some_and(|license| license.name.is_empty())
        {
            spec.info.license = None;
        }
        println!("{}", spec.to_pretty_json()?);
        return Ok(());
    }
    if let [command, data_root, research_root, port] = args.as_slice()
        && command == "serve-isolated"
    {
        let data_root = PathBuf::from(data_root);
        let research_root = PathBuf::from(research_root);
        let port: u16 = port.parse()?;
        if port == 0 || !data_root.is_absolute() || !research_root.is_absolute() {
            return Err("isolated server requires absolute roots and a nonzero port".into());
        }
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await?;
        return serve(
            listener,
            AppState {
                data_root,
                research_root,
            },
            true,
        )
        .await;
    }
    if !args.is_empty() {
        return Err(
            "usage: finance-api [export-openapi|serve-isolated DATA_ROOT RESEARCH_ROOT PORT]"
                .into(),
        );
    }
    let env: BTreeMap<_, _> = std::env::vars()
        .filter(|(k, _)| {
            matches!(
                k.as_str(),
                "FERTILE_PROFILE"
                    | "FERTILE_DATA_ROOT"
                    | "FERTILE_RESEARCH_ROOT"
                    | "FINANCE_API_BIND_HOST"
                    | "FINANCE_API_PORT"
            )
        })
        .collect();
    let dir = std::env::var_os("FINANCE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("config"));
    let settings = Settings::load(&dir, &env)?;
    let listener =
        tokio::net::TcpListener::bind((settings.api.bind_host, settings.api.port)).await?;
    serve(
        listener,
        AppState {
            data_root: settings.paths.data_root,
            research_root: settings.paths.research_root,
        },
        false,
    )
    .await
}

async fn serve(
    listener: tokio::net::TcpListener,
    state: AppState,
    isolated: bool,
) -> Result<(), Box<dyn Error>> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    if isolated {
        // Acknowledge this process's own bind before the runner sends a request.
        println!("{}", listener.local_addr()?);
    } else {
        eprintln!("finance-api listening on {}", listener.local_addr()?);
    }
    axum::serve(listener, router(state))
        .with_graceful_shutdown(async move {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {},
                _ = terminate.recv() => {},
            }
        })
        .await?;
    Ok(())
}
