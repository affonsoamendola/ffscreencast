//! HTTP signaling: serves the viewer page, config API, and accepts WebRTC offers.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Instant;

use axum::extract::State;
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use tokio::sync::RwLock;
use rtc::interceptor::Registry;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors;
use rtc::peer_connection::configuration::media_engine::{MediaEngine, MIME_TYPE_H264, MIME_TYPE_OPUS};
use rtc::peer_connection::configuration::RTCConfigurationBuilder;
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::peer_connection::transport::RTCIceServer;
use webrtc::peer_connection::RTCIceCandidateInit;
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters,
    RtpCodecKind,
};
use serde::{Deserialize, Serialize};
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::media_stream::Track;
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCPeerConnectionIceErrorEvent,
    RTCPeerConnectionIceEvent, RTCIceCandidateType, RTCIceGatheringState, RTCPeerConnectionState,
};
use webrtc::runtime::{channel, Sender, TokioRuntime};

use crate::audio::AudioSettings;
use crate::broadcast::Broadcaster;
use crate::capture::{Capture, MonitorInfo, Target, WindowInfo};
use crate::stats::StreamStats;

pub const VIEWER_HTML: &str = include_str!("../viewer.html");

const BRUTE_FORCE_THRESHOLD: u32 = 10;

#[derive(Clone)]
pub struct AppState {
    pub capture: Capture,
    pub stun_urls: Vec<String>,
    pub fps: Arc<AtomicU32>,
    pub stats: Arc<StreamStats>,
    pub password: Arc<String>,
    pub sessions: Arc<RwLock<HashMap<String, Instant>>>,
    pub auth_failures: Arc<RwLock<HashMap<IpAddr, u32>>>,
    pub shutdown_tx: tokio::sync::watch::Sender<bool>,
    pub audio_settings: Arc<tokio::sync::Mutex<AudioSettings>>,
    pub peer_connections: Arc<RwLock<HashMap<String, Arc<dyn PeerConnection>>>>,
    pub server_candidates: Arc<RwLock<HashMap<String, Vec<RTCIceCandidateInit>>>>,
    pub pending_client_candidates: Arc<RwLock<HashMap<String, Vec<RTCIceCandidateInit>>>>,
    pub broadcaster: Broadcaster,
}

impl AppState {
    pub fn new(
        capture: Capture,
        stun_urls: Vec<String>,
        fps: u32,
        stats: Arc<StreamStats>,
        password: String,
        shutdown_tx: tokio::sync::watch::Sender<bool>,
        audio_settings: AudioSettings,
    ) -> Self {
        Self {
            capture,
            stun_urls,
            fps: Arc::new(AtomicU32::new(fps)),
            stats,
            password: Arc::new(password),
            sessions: Arc::new(RwLock::new(HashMap::new())),
            auth_failures: Arc::new(RwLock::new(HashMap::new())),
            shutdown_tx,
            audio_settings: Arc::new(tokio::sync::Mutex::new(audio_settings)),
            peer_connections: Arc::new(RwLock::new(HashMap::new())),
            server_candidates: Arc::new(RwLock::new(HashMap::new())),
            pending_client_candidates: Arc::new(RwLock::new(HashMap::new())),
            broadcaster: Broadcaster::new(),
        }
    }

    pub fn get_fps(&self) -> u32 {
        self.fps.load(Ordering::Relaxed)
    }

    pub fn set_fps(&self, fps: u32) {
        self.fps.store(fps, Ordering::Relaxed);
    }
}

// ── Constant-time password comparison ─────────────────────────

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ── Auth ──────────────────────────────────────────────────────

fn extract_client_ip(headers: &HeaderMap) -> IpAddr {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)))
}

#[derive(Deserialize)]
pub struct AuthRequest {
    pub password: String,
}

