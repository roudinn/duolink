//! Real QUIC over a loopback UDP shaper, with synthetic 144 FPS frame bursts.
//! This measures network queueing, not a video decoder or input-to-photon time.
use super::*;
use crate::{
    bitrate::{Backoff, SAMPLE_INTERVAL},
    media::{lobby_bitrate_limit, stream_wire_kbps},
};
use std::collections::BTreeMap;

#[link(name = "winmm")]
unsafe extern "system" {
    fn timeBeginPeriod(period: u32) -> u32;
    fn timeEndPeriod(period: u32) -> u32;
}

struct Timer;
impl Drop for Timer {
    fn drop(&mut self) { unsafe { timeEndPeriod(1); } }
}

#[test]
fn route_selection_compares_media_rtt() {
    assert_eq!(unsafe { timeBeginPeriod(1) }, 0);
    let _timer = Timer;
    runtime().unwrap().block_on(async {
        let identity = Identity::generate("route QA").unwrap();
        let config = client_config(identity.fingerprint()).unwrap();
        let token = [42; 16];
        let localhost = SocketAddr::from((HOST_LOOPBACK, 0));
        let server = Endpoint::server(server_config(&identity).unwrap(), localhost).unwrap();
        let upstream = server.local_addr().unwrap();
        let mut tasks = tokio::task::JoinSet::new();
        let links = Arc::new(Mutex::new(Vec::new()));
        tasks.spawn(accept(server.clone(), token, links, Arc::new(ProbeGate::default())));

        let mut candidates = Vec::new();
        for (kind, propagation, startup_delay) in [
            (PathKind::Forwarded, Duration::from_millis(9), Duration::ZERO),
            (PathKind::Public, Duration::from_millis(4), Duration::from_millis(180)),
        ] {
            let proxy = Arc::new(UdpSocket::bind(localhost).await.unwrap());
            let SocketAddr::V4(addr) = proxy.local_addr().unwrap() else { unreachable!() };
            candidates.push(Candidate { kind, addr });
            tasks.spawn(async move {
                let mut ready_at = None;
                let mut guest = None;
                let mut buffer = [0; 4096];
                let mut deliveries = tokio::task::JoinSet::new();
                loop {
                    let (len, from) = proxy.recv_from(&mut buffer).await.unwrap();
                    let to = if from == upstream {
                        let Some(guest) = guest else { continue };
                        guest
                    } else {
                        guest = Some(from);
                        upstream
                    };
                    let now = tokio::time::Instant::now();
                    let ready_at = *ready_at.get_or_insert(now + startup_delay);
                    let due = now.max(ready_at) + propagation;
                    let data = buffer[..len].to_vec();
                    let socket = proxy.clone();
                    deliveries.spawn(async move {
                        tokio::time::sleep_until(due).await;
                        let _ = socket.send_to(&data, to).await;
                    });
                    while deliveries.try_join_next().is_some() {}
                }
            });
        }
        let client = Endpoint::client(localhost).unwrap();
        let reflexive = Reflexive::default();
        let first = client.connect_with(config.clone(), candidates[0].addr.into(), "duolink")
            .unwrap().await.unwrap();
        authenticate(&first, token).await.unwrap();
        let (echoes, slow_rtt) = measure_route(&first).await;
        assert_eq!(echoes, ROUTE_PINGS);
        first.close(VarInt::from_u32(0), b"sampled");

        let winner = choose_route(&client, config, token, &candidates, &reflexive, || Ok(()))
            .await.unwrap();
        assert_eq!(winner.kind, PathKind::Public);
        assert_eq!(winner.echo_count, ROUTE_PINGS);
        assert!(winner.rtt + Duration::from_millis(5) < slow_rtt);
        eprintln!("first path {:.2} ms RTT, selected {:.2} ms RTT",
            slow_rtt.as_secs_f64() * 1_000.0, winner.rtt.as_secs_f64() * 1_000.0);
    });
}

#[derive(Default)]
struct Samples {
    packets: Vec<f64>,
    frames: Vec<f64>,
    gaps: Vec<f64>,
    incomplete: usize,
}

fn quantile(values: &[f64], q: f64) -> f64 {
    if values.is_empty() { return f64::NAN; }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    sorted[((sorted.len() - 1) as f64 * q).round() as usize]
}

