//! StrategyBundle job admission, version registration and replay pinning.

use crate::{
    ApiError, AppState, ErrorBody, error,
    jobs::{db, invalid, storage_error},
    strategy_bundle::{
        Bars, Catalog, CompiledPlan, Diagnostic, InputFormat, MAX_BYTES, MemoryBindings,
        ResolvedInputs, Universe, compile, example_catalog, valid_strategy_version,
    },
};
use axum::{
    Json,
    extract::{State, rejection::JsonRejection},
    http::StatusCode,
};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::{self, Read},
    path::Path,
};
use utoipa::ToSchema;

const INPUT_DIR: &str = "finance-state/backtests/strategy-inputs";
const MAX_REPLAY_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum StrategyFormat {
    Yaml,
    Json,
}

impl StrategyFormat {
    fn name(self) -> &'static str {
        match self {
            Self::Yaml => "yaml",
            Self::Json => "json",
        }
    }

    fn compiler_format(self) -> InputFormat {
        match self {
            Self::Yaml => InputFormat::Yaml,
            Self::Json => InputFormat::Json,
        }
    }
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct StrategyDocument {
    format: StrategyFormat,
    /// UTF-8 StrategyBundle source, at most 256 KiB before JSON escaping.
    content: String,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub enum BundleStrategy {
    StrategyBundle,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BundleSubmission {
    pub strategy: BundleStrategy,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub source: StrategySource,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum StrategySource {
    Inline {
        format: StrategyFormat,
        /// UTF-8 StrategyBundle source, at most 256 KiB before JSON escaping.
        content: String,
    },
    Registered {
        id: String,
        version: String,
    },
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RegisteredStrategy {
    id: String,
    version: String,
    /// SHA-256 of the compiled, canonical bundle.
    #[schema(pattern = r"^[0-9a-f]{64}$")]
    content_sha256: String,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum AcceptedSource {
    Registered { id: String, version: String },
}

#[derive(Debug, Serialize, ToSchema)]
pub struct BundleRequestView {
    strategy: BundleStrategy,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    source: AcceptedSource,
    #[schema(pattern = r"^[0-9a-f]{64}$")]
    content_sha256: String,
}

/// Internal queue format. Public responses use BundleRequestView.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct QueuedBundle {
    pub strategy: BundleStrategy,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub plan: Value,
    pub snapshot_sha256: String,
}

impl QueuedBundle {
    pub fn view(self) -> Result<BundleRequestView, serde_json::Error> {
        let registered = serde_json::from_value::<PlanIdentity>(self.plan)?;
        Ok(BundleRequestView {
            strategy: self.strategy,
            start: self.start,
            end: self.end,
            source: AcceptedSource::Registered {
                id: registered.bundle.metadata.id,
                version: registered.bundle.metadata.version,
            },
            content_sha256: registered.content_sha256,
        })
    }
}

#[derive(Deserialize)]
struct PlanIdentity {
    content_sha256: String,
    bundle: BundleIdentity,
}

#[derive(Deserialize)]
struct BundleIdentity {
    metadata: Identity,
}

#[derive(Deserialize)]
struct Identity {
    id: String,
    version: String,
}

struct PreparedBundle {
    registered: RegisteredStrategy,
    document: StrategyDocument,
    inputs: String,
    plan: Value,
}

impl PreparedBundle {
    fn new(plan: CompiledPlan<'_>, document: StrategyDocument) -> Result<Self, ApiError> {
        let value = serde_json::to_value(&plan).map_err(storage_error)?;
        let metadata = &plan.bundle["metadata"];
        Ok(Self {
            registered: RegisteredStrategy {
                id: metadata["id"].as_str().expect("compiled metadata").into(),
                version: metadata["version"]
                    .as_str()
                    .expect("compiled metadata")
                    .into(),
                content_sha256: plan.content_sha256,
            },
            document,
            inputs: serde_json::to_string(&plan.inputs).map_err(storage_error)?,
            plan: value,
        })
    }

    fn bind(&self, tx: &Connection) -> Result<(), ApiError> {
        let RegisteredStrategy {
            id,
            version,
            content_sha256,
        } = &self.registered;
        tx.execute(
            "INSERT INTO strategy_backtest_versions(id,version,content_sha256,format,source,inputs)
             VALUES (?1,?2,?3,?4,?5,?6) ON CONFLICT(id,version) DO NOTHING",
            params![
                id,
                version,
                content_sha256,
                self.document.format.name(),
                self.document.content.as_bytes(),
                self.inputs
            ],
        )
        .map_err(storage_error)?;
        let binding: (String, String) = tx
            .query_row(
                "SELECT content_sha256,inputs FROM strategy_backtest_versions WHERE id=?1 AND version=?2",
                params![id, version],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(storage_error)?;
        if binding.0 != *content_sha256 {
            return Err(error(
                StatusCode::CONFLICT,
                "VERSION_CONFLICT",
                "Different content is already bound to this strategy ID and version",
            ));
        }
        if binding.1 != self.inputs {
            return Err(error(
                StatusCode::CONFLICT,
                "strategy_input_conflict",
                "Input metadata is already pinned for this strategy version",
            ));
        }
        Ok(())
    }

    fn using_pins(document: StrategyDocument, inputs: &str) -> Result<Self, ApiError> {
        let inputs: ResolvedInputs<'_> = serde_json::from_str(inputs).map_err(storage_error)?;
        Self::using_catalog(
            document,
            &Catalog {
                calendars: &[inputs.universe.calendar],
                universes: &[inputs.universe],
                bars: &[inputs.bars],
            },
        )
    }

    fn compile(root: &Path, document: StrategyDocument) -> Result<Self, ApiError> {
        let examples = example_catalog();
        let path = root.join(INPUT_DIR).join("catalog.json");
        check_input_directory(path.parent().expect("fixed catalog parent"))?;
        let Some(bytes) = read_bounded(&path, MAX_BYTES).map_err(input_error)? else {
            return Self::using_catalog(document, &examples);
        };
        let configured: ConfiguredCatalog<'_> =
            serde_json::from_slice(&bytes).map_err(|_| input_error("invalid local catalog"))?;
        let universes: Vec<_> = examples
            .universes
            .iter()
            .chain(&configured.universes)
            .copied()
            .collect();
        let bars: Vec<_> = examples
            .bars
            .iter()
            .chain(&configured.bars)
            .copied()
            .collect();
        if !unique_ids(universes.iter().map(|universe| universe.id))
            || !unique_ids(bars.iter().map(|series| series.id))
        {
            return Err(input_error("duplicate versioned input in local catalog"));
        }
        Self::using_catalog(
            document,
            &Catalog {
                calendars: examples.calendars,
                universes: &universes,
                bars: &bars,
            },
        )
    }

    fn using_catalog(document: StrategyDocument, catalog: &Catalog<'_>) -> Result<Self, ApiError> {
        let plan = compile(
            document.content.as_bytes(),
            document.format.compiler_format(),
            catalog,
            &mut MemoryBindings::default(),
        )
        .map_err(compile_error)?;
        Self::new(plan, document)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfiguredCatalog<'a> {
    #[serde(borrow)]
    universes: Vec<Universe<'a>>,
    #[serde(borrow)]
    bars: Vec<Bars<'a>>,
}

fn unique_ids<'a>(mut ids: impl Iterator<Item = &'a str>) -> bool {
    let mut seen = BTreeSet::new();
    ids.all(|id| seen.insert(id))
}

fn compile_error(diagnostic: Diagnostic) -> ApiError {
    let mut response = error(
        StatusCode::UNPROCESSABLE_ENTITY,
        diagnostic.code,
        &diagnostic.message,
    );
    response.1.error.path = Some(diagnostic.path);
    response.1.error.location = Some(diagnostic.location);
    response
}

fn input_error(err: impl std::fmt::Display) -> ApiError {
    eprintln!("strategy backtest input: {err}");
    error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "strategy_input_error",
        "Cannot read the local strategy input catalog",
    )
}

fn version_missing() -> ApiError {
    error(
        StatusCode::NOT_FOUND,
        "strategy_version_not_found",
        "No registered strategy version",
    )
}

fn read_version(tx: &Connection, id: &str, version: &str) -> Result<PreparedBundle, ApiError> {
    if !valid_strategy_version(id, version) {
        return Err(invalid("Invalid strategy version reference"));
    }
    let (format, bytes, hash, inputs): (String, Vec<u8>, String, String) = tx
        .query_row(
            "SELECT format,source,content_sha256,inputs FROM strategy_backtest_versions WHERE id=?1 AND version=?2",
            params![id, version],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()
        .map_err(storage_error)?
        .ok_or_else(version_missing)?;
    let document = StrategyDocument {
        format: match format.as_str() {
            "yaml" => StrategyFormat::Yaml,
            "json" => StrategyFormat::Json,
            _ => return Err(storage_error("invalid stored strategy format")),
        },
        content: String::from_utf8(bytes).map_err(storage_error)?,
    };
    let prepared = PreparedBundle::using_pins(document, &inputs)?;
    if prepared.registered.id != id
        || prepared.registered.version != version
        || prepared.registered.content_sha256 != hash
    {
        return Err(storage_error(
            "stored strategy version binding does not match its content",
        ));
    }
    Ok(prepared)
}

fn read_bounded(path: &Path, limit: usize) -> io::Result<Option<Vec<u8>>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    if !metadata.is_file() || metadata.len() > limit as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected a bounded regular input file",
        ));
    }
    let mut bytes = Vec::new();
    File::open(path)?
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "input file exceeds the size limit",
        ));
    }
    Ok(Some(bytes))
}

