//! Package download gateway. Only this broker has network access; containers do not.
use anyhow::{Context, Result, ensure};
use std::{
    net::IpAddr,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpStream, UnixListener, UnixStream},
    sync::{Semaphore, watch},
};
use tokio_util::sync::CancellationToken;

// No user-supplied hosts, credentials, forwarding proxy, or arbitrary CONNECT ports.
const HOSTS: &[&str] = &[
    "registry.npmjs.org",
    "registry.yarnpkg.com",
    "pypi.org",
    "files.pythonhosted.org",
    "index.crates.io",
    "static.crates.io",
    "crates.io",
    "proxy.golang.org",
    "sum.golang.org",
    "storage.googleapis.com",
    "repo.maven.apache.org",
    "rubygems.org",
    "index.rubygems.org",
];
fn public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, _, _] = ip.octets();
            !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_broadcast()
                || ip.is_documentation()
                || ip.is_unspecified()
                || ip.is_multicast()
                || a == 0
                || a >= 240
                || (a == 100 && (64..=127).contains(&b))
                || (a == 198 && (b == 18 || b == 19))
                || (a == 192 && b == 0))
        }
        // Restrict IPv6 to global unicast, excluding the IETF special-purpose
        // /23, documentation prefixes and 6to4. Ordinary 2001:: allocations
        // such as Google's 2001:4860::/32 must remain usable.
        IpAddr::V6(ip) => {
            let s = ip.segments();
            (s[0] & 0xe000) == 0x2000
                && !(s[0] == 0x2001 && (s[1] < 0x0200 || s[1] == 0x0db8))
                && s[0] != 0x2002
                && !(s[0] == 0x3fff && s[1] < 0x1000)
        }
    }
}
fn destination(header: &str) -> Result<&str> {
    let words: Vec<_> = header
        .lines()
        .next()
        .unwrap_or("")
        .split_whitespace()
        .collect();
    ensure!(
        words.len() == 3 && words[0] == "CONNECT" && ["HTTP/1.0", "HTTP/1.1"].contains(&words[2]),
        "Only HTTPS package downloads are allowed"
    );
    let host = words[1]
        .strip_suffix(":443")
        .context("Only port 443 is allowed")?;
    ensure!(HOSTS.contains(&host), "Package host is not allowed: {host}");
    Ok(host)
}
// Check the plaintext ClientHello before sending any bytes to an upstream. CONNECT
// alone cannot constrain the TLS virtual host on shared CDN addresses. This is
// inspection only: TLS remains end-to-end and HTTP headers stay encrypted.
const HELLO_LIMIT: usize = 64 * 1024;
const HELLO_RECORD_LIMIT: usize = 16;
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
// Keep a completed HTTP keep-alive from consuming every tunnel indefinitely
// when another package request is waiting. This deadline applies only while
// another accepted connection is waiting for admission. A quiet connection with
// no demand keeps the existing 120-second whole-connection limit. Because TLS
// remains encrypted, a slow first response under pressure is indistinguishable
// from keep-alive and may require the package manager to retry.
const PRESSURE_IDLE_TIMEOUT: Duration = Duration::from_secs(15);

fn take_field<'a>(bytes: &mut &'a [u8], count: usize) -> Result<&'a [u8]> {
    ensure!(bytes.len() >= count, "Truncated TLS ClientHello");
    let (field, rest) = bytes.split_at(count);
    *bytes = rest;
    Ok(field)
}

fn sized_field<'a>(bytes: &mut &'a [u8], width: usize) -> Result<&'a [u8]> {
    let size = take_field(bytes, width)?
        .iter()
        .fold(0usize, |size, byte| (size << 8) | usize::from(*byte));
    take_field(bytes, size)
}

