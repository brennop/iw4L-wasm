//! The second listener: WebTransport over HTTP/3, for browsers.
//!
//! Browsers cannot speak raw QUIC with a custom ALPN and ignore the private CA
//! the native listener uses, so this one has its own port and its own
//! certificate: ECDSA P-256, self-signed, valid under 14 days, minted at
//! startup. A page trusts it through `serverCertificateHashes`; the SHA-256 is
//! logged and written to `webtransport.json` for the page config. Everything
//! after the handshake goes through the same `handle_connection` as native
//! peers, so rooms mix both.

use std::collections::HashMap;
use std::io::Write;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use master_protocol::{SESSION_IDLE, SESSION_KEEP_ALIVE};
use tokio::sync::{Mutex, Semaphore};
use wtransport::tls::self_signed::time::{Duration as CertDuration, OffsetDateTime};
use wtransport::{Endpoint, Identity, ServerConfig};

use crate::peer_conn::PeerConnection;
use crate::{
    AddressSlot, HELLO_DEADLINE, MAX_CONNECTIONS, MAX_CONNECTIONS_PER_ADDRESS, Result,
    ServiceState, handle_connection,
};

/// Browsers accept at most 14 days; 13 leaves room for rounding.
const CERT_VALIDITY_DAYS: i64 = 13;
/// Back-dated so a browser clock slightly behind ours still accepts the cert.
/// The total stays under 14 days.
const CERT_BACKDATE_HOURS: i64 = 1;
const HASH_FILE: &str = "webtransport.json";

pub struct WebTransportConfig {
    pub bind: SocketAddr,
    /// Where `webtransport.json` goes.
    pub dir: PathBuf,
    /// Names and addresses the certificate lists; browsers using
    /// `serverCertificateHashes` do not check them, other clients might.
    pub sans: Vec<String>,
}

pub struct WebTransportListener {
    endpoint: Endpoint<wtransport::endpoint::endpoint_side::Server>,
}

/// Mints the certificate, binds the port and writes the hash file.
pub fn bind(config: &WebTransportConfig) -> Result<WebTransportListener> {
    let mut sans = vec![
        "localhost".to_owned(),
        "127.0.0.1".to_owned(),
        "::1".to_owned(),
    ];
    if !config.bind.ip().is_unspecified() {
        sans.push(config.bind.ip().to_string());
    }
    sans.extend(config.sans.iter().cloned());
    let mut seen = std::collections::HashSet::new();
    sans.retain(|name| seen.insert(name.clone()));

    let not_before = OffsetDateTime::now_utc() - CertDuration::hours(CERT_BACKDATE_HOURS);
    let identity = Identity::self_signed_builder()
        .subject_alt_names(&sans)
        .not_before(not_before)
        .offset_from_not_before(CertDuration::days(CERT_VALIDITY_DAYS))
        .build()?;
    let der = identity.certificate_chain().as_slice()[0].der().to_vec();
    let hash = ring::digest::digest(&ring::digest::SHA256, &der);
    let hash_hex: String = hash.as_ref().iter().map(|b| format!("{b:02x}")).collect();

    let mut transport = wtransport::quinn::TransportConfig::default();
    transport.max_concurrent_bidi_streams(4_u8.into());
    // HTTP/3 spends three unidirectional streams per side on its own control
    // and QPACK streams before the relay's bootstrap streams.
    transport.max_concurrent_uni_streams((master_protocol::MAX_RELAY_UNI_STREAMS + 8).into());
    transport.datagram_receive_buffer_size(Some(128 * 1024));
    transport.datagram_send_buffer_size(128 * 1024);
    let server = ServerConfig::builder()
        .with_bind_address(config.bind)
        .with_custom_transport(identity, transport)
        .max_idle_timeout(Some(SESSION_IDLE))?
        .keep_alive_interval(Some(SESSION_KEEP_ALIVE))
        .build();
    let endpoint = Endpoint::server(server)?;
    let local = endpoint.local_addr()?;

    let unix = |time: OffsetDateTime| time.unix_timestamp();
    let not_after = not_before + CertDuration::days(CERT_VALIDITY_DAYS);
    let bytes: Vec<String> = hash.as_ref().iter().map(u8::to_string).collect();
    let sans_json: Vec<String> = sans.iter().map(|s| format!("\"{s}\"")).collect();
    let json = format!(
        "{{\n  \"port\": {},\n  \"algorithm\": \"sha-256\",\n  \"hash_hex\": \"{hash_hex}\",\n  \"hash_bytes\": [{}],\n  \"not_before_unix\": {},\n  \"not_after_unix\": {},\n  \"sans\": [{}]\n}}\n",
        local.port(),
        bytes.join(", "),
        unix(not_before),
        unix(not_after),
        sans_json.join(", "),
    );
    std::fs::create_dir_all(&config.dir)?;
    std::fs::write(config.dir.join(HASH_FILE), json)?;

    let expires_in =
        not_after.unix_timestamp() - SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
    let mut stderr = std::io::stderr();
    writeln!(
        stderr,
        "iw4l-master webtransport listening on {local} cert-sha256={hash_hex} valid_for_days={} (restart before it expires; hash in {})",
        expires_in / 86_400,
        config.dir.join(HASH_FILE).display()
    )?;
    Ok(WebTransportListener { endpoint })
}

