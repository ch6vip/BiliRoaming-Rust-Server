//! Real TLS and HTTP/2 wire tests. All peers use local, temporary endpoints.
use actix_web::{web, App, HttpResponse, HttpServer};
use biliroaming_rust_server::mods::config::sslconfig_from_readers;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[test]
fn md5_protocol_signatures_match_independent_known_vectors() {
    assert_eq!(
        biliroaming_rust_server::calc_md5!(""),
        "d41d8cd98f00b204e9800998ecf8427e"
    );
    assert_eq!(
        biliroaming_rust_server::calc_md5!("abc"),
        "900150983cd24fb0d6963f7d28e17f72"
    );
    assert_eq!(
        biliroaming_rust_server::calc_md5!("message digest"),
        "f96b697d7cb7938d525a2f31aaf161d0"
    );
}

#[actix_web::test]
async fn tls_handshake_verifies_certificates_and_negotiates_http2() {
    let generated = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let cert = generated.cert.pem();
    let key = generated.signing_key.serialize_pem();
    let tls = sslconfig_from_readers(&mut cert.as_bytes(), &mut key.as_bytes()).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!(
        "https://localhost:{}/",
        listener.local_addr().unwrap().port()
    );
    let server = HttpServer::new(|| {
        App::new().route(
            "/",
            web::get().to(|| async { HttpResponse::Ok().body("tls-ok") }),
        )
    })
    .workers(1)
    .listen_rustls_0_23(listener, tls)
    .unwrap()
    .run();
    let handle = server.handle();
    let task = actix_web::rt::spawn(server);
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .add_root_certificate(reqwest::Certificate::from_pem(cert.as_bytes()).unwrap())
        .build()
        .unwrap();
    let result = client.get(&url).send().await;
    // Always shut down the temporary listener, including when an assertion fails.
    let untrusted = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
        .get(&url)
        .send()
        .await;
    handle.stop(false).await;
    task.await.unwrap().unwrap();
    let response = result.unwrap();
    assert_eq!(response.version(), reqwest::Version::HTTP_2);
    assert_eq!(response.text().await.unwrap(), "tls-ok");
    assert!(
        untrusted.is_err(),
        "An untrusted certificate must never be accepted"
    );
}

fn frame(kind: u8, flags: u8, stream: u32, payload: &[u8]) -> Vec<u8> {
    let length = payload.len() as u32;
    let mut bytes = vec![
        (length >> 16) as u8,
        (length >> 8) as u8,
        length as u8,
        kind,
        flags,
    ];
    bytes.extend_from_slice(&stream.to_be_bytes());
    bytes.extend_from_slice(payload);
    bytes
}

async fn read_frame(peer: &mut tokio::io::DuplexStream) -> (u8, Vec<u8>) {
    let mut header = [0; 9];
    peer.read_exact(&mut header).await.unwrap();
    let length = ((header[0] as usize) << 16) | ((header[1] as usize) << 8) | header[2] as usize;
    let mut payload = vec![0; length];
    peer.read_exact(&mut payload).await.unwrap();
    (header[3], payload)
}

fn request_headers() -> Vec<u8> {
    // HPACK static indexes: :method GET, :scheme http, :path /.
    frame(1, 4, 1, &[0x82, 0x86, 0x84])
}

async fn excessive_frames_are_rejected(payload: &[u8], flags: u8) {
    let (mut peer, transport) = tokio::io::duplex(65536);
    let server = tokio::spawn(async move {
        let mut connection = h2_legacy::server::handshake(transport).await.unwrap();
        let _unconsumed = match connection.accept().await.unwrap() {
            Ok(stream) => stream,
            Err(error) => return error,
        };
        // The flood must surface as a connection error, not an extra stream.
        match connection.accept().await {
            Some(Err(error)) => error,
            None => panic!("Connection closed without reporting excessive frames"),
            Some(Ok(_)) => panic!("Unexpected additional stream"),
        }
    });
    peer.write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n")
        .await
        .unwrap();
    peer.write_all(&frame(4, 0, 0, &[])).await.unwrap();
    peer.write_all(&request_headers()).await.unwrap();
    for _ in 0..101 {
        peer.write_all(&frame(0, flags, 1, payload)).await.unwrap();
    }
    let goaway = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let (kind, payload) = read_frame(&mut peer).await;
            if kind == 7 {
                break payload;
            }
        }
    })
    .await;
    if goaway.is_err() {
        server.abort();
    }
    let goaway = goaway.expect("Flood must produce GOAWAY rather than grow the receive queue");
    assert_eq!(u32::from_be_bytes(goaway[4..8].try_into().unwrap()), 11);
    assert_eq!(&goaway[8..], b"too_many_data_frames");
    assert_eq!(
        server.await.unwrap().reason(),
        Some(h2_legacy::Reason::ENHANCE_YOUR_CALM)
    );
}

