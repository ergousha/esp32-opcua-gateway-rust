use std::path::Path;

fn main() {
    // Claim certificates contain actual SECRET and are in .gitignore; therefore,
    // in a clean checkout (including CI), these files do not exist. include_str! (src/device_id.rs)
    // reads them at compile time, meaning they must exist. Generate PLACEHOLDER if missing:
    // the build will pass, but TLS will not work until the actual claim identity is flashed.
    //
    // To generate the actual identity:
    //   cd terraform && terraform output -raw claim_certificate_pem > ../certs/claim.crt.pem
    //                   terraform output -raw claim_private_key     > ../certs/claim.private.key
    ensure_placeholder(
        "certs/claim.crt.pem",
        "-----BEGIN CERTIFICATE-----\nPLACEHOLDER-run-terraform-output-claim_certificate_pem\n-----END CERTIFICATE-----\n",
    );
    ensure_placeholder(
        "certs/claim.private.key",
        "-----BEGIN RSA PRIVATE KEY-----\nPLACEHOLDER-run-terraform-output-claim_private_key\n-----END RSA PRIVATE KEY-----\n",
    );

    embuild::espidf::sysenv::output();
}

fn ensure_placeholder(rel: &str, content: &str) {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let path = Path::new(&manifest).join(rel);
    if !path.exists() {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(&path, content).expect("Failed to write placeholder cert");
        println!(
            "cargo:warning=Placeholder created: {rel} — use terraform output for actual claim identity (see certs/README.md)"
        );
    }
}
