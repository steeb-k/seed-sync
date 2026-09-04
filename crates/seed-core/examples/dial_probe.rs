//! Dial one remote endpoint from a throwaway endpoint and report exactly what
//! iroh knew and tried: discovered addresses (n0 DNS, mDNS), the relay URL,
//! which paths ended up active, and the connect outcome.
//!
//! Field-diagnostic counterpart of `relay_probe`: answers "can THIS box reach
//! THAT member at all, and over which path?" independently of the daemon's
//! own state (roster, ladders, relay-map flip-flops).
//!
//! Usage:
//!     cargo run -p seed-core --example dial_probe -- <endpoint-id-hex> \
//!         [--relay-settings-db <state.db>] [--ip <sock-addr>] [--alpn docs|gossip|blobs] \
//!         [--timeout <secs>] [--hold <secs>]
//!
//! `--relay-settings-db` reads the daemon's persisted relay settings (custom
//! relay + token) so the probe endpoint is configured like the daemon; without
//! it the probe uses the public n0 relays. The token never touches the command
//! line or the output.

use std::time::Duration;

use anyhow::{bail, Context};
use iroh::{endpoint::presets, Endpoint, EndpointAddr, EndpointId, TransportAddr, Watcher as _};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();

    let mut args = std::env::args().skip(1);
    let id_hex = args
        .next()
        .context("usage: dial_probe <endpoint-id-hex> [...]")?;
    let peer: EndpointId = id_hex
        .parse()
        .context("endpoint id must be a 64-char hex endpoint id")?;

    let mut settings_db: Option<String> = None;
    let mut ips: Vec<std::net::SocketAddr> = Vec::new();
    let mut junk: u32 = 0;
    let mut alpn: &[u8] = iroh_docs::ALPN;
    let mut timeout = 30u64;
    let mut hold = 5u64;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--relay-settings-db" => settings_db = Some(args.next().context("value")?),
            "--ip" => ips.push(args.next().context("value")?.parse()?),
            "--junk" => junk = args.next().context("value")?.parse()?,
            "--alpn" => {
                alpn = match args.next().context("value")?.as_str() {
                    "docs" => iroh_docs::ALPN,
                    "gossip" => iroh_gossip::ALPN,
                    "blobs" => iroh_blobs::ALPN,
                    other => bail!("unknown alpn {other}"),
                }
            }
            "--timeout" => timeout = args.next().context("value")?.parse()?,
            "--hold" => hold = args.next().context("value")?.parse()?,
            other => bail!("unknown arg: {other}"),
        }
    }

    let mut builder = Endpoint::builder(presets::N0);
    if let Some(path) = &settings_db {
        let conn =
            rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .context("open state.db read-only")?;
        let v: String = conn
            .query_row(
                "SELECT v FROM settings WHERE k = 'relay_settings'",
                [],
                |r| r.get(0),
            )
            .context("read relay_settings")?;
        let settings: seed_core::relays::RelaySettings =
            serde_json::from_str(&v).context("parse relay settings")?;
        let configs = seed_core::relays::relay_configs(&settings)?;
        println!(
            "relay mode: CUSTOM ({} server(s): {}) - like the daemon",
            configs.len(),
            configs
                .iter()
                .map(|c| format!(
                    "{}{}",
                    c.url,
                    if c.auth_token.is_some() {
                        " [token]"
                    } else {
                        ""
                    }
                ))
                .collect::<Vec<_>>()
                .join(", ")
        );
        builder = builder.relay_mode(iroh::RelayMode::Custom(iroh::RelayMap::from_iter(configs)));
    } else {
        println!("relay mode: DEFAULT (public n0 relays)");
    }
    // Same LAN discovery as node.rs.
    let secret = iroh::SecretKey::generate();
    let self_id = secret.public();
    builder = builder.secret_key(secret);
    match iroh_mdns_address_lookup::MdnsAddressLookup::builder().build(self_id) {
        Ok(mdns) => builder = builder.address_lookup(mdns),
        Err(e) => println!("mDNS unavailable: {e}"),
    }
    let ep = builder.bind().await.context("bind")?;
    println!("probe endpoint id: {}", ep.id());

    let online = tokio::time::timeout(Duration::from_secs(15), ep.online())
        .await
        .is_ok();
    let me = ep.addr();
    println!(
        "online: {online}; our addr: relays={:?} ips={:?}",
        me.relay_urls().map(|u| u.to_string()).collect::<Vec<_>>(),
        me.ip_addrs().collect::<Vec<_>>()
    );
    if let Some(report) = ep.net_report().get() {
        println!(
            "net-report: udp_v4={} udp_v6={} preferred_relay={:?}",
            report.udp_v4, report.udp_v6, report.preferred_relay
        );
    }

    // Give mDNS a moment to hear the LAN before dialing, then show what
    // discovery already knows about the peer (before any dial).
    tokio::time::sleep(Duration::from_secs(3)).await;
    print_remote(&ep, peer, "before dial").await;

    let mut addr = EndpointAddr::new(peer);
    for ip in &ips {
        addr = addr.with_ip_addr(*ip);
        println!("dialing with explicit ip hint {ip}");
    }
    // Unroutable candidate addresses (TEST-NET-3 + a dead corner of the
    // overlay), to emulate a remote whose advertised set has gone stale.
    for i in 0..junk {
        let a: std::net::SocketAddr = if i % 2 == 0 {
            format!("203.0.113.{}:4242", 1 + i / 2).parse().unwrap()
        } else {
            format!("10.99.0.{}:42730", 200 + i / 2).parse().unwrap()
        };
        addr = addr.with_ip_addr(a);
    }
    if junk > 0 {
        println!("added {junk} unroutable junk address hints");
    }
    println!(
        "dialing {} (alpn {:?}) with {}s timeout ...",
        peer.fmt_short(),
        String::from_utf8_lossy(alpn),
        timeout
    );
    let started = std::time::Instant::now();
    let res = tokio::time::timeout(Duration::from_secs(timeout), ep.connect(addr, alpn)).await;
    match res {
        Ok(Ok(conn)) => {
            println!(
                "CONNECTED in {:.1}s (remote {})",
                started.elapsed().as_secs_f32(),
                conn.remote_id().fmt_short()
            );
            for i in 0..hold {
                tokio::time::sleep(Duration::from_secs(1)).await;
                print_remote(&ep, peer, &format!("connected +{}s", i + 1)).await;
            }
            conn.close(0u32.into(), b"probe done");
        }
        Ok(Err(e)) => {
            println!(
                "CONNECT FAILED after {:.1}s: {e:#}",
                started.elapsed().as_secs_f32()
            );
            print_remote(&ep, peer, "after failure").await;
        }
        Err(_) => {
            println!("CONNECT TIMED OUT after {timeout}s");
            print_remote(&ep, peer, "after timeout").await;
        }
    }
    ep.close().await;
    Ok(())
}

async fn print_remote(ep: &Endpoint, peer: EndpointId, label: &str) {
    match ep.remote_info(peer).await {
        None => println!("  remote_info [{label}]: nothing known"),
        Some(info) => {
            let addrs: Vec<String> = info
                .addrs()
                .map(|a| {
                    let kind = match a.addr() {
                        TransportAddr::Ip(s) => format!("ip {s}"),
                        TransportAddr::Relay(u) => format!("relay {u}"),
                        other => format!("{other:?}"),
                    };
                    format!("{kind} [{:?}]", a.usage())
                })
                .collect();
            println!("  remote_info [{label}]: {}", addrs.join(" | "));
        }
    }
}
