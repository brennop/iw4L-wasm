//! WebTransport smoke client for the master's second listener, no game needed.
//!
//!   cargo run -p iw4l-master --example wt_smoke -- list  PATH/webtransport.json
//!   cargo run -p iw4l-master --example wt_smoke -- e2e   PATH/webtransport.json
//!
//! `list` connects with the certificate hash from the JSON, sends `Hello`
//! (role `Cli`) and `ListRooms`, and prints the rooms. `e2e` runs a host and a
//! member over two WebTransport sessions: create, join, member -> host
//! datagram, host -> member datagram, member -> host uni-stream bootstrap.

use std::time::Duration;

use master_protocol::{
    ContentFlags, ControlFrame, ControlHello, ControlRequest, EndpointRole, MAX_OPAQUE_PAYLOAD,
    RelayDatagram, RequestBody, ResponseBody, decode_relay, decode_relay_stream,
    decode_stream_payload, encode_relay, encode_relay_stream, encode_stream_frame,
    stream_frame_len,
};
use wtransport::tls::Sha256Digest;
use wtransport::{ClientConfig, Connection, Endpoint, RecvStream, SendStream};

type Error = Box<dyn std::error::Error + Send + Sync>;
type Result<T> = std::result::Result<T, Error>;

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mode = args
        .next()
        .ok_or("usage: wt_smoke list|e2e webtransport.json")?;
    let json = std::fs::read_to_string(args.next().ok_or("missing webtransport.json")?)?;
    let hash = field(&json, "hash_hex")?;
    let port: u16 = field_raw(&json, "port")?.parse()?;
    let digest: [u8; 32] = (0..32)
        .map(|i| u8::from_str_radix(&hash[i * 2..i * 2 + 2], 16))
        .collect::<std::result::Result<Vec<_>, _>>()?
        .try_into()
        .map_err(|_| "hash is not 32 bytes")?;
    let url = format!("https://127.0.0.1:{port}");
    match mode.as_str() {
        "list" => list(&url, digest).await,
        "e2e" => e2e(&url, digest).await,
        _ => Err("mode must be list or e2e".into()),
    }
}

fn field_raw(json: &str, key: &str) -> Result<String> {
    let rest = json
        .split(&format!("\"{key}\":"))
        .nth(1)
        .ok_or_else(|| format!("no {key} in json"))?;
    Ok(rest
        .split([',', '\n'])
        .next()
        .unwrap_or("")
        .trim()
        .to_owned())
}

fn field(json: &str, key: &str) -> Result<String> {
    Ok(field_raw(json, key)?.trim_matches('"').to_owned())
}

async fn connect(
    url: &str,
    digest: [u8; 32],
) -> Result<(
    Endpoint<wtransport::endpoint::endpoint_side::Client>,
    Connection,
)> {
    let config = ClientConfig::builder()
        .with_bind_default()
        .with_server_certificate_hashes([Sha256Digest::new(digest)])
        .build();
    let endpoint = Endpoint::client(config)?;
    let connection = endpoint.connect(url).await?;
    println!(
        "connected {url} rtt={:?} max_datagram_size={:?}",
        connection.rtt(),
        connection.max_datagram_size()
    );
    Ok((endpoint, connection))
}

async fn write_frame(send: &mut SendStream, frame: &ControlFrame) -> Result<()> {
    send.write_all(&encode_stream_frame(frame)?).await?;
    Ok(())
}

async fn read_frame(recv: &mut RecvStream) -> Result<ControlFrame> {
    let mut header = [0_u8; 4];
    recv.read_exact(&mut header).await?;
    let mut body = vec![0_u8; stream_frame_len(header)?];
    recv.read_exact(&mut body).await?;
    Ok(decode_stream_payload(&body)?)
}

fn hello(role: EndpointRole, name: &str) -> ControlFrame {
    ControlFrame::Hello(ControlHello {
        protocol_version: master_protocol::PROTOCOL_VERSION,
        game_protocol: 0,
        role,
        build: "wt_smoke".into(),
        player_name: name.into(),
    })
}

async fn call(
    send: &mut SendStream,
    recv: &mut RecvStream,
    request_id: u64,
    body: RequestBody,
) -> Result<ResponseBody> {
    write_frame(
        send,
        &ControlFrame::Request(ControlRequest { request_id, body }),
    )
    .await?;
    loop {
        match read_frame(recv).await? {
            ControlFrame::Response(r) if r.request_id == request_id => return Ok(r.body),
            other => println!("  (skipped frame while waiting: {other:?})"),
        }
    }
}

