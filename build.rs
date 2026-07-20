use std::path::Path;

fn main() {
    // 1. Try to fetch parameters from SSM
    println!("cargo:warning=Attempting to fetch configurations from AWS SSM Parameter Store...");
    
    let fetched_cert = fetch_ssm_parameter("/esp32-ztp/poc/claim_certificate_pem", true);
    let fetched_key = fetch_ssm_parameter("/esp32-ztp/poc/claim_private_key", true);
    let fetched_endpoint = fetch_ssm_parameter("/esp32-ztp/poc/iot_endpoint", false);
    let fetched_template = fetch_ssm_parameter("/esp32-ztp/poc/provisioning_template_name", false);

    // 2. Write certificates if successfully fetched
    if let (Some(cert), Some(key)) = (&fetched_cert, &fetched_key) {
        write_file("certs/claim.crt.pem", cert);
        write_file("certs/claim.private.key", key);
        println!("cargo:warning=Successfully updated claim certificates from AWS SSM.");
    } else {
        println!("cargo:warning=Could not fetch claim certificates from SSM. Checking for existing files or placeholders...");
        // Fallback to placeholders if files do not exist
        ensure_placeholder(
            "certs/claim.crt.pem",
            "-----BEGIN CERTIFICATE-----\nPLACEHOLDER-run-terraform-output-claim_certificate_pem\n-----END CERTIFICATE-----\n",
        );
        ensure_placeholder(
            "certs/claim.private.key",
            "-----BEGIN RSA PRIVATE KEY-----\nPLACEHOLDER-run-terraform-output-claim_private_key\n-----END RSA PRIVATE KEY-----\n",
        );
    }

    // 3. Update cfg.toml if endpoints fetched successfully
    if let (Some(endpoint), Some(template)) = (fetched_endpoint, fetched_template) {
        update_cfg_toml(&endpoint, &template);
    } else {
        println!("cargo:warning=Could not fetch IoT endpoints from SSM. Please ensure credentials are correct or manually edit cfg.toml.");
    }

    embuild::espidf::sysenv::output();
}

fn write_file(rel: &str, content: &str) {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let path = Path::new(&manifest).join(rel);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, content);
}

fn ensure_placeholder(rel: &str, content: &str) {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let path = Path::new(&manifest).join(rel);
    if !path.exists() {
        write_file(rel, content);
        println!(
            "cargo:warning=Placeholder created: {rel} — use terraform output / AWS SSM for actual claim identity"
        );
    }
}

fn load_aws_env_file() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let path = Path::new(&manifest).join("aws-env.sh");
    if path.exists() {
        if let Ok(content) = std::fs::read_to_string(path) {
            for line in content.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("export ") {
                    let parts: Vec<&str> = trimmed[7..].splitn(2, '=').collect();
                    if parts.len() == 2 {
                        let key = parts[0].trim();
                        let mut val = parts[1].trim();
                        // Remove surrounding quotes if present
                        if (val.starts_with('"') && val.ends_with('"')) || (val.starts_with('\'') && val.ends_with('\'')) {
                            val = &val[1..val.len() - 1];
                        }
                        std::env::set_var(key, val);
                    }
                }
            }
        }
    }
}

fn fetch_ssm_parameter(name: &str, decrypt: bool) -> Option<String> {
    load_aws_env_file();
    let mut args = vec!["ssm", "get-parameter", "--name", name, "--query", "Parameter.Value", "--output", "text"];
    if decrypt {
        args.push("--with-decryption");
    }
    let output = std::process::Command::new("aws")
        .args(&args)
        .output()
        .ok()?;
    
    if output.status.success() {
        let val = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !val.is_empty() && !val.contains("ParameterNotFound") {
            return Some(val);
        }
    }
    None
}

fn update_cfg_toml(iot_endpoint: &str, provisioning_template: &str) {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let path = Path::new(&manifest).join("cfg.toml");
    
    let content = if path.exists() {
        std::fs::read_to_string(&path).unwrap_or_default()
    } else {
        let example_path = Path::new(&manifest).join("cfg.toml.example");
        std::fs::read_to_string(&example_path).unwrap_or_else(|_| {
            "[esp32-opcua-gateway]\nwifi_ssid = \"\"\nwifi_psk = \"\"\n".to_string()
        })
    };

    let mut lines: Vec<String> = content.lines().map(|s| s.to_string()).collect();
    let mut updated = false;

    let mut endpoint_idx = None;
    let mut template_idx = None;
    let mut section_idx = None;

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("[esp32-opcua-gateway]") {
            section_idx = Some(i);
        } else if trimmed.starts_with("iot_endpoint") {
            endpoint_idx = Some(i);
        } else if trimmed.starts_with("provisioning_template") {
            template_idx = Some(i);
        }
    }

    if let Some(idx) = endpoint_idx {
        let expected = format!("iot_endpoint = \"{}\"", iot_endpoint);
        if lines[idx].trim() != expected {
            lines[idx] = expected;
            updated = true;
        }
    } else {
        let insert_idx = section_idx.map(|i| i + 1).unwrap_or(lines.len());
        lines.insert(insert_idx, format!("iot_endpoint = \"{}\"", iot_endpoint));
        updated = true;
    }

    // Re-scan indices as we might have inserted a line
    let mut template_idx = None;
    let mut section_idx = None;
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("[esp32-opcua-gateway]") {
            section_idx = Some(i);
        } else if trimmed.starts_with("provisioning_template") {
            template_idx = Some(i);
        }
    }

    if let Some(idx) = template_idx {
        let expected = format!("provisioning_template = \"{}\"", provisioning_template);
        if lines[idx].trim() != expected {
            lines[idx] = expected;
            updated = true;
        }
    } else {
        let insert_idx = section_idx.map(|i| i + 1).unwrap_or(lines.len());
        lines.insert(insert_idx, format!("provisioning_template = \"{}\"", provisioning_template));
        updated = true;
    }

    if updated {
        let new_content = lines.join("\n") + "\n";
        let _ = std::fs::write(&path, new_content);
        println!("cargo:warning=Updated cfg.toml with values from SSM Parameter Store.");
    }
}
