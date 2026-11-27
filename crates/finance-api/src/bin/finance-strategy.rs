use finance_api::strategy_bundle::{
    InputFormat, MAX_BYTES, MemoryBindings, SqliteBindings, VersionBindings, bundle_schema,
    compile, compile_legacy_sma, component_registry, example_catalog,
};
use serde::Serialize;
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

#[path = "finance-strategy/indicator_stream.rs"]
mod indicator_stream;

const USAGE: &str = "usage: finance-strategy schema | registry | compile [--format yaml|json] [--bindings SQLITE] <FILE|->\n       finance-strategy compile-legacy-sma <INSTRUMENT> <FAST_PERIOD> <SLOW_PERIOD> <SHARES>\n       finance-strategy evaluate-indicators";

fn main() -> ExitCode {
    match run(std::env::args_os().skip(1).collect()) {
        Ok(status) => status,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Vec<OsString>) -> Result<ExitCode, &'static str> {
    match args.as_slice() {
        [command] if command == "schema" => json_line(io::stdout().lock(), &bundle_schema())?,
        [command] if command == "registry" => {
            json_line(io::stdout().lock(), &component_registry())?
        }
        [command] if command == "evaluate-indicators" => {
            #[cfg(target_os = "linux")]
            indicator_stream::bind_parent_lifetime()?;
            indicator_stream::run(io::stdin().lock(), io::stdout().lock())?
        }
        [command] if command == "--help" || command == "-h" => println!("{USAGE}"),
        [command, args @ ..] if command == "compile" => return compile_file(args),
        [command, instrument, fast, slow, shares] if command == "compile-legacy-sma" => {
            let parse = |value: &OsString| {
                value
                    .to_str()
                    .and_then(|text| text.parse().ok())
                    .ok_or("SMA periods and shares must be integers.")
            };
            let plan = compile_legacy_sma(
                instrument.to_str().ok_or("Choose an XNYS instrument ID.")?,
                parse(fast)?,
                parse(slow)?,
                parse(shares)?,
            )?;
            json_line(io::stdout().lock(), &plan)?;
        }
        _ => return Err(USAGE),
    }
    Ok(ExitCode::SUCCESS)
}

fn compile_file(args: &[OsString]) -> Result<ExitCode, &'static str> {
    let mut args = args.iter();
    let mut input = None;
    let mut format = None;
    let mut bindings = None;
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--format") if format.is_none() => {
                format = Some(match args.next().and_then(|value| value.to_str()) {
                    Some("json") => InputFormat::Json,
                    Some("yaml") => InputFormat::Yaml,
                    _ => return Err(USAGE),
                });
            }
            Some("--bindings") if bindings.is_none() => {
                bindings = Some(PathBuf::from(args.next().ok_or(USAGE)?));
            }
            Some(arg) if arg.starts_with('-') && arg != "-" => return Err(USAGE),
            _ if input.is_none() => input = Some(PathBuf::from(arg)),
            _ => return Err(USAGE),
        }
    }
    let input = input.ok_or(USAGE)?;
    let format = format.unwrap_or_else(|| match input.extension().and_then(|ext| ext.to_str()) {
        Some("json") => InputFormat::Json,
        _ => InputFormat::Yaml,
    });
    let mut bytes = Vec::new();
    let limit = MAX_BYTES as u64 + 1;
    let read = if input.as_os_str() == "-" {
        io::stdin().lock().take(limit).read_to_end(&mut bytes)
    } else {
        File::open(input)
            .map_err(|_| "Could not open the strategy input file.")?
            .take(limit)
            .read_to_end(&mut bytes)
    };
    read.map_err(|_| "Could not read the strategy input.")?;
    let mut bindings: Box<dyn VersionBindings> = match bindings {
        Some(path) => Box::new(
            SqliteBindings::open(&path).map_err(|_| "Could not open the version binding store.")?,
        ),
        None => Box::<MemoryBindings>::default(),
    };
    match compile(&bytes, format, &example_catalog(), bindings.as_mut()) {
        Ok(plan) => {
            json_line(io::stdout().lock(), &plan)?;
            Ok(ExitCode::SUCCESS)
        }
        Err(diagnostic) => {
            json_line(io::stderr().lock(), &diagnostic)?;
            Ok(ExitCode::from(2))
        }
    }
}

fn json_line(mut output: impl Write, value: &impl Serialize) -> Result<(), &'static str> {
    const WRITE_FAILED: &str = "Could not write JSON output.";
    serde_json::to_writer_pretty(&mut output, value).map_err(|_| WRITE_FAILED)?;
    writeln!(output).map_err(|_| WRITE_FAILED)
}
