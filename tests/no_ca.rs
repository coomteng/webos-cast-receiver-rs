//! The release image is `scratch`: there is no system CA store. Every HTTP
//! client the server builds at startup must cope with that, or the process
//! panics before it serves anything (seen with the lyrics client in 0.3.0).
//!
//! Own test binary on purpose: it points this process at an empty CA store
//! through the environment, which must not leak into the other tests.
use std::collections::HashMap;
use webos_cast_receiver::config::Config;
use webos_cast_receiver::server::build;

#[tokio::test]
async fn server_builds_without_any_system_ca_certificates() {
    let tmp = tempfile::tempdir().unwrap();
    let empty_file = tmp.path().join("no-certs.pem");
    std::fs::write(&empty_file, "").unwrap();
    let empty_dir = tmp.path().join("no-certs.d");
    std::fs::create_dir_all(&empty_dir).unwrap();
    std::env::set_var("SSL_CERT_FILE", &empty_file);
    std::env::set_var("SSL_CERT_DIR", &empty_dir);

    let env: HashMap<String, String> = [("DATA_DIR", tmp.path().to_str().unwrap()), ("PORT", "0"), ("ADVERTISE_HOST", "127.0.0.1")]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let (app, _router) = build(Config::from_map(env));
    assert!(!app.cfg.uuid.is_empty());
}
