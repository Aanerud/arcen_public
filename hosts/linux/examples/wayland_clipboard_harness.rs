//! Lab harness for the headless Wayland clipboard bridge.
//!
//! Run as the session user of a running headless desktop, never on a
//! production Pier:
//!
//! ```text
//! wayland_clipboard_harness <bus-address> <scratch-dir> [png-file]
//! ```
//!
//! On the session's private bus it creates the agent's clipboard
//! `RemoteDesktop` session (`MutterClipboard`, exactly as the agent does) and
//! serves `ClipboardService` on a socket in `<scratch-dir>`. A second bus
//! connection plays an in-session application: its own `RemoteDesktop`
//! clipboard, which owns and reads the selection through Mutter like any
//! client. The harness then drives the bridge protocol as the Pier does
//! (ready line, WebSocket, policy, offers and chunks) and checks:
//!
//! - Deck→host text and PNG, read back by the application;
//! - host→Deck text and PNG, copied by the application;
//! - a host copy above the policy refused while the bridge goes on;
//! - a supersession by a second connection.
//!
//! Each check prints `HARNESS <name>: ok|FAIL ...` with sizes and equality,
//! never content.

#[cfg(target_os = "linux")]
mod harness {
    use std::collections::HashMap;
    use std::os::fd::OwnedFd;
    use std::time::Duration;

    use arcen_media::clipboard::{
        policy_message, ClipboardContent, ClipboardDirection, ClipboardPolicy,
    };
    use arcen_pier_linux::session::wayland::clipboard::{ClipboardService, MutterClipboard};
    use arcen_protocol::clipboard::{ClipboardTransfer, ClipboardWireMessage};
    use arcen_protocol::messages::{ClipboardContentKind, ClipboardDataMsg};
    use futures_util::{SinkExt, StreamExt};
    use tokio::io::AsyncBufReadExt;
    use tokio::net::UnixStream;
    use tokio_tungstenite::tungstenite::protocol::Role;
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::WebSocketStream;
    use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
    use zbus::Proxy;

    const RD: &str = "org.gnome.Mutter.RemoteDesktop";
    const RD_PATH: &str = "/org/gnome/Mutter/RemoteDesktop";
    const RDS: &str = "org.gnome.Mutter.RemoteDesktop.Session";
    const MAX_BYTES: usize = 8 * 1024 * 1024;

    struct App {
        session: Proxy<'static>,
        owner: zbus::proxy::SignalStream<'static>,
        transfer: zbus::proxy::SignalStream<'static>,
    }

    fn mimes(names: &[&str]) -> HashMap<&'static str, Value<'static>> {
        let mut options = HashMap::new();
        options.insert(
            "mime-types",
            Value::from(
                names
                    .iter()
                    .map(|name| (*name).to_owned())
                    .collect::<Vec<_>>(),
            ),
        );
        options
    }

    impl App {
        async fn new(connection: &zbus::Connection) -> Self {
            let root = Proxy::new(connection, RD, RD_PATH, RD).await.unwrap();
            let path: OwnedObjectPath = root.call("CreateSession", &()).await.unwrap();
            let session = Proxy::new(connection, RD, path, RDS).await.unwrap();
            let owner = session
                .receive_signal("SelectionOwnerChanged")
                .await
                .unwrap();
            let transfer = session.receive_signal("SelectionTransfer").await.unwrap();
            let options: HashMap<&str, Value<'_>> = HashMap::new();
            session
                .call::<_, _, ()>("EnableClipboard", &(options,))
                .await
                .unwrap();
            Self {
                session,
                owner,
                transfer,
            }
        }

        /// Drops owner changes already delivered (the owner announced when the
        /// application enabled its clipboard, earlier copies).
        async fn drain(&mut self) {
            tokio::time::sleep(Duration::from_millis(300)).await;
            while tokio::time::timeout(Duration::from_millis(50), self.owner.next())
                .await
                .is_ok()
            {}
        }