impl WebTransportListener {
    /// Same admission rules as the QUIC accept loop in `serve`: address
    /// validation, per-address cap, shared connection cap.
    pub async fn run(
        self,
        state: Arc<Mutex<ServiceState>>,
        next_connection_id: Arc<AtomicU64>,
        connection_slots: Arc<Semaphore>,
        address_counts: Arc<std::sync::Mutex<HashMap<IpAddr, usize>>>,
    ) {
        loop {
            let incoming = self.endpoint.accept().await;
            if !incoming.remote_address_validated() {
                incoming.retry();
                continue;
            }
            let remote = incoming.remote_address();
            let Some(address_slot) = AddressSlot::claim(&address_counts, remote.ip()) else {
                let _ = writeln!(
                    std::io::stderr(),
                    "webtransport refused: {remote} at per-address cap {MAX_CONNECTIONS_PER_ADDRESS}"
                );
                incoming.refuse();
                continue;
            };
            let Ok(permit) = connection_slots.clone().try_acquire_owned() else {
                let _ = writeln!(
                    std::io::stderr(),
                    "webtransport refused: at connection cap {MAX_CONNECTIONS}"
                );
                incoming.refuse();
                continue;
            };
            let state = Arc::clone(&state);
            let connection_id = next_connection_id.fetch_add(1, Ordering::Relaxed);
            let started = Instant::now();
            let _ = writeln!(
                std::io::stderr(),
                "connection {connection_id} incoming (webtransport) remote={remote}"
            );
            tokio::spawn(async move {
                let _permit = permit;
                let _address_slot = address_slot;
                match accept_session(incoming).await {
                    Ok(connection) => {
                        let _ = writeln!(
                            std::io::stderr(),
                            "connection {connection_id} handshake ok (webtransport) remote={remote} elapsed_ms={}",
                            started.elapsed().as_millis()
                        );
                        handle_connection(
                            state,
                            connection_id,
                            PeerConnection::WebTransport(connection),
                        )
                        .await;
                    }
                    Err(error) => {
                        let _ = writeln!(
                            std::io::stderr(),
                            "connection {connection_id} handshake failed (webtransport) remote={remote} elapsed_ms={}: {error}",
                            started.elapsed().as_millis()
                        );
                    }
                }
            });
        }
    }
}

async fn accept_session(
    incoming: wtransport::endpoint::IncomingSession,
) -> Result<wtransport::Connection> {
    tokio::time::timeout(HELLO_DEADLINE, async {
        let request = incoming.await?;
        let _ = writeln!(
            std::io::stderr(),
            "webtransport session request path={} origin={:?}",
            request.path(),
            request.origin()
        );
        Ok(request.accept().await?)
    })
    .await
    .map_err(|_| "webtransport handshake deadline")?
}
