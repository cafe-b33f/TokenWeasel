//! TLS certificate loading and self signed generation.

use crate::server::ServerError;
use axum_server::tls_rustls::RustlsConfig;
use std::path::Path;
use twl_config::TlsConfig;

/// Load TLS configuration from disk, or generate a self-signed certificate if needed.
pub(crate) async fn load_or_generate(cfg: &TlsConfig) -> Result<RustlsConfig, ServerError> {
    cfg.validate_distinct_paths()
        .map_err(|e| ServerError::Tls { err: e.to_string() })?;

    let cert_path = Path::new(&cfg.cert_path);
    let key_path = Path::new(&cfg.key_path);

    if cert_path.is_file() && key_path.is_file() {
        return RustlsConfig::from_pem_file(cert_path, key_path)
            .await
            .map_err(|e| ServerError::Tls { err: e.to_string() });
    }

    let (cert_pem, key_pem) = generate_self_signed(&cfg.hostnames)?;

    if let Some(parent) = cert_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ServerError::Tls { err: e.to_string() })?;
    }
    if let Some(parent) = key_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ServerError::Tls { err: e.to_string() })?;
    }

    std::fs::write(cert_path, &cert_pem).map_err(|e| ServerError::Tls { err: e.to_string() })?;

    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut key_file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(key_path)
            .map_err(|e| ServerError::Tls { err: e.to_string() })?;
        key_file
            .write_all(key_pem.as_bytes())
            .map_err(|e| ServerError::Tls { err: e.to_string() })?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(key_path, &key_pem).map_err(|e| ServerError::Tls { err: e.to_string() })?;
    }

    let config = RustlsConfig::from_pem(cert_pem.into_bytes(), key_pem.into_bytes())
        .await
        .map_err(|e| ServerError::Tls { err: e.to_string() })?;
    Ok(config)
}

/// Generate a simple self-signed certificate for the given hostnames.
fn generate_self_signed(hostnames: &[String]) -> Result<(String, String), rcgen::Error> {
    let certified = rcgen::generate_simple_self_signed(hostnames.to_vec())?;
    Ok((
        certified.cert.pem().to_string(),
        certified.signing_key.serialize_pem().to_string(),
    ))
}
