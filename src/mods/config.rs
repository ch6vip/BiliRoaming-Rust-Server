use super::types::{BiliConfig, BiliRuntime};
use std::{
    fs::File,
    path::{Path, PathBuf},
};

// Loading and migration must select the same file. Preserve historical .yml priority.
fn config_path(dir: &Path) -> Option<PathBuf> {
    ["yml", "yaml", "json"]
        .iter()
        .map(|ext| dir.join(format!("config.{ext}")))
        .find(|p| p.exists())
}

pub fn init_biliconfig() -> BiliConfig {
    let result = config_path(Path::new("."))
        .ok_or_else(|| "无配置文件".to_owned())
        .and_then(|path| load_biliconfig(&path));
    let mut config = result.unwrap_or_else(|error| {
        eprintln!("Configuration error: {error}");
        std::process::exit(78);
    });
    if config.report_open {
        if let Err(error) = config.report_config.init() {
            eprintln!("{error}");
            config.report_open = false;
        }
    }
    config
}

// Read-only: don't erase credentials, unknown fields, YAML comments or rotate default secrets.
fn load_biliconfig(path: &Path) -> Result<BiliConfig, String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    let config: BiliConfig = if path.extension().and_then(|s| s.to_str()) == Some("json") {
        serde_json::from_reader(file).map_err(|e| format!("Invalid JSON configuration: {e}"))?
    } else {
        serde_yaml::from_reader(file).map_err(|e| format!("Invalid YAML configuration: {e}"))?
    };
    if config.worker_num == 0 {
        return Err("worker_num must be positive".into());
    }
    if config.api_assesskey_open.values().any(|enabled| *enabled)
        && config.api_sign.trim().is_empty()
    {
        return Err("api_sign must be configured when the accesskey API is enabled".into());
    }
    Ok(config)
}

pub async fn prepare_before_start(bili_runtime: BiliRuntime<'_>) {
    // set resign_info
    if !bili_runtime.config.cn_resign_info.access_key.is_empty()
        && bili_runtime.redis_get("a11101").await.is_none()
    {
        bili_runtime
            .redis_set("a11101", &bili_runtime.config.cn_resign_info.to_json(), 0)
            .await;
    }

    if !bili_runtime.config.th_resign_info.access_key.is_empty()
        && bili_runtime.redis_get("a41101").await.is_none()
    {
        bili_runtime
            .redis_set("a41101", &bili_runtime.config.th_resign_info.to_json(), 0)
            .await;
    }
}

pub fn load_sslconfig() -> Result<rustls::ServerConfig, Box<dyn std::error::Error>> {
    use std::io::BufReader;
    sslconfig_from_readers(
        &mut BufReader::new(File::open("certificates/fullchain.pem")?),
        &mut BufReader::new(File::open("certificates/privkey.pem")?),
    )
}

