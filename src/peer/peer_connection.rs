use crate::api::media_engine::MediaEngine;
use crate::api::setting_engine::SettingEngine;
use crate::api::API;
use crate::data::data_channel::DataChannel;
use crate::data::sctp_transport::SCTPTransport;
use crate::media::dtls_transport::dtls_transport_state::DTLSTransportState;
use crate::media::dtls_transport::DTLSTransport;
use crate::media::ice_transport::ice_transport_state::ICETransportState;
use crate::media::ice_transport::ICETransport;
use crate::media::interceptor::Interceptor;
use crate::media::rtp::rtp_receiver::RTPReceiver;
use crate::media::rtp::rtp_transceiver::{find_by_mid, satisfy_type_and_direction, RTPTransceiver};
use crate::media::track::track_remote::TrackRemote;
use crate::peer::configuration::Configuration;
use crate::peer::ice::ice_connection_state::ICEConnectionState;
use crate::peer::ice::ice_gather::ice_gatherer::{
    ICEGatherer, OnGatheringCompleteHdlrFn, OnICEGathererStateChangeHdlrFn, OnLocalCandidateHdlrFn,
};
use crate::peer::ice::ice_gather::ICEGatherOptions;
use crate::peer::peer_connection_state::{NegotiationNeededState, PeerConnectionState};
use crate::peer::policy::bundle_policy::BundlePolicy;
use crate::peer::policy::ice_transport_policy::ICETransportPolicy;
use crate::peer::policy::rtcp_mux_policy::RTCPMuxPolicy;
use crate::peer::policy::sdp_semantics::SDPSemantics;
use crate::peer::sdp::session_description::{SessionDescription, SessionDescriptionSerde};
use crate::peer::signaling_state::{check_next_signaling_state, SignalingState, StateChangeOp};

use crate::error::Error;
use crate::media::dtls_transport::dtls_role::{DEFAULT_DTLS_ROLE_ANSWER, DEFAULT_DTLS_ROLE_OFFER};
use crate::media::rtp::rtp_codec::RTPCodecType;
use crate::media::rtp::rtp_transceiver_direction::RTPTransceiverDirection;
use crate::media::rtp::RTPCodingParameters;
use crate::peer::ice::ice_gather::ice_gatherer_state::ICEGathererState;
use crate::peer::ice::ice_gather::ice_gathering_state::ICEGatheringState;
use crate::peer::ice::ice_role::ICERole;
use crate::peer::offer_answer_options::{AnswerOptions, OfferOptions};
use crate::peer::operation::Operations;
use crate::peer::sdp::sdp_type::SDPType;
use crate::peer::sdp::{
    description_is_plan_b, extract_fingerprint, extract_ice_details, get_by_mid, get_mid_value,
    get_peer_direction, have_data_channel, populate_local_candidates, update_sdp_origin,
    TrackDetails,
};
use crate::MEDIA_SECTION_APPLICATION;
use anyhow::Result;
use defer::defer;
use sdp::session_description::{ATTR_KEY_ICELITE, ATTR_KEY_MSID};
use sdp::util::ConnectionRole;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

pub type OnSignalingStateChangeHdlrFn = Box<
    dyn (FnMut(SignalingState) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>) + Send + Sync,
>;

pub type OnICEConnectionStateChangeHdlrFn = Box<
    dyn (FnMut(ICEConnectionState) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>)
        + Send
        + Sync,
>;

pub type OnPeerConnectionStateChangeHdlrFn = Box<
    dyn (FnMut(PeerConnectionState) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>)
        + Send
        + Sync,
>;

pub type OnDataChannelHdlrFn = Box<
    dyn (FnMut(Arc<DataChannel>) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>)
        + Send
        + Sync,
>;

pub type OnTrackHdlrFn = Box<
    dyn (FnMut(
            Option<Arc<TrackRemote>>,
            Option<Arc<RTPReceiver>>,
        ) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>)
        + Send
        + Sync,
>;

pub type OnNegotiationNeededHdlrFn =
    Box<dyn (FnMut() -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>) + Send + Sync>;

/// PeerConnection represents a WebRTC connection that establishes a
/// peer-to-peer communications with another PeerConnection instance in a
/// browser, or to another endpoint implementing the required protocols.
#[derive(Default)]
pub struct PeerConnection {
    stats_id: String,

    sdp_origin: sdp::session_description::Origin,

    // ops is an operations queue which will ensure the enqueued actions are
    // executed in order. It is used for asynchronously, but serially processing
    // remote and local descriptions
    ops: Operations,

    configuration: Configuration,

    current_local_description: Option<SessionDescription>,
    pending_local_description: Option<SessionDescription>,
    current_remote_description: Option<SessionDescription>,
    pending_remote_description: Option<SessionDescription>,
    signaling_state: AtomicU8,      //SignalingState,
    ice_connection_state: AtomicU8, //ICEConnectionState,
    connection_state: AtomicU8,     //PeerConnectionState,

    idp_login_url: Option<String>,

    is_closed: AtomicBool,              //*atomicBool
    is_negotiation_needed: AtomicBool,  //*atomicBool
    negotiation_needed_state: AtomicU8, //NegotiationNeededState,

    last_offer: String,
    last_answer: String,

    /// a value containing the last known greater mid value
    /// we internally generate mids as numbers. Needed since JSEP
    /// requires that when reusing a media section a new unique mid
    /// should be defined (see JSEP 3.4.1).
    greater_mid: isize,

    rtp_transceivers: Vec<Arc<RTPTransceiver>>,

    on_signaling_state_change_handler: Arc<Mutex<Option<OnSignalingStateChangeHdlrFn>>>,
    on_connection_state_change_handler: Arc<Mutex<Option<OnPeerConnectionStateChangeHdlrFn>>>,
    on_track_handler: Arc<Mutex<Option<OnTrackHdlrFn>>>,
    on_ice_connection_state_change_handler: Arc<Mutex<Option<OnICEConnectionStateChangeHdlrFn>>>,
    on_data_channel_handler: Arc<Mutex<Option<OnDataChannelHdlrFn>>>,
    on_negotiation_needed_handler: Arc<Mutex<Option<OnNegotiationNeededHdlrFn>>>,

    // interceptorRTCPWriter interceptor.RTCPWriter
    ice_gatherer: Arc<ICEGatherer>,
    ice_transport: Arc<ICETransport>,
    dtls_transport: Arc<DTLSTransport>,
    sctp_transport: Arc<SCTPTransport>,

    // A reference to the associated API state used by this connection
    setting_engine: Arc<SettingEngine>,
    media_engine: Arc<MediaEngine>,
    interceptor: Option<Arc<dyn Interceptor + Send + Sync>>,
}

impl PeerConnection {
    /// creates a PeerConnection with the default codecs and
    /// interceptors.  See register_default_codecs and RegisterDefaultInterceptors.
    ///
    /// If you wish to customize the set of available codecs or the set of
    /// active interceptors, create a MediaEngine and call api.new_peer_connection
    /// instead of this function.
    pub(crate) async fn new(api: &API, configuration: Configuration) -> Result<Arc<Self>> {
        // https://w3c.github.io/webrtc-pc/#constructor (Step #2)
        // Some variables defined explicitly despite their implicit zero values to
        // allow better readability to understand what is happening.
        let mut pc = PeerConnection {
            stats_id: format!(
                "PeerConnection-{}",
                SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
            ),
            configuration: Configuration {
                ice_servers: vec![],
                ice_transport_policy: ICETransportPolicy::All,
                bundle_policy: BundlePolicy::Balanced,
                rtcp_mux_policy: RTCPMuxPolicy::Require,
                peer_identity: String::new(),
                certificates: vec![],
                ice_candidate_pool_size: 0,
                sdp_semantics: SDPSemantics::default(),
            },
            ops: Operations::new(),
            is_closed: AtomicBool::new(false),
            is_negotiation_needed: AtomicBool::new(false),
            negotiation_needed_state: AtomicU8::new(NegotiationNeededState::Empty as u8),
            last_offer: "".to_owned(),
            last_answer: "".to_owned(),
            greater_mid: -1,
            signaling_state: AtomicU8::new(SignalingState::Stable as u8),
            ice_connection_state: AtomicU8::new(ICEConnectionState::New as u8),
            connection_state: AtomicU8::new(PeerConnectionState::New as u8),

            setting_engine: Arc::clone(&api.setting_engine),
            media_engine: if !api.setting_engine.disable_media_engine_copy {
                Arc::new(api.media_engine.clone_to())
            } else {
                Arc::clone(&api.media_engine)
            },
            interceptor: api.interceptor.clone(),

            ..Default::default()
        };

        pc.init_configuration(configuration)?;

        // Create the ice gatherer
        pc.ice_gatherer = Arc::new(api.new_ice_gatherer(ICEGatherOptions {
            ice_servers: pc.configuration.get_ice_servers(),
            ice_gather_policy: pc.configuration.ice_transport_policy,
        })?);

        // Create the ice transport
        pc.ice_transport = Arc::new(api.new_ice_transport(Arc::clone(&pc.ice_gatherer)));

        // Create the DTLS transport
        pc.dtls_transport = Arc::new(api.new_dtls_transport(
            Arc::clone(&pc.ice_transport),
            pc.configuration.certificates.clone(),
        )?);

        // Create the SCTP transport
        pc.sctp_transport = Arc::new(api.new_sctp_transport(Arc::clone(&pc.dtls_transport))?);

        //TODO: pc.interceptorRTCPWriter = api.interceptor.bind_rtcpwriter(interceptor.RTCPWriterFunc(pc.writeRTCP))

        let pc = Arc::new(pc);

        let pc1 = Arc::clone(&pc);
        pc.ice_transport
            .on_connection_state_change(Box::new(move |state: ICETransportState| {
                let cs = match state {
                    ICETransportState::New => ICEConnectionState::New,
                    ICETransportState::Checking => ICEConnectionState::Checking,
                    ICETransportState::Connected => ICEConnectionState::Connected,
                    ICETransportState::Completed => ICEConnectionState::Completed,
                    ICETransportState::Failed => ICEConnectionState::Failed,
                    ICETransportState::Disconnected => ICEConnectionState::Disconnected,
                    ICETransportState::Closed => ICEConnectionState::Closed,
                    _ => {
                        log::warn!("on_connection_state_change: unhandled ICE state: {}", state);
                        return Box::pin(async {});
                    }
                };
                let pc2 = Arc::clone(&pc1);
                Box::pin(async move {
                    pc2.do_ice_connection_state_change(cs).await;
                    pc2.update_connection_state(cs, pc2.dtls_transport.state())
                        .await;
                })
            }))
            .await;

        // Wire up the on datachannel handler
        let pc1 = Arc::clone(&pc);
        pc.sctp_transport
            .on_data_channel(Box::new(move |d: Arc<DataChannel>| {
                let pc2 = Arc::clone(&pc1);
                Box::pin(async move {
                    let mut handler = pc2.on_data_channel_handler.lock().await;
                    if let Some(f) = &mut *handler {
                        f(d).await;
                    }
                })
            }))
            .await;

        Ok(pc)
    }