fn check_input_directory(path: &Path) -> Result<(), ApiError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(input_error(
            "strategy input directories must be concrete directories",
        )),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(input_error(err)),
    }
}

fn replay_component(id: &str, suffix: &str) -> String {
    if id.len() + suffix.len() <= 255 {
        return format!("{id}{suffix}");
    }
    // F2 accepts an identifier up to 256 ASCII bytes, larger than NAME_MAX.
    // '~' cannot start a literal F2 identifier, so the two namespaces stay disjoint.
    format!("~{:x}{suffix}", Sha256::digest(id.as_bytes()))
}

#[derive(Deserialize)]
struct ReplayHeader {
    schema: String,
    bars_ref: String,
    universe_ref: String,
    calendar: String,
    currency: String,
    timeframe: String,
    price_basis: String,
}

fn pin_replay(tx: &Connection, root: &Path, inputs: &str) -> Result<String, ApiError> {
    let inputs: ResolvedInputs<'_> = serde_json::from_str(inputs).map_err(storage_error)?;
    // F2 validates the references before resolving inputs. Do not use caller-supplied paths.
    let input_root = root.join(INPUT_DIR);
    check_input_directory(&input_root)?;
    let directory = input_root.join(replay_component(inputs.bars.id, ""));
    check_input_directory(&directory)?;
    let path = directory.join(replay_component(inputs.universe.id, ".json"));
    let bytes = read_bounded(&path, MAX_REPLAY_BYTES)
        .map_err(input_error)?
        .ok_or_else(|| {
            error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "no_data",
                "No local replay snapshot is published for the strategy inputs",
            )
        })?;
    let header = serde_json::from_slice::<ReplayHeader>(&bytes)
        .map_err(|_| input_error("invalid replay header"))?;
    if header.schema != "fertile.finance/strategy-replay/v1"
        || header.bars_ref != inputs.bars.id
        || header.universe_ref != inputs.universe.id
        || header.calendar != inputs.bars.calendar
        || header.currency != inputs.bars.currency
        || header.timeframe != inputs.bars.timeframe
        || header.price_basis != inputs.bars.price_basis
    {
        return Err(error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "strategy_snapshot_invalid",
            "Published replay snapshot metadata does not match the compiled strategy inputs",
        ));
    }
    let digest = format!("{:x}", Sha256::digest(&bytes));
    tx.execute(
        "INSERT INTO strategy_backtest_snapshots(snapshot_sha256,content) VALUES (?1,?2)
         ON CONFLICT(snapshot_sha256) DO NOTHING",
        params![digest, bytes],
    )
    .map_err(storage_error)?;
    Ok(digest)
}

