use serde::Deserialize;
use serde_yaml::Value;
use std::{
    collections::BTreeMap,
    error::Error,
    fs,
    net::IpAddr,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub api: Api,
    pub paths: Paths,
    /// IB configuration belongs to the separate collector / probe processes.
    #[serde(rename = "ib", default)]
    pub _ib: Option<serde_yaml::Mapping>,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Api {
    pub bind_host: IpAddr,
    pub port: u16,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Paths {
    pub data_root: PathBuf,
}
fn merge(base: &mut Value, overlay: Value) {
    if let (Value::Mapping(base), Value::Mapping(overlay)) = (&mut *base, &overlay) {
        for (key, value) in overlay {
            merge(
                base.entry(key.clone()).or_insert(Value::Null),
                value.clone(),
            );
        }
    } else {
        *base = overlay;
    }
}
impl Settings {
    // 显式传入环境快照，测试不修改全局环境或读取账户密钥。
    pub fn load(dir: &Path, env: &BTreeMap<String, String>) -> Result<Self, Box<dyn Error>> {
        let profile = env
            .get("FERTILE_PROFILE")
            .map(String::as_str)
            .unwrap_or("cloudbox");
        if !matches!(profile, "cloudbox" | "stable") {
            return Err("FERTILE_PROFILE must be cloudbox or stable".into());
        }
        let mut value: Value = serde_yaml::from_str(&fs::read_to_string(dir.join("base.yaml"))?)?;
        merge(
            &mut value,
            serde_yaml::from_str(&fs::read_to_string(dir.join(format!("{profile}.yaml")))?)?,
        );
        let mut settings: Self = serde_yaml::from_value(value)?;
        if let Some(host) = env.get("FINANCE_API_BIND_HOST") {
            settings.api.bind_host = host.parse()?;
        }
        if let Some(port) = env.get("FINANCE_API_PORT") {
            settings.api.port = port.parse()?;
        }
        if let Some(root) = env.get("FERTILE_DATA_ROOT") {
            settings.paths.data_root = root.into();
        }
        if settings.api.port == 0 || !settings.paths.data_root.is_absolute() {
            return Err("port must be nonzero and data_root must be absolute".into());
        }
        Ok(settings)
    }
}