    /// init_configuration defines validation of the specified Configuration and
    /// its assignment to the internal configuration variable. This function differs
    /// from its set_configuration counterpart because most of the checks do not
    /// include verification statements related to the existing state. Thus the
    /// function describes only minor verification of some the struct variables.
    fn init_configuration(&mut self, configuration: Configuration) -> Result<()> {
        let sanitized_ice_servers = configuration.get_ice_servers();
        if !sanitized_ice_servers.is_empty() {
            for server in &sanitized_ice_servers {
                server.validate()?;
            }
            self.configuration.ice_servers = sanitized_ice_servers;
        }

        if !configuration.peer_identity.is_empty() {
            self.configuration.peer_identity = configuration.peer_identity;
        }

        /*TODO:
        // https://www.w3.org/TR/webrtc/#constructor (step #3)
        if !configuration.certificates.is_empty() {
            now := time.Now()
            for _, x509Cert := range configuration.Certificates {
                if !x509Cert.Expires().IsZero() && now.After(x509Cert.Expires()) {
                    return &rtcerr.InvalidAccessError{Err: ErrCertificateExpired}
                }
                pc.configuration.Certificates = append(pc.configuration.Certificates, x509Cert)
            }
        } else {
            sk, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
            if err != nil {
                return &rtcerr.UnknownError{Err: err}
            }
            certificate, err := GenerateCertificate(sk)
            if err != nil {
                return err
            }
            pc.configuration.Certificates = []Certificate{*certificate}
        }  */

        if configuration.bundle_policy != BundlePolicy::Unspecified {
            self.configuration.bundle_policy = configuration.bundle_policy;
        }

        if configuration.rtcp_mux_policy != RTCPMuxPolicy::Unspecified {
            self.configuration.rtcp_mux_policy = configuration.rtcp_mux_policy;
        }

        if configuration.ice_candidate_pool_size != 0 {
            self.configuration.ice_candidate_pool_size = configuration.ice_candidate_pool_size;
        }

        if configuration.ice_transport_policy != ICETransportPolicy::Unspecified {
            self.configuration.ice_transport_policy = configuration.ice_transport_policy;
        }

        if configuration.sdp_semantics != SDPSemantics::Unspecified {
            self.configuration.sdp_semantics = configuration.sdp_semantics;
        }

        Ok(())
    }

    /// on_signaling_state_change sets an event handler which is invoked when the
    /// peer connection's signaling state changes
    pub async fn on_signaling_state_change(&self, f: OnSignalingStateChangeHdlrFn) {
        let mut on_signaling_state_change_handler =
            self.on_signaling_state_change_handler.lock().await;
        *on_signaling_state_change_handler = Some(f);
    }

    async fn do_signaling_state_change(&self, new_state: SignalingState) {
        log::info!("signaling state changed to {}", new_state);
        let mut handler = self.on_signaling_state_change_handler.lock().await;
        if let Some(f) = &mut *handler {
            f(new_state).await;
        }
    }

    /// on_data_channel sets an event handler which is invoked when a data
    /// channel message arrives from a remote peer.
    pub async fn on_data_channel(&self, f: OnDataChannelHdlrFn) {
        let mut on_data_channel_handler = self.on_data_channel_handler.lock().await;
        *on_data_channel_handler = Some(f);
    }

    /// on_negotiation_needed sets an event handler which is invoked when
    /// a change has occurred which requires session negotiation
    pub async fn on_negotiation_needed(&self, f: OnNegotiationNeededHdlrFn) {
        let mut on_negotiation_needed_handler = self.on_negotiation_needed_handler.lock().await;
        *on_negotiation_needed_handler = Some(f);
    }

    /// do_negotiation_needed enqueues negotiation_needed_op if necessary
    /// caller of this method should hold `pc.mu` lock
    async fn do_negotiation_needed(&self) {
        // https://w3c.github.io/webrtc-pc/#updating-the-negotiation-needed-flag
        // non-canon step 1
        let negotiation_needed_state: NegotiationNeededState =
            self.negotiation_needed_state.load(Ordering::SeqCst).into();
        if negotiation_needed_state == NegotiationNeededState::Run {
            self.negotiation_needed_state
                .store(NegotiationNeededState::Queue as u8, Ordering::SeqCst);
            return;
        } else if negotiation_needed_state == NegotiationNeededState::Queue {
            return;
        }
        self.negotiation_needed_state
            .store(NegotiationNeededState::Run as u8, Ordering::SeqCst);
        //TODO: pc.ops.Enqueue(pc.negotiation_needed_op)
        /*let _ = self
        .ops
        .enqueue(Operation(Box::new(move || Box::pin(async {}))))
        .await;*/
    }

    async fn negotiation_needed_op(&self) {
        // Don't run NegotiatedNeeded checks if on_negotiation_needed is not set
        {
            let handler = self.on_negotiation_needed_handler.lock().await;
            if handler.is_none() {
                return;
            }
        }

        // https://www.w3.org/TR/webrtc/#updating-the-negotiation-needed-flag
        // Step 2.1
        if self.is_closed.load(Ordering::SeqCst) {
            return;
        }
        // non-canon step 2.2
        if !self.ops.is_empty().await {
            //TODO: pc.ops.Enqueue(pc.negotiation_needed_op)
            return;
        }

        // non-canon, run again if there was a request
        defer(|| {
            if self.negotiation_needed_state.load(Ordering::SeqCst)
                == NegotiationNeededState::Queue as u8
            {
                Box::pin(async {
                    self.do_negotiation_needed().await;
                });
            } else {
                self.negotiation_needed_state
                    .store(NegotiationNeededState::Empty as u8, Ordering::SeqCst);
            }
        });

        // Step 2.3
        if self.signaling_state() != SignalingState::Stable {
            return;
        }

        // Step 2.4
        if !self.check_negotiation_needed().await {
            self.is_negotiation_needed.store(false, Ordering::SeqCst);
            return;
        }

        // Step 2.5
        if self.is_negotiation_needed.load(Ordering::SeqCst) {
            return;
        }

        // Step 2.6
        self.is_negotiation_needed.store(true, Ordering::SeqCst);

        // Step 2.7
        let mut handler = self.on_negotiation_needed_handler.lock().await;
        if let Some(f) = &mut *handler {
            f().await;
        }
    }

    async fn check_negotiation_needed(&self) -> bool {
        // To check if negotiation is needed for connection, perform the following checks:
        // Skip 1, 2 steps
        // Step 3
        if let Some(local_desc) = &self.current_local_description {
            let len_data_channel = {
                let data_channels = self.sctp_transport.data_channels.lock().await;
                data_channels.len()
            };

            if len_data_channel != 0 && have_data_channel(local_desc).is_none() {
                return true;
            }

            for t in &self.rtp_transceivers {
                // https://www.w3.org/TR/webrtc/#dfn-update-the-negotiation-needed-flag
                // Step 5.1
                // if t.stopping && !t.stopped {
                // 	return true
                // }
                let m = get_by_mid(t.mid(), local_desc);
                // Step 5.2
                if !t.stopped && m.is_none() {
                    return true;
                }
                if !t.stopped {
                    if let Some(m) = m {
                        // Step 5.3.1
                        if t.direction() == RTPTransceiverDirection::Sendrecv
                            || t.direction() == RTPTransceiverDirection::Sendonly
                        {
                            if let (Some(desc_msid), Some(sender)) =
                                (m.attribute(ATTR_KEY_MSID), t.sender())
                            {
                                if let Some(track) = &sender.track() {
                                    if desc_msid.as_str()
                                        != track.stream_id().to_owned() + " " + track.id()
                                    {
                                        return true;
                                    }
                                }
                            } else {
                                return true;
                            }
                        }
                        match local_desc.serde.sdp_type {
                            SDPType::Offer => {
                                // Step 5.3.2
                                if let Some(remote_desc) = &self.current_remote_description {
                                    if let Some(rm) = get_by_mid(t.mid(), remote_desc) {
                                        if get_peer_direction(m) != t.direction()
                                            && get_peer_direction(rm) != t.direction().reverse()
                                        {
                                            return true;
                                        }
                                    } else {
                                        return true;
                                    }
                                }
                            }
                            SDPType::Answer => {
                                // Step 5.3.3
                                if m.attribute(t.direction().to_string().as_str()).is_none() {
                                    return true;
                                }
                            }
                            _ => {}
                        };
                    }
                }
                // Step 5.4
                if t.stopped && !t.mid().is_empty() {
                    if let Some(remote_desc) = &self.current_remote_description {
                        if get_by_mid(t.mid(), local_desc).is_some()
                            || get_by_mid(t.mid(), remote_desc).is_some()
                        {
                            return true;
                        }
                    }
                }
            }
            // Step 6
            false
        } else {
            true
        }
    }

    /// on_ice_candidate sets an event handler which is invoked when a new ICE
    /// candidate is found.
    /// Take note that the handler is gonna be called with a nil pointer when
    /// gathering is finished.
    pub async fn on_ice_candidate(&self, f: OnLocalCandidateHdlrFn) {
        self.ice_gatherer.on_local_candidate(f).await
    }

    /// on_ice_gathering_state_change sets an event handler which is invoked when the
    /// ICE candidate gathering state has changed.
    pub async fn on_ice_gathering_state_change(&self, f: OnICEGathererStateChangeHdlrFn) {
        self.ice_gatherer.on_state_change(f).await
    }

    /// on_track sets an event handler which is called when remote track
    /// arrives from a remote peer.
    pub async fn on_track(&self, f: OnTrackHdlrFn) {
        let mut on_track_handler = self.on_track_handler.lock().await;
        *on_track_handler = Some(f);
    }

    async fn do_track(&self, t: Option<Arc<TrackRemote>>, r: Option<Arc<RTPReceiver>>) {
        log::debug!(
            "got new track: {}",
            if let Some(t) = &t { t.id() } else { "None" }
        );

        if t.is_some() {
            let mut handler = self.on_track_handler.lock().await;
            if let Some(f) = &mut *handler {
                f(t, r).await;
            } else {
                log::warn!("on_track unset, unable to handle incoming media streams");
            }
        }
    }

    /// on_ice_connection_state_change sets an event handler which is called
    /// when an ICE connection state is changed.
    pub async fn on_ice_connection_state_change(&self, f: OnICEConnectionStateChangeHdlrFn) {
        let mut on_ice_connection_state_change_handler =
            self.on_ice_connection_state_change_handler.lock().await;
        *on_ice_connection_state_change_handler = Some(f);
    }

    async fn do_ice_connection_state_change(&self, cs: ICEConnectionState) {
        self.ice_connection_state.store(cs as u8, Ordering::SeqCst);

        log::info!("ICE connection state changed: {}", cs);
        let mut handler = self.on_ice_connection_state_change_handler.lock().await;
        if let Some(f) = &mut *handler {
            f(cs).await;
        }
    }

