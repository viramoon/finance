use std::{
    env,
    fs::File,
    io::{self, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use finance_collector::{
    market_ingress::{
        BackupManifest, MarketIngress, backup_manifest, read_quality, read_watermarks,
        recording_policy, restore_backup, wal_root,
    },
    market_wal::WalReader,
};
use finance_market_protocol::{Token, U64Text};
use serde::Serialize;

const USAGE: &str = "Usage: finance-market-wal --data-root PATH COMMAND

Commands:
  products
  read       --source SOURCE --product PRODUCT [--after POSITION] [--limit N]
  watermarks --source SOURCE --product PRODUCT
  quality    --source SOURCE --product PRODUCT
  manifest   --source SOURCE --product PRODUCT
  seal       --source SOURCE --product PRODUCT
  restore    --manifest PATH";

#[derive(Serialize)]
struct Product {
    source: Token,
    product: Token,
}

fn json_line(output: &mut impl Write, value: &impl Serialize) -> Result<()> {
    serde_json::to_writer(&mut *output, value)?;
    output.write_all(b"\n")?;
    Ok(())
}

fn products(output: &mut impl Write, data_root: &Path) -> Result<()> {
    let mut products = Vec::new();
    for (source, product) in WalReader::products(&wal_root(data_root))? {
        if recording_policy(data_root, &source, &product)?.is_some() {
            products.push(Product { source, product });
        }
    }
    json_line(output, &products)
}

fn main() -> Result<()> {
    let mut args = env::args_os().skip(1);
    let root_flag = args.next().context(USAGE)?;
    if root_flag == "--help" || root_flag == "-h" {
        println!("{USAGE}");
        return Ok(());
    }
    ensure!(root_flag == "--data-root", "{USAGE}");
    let data_root = PathBuf::from(args.next().context("missing data root")?);
    ensure!(
        !data_root.as_os_str().is_empty(),
        "data root must not be empty"
    );
    let command = args.next().context(USAGE)?;
    let mut output = BufWriter::new(io::stdout().lock());
    if command == "products" {
        ensure!(args.next().is_none(), "{USAGE}");
        products(&mut output, &data_root)?;
        output.flush()?;
        return Ok(());
    }
    if command == "restore" {
        ensure!(
            args.next().is_some_and(|flag| flag == "--manifest"),
            "{USAGE}"
        );
        let path = args.next().context("missing manifest path")?;
        ensure!(args.next().is_none(), "{USAGE}");
        let manifest: BackupManifest = serde_json::from_reader(BufReader::new(File::open(path)?))?;
        return restore_backup(&data_root, &manifest);
    }

    let mut source = None;
    let mut product = None;
    let mut after = None;
    let mut limit = None;
    while let Some(flag) = args.next() {
        let flag = flag.to_str().context("argument name must be UTF-8")?;
        let value = args
            .next()
            .with_context(|| format!("missing value for {flag}"))?;
        let value = value.to_str().context("argument value must be UTF-8")?;
        match flag {
            "--source" if source.is_none() => source = Some(value.parse::<Token>()?),
            "--product" if product.is_none() => product = Some(value.parse::<Token>()?),
            "--after" if command == "read" && after.is_none() => {
                after = Some(value.parse::<U64Text>()?.get())
            }
            "--limit" if command == "read" && limit.is_none() => limit = Some(value.parse()?),
            _ => bail!("unknown or repeated flag {flag}\n{USAGE}"),
        }
    }
    let source = source.context("missing --source")?;
    let product = product.context("missing --product")?;
    match command.to_str().context("command must be UTF-8")? {
        "read" => {
            recording_policy(&data_root, &source, &product)?.context("WAL policy is missing")?;
            let reader = WalReader::open(&wal_root(&data_root), source.as_str(), product.as_str())?;
            for record in reader.read_after(after.unwrap_or(0), limit.unwrap_or(512))? {
                json_line(&mut output, &record)?;
            }
        }
        "watermarks" => json_line(
            &mut output,
            &read_watermarks(&data_root, &source, &product)?,
        )?,
        "quality" => json_line(&mut output, &read_quality(&data_root, &source, &product)?)?,
        "manifest" => json_line(
            &mut output,
            &backup_manifest(&data_root, &source, &product)?,
        )?,
        "seal" => {
            let access = recording_policy(&data_root, &source, &product)?
                .context("WAL policy is missing")?;
            ensure!(
                access.backup_personal_drive,
                "the product policy does not authorize personal Drive backup"
            );
            let mut ingress = MarketIngress::open(&data_root, access)?;
            json_line(&mut output, &ingress.seal_for_backup()?)?;
        }
        _ => bail!("unknown command\n{USAGE}"),
    }
    output.flush()?;
    Ok(())
}
