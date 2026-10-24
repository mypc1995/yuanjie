use crate::ResultType;
use rustls_pki_types::{ServerName, UnixTime};
use std::sync::Arc;
use tokio_rustls::rustls::{self, client::WebPkiServerVerifier, ClientConfig};
use tokio_rustls::rustls::{
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    DigitallySignedStruct, Error as TLSError, SignatureScheme,
};

// https://github.com/seanmonstar/reqwest/blob/fd61bc93e6f936454ce0b978c6f282f06eee9287/src/tls.rs#L608
#[derive(Debug)]
pub(crate) struct NoVerifier;

impl ServerCertVerifier for NoVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls_pki_types::CertificateDer,
        _intermediates: &[rustls_pki_types::CertificateDer],
        _server_name: &ServerName,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TLSError> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls_pki_types::CertificateDer,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TLSError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls_pki_types::CertificateDer,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TLSError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA1,
            SignatureScheme::ECDSA_SHA1_Legacy,
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::ECDSA_NISTP521_SHA512,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::ED25519,
            SignatureScheme::ED448,
        ]
    }
}

/// 一个先尝试主验证器、
/// 在主验证器失败时回退到平台验证器的证书验证器。
#[cfg(any(target_os = "android", target_os = "ios"))]
#[derive(Debug)]
struct FallbackPlatformVerifier {
    primary: Arc<dyn ServerCertVerifier>,
    fallback: Arc<dyn ServerCertVerifier>,
}

#[cfg(any(target_os = "android", target_os = "ios"))]
impl FallbackPlatformVerifier {
    fn with_platform_fallback(
        primary: Arc<dyn ServerCertVerifier>,
        provider: Arc<rustls::crypto::CryptoProvider>,
    ) -> Result<Self, TLSError> {
        #[cfg(target_os = "android")]
        if !crate::config::ANDROID_RUSTLS_PLATFORM_VERIFIER_INITIALIZED
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return Err(TLSError::General(
                "rustls-platform-verifier not initialized".to_string(),
            ));
        }
        let fallback = Arc::new(rustls_platform_verifier::Verifier::new(provider)?);
        Ok(Self { primary, fallback })
    }
}