    /// on_connection_state_change sets an event handler which is called
    /// when the PeerConnectionState has changed
    pub async fn on_connection_state_change(&self, f: OnPeerConnectionStateChangeHdlrFn) {
        let mut on_connection_state_change_handler =
            self.on_connection_state_change_handler.lock().await;
        *on_connection_state_change_handler = Some(f);
    }

    async fn do_connection_state_change(&self, cs: PeerConnectionState) {
        log::info!("Peer connection state changed: {}", cs);
        let mut handler = self.on_connection_state_change_handler.lock().await;
        if let Some(f) = &mut *handler {
            f(cs).await;
        }
    }

    /// set_configuration updates the configuration of this PeerConnection object.
    pub async fn set_configuration(&mut self, configuration: Configuration) -> Result<()> {
        //nolint:gocognit
        // https://www.w3.org/TR/webrtc/#dom-rtcpeerconnection-setconfiguration (step #2)
        if self.is_closed.load(Ordering::SeqCst) {
            return Err(Error::ErrConnectionClosed.into());
        }

        // https://www.w3.org/TR/webrtc/#set-the-configuration (step #3)
        if !configuration.peer_identity.is_empty() {
            if configuration.peer_identity != self.configuration.peer_identity {
                return Err(Error::ErrModifyingPeerIdentity.into());
            }
            self.configuration.peer_identity = configuration.peer_identity;
        }

        // https://www.w3.org/TR/webrtc/#set-the-configuration (step #4)
        if !configuration.certificates.is_empty() {
            if configuration.certificates.len() != self.configuration.certificates.len() {
                return Err(Error::ErrModifyingCertificates.into());
            }

            /*TODO: for (i, certificate) in configuration.certificates.iter().enumerate() {
                if !self.configuration.certificates[i].Equals(certificate) {
                    return Err(Error::ErrModifyingCertificates.into());
                }
            }*/
            self.configuration.certificates = configuration.certificates;
        }

        // https://www.w3.org/TR/webrtc/#set-the-configuration (step #5)
        if configuration.bundle_policy != BundlePolicy::Unspecified {
            if configuration.bundle_policy != self.configuration.bundle_policy {
                return Err(Error::ErrModifyingBundlePolicy.into());
            }
            self.configuration.bundle_policy = configuration.bundle_policy;
        }

        // https://www.w3.org/TR/webrtc/#set-the-configuration (step #6)
        if configuration.rtcp_mux_policy != RTCPMuxPolicy::Unspecified {
            if configuration.rtcp_mux_policy != self.configuration.rtcp_mux_policy {
                return Err(Error::ErrModifyingRTCPMuxPolicy.into());
            }
            self.configuration.rtcp_mux_policy = configuration.rtcp_mux_policy;
        }

        // https://www.w3.org/TR/webrtc/#set-the-configuration (step #7)
        if configuration.ice_candidate_pool_size != 0 {
            if self.configuration.ice_candidate_pool_size != configuration.ice_candidate_pool_size
                && self.local_description().await.is_some()
            {
                return Err(Error::ErrModifyingICECandidatePoolSize.into());
            }
            self.configuration.ice_candidate_pool_size = configuration.ice_candidate_pool_size;
        }

        // https://www.w3.org/TR/webrtc/#set-the-configuration (step #8)
        if configuration.ice_transport_policy != ICETransportPolicy::Unspecified {
            self.configuration.ice_transport_policy = configuration.ice_transport_policy
        }

        // https://www.w3.org/TR/webrtc/#set-the-configuration (step #11)
        if !configuration.ice_servers.is_empty() {
            // https://www.w3.org/TR/webrtc/#set-the-configuration (step #11.3)
            for server in &configuration.ice_servers {
                server.validate()?;
            }
            self.configuration.ice_servers = configuration.ice_servers
        }
        Ok(())
    }

    /// get_configuration returns a Configuration object representing the current
    /// configuration of this PeerConnection object. The returned object is a
    /// copy and direct mutation on it will not take affect until set_configuration
    /// has been called with Configuration passed as its only argument.
    /// https://www.w3.org/TR/webrtc/#dom-rtcpeerconnection-getconfiguration
    pub fn get_configuration(&self) -> &Configuration {
        &self.configuration
    }

    fn get_stats_id(&self) -> &str {
        self.stats_id.as_str()
    }

    /// has_local_description_changed returns whether local media (rtp_transceivers) has changed
    /// caller of this method should hold `pc.mu` lock
    fn has_local_description_changed(&self, desc: &SessionDescription) -> bool {
        for t in &self.rtp_transceivers {
            if let Some(m) = get_by_mid(t.mid(), desc) {
                if get_peer_direction(m) != t.direction() {
                    return true;
                }
            } else {
                return true;
            }
        }
        false
    }

    /// create_offer starts the PeerConnection and generates the localDescription
    /// https://w3c.github.io/webrtc-pc/#dom-rtcpeerconnection-createoffer
    pub async fn create_offer(
        &mut self,
        options: Option<OfferOptions>,
    ) -> Result<SessionDescription> {
        let use_identity = self.idp_login_url.is_some();
        if use_identity {
            return Err(Error::ErrIdentityProviderNotImplemented.into());
        } else if self.is_closed.load(Ordering::SeqCst) {
            return Err(Error::ErrConnectionClosed.into());
        }

        if let Some(options) = options {
            if options.ice_restart {
                self.ice_transport.restart().await?;
            }
        }

        // This may be necessary to recompute if, for example, createOffer was called when only an
        // audio RTCRtpTransceiver was added to connection, but while performing the in-parallel
        // steps to create an offer, a video RTCRtpTransceiver was added, requiring additional
        // inspection of video system resources.
        let mut count = 0;
        let mut offer;

        loop {
            // We cache current transceivers to ensure they aren't
            // mutated during offer generation. We later check if they have
            // been mutated and recompute the offer if necessary.
            let current_transceivers = &mut self.rtp_transceivers;

            // in-parallel steps to create an offer
            // https://w3c.github.io/webrtc-pc/#dfn-in-parallel-steps-to-create-an-offer
            let is_plan_b = if self.current_remote_description.is_some() {
                description_is_plan_b(self.current_remote_description.as_ref())?
            } else {
                self.configuration.sdp_semantics == SDPSemantics::PlanB
            };

            // include unmatched local transceivers
            if !is_plan_b {
                // update the greater mid if the remote description provides a greater one
                if let Some(current_remote_description) = &self.current_remote_description {
                    if let Some(parsed) = &current_remote_description.parsed {
                        for media in &parsed.media_descriptions {
                            if let Some(mid) = get_mid_value(media) {
                                if mid.is_empty() {
                                    continue;
                                }
                                let numeric_mid = match mid.parse::<isize>() {
                                    Ok(n) => n,
                                    Err(_) => continue,
                                };
                                if numeric_mid > self.greater_mid {
                                    self.greater_mid = numeric_mid;
                                }
                            }
                        }
                    }
                }
                for t in current_transceivers {
                    if !t.mid().is_empty() {
                        continue;
                    }
                    self.greater_mid += 1;
                    //TODO: t.set_mid(format!("{}", self.greater_mid))?;
                }
            }

            let mut d = if self.current_remote_description.is_none() {
                self.generate_unmatched_sdp(/*current_transceivers,*/ use_identity)?
            } else {
                self.generate_matched_sdp(
                    /*current_transceivers,*/
                    use_identity,
                    true, /*includeUnmatched */
                    DEFAULT_DTLS_ROLE_OFFER.to_connection_role(),
                )?
            };

            update_sdp_origin(&mut self.sdp_origin, &mut d);
            let sdp = d.marshal();

            offer = SessionDescription {
                serde: SessionDescriptionSerde {
                    sdp_type: SDPType::Offer,
                    sdp,
                },
                parsed: Some(d),
            };

            // Verify local media hasn't changed during offer
            // generation. Recompute if necessary
            if is_plan_b || !self.has_local_description_changed(&offer) {
                break;
            }
            count += 1;
            if count >= 128 {
                return Err(Error::ErrExcessiveRetries.into());
            }
        }

        self.last_offer = offer.serde.sdp.clone();
        Ok(offer)
    }

    /// Update the PeerConnectionState given the state of relevant transports
    /// https://www.w3.org/TR/webrtc/#rtcpeerconnectionstate-enum
    async fn update_connection_state(
        &self,
        ice_connection_state: ICEConnectionState,
        dtls_transport_state: DTLSTransportState,
    ) {
        let  connection_state =
        // The RTCPeerConnection object's [[IsClosed]] slot is true.
        if self.is_closed.load(Ordering::SeqCst) {
             PeerConnectionState::Closed
        }else if ice_connection_state == ICEConnectionState::Failed || dtls_transport_state == DTLSTransportState::Failed {
            // Any of the RTCIceTransports or RTCDtlsTransports are in a "failed" state.
             PeerConnectionState::Failed
        }else if ice_connection_state == ICEConnectionState::Disconnected {
            // Any of the RTCIceTransports or RTCDtlsTransports are in the "disconnected"
            // state and none of them are in the "failed" or "connecting" or "checking" state.
            PeerConnectionState::Disconnected
        }else if ice_connection_state == ICEConnectionState::Connected && dtls_transport_state == DTLSTransportState::Connected {
            // All RTCIceTransports and RTCDtlsTransports are in the "connected", "completed" or "closed"
            // state and at least one of them is in the "connected" or "completed" state.
            PeerConnectionState::Connected
        }else if ice_connection_state == ICEConnectionState::Checking && dtls_transport_state == DTLSTransportState::Connecting{
        //  Any of the RTCIceTransports or RTCDtlsTransports are in the "connecting" or
        // "checking" state and none of them is in the "failed" state.
             PeerConnectionState::Connecting
        }else{
            PeerConnectionState::New
        };

        if self.connection_state.load(Ordering::SeqCst) == connection_state as u8 {
            return;
        }

        log::info!("peer connection state changed: {}", connection_state);
        self.connection_state
            .store(connection_state as u8, Ordering::SeqCst);

        self.do_connection_state_change(connection_state).await;
    }

    /// create_answer starts the PeerConnection and generates the localDescription
    pub fn create_answer(&mut self, _options: Option<AnswerOptions>) -> Result<SessionDescription> {
        let use_identity = self.idp_login_url.is_some();
        if self.remote_description().is_none() {
            return Err(Error::ErrNoRemoteDescription.into());
        } else if use_identity {
            return Err(Error::ErrIdentityProviderNotImplemented.into());
        } else if self.is_closed.load(Ordering::SeqCst) {
            return Err(Error::ErrConnectionClosed.into());
        } else if self.signaling_state() != SignalingState::HaveRemoteOffer
            && self.signaling_state() != SignalingState::HaveLocalPranswer
        {
            return Err(Error::ErrIncorrectSignalingState.into());
        }

        let mut connection_role = self.setting_engine.answering_dtls_role.to_connection_role();
        if connection_role == ConnectionRole::Unspecified {
            connection_role = DEFAULT_DTLS_ROLE_ANSWER.to_connection_role();
        }

        let mut d = self.generate_matched_sdp(
            /*self.rtp_transceivers,*/ use_identity,
            false, /*includeUnmatched */
            connection_role,
        )?;

        update_sdp_origin(&mut self.sdp_origin, &mut d);
        let sdp = d.marshal();

        let answer = SessionDescription {
            serde: SessionDescriptionSerde {
                sdp_type: SDPType::Answer,
                sdp,
            },
            parsed: Some(d),
        };

        self.last_answer = answer.serde.sdp.clone();
        Ok(answer)
    }

