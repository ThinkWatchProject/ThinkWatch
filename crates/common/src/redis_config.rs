//! The fred client config for a Redis URL, TLS included.
//!
//! Every Redis client the server builds — the command client and the
//! three Pub/Sub subscribers — goes through [`client_config`], so a
//! `rediss://` URL means the same thing for all of them.
//!
//! A `rediss` / `valkeys` scheme (with or without `-cluster` /
//! `-sentinel`) turns TLS on. The connector is built here rather than
//! left to fred, which would build its own on `ClientConfig::builder()`:
//! that picks the process-wide rustls crypto provider, and this binary
//! compiles in two (aws-lc-rs for reqwest 0.13, ring for the reqwest
//! 0.12 under openidconnect), so rustls cannot choose one and panics.
//! Here the provider is named — aws-lc-rs, as reqwest uses — and the
//! server certificate is checked the way the HTTP client checks an
//! upstream's: against the platform's roots, through
//! rustls-platform-verifier. Managed Redis services (ElastiCache,
//! Upstash, Azure Cache, Redis Cloud) present certificates from public
//! CAs, which those roots cover.
//!
//! A self-hosted Redis whose certificate a private CA signed needs that
//! CA: `REDIS_CA_CERT` names a PEM file holding it (one certificate or
//! several), and then only those certificates are trusted — the same as
//! `redis-cli --cacert`.

use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use fred::types::config::{Config, TlsConfig, TlsConnector, TlsHostMapping};
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;

/// The fred config for `url`, with a TLS connector when its scheme asks
/// for TLS. `ca_cert` is the PEM file of `REDIS_CA_CERT`; it only
/// matters for a TLS URL.
///
/// Errors never quote `url`: it carries the Redis password.
pub fn client_config(url: &str, ca_cert: Option<&Path>) -> anyhow::Result<Config> {
    let mut parsed = url::Url::parse(url).context("REDIS_URL is not a valid URL")?;
    // Hand fred the plain-TCP twin of a TLS scheme, so that it parses
    // the URL without building a connector of its own (see the module
    // docs), and attach ours.
    let tls = match plain_twin(parsed.scheme()) {
        Some(plain) => {
            parsed
                .set_scheme(&plain)
                .map_err(|()| anyhow::anyhow!("REDIS_URL has an unusable scheme"))?;
            true
        }
        None => false,
    };
    let mut config = Config::from_url(parsed.as_str()).context("REDIS_URL is not usable")?;
    if tls {
        config.tls = Some(TlsConfig {
            connector: tls_connector(ca_cert)?,
            // A cluster node is reached at the address it announces, and
            // its certificate must name that address (host name or IP).
            hostnames: TlsHostMapping::None,
        });
    }
    Ok(config)
}

/// Whether `url` asks for TLS, by the same rule fred applies.
pub fn uses_tls(url: &str) -> bool {
    url::Url::parse(url).is_ok_and(|u| plain_twin(u.scheme()).is_some())
}

/// `rediss` → `redis`, `rediss-cluster` → `redis-cluster`, `valkeys` →
/// `valkey`, …; `None` for a scheme without TLS.
fn plain_twin(scheme: &str) -> Option<String> {
    [("rediss", "redis"), ("valkeys", "valkey")]
        .into_iter()
        .find_map(|(tls, plain)| {
            scheme
                .strip_prefix(tls)
                .map(|rest| format!("{plain}{rest}"))
        })
}

fn tls_connector(ca_cert: Option<&Path>) -> anyhow::Result<TlsConnector> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .context("Redis TLS: no usable protocol version")?;
    let config = match ca_cert {
        Some(path) => builder.with_root_certificates(private_roots(path)?),
        None => {
            let verifier = rustls_platform_verifier::Verifier::new(provider)
                .context("Redis TLS: could not load the system's CA certificates")?;
            // `dangerous()` is only rustls's door to a verifier of one's
            // own; this one verifies fully (chain and host name), as it
            // does inside reqwest.
            builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(verifier))
        }
    };
    Ok(config.with_no_client_auth().into())
}

/// The certificates of `REDIS_CA_CERT`, as the only roots trusted.
fn private_roots(path: &Path) -> anyhow::Result<rustls::RootCertStore> {
    let shown = path.display();
    let mut roots = rustls::RootCertStore::empty();
    for cert in CertificateDer::pem_file_iter(path)
        .with_context(|| format!("REDIS_CA_CERT: cannot read {shown}"))?
    {
        let cert = cert.with_context(|| format!("REDIS_CA_CERT: {shown} is not valid PEM"))?;
        roots
            .add(cert)
            .with_context(|| format!("REDIS_CA_CERT: {shown} holds an unusable certificate"))?;
    }
    anyhow::ensure!(
        !roots.is_empty(),
        "REDIS_CA_CERT: {shown} holds no certificate"
    );
    Ok(roots)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_urls_have_no_tls() {
        for url in [
            "redis://:pw@localhost:6379/1",
            "redis-cluster://:pw@redis-0:6379?node=redis-1:6379",
            "valkey://localhost",
        ] {
            let config = client_config(url, None).unwrap();
            assert!(config.tls.is_none(), "{url}");
            assert!(!uses_tls(url), "{url}");
        }
    }

    #[test]
    fn tls_urls_get_our_connector_and_keep_everything_else() {
        let config = client_config("rediss://user:p%40ss@cache.example.com:6380/3", None).unwrap();
        assert!(config.uses_rustls());
        assert_eq!(config.username.as_deref(), Some("user"));
        assert_eq!(config.password.as_deref(), Some("p@ss"));
        assert_eq!(config.database, Some(3));
        let fred::types::config::ServerConfig::Centralized { server } = &config.server else {
            panic!("not centralized: {:?}", config.server);
        };
        assert_eq!((&*server.host, server.port), ("cache.example.com", 6380));

        let config = client_config(
            "rediss-cluster://:pw@redis-0:6379?node=redis-1:6379&node=redis-2:6379",
            None,
        )
        .unwrap();
        assert!(config.uses_rustls());
        assert!(config.server.is_clustered());
        assert_eq!(config.server.hosts().len(), 3);

        for url in [
            "valkeys://localhost",
            "rediss-sentinel://localhost:26379/0?sentinelServiceName=m",
        ] {
            assert!(client_config(url, None).unwrap().uses_rustls(), "{url}");
            assert!(uses_tls(url), "{url}");
        }
    }

    #[test]
    fn a_ca_file_without_certificates_is_refused() {
        let dir = std::env::temp_dir().join(format!("tw-redis-ca-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let empty = dir.join("empty.pem");
        std::fs::write(&empty, "not a certificate\n").unwrap();
        let err = client_config("rediss://localhost", Some(&empty)).unwrap_err();
        assert!(
            format!("{err:#}").contains("holds no certificate"),
            "{err:#}"
        );

        let missing = dir.join("missing.pem");
        let err = client_config("rediss://localhost", Some(&missing)).unwrap_err();
        assert!(format!("{err:#}").contains("cannot read"), "{err:#}");
        std::fs::remove_dir_all(&dir).unwrap();

        // A plain URL never reads it.
        assert!(client_config("redis://localhost", Some(&missing)).is_ok());
    }

    #[test]
    fn errors_do_not_quote_the_url() {
        let err = client_config("rediss://:hunter2@", None).unwrap_err();
        assert!(!format!("{err:#}").contains("hunter2"), "{err:#}");
    }
}
