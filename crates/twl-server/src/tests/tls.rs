//! Tests for TLS certificate generation and reuse.

use crate::tls::load_or_generate;
use twl_config::TlsConfig;

/// Returns a unique temporary directory path for TLS tests.
fn unique_tls_dir() -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "twl-tls-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

#[tokio::test]
async fn generate_writes_and_reloads() {
    let _guard = rustls::crypto::ring::default_provider().install_default();

    let dir = unique_tls_dir();

    let config = TlsConfig {
        enabled: true,
        cert_path: dir.join("cert.pem").to_string_lossy().into_owned(),
        key_path: dir.join("key.pem").to_string_lossy().into_owned(),
        hostnames: vec!["127.0.0.1".to_string(), "localhost".to_string()],
    };

    let _cfg = load_or_generate(&config)
        .await
        .expect("first load_or_generate should succeed");

    let cert_path = std::path::PathBuf::from(&config.cert_path);
    let key_path = std::path::PathBuf::from(&config.key_path);

    assert!(cert_path.is_file());
    assert!(key_path.is_file());

    let cert_first = std::fs::read(&cert_path).expect("read cert");
    let key_first = std::fs::read(&key_path).expect("read key");

    let _cfg2 = load_or_generate(&config)
        .await
        .expect("second load_or_generate should succeed");

    let cert_second = std::fs::read(&cert_path).expect("read cert again");
    let key_second = std::fs::read(&key_path).expect("read key again");

    assert_eq!(cert_first, cert_second);
    assert_eq!(key_first, key_second);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(&key_path).expect("key metadata");
        let mode = meta.permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn same_missing_path_is_rejected_before_generation_on_every_start() {
    let dir = unique_tls_dir();
    let shared_path = dir.join("shared.pem");
    let config = TlsConfig {
        enabled: true,
        cert_path: shared_path.to_string_lossy().into_owned(),
        key_path: shared_path.to_string_lossy().into_owned(),
        hostnames: vec!["localhost".to_string()],
    };

    for attempt in ["initial startup", "restart"] {
        let err = load_or_generate(&config)
            .await
            .expect_err("same certificate/key path must fail");
        assert!(
            err.to_string().contains("different files"),
            "{attempt} returned an unexpected error: {err}"
        );
        assert!(
            !shared_path.exists(),
            "{attempt} must reject the paths before generating or writing TLS material"
        );
    }
}