    // 4.4.1.6 Set the SessionDescription
    pub(crate) async fn set_description(
        &mut self,
        sd: &SessionDescription,
        op: StateChangeOp,
    ) -> Result<()> {
        if self.is_closed.load(Ordering::SeqCst) {
            return Err(Error::ErrConnectionClosed.into());
        } else if sd.serde.sdp_type == SDPType::Unspecified {
            return Err(Error::ErrPeerConnSDPTypeInvalidValue.into());
        }

        let next_state = {
            let cur = self.signaling_state();
            let new_sdpdoes_not_match_offer = Error::ErrSDPDoesNotMatchOffer;
            let new_sdpdoes_not_match_answer = Error::ErrSDPDoesNotMatchAnswer;

            match op {
                StateChangeOp::SetLocal => {
                    match sd.serde.sdp_type {
                        // stable->SetLocal(offer)->have-local-offer
                        SDPType::Offer => {
                            if sd.serde.sdp != self.last_offer {
                                Err(new_sdpdoes_not_match_offer.into())
                            } else {
                                let next_state = check_next_signaling_state(
                                    cur,
                                    SignalingState::HaveLocalOffer,
                                    StateChangeOp::SetLocal,
                                    sd.serde.sdp_type,
                                );
                                if next_state.is_ok() {
                                    self.pending_local_description = Some(sd.clone());
                                }
                                next_state
                            }
                        }
                        // have-remote-offer->SetLocal(answer)->stable
                        // have-local-pranswer->SetLocal(answer)->stable
                        SDPType::Answer => {
                            if sd.serde.sdp != self.last_answer {
                                Err(new_sdpdoes_not_match_answer.into())
                            } else {
                                let next_state = check_next_signaling_state(
                                    cur,
                                    SignalingState::Stable,
                                    StateChangeOp::SetLocal,
                                    sd.serde.sdp_type,
                                );
                                if next_state.is_ok() {
                                    self.current_local_description = Some(sd.clone());
                                    self.current_remote_description =
                                        self.pending_remote_description.take();
                                    self.pending_remote_description = None;
                                    self.pending_local_description = None;
                                }
                                next_state
                            }
                        }
                        SDPType::Rollback => {
                            let next_state = check_next_signaling_state(
                                cur,
                                SignalingState::Stable,
                                StateChangeOp::SetLocal,
                                sd.serde.sdp_type,
                            );
                            if next_state.is_ok() {
                                self.pending_local_description = None;
                            }
                            next_state
                        }
                        // have-remote-offer->SetLocal(pranswer)->have-local-pranswer
                        SDPType::Pranswer => {
                            if sd.serde.sdp != self.last_answer {
                                Err(new_sdpdoes_not_match_answer.into())
                            } else {
                                let next_state = check_next_signaling_state(
                                    cur,
                                    SignalingState::HaveLocalPranswer,
                                    StateChangeOp::SetLocal,
                                    sd.serde.sdp_type,
                                );
                                if next_state.is_ok() {
                                    self.pending_local_description = Some(sd.clone());
                                }
                                next_state
                            }
                        }
                        _ => Err(Error::ErrPeerConnStateChangeInvalid.into()),
                    }
                }
                StateChangeOp::SetRemote => {
                    match sd.serde.sdp_type {
                        // stable->SetRemote(offer)->have-remote-offer
                        SDPType::Offer => {
                            let next_state = check_next_signaling_state(
                                cur,
                                SignalingState::HaveRemoteOffer,
                                StateChangeOp::SetRemote,
                                sd.serde.sdp_type,
                            );
                            if next_state.is_ok() {
                                self.pending_remote_description = Some(sd.clone());
                            }
                            next_state
                        }
                        // have-local-offer->SetRemote(answer)->stable
                        // have-remote-pranswer->SetRemote(answer)->stable
                        SDPType::Answer => {
                            let next_state = check_next_signaling_state(
                                cur,
                                SignalingState::Stable,
                                StateChangeOp::SetRemote,
                                sd.serde.sdp_type,
                            );
                            if next_state.is_ok() {
                                self.current_remote_description = Some(sd.clone());
                                self.current_local_description =
                                    self.pending_local_description.take();
                                self.pending_remote_description = None;
                                self.pending_local_description = None;
                            }
                            next_state
                        }
                        SDPType::Rollback => {
                            let next_state = check_next_signaling_state(
                                cur,
                                SignalingState::Stable,
                                StateChangeOp::SetRemote,
                                sd.serde.sdp_type,
                            );
                            if next_state.is_ok() {
                                self.pending_remote_description = None;
                            }
                            next_state
                        }
                        // have-local-offer->SetRemote(pranswer)->have-remote-pranswer
                        SDPType::Pranswer => {
                            let next_state = check_next_signaling_state(
                                cur,
                                SignalingState::HaveRemotePranswer,
                                StateChangeOp::SetRemote,
                                sd.serde.sdp_type,
                            );
                            if next_state.is_ok() {
                                self.pending_remote_description = Some(sd.clone());
                            }
                            next_state
                        }
                        _ => Err(Error::ErrPeerConnStateChangeInvalid.into()),
                    }
                } //_ => Err(Error::ErrPeerConnStateChangeUnhandled.into()),
            }
        };

        match next_state {
            Ok(next_state) => {
                self.signaling_state
                    .store(next_state as u8, Ordering::SeqCst);
                if self.signaling_state() == SignalingState::Stable {
                    self.is_negotiation_needed.store(false, Ordering::SeqCst);
                    self.do_negotiation_needed().await;
                }
                self.do_signaling_state_change(next_state).await;
                Ok(())
            }
            Err(err) => Err(err),
        }
    }

    /// set_local_description sets the SessionDescription of the local peer
    pub async fn set_local_description(&mut self, mut desc: SessionDescription) -> Result<()> {
        if self.is_closed.load(Ordering::SeqCst) {
            return Err(Error::ErrConnectionClosed.into());
        }

        let _have_local_description = self.current_local_description.is_some();

        // JSEP 5.4
        if desc.serde.sdp.is_empty() {
            match desc.serde.sdp_type {
                SDPType::Answer | SDPType::Pranswer => {
                    desc.serde.sdp = self.last_answer.clone();
                }
                SDPType::Offer => {
                    desc.serde.sdp = self.last_offer.clone();
                }
                _ => return Err(Error::ErrPeerConnSDPTypeInvalidValueSetLocalDescription.into()),
            }
        }

        desc.parsed = Some(desc.unmarshal()?);
        self.set_description(&desc, StateChangeOp::SetLocal).await?;

        let current_transceivers = self.get_transceivers();

        let we_answer = desc.serde.sdp_type == SDPType::Answer;
        let remote_desc = self.remote_description();
        if we_answer && remote_desc.is_some() {
            self.start_rtp_senders(current_transceivers).await?;
            /*TODO:pc.ops.Enqueue(func() {
                pc.startRTP(have_local_description, remote_desc, current_transceivers)
            })*/
        }

        if self.ice_gatherer.state() == ICEGathererState::New {
            self.ice_gatherer.gather().await
        } else {
            Ok(())
        }
    }

    /// local_description returns PendingLocalDescription if it is not null and
    /// otherwise it returns CurrentLocalDescription. This property is used to
    /// determine if set_local_description has already been called.
    /// https://www.w3.org/TR/webrtc/#dom-rtcpeerconnection-localdescription
    pub async fn local_description(&self) -> Option<SessionDescription> {
        if let Some(pending_local_description) = self.pending_local_description().await {
            return Some(pending_local_description);
        }
        self.current_local_description().await
    }

