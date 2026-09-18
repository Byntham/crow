//! Package download gateway. Only this broker has network access; containers do not.
use anyhow::{Context, Result, ensure};
use std::{net::IpAddr, path::Path, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpStream, UnixListener, UnixStream},
    sync::Semaphore,
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
        // Restrict IPv6 to global unicast. Exclude translation, documentation and special ranges.
        IpAddr::V6(ip) => {
            let s = ip.segments();
            (s[0] & 0xe000) == 0x2000 && s[0] != 0x2001 && s[0] != 0x2002 && s[0] != 0x3fff
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
async fn tunnel(client: &mut UnixStream) -> Result<()> {
    let mut header = Vec::new();
    while !header.ends_with(b"\r\n\r\n") {
        ensure!(header.len() < 8192, "Proxy header too large");
        header.push(client.read_u8().await?);
    }
    let request = std::str::from_utf8(&header)?;
    let host = destination(request)?;
    let addresses: Vec<_> = tokio::net::lookup_host((host, 443)).await?.collect();
    ensure!(
        !addresses.is_empty() && addresses.iter().all(|a| public(a.ip())),
        "Package host resolved to a non-public address"
    );
    // Connect to the validated address, never resolve the hostname again.
    let mut remote = TcpStream::connect(addresses.as_slice()).await?;
    client
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await?;
    let (cr, cw) = client.split();
    let (rr, rw) = remote.split();
    // Bound bytes in each direction as well as lifetime and concurrency.
    let mut upload = cr.take(16 * 1024 * 1024);
    let mut download = rr.take(256 * 1024 * 1024);
    let mut rw = rw;
    let mut cw = cw;
    tokio::select! {result=tokio::io::copy(&mut upload, &mut rw)=>{result?;}, result=tokio::io::copy(&mut download, &mut cw)=>{result?;}}
    Ok(())
}

pub struct Gateway {
    stop: CancellationToken,
    task: tokio::task::JoinHandle<()>,
    _directory: tempfile::TempDir,
}
impl Gateway {
    pub fn start() -> Result<Self> {
        let directory = tempfile::Builder::new().prefix("downloads-").tempdir()?;
        let listener = UnixListener::bind(directory.path().join("socket"))?;
        let stop = CancellationToken::new();
        let cancelled = stop.clone();
        let task = tokio::spawn(async move {
            let slots = Arc::new(Semaphore::new(16));
            let mut tasks = tokio::task::JoinSet::new();
            loop {
                // Leave excess connections in the bounded socket backlog until a
                // tunnel finishes. Accepting and dropping them makes concurrent
                // package managers see connection resets and retry whole batches.
                let permit = tokio::select! {
                    _ = cancelled.cancelled() => break,
                    permit = slots.clone().acquire_owned() => {
                        let Ok(permit) = permit else {break};
                        permit
                    }
                };
                tokio::select! {
                    _ = cancelled.cancelled() => break,
                    Some(_) = tasks.join_next(), if !tasks.is_empty() => {},
                    incoming = listener.accept() => {
                        let Ok((mut stream,_)) = incoming else {break};
                        tasks.spawn(async move {
                            let _permit = permit;
                            if let Ok(Err(error)) = tokio::time::timeout(Duration::from_secs(120), tunnel(&mut stream)).await {
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
            _directory: directory,
        })
    }
    pub fn directory(&self) -> &Path {
        self._directory.path()
    }
    pub async fn close(self) {
        self.stop.cancel();
        // Drop aborts the listener too; yield until all active tunnels have been dropped.
        while !self.task.is_finished() {
            tokio::task::yield_now().await;
        }
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
            "2001:db8::1",
            "2002:7f00:1::",
        ] {
            assert!(!public(ip.parse().unwrap()), "{ip}");
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