pub async fn post_auth(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<AuthRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let ip = extract_client_ip(&headers);

    if !constant_time_eq(req.password.as_bytes(), state.password.as_bytes()) {
        let mut failures = state.auth_failures.write().await;
        let count = failures.entry(ip).or_insert(0);
        *count += 1;
        let current = *count;
        drop(failures);

        logln!("[auth] failed login from {ip} (attempt {current})");

        if current >= BRUTE_FORCE_THRESHOLD {
            logln!("[auth] brute force threshold reached from {ip}, shutting down");
            let _ = state.shutdown_tx.send(true);
            return Err(StatusCode::TOO_MANY_REQUESTS);
        }

        return Err(StatusCode::FORBIDDEN);
    }

    // Clear failures on success
    state.auth_failures.write().await.remove(&ip);

    let token = uuid_simple();
    state
        .sessions
        .write()
        .await
        .insert(token.clone(), Instant::now());

    logln!("[auth] successful login from {ip}");

    Ok(Json(serde_json::json!({ "token": token })))
}

fn uuid_simple() -> String {
    use std::fmt::Write;
    let bytes: [u8; 16] = rand::random();
    let mut s = String::with_capacity(36);
    for (i, b) in bytes.iter().enumerate() {
        if i == 4 || i == 6 || i == 8 || i == 10 {
            s.push('-');
        }
        let _ = write!(s, "{:02x}", b);
    }
    s
}