// rustls validates ClientHello syntax and SNI, but its public ClientHello API
// does not expose encrypted_client_hello. Refuse that extension so an allowed
// outer name cannot conceal a different upstream name.
fn reject_encrypted_hello(handshake: &[u8]) -> Result<()> {
    let mut message = handshake;
    ensure!(
        take_field(&mut message, 1)? == [1],
        "Expected TLS ClientHello"
    );
    let mut hello = sized_field(&mut message, 3)?;
    ensure!(message.is_empty(), "Unexpected data after TLS ClientHello");
    take_field(&mut hello, 34)?; // legacy_version and random
    sized_field(&mut hello, 1)?; // session ID
    sized_field(&mut hello, 2)?; // cipher suites
    sized_field(&mut hello, 1)?; // compression methods
    let mut extensions = sized_field(&mut hello, 2)?;
    ensure!(hello.is_empty(), "Unexpected TLS ClientHello data");
    while !extensions.is_empty() {
        let kind = take_field(&mut extensions, 2)?;
        ensure!(
            kind != [0xfe, 0x0d],
            "Encrypted TLS ClientHello is not allowed"
        );
        sized_field(&mut extensions, 2)?;
    }
    Ok(())
}

async fn read_client_hello(client: &mut UnixStream, host: &str) -> Result<Vec<u8>> {
    let mut acceptor = rustls::server::Acceptor::default();
    let mut wire = Vec::new();
    let mut handshake = Vec::new();
    for _ in 0..HELLO_RECORD_LIMIT {
        let mut header = [0u8; 5];
        client.read_exact(&mut header).await?;
        ensure!(header[0] == 22, "Expected a TLS handshake record");
        let size = usize::from(u16::from_be_bytes([header[3], header[4]]));
        ensure!(
            size > 0 && size <= 16 * 1024,
            "Invalid TLS handshake record size"
        );
        ensure!(
            wire.len() + header.len() + size <= HELLO_LIMIT,
            "TLS ClientHello too large"
        );
        let start = wire.len();
        wire.extend_from_slice(&header);
        wire.resize(wire.len() + size, 0);
        client.read_exact(&mut wire[start + 5..]).await?;
        handshake.extend_from_slice(&wire[start + 5..]);
        let mut record = std::io::Cursor::new(&wire[start..]);
        while record.position() < (wire.len() - start) as u64 {
            ensure!(acceptor.read_tls(&mut record)? > 0, "Incomplete TLS record");
        }
        if let Some(accepted) = acceptor
            .accept()
            .map_err(|(error, _)| anyhow::anyhow!("Invalid TLS ClientHello: {error}"))?
        {
            ensure!(
                accepted
                    .client_hello()
                    .server_name()
                    .is_some_and(|name| name.eq_ignore_ascii_case(host)),
                "TLS server name must match the CONNECT package host"
            );
            reject_encrypted_hello(&handshake)?;
            return Ok(wire);
        }
    }
    anyhow::bail!("TLS ClientHello has too many fragments")
}

async fn client_hello(client: &mut UnixStream, host: &str) -> Result<Vec<u8>> {
    tokio::time::timeout(HELLO_TIMEOUT, read_client_hello(client, host))
        .await
        .context("Timed out waiting for TLS ClientHello")?
}

async fn copy_direction<R, W>(
    reader: R,
    writer: W,
    activity: watch::Sender<tokio::time::Instant>,
    limit: u64,
) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut reader = reader.take(limit);
    let mut writer = writer;
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let size = reader.read(&mut buffer).await?;
        if size == 0 {
            return Ok(());
        }
        activity.send_replace(tokio::time::Instant::now());
        let mut remaining = &buffer[..size];
        while !remaining.is_empty() {
            let written = writer.write(remaining).await?;
            if written == 0 {
                return Err(std::io::ErrorKind::WriteZero.into());
            }
            activity.send_replace(tokio::time::Instant::now());
            remaining = &remaining[written..];
        }
    }
}