    /// set_remote_description sets the SessionDescription of the remote peer
    pub async fn set_remote_description(&mut self, mut desc: SessionDescription) -> Result<()> {
        if self.is_closed.load(Ordering::SeqCst) {
            return Err(Error::ErrConnectionClosed.into());
        }

        let is_renegotation = self.current_remote_description.is_some();

        desc.parsed = Some(desc.unmarshal()?);
        self.set_description(&desc, StateChangeOp::SetRemote)
            .await?;

        if let Some(parsed) = &desc.parsed {
            self.media_engine
                .update_from_remote_description(parsed)
                .await?;

            let mut local_transceivers = self.get_transceivers().to_vec();
            let detected_plan_b = description_is_plan_b(self.remote_description())?;
            let we_offer = desc.serde.sdp_type == SDPType::Answer;

            if !we_offer && !detected_plan_b {
                let desc = self.remote_description();
                if let Some(desc) = desc {
                    if let Some(parsed) = &desc.parsed {
                        for media in &parsed.media_descriptions {
                            if let Some(mid_value) = get_mid_value(media) {
                                if mid_value.is_empty() {
                                    return Err(
                                        Error::ErrPeerConnRemoteDescriptionWithoutMidValue.into()
                                    );
                                }

                                if media.media_name.media == MEDIA_SECTION_APPLICATION {
                                    continue;
                                }

                                let kind = RTPCodecType::from(media.media_name.media.as_str());
                                let direction = get_peer_direction(media);
                                if kind == RTPCodecType::Unspecified
                                    || direction == RTPTransceiverDirection::Unspecified
                                {
                                    continue;
                                }

                                let t = if let Some(t) =
                                    find_by_mid(mid_value, &mut local_transceivers)
                                {
                                    //TODO:  t.stop().await?;
                                    Some(t)
                                } else {
                                    satisfy_type_and_direction(
                                        kind,
                                        direction,
                                        &mut local_transceivers,
                                    )
                                };

                                if let Some(t) = t {
                                    if direction == RTPTransceiverDirection::Recvonly {
                                        if t.direction() == RTPTransceiverDirection::Sendrecv {
                                            t.set_direction(RTPTransceiverDirection::Sendonly);
                                        }
                                    } else if direction == RTPTransceiverDirection::Sendrecv
                                        && t.direction() == RTPTransceiverDirection::Sendonly
                                    {
                                        t.set_direction(RTPTransceiverDirection::Sendrecv);
                                    }

                                    if t.mid().is_empty() {
                                        //TODO: t.set_mid(midValue)?;
                                    }
                                } else {
                                    let receiver = API::new_rtp_receiver(
                                        kind,
                                        Arc::clone(&self.dtls_transport),
                                        Arc::clone(&self.media_engine),
                                        self.interceptor.clone(),
                                    );

                                    let local_direction =
                                        if direction == RTPTransceiverDirection::Recvonly {
                                            RTPTransceiverDirection::Sendonly
                                        } else {
                                            RTPTransceiverDirection::Recvonly
                                        };

                                    let t = Arc::new(RTPTransceiver::new(
                                        Some(receiver),
                                        None,
                                        local_direction,
                                        kind,
                                        vec![],
                                        Arc::clone(&self.media_engine),
                                    ));
                                    //TODO: self.add_rtp_transceiver(Arc::clone(&t));

                                    if t.mid().is_empty() {
                                        //TODO: t.set_mid(midValue)?;
                                    }
                                }
                            }
                        }
                    }
                }
            }

            let (remote_ufrag, remote_pwd, candidates) = extract_ice_details(parsed).await?;

            if is_renegotation
                && self
                    .ice_transport
                    .have_remote_credentials_change(&remote_ufrag, &remote_pwd)
                    .await
            {
                // An ICE Restart only happens implicitly for a set_remote_description of type offer
                if !we_offer {
                    self.ice_transport.restart().await?;
                }

                self.ice_transport
                    .set_remote_credentials(remote_ufrag, remote_pwd)
                    .await?;
            }

            for candidate in candidates {
                self.ice_transport
                    .add_remote_candidate(Some(candidate))
                    .await?;
            }

            let current_transceivers = self.get_transceivers();

            if is_renegotation {
                if we_offer {
                    self.start_rtp_senders(current_transceivers).await?;
                    /*TODO: self.ops.Enqueue(func() {
                        self.startRTP(true, &desc, current_transceivers)
                    })*/
                }
                return Ok(());
            }

            let mut remote_is_lite = false;
            for a in &parsed.attributes {
                if a.key.trim() == ATTR_KEY_ICELITE {
                    remote_is_lite = true;
                    break;
                }
            }

            let (_fingerprint, _fingerprint_hash) = extract_fingerprint(parsed)?;

            let _ice_role =
            // If one of the agents is lite and the other one is not, the lite agent must be the controlling agent.
            // If both or neither agents are lite the offering agent is controlling.
            // RFC 8445 S6.1.1
            if (we_offer && remote_is_lite == self.setting_engine.candidates.ice_lite)
                || (remote_is_lite && !self.setting_engine.candidates.ice_lite)
            {
                ICERole::Controlling
            }else{
                ICERole::Controlled
            };

            // Start the networking in a new routine since it will block until
            // the connection is actually established.
            if we_offer {
                self.start_rtp_senders(current_transceivers).await?;
            }

            /*TODO: self.ops.Enqueue(func() {
                self.startTransports(iceRole, dtlsRoleFromRemoteSDP(desc.parsed), remote_ufrag, remote_pwd, fingerprint, fingerprintHash)
                if weOffer {
                    self.startRTP(false, &desc, current_transceivers)
                }
            })*/
        }

        Ok(())
    }

    async fn start_receiver(&self, incoming: TrackDetails, _receiver: Arc<RTPReceiver>) {
        let mut encodings = vec![];
        if incoming.ssrc != 0 {
            encodings.push(RTPCodingParameters {
                ssrc: incoming.ssrc,
                ..Default::default()
            });
        }
        for rid in incoming.rids {
            encodings.push(RTPCodingParameters {
                rid,
                ..Default::default()
            });
        }
        /*
        if let Err(err) = receiver.receive(&RTPReceiveParameters { encodings }).await {
            log::warn!("RTPReceiver Receive failed {}", err);
            return;
        }

        // set track id and label early so they can be set as new track information
        // is received from the SDP.
        for track_streams in &receiver.tracks {
            track_streams.track.id = incoming.id;
            track_streams.track.stream_id = incoming.stream_id;
        }

        // We can't block and wait for a single SSRC
        if incoming.ssrc == 0 {
            return;
        }

        let media_engine = Arc::clone(&self.media_engine);
        tokio::spawn(async move {
            if let Some(track) = receiver.track() {
                if let Err(err) = track.determine_payload_type() {
                    log::warn!("Could not determine PayloadType for SSRC {}", track.ssrc());
                    return;
                }

                let params =
                    match media_engine.get_rtp_parameters_by_payload_type(track.payload_type()) {
                        Ok(params) => params,
                        Err(err) => {
                            log::warnf(
                                "no codec could be found for payloadType {}",
                                track.payload_type(),
                            );
                            return;
                        }
                    };

                track.kind = receiver.kind;
                track.codec = params.Codecs[0];
                track.params = params;

                self.onTrack(receiver.Track(), receiver)
            }
        });*/
    }
    /*
    // startRTPReceivers opens knows inbound SRTP streams from the remote_description
    func (pc *PeerConnection) startRTPReceivers(incomingTracks []TrackDetails, currentTransceivers []*RTPTransceiver) { //nolint:gocognit
        localTransceivers := append([]*RTPTransceiver{}, currentTransceivers...)

        remoteIsPlanB := false
        switch self.configuration.SDPSemantics {
        case SDPSemanticsPlanB:
            remoteIsPlanB = true
        case SDPSemanticsUnifiedPlanWithFallback:
            remoteIsPlanB = description_is_plan_b(self.remote_description())
        default:
            // none
        }

        // Ensure we haven't already started a transceiver for this ssrc
        for i := range incomingTracks {
            if len(incomingTracks) <= i {
                break
            }
            incomingTrack := incomingTracks[i]

            for _, t := range localTransceivers {
                if (t.Receiver()) == nil || t.Receiver().Track() == nil || t.Receiver().Track().ssrc != incomingTrack.ssrc {
                    continue
                }

                incomingTracks = filter_track_with_ssrc(incomingTracks, incomingTrack.ssrc)
            }
        }

        unhandledTracks := incomingTracks[:0]
        for i := range incomingTracks {
            trackHandled := false
            for j := range localTransceivers {
                t := localTransceivers[j]
                incomingTrack := incomingTracks[i]

                if t.Mid() != incomingTrack.mid {
                    continue
                }

                if (incomingTrack.kind != t.kind) ||
                    (t.Direction() != RTPTransceiverDirectionRecvonly && t.Direction() != RTPTransceiverDirectionSendrecv) ||
                    (t.Receiver()) == nil ||
                    (t.Receiver().haveReceived()) {
                    continue
                }

                self.start_receiver(incomingTrack, t.Receiver())
                trackHandled = true
                break
            }

            if !trackHandled {
                unhandledTracks = append(unhandledTracks, incomingTracks[i])
            }
        }

        if remoteIsPlanB {
            for _, incoming := range unhandledTracks {
                t, err := self.AddTransceiverFromKind(incoming.kind, RTPTransceiverInit{
                    Direction: RTPTransceiverDirectionSendrecv,
                })
                if err != nil {
                    self.log.Warnf("Could not add transceiver for remote SSRC %d: %s", incoming.ssrc, err)
                    continue
                }
                self.start_receiver(incoming, t.Receiver())
            }
        }
    }*/

    /// start_rtp_senders starts all outbound RTP streams
    pub(crate) async fn start_rtp_senders(
        &self,
        current_transceivers: &[Arc<RTPTransceiver>],
    ) -> Result<()> {
        for transceiver in current_transceivers {
            if let Some(sender) = transceiver.sender() {
                if sender.is_negotiated() && !sender.has_sent() {
                    //TODO: sender.send(&sender.get_parameters()).await?;
                }
            }
        }

        Ok(())
    }
    /*
    // Start SCTP subsystem
    func (pc *PeerConnection) startSCTP() {
        // Start sctp
        if err := self.sctpTransport.Start(SCTPCapabilities{
            MaxMessageSize: 0,
        }); err != nil {
            self.log.Warnf("Failed to start SCTP: %s", err)
            if err = self.sctpTransport.Stop(); err != nil {
                self.log.Warnf("Failed to stop SCTPTransport: %s", err)
            }

            return
        }

        // DataChannels that need to be opened now that SCTP is available
        // make a copy we may have incoming DataChannels mutating this while we open
        self.sctpTransport.lock.RLock()
        dataChannels := append([]*DataChannel{}, self.sctpTransport.dataChannels...)
        self.sctpTransport.lock.RUnlock()

        var openedDCCount uint32
        for _, d := range dataChannels {
            if d.ReadyState() == DataChannelStateConnecting {
                err := d.open(self.sctpTransport)
                if err != nil {
                    self.log.Warnf("failed to open data channel: %s", err)
                    continue
                }
                openedDCCount++
            }
        }

        self.sctpTransport.lock.Lock()
        self.sctpTransport.dataChannelsOpened += openedDCCount
        self.sctpTransport.lock.Unlock()
    }

    func (pc *PeerConnection) handleUndeclaredSSRC(rtpStream io.Reader, ssrc SSRC) error { //nolint:gocognit
        remoteDescription := self.remote_description()
        if remoteDescription == nil {
            return errPeerConnRemoteDescriptionNil
        }

        // If the remote SDP was only one media section the ssrc doesn't have to be explicitly declared
        if len(remoteDescription.parsed.MediaDescriptions) == 1 {
            onlyMediaSection := remoteDescription.parsed.MediaDescriptions[0]
            for _, a := range onlyMediaSection.Attributes {
                if a.Key == ssrcStr {
                    return errPeerConnSingleMediaSectionHasExplicitSSRC
                }
            }

            incoming := TrackDetails{
                ssrc: ssrc,
                kind: RTPCodecTypeVideo,
            }
            if onlyMediaSection.MediaName.Media == RTPCodecTypeAudio.String() {
                incoming.kind = RTPCodecTypeAudio
            }

            t, err := self.AddTransceiverFromKind(incoming.kind, RTPTransceiverInit{
                Direction: RTPTransceiverDirectionSendrecv,
            })
            if err != nil {
                return fmt.Errorf("%w: %d: %s", errPeerConnRemoteSSRCAddTransceiver, ssrc, err)
            }
            self.start_receiver(incoming, t.Receiver())
            return nil
        }

        midExtensionID, audioSupported, videoSupported := self.api.mediaEngine.getHeaderExtensionID(RTPHeaderExtensionCapability{sdp.SDESMidURI})
        if !audioSupported && !videoSupported {
            return errPeerConnSimulcastMidRTPExtensionRequired
        }

        streamIDExtensionID, audioSupported, videoSupported := self.api.mediaEngine.getHeaderExtensionID(RTPHeaderExtensionCapability{sdp.SDESRTPStreamIDURI})
        if !audioSupported && !videoSupported {
            return errPeerConnSimulcastStreamIDRTPExtensionRequired
        }

        b := make([]byte, receiveMTU)
        var mid, rid string
        for readCount := 0; readCount <= simulcastProbeCount; readCount++ {
            i, err := rtpStream.Read(b)
            if err != nil {
                return err
            }

            maybeMid, maybeRid, payloadType, err := handleUnknownRTPPacket(b[:i], uint8(midExtensionID), uint8(streamIDExtensionID))
            if err != nil {
                return err
            }

            if maybeMid != "" {
                mid = maybeMid
            }
            if maybeRid != "" {
                rid = maybeRid
            }

            if mid == "" || rid == "" {
                continue
            }

            params, err := self.api.mediaEngine.getRTPParametersByPayloadType(payloadType)
            if err != nil {
                return err
            }

            for _, t := range self.get_transceivers() {
                if t.Mid() != mid || t.Receiver() == nil {
                    continue
                }

                track, err := t.Receiver().receiveForRid(rid, params, ssrc)
                if err != nil {
                    return err
                }
                self.onTrack(track, t.Receiver())
                return nil
            }
        }

        return errPeerConnSimulcastIncomingSSRCFailed
    }

    // undeclaredMediaProcessor handles RTP/RTCP packets that don't match any a:ssrc lines
    func (pc *PeerConnection) undeclaredMediaProcessor() {
        go func() {
            var simulcastRoutineCount uint64
            for {
                srtpSession, err := self.dtlsTransport.getSRTPSession()
                if err != nil {
                    self.log.Warnf("undeclaredMediaProcessor failed to open SrtpSession: %v", err)
                    return
                }

                stream, ssrc, err := srtpSession.AcceptStream()
                if err != nil {
                    self.log.Warnf("Failed to accept RTP %v", err)
                    return
                }

                if self.is_closed.get() {
                    if err = stream.Close(); err != nil {
                        self.log.Warnf("Failed to close RTP stream %v", err)
                    }
                    continue
                }

                if atomic.AddUint64(&simulcastRoutineCount, 1) >= simulcastMaxProbeRoutines {
                    atomic.AddUint64(&simulcastRoutineCount, ^uint64(0))
                    self.log.Warn(ErrSimulcastProbeOverflow.Error())
                    continue
                }

                go func(rtpStream io.Reader, ssrc SSRC) {
                    self.dtlsTransport.storeSimulcastStream(stream)

                    if err := self.handleUndeclaredSSRC(rtpStream, ssrc); err != nil {
                        self.log.Errorf("Incoming unhandled RTP ssrc(%d), on_track will not be fired. %v", ssrc, err)
                    }
                    atomic.AddUint64(&simulcastRoutineCount, ^uint64(0))
                }(stream, SSRC(ssrc))
            }
        }()

        go func() {
            for {
                srtcpSession, err := self.dtlsTransport.getSRTCPSession()
                if err != nil {
                    self.log.Warnf("undeclaredMediaProcessor failed to open SrtcpSession: %v", err)
                    return
                }

                _, ssrc, err := srtcpSession.AcceptStream()
                if err != nil {
                    self.log.Warnf("Failed to accept RTCP %v", err)
                    return
                }
                self.log.Warnf("Incoming unhandled RTCP ssrc(%d), on_track will not be fired", ssrc)
            }
        }()
    }*/