async fn validate_token(state: &AppState, headers: &HeaderMap) -> Result<String, StatusCode> {
    let token = headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(StatusCode::UNAUTHORIZED)?;

    let mut sessions = state.sessions.write().await;
    let now = Instant::now();
    // Expire tokens older than 24 hours and check the requested token
    sessions.retain(|_, created| now.duration_since(*created).as_secs() < 86400);
    if sessions.contains_key(token) {
        Ok(token.to_string())
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

// ── Viewer page ──────────────────────────────────────────────

pub async fn index() -> Response {
    (
        StatusCode::OK,
        [
            ("content-type", "text/html; charset=utf-8"),
            ("cache-control", "no-store"),
        ],
        VIEWER_HTML,
    )
        .into_response()
}

// ── Config API ───────────────────────────────────────────────

#[derive(Serialize)]
pub struct TargetsResponse {
    pub monitors: Vec<MonitorInfo>,
    pub windows: Vec<WindowInfo>,
}

pub async fn get_targets(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<TargetsResponse>, StatusCode> {
    _ = validate_token(&state, &headers).await?;

    let monitors = Capture::list_monitors().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let windows = Capture::list_windows().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(TargetsResponse { monitors, windows }))
}

#[derive(Deserialize)]
pub struct ConfigRequest {
    pub target_type: String,
    pub target_index: Option<usize>,
    pub target_title: Option<String>,
    pub fps: Option<u32>,
}

pub async fn post_config(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ConfigRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    _ = validate_token(&state, &headers).await?;

    let target = match req.target_type.as_str() {
        "monitor" => {
            let idx = req.target_index.unwrap_or(0);
            Target::Monitor(idx)
        }
        "window" => {
            let title = req
                .target_title
                .ok_or(StatusCode::BAD_REQUEST)?;
            Target::Window(title)
        }
        "combined" => Target::Combined,
        _ => return Err(StatusCode::BAD_REQUEST),
    };

    state.capture.set_target(target);

    if let Some(fps) = req.fps {
        let fps = fps.clamp(1, 60);
        state.set_fps(fps);
    }

    Ok(Json(serde_json::json!({ "ok": true })))
}

// ── Keyframe request ─────────────────────────────────────────

pub async fn post_keyframe(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, StatusCode> {
    _ = validate_token(&state, &headers).await?;
    state.broadcaster.request_keyframe();
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ── WebRTC offer ─────────────────────────────────────────────

#[derive(Deserialize)]
pub struct OfferRequest {
    pub sdp: String,
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Serialize)]
pub struct AnswerResponse {
    pub sdp: String,
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Clone)]
struct ConnHandler {
    gather_complete_tx: Sender<()>,
    connected_tx: Sender<()>,
    disconnected_tx: tokio::sync::broadcast::Sender<()>,
    session_token: String,
    server_candidates: Arc<RwLock<HashMap<String, Vec<RTCIceCandidateInit>>>>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for ConnHandler {
    async fn on_ice_candidate(&self, event: RTCPeerConnectionIceEvent) {
        let c = &event.candidate;
        match c.typ {
            RTCIceCandidateType::Host => {}
            _ => {
                logln!(
                    "[webrtc] ICE candidate: {:?} {}:{} (related={}:{})",
                    c.typ,
                    c.address,
                    c.port,
                    c.related_address,
                    c.related_port,
                );
            }
        }
        if let Ok(candidate_init) = event.candidate.to_json() {
            if !candidate_init.candidate.is_empty() {
                if let Some(candidates) = self.server_candidates.write().await.get_mut(&self.session_token) {
                    candidates.push(candidate_init);
                }
            }
        }
    }
    async fn on_ice_candidate_error(&self, event: RTCPeerConnectionIceErrorEvent) {
        logln!(
            "[webrtc] ICE candidate error: code={} url={} text={}",
            event.error_code,
            event.url,
            event.error_text,
        );
    }
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        logln!("[webrtc] ICE gathering state: {state:?}");
        if state == RTCIceGatheringState::Complete {
            let _ = self.gather_complete_tx.try_send(());
        }
    }
    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        logln!("[webrtc] connection state: {state:?}");
        match state {
            RTCPeerConnectionState::Connected => {
                let _ = self.connected_tx.try_send(());
            }
            RTCPeerConnectionState::Failed
            | RTCPeerConnectionState::Disconnected
            | RTCPeerConnectionState::Closed => {
                let _ = self.disconnected_tx.send(());
            }
            _ => {}
        }
    }
}

pub async fn offer(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<OfferRequest>,
) -> Result<Json<AnswerResponse>, (StatusCode, String)> {
    let token = validate_token(&state, &headers)
        .await
        .map_err(|e| (e, "unauthorized".into()))?;

    logln!("[signaling] received {} offer, SDP length={}", req.kind, req.sdp.len());
    if req.kind != "offer" {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("expected type 'offer', got '{}'", req.kind),
        ));
    }

    let mut media_engine = MediaEngine::default();
    media_engine
        .register_codec(
            RTCRtpCodecParameters {
                rtp_codec: RTCRtpCodec {
                    mime_type: MIME_TYPE_H264.to_owned(),
                    clock_rate: 90000,
                    channels: 0,
                    sdp_fmtp_line:
                        "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=640033"
                            .to_owned(),
                    rtcp_feedback: vec![],
                },
                payload_type: 96,
                ..Default::default()
            },
            RtpCodecKind::Video,
        )
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("register codec: {e}")))?;

    media_engine
        .register_codec(
            RTCRtpCodecParameters {
                rtp_codec: RTCRtpCodec {
                    mime_type: MIME_TYPE_OPUS.to_owned(),
                    clock_rate: 48000,
                    channels: 2,
                    sdp_fmtp_line: String::new(),
                    rtcp_feedback: vec![],
                },
                payload_type: 111,
                ..Default::default()
            },
            RtpCodecKind::Audio,
        )
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("register audio codec: {e}")))?;

    let registry = register_default_interceptors(Registry::new(), &mut media_engine)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("interceptors: {e}")))?;

    let config = if !state.stun_urls.is_empty() {
        RTCConfigurationBuilder::new()
            .with_ice_servers(vec![RTCIceServer {
                urls: state.stun_urls.clone(),
                ..Default::default()
            }])
            .build()
    } else {
        RTCConfigurationBuilder::new().build()
    };

    state.server_candidates.write().await.insert(token.clone(), Vec::new());

    let (gather_complete_tx, _gather_complete_rx) = channel::<()>(1);
    let (connected_tx, mut connected_rx) = channel::<()>(1);
    let (disconnected_tx, _) = tokio::sync::broadcast::channel::<()>(1);
    let handler = Arc::new(ConnHandler {
        gather_complete_tx,
        connected_tx,
        disconnected_tx: disconnected_tx.clone(),
        session_token: token.clone(),
        server_candidates: state.server_candidates.clone(),
    });

    let peer_connection = PeerConnectionBuilder::new()
        .with_configuration(config)
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .with_handler(handler)
        .with_runtime(Arc::new(TokioRuntime))
        .with_udp_addrs(vec!["0.0.0.0:0".to_string()])
        .build()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("peer build: {e}")))?;

    let ssrc: u32 = rand::random();
    let video_track = Arc::new(
        TrackLocalStaticSample::new(MediaStreamTrack::new(
            "ffscreencast-stream".to_string(),
            "ffscreencast-track".to_string(),
            "ffscreencast".to_string(),
            RtpCodecKind::Video,
            vec![RTCRtpEncodingParameters {
                rtp_coding_parameters: RTCRtpCodingParameters {
                    ssrc: Some(ssrc),
                    ..Default::default()
                },
                codec: RTCRtpCodec {
                    mime_type: MIME_TYPE_H264.to_string(),
                    clock_rate: 90000,
                    channels: 0,
                    sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=640033".to_string(),
                    rtcp_feedback: vec![],
                },
                ..Default::default()
            }],
        ))
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("create track: {e}")))?,
    );

    let sender = peer_connection
        .add_track(video_track.clone() as Arc<dyn TrackLocal>)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("add_track: {e}")))?;

    let audio_ssrc: u32 = rand::random();
    let audio_track = Arc::new(
        TrackLocalStaticSample::new(MediaStreamTrack::new(
            "ffscreencast-audio-stream".to_string(),
            "ffscreencast-audio-track".to_string(),
            "ffscreencast-audio".to_string(),
            RtpCodecKind::Audio,
            vec![RTCRtpEncodingParameters {
                rtp_coding_parameters: RTCRtpCodingParameters {
                    ssrc: Some(audio_ssrc),
                    ..Default::default()
                },
                codec: RTCRtpCodec {
                    mime_type: MIME_TYPE_OPUS.to_string(),
                    clock_rate: 48000,
                    channels: 2,
                    sdp_fmtp_line: String::new(),
                    rtcp_feedback: vec![],
                },
                ..Default::default()
            }],
        ))
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("create audio track: {e}")))?,
    );

    let audio_sender = peer_connection
        .add_track(audio_track.clone() as Arc<dyn TrackLocal>)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("add audio track: {e}")))?;

    let offer_desc = RTCSessionDescription::offer(req.sdp)
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("invalid offer SDP: {e}")))?;

    for line in offer_desc.sdp.lines() {
        if line.starts_with("a=candidate:") {
            logln!("[signaling] offer candidate: {line}");
        }
    }

    peer_connection
        .set_remote_description(offer_desc)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("set_remote: {e}")))?;
    let answer = peer_connection
        .create_answer(None)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("create_answer: {e}")))?;

    peer_connection
        .set_local_description(answer)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("set_local: {e}")))?;

    let peer_arc = Arc::new(peer_connection) as Arc<dyn webrtc::peer_connection::PeerConnection>;

    state.peer_connections.write().await.insert(token.clone(), peer_arc.clone());

    {
        let mut pending = state.pending_client_candidates.write().await;
        if let Some(candidates) = pending.remove(&token) {
            for c in candidates {
                if let Err(e) = peer_arc.add_ice_candidate(c).await {
                    logln!("[signaling] add pending client candidate failed: {e}");
                }
            }
        }
    }

    let answer_sdp = {
        if let Some(local_desc) = peer_arc.local_description().await {
            local_desc.sdp
        } else {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                "no local description".to_string(),
            ));
        }
    };

    for line in answer_sdp.lines() {
        if line.starts_with("a=candidate:") {
            logln!("[signaling] answer candidate: {line}");
        }
    }

    let response = Json(AnswerResponse {
        sdp: answer_sdp,
        kind: "answer".to_string(),
    });

    logln!("[signaling] answer sent, SDP length={}", response.sdp.len());

    let fps = state.get_fps();

    tokio::spawn(async move {
        logln!("[signaling] waiting for peer connection (30s timeout)...");
        match tokio::time::timeout(std::time::Duration::from_secs(30), connected_rx.recv()).await {
            Ok(Some(())) => {
                logln!("[signaling] peer connected, subscribing to broadcast");
            }
            Ok(None) => {
                logln!("[signaling] connected channel closed without value");
                return;
            }
            _ => {
                logln!("[signaling] timed out waiting for connected state");
                return;
            }
        }

        let negotiated_pt = sender
            .get_parameters()
            .await
            .ok()
            .and_then(|p| p.rtp_parameters.codecs.first().map(|c| c.payload_type))
            .unwrap_or(96);

        let actual_ssrc = *video_track.ssrcs().await.first().unwrap_or(&ssrc);

        let audio_negotiated_pt = audio_sender
            .get_parameters()
            .await
            .ok()
            .and_then(|p| p.rtp_parameters.codecs.first().map(|c| c.payload_type))
            .unwrap_or(111);

        let actual_audio_ssrc = *audio_track.ssrcs().await.first().unwrap_or(&audio_ssrc);

        let audio_settings = state.audio_settings.lock().await.clone();
        let audio_enabled = audio_settings.enabled;

        state.broadcaster.add_subscriber();
        let video_rx = state.broadcaster.subscribe_video();
        let audio_rx = if audio_enabled {
            Some(state.broadcaster.subscribe_audio())
        } else {
            None
        };

        state.broadcaster.ensure_running(
            state.capture.clone(),
            fps,
            state.stats.clone(),
            audio_settings,
        );

        let disconnected_rx = disconnected_tx.subscribe();
        let video_handle = tokio::spawn(crate::broadcast::forward_video(
            video_rx, video_track, actual_ssrc, negotiated_pt, disconnected_rx,
        ));
        let audio_handle = if let Some(audio_rx) = audio_rx {
            tokio::spawn(crate::broadcast::forward_audio(
                audio_rx, audio_track, actual_audio_ssrc, audio_negotiated_pt,
            ))
        } else {
            tokio::spawn(async {})
        };

        let _ = tokio::join!(video_handle, audio_handle);
        state.broadcaster.remove_subscriber();
        drop(peer_arc);
        state.peer_connections.write().await.remove(&token);
        state.server_candidates.write().await.remove(&token);
        state.pending_client_candidates.write().await.remove(&token);
        logln!("[signaling] peer connection cleaned up for session");
    });

    Ok(response)
}