async fn relay<C, R>(
    client: &mut C,
    remote: &mut R,
    mut pressure: watch::Receiver<bool>,
    idle_timeout: Duration,
    upload_limit: u64,
    download_limit: u64,
) -> Result<()>
where
    C: AsyncRead + AsyncWrite + Unpin,
    R: AsyncRead + AsyncWrite + Unpin,
{
    let (client_read, client_write) = tokio::io::split(client);
    let (remote_read, remote_write) = tokio::io::split(remote);
    let (activity, mut last_activity) = watch::channel(tokio::time::Instant::now());
    let upload = copy_direction(client_read, remote_write, activity.clone(), upload_limit);
    let download = copy_direction(remote_read, client_write, activity, download_limit);
    tokio::pin!(upload, download);
    loop {
        let deadline = *last_activity.borrow_and_update() + idle_timeout;
        let pressured = *pressure.borrow_and_update();
        tokio::select! {
            // Consume newly observed traffic before deciding a timer has expired.
            biased;
            result = &mut upload => { result?; return Ok(()); }
            result = &mut download => { result?; return Ok(()); }
            changed = last_activity.changed() => { changed.context("Package gateway traffic monitor closed")?; }
            changed = pressure.changed() => { changed.context("Package gateway admission monitor closed")?; }
            _ = tokio::time::sleep_until(deadline), if pressured => {
                anyhow::bail!("Package gateway idle timeout after {} seconds while another connection was waiting", idle_timeout.as_secs());
            }
        }
    }
}

async fn tunnel(
    client: &mut UnixStream,
    established: &mut bool,
    pressure: watch::Receiver<bool>,
) -> Result<()> {
    let mut header = Vec::new();
    while !header.ends_with(b"\r\n\r\n") {
        ensure!(header.len() < 8192, "Proxy header too large");
        match client.read_u8().await {
            // The setup preflight opens and closes the socket to verify access.
            Err(error)
                if header.is_empty() && error.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                return Ok(());
            }
            byte => header.push(byte?),
        }
    }
    let request = std::str::from_utf8(&header)?;
    let host = destination(request)?;
    client
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await?;
    *established = true;
    let hello = client_hello(client, host).await?;
    let addresses: Vec<_> = tokio::net::lookup_host((host, 443)).await?.collect();
    ensure!(
        !addresses.is_empty() && addresses.iter().all(|a| public(a.ip())),
        "Package host resolved to a non-public address"
    );
    // Connect to the validated address, never resolve the hostname again.
    let mut remote = TcpStream::connect(addresses.as_slice()).await?;
    remote.write_all(&hello).await?;
    // Bound bytes in each direction as well as lifetime and concurrency.
    relay(
        client,
        &mut remote,
        pressure,
        PRESSURE_IDLE_TIMEOUT,
        16 * 1024 * 1024 - hello.len() as u64,
        256 * 1024 * 1024,
    )
    .await
}

const ERROR_LIMIT: usize = 16;
const ERROR_CHARS: usize = 512;

fn remember_error(errors: &Mutex<Vec<String>>, error: &str) {
    let mut errors = errors.lock().unwrap();
    if errors.len() == ERROR_LIMIT {
        errors.remove(0);
    }
    errors.push(error.chars().take(ERROR_CHARS).collect());
}

