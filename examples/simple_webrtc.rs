//! To run this, clone https://github.com/centricular/gstwebrtc-demos, then:
//! $ cd signalling
//! $ ./simple-server.py
//! $ cd ../sendrcv/js
//! $ python -m SimpleHTTPServer
//! Then load http://localhost:8000 in a web browser, note the client id.
//! Then run this example with arguments `8443 {id}`.

extern crate env_logger;
extern crate rand;
extern crate serde;
#[macro_use]
extern crate serde_derive;
extern crate serde_json;
extern crate servo_media;
extern crate websocket;

use rand::Rng;
use servo_media::ServoMedia;
use servo_media::webrtc::*;
use std::env;
use std::net;
use std::sync::{Arc, Mutex, mpsc, Weak};
use std::thread;
use websocket::OwnedMessage;

#[derive(PartialEq, PartialOrd, Eq, Debug, Copy, Clone, Ord)]
#[allow(unused)]
enum AppState {
    Error = 1,
    ServerConnected,
    ServerRegistering = 2000,
    ServerRegisteringError,
    ServerRegistered,
    PeerConnecting = 3000,
    PeerConnectionError,
    PeerConnected,
    PeerCallNegotiating = 4000,
    PeerCallStarted,
    PeerCallError,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum JsonMsg {
    Ice {
        candidate: String,
        #[serde(rename = "sdpMLineIndex")]
        sdp_mline_index: u32,
    },
    Sdp {
        #[serde(rename = "type")]
        type_: String,
        sdp: String,
    },
}

fn send_loop(
    mut sender: websocket::sender::Writer<net::TcpStream>,
    send_msg_rx: mpsc::Receiver<OwnedMessage>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || loop {
        let msg = match send_msg_rx.recv() {
            Ok(msg) => msg,
            Err(err) => {
                println!("Send loop error {:?}", err);
                return;
            }
        };

        if let OwnedMessage::Close(_) = msg {
            let _ = sender.send_message(&msg);
            return;
        }

        if let Err(err) = sender.send_message(&msg) {
            println!("Error sending {:?}", err);
        }
    })
}

struct State {
    app_state: AppState,
    send_msg_tx: mpsc::Sender<OwnedMessage>,
    _uid: u32,
    peer_id: Option<String>,
    media: Arc<ServoMedia>,
    webrtc: Option<Arc<WebRtcController>>,
}

impl State {
    fn handle_error(&self) {
        let _error = match self.app_state {
            AppState::ServerRegistering => AppState::ServerRegisteringError,
            AppState::PeerConnecting => AppState::PeerConnectionError,
            AppState::PeerConnected => AppState::PeerCallError,
            AppState::PeerCallNegotiating => AppState::PeerCallError,
            AppState::ServerRegisteringError => AppState::ServerRegisteringError,
            AppState::PeerConnectionError => AppState::PeerConnectionError,
            AppState::PeerCallError => AppState::PeerCallError,
            AppState::Error => AppState::Error,
            AppState::ServerConnected => AppState::Error,
            AppState::ServerRegistered => AppState::Error,
            AppState::PeerCallStarted => AppState::Error,
        };
    }

    fn handle_hello(&mut self) {
        assert_eq!(self.app_state, AppState::ServerRegistering);
        self.app_state = AppState::ServerRegistered;
        if let Some(ref peer_id) = self.peer_id {
            self.send_msg_tx.send(OwnedMessage::Text(format!("SESSION {}", peer_id))).unwrap();
            self.app_state = AppState::PeerConnecting;
        }
        if self.peer_id.is_none() {
            let signaller = SignallerWrap::new(self.send_msg_tx.clone());
            let s = signaller.clone();
            self.webrtc = Some(self.media.create_webrtc_arc(Box::new(signaller)));
            s.0.lock().unwrap().1 = Some(Arc::downgrade(self.webrtc.as_ref().unwrap()));
        }
    }

    fn handle_session_ok(&mut self) {
        println!("session is ok; creating webrtc objects");
        assert_eq!(self.app_state, AppState::PeerConnecting);
        self.app_state = AppState::PeerConnected;
        if self.peer_id.is_some() {
            let signaller = SignallerWrap::new(self.send_msg_tx.clone());
            let s = signaller.clone();
            self.webrtc = Some(self.media.create_webrtc_arc(Box::new(signaller)));
            s.0.lock().unwrap().1 = Some(Arc::downgrade(self.webrtc.as_ref().unwrap()));
        }
    }
}

struct Signaller(mpsc::Sender<OwnedMessage>, Option<Weak<WebRtcController>>);

#[derive(Clone)]
struct SignallerWrap(Arc<Mutex<Signaller>>);

impl WebRtcSignaller for SignallerWrap {
    fn close(&self, reason: String) {
        let signaller = self.0.lock().unwrap();
        let _ = signaller.0.send(OwnedMessage::Close(Some(websocket::message::CloseData {
            status_code: 1011, //Internal Error
            reason: reason,
        })));
    }

