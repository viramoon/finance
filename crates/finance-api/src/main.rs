use finance_api::{ApiDoc, AppState, config::Settings, router};
use std::{collections::BTreeMap, error::Error, path::PathBuf};
use utoipa::OpenApi;
#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args == ["export-openapi"] {
        println!("{}", ApiDoc::openapi().to_pretty_json()?);
        return Ok(());
    }
    if !args.is_empty() {
        return Err("usage: finance-api [export-openapi]".into());
    }
    let env: BTreeMap<_, _> = std::env::vars()
        .filter(|(k, _)| {
            matches!(
                k.as_str(),
                "FERTILE_PROFILE"
                    | "FERTILE_DATA_ROOT"
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
    eprintln!("finance-api listening on {}", listener.local_addr()?);
    axum::serve(
        listener,
        router(AppState {
            data_root: settings.paths.data_root,
        }),
    )
    .with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await?;
    Ok(())
}