pub struct Gateway {
    stop: CancellationToken,
    task: tokio::task::JoinHandle<()>,
    errors: Arc<Mutex<Vec<String>>>,
    _directory: tempfile::TempDir,
}
impl Gateway {
    #[cfg(test)]
    pub fn start() -> Result<Self> {
        Self::start_in(&std::env::temp_dir())
    }
    pub fn start_in(root: &Path) -> Result<Self> {
        let directory = tempfile::Builder::new()
            .prefix(".crow-runtime-downloads-")
            .tempdir_in(root)?;
        // A review directory can exceed the Unix socket address limit. Bind through
        // its open directory descriptor while keeping the socket inside the review.
        #[cfg(target_os = "linux")]
        let listener = {
            use std::os::fd::AsRawFd;
            let dir = std::fs::File::open(directory.path())?;
            UnixListener::bind(format!("/proc/self/fd/{}/socket", dir.as_raw_fd()))?
        };
        #[cfg(not(target_os = "linux"))]
        let listener = UnixListener::bind(directory.path().join("socket"))?;
        let stop = CancellationToken::new();
        let cancelled = stop.clone();
        let errors = Arc::new(Mutex::new(Vec::new()));
        let task_errors = errors.clone();
        let task = tokio::spawn(async move {
            let (pressure, pressured) = watch::channel(false);
            let slots = Arc::new(Semaphore::new(16));
            let mut tasks = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    _ = cancelled.cancelled() => break,
                    Some(_) = tasks.join_next(), if !tasks.is_empty() => {},
                    incoming = listener.accept() => {
                        let Ok((mut stream,_)) = incoming else {break};
                        // At most one accepted stream waits for capacity. The
                        // remaining requests stay in the bounded socket backlog.
                        // Only real waiting demand enables idle reclamation.
                        let permit = match slots.clone().try_acquire_owned() {
                            Ok(permit) => permit,
                            Err(_) => {
                                pressure.send_replace(true);
                                let permit = tokio::select! {
                                    _ = cancelled.cancelled() => break,
                                    permit = slots.clone().acquire_owned() => {
                                        let Ok(permit) = permit else { break };
                                        permit
                                    }
                                };
                                pressure.send_replace(false);
                                permit
                            }
                        };
                        let pressured = pressured.clone();
                        let errors = task_errors.clone();
                        tasks.spawn(async move {
                            let _permit = permit;
                            let mut established = false;
                            let error = match tokio::time::timeout(Duration::from_secs(120), tunnel(&mut stream, &mut established, pressured)).await {
                                Ok(Ok(())) => return,
                                Ok(Err(error)) => format!("{error:#}"),
                                Err(_) => "Package gateway connection timed out after 120 seconds".to_owned(),
                            };
                            remember_error(&errors, &error);
                            // After CONNECT, the peer expects TLS. Keep the actual
                            // reason in private receipts instead of sending invalid
                            // plaintext HTTP into the TLS stream.
                            if !established {
                                let body = format!("Crow package gateway: {error}");
                                let response = format!("HTTP/1.1 403 Forbidden\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len());
                                let _ = tokio::time::timeout(Duration::from_secs(1),stream.write_all(response.as_bytes())).await;
                            }
                        });
                    }
                }
            }
            tasks.abort_all();
            while tasks.join_next().await.is_some() {}
        });
        Ok(Self {
            stop,
            task,
            errors,
            _directory: directory,
        })
    }
    pub fn directory(&self) -> &Path {
        self._directory.path()
    }
    /// Return bounded private diagnostics for the execution receipt.
    pub async fn close(self) -> Vec<String> {
        self.stop.cancel();
        // Drop aborts the listener too; yield until all active tunnels have been dropped.
        while !self.task.is_finished() {
            tokio::task::yield_now().await;
        }
        self.errors.lock().unwrap().clone()
    }
}
impl Drop for Gateway {
    fn drop(&mut self) {
        self.stop.cancel();
        self.task.abort();
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn retained_completed_streams_release_capacity_only_for_waiting_requests() {
        let slots = Arc::new(Semaphore::new(16));
        let (pressure, pressured) = watch::channel(false);
        let idle = Duration::from_millis(150);
        let mut clients = Vec::new();
        let mut remotes = Vec::new();
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..16 {
            let permit = slots.clone().acquire_owned().await.unwrap();
            let (mut client, mut gateway_client) = tokio::io::duplex(64);
            let (mut remote, mut gateway_remote) = tokio::io::duplex(64);
            let pressured = pressured.clone();
            tasks.spawn(async move {
                let _permit = permit;
                relay(
                    &mut gateway_client,
                    &mut gateway_remote,
                    pressured,
                    idle,
                    1024,
                    1024,
                )
                .await
            });
            remote.write_all(b"completed download").await.unwrap();
            let mut received = [0u8; 18];
            client.read_exact(&mut received).await.unwrap();
            assert_eq!(&received, b"completed download");
            clients.push(client);
            remotes.push(remote);
        }
        // All downloads have finished but the peers retain their streams.
        tokio::time::sleep(idle * 2).await;
        assert_eq!(slots.available_permits(), 0);
        assert!(slots.clone().try_acquire_owned().is_err());
        pressure.send_replace(true);
        let permit = tokio::time::timeout(Duration::from_secs(2), slots.clone().acquire_owned())
            .await
            .unwrap()
            .unwrap();
        assert!(!clients.is_empty() && !remotes.is_empty());
        let error = tasks.join_next().await.unwrap().unwrap().unwrap_err();
        assert!(error.to_string().contains("idle timeout"));
        drop(permit);
        pressure.send_replace(false);
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }

    #[tokio::test]
    async fn one_way_traffic_in_either_direction_resets_the_shared_idle_deadline() {
        for upload in [false, true] {
            let (_pressure, pressured) = watch::channel(true);
            let (mut client, mut gateway_client) = tokio::io::duplex(64);
            let (mut remote, mut gateway_remote) = tokio::io::duplex(64);
            let task = tokio::spawn(async move {
                relay(
                    &mut gateway_client,
                    &mut gateway_remote,
                    pressured,
                    Duration::from_millis(250),
                    1024,
                    1024,
                )
                .await
            });
            // Exercise a transfer lasting much longer than the idle deadline,
            // with no traffic at all in the other direction.
            for value in 0..10 {
                let (writer, reader) = if upload {
                    (&mut client, &mut remote)
                } else {
                    (&mut remote, &mut client)
                };
                writer.write_all(&[value]).await.unwrap();
                assert_eq!(reader.read_u8().await.unwrap(), value);
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            assert!(!task.is_finished());
            let error = tokio::time::timeout(Duration::from_secs(2), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err();
            assert!(error.to_string().contains("idle timeout"));
        }
    }

    #[tokio::test]
    async fn clearing_admission_pressure_preserves_a_silent_stream() {
        let (pressure, pressured) = watch::channel(true);
        let (mut client, mut gateway_client) = tokio::io::duplex(64);
        let (mut remote, mut gateway_remote) = tokio::io::duplex(64);
        let idle = Duration::from_millis(150);
        let task = tokio::spawn(async move {
            relay(
                &mut gateway_client,
                &mut gateway_remote,
                pressured,
                idle,
                1024,
                1024,
            )
            .await
        });
        client.write_all(b"request").await.unwrap();
        let mut request = [0; 7];
        remote.read_exact(&mut request).await.unwrap();
        pressure.send_replace(false);
        tokio::time::sleep(idle * 2).await;
        assert!(!task.is_finished());
        remote.write_all(b"slow response").await.unwrap();
        let mut response = [0; 13];
        client.read_exact(&mut response).await.unwrap();
        assert_eq!(&response, b"slow response");
        drop(remote);
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn monitored_copy_preserves_upload_and_download_byte_limits() {
        for upload in [false, true] {
            let (_pressure, pressured) = watch::channel(false);
            let (mut client, mut gateway_client) = tokio::io::duplex(64);
            let (mut remote, mut gateway_remote) = tokio::io::duplex(64);
            let task = tokio::spawn(async move {
                relay(
                    &mut gateway_client,
                    &mut gateway_remote,
                    pressured,
                    PRESSURE_IDLE_TIMEOUT,
                    3,
                    5,
                )
                .await
            });
            let (writer, reader) = if upload {
                (&mut client, &mut remote)
            } else {
                (&mut remote, &mut client)
            };
            writer.write_all(b"oversized").await.unwrap();
            let mut received = Vec::new();
            reader.read_to_end(&mut received).await.unwrap();
            assert_eq!(
                received,
                if upload {
                    b"ove".as_slice()
                } else {
                    b"overs".as_slice()
                }
            );
            task.await.unwrap().unwrap();
        }
    }

    #[tokio::test]
    async fn closed_admission_monitor_ends_the_relay() {
        let (pressure, pressured) = watch::channel(false);
        let (_client, mut gateway_client) = tokio::io::duplex(64);
        let (_remote, mut gateway_remote) = tokio::io::duplex(64);
        drop(pressure);
        let error = tokio::time::timeout(
            Duration::from_secs(1),
            relay(
                &mut gateway_client,
                &mut gateway_remote,
                pressured,
                PRESSURE_IDLE_TIMEOUT,
                1024,
                1024,
            ),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert!(error.to_string().contains("admission monitor closed"));
    }

    fn hello(host: &str, sni: bool) -> Vec<u8> {
        let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(rustls::RootCertStore::empty())
        .with_no_client_auth();
        config.enable_sni = sni;
        let mut client =
            rustls::ClientConnection::new(Arc::new(config), host.to_owned().try_into().unwrap())
                .unwrap();
        let mut wire = Vec::new();
        client.write_tls(&mut wire).unwrap();
        wire
    }

    async fn inspect(wire: Vec<u8>) -> Result<Vec<u8>> {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        let writer = tokio::spawn(async move {
            // Also exercise TCP-style partial reads within record headers/bodies.
            for chunk in wire.chunks(7) {
                client.write_all(chunk).await?;
            }
            anyhow::Ok(())
        });
        let result = client_hello(&mut server, "registry.npmjs.org").await;
        drop(server);
        let _ = writer.await.unwrap();
        result
    }

    #[test]
    fn gateway_diagnostics_keep_only_recent_bounded_messages() {
        let errors = Mutex::new(Vec::new());
        for number in 0..ERROR_LIMIT + 3 {
            remember_error(&errors, &format!("{number}:{}", "é".repeat(ERROR_CHARS)));
        }
        let errors = errors.lock().unwrap();
        assert_eq!(errors.len(), ERROR_LIMIT);
        assert!(errors[0].starts_with("3:"));
        assert!(
            errors
                .iter()
                .all(|error| error.chars().count() == ERROR_CHARS)
        );
    }

    #[tokio::test]
    async fn socket_access_preflight_does_not_record_a_failure() {
        let gateway = Gateway::start().unwrap();
        let mut client = UnixStream::connect(gateway.directory().join("socket"))
            .await
            .unwrap();
        client.shutdown().await.unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        assert!(response.is_empty());
        assert!(gateway.close().await.is_empty());
    }

    #[tokio::test]
    async fn tls_inspection_preserves_valid_and_fragmented_client_hello() {
        let wire = hello("registry.npmjs.org", true);
        assert_eq!(inspect(wire.clone()).await.unwrap(), wire);
        let mut fragmented = Vec::new();
        for fragment in wire[5..].chunks(80) {
            fragmented.extend_from_slice(&[22, 3, 1]);
            fragmented.extend_from_slice(&(fragment.len() as u16).to_be_bytes());
            fragmented.extend_from_slice(fragment);
        }
        assert_eq!(inspect(fragmented.clone()).await.unwrap(), fragmented);
    }

    #[tokio::test]
    async fn tls_inspection_rejects_other_tenants_and_missing_names() {
        for wire in [
            hello("other-tenant.example", true),
            hello("registry.npmjs.org", false),
        ] {
            let error = inspect(wire).await.unwrap_err().to_string();
            assert!(error.contains("must match"), "{error}");
        }
    }

    #[tokio::test]
    async fn tls_inspection_rejects_encrypted_client_hello() {
        let mut wire = hello("registry.npmjs.org", true);
        let mut fields = &wire[9..];
        take_field(&mut fields, 34).unwrap();
        sized_field(&mut fields, 1).unwrap();
        sized_field(&mut fields, 2).unwrap();
        sized_field(&mut fields, 1).unwrap();
        let offset = wire.len() - fields.len();
        // Valid outer ECH: KDF/AEAD IDs, config ID, empty enc, one payload byte.
        let extension = [0xfe, 0x0d, 0, 11, 0, 0, 1, 0, 1, 0, 0, 0, 0, 1, 0];
        let extensions_len = u16::from_be_bytes([wire[offset], wire[offset + 1]]);
        wire[offset..offset + 2]
            .copy_from_slice(&(extensions_len + extension.len() as u16).to_be_bytes());
        wire.extend_from_slice(&extension);
        let record_len = (wire.len() - 5) as u16;
        wire[3..5].copy_from_slice(&record_len.to_be_bytes());
        let handshake_len = (wire.len() - 9) as u32;
        wire[6..9].copy_from_slice(&handshake_len.to_be_bytes()[1..]);
        let error = inspect(wire).await.unwrap_err().to_string();
        assert!(error.contains("Encrypted TLS ClientHello"), "{error}");
    }

    #[tokio::test]
    async fn tls_inspection_bounds_malformed_oversized_and_fragmented_input() {
        for wire in [
            b"GET / HTTP/1.1\r\n\r\n".to_vec(),
            vec![22, 3, 1, 0xff, 0xff],
            vec![22, 3, 1, 0, 0],
            vec![22, 3, 1, 0, 4, 1, 0, 0, 0],
        ] {
            assert!(inspect(wire).await.is_err());
        }
        let original = hello("registry.npmjs.org", true);
        let mut fragments = Vec::new();
        for byte in &original[5..5 + HELLO_RECORD_LIMIT] {
            fragments.extend_from_slice(&[22, 3, 1, 0, 1, *byte]);
        }
        let error = inspect(fragments).await.unwrap_err().to_string();
        assert!(error.contains("too many fragments"), "{error}");
        // A declared large handshake is bounded before allocating/reading its
        // fourth maximum-size record, regardless of its contents.
        let mut oversized = vec![22, 3, 1, 0x40, 0, 1, 0x01, 0, 0];
        oversized.resize(5 + 16 * 1024, 0);
        for _ in 0..2 {
            oversized.extend_from_slice(&[22, 3, 1, 0x40, 0]);
            oversized.resize(oversized.len() + 16 * 1024, 0);
        }
        oversized.extend_from_slice(&[22, 3, 1, 0x40, 0]);
        assert!(inspect(oversized).await.is_err());
    }

    #[tokio::test]
    async fn tls_inspection_times_out_an_incomplete_client_hello() {
        let gateway = Gateway::start().unwrap();
        let mut client = UnixStream::connect(gateway.directory().join("socket"))
            .await
            .unwrap();
        client
            .write_all(b"CONNECT registry.npmjs.org:443 HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        while !response.ends_with(b"\r\n\r\n") {
            response.push(client.read_u8().await.unwrap());
        }
        client.write_all(&[22, 3, 1, 0, 10, 1]).await.unwrap();
        let mut tail = Vec::new();
        tokio::time::timeout(
            HELLO_TIMEOUT + Duration::from_secs(2),
            client.read_to_end(&mut tail),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(tail.is_empty());
        let errors = gateway.close().await;
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("Timed out waiting for TLS ClientHello"));
    }

    #[tokio::test]
    async fn gateway_rejects_a_different_tls_tenant_before_resolving_upstream() {
        let gateway = Gateway::start().unwrap();
        let mut client = UnixStream::connect(gateway.directory().join("socket"))
            .await
            .unwrap();
        client
            .write_all(b"CONNECT registry.npmjs.org:443 HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        while !response.ends_with(b"\r\n\r\n") {
            response.push(client.read_u8().await.unwrap());
        }
        assert!(response.starts_with(b"HTTP/1.1 200"));
        client
            .write_all(&hello("other-tenant.example", true))
            .await
            .unwrap();
        let mut rejection = String::new();
        client.read_to_string(&mut rejection).await.unwrap();
        assert!(
            rejection.is_empty(),
            "TLS clients receive EOF, not plaintext HTTP"
        );
        let errors = gateway.close().await;
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("TLS server name must match"));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn gateway_socket_survives_long_review_paths_and_is_removed() {
        use std::os::fd::AsRawFd;
        let root = tempfile::tempdir().unwrap();
        let long = root.path().join("long-review-directory-".repeat(8));
        std::fs::create_dir(&long).unwrap();
        let gateway = Gateway::start_in(&long).unwrap();
        let dir = gateway.directory().to_owned();
        assert!(dir.starts_with(&long));
        let handle = std::fs::File::open(&dir).unwrap();
        let mut client =
            UnixStream::connect(format!("/proc/self/fd/{}/socket", handle.as_raw_fd()))
                .await
                .unwrap();
        client
            .write_all(b"CONNECT forbidden.invalid:443 HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        tokio::time::timeout(Duration::from_secs(2), client.read_to_string(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert!(response.contains("Package host is not allowed"));
        gateway.close().await;
        assert!(!dir.exists());
    }

    #[tokio::test]
    async fn concurrent_downloads_wait_for_capacity_without_connection_resets() {
        let gateway = Gateway::start().unwrap();
        let socket = gateway.directory().join("socket");
        let mut clients = Vec::new();
        // Partial headers occupy all active tunnels without using the network.
        for _ in 0..16 {
            let mut client = UnixStream::connect(&socket).await.unwrap();
            client.write_all(b"CONNECT ").await.unwrap();
            clients.push(client);
        }
        let mut waiting = UnixStream::connect(&socket).await.unwrap();
        waiting
            .write_all(b"CONNECT forbidden.invalid:443 HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        assert!(
            tokio::time::timeout(
                Duration::from_millis(100),
                waiting.read_to_end(&mut response)
            )
            .await
            .is_err()
        );
        drop(clients.pop());
        tokio::time::timeout(Duration::from_secs(2), waiting.read_to_end(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert!(
            String::from_utf8(response)
                .unwrap()
                .contains("Package host is not allowed")
        );
        gateway.close().await;
    }

    #[test]
    fn destinations_and_addresses_are_restricted() {
        assert_eq!(
            destination("CONNECT registry.npmjs.org:443 HTTP/1.1\r\n\r\n").unwrap(),
            "registry.npmjs.org"
        );
        for host in [
            "127.0.0.1:443",
            "registry.npmjs.org.evil.test:443",
            "registry.npmjs.org:80",
            "user@registry.npmjs.org:443",
            "[::1]:443",
        ] {
            assert!(destination(&format!("CONNECT {host} HTTP/1.1\r\n\r\n")).is_err());
        }
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "169.254.169.254",
            "100.100.100.100",
            "192.168.1.1",
            "0.0.0.0",
            "::1",
            "::ffff:127.0.0.1",
            "fc00::1",
            "2001::1",
            "2001:1ff:ffff::1",
            "2001:db8::1",
            "2002:7f00:1::",
            "3fff::1",
            "3fff:fff:ffff::1",
            "64:ff9b::7f00:1",
        ] {
            assert!(!public(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "2001:4860:4860::8888",
            "2001:200::1",
            "2606:4700:4700::1111",
        ] {
            assert!(public(ip.parse().unwrap()), "{ip}");
        }
        assert!(public("1.1.1.1".parse().unwrap()));
    }
}

#[cfg(test)]
mod gateway_tests {
    use super::*;
    #[tokio::test]
    async fn gateway_rejects_private_hosts_and_removes_socket_on_close() {
        let gateway = Gateway::start().unwrap();
        let path = gateway.directory().join("socket");
        let mut client = UnixStream::connect(&path).await.unwrap();
        client
            .write_all(b"CONNECT 169.254.169.254:443 HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        tokio::time::timeout(Duration::from_secs(2), client.read_to_string(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert!(response.starts_with("HTTP/1.1 403 Forbidden"), "{response}");
        assert!(response.contains("Package host is not allowed"));
        gateway.close().await;
        assert!(!path.exists());
    }
}
