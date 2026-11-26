use finance_api::strategy_bundle::{
    InputFormat, MAX_BYTES, MemoryBindings, SqliteBindings, VersionBindings, bundle_schema,
    compile, component_registry, example_catalog,
};
use serde::Serialize;
use std::error::Error;
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "usage: finance-strategy schema | registry | compile [--format yaml|json] [--bindings SQLITE] <FILE|->";

fn main() -> ExitCode {
    match run(std::env::args_os().skip(1).collect()) {
        Ok(status) => status,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Vec<OsString>) -> Result<ExitCode, Box<dyn Error>> {
    match args.as_slice() {
        [command] if command == "schema" => json_line(io::stdout().lock(), &bundle_schema())?,
        [command] if command == "registry" => {
            json_line(io::stdout().lock(), &component_registry())?
        }
        [command] if command == "--help" || command == "-h" => println!("{USAGE}"),
        [command, args @ ..] if command == "compile" => return compile_file(args),
        _ => return Err(USAGE.into()),
    }
    Ok(ExitCode::SUCCESS)
}

fn compile_file(args: &[OsString]) -> Result<ExitCode, Box<dyn Error>> {
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
                    _ => return Err(USAGE.into()),
                });
            }
            Some("--bindings") if bindings.is_none() => {
                bindings = Some(PathBuf::from(args.next().ok_or(USAGE)?));
            }
            Some(arg) if arg.starts_with('-') && arg != "-" => return Err(USAGE.into()),
            _ if input.is_none() => input = Some(PathBuf::from(arg)),
            _ => return Err(USAGE.into()),
        }
    }
    let input = input.ok_or(USAGE)?;
    let format = format.unwrap_or_else(|| match input.extension().and_then(|ext| ext.to_str()) {
        Some("json") => InputFormat::Json,
        _ => InputFormat::Yaml,
    });
    let mut bytes = Vec::new();
    let limit = MAX_BYTES as u64 + 1;
    if input.as_os_str() == "-" {
        io::stdin().lock().take(limit).read_to_end(&mut bytes)?;
    } else {
        File::open(input)?.take(limit).read_to_end(&mut bytes)?;
    }
    let mut bindings: Box<dyn VersionBindings> = match bindings {
        Some(path) => Box::new(SqliteBindings::open(&path)?),
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

fn json_line(mut output: impl Write, value: &impl Serialize) -> Result<(), Box<dyn Error>> {
    serde_json::to_writer_pretty(&mut output, value)?;
    writeln!(output)?;
    Ok(())
}