#[cfg(any(target_os = "android", target_os = "ios"))]
impl ServerCertVerifier for FallbackPlatformVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls_pki_types::CertificateDer<'_>,
        intermediates: &[rustls_pki_types::CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, TLSError> {
        match self.primary.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        ) {
            Ok(verified) => Ok(verified),
            Err(primary_err) => {
                match self.fallback.verify_server_cert(
                    end_entity,
                    intermediates,
                    server_name,
                    ocsp_response,
                    now,
                ) {
                    Ok(verified) => Ok(verified),
                    Err(fallback_err) => {
                        log::error!(
                            "Both primary and fallback verifiers failed to verify server certificate, primary error: {:?}, fallback error: {:?}",
                            primary_err,
                            fallback_err
                        );
                        Err(primary_err)
                    }
                }
            }
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls_pki_types::CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TLSError> {
        // WebPkiServerVerifier 和 rustls_platform_verifier 使用相同的签名验证实现。
        // https://github.com/rustls/rustls/blob/1ee126adb3352a2dcd72420dcd6040351a6ddc1e/rustls/src/webpki/server_verifier.rs#L278
        // https://github.com/rustls/rustls/blob/1ee126adb3352a2dcd72420dcd6040351a6ddc1e/rustls/src/crypto/mod.rs#L17
        // https://github.com/rustls/rustls-platform-verifier/blob/1099f161bfc5e3ac7f90aad88b1bf788e72906cb/rustls-platform-verifier/src/verification/android.rs#L9
        // https://github.com/rustls/rustls-platform-verifier/blob/1099f161bfc5e3ac7f90aad88b1bf788e72906cb/rustls-platform-verifier/src/verification/apple.rs#L6
        self.primary.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls_pki_types::CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TLSError> {
        // 与 verify_tls12_signature 相同的实现。
        self.primary.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        // WebPkiServerVerifier 和 rustls_platform_verifier 使用相同的 crypto provider，
        // 因此它们支持的签名方案完全一致。
        // https://github.com/rustls/rustls/blob/1ee126adb3352a2dcd72420dcd6040351a6ddc1e/rustls/src/webpki/server_verifier.rs#L172C52-L172C85
        // https://github.com/rustls/rustls-platform-verifier/blob/1099f161bfc5e3ac7f90aad88b1bf788e72906cb/rustls-platform-verifier/src/verification/android.rs#L327
        // https://github.com/rustls/rustls-platform-verifier/blob/1099f161bfc5e3ac7f90aad88b1bf788e72906cb/rustls-platform-verifier/src/verification/apple.rs#L304
        self.primary.supported_verify_schemes()
    }
}

fn webpki_server_verifier(
    provider: Arc<rustls::crypto::CryptoProvider>,
) -> ResultType<Arc<WebPkiServerVerifier>> {
    // 同时从内置的 webpki_roots 和系统原生的证书库加载根证书。
    // 这种做法与 reqwest 和 tokio-tungstenite 处理根证书的方式一致。
    // https://github.com/snapview/tokio-tungstenite/blob/35d110c24c9d030d1608ec964d70c789dfb27452/src/tls.rs#L95
    // https://github.com/seanmonstar/reqwest/blob/b126ca49da7897e5d676639cdbf67a0f6838b586/src/async_impl/client.rs#L643
    let mut root_cert_store = rustls::RootCertStore::empty();
    root_cert_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let rustls_native_certs::CertificateResult { certs, errors, .. } =
        rustls_native_certs::load_native_certs();
    if !errors.is_empty() {
        log::warn!("native root CA certificate loading errors: {errors:?}");
    }
    root_cert_store.add_parsable_certificates(certs);

    // 按 with_root_certificates 的行为构建验证器（不带 CRL 的 WebPkiServerVerifier）。
    // reqwest 和 tokio-tungstenite 都采用这种方式。
    // https://github.com/seanmonstar/reqwest/blob/b126ca49da7897e5d676639cdbf67a0f6838b586/src/async_impl/client.rs#L749
    // https://github.com/snapview/tokio-tungstenite/blob/35d110c24c9d030d1608ec964d70c789dfb27452/src/tls.rs#L127
    // https://github.com/rustls/rustls/blob/1ee126adb3352a2dcd72420dcd6040351a6ddc1e/rustls/src/client/builder.rs#L47
    // with_root_certificates 会创建一个不做吊销检查的 WebPkiServerVerifier：
    // https://github.com/rustls/rustls/blob/1ee126adb3352a2dcd72420dcd6040351a6ddc1e/rustls/src/webpki/server_verifier.rs#L177
    // https://github.com/rustls/rustls/blob/1ee126adb3352a2dcd72420dcd6040351a6ddc1e/rustls/src/webpki/server_verifier.rs#L168
    // 由于没有提供 CRL（这里正是这种情况），必须显式设置 allow_unknown_revocation_status()
    // 以与 with_root_certificates 的行为保持一致，后者默认允许未知的吊销状态。
    // https://github.com/rustls/rustls/blob/1ee126adb3352a2dcd72420dcd6040351a6ddc1e/rustls/src/webpki/server_verifier.rs#L37
    // 注意：只有当根证书库为空时 build() 才会返回错误，这里不会出现这种情况。
    let verifier = rustls::client::WebPkiServerVerifier::builder_with_provider(
        Arc::new(root_cert_store),
        provider.clone(),
    )
    .allow_unknown_revocation_status()
    .build()
    .map_err(|e| anyhow::anyhow!(e))?;
    Ok(verifier)
}

pub fn client_config(danger_accept_invalid_cert: bool) -> ResultType<ClientConfig> {
    if danger_accept_invalid_cert {
        client_config_danger()
    } else {
        client_config_safe()
    }
}

pub fn client_config_safe() -> ResultType<ClientConfig> {
    // 使用默认 builder，它会采用默认的协议版本和 crypto provider。
    // with_protocol_versions API 在 rustls 主分支中已被移除：
    // https://github.com/rustls/rustls/pull/2599
    // 这种做法与 tokio-tungstenite 的用法一致：
    // https://github.com/snapview/tokio-tungstenite/blob/35d110c24c9d030d1608ec964d70c789dfb27452/src/tls.rs#L126
    let config_builder = rustls::ClientConfig::builder();
    let provider = config_builder.crypto_provider().clone();
    let webpki_verifier = webpki_server_verifier(provider.clone())?;
    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        match FallbackPlatformVerifier::with_platform_fallback(webpki_verifier.clone(), provider) {
            Ok(fallback_verifier) => {
                let config = config_builder
                    .dangerous()
                    .with_custom_certificate_verifier(Arc::new(fallback_verifier))
                    .with_no_client_auth();
                Ok(config)
            }
            Err(e) => {
                log::error!(
                    "Failed to create fallback verifier: {:?}, use webpki verifier instead",
                    e
                );
                let config = config_builder
                    .with_webpki_verifier(webpki_verifier)
                    .with_no_client_auth();
                Ok(config)
            }
        }
    }
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        let config = config_builder
            .with_webpki_verifier(webpki_verifier)
            .with_no_client_auth();
        Ok(config)
    }
}

pub fn client_config_danger() -> ResultType<ClientConfig> {
    let config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerifier))
        .with_no_client_auth();
    Ok(config)
}
