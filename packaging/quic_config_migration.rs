use serde_json::{Map, Value};

const LEGACY_DIRECT_PORT: u64 = 18_443;
const QUIC_DIRECT_PORT: u64 = 18_444;
const TLS12_CIPHER_SUITES: &[&str] = &[
    "TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384",
    "TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256",
    "TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256",
    "TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384",
    "TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256",
    "TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256",
];

pub(crate) fn migrate_quic_product_config(bytes: &[u8]) -> Result<Option<Vec<u8>>, String> {
    let mut document: Value = serde_json::from_slice(bytes)
        .map_err(|error| format!("parse existing pier.json: {error}"))?;
    let root = document
        .as_object_mut()
        .ok_or_else(|| "existing pier.json must contain a JSON object".to_string())?;
    let mut changed = migrate_listen(root)?;
    changed |= migrate_tls(root)?;
    if !changed {
        return Ok(None);
    }

    let mut migrated = serde_json::to_vec_pretty(&document)
        .map_err(|error| format!("serialize migrated pier.json: {error}"))?;
    migrated.push(b'\n');
    Ok(Some(migrated))
}

fn migrate_listen(root: &mut Map<String, Value>) -> Result<bool, String> {
    let Some(listen) = root.get_mut("listen") else {
        return Ok(false);
    };
    let listen = listen
        .as_object_mut()
        .ok_or_else(|| "existing pier.json listen value must be an object".to_string())?;
    let current_port = listen
        .get("port")
        .map(|value| parse_port(value, "listen.port"))
        .transpose()?;
    let alias_port = listen
        .get("quic_port")
        .map(|value| parse_port(value, "listen.quic_port"))
        .transpose()?;
    if let Some(alias_port) = alias_port {
        let changed_port = current_port != Some(alias_port);
        let removed_alias = listen.remove("quic_port").is_some();
        listen.insert("port".to_string(), Value::from(alias_port));
        return Ok(changed_port || removed_alias);
    }

    if current_port == Some(LEGACY_DIRECT_PORT) {
        listen.insert("port".to_string(), Value::from(QUIC_DIRECT_PORT));
        return Ok(true);
    }
    Ok(false)
}

fn parse_port(value: &Value, field: &str) -> Result<u64, String> {
    value
        .as_u64()
        .filter(|port| (1..=u16::MAX.into()).contains(port))
        .ok_or_else(|| format!("existing pier.json {field} must be an integer from 1 to 65535"))
}

fn migrate_tls(root: &mut Map<String, Value>) -> Result<bool, String> {
    let tls = root
        .entry("tls".to_string())
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| "existing pier.json tls value must be an object".to_string())?;
    let mut changed = false;
    match tls.get("minimum_version") {
        Some(Value::String(version)) if version == "TLS1.3" => {}
        Some(Value::String(version)) if version == "TLS1.2" => {
            tls.insert("minimum_version".to_string(), Value::from("TLS1.3"));
            changed = true;
        }
        Some(Value::String(version)) => {
            return Err(format!(
                "existing pier.json tls.minimum_version is unsupported: {version}"
            ));
        }
        Some(_) => {
            return Err("existing pier.json tls.minimum_version must be \"TLS1.3\"".to_string());
        }
        None => {
            tls.insert("minimum_version".to_string(), Value::from("TLS1.3"));
            changed = true;
        }
    }
    if let Some(value) = tls.get_mut("disabled_cipher_suites") {
        let suites = value.as_array_mut().ok_or_else(|| {
            "existing pier.json tls.disabled_cipher_suites must be an array".to_string()
        })?;
        if suites.iter().any(|suite| !suite.is_string()) {
            return Err(
                "existing pier.json tls.disabled_cipher_suites entries must be strings".to_string(),
            );
        }
        let original_len = suites.len();
        suites.retain(|suite| {
            suite
                .as_str()
                .is_none_or(|suite| !TLS12_CIPHER_SUITES.contains(&suite))
        });
        changed |= suites.len() != original_len;
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_dual_listener_tls12_config_to_quic_tls13() {
        let migrated = migrate_quic_product_config(
            br#"{
                "listen": {"host": "0.0.0.0", "port": 18443, "quic_port": 19444},
                "tls": {
                    "minimum_version": "TLS1.2",
                    "disabled_cipher_suites": [
                        "TLS13_AES_128_GCM_SHA256",
                        "TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256"
                    ]
                },
                "future": {"keep": true}
            }"#,
        )
        .expect("migration")
        .expect("changed");
        let parsed: Value = serde_json::from_slice(&migrated).expect("migrated JSON");
        assert_eq!(parsed["listen"]["port"], 19_444);
        assert!(parsed["listen"].get("quic_port").is_none());
        assert_eq!(parsed["tls"]["minimum_version"], "TLS1.3");
        assert_eq!(
            parsed["tls"]["disabled_cipher_suites"],
            serde_json::json!(["TLS13_AES_128_GCM_SHA256"])
        );
        assert_eq!(parsed["future"]["keep"], true);
    }

    #[test]
    fn migrates_legacy_single_port_and_implicit_tls_floor() {
        let migrated = migrate_quic_product_config(
            br#"{"listen":{"port":18443},"tls":{"disabled_cipher_suites":[]}}"#,
        )
        .expect("migration")
        .expect("changed");
        let parsed: Value = serde_json::from_slice(&migrated).expect("migrated JSON");
        assert_eq!(parsed["listen"]["port"], 18_444);
        assert_eq!(parsed["tls"]["minimum_version"], "TLS1.3");
    }

    #[test]
    fn current_quic_tls13_config_is_unchanged() {
        assert!(
            migrate_quic_product_config(
                br#"{
                    "listen": {"port": 18444},
                    "tls": {
                        "minimum_version": "TLS1.3",
                        "disabled_cipher_suites": ["TLS13_AES_128_GCM_SHA256"]
                    }
                }"#,
            )
            .expect("migration")
            .is_none()
        );
    }

    #[test]
    fn rejects_malformed_known_fields_instead_of_partially_rewriting() {
        let error = migrate_quic_product_config(
            br#"{
                "listen": {"port": 18443, "quic_port": "18444"},
                "tls": {"minimum_version": "TLS1.2"}
            }"#,
        )
        .expect_err("invalid alias");
        assert!(error.contains("listen.quic_port"));

        let error = migrate_quic_product_config(
            br#"{
                "listen": {"port": 18443},
                "tls": {"minimum_version": 1.2}
            }"#,
        )
        .expect_err("invalid TLS floor");
        assert!(error.contains("tls.minimum_version"));
    }
}