    /// remote_description returns pending_remote_description if it is not null and
    /// otherwise it returns current_remote_description. This property is used to
    /// determine if setRemoteDescription has already been called.
    /// https://www.w3.org/TR/webrtc/#dom-rtcpeerconnection-remotedescription
    pub fn remote_description(&self) -> Option<&SessionDescription> {
        if self.pending_remote_description.is_some() {
            self.pending_remote_description.as_ref()
        } else {
            self.current_remote_description.as_ref()
        }
    }
    /*
    // AddICECandidate accepts an ICE candidate string and adds it
    // to the existing set of candidates.
    func (pc *PeerConnection) AddICECandidate(candidate ICECandidateInit) error {
        if self.remote_description() == nil {
            return &rtcerr.InvalidStateError{Err: ErrNoRemoteDescription}
        }

        candidateValue := strings.TrimPrefix(candidate.Candidate, "candidate:")

        var iceCandidate *ICECandidate
        if candidateValue != "" {
            candidate, err := ice.UnmarshalCandidate(candidateValue)
            if err != nil {
                return err
            }

            c, err := newICECandidateFromICE(candidate)
            if err != nil {
                return err
            }
            iceCandidate = &c
        }

        return self.iceTransport.AddRemoteCandidate(iceCandidate)
    }

    // ICEConnectionState returns the ICE connection state of the
    // PeerConnection instance.
    func (pc *PeerConnection) ICEConnectionState() ICEConnectionState {
        self.mu.RLock()
        defer self.mu.RUnlock()

        return self.ice_connection_state
    }

    // GetSenders returns the RTPSender that are currently attached to this PeerConnection
    func (pc *PeerConnection) GetSenders() (result []*RTPSender) {
        self.mu.Lock()
        defer self.mu.Unlock()

        for _, transceiver := range self.rtp_transceivers {
            if transceiver.Sender() != nil {
                result = append(result, transceiver.Sender())
            }
        }
        return result
    }

    // GetReceivers returns the RTPReceivers that are currently attached to this PeerConnection
    func (pc *PeerConnection) GetReceivers() (receivers []*RTPReceiver) {
        self.mu.Lock()
        defer self.mu.Unlock()

        for _, transceiver := range self.rtp_transceivers {
            if transceiver.Receiver() != nil {
                receivers = append(receivers, transceiver.Receiver())
            }
        }
        return
    }*/