        /// Waits for a foreign owner offering `mime`.
        async fn wait_owner(&mut self, mime: &str, within: Duration) -> Option<Vec<String>> {
            tokio::time::timeout(within, async {
                loop {
                    let message = self.owner.next().await?;
                    let (options,): (HashMap<String, OwnedValue>,) =
                        message.body().deserialize().ok()?;
                    let value = |key: &str| {
                        options
                            .get(key)
                            .and_then(|value| value.try_clone().ok())
                            .map(Value::from)
                    };
                    let names = value("mime-types")
                        .and_then(arcen_pier_linux::session::wayland::clipboard::mime_types)
                        .unwrap_or_default();
                    let ours = value("session-is-owner")
                        .and_then(|value| bool::try_from(value).ok())
                        .unwrap_or(false);
                    if !ours && names.iter().any(|name| name == mime) {
                        return Some(names);
                    }
                }
            })
            .await
            .ok()
            .flatten()
        }

        async fn paste(&self, mime: &str) -> Vec<u8> {
            let reply = self
                .session
                .call_method("SelectionRead", &(mime,))
                .await
                .unwrap();
            let (fd,): (zbus::zvariant::OwnedFd,) = reply.body().deserialize().unwrap();
            let fd: OwnedFd = fd.into();
            // Mutter hands the reader a non-blocking pipe.
            let flags = nix::fcntl::fcntl(
                std::os::fd::AsRawFd::as_raw_fd(&fd),
                nix::fcntl::FcntlArg::F_GETFL,
            )
            .unwrap();
            let flags =
                nix::fcntl::OFlag::from_bits_truncate(flags) - nix::fcntl::OFlag::O_NONBLOCK;
            nix::fcntl::fcntl(
                std::os::fd::AsRawFd::as_raw_fd(&fd),
                nix::fcntl::FcntlArg::F_SETFL(flags),
            )
            .unwrap();
            let file = std::fs::File::from(fd);
            tokio::task::spawn_blocking(move || {
                let mut file = file;
                let mut bytes = Vec::new();
                let _ = std::io::Read::read_to_end(&mut file, &mut bytes);
                bytes
            })
            .await
            .unwrap()
        }

        /// Owns the selection with `names` and serves transfers for `hold`.
        async fn copy(&mut self, names: &[&str], bytes: &[u8], hold: Duration) -> usize {
            self.session
                .call::<_, _, ()>("SetSelection", &(mimes(names),))
                .await
                .unwrap();
            let mut served = 0;
            let _ = tokio::time::timeout(hold, async {
                while let Some(message) = self.transfer.next().await {
                    let Ok((_, serial)): Result<(String, u32), _> = message.body().deserialize()
                    else {
                        continue;
                    };
                    let reply = self
                        .session
                        .call_method("SelectionWrite", &(serial,))
                        .await
                        .unwrap();
                    let (fd,): (zbus::zvariant::OwnedFd,) = reply.body().deserialize().unwrap();
                    let fd: OwnedFd = fd.into();
                    let payload = bytes.to_vec();
                    let ok = tokio::task::spawn_blocking(move || {
                        let mut file = std::fs::File::from(fd);
                        std::io::Write::write_all(&mut file, &payload).is_ok()
                    })
                    .await
                    .unwrap();
                    let _ = self
                        .session
                        .call::<_, _, ()>("SelectionWriteDone", &(serial, ok))
                        .await;
                    served += 1;
                }
            })
            .await;
            served
        }
    }