pub fn sslconfig_from_readers(
    cert: &mut dyn std::io::BufRead,
    key: &mut dyn std::io::BufRead,
) -> Result<rustls::ServerConfig, Box<dyn std::error::Error>> {
    use rustls_pki_types::{pem::PemObject, CertificateDer};
    let certs = CertificateDer::pem_reader_iter(cert).collect::<Result<Vec<_>, _>>()?;
    let key = read_private_key(key)?;
    Ok(
        rustls::ServerConfig::builder_with_provider(std::sync::Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(certs, key)?,
    )
}

pub fn read_private_key(
    reader: &mut dyn std::io::BufRead,
) -> std::io::Result<rustls_pki_types::PrivateKeyDer<'static>> {
    use rustls_pki_types::pem::PemObject;
    rustls_pki_types::PrivateKeyDer::from_pem_reader(reader)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

pub async fn update_biliconfig() -> Result<bool, Box<dyn std::error::Error>> {
    let Some(path) = config_path(Path::new(".")) else {
        return Ok(false);
    };
    migrate_config(&path).await
}

async fn migrate_config(path: &Path) -> Result<bool, Box<dyn std::error::Error>> {
    let original = tokio::fs::read_to_string(path).await?;
    // serde_yaml::Value preserves YAML enum tags and also parses JSON.
    let mut value: serde_yaml::Value = serde_yaml::from_str(&original)?;
    if value["config_version"].as_u64().unwrap_or(3) > 3 {
        return Ok(false);
    }
    for (old, new) in [("port", "http_port"), ("woker_num", "worker_num")] {
        if value[new].is_null() && !value[old].is_null() {
            value[new] = value[old].clone();
        }
    }
    value["config_version"] = 4.into();
    let migrated = if path.extension().and_then(|s| s.to_str()) == Some("json") {
        serde_json::to_string_pretty(&value)?
    } else {
        serde_yaml::to_string(&value)?
    };
    // Validate before touching the existing file; preserve credentials and unknown fields.
    let _: BiliConfig = if path.extension().and_then(|s| s.to_str()) == Some("json") {
        serde_json::from_str(&migrated)?
    } else {
        serde_yaml::from_str(&migrated)?
    };
    let backup = path.with_extension(format!(
        "{}.v3.bak",
        path.extension().unwrap().to_string_lossy()
    ));
    // Never overwrite a pre-existing migration backup.
    use tokio::io::AsyncWriteExt;
    let mut backup_file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(backup)
        .await?;
    backup_file.write_all(original.as_bytes()).await?;
    backup_file.sync_all().await?;
    tokio::fs::write(path, migrated).await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("bili-config-test-{}", rand::random::<u64>()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn load_and_migration_preserve_credentials_for_all_extensions() {
        for extension in ["json", "yml", "yaml"] {
            let dir = TempDir::new();
            let path = dir.0.join(format!("config.{extension}"));
            let mut cfg: BiliConfig =
                serde_json::from_str(include_str!("../../config.example.json")).unwrap();
            cfg.cn_resign_info.access_key = "dummy-key".into();
            cfg.cn_resign_info.refresh_token = "dummy-refresh".into();
            let original = if extension == "json" {
                serde_json::to_string_pretty(&cfg).unwrap()
            } else {
                serde_yaml::to_string(&cfg).unwrap()
            };
            std::fs::write(&path, &original).unwrap();
            assert_eq!(config_path(&dir.0), Some(path.clone()));
            assert_eq!(
                load_biliconfig(&path).unwrap().cn_resign_info.refresh_token,
                "dummy-refresh"
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
            assert!(!migrate_config(&path).await.unwrap());
            let mut old: serde_yaml::Value = serde_yaml::from_str(&original).unwrap();
            old["config_version"] = 3.into();
            old["port"] = 2662.into();
            old["woker_num"] = 2.into();
            old.as_mapping_mut()
                .unwrap()
                .remove(serde_yaml::Value::from("http_port"));
            old.as_mapping_mut()
                .unwrap()
                .remove(serde_yaml::Value::from("worker_num"));
            let old = if extension == "json" {
                serde_json::to_string(&old).unwrap()
            } else {
                serde_yaml::to_string(&old).unwrap()
            };
            std::fs::write(&path, &old).unwrap();
            assert!(migrate_config(&path).await.unwrap());
            let loaded = load_biliconfig(&path).unwrap();
            assert_eq!(loaded.worker_num, 2);
            assert_eq!(loaded.http_port, 2662);
            assert_eq!(loaded.cn_resign_info.access_key, "dummy-key");
            assert_eq!(
                std::fs::read_to_string(path.with_extension(format!("{extension}.v3.bak")))
                    .unwrap(),
                old
            );
        }
    }

    #[test]
    fn examples_load_and_enabled_api_requires_explicit_secret() {
        let _: BiliConfig = serde_yaml::from_str(include_str!("../../config.example.yml")).unwrap();
        let dir = TempDir::new();
        let path = dir.0.join("config.json");
        let mut cfg: BiliConfig =
            serde_json::from_str(include_str!("../../config.example.json")).unwrap();
        cfg.api_sign.clear();
        cfg.api_assesskey_open.insert("1".into(), true);
        std::fs::write(&path, serde_json::to_string(&cfg).unwrap()).unwrap();
        assert!(load_biliconfig(&path).is_err());
        std::fs::write(&path, "invalid JSON").unwrap();
        assert!(load_biliconfig(&path).is_err());
    }
}