    /// get_transceivers returns the RtpTransceiver that are currently attached to this PeerConnection
    pub fn get_transceivers(&self) -> &[Arc<RTPTransceiver>] {
        &self.rtp_transceivers
    }
    /*
    // AddTrack adds a Track to the PeerConnection
    func (pc *PeerConnection) AddTrack(track TrackLocal) (*RTPSender, error) {
        if self.is_closed.get() {
            return nil, &rtcerr.InvalidStateError{Err: ErrConnectionClosed}
        }

        self.mu.Lock()
        defer self.mu.Unlock()
        for _, t := range self.rtp_transceivers {
            if !t.stopped && t.kind == track.kind() && t.Sender() == nil {
                sender, err := self.api.new_rtpsender(track, self.dtlsTransport)
                if err == nil {
                    err = t.SetSender(sender, track)
                    if err != nil {
                        _ = sender.Stop()
                        t.setSender(nil)
                    }
                }
                if err != nil {
                    return nil, err
                }
                self.onNegotiationNeeded()
                return sender, nil
            }
        }

        transceiver, err := self.newTransceiverFromTrack(RTPTransceiverDirectionSendrecv, track)
        if err != nil {
            return nil, err
        }
        self.add_rtptransceiver(transceiver)
        return transceiver.Sender(), nil
    }

    // RemoveTrack removes a Track from the PeerConnection
    func (pc *PeerConnection) RemoveTrack(sender *RTPSender) (err error) {
        if self.is_closed.get() {
            return &rtcerr.InvalidStateError{Err: ErrConnectionClosed}
        }

        var transceiver *RTPTransceiver
        self.mu.Lock()
        defer self.mu.Unlock()
        for _, t := range self.rtp_transceivers {
            if t.Sender() == sender {
                transceiver = t
                break
            }
        }
        if transceiver == nil {
            return &rtcerr.InvalidAccessError{Err: ErrSenderNotCreatedByConnection}
        } else if err = sender.Stop(); err == nil {
            err = transceiver.setSendingTrack(nil)
            if err == nil {
                self.onNegotiationNeeded()
            }
        }
        return
    }

    func (pc *PeerConnection) newTransceiverFromTrack(direction RTPTransceiverDirection, track TrackLocal) (t *RTPTransceiver, err error) {
        var (
            r *RTPReceiver
            s *RTPSender
        )
        switch direction {
        case RTPTransceiverDirectionSendrecv:
            r, err = self.api.new_rtpreceiver(track.kind(), self.dtlsTransport)
            if err != nil {
                return
            }
            s, err = self.api.new_rtpsender(track, self.dtlsTransport)
        case RTPTransceiverDirectionSendonly:
            s, err = self.api.new_rtpsender(track, self.dtlsTransport)
        default:
            err = errPeerConnAddTransceiverFromTrackSupport
        }
        if err != nil {
            return
        }
        return newRTPTransceiver(r, s, direction, track.kind()), nil
    }

    // AddTransceiverFromKind Create a new RtpTransceiver and adds it to the set of transceivers.
    func (pc *PeerConnection) AddTransceiverFromKind(kind RTPCodecType, init ...RTPTransceiverInit) (t *RTPTransceiver, err error) {
        if self.is_closed.get() {
            return nil, &rtcerr.InvalidStateError{Err: ErrConnectionClosed}
        }

        direction := RTPTransceiverDirectionSendrecv
        if len(init) > 1 {
            return nil, errPeerConnAddTransceiverFromKindOnlyAcceptsOne
        } else if len(init) == 1 {
            direction = init[0].Direction
        }
        switch direction {
        case RTPTransceiverDirectionSendonly, RTPTransceiverDirectionSendrecv:
            codecs := self.api.mediaEngine.getCodecsByKind(kind)
            if len(codecs) == 0 {
                return nil, ErrNoCodecsAvailable
            }
            track, err := NewTrackLocalStaticSample(codecs[0].RTPCodecCapability, util.MathRandAlpha(16), util.MathRandAlpha(16))
            if err != nil {
                return nil, err
            }
            t, err = self.newTransceiverFromTrack(direction, track)
            if err != nil {
                return nil, err
            }
        case RTPTransceiverDirectionRecvonly:
            receiver, err := self.api.new_rtpreceiver(kind, self.dtlsTransport)
            if err != nil {
                return nil, err
            }
            t = newRTPTransceiver(receiver, nil, RTPTransceiverDirectionRecvonly, kind)
        default:
            return nil, errPeerConnAddTransceiverFromKindSupport
        }
        self.mu.Lock()
        self.add_rtptransceiver(t)
        self.mu.Unlock()
        return t, nil
    }

    // AddTransceiverFromTrack Create a new RtpTransceiver(SendRecv or SendOnly) and add it to the set of transceivers.
    func (pc *PeerConnection) AddTransceiverFromTrack(track TrackLocal, init ...RTPTransceiverInit) (t *RTPTransceiver, err error) {
        if self.is_closed.get() {
            return nil, &rtcerr.InvalidStateError{Err: ErrConnectionClosed}
        }

        direction := RTPTransceiverDirectionSendrecv
        if len(init) > 1 {
            return nil, errPeerConnAddTransceiverFromTrackOnlyAcceptsOne
        } else if len(init) == 1 {
            direction = init[0].Direction
        }

        t, err = self.newTransceiverFromTrack(direction, track)
        if err == nil {
            self.mu.Lock()
            self.add_rtptransceiver(t)
            self.mu.Unlock()
        }
        return
    }

    // CreateDataChannel creates a new DataChannel object with the given label
    // and optional DataChannelInit used to configure properties of the
    // underlying channel such as data reliability.
    func (pc *PeerConnection) CreateDataChannel(label string, options *DataChannelInit) (*DataChannel, error) {
        // https://w3c.github.io/webrtc-pc/#peer-to-peer-data-api (Step #2)
        if self.is_closed.get() {
            return nil, &rtcerr.InvalidStateError{Err: ErrConnectionClosed}
        }

        params := &DataChannelParameters{
            Label:   label,
            Ordered: true,
        }

        // https://w3c.github.io/webrtc-pc/#peer-to-peer-data-api (Step #19)
        if options != nil {
            params.ID = options.ID
        }

        if options != nil {
            // Ordered indicates if data is allowed to be delivered out of order. The
            // default value of true, guarantees that data will be delivered in order.
            // https://w3c.github.io/webrtc-pc/#peer-to-peer-data-api (Step #9)
            if options.Ordered != nil {
                params.Ordered = *options.Ordered
            }

            // https://w3c.github.io/webrtc-pc/#peer-to-peer-data-api (Step #7)
            if options.MaxPacketLifeTime != nil {
                params.MaxPacketLifeTime = options.MaxPacketLifeTime
            }

            // https://w3c.github.io/webrtc-pc/#peer-to-peer-data-api (Step #8)
            if options.MaxRetransmits != nil {
                params.MaxRetransmits = options.MaxRetransmits
            }

            // https://w3c.github.io/webrtc-pc/#peer-to-peer-data-api (Step #10)
            if options.Protocol != nil {
                params.Protocol = *options.Protocol
            }

            // https://w3c.github.io/webrtc-pc/#peer-to-peer-data-api (Step #11)
            if len(params.Protocol) > 65535 {
                return nil, &rtcerr.TypeError{Err: ErrProtocolTooLarge}
            }

            // https://w3c.github.io/webrtc-pc/#peer-to-peer-data-api (Step #12)
            if options.Negotiated != nil {
                params.Negotiated = *options.Negotiated
            }
        }

        d, err := self.api.newDataChannel(params, self.log)
        if err != nil {
            return nil, err
        }

        // https://w3c.github.io/webrtc-pc/#peer-to-peer-data-api (Step #16)
        if d.maxPacketLifeTime != nil && d.maxRetransmits != nil {
            return nil, &rtcerr.TypeError{Err: ErrRetransmitsOrPacketLifeTime}
        }

        self.sctpTransport.lock.Lock()
        self.sctpTransport.dataChannels = append(self.sctpTransport.dataChannels, d)
        self.sctpTransport.dataChannelsRequested++
        self.sctpTransport.lock.Unlock()

        // If SCTP already connected open all the channels
        if self.sctpTransport.State() == SCTPTransportStateConnected {
            if err = d.open(self.sctpTransport); err != nil {
                return nil, err
            }
        }

        self.mu.Lock()
        self.onNegotiationNeeded()
        self.mu.Unlock()

        return d, nil
    }

    // SetIdentityProvider is used to configure an identity provider to generate identity assertions
    func (pc *PeerConnection) SetIdentityProvider(provider string) error {
        return errPeerConnSetIdentityProviderNotImplemented
    }

    // WriteRTCP sends a user provided RTCP packet to the connected peer. If no peer is connected the
    // packet is discarded. It also runs any configured interceptors.
    func (pc *PeerConnection) WriteRTCP(pkts []rtcp.Packet) error {
        _, err := self.interceptorRTCPWriter.Write(pkts, make(interceptor.Attributes))
        return err
    }

    func (pc *PeerConnection) writeRTCP(pkts []rtcp.Packet, _ interceptor.Attributes) (int, error) {
        return self.dtlsTransport.WriteRTCP(pkts)
    }

    // Close ends the PeerConnection
    func (pc *PeerConnection) Close() error {
        // https://www.w3.org/TR/webrtc/#dom-rtcpeerconnection-close (step #1)
        if self.is_closed.get() {
            return nil
        }

        // https://www.w3.org/TR/webrtc/#dom-rtcpeerconnection-close (step #2)
        self.is_closed.set(true)

        // https://www.w3.org/TR/webrtc/#dom-rtcpeerconnection-close (step #3)
        self.signaling_state.Set(SignalingStateClosed)

        // Try closing everything and collect the errors
        // Shutdown strategy:
        // 1. All Conn close by closing their underlying Conn.
        // 2. A Mux stops this chain. It won't close the underlying
        //    Conn if one of the endpoints is closed down. To
        //    continue the chain the Mux has to be closed.
        closeErrs := make([]error, 4)

        closeErrs = append(closeErrs, self.api.interceptor.Close())

        // https://www.w3.org/TR/webrtc/#dom-rtcpeerconnection-close (step #4)
        self.mu.Lock()
        for _, t := range self.rtp_transceivers {
            if !t.stopped {
                closeErrs = append(closeErrs, t.Stop())
            }
        }
        self.mu.Unlock()

        // https://www.w3.org/TR/webrtc/#dom-rtcpeerconnection-close (step #5)
        self.sctpTransport.lock.Lock()
        for _, d := range self.sctpTransport.dataChannels {
            d.setReadyState(DataChannelStateClosed)
        }
        self.sctpTransport.lock.Unlock()

        // https://www.w3.org/TR/webrtc/#dom-rtcpeerconnection-close (step #6)
        if self.sctpTransport != nil {
            closeErrs = append(closeErrs, self.sctpTransport.Stop())
        }

        // https://www.w3.org/TR/webrtc/#dom-rtcpeerconnection-close (step #7)
        closeErrs = append(closeErrs, self.dtlsTransport.Stop())

        // https://www.w3.org/TR/webrtc/#dom-rtcpeerconnection-close (step #8, #9, #10)
        if self.iceTransport != nil {
            closeErrs = append(closeErrs, self.iceTransport.Stop())
        }

        // https://www.w3.org/TR/webrtc/#dom-rtcpeerconnection-close (step #11)
        self.update_connection_state(self.ICEConnectionState(), self.dtlsTransport.State())

        return util.FlattenErrs(closeErrs)
    }*/

    /// add_rtp_transceiver appends t into rtp_transceivers
    /// and fires onNegotiationNeeded;
    /// caller of this method should hold `self.mu` lock
    async fn add_rtp_transceiver(&mut self, t: Arc<RTPTransceiver>) {
        self.rtp_transceivers.push(t);
        self.do_negotiation_needed().await;
    }

    /// CurrentLocalDescription represents the local description that was
    /// successfully negotiated the last time the PeerConnection transitioned
    /// into the stable state plus any local candidates that have been generated
    /// by the ICEAgent since the offer or answer was created.
    pub async fn current_local_description(&self) -> Option<SessionDescription> {
        let local_description = self.current_local_description.as_ref();
        let ice_gather = Some(&self.ice_gatherer);
        let ice_gathering_state = self.ice_gathering_state();

        populate_local_candidates(local_description, ice_gather, ice_gathering_state).await
    }

    /// PendingLocalDescription represents a local description that is in the
    /// process of being negotiated plus any local candidates that have been
    /// generated by the ICEAgent since the offer or answer was created. If the
    /// PeerConnection is in the stable state, the value is null.
    pub async fn pending_local_description(&self) -> Option<SessionDescription> {
        let local_description = self.pending_local_description.as_ref();
        let ice_gather = Some(&self.ice_gatherer);
        let ice_gathering_state = self.ice_gathering_state();

        populate_local_candidates(local_description, ice_gather, ice_gathering_state).await
    }

    /// current_remote_description represents the last remote description that was
    /// successfully negotiated the last time the PeerConnection transitioned
    /// into the stable state plus any remote candidates that have been supplied
    /// via AddICECandidate() since the offer or answer was created.
    pub fn current_remote_description(&self) -> Option<&SessionDescription> {
        self.current_remote_description.as_ref()
    }

    /// pending_remote_description represents a remote description that is in the
    /// process of being negotiated, complete with any remote candidates that
    /// have been supplied via AddICECandidate() since the offer or answer was
    /// created. If the PeerConnection is in the stable state, the value is
    /// null.
    pub fn pending_remote_description(&self) -> Option<&SessionDescription> {
        self.pending_remote_description.as_ref()
    }

    /// signaling_state attribute returns the signaling state of the
    /// PeerConnection instance.
    pub fn signaling_state(&self) -> SignalingState {
        self.signaling_state.load(Ordering::SeqCst).into()
    }

    /// icegathering_state attribute returns the ICE gathering state of the
    /// PeerConnection instance.
    pub fn ice_gathering_state(&self) -> ICEGatheringState {
        //if self.ice_gatherer == nil {
        //    return ICEGatheringStateNew
        //}

        match self.ice_gatherer.state() {
            ICEGathererState::New => ICEGatheringState::New,
            ICEGathererState::Gathering => ICEGatheringState::Gathering,
            _ => ICEGatheringState::Complete,
        }
    }