    fn on_ice_candidate(&self, candidate: IceCandidate) {
        let signaller = self.0.lock().unwrap();
        let message = serde_json::to_string(&JsonMsg::Ice {
            candidate: candidate.candidate,
            sdp_mline_index: candidate.sdp_mline_index,
        }).unwrap();
        signaller.0.send(OwnedMessage::Text(message)).unwrap();
    }

    fn on_negotiation_needed(&self) {
        let s2 = self.0.clone();
        let signaller = self.0.lock().unwrap();
        let controller = signaller.1.as_ref().unwrap().upgrade().unwrap();
        let c2 = controller.clone();
        thread::spawn(move || {
            controller.create_offer((move |offer: SessionDescription| {
                thread::spawn(move || {
                    c2.set_local_description(offer.clone(), (move || {
                        s2.lock().unwrap().send_sdp(offer);
                    }).into());
                });

            }).into());
        });
    }
}

impl Signaller {
    fn send_sdp(&self, desc: SessionDescription) {
        let message = serde_json::to_string(&JsonMsg::Sdp {
            type_: desc.type_.as_str().into(),
            sdp: desc.sdp,
        }).unwrap();
        self.0.send(OwnedMessage::Text(message)).unwrap();
    }
}

impl SignallerWrap {
    fn new(sender: mpsc::Sender<OwnedMessage>) -> Self {
        let signaller = Signaller(sender, None);
        SignallerWrap(Arc::new(Mutex::new(signaller)))
    }
}

fn receive_loop(
    mut receiver: websocket::receiver::Reader<net::TcpStream>,
    send_msg_tx: mpsc::Sender<OwnedMessage>,
    mut state: State,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        for message in receiver.incoming_messages() {
            let message = match message {
                Ok(m) => m,
                Err(e) => {
                    println!("Receive Loop error: {:?}", e);
                    if let Some(ref mut controller) = state.webrtc {
                        controller.notify_signal_server_error();
                    }
                    let _ = send_msg_tx.send(OwnedMessage::Close(None));
                    return;
                }
            };

            match message {
                OwnedMessage::Close(_) => {
                    let _ = send_msg_tx.send(OwnedMessage::Close(None));
                    return;
                }

                OwnedMessage::Ping(data) => {
                    if let Err(e) = send_msg_tx.send(OwnedMessage::Pong(data)) {
                        println!("Receive Loop error: {:?}", e);
                        return;
                    }
                }

                OwnedMessage::Text(msg) => {
                    match &*msg {
                        "HELLO" => state.handle_hello(),

                        "SESSION_OK" => state.handle_session_ok(),

                        x if x.starts_with("ERROR") => {
                            println!("Got error message! {}", msg);
                            state.handle_error()
                        }

                        _ => {
                            let json_msg: JsonMsg = serde_json::from_str(&msg).unwrap();

                            match json_msg {
                                JsonMsg::Sdp { type_, sdp } => {
                                    let desc = SessionDescription {
                                        type_: type_.parse().unwrap(),
                                        sdp: sdp.into()
                                    };
                                    state.webrtc.as_ref().unwrap().set_remote_description(desc, (|| {}).into());
                                }
                                JsonMsg::Ice {
                                    sdp_mline_index,
                                    candidate,
                                } => {
                                    let candidate = IceCandidate {
                                        sdp_mline_index, candidate
                                    };
                                    state.webrtc.as_ref().unwrap().add_ice_candidate(candidate).into()
                                }
                            };
                        }
                    }
                }

                _ => {
                    println!("Unmatched message type: {:?}", message);
                }
            }
        }
    })
}

fn run_example(servo_media: Arc<ServoMedia>) {
    env_logger::init();
    let mut args = env::args();
    let _ = args.next();
    let server_port = args.next().expect("Usage: simple_webrtc <port> <peer id>").parse::<u32>().unwrap();
    let server = format!("ws://localhost:{}", server_port);
    let peer_id = args.next();

    println!("Connecting to server {}", server);
    let client = match websocket::client::ClientBuilder::new(&server)
        .unwrap()
        .connect_insecure()
    {
        Ok(client) => client,
        Err(err) => {
            println!("Failed to connect to {} with error: {:?}", server, err);
            panic!("uh oh");
        }
    };
    let (receiver, sender) = client.split().unwrap();

    let (send_msg_tx, send_msg_rx) = mpsc::channel::<OwnedMessage>();
    let send_loop = send_loop(sender, send_msg_rx);

    let our_id = rand::thread_rng().gen_range(10, 10_000);
    println!("Registering id {} with server", our_id);
    send_msg_tx.send(OwnedMessage::Text(format!("HELLO {}", our_id))).expect("error sending");

    let state = State {
        app_state: AppState::ServerRegistering,
        send_msg_tx: send_msg_tx.clone(),
        _uid: our_id,
        peer_id: peer_id,
        media: servo_media,
        webrtc: None,
    };

    let receive_loop = receive_loop(receiver, send_msg_tx, state);
    let _ = send_loop.join();
    let _ = receive_loop.join();
}

fn main() {
    if let Ok(servo_media) = ServoMedia::get() {
        run_example(servo_media);
    } else {
        unreachable!();
    }
}
