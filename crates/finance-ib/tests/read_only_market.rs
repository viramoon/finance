use std::{fs, path::Path};

// ibapi 3.2.0 exposes order APIs and low-level message builders on the same
// client used for market data. Check production sources for both entry paths.
const TRADING_OR_RAW_WIRE: &[&str] = &[
    "place_order",
    "submit_order",
    "submit_oca_orders",
    "submit_all",
    "submit",
    "build_order",
    "cancel_order",
    "global_cancel",
    "exercise_options",
    "auto_open_orders",
    "OrderBuilder",
    "PlaceOrder",
    "OrderRequestBuilder",
    "RequestBuilder",
    "SharedRequestBuilder",
    "FireAndForgetBuilder",
    "OutgoingMessages",
    "RequestMessage",
    "send_order",
    "send_raw",
    "send_with_request_id",
    "send_shared",
    "TcpStream",
];

fn check_source(path: &Path) {
    if path.is_dir() {
        for entry in fs::read_dir(path).unwrap() {
            check_source(&entry.unwrap().path());
        }
    } else if path
        .extension()
        .is_some_and(|ext| ext == "rs" || ext == "sh")
    {
        let source = fs::read_to_string(path).unwrap();
        for identifier in source.split(|c: char| !c.is_ascii_alphanumeric() && c != '_') {
            assert!(
                !TRADING_OR_RAW_WIRE.contains(&identifier),
                "{} references {identifier}",
                path.display()
            );
        }
    }
}

#[test]
fn market_sources_have_no_order_or_raw_wire_entrypoints() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    check_source(&root.join("src"));
    check_source(&root.join("../finance-collector/src/ib.rs"));
    check_source(&root.join("../../scripts/check-ib-entitlements.sh"));
}

#[test]
fn line_budget_sources_have_no_order_or_raw_wire_entrypoints() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    check_source(&root.join("../finance-collector/src/line_budget"));
}