async fn list(url: &str, digest: [u8; 32]) -> Result<()> {
    let (_endpoint, connection) = connect(url, digest).await?;
    let (mut send, mut recv) = connection.open_bi().await?.await?;
    write_frame(&mut send, &hello(EndpointRole::Cli, "")).await?;
    match call(&mut send, &mut recv, 1, RequestBody::ListRooms).await? {
        ResponseBody::RoomList {
            generation,
            adverts,
        } => {
            println!("generation={generation} adverts={}", adverts.len());
            for a in adverts {
                println!(
                    "{}\t{}/{}\t{}\t{}\tin_match={}\t{}",
                    a.id,
                    a.players,
                    a.max_players,
                    a.map,
                    a.mode,
                    u8::from(a.in_match),
                    a.name
                );
            }
            Ok(())
        }
        other => Err(format!("unexpected response {other:?}").into()),
    }
}

async fn e2e(url: &str, digest: [u8; 32]) -> Result<()> {
    let (_e1, host) = connect(url, digest).await?;
    let (mut hs, mut hr) = host.open_bi().await?.await?;
    write_frame(&mut hs, &hello(EndpointRole::Host, "wt-host")).await?;
    let ResponseBody::RoomCreated { view, .. } = call(
        &mut hs,
        &mut hr,
        1,
        RequestBody::CreateRoom {
            name: "wt-smoke".into(),
            map: "mp_rust".into(),
            mode: "war".into(),
            max_players: 4,
            requires: ContentFlags(0),
            available: ContentFlags(0),
            password: String::new(),
        },
    )
    .await?
    else {
        return Err("CreateRoom failed".into());
    };
    println!("host created room {}", view.room_id);

    let (_e2, member) = connect(url, digest).await?;
    let (mut ms, mut mr) = member.open_bi().await?.await?;
    write_frame(&mut ms, &hello(EndpointRole::Join, "wt-member")).await?;
    let ResponseBody::RoomJoined { member_id, view } = call(
        &mut ms,
        &mut mr,
        1,
        RequestBody::JoinRoom {
            room_id: view.room_id,
            have: ContentFlags(0),
            password: String::new(),
        },
    )
    .await?
    else {
        return Err("JoinRoom failed".into());
    };
    println!(
        "member joined as {member_id}, members={}",
        view.members.len()
    );

    // member -> host datagram at the payload ceiling
    let payload = vec![0xAB_u8; MAX_OPAQUE_PAYLOAD];
    let wire = encode_relay(RelayDatagram::ClientToHost(&payload))?;
    println!(
        "envelope: {} B payload + {} B header = {} B",
        payload.len(),
        wire.len() - payload.len(),
        wire.len()
    );
    member.send_datagram(&wire)?;
    let got = tokio::time::timeout(Duration::from_secs(3), host.receive_datagram()).await??;
    let wire_in = got.payload();
    let RelayDatagram::ServiceToHost {
        member_id: from,
        payload: p,
    } = decode_relay(&wire_in)?
    else {
        return Err("host got a datagram that is not ServiceToHost".into());
    };
    assert_eq!(from, member_id);
    assert_eq!(p, payload.as_slice());
    println!(
        "member -> host datagram ok ({} B payload, wire {} B)",
        p.len(),
        wire_in.len()
    );

    // host -> member datagram
    let reply = vec![0xCD_u8; 600];
    host.send_datagram(&encode_relay(RelayDatagram::HostToMember {
        member_id,
        payload: &reply,
    })?)?;
    let got = tokio::time::timeout(Duration::from_secs(3), member.receive_datagram()).await??;
    let wire_in = got.payload();
    let RelayDatagram::ServiceToMember(p) = decode_relay(&wire_in)? else {
        return Err("member got a datagram that is not ServiceToMember".into());
    };
    assert_eq!(p, reply.as_slice());
    println!("host -> member datagram ok ({} B payload)", p.len());

    // member -> host bootstrap over a uni stream (relay forwards on a fresh uni stream)
    let blob = vec![0x5A_u8; 200_000];
    let mut up = member.open_uni().await?.await?;
    up.write_all(&encode_relay_stream(RelayDatagram::ClientToHost(&blob))?)
        .await?;
    up.finish().await?;
    let mut down = tokio::time::timeout(Duration::from_secs(5), host.accept_uni()).await??;
    let mut bytes = Vec::new();
    let mut buf = [0_u8; 16 * 1024];
    while let Some(n) = down.read(&mut buf).await? {
        bytes.extend_from_slice(&buf[..n]);
    }
    let RelayDatagram::ServiceToHost { payload: p, .. } = decode_relay_stream(&bytes)? else {
        return Err("host got a uni stream that is not ServiceToHost".into());
    };
    assert_eq!(p, blob.as_slice());
    println!("member -> host uni bootstrap ok ({} B)", p.len());

    member.close(0_u8.into(), b"done");
    host.close(0_u8.into(), b"done");
    println!("e2e ok");
    Ok(())
}
