use std::path::Path;

fn main() {
    // Claim sertifikalari gercek SIR icerir ve .gitignore'dadir; bu yuzden temiz
    // bir checkout'ta (CI dahil) dosyalar yoktur. include_str! (src/device_id.rs)
    // derleme aninda bunlari okur, yani var olmalari gerekir. Eksikse PLACEHOLDER
    // uret: build gecer, ama TLS gercek claim kimligi flashlanana kadar calismaz.
    //
    // Gercek kimligi uretmek icin:
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
        std::fs::write(&path, content).expect("placeholder cert yazilamadi");
        println!(
            "cargo:warning=Placeholder olusturuldu: {rel} — gercek claim kimligi icin terraform output kullanin (bkz. certs/README.md)"
        );
    }
}
