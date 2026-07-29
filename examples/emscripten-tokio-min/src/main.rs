//! Minimal `#[wasm_bindgen(tokio)]` worker: sleep, then resolve DNS, then
//! return the outcome as text. No goose/reqwest/rustls — just the event-loop
//! bridge + epoll reactor, to see whether the schedule/drive + setTimeout
//! re-drive + async DNS work under workerd (they do under Node).

use wasm_bindgen::prelude::*;
use web_sys::{Request, Response, ResponseInit};

fn main() {}

#[wasm_bindgen(tokio, js_namespace = ["default"])]
pub async fn fetch(_request: Request, _env: JsValue, _ctx: JsValue) -> Result<Response, JsValue> {
    std::panic::set_hook(Box::new(|info| {
        web_sys::console::error_1(&format!("RUST PANIC: {info}").into());
    }));

    let mut log = String::new();
    macro_rules! step {
        ($($a:tt)*) => {{
            let s = format!($($a)*);
            web_sys::console::log_1(&format!("[min] {s}").into());
            log.push_str(&s);
            log.push('\n');
        }};
    }

    let t0 = now();
    step!("begin");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    step!("after sleep #1 ({}ms)", now() - t0);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    step!("after sleep #2 ({}ms)", now() - t0);

    step!("dns lookup_host openrouter.ai:443 ...");
    let mut first_addr = None;
    let mut first_v6 = None;
    match tokio::net::lookup_host(("openrouter.ai", 443)).await {
        Ok(addrs) => {
            let v: Vec<_> = addrs.collect();
            first_addr = v.iter().find(|a| a.is_ipv4()).copied();
            first_v6 = v.iter().find(|a| a.is_ipv6()).copied();
            step!("dns ok ({}ms): {:?}", now() - t0, v);
        }
        Err(e) => step!("dns err ({}ms): {e}", now() - t0),
    }

    // reqwest uses a SYNCHRONOUS getaddrinfo (GaiResolver on spawn_blocking).
    // The README claims the async prewarm above populates emscripten's cache so
    // this sync path resolves. Reproduce it directly.
    step!("sync getaddrinfo (to_socket_addrs) after prewarm ...");
    {
        use std::net::ToSocketAddrs;
        match ("openrouter.ai", 443u16).to_socket_addrs() {
            Ok(it) => {
                let v: Vec<_> = it.collect();
                step!("sync getaddrinfo ok ({}ms): {:?}", now() - t0, v);
            }
            Err(e) => step!("sync getaddrinfo ERR ({}ms): kind={:?} {e}", now() - t0, e.kind()),
        }
    }

    // Also via spawn_blocking, which is how reqwest's GaiResolver actually runs.
    step!("sync getaddrinfo via spawn_blocking ...");
    match tokio::task::spawn_blocking(|| {
        use std::net::ToSocketAddrs;
        ("openrouter.ai", 443u16).to_socket_addrs().map(|it| it.collect::<Vec<_>>())
    })
    .await
    {
        Ok(Ok(v)) => step!("spawn_blocking getaddrinfo ok ({}ms): {:?}", now() - t0, v),
        Ok(Err(e)) => step!("spawn_blocking getaddrinfo ERR ({}ms): kind={:?} {e}", now() - t0, e.kind()),
        Err(e) => step!("spawn_blocking join ERR ({}ms): {e}", now() - t0),
    }

    // Raw TCP connect via tokio (emscripten NODERAWSOCKETS) to isolate the
    // socket path from reqwest's synchronous GaiResolver + TLS.
    if let Some(addr) = first_addr {
        step!("tcp connect IPv4 {addr} ...");
        match tokio::net::TcpStream::connect(addr).await {
            Ok(s) => step!("tcp v4 connected ({}ms): peer={:?}", now() - t0, s.peer_addr()),
            Err(e) => step!("tcp v4 err ({}ms): {e}", now() - t0),
        }
    }

    // IPv6 connect: reqwest/hyper may try an AAAA address first; if v6 fails
    // under emscripten NODERAWSOCKETS the connection can wedge.
    if let Some(addr) = first_v6 {
        step!("tcp connect IPv6 {addr} ...");
        match tokio::net::TcpStream::connect(addr).await {
            Ok(s) => step!("tcp v6 connected ({}ms): peer={:?}", now() - t0, s.peer_addr()),
            Err(e) => step!("tcp v6 err ({}ms): {e}", now() - t0),
        }
    }

    let _ = rustls::crypto::ring::default_provider().install_default();

    // Clock check: rustls verifies cert validity windows against the wall clock.
    // If workerd zeroes/freezes CLOCK_REALTIME, certs look invalid -> UnknownIssuer.
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => step!("SystemTime::now() unix secs = {}", d.as_secs()),
        Err(e) => step!("SystemTime::now() before epoch: {e}"),
    }
    step!("UnixTime::now() secs = {}", rustls::pki_types::UnixTime::now().as_secs());
    step!("Date.now() ms = {}", js_sys::Date::now());

    // ring signature self-test: webpki validates the cert chain by verifying
    // each cert's signature via ring. If ring's verify is wrong on emscripten,
    // the chain can't be built -> UnknownIssuer. Prove it in isolation.
    ring_selftest(&mut |m| step!("{m}"));

    // Real rustls (ring) TLS handshake over an IPv4 socket — the layer reqwest
    // adds on top of the (working) socket, isolated from reqwest/hyper.
    if let Some(addr) = first_addr {
        step!("TLS (verifying) to openrouter.ai via {addr} ...");
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        step!("root store loaded {} anchors", roots.len());
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        do_tls(addr, config, t0).await;
    }

    // Same handshake with certificate/signature verification DISABLED, to split
    // ring's handshake crypto (key exchange) from chain/signature verification.
    if let Some(addr) = first_addr {
        step!("TLS (no-verify) to openrouter.ai via {addr} ...");
        let config = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(std::sync::Arc::new(NoVerify))
            .with_no_client_auth();
        do_tls(addr, config, t0).await;
    }

    let init = ResponseInit::new();
    init.set_status(200);
    let headers = web_sys::Headers::new()?;
    headers.set("content-type", "text/plain; charset=utf-8")?;
    init.set_headers(&headers);
    Response::new_with_opt_str_and_init(Some(&log), &init)
}