    async fn pier(socket: &std::path::Path) -> WebSocketStream<UnixStream> {
        let mut stream = UnixStream::connect(socket).await.unwrap();
        let mut line = String::new();
        {
            let mut reader = tokio::io::BufReader::new(&mut stream);
            reader.read_line(&mut line).await.unwrap();
        }
        println!("HARNESS ready_line: {}", line.trim());
        let mut websocket = WebSocketStream::from_raw_socket(stream, Role::Client, None).await;
        let policy =
            ClipboardPolicy::new(ClipboardDirection::Both, ClipboardContent::All, MAX_BYTES)
                .unwrap();
        let message = serde_json::json!({
            "type": "arcen_clipboard_policy",
            "policy": policy_message(policy),
            "directions": {"text_c2h": true, "text_h2c": true, "image_c2h": true, "image_h2c": true},
        });
        websocket
            .send(Message::Text(message.to_string().into()))
            .await
            .unwrap();
        websocket
    }

    async fn send_item(
        websocket: &mut WebSocketStream<UnixStream>,
        sequence: u64,
        kind: ClipboardContentKind,
        bytes: &[u8],
    ) {
        let mut cursor = ClipboardTransfer::new(sequence, kind, bytes.to_vec())
            .unwrap()
            .into_cursor();
        while let Some(message) = cursor.next_message() {
            let message = match message.unwrap() {
                ClipboardWireMessage::Offer(offer) => {
                    Message::Text(serde_json::to_string(&offer).unwrap().into())
                }
                ClipboardWireMessage::Chunk(frame) => Message::Binary(frame.into()),
            };
            websocket.send(message).await.unwrap();
        }
    }