#[tokio::test]
async fn legacy_h2_rejects_empty_small_and_padded_frame_floods() {
    excessive_frames_are_rejected(&[], 0).await;
    excessive_frames_are_rejected(b"x", 0).await;
    excessive_frames_are_rejected(&[1, 0], 8).await;
}

#[tokio::test]
async fn legacy_h2_allows_consumed_small_frames_and_preserves_end_stream() {
    let (mut peer, transport) = tokio::io::duplex(65536);
    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::channel(1);
    let server = tokio::spawn(async move {
        let mut connection = h2_legacy::server::handshake(transport).await.unwrap();
        let (request, _response) = connection.accept().await.unwrap().unwrap();
        let mut body = request.into_body();
        let driver = tokio::spawn(async move { while connection.accept().await.is_some() {} });
        let mut received = 0;
        while let Some(data) = body.data().await {
            let data = data.unwrap();
            body.flow_control().release_capacity(data.len()).unwrap();
            if !data.is_empty() {
                received += data.len();
                progress_tx.send(()).await.unwrap();
            }
        }
        driver.abort();
        received
    });
    peer.write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n")
        .await
        .unwrap();
    peer.write_all(&frame(4, 0, 0, &[])).await.unwrap();
    peer.write_all(&request_headers()).await.unwrap();
    for _ in 0..500 {
        peer.write_all(&frame(0, 0, 1, b"x")).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), progress_rx.recv())
            .await
            .unwrap()
            .unwrap();
    }
    // A non-final empty frame is ignored, but the final empty frame completes the body.
    peer.write_all(&frame(0, 0, 1, &[])).await.unwrap();
    peer.write_all(&frame(0, 1, 1, &[])).await.unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap(),
        500
    );
}

#[tokio::test]
async fn unread_request_body_returns_its_data_frame_budget() {
    let (mut peer, transport) = tokio::io::duplex(65536);
    let (dropped_tx, mut dropped_rx) = tokio::sync::mpsc::channel::<()>(1);
    let server = tokio::spawn(async move {
        let mut connection = h2_legacy::server::handshake(transport).await.unwrap();
        let unread = connection.accept().await.unwrap().unwrap().0;
        // Stream 3's HEADERS follow stream 1's DATA frames on the wire, so returning here
        // proves those frames were charged to the connection budget and are still buffered.
        let _third = connection.accept().await.unwrap().unwrap().0;
        // Respond without reading the body: the buffered budget must come back.
        drop(unread);
        dropped_tx.send(()).await.unwrap();
        // A leaked budget would surface as GOAWAY instead of a new stream.
        match connection.accept().await {
            Some(Ok(_)) => {}
            Some(Err(error)) => panic!("unread request body leaked its budget: {error:?}"),
            None => panic!("connection closed after an unread request body was dropped"),
        }
    });
    peer.write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n")
        .await
        .unwrap();
    peer.write_all(&frame(4, 0, 0, &[])).await.unwrap();
    peer.write_all(&frame(1, 4, 1, &[0x82, 0x86, 0x84]))
        .await
        .unwrap();
    for _ in 0..60 {
        peer.write_all(&frame(0, 0, 1, b"x")).await.unwrap();
    }
    peer.write_all(&frame(1, 4, 3, &[0x82, 0x86, 0x84]))
        .await
        .unwrap();
    dropped_rx.recv().await.unwrap();
    for _ in 0..60 {
        peer.write_all(&frame(0, 0, 3, b"x")).await.unwrap();
    }
    peer.write_all(&frame(0, 1, 3, &[])).await.unwrap();
    peer.write_all(&frame(1, 4, 5, &[0x82, 0x86, 0x84]))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
}