fn now() -> f64 {
    js_sys::Date::now()
}

fn ring_selftest(log: &mut dyn FnMut(String)) {
    use ring::rand::SystemRandom;
    use ring::signature::{
        EcdsaKeyPair, KeyPair, UnparsedPublicKey, ECDSA_P256_SHA256_ASN1,
        ECDSA_P256_SHA256_ASN1_SIGNING,
    };

    let rng = SystemRandom::new();
    let msg = b"emscripten ring self-test";

    // SHA-256 sanity (webpki hashes the TBS cert before verifying).
    let digest = ring::digest::digest(&ring::digest::SHA256, msg);
    log(format!(
        "ring SHA256 first byte = {:#04x} (len {})",
        digest.as_ref()[0],
        digest.as_ref().len()
    ));

    match EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng) {
        Ok(pkcs8) => {
            match EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), &rng) {
                Ok(key) => {
                    let pubkey = key.public_key().as_ref().to_vec();
                    match key.sign(&rng, msg) {
                        Ok(sig) => {
                            let good = UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, &pubkey)
                                .verify(msg, sig.as_ref());
                            let bad = UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, &pubkey)
                                .verify(b"tampered", sig.as_ref());
                            log(format!(
                                "ring ECDSA P256: sign ok, verify(good)={:?} verify(bad_should_err)={:?}",
                                good.is_ok(),
                                bad.is_err()
                            ));
                        }
                        Err(e) => log(format!("ring ECDSA sign ERR: {e}")),
                    }
                }
                Err(e) => log(format!("ring ECDSA from_pkcs8 ERR: {e}")),
            }
        }
        Err(e) => log(format!("ring ECDSA generate ERR: {e}")),
    }
}

async fn do_tls(addr: std::net::SocketAddr, config: rustls::ClientConfig, t0: f64) {
    let connector = tokio_rustls::TlsConnector::from(std::sync::Arc::new(config));
    let server_name = rustls::pki_types::ServerName::try_from("openrouter.ai").unwrap();
    let log = |m: String| web_sys::console::log_1(&format!("[min] {m}").into());
    match tokio::net::TcpStream::connect(addr).await {
        Ok(sock) => match connector.connect(server_name, sock).await {
            Ok(tls) => {
                let (_, conn) = tls.get_ref();
                log(format!(
                    "TLS ok ({}ms): protocol={:?} suite={:?}",
                    js_sys::Date::now() - t0,
                    conn.protocol_version(),
                    conn.negotiated_cipher_suite().map(|s| s.suite())
                ));
            }
            Err(e) => log(format!("TLS handshake ERR ({}ms): {e}", js_sys::Date::now() - t0)),
        },
        Err(e) => log(format!("TLS pre-connect ERR ({}ms): {e}", js_sys::Date::now() - t0)),
    }
}

/// Certificate verifier that accepts everything — for isolating handshake
/// crypto from chain/signature verification. NOT for production.
#[derive(Debug)]
struct NoVerify;

impl rustls::client::danger::ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        intermediates: &[rustls::pki_types::CertificateDer<'_>],
        server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let log = |m: String| web_sys::console::log_1(&format!("[min] {m}").into());
        log(format!(
            "presented cert chain: 1 leaf + {} intermediates, sni={:?}",
            intermediates.len(),
            server_name
        ));
        for (i, der) in std::iter::once(end_entity).chain(intermediates.iter()).enumerate() {
            match x509_parser::parse_x509_certificate(der) {
                Ok((_, c)) => log(format!(
                    "  cert[{i}] subject=({}) issuer=({})",
                    c.subject(),
                    c.issuer()
                )),
                Err(e) => log(format!("  cert[{i}] parse err: {e}")),
            }
        }
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}
