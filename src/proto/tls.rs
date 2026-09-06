// MySQL over TLS — 自签名证书生成 + TLS acceptor
// 用于代理握手阶段升级客户端连接

use std::sync::Arc;

use rcgen::{CertificateParams, KeyPair};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;

/// 生成自签名 TLS 证书和私钥（仅用于开发/测试）
pub fn make_self_signed_config() -> Result<ServerConfig, Box<dyn std::error::Error>> {
    let key_pair = KeyPair::generate()?;
    let mut params = CertificateParams::new(vec!["localhost".to_string()])?;
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "newproxy-proxy");

    let cert = params.self_signed(&key_pair)?;

    let cert_der = CertificateDer::from(cert);
    let key_der = PrivateKeyDer::Pkcs8(key_pair.serialize_der().into());

    let mut config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)?;

    // 允许不安全的旧版密码套件（兼容各种 sysbench 版本）
    config.ignore_client_order = true;

    Ok(config)
}

/// 创建一个 TLS acceptor
pub fn make_acceptor() -> Result<TlsAcceptor, Box<dyn std::error::Error>> {
    let config = make_self_signed_config()?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}