    /// connection_state attribute returns the connection state of the
    /// PeerConnection instance.
    pub fn connection_state(&self) -> PeerConnectionState {
        self.connection_state.load(Ordering::SeqCst).into()
    }
    /*
    // GetStats return data providing statistics about the overall connection
    func (pc *PeerConnection) GetStats() StatsReport {
        var (
            dataChannelsAccepted  uint32
            dataChannelsClosed    uint32
            dataChannelsOpened    uint32
            dataChannelsRequested uint32
        )
        statsCollector := newStatsReportCollector()
        statsCollector.Collecting()

        self.mu.Lock()
        if self.iceGatherer != nil {
            self.iceGatherer.collectStats(statsCollector)
        }
        if self.iceTransport != nil {
            self.iceTransport.collectStats(statsCollector)
        }

        self.sctpTransport.lock.Lock()
        dataChannels := append([]*DataChannel{}, self.sctpTransport.dataChannels...)
        dataChannelsAccepted = self.sctpTransport.dataChannelsAccepted
        dataChannelsOpened = self.sctpTransport.dataChannelsOpened
        dataChannelsRequested = self.sctpTransport.dataChannelsRequested
        self.sctpTransport.lock.Unlock()

        for _, d := range dataChannels {
            state := d.ReadyState()
            if state != DataChannelStateConnecting && state != DataChannelStateOpen {
                dataChannelsClosed++
            }

            d.collectStats(statsCollector)
        }
        self.sctpTransport.collectStats(statsCollector)

        stats := PeerConnectionStats{
            Timestamp:             statsTimestampNow(),
            Type:                  StatsTypePeerConnection,
            ID:                    self.stats_id,
            DataChannelsAccepted:  dataChannelsAccepted,
            DataChannelsClosed:    dataChannelsClosed,
            DataChannelsOpened:    dataChannelsOpened,
            DataChannelsRequested: dataChannelsRequested,
        }

        statsCollector.Collect(stats.ID, stats)

        certificates := self.configuration.Certificates
        for _, certificate := range certificates {
            if err := certificate.collectStats(statsCollector); err != nil {
                continue
            }
        }
        self.mu.Unlock()

        self.api.mediaEngine.collectStats(statsCollector)

        return statsCollector.Ready()
    }

    // Start all transports. PeerConnection now has enough state
    func (pc *PeerConnection) startTransports(iceRole ICERole, dtlsRole DTLSRole, remoteUfrag, remotePwd, fingerprint, fingerprintHash string) {
        // Start the ice transport
        err := self.iceTransport.Start(
            self.iceGatherer,
            ICEParameters{
                UsernameFragment: remoteUfrag,
                Password:         remotePwd,
                ICELite:          false,
            },
            &iceRole,
        )
        if err != nil {
            self.log.Warnf("Failed to start manager: %s", err)
            return
        }

        // Start the dtls_transport transport
        err = self.dtlsTransport.Start(DTLSParameters{
            Role:         dtlsRole,
            Fingerprints: []DTLSFingerprint{{Algorithm: fingerprintHash, Value: fingerprint}},
        })
        self.update_connection_state(self.ICEConnectionState(), self.dtlsTransport.State())
        if err != nil {
            self.log.Warnf("Failed to start manager: %s", err)
            return
        }
    }

    func (pc *PeerConnection) startRTP(isRenegotiation bool, remoteDesc *SessionDescription, currentTransceivers []*RTPTransceiver) {
        TrackDetails := track_details_from_sdp(self.log, remoteDesc.parsed)
        if isRenegotiation {
            for _, t := range currentTransceivers {
                if t.Receiver() == nil || t.Receiver().Track() == nil {
                    continue
                }

                t.Receiver().Track().mu.Lock()
                ssrc := t.Receiver().Track().ssrc

                if details := track_details_for_ssrc(TrackDetails, ssrc); details != nil {
                    t.Receiver().Track().id = details.id
                    t.Receiver().Track().streamID = details.streamID
                    t.Receiver().Track().mu.Unlock()
                    continue
                }

                t.Receiver().Track().mu.Unlock()

                if err := t.Receiver().Stop(); err != nil {
                    self.log.Warnf("Failed to stop RtpReceiver: %s", err)
                    continue
                }

                receiver, err := self.api.new_rtpreceiver(t.Receiver().kind, self.dtlsTransport)
                if err != nil {
                    self.log.Warnf("Failed to create new RtpReceiver: %s", err)
                    continue
                }
                t.setReceiver(receiver)
            }
        }

        self.startRTPReceivers(TrackDetails, currentTransceivers)
        if have_application_media_section(remoteDesc.parsed) {
            self.startSCTP()
        }

        if !isRenegotiation {
            self.undeclaredMediaProcessor()
        }
    }*/

    /// generate_unmatched_sdp generates an SDP that doesn't take remote state into account
    /// This is used for the initial call for CreateOffer
    fn generate_unmatched_sdp(
        &self,
        //_transceivers: &[RTPTransceiver],
        _use_identity: bool,
    ) -> Result<sdp::session_description::SessionDescription> {
        Ok(sdp::session_description::SessionDescription::default())
        /*TODO:
        let current_transceivers = &self.rtp_transceivers;
        d, err := sdp.NewJSEPSessionDescription(useIdentity)
        if err != nil {
            return nil, err
        }

        iceParams, err := self.iceGatherer.GetLocalParameters()
        if err != nil {
            return nil, err
        }

        candidates, err := self.iceGatherer.GetLocalCandidates()
        if err != nil {
            return nil, err
        }

        isPlanB := self.configuration.SDPSemantics == SDPSemanticsPlanB
        mediaSections := []mediaSection{}

        // Needed for self.sctpTransport.dataChannelsRequested
        self.sctpTransport.lock.Lock()
        defer self.sctpTransport.lock.Unlock()

        if isPlanB {
            video := make([]*RTPTransceiver, 0)
            audio := make([]*RTPTransceiver, 0)

            for _, t := range transceivers {
                if t.kind == RTPCodecTypeVideo {
                    video = append(video, t)
                } else if t.kind == RTPCodecTypeAudio {
                    audio = append(audio, t)
                }
                if t.Sender() != nil {
                    t.Sender().setNegotiated()
                }
            }

            if len(video) > 0 {
                mediaSections = append(mediaSections, mediaSection{id: "video", transceivers: video})
            }
            if len(audio) > 0 {
                mediaSections = append(mediaSections, mediaSection{id: "audio", transceivers: audio})
            }

            if self.sctpTransport.dataChannelsRequested != 0 {
                mediaSections = append(mediaSections, mediaSection{id: "data", data: true})
            }
        } else {
            for _, t := range transceivers {
                if t.Sender() != nil {
                    t.Sender().setNegotiated()
                }
                mediaSections = append(mediaSections, mediaSection{id: t.Mid(), transceivers: []*RTPTransceiver{t}})
            }

            if self.sctpTransport.dataChannelsRequested != 0 {
                mediaSections = append(mediaSections, mediaSection{id: strconv.Itoa(len(mediaSections)), data: true})
            }
        }

        dtlsFingerprints, err := self.configuration.Certificates[0].GetFingerprints()
        if err != nil {
            return nil, err
        }

        return populate_sdp(d, isPlanB, dtlsFingerprints, self.api.settingEngine.sdpMediaLevelFingerprints, self.api.settingEngine.candidates.ICELite, self.api.mediaEngine, connectionRoleFromDtlsRole(defaultDtlsRoleOffer), candidates, iceParams, mediaSections, self.icegathering_state())
        */
    }

    /// generate_matched_sdp generates a SDP and takes the remote state into account
    /// this is used everytime we have a remote_description
    fn generate_matched_sdp(
        &self,
        //_transceivers: &[RTPTransceiver],
        _use_identity: bool,
        _include_unmatched: bool,
        _connection_role: ConnectionRole,
    ) -> Result<sdp::session_description::SessionDescription> {
        Ok(sdp::session_description::SessionDescription::default())

        /*TODO:
        let current_transceivers = &self.rtp_transceivers;
           d, err := sdp.NewJSEPSessionDescription(useIdentity)
        if err != nil {
            return nil, err
        }

        iceParams, err := self.iceGatherer.GetLocalParameters()
        if err != nil {
            return nil, err
        }

        candidates, err := self.iceGatherer.GetLocalCandidates()
        if err != nil {
            return nil, err
        }

        var t *RTPTransceiver
        remoteDescription := self.current_remote_description
        if self.pending_remote_description != nil {
            remoteDescription = self.pending_remote_description
        }
        localTransceivers := append([]*RTPTransceiver{}, transceivers...)
        detectedPlanB := description_is_plan_b(remoteDescription)
        mediaSections := []mediaSection{}
        alreadyHaveApplicationMediaSection := false
        for _, media := range remoteDescription.parsed.MediaDescriptions {
            midValue := getMidValue(media)
            if midValue == "" {
                return nil, errPeerConnRemoteDescriptionWithoutMidValue
            }

            if media.MediaName.Media == mediaSectionApplication {
                mediaSections = append(mediaSections, mediaSection{id: midValue, data: true})
                alreadyHaveApplicationMediaSection = true
                continue
            }

            kind := NewRTPCodecType(media.MediaName.Media)
            direction := get_peer_direction(media)
            if kind == 0 || direction == RTPTransceiverDirection(Unknown) {
                continue
            }

            sdpSemantics := self.configuration.SDPSemantics

            switch {
            case sdpSemantics == SDPSemanticsPlanB || sdpSemantics == SDPSemanticsUnifiedPlanWithFallback && detectedPlanB:
                if !detectedPlanB {
                    return nil, &rtcerr.TypeError{Err: ErrIncorrectSDPSemantics}
                }
                // If we're responding to a plan-b offer, then we should try to fill up this
                // media entry with all matching local transceivers
                mediaTransceivers := []*RTPTransceiver{}
                for {
                    // keep going until we can't get any more
                    t, localTransceivers = satisfy_type_and_direction(kind, direction, localTransceivers)
                    if t == nil {
                        if len(mediaTransceivers) == 0 {
                            t = &RTPTransceiver{kind: kind}
                            t.setDirection(RTPTransceiverDirectionInactive)
                            mediaTransceivers = append(mediaTransceivers, t)
                        }
                        break
                    }
                    if t.Sender() != nil {
                        t.Sender().setNegotiated()
                    }
                    mediaTransceivers = append(mediaTransceivers, t)
                }
                mediaSections = append(mediaSections, mediaSection{id: midValue, transceivers: mediaTransceivers})
            case sdpSemantics == SDPSemanticsUnifiedPlan || sdpSemantics == SDPSemanticsUnifiedPlanWithFallback:
                if detectedPlanB {
                    return nil, &rtcerr.TypeError{Err: ErrIncorrectSDPSemantics}
                }
                t, localTransceivers = findByMid(midValue, localTransceivers)
                if t == nil {
                    return nil, fmt.Errorf("%w: %q", errPeerConnTranscieverMidNil, midValue)
                }
                if t.Sender() != nil {
                    t.Sender().setNegotiated()
                }
                mediaTransceivers := []*RTPTransceiver{t}
                mediaSections = append(mediaSections, mediaSection{id: midValue, transceivers: mediaTransceivers, ridMap: getRids(media)})
            }
        }

        // If we are offering also include unmatched local transceivers
        if includeUnmatched {
            if !detectedPlanB {
                for _, t := range localTransceivers {
                    if t.Sender() != nil {
                        t.Sender().setNegotiated()
                    }
                    mediaSections = append(mediaSections, mediaSection{id: t.Mid(), transceivers: []*RTPTransceiver{t}})
                }
            }

            if self.sctpTransport.dataChannelsRequested != 0 && !alreadyHaveApplicationMediaSection {
                if detectedPlanB {
                    mediaSections = append(mediaSections, mediaSection{id: "data", data: true})
                } else {
                    mediaSections = append(mediaSections, mediaSection{id: strconv.Itoa(len(mediaSections)), data: true})
                }
            }
        }

        if self.configuration.SDPSemantics == SDPSemanticsUnifiedPlanWithFallback && detectedPlanB {
            self.log.Info("Plan-B Offer detected; responding with Plan-B Answer")
        }

        dtlsFingerprints, err := self.configuration.Certificates[0].GetFingerprints()
        if err != nil {
            return nil, err
        }

        return populate_sdp(d, detectedPlanB, dtlsFingerprints, self.api.settingEngine.sdpMediaLevelFingerprints, self.api.settingEngine.candidates.ICELite, self.api.mediaEngine, connectionRole, candidates, iceParams, mediaSections, self.icegathering_state())
        */
    }

    async fn set_gather_complete_handler(&self, f: OnGatheringCompleteHdlrFn) {
        self.ice_gatherer.on_gathering_complete(f).await;
    }

    /// sctp returns the SCTPTransport for this PeerConnection
    ///
    /// The SCTP transport over which SCTP data is sent and received. If SCTP has not been negotiated, the value is nil.
    /// https://www.w3.org/TR/webrtc/#attributes-15
    pub fn sctp(&self) -> Arc<SCTPTransport> {
        Arc::clone(&self.sctp_transport)
    }
}
