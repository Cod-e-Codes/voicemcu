use std::path::Path;
use std::sync::Arc;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

pub struct TlsSetup {
    pub server_config: quinn::ServerConfig,
    pub cert_fingerprint: String,
}

/// Build TLS configuration from files on disk, or generate a new self-signed
/// certificate. When `cert_file` and `key_file` are both `Some` and exist,
/// the certificate is loaded from disk. If they are `Some` but missing, a
/// new self-signed pair is generated and saved to those paths. If both are
/// `None`, the certificate is purely ephemeral.
pub fn setup_tls(
    cert_file: Option<&Path>,
    key_file: Option<&Path>,
    datagram_buffer: usize,
) -> Result<TlsSetup, BoxError> {
    let (cert_der, key_der) = match (cert_file, key_file) {
        (Some(cf), Some(kf)) if cf.exists() && kf.exists() => {
            tracing::info!(cert = %cf.display(), key = %kf.display(), "loading TLS certificate from disk");
            let cert_pem = std::fs::read(cf)?;
            let key_pem = std::fs::read(kf)?;
            let cert = rustls_pemfile::certs(&mut &cert_pem[..])
                .next()
                .ok_or("no certificate found in cert file")??;
            let key = rustls_pemfile::private_key(&mut &key_pem[..])?
                .ok_or("no private key found in key file")?;
            (cert, key)
        }
        (cert_path, key_path) => {
            let key_pair = rcgen::KeyPair::generate()?;
            let cert_params = rcgen::CertificateParams::new(vec!["localhost".into()])?;
            let cert = cert_params.self_signed(&key_pair)?;

            if let (Some(cf), Some(kf)) = (cert_path, key_path) {
                let cert_pem = cert.pem();
                let key_pem = key_pair.serialize_pem();
                std::fs::write(cf, cert_pem.as_bytes())?;
                std::fs::write(kf, key_pem.as_bytes())?;
                tracing::info!(
                    cert = %cf.display(),
                    key = %kf.display(),
                    "generated and saved new TLS certificate"
                );
            } else {
                tracing::info!(
                    "using ephemeral TLS certificate (will not persist across restarts)"
                );
            }

            let cert_der = cert.der().clone();
            let key_der = rustls::pki_types::PrivateKeyDer::from(
                rustls::pki_types::PrivatePkcs8KeyDer::from(key_pair.serialize_der()),
            );
            (cert_der, key_der)
        }
    };

    let fingerprint = ring::digest::digest(&ring::digest::SHA256, cert_der.as_ref());
    let cert_fingerprint: String = fingerprint
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();

    let server_crypto = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)?;

    let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(server_crypto)?,
    ));

    let mut transport = quinn::TransportConfig::default();
    transport.datagram_receive_buffer_size(Some(datagram_buffer));
    server_config.transport_config(Arc::new(transport));

    Ok(TlsSetup {
        server_config,
        cert_fingerprint,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ephemeral_cert_produces_fingerprint() {
        let setup = setup_tls(None, None, 65_536).expect("ephemeral tls");
        assert_eq!(setup.cert_fingerprint.len(), 64);
        assert!(
            setup
                .cert_fingerprint
                .chars()
                .all(|c| c.is_ascii_hexdigit())
        );
    }

    #[test]
    fn generate_and_reload_from_disk() {
        let dir = std::env::temp_dir().join("voicemcu_tls_test");
        let _ = std::fs::create_dir_all(&dir);
        let cert_path = dir.join("test_cert.pem");
        let key_path = dir.join("test_key.pem");

        // Clean up any prior run
        let _ = std::fs::remove_file(&cert_path);
        let _ = std::fs::remove_file(&key_path);

        // First call: generates and saves
        let setup1 = setup_tls(Some(&cert_path), Some(&key_path), 65_536).expect("generate");
        assert!(cert_path.exists());
        assert!(key_path.exists());

        // Second call: loads from disk -- fingerprint must match
        let setup2 = setup_tls(Some(&cert_path), Some(&key_path), 65_536).expect("reload");
        assert_eq!(setup1.cert_fingerprint, setup2.cert_fingerprint);

        // Cleanup
        let _ = std::fs::remove_file(&cert_path);
        let _ = std::fs::remove_file(&key_path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn missing_files_generates_new_and_saves() {
        let dir = std::env::temp_dir().join("voicemcu_tls_test_missing");
        let _ = std::fs::create_dir_all(&dir);
        let cert_path = dir.join("new_cert.pem");
        let key_path = dir.join("new_key.pem");

        let _ = std::fs::remove_file(&cert_path);
        let _ = std::fs::remove_file(&key_path);

        assert!(!cert_path.exists());
        assert!(!key_path.exists());

        let setup =
            setup_tls(Some(&cert_path), Some(&key_path), 65_536).expect("generate to new paths");

        assert!(cert_path.exists());
        assert!(key_path.exists());
        assert_eq!(setup.cert_fingerprint.len(), 64);

        let _ = std::fs::remove_file(&cert_path);
        let _ = std::fs::remove_file(&key_path);
        let _ = std::fs::remove_dir(&dir);
    }
}