pub(super) enum BundleAdmission {
    Retry,
    New(QueuedBundle),
}

pub(super) fn admit(
    tx: &Connection,
    root: &Path,
    submission: BundleSubmission,
    existing: Option<&QueuedBundle>,
    check_capacity: impl FnOnce(&Connection) -> Result<(), ApiError>,
) -> Result<BundleAdmission, ApiError> {
    let prepared = match submission.source {
        StrategySource::Inline { format, content } => {
            let document = StrategyDocument { format, content };
            if let Some(previous) = existing {
                let inputs =
                    serde_json::to_string(&previous.plan["inputs"]).map_err(storage_error)?;
                let ephemeral = StrategyDocument {
                    format,
                    content: document.content.clone(),
                };
                match PreparedBundle::using_pins(ephemeral, &inputs) {
                    Err((_, body)) if body.error.code == "UNKNOWN_CATALOG_REF" => {
                        PreparedBundle::compile(root, document)?
                    }
                    result => result?,
                }
            } else {
                PreparedBundle::compile(root, document)?
            }
        }
        StrategySource::Registered { id, version } => read_version(tx, &id, &version)?,
    };
    if let Some(previous) = existing {
        if previous.start == submission.start
            && previous.end == submission.end
            && previous.plan["content_sha256"].as_str() == Some(&prepared.registered.content_sha256)
        {
            return Ok(BundleAdmission::Retry);
        }
        return Err(super::jobs::key_conflict());
    }
    check_capacity(tx)?;
    prepared.bind(tx)?;
    let snapshot_sha256 = pin_replay(tx, root, &prepared.inputs)?;
    Ok(BundleAdmission::New(QueuedBundle {
        strategy: submission.strategy,
        start: submission.start,
        end: submission.end,
        plan: prepared.plan,
        snapshot_sha256,
    }))
}

#[utoipa::path(post, path = "/api/v1/strategies", request_body = StrategyDocument, responses(
    (status=200,body=RegisteredStrategy),(status=409,body=ErrorBody),
    (status=422,body=ErrorBody),(status=500,body=ErrorBody)))]
pub async fn register(
    State(state): State<AppState>,
    request: Result<Json<StrategyDocument>, JsonRejection>,
) -> Result<Json<RegisteredStrategy>, ApiError> {
    let document = request
        .map_err(|_| invalid("Invalid strategy registration request"))?
        .0;
    let root = state.data_root.clone();
    db(state, move |conn| {
        let prepared = PreparedBundle::compile(&root, document)?;
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        prepared.bind(&tx)?;
        tx.commit().map_err(storage_error)?;
        Ok(Json(prepared.registered))
    })
    .await
}