    async fn receive_item(
        websocket: &mut WebSocketStream<UnixStream>,
        within: Duration,
    ) -> Option<(ClipboardDataMsg, Vec<u8>)> {
        tokio::time::timeout(within, async {
            let mut offer: Option<ClipboardDataMsg> = None;
            let mut bytes = Vec::new();
            loop {
                match websocket.next().await? {
                    Ok(Message::Text(text)) => {
                        offer = serde_json::from_str(&text).ok();
                        bytes.clear();
                    }
                    Ok(Message::Binary(frame)) => {
                        let (_, payload) = arcen_protocol::decode_clipboard_chunk(&frame).ok()?;
                        bytes.extend_from_slice(payload);
                        if let Some(offer) = offer.as_ref() {
                            if bytes.len() == offer.size_bytes as usize {
                                return Some((offer.clone(), std::mem::take(&mut bytes)));
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(_) => return None,
                }
            }
        })
        .await
        .ok()
        .flatten()
    }

    fn verdict(name: &str, ok: bool, detail: &str) {
        println!(
            "HARNESS {name}: {} {detail}",
            if ok { "ok" } else { "FAIL" }
        );
    }

    #[allow(clippy::too_many_lines)]
    pub async fn main() {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "info".into()),
            )
            .init();
        let args: Vec<String> = std::env::args().collect();
        let (Some(address), Some(scratch)) = (args.get(1), args.get(2)) else {
            eprintln!("usage: wayland_clipboard_harness <bus-address> <scratch-dir> [png-file]");
            std::process::exit(2);
        };
        let png = args
            .get(3)
            .map(|path| std::fs::read(path).expect("png file"));
        let connect = || async {
            zbus::connection::Builder::address(address.as_str())
                .unwrap()
                .build()
                .await
                .unwrap()
        };
        let bridge_bus = connect().await;
        let app_bus = connect().await;
        let compositor = match MutterClipboard::create(&bridge_bus).await {
            Ok(compositor) => {
                verdict("proof", true, "clipboard session created and enabled");
                compositor
            }
            Err(error) => {
                verdict("proof", false, &error.to_string());
                std::process::exit(1);
            }
        };
        let scratch = std::path::PathBuf::from(scratch);
        let socket = scratch.join("arcen-clipboard.sock");
        let _ = std::fs::remove_file(&socket);
        let uid = nix::unistd::Uid::effective().as_raw();
        let user = std::env::var("USER").unwrap_or_default();
        let service = ClipboardService::bind(&socket, uid, &user).unwrap();
        let mut app = App::new(&app_bus).await;
        let checks = async {
            let mut deck = pier(&socket).await;
            tokio::time::sleep(Duration::from_millis(300)).await;

            let text = "Deck→Wayland ÆØÅ ✓ harness".as_bytes();
            app.drain().await;
            send_item(&mut deck, 1, ClipboardContentKind::TextUtf8, text).await;
            let offered = app
                .wait_owner("text/plain;charset=utf-8", Duration::from_secs(5))
                .await;
            let pasted = app.paste("text/plain;charset=utf-8").await;
            verdict(
                "deck_to_host_text",
                pasted == text,
                &format!("offered={offered:?} bytes={}", pasted.len()),
            );
            app.drain().await;
            send_item(
                &mut deck,
                3,
                ClipboardContentKind::TextUtf8,
                "café ÆØÅ".as_bytes(),
            )
            .await;
            let offered = app.wait_owner("STRING", Duration::from_secs(5)).await;
            let latin1 = app.paste("STRING").await;
            verdict(
                "deck_to_host_text_latin1",
                latin1 == b"caf\xe9 \xc6\xd8\xc5",
                &format!("offered={offered:?} bytes={}", latin1.len()),
            );

            if let Some(png) = png.as_ref() {
                app.drain().await;
                send_item(&mut deck, 4, ClipboardContentKind::ImagePng, png).await;
                let offered = app.wait_owner("image/png", Duration::from_secs(5)).await;
                let pasted = app.paste("image/png").await;
                verdict(
                    "deck_to_host_png",
                    &pasted == png,
                    &format!("offered={offered:?} bytes={}", pasted.len()),
                );
            }

            let host = "Wayland host → Deck Ω harness".as_bytes();
            let (served, received) = tokio::join!(
                app.copy(
                    &["text/plain;charset=utf-8", "text/plain"],
                    host,
                    Duration::from_secs(3)
                ),
                receive_item(&mut deck, Duration::from_secs(5))
            );
            verdict(
                "host_to_deck_text",
                received.as_ref().is_some_and(|(_, bytes)| bytes == host),
                &format!(
                    "served={served} offer={:?}",
                    received.as_ref().map(|(offer, _)| (
                        offer.sequence,
                        offer.kind,
                        offer.size_bytes
                    ))
                ),
            );

            let big = vec![b'x'; MAX_BYTES + 1024];
            let (served, received) = tokio::join!(
                app.copy(&["text/plain;charset=utf-8"], &big, Duration::from_secs(4)),
                receive_item(&mut deck, Duration::from_secs(4))
            );
            verdict(
                "host_to_deck_oversize_refused",
                received.is_none(),
                &format!("served_attempts={served} received={}", received.is_some()),
            );

            if let Some(png) = png.as_ref() {
                let (served, received) = tokio::join!(
                    app.copy(&["image/png"], png, Duration::from_secs(3)),
                    receive_item(&mut deck, Duration::from_secs(5))
                );
                verdict(
                    "host_to_deck_png_after_oversize",
                    received.as_ref().is_some_and(|(_, bytes)| bytes == png),
                    &format!(
                        "served={served} offer={:?}",
                        received.as_ref().map(|(offer, _)| (
                            offer.sequence,
                            offer.kind,
                            offer.size_bytes
                        ))
                    ),
                );
            }

            // Large items both ways at once through the Pier's own bridge
            // link: a host copy streams to the "Pier" while a client item
            // streams to the agent. Each side must keep reading while it
            // writes, or both block on full socket buffers.
            {
                use arcen_pier_linux::clipboard::{
                    ClipboardAgentIo, ClipboardBridgeKind, ClipboardBridgeLink, ClipboardItem,
                };
                const SIZE: usize = 4 * 1024 * 1024;
                let mut stream = UnixStream::connect(&socket).await.unwrap();
                let mut line = String::new();
                {
                    let mut reader = tokio::io::BufReader::new(&mut stream);
                    reader.read_line(&mut line).await.unwrap();
                }
                let mut websocket = WebSocketStream::from_raw_socket(
                    ClipboardAgentIo::Session(stream),
                    Role::Client,
                    None,
                )
                .await;
                let policy = ClipboardPolicy::new(
                    ClipboardDirection::Both,
                    ClipboardContent::All,
                    MAX_BYTES,
                )
                .unwrap();
                let message = serde_json::json!({
                    "type": "arcen_clipboard_policy",
                    "policy": policy_message(policy),
                    "directions": {"text_c2h": true, "text_h2c": true, "image_c2h": true, "image_h2c": true},
                });
                websocket
                    .send(Message::Text(message.to_string().into()))
                    .await
                    .unwrap();
                let mut link =
                    ClipboardBridgeLink::new(websocket, ClipboardBridgeKind::WaylandSession);
                tokio::time::sleep(Duration::from_millis(300)).await;
                app.drain().await;
                let host_big = vec![b'h'; SIZE];
                let deck_big = vec![b'd'; SIZE];
                let started = std::time::Instant::now();
                let receive_host = async {
                    // Once the host copy is streaming in, the client item
                    // streams out through the link's own writer.
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    assert!(link.offer(
                        ClipboardItem::new(
                            9,
                            ClipboardContentKind::TextUtf8,
                            deck_big.clone(),
                            false
                        )
                        .unwrap()
                    ));
                    let mut received = 0usize;
                    while received < SIZE {
                        match tokio::time::timeout(Duration::from_secs(20), link.next_event()).await
                        {
                            Ok(arcen_pier_linux::clipboard::ClipboardBridgeEvent::Message(
                                Some(Ok(Message::Binary(frame))),
                            )) => {
                                received += arcen_protocol::decode_clipboard_chunk(&frame)
                                    .map_or(0, |(_, payload)| payload.len());
                            }
                            Ok(arcen_pier_linux::clipboard::ClipboardBridgeEvent::Message(
                                Some(Ok(_)),
                            )) => {}
                            _ => break,
                        }
                    }
                    received
                };
                let copy = app.copy(
                    &["text/plain;charset=utf-8"],
                    &host_big,
                    Duration::from_secs(3),
                );
                let (received, _) = tokio::join!(receive_host, copy);
                let elapsed = started.elapsed();
                let offered = app
                    .wait_owner("text/plain;charset=utf-8", Duration::from_secs(10))
                    .await;
                let pasted = app.paste("text/plain;charset=utf-8").await;
                verdict(
                    "both_directions_at_once_4mib",
                    received == SIZE && pasted == deck_big,
                    &format!(
                        "host_to_deck_bytes={received} deck_to_host_bytes={} owner_seen={} in {elapsed:?}",
                        pasted.len(),
                        offered.is_some()
                    ),
                );
                drop(link);
                tokio::time::sleep(Duration::from_millis(300)).await;
            }

            // A second connection supersedes the first; the clipboard keeps
            // working for it.
            let mut successor = pier(&socket).await;
            tokio::time::sleep(Duration::from_millis(300)).await;
            let old_closed = tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    match deck.next().await {
                        None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return true,
                        Some(Ok(_)) => {}
                    }
                }
            })
            .await
            .unwrap_or(false);
            let again = "after supersession".as_bytes();
            let (_, received) = tokio::join!(
                app.copy(&["text/plain;charset=utf-8"], again, Duration::from_secs(3)),
                receive_item(&mut successor, Duration::from_secs(5))
            );
            verdict(
                "supersession",
                old_closed && received.as_ref().is_some_and(|(_, bytes)| bytes == again),
                &format!("old_closed={old_closed} received={}", received.is_some()),
            );
            drop(successor);
            tokio::time::sleep(Duration::from_millis(500)).await;
        };
        tokio::select! {
            () = service.serve(&compositor) => verdict("service", false, "ended on its own"),
            () = checks => {}
        }
        println!("HARNESS RESULT done");
    }
}

#[cfg(target_os = "linux")]
#[tokio::main]
async fn main() {
    harness::main().await;
}

#[cfg(not(target_os = "linux"))]
fn main() {}