async fn shaped_run(
    rates: &[u32],
    loss_every: u64,
    capacity: Arc<AtomicU64>,
    backoff_rate: Option<Arc<AtomicU64>>,
) -> Samples {
    let identity = Identity::generate("shaped QA").unwrap();
    let server = Endpoint::server(server_config(&identity).unwrap(), (HOST_LOOPBACK, 0).into()).unwrap();
    let upstream = server.local_addr().unwrap();
    let origin = Instant::now();
    let queue = Arc::new(tokio::sync::Mutex::new(origin));
    let dropped = Arc::new(AtomicU64::new(0));
    let packet_number = Arc::new(AtomicU64::new(0));
    let mut tasks = tokio::task::JoinSet::new();
    let mut endpoints = Vec::new();
    let mut connections = Vec::new();
    for _ in rates {
        let proxy = Arc::new(UdpSocket::bind((HOST_LOOPBACK, 0)).await.unwrap());
        let address = proxy.local_addr().unwrap();
        let queue = queue.clone();
        let dropped = dropped.clone();
        let packet_number = packet_number.clone();
        let capacity = capacity.clone();
        tasks.spawn(async move {
            let mut peer = None;
            let mut buffer = [0u8; 65536];
            let mut deliveries = tokio::task::JoinSet::new();
            loop {
                let (len, from) = proxy.recv_from(&mut buffer).await.unwrap();
                if from != upstream {
                    peer = Some(from);
                    proxy.send_to(&buffer[..len], upstream).await.unwrap();
                    continue;
                }
                let Some(peer) = peer else { continue };
                let now = Instant::now();
                let mut tail = queue.lock().await;
                // Shared host upload, 250 ms tail-drop queue, 5 ms path.
                if *tail > now + Duration::from_millis(250) {
                    dropped.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                let bps = capacity.load(Ordering::Relaxed) as f64;
                *tail = (*tail).max(now) + Duration::from_secs_f64((len + 28) as f64 * 8.0 / bps);
                let due = *tail + Duration::from_millis(5);
                drop(tail);
                let sequence = packet_number.fetch_add(1, Ordering::Relaxed);
                if loss_every > 0 && origin.elapsed() > Duration::from_secs(1) && sequence.is_multiple_of(loss_every) {
                    dropped.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                let bytes = buffer[..len].to_vec();
                let socket = proxy.clone();
                deliveries.spawn(async move {
                    tokio::time::sleep_until(due.into()).await;
                    let _ = socket.send_to(&bytes, peer).await;
                });
                while deliveries.try_join_next().is_some() {}
            }
        });
        let client = Endpoint::client((HOST_LOOPBACK, 0).into()).unwrap();
        let connecting = client.connect_with(client_config(identity.fingerprint()).unwrap(), address, "duolink").unwrap();
        let incoming = server.accept().await.unwrap();
        let (guest, host) = tokio::join!(connecting, incoming);
        connections.push((host.unwrap(), guest.unwrap()));
        endpoints.push(client);
    }
    let start = Instant::now() + Duration::from_millis(100);
    if let Some(chosen) = &backoff_rate {
        let (host, _) = connections[0].clone();
        let pinger = Pinger::new();
        // The receiver below echoes pings and updates this same host clock.
        tasks.spawn(ping_loop(host.clone(), pinger.clone()));
        let reader = pinger.clone();
        let stats = host.clone();
        tasks.spawn(async move {
            while let Ok(packet) = host.read_datagram().await {
                if packet.starts_with(&PONG_PORT.to_be_bytes()) {
                    reader.record(&packet[2..]);
                }
            }
        });
        let chosen = chosen.clone();
        let initial = rates[0];
        tasks.spawn(async move {
            let mut policy = Backoff::new(start, None);
            loop {
                tokio::time::sleep(SAMPLE_INTERVAL).await;
                if let Some(kbps) = policy.sample(Instant::now(), UploadSample::read(&stats, &pinger), initial) {
                    chosen.store(u64::from(kbps), Ordering::Relaxed);
                    eprintln!("upload backoff at {:.1}s: {initial} → {kbps} kbps", start.elapsed().as_secs_f64());
                    break;
                }
            }
        });
    }
    let mut receiver_tasks = Vec::new();
    for (i, ((host, guest), rate)) in connections.into_iter().zip(rates.iter().copied()).enumerate() {
        let chosen = backoff_rate.clone();
        receiver_tasks.push(tokio::spawn(async move {
            let mut result = Samples::default();
            let mut assembling = BTreeMap::<u32, (u16, u16, u64)>::new();
            let mut previous = None::<Instant>;
            let deadline = start + Duration::from_secs(7);
            while let Ok(Ok(packet)) = tokio::time::timeout_at(deadline.into(), guest.read_datagram()).await {
                if packet.starts_with(&PING_PORT.to_be_bytes()) && packet.len() == 10 {
                    forward(&guest, PONG_PORT, &packet[2..]);
                    continue;
                }
                if packet.len() < 16 { continue; }
                let frame = u32::from_be_bytes(packet[0..4].try_into().unwrap());
                let count = u16::from_be_bytes(packet[6..8].try_into().unwrap());
                let sent = u64::from_be_bytes(packet[8..16].try_into().unwrap());
                if sent < 2_000_000 { continue; }
                let now = Instant::now();
                let age = now.duration_since(start).as_secs_f64() * 1000.0 - sent as f64 / 1000.0;
                result.packets.push(age);
                let entry = assembling.entry(frame).or_insert((0, count, sent));
                entry.0 += 1;
                if entry.0 == entry.1 {
                    result.frames.push(age);
                    if let Some(previous) = previous { result.gaps.push(now.duration_since(previous).as_secs_f64() * 1000.0); }
                    previous = Some(now);
                    assembling.remove(&frame);
                }
            }
            result.incomplete = assembling.len();
            guest.close(VarInt::from_u32(0), b"done");
            (i, result)
        }));
        tasks.spawn(async move {
            // The first stream is established before the late arrivals.
            let offset = if i == 0 { Duration::ZERO } else { Duration::from_secs(1) + Duration::from_micros(i as u64 * 1700) };
            let mut frames = tokio::time::interval_at((start + offset).into(), Duration::from_secs_f64(1.0 / 144.0));
            frames.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut frame = 0u32;
            while Instant::now() < start + Duration::from_secs(6) {
                frames.tick().await;
                let bitrate = chosen.as_ref().map_or(rate, |chosen| {
                    match chosen.load(Ordering::Relaxed) {
                        0 => rate,
                        kbps => kbps as u32,
                    }
                });
                let bytes = (bitrate as usize * 1000 / 8 / 144).max(16);
                let count = bytes.div_ceil(1100) as u16;
                let sent = start.elapsed().as_micros() as u64;
                for part in 0..count {
                    let mut payload = vec![0u8; (bytes.saturating_sub(part as usize * 1100)).clamp(16, 1100)];
                    payload[0..4].copy_from_slice(&frame.to_be_bytes());
                    payload[4..6].copy_from_slice(&part.to_be_bytes());
                    payload[6..8].copy_from_slice(&count.to_be_bytes());
                    payload[8..16].copy_from_slice(&sent.to_be_bytes());
                    let _ = host.send_datagram(Bytes::from(payload));
                }
                frame += 1;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        });
    }
    let mut first = None;
    for task in receiver_tasks {
        let (i, sample) = task.await.unwrap();
        eprintln!("guest={i}, packet_ms p50={:.3} p95={:.3} p99={:.3}, frame_ms p50={:.3} p95={:.3} p99={:.3}, gaps>50ms={}, complete={}, partial={}",
            quantile(&sample.packets, 0.5), quantile(&sample.packets, 0.95), quantile(&sample.packets, 0.99),
            quantile(&sample.frames, 0.5), quantile(&sample.frames, 0.95), quantile(&sample.frames, 0.99),
            sample.gaps.iter().filter(|&&gap| gap > 50.0).count(), sample.frames.len(), sample.incomplete);
        if i == 0 { first = Some(sample); }
    }
    eprintln!("shaper dropped={} UDP packets (queue + configured loss)", dropped.load(Ordering::Relaxed));
    tasks.abort_all();
    first.unwrap()
}

#[test]
#[ignore = "local QUIC shaper benchmark; run alone, about 30 seconds"]
fn constrained_shared_upload() {
    // The emulated link's sleeps need millisecond resolution. This is a QA
    // process setting, not a dependency of DuoLink's event-driven forwarding.
    assert_eq!(unsafe { timeBeginPeriod(1) }, 0);
    let _timer = Timer;
    runtime().unwrap().block_on(async {
        let first = lobby_bitrate_limit(Some(12_000), 0, 256);
        let mut reserved = stream_wire_kbps(first, 256);
        let mut after = vec![reserved];
        for _ in 0..2 {
            let rate = lobby_bitrate_limit(Some(12_000), reserved, 256);
            if rate > 0 { let wire = stream_wire_kbps(rate, 256); after.push(wire); reserved += wire; }
        }
        assert!(reserved <= 7_200);
        for loss_every in [0, 100] {
            eprintln!("BEFORE v1.12.3 policy: 4200 + 15000 + 15000 kbps; loss_every={loss_every}");
            let capacity = Arc::new(AtomicU64::new(12_000_000));
            let before = shaped_run(
                &[stream_wire_kbps(4200, 256), stream_wire_kbps(15000, 256), stream_wire_kbps(15000, 256)],
                loss_every, capacity.clone(), None,
            ).await;
            eprintln!("AFTER: wire rates {after:?}, one late guest refused; loss_every={loss_every}");
            let after = shaped_run(&after, loss_every, capacity, None).await;
            assert!(before.packets.len() > 100 && after.packets.len() > 100);
            assert!(quantile(&after.packets, 0.95) < quantile(&before.packets, 0.95) / 2.0);
        }
    });
}

#[test]
#[ignore = "uses real QUIC over a local link with an upload drop"]
fn live_upload_backoff() {
    assert_eq!(unsafe { timeBeginPeriod(1) }, 0);
    let _timer = Timer;
    runtime().unwrap().block_on(async {
        let capacity = Arc::new(AtomicU64::new(24_000_000));
        let change = capacity.clone();
        let capacity_drop = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(2)).await;
            change.store(6_000_000, Ordering::Relaxed);
        });
        let bitrate = Arc::new(AtomicU64::new(0));
        shaped_run(&[12_000], 0, capacity, Some(bitrate.clone())).await;
        capacity_drop.await.unwrap();
        let bitrate = bitrate.load(Ordering::Relaxed);
        assert!((500..6_000).contains(&bitrate), "expected a rate below the new capacity, got {bitrate}");
    });
}