// ── Trickle ICE ──────────────────────────────────────────────

#[derive(Deserialize)]
pub struct CandidateRequest {
    pub candidate: RTCIceCandidateInit,
}

pub async fn post_candidate(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CandidateRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let token = validate_token(&state, &headers).await.map_err(|e| e)?;

    let pc_exists = {
        let pcs = state.peer_connections.read().await;
        pcs.contains_key(&token)
    };
    if pc_exists {
        let pcs = state.peer_connections.read().await;
        if let Some(pc) = pcs.get(&token) {
            if let Err(e) = pc.add_ice_candidate(req.candidate).await {
                logln!("[signaling] add_ice_candidate failed: {e}");
            }
        }
    } else {
        state.pending_client_candidates.write().await
            .entry(token)
            .or_default()
            .push(req.candidate);
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Serialize)]
pub struct CandidatesResponse {
    pub candidates: Vec<RTCIceCandidateInit>,
}

pub async fn get_candidates(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<CandidatesResponse>, StatusCode> {
    let token = validate_token(&state, &headers).await.map_err(|e| e)?;

    let mut candidates_map = state.server_candidates.write().await;
    let candidates = candidates_map.remove(&token).unwrap_or_default();
    Ok(Json(CandidatesResponse { candidates }))
}


pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/auth", post(post_auth))
        .route("/offer", post(offer))
        .route("/api/targets", get(get_targets))
        .route("/api/config", post(post_config))
        .route("/api/keyframe", post(post_keyframe))
        .route("/api/candidate", post(post_candidate))
        .route("/api/candidates", get(get_candidates))
        .with_state(state)
}
