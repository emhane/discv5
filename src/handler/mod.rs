//! Session and packet handling for the Discv5 Discovery service.
//!
//! The [`Handler`] is responsible for establishing and maintaining sessions with
//! connected/discovered nodes. Each node, identified by it's [`NodeId`] is associated with a
//! `Session`. This service drives the handshakes for establishing the sessions and associated
//! logic for sending/requesting initial connections/ENR's to/from unknown peers.
//!
//! The [`Handler`] also manages the timeouts for each request and reports back RPC failures,
//! and received messages. Messages are encrypted and decrypted using the
//! associated `Session` for each node.
//!
//! An ongoing established connection is abstractly represented by a `Session`. A node that provides an ENR with an
//! IP address/port that doesn't match the source, is considered invalid. A node that doesn't know
//! their external contactable addresses should set their ENR IP field to `None`.
//!
//! The Handler also routinely checks the timeouts for banned nodes and removes them from the
//! banned list once their ban expires.
//!
//! # Usage
//!
//! Interacting with a handler is done via channels. A Handler is spawned using the [`Handler::spawn`]
//! function. This returns an exit channel, a sending and receiving channel respectively. If the
//! exit channel is dropped or fired, the handler task gets shutdown.
//!
//! Requests from the application layer can be made via the receive channel using a [`HandlerIn`].
//! Responses from the application layer can be made via the receive channel using a [`HandlerIn`].
//! Messages from a node on the network come by [`Socket`] and get the form of a [`HandlerOut`]
//! and can be forwarded to the application layer via the send channel.
use crate::{
    config::Discv5Config,
    discv5::PERMIT_BAN_LIST,
    error::{Discv5Error, RequestError},
    packet::{ChallengeData, IdNonce, MessageNonce, Packet, PacketKind, ProtocolIdentity},
    rpc::{
        Message, Notification, Payload, Request, RequestBody, RequestId, Response, ResponseBody,
    },
    socket,
    socket::{FilterConfig, Outbound, Socket},
    Enr,
};
use delay_map::HashMapDelay;
use enr::{CombinedKey, NodeId};
use futures::prelude::*;
use parking_lot::RwLock;
use smallvec::SmallVec;
use std::{
    collections::HashMap,
    convert::TryFrom,
    default::Default,
    net::SocketAddr,
    pin::Pin,
    sync::{atomic::Ordering, Arc},
    task::{Context, Poll},
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, trace, warn};

mod active_requests;
mod nat_hole_punch;
mod request_call;
mod sessions;
mod tests;

use crate::metrics::METRICS;
pub use crate::node_info::{NodeAddress, NodeContact};
pub use sessions::MIN_SESSIONS_UNREACHABLE_ENR;

use crate::{lru_time_cache::LruTimeCache, socket::ListenConfig};
use active_requests::ActiveRequests;
use nat_hole_punch::{Error as NatHolePunchError, HolePunchNat, NatHolePunchUtils};
use request_call::RequestCall;
use sessions::{Session, Sessions};

// The time interval to check banned peer timeouts and unban peers when the timeout has elapsed (in
// seconds).
const BANNED_NODES_CHECK: u64 = 300; // Check every 5 minutes.

// The one-time session timeout.
const ONE_TIME_SESSION_TIMEOUT: u64 = 30;

// The maximum number of established one-time sessions to maintain.
const ONE_TIME_SESSION_CACHE_CAPACITY: usize = 100;

/// Messages sent from the application layer to `Handler`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum HandlerIn {
    /// A Request to send to a `NodeContact` has been received from the application layer. A
    /// `NodeContact` is an abstract type that allows for either an ENR to be sent or a `Raw` type
    /// which represents an `SocketAddr`, `PublicKey` and `NodeId`. This type can be created from
    /// MultiAddrs and MultiAddr strings for some keys.
    ///
    /// This permits us to send messages to nodes without knowing their ENR. In this case their ENR
    /// will be requested during the handshake.
    ///
    /// A Request is flagged and permits responses through the packet filter.
    ///
    /// Note: To update an ENR for an unknown node, we request a FINDNODE with distance 0 to the
    /// `NodeContact` we know of.
    Request(NodeContact, Box<Request>),

    /// A Response to send to a particular node to answer a HandlerOut::Request has been
    /// received from the application layer.
    ///
    /// The handler does not keep state of requests, so the application layer must send the
    /// response back to the `NodeAddress` from which the request was received.
    Response(NodeAddress, Box<Response>),

    /// A Random packet has been received and we have requested the application layer to inform
    /// us what the highest known ENR is for this node.
    /// The `WhoAreYouRef` is sent out in the `HandlerOut::WhoAreYou` event and should
    /// be returned here to submit the application's response.
    WhoAreYou(WhoAreYouRef, Option<Enr>),

    /// A response to a [`HandlerOut::FindHolePunchEnr`]. Returns the ENR and the
    /// [`Notification::RelayMsg`] we intend to relay to that peer.
    HolePunchEnr(Enr, Notification),

    /// Observed socket has been update. The old socket and the current socket.
    SocketUpdate(Option<SocketAddr>, SocketAddr),
}

/// Messages sent between a node on the network and `Handler`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandlerOut {
    /// A session has been established with a node.
    ///
    /// A session is only considered established once we have received a signed ENR from the
    /// node and either the observed `SocketAddr` matches the one declared in the ENR or the
    /// ENR declares no `SocketAddr`.
    Established(Enr, SocketAddr, ConnectionDirection),

    /// A Request has been received from a node on the network.
    Request(NodeAddress, Box<Request>),

    /// A Response has been received from a node on the network.
    Response(NodeAddress, Box<Response>),

    /// An unknown source has requested information from us. Return the reference with the known
    /// ENR of this node (if known). See the `HandlerIn::WhoAreYou` variant.
    WhoAreYou(WhoAreYouRef),

    /// An RPC request failed.
    ///
    /// This returns the request ID and an error indicating why the request failed.
    RequestFailed(RequestId, RequestError),

    /// Look-up an ENR in k-buckets. Passes the node id of the peer to look up and the
    /// [`Notification::RelayMsg`] we intend to send to it.
    FindHolePunchEnr(NodeId, Notification),
}

/// How we connected to the node.
#[derive(PartialEq, Eq, Debug, Copy, Clone)]
pub enum ConnectionDirection {
    /// The node contacted us.
    Incoming,
    /// We contacted the node.
    Outgoing,
}

/// A reference for the application layer to send back when the handler requests any known
/// ENR for the NodeContact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhoAreYouRef(pub NodeAddress, MessageNonce);

#[derive(Debug)]
/// A Challenge (WHOAREYOU) object used to handle and send WHOAREYOU requests.
pub struct Challenge {
    /// The challenge data received from the node.
    data: ChallengeData,
    /// The remote's ENR if we know it. We can receive a challenge from an unknown node.
    remote_enr: Option<Enr>,
}

/// Request ID from the handler's perspective.
#[derive(Debug, Clone)]
enum HandlerReqId {
    /// Requests made by the handler.
    Internal(RequestId),
    /// Requests made from outside the handler.
    External(RequestId),
}

/// A request queued for sending.
struct PendingRequest {
    contact: NodeContact,
    request_id: HandlerReqId,
    request: RequestBody,
}

/// Process to handle handshakes and sessions established from raw RPC communications between nodes.
pub struct Handler {
    /// Configuration for the discv5 service.
    request_retries: u8,
    /// The local node id to save unnecessary read locks on the ENR. The NodeID should not change
    /// during the operation of the server.
    node_id: NodeId,
    /// The local ENR.
    enr: Arc<RwLock<Enr>>,
    /// The key to sign the ENR and set up encrypted communication with peers.
    key: Arc<RwLock<CombinedKey>>,
    /// Pending raw requests.
    active_requests: ActiveRequests,
    /// The expected responses by SocketAddr which allows packets to pass the underlying filter.
    filter_expected_responses: Arc<RwLock<HashMap<SocketAddr, usize>>>,
    /// Requests awaiting a handshake completion.
    pending_requests: HashMap<NodeAddress, Vec<PendingRequest>>,
    /// Currently in-progress outbound handshakes (WHOAREYOU packets) with peers.
    active_challenges: HashMapDelay<NodeAddress, Challenge>,
    /// Established sessions with peers.
    sessions: Sessions,
    /// Established sessions with peers for a specific request, stored just one per node.
    one_time_sessions: LruTimeCache<NodeAddress, (RequestId, Session)>,
    /// The channel to receive messages from the application layer.
    service_recv: mpsc::UnboundedReceiver<HandlerIn>,
    /// The channel to send messages to the application layer.
    service_send: mpsc::Sender<HandlerOut>,
    /// The listening sockets to filter out any attempted requests to self.
    listen_sockets: SmallVec<[SocketAddr; 2]>,
    /// The discovery v5 UDP socket tasks.
    socket: Socket,
    /// Exit channel to shutdown the handler.
    exit: oneshot::Receiver<()>,
    /// Types necessary to plug in nat hole punching.
    nat_hole_puncher: NatHolePunchUtils,
}

type HandlerReturn = (
    oneshot::Sender<()>,
    mpsc::UnboundedSender<HandlerIn>,
    mpsc::Receiver<HandlerOut>,
);

impl Handler {
    /// A new Session service which instantiates the UDP socket send/recv tasks.
    pub async fn spawn<P: ProtocolIdentity>(
        enr: Arc<RwLock<Enr>>,
        key: Arc<RwLock<CombinedKey>>,
        config: Discv5Config,
    ) -> Result<HandlerReturn, std::io::Error> {
        let (exit_sender, exit) = oneshot::channel();
        // create the channels to send/receive messages from the application
        let (handler_send, service_recv) = mpsc::unbounded_channel();
        let (service_send, handler_recv) = mpsc::channel(50);

        // Creates a SocketConfig to pass to the underlying UDP socket tasks.

        // Lets the underlying filter know that we are expecting a packet from this source.
        let filter_expected_responses = Arc::new(RwLock::new(HashMap::new()));

        // The local node id
        let node_id = enr.read().node_id();

        // enable the packet filter if required
        let filter_config = FilterConfig {
            enabled: config.enable_packet_filter,
            rate_limiter: config.filter_rate_limiter.clone(),
            max_nodes_per_ip: config.filter_max_nodes_per_ip,
            max_bans_per_ip: config.filter_max_bans_per_ip,
        };

        let mut listen_sockets = SmallVec::default();
        match config.listen_config {
            ListenConfig::Ipv4 { ip, port } => listen_sockets.push((ip, port).into()),
            ListenConfig::Ipv6 { ip, port } => listen_sockets.push((ip, port).into()),
            ListenConfig::DualStack {
                ipv4,
                ipv4_port,
                ipv6,
                ipv6_port,
            } => {
                listen_sockets.push((ipv4, ipv4_port).into());
                listen_sockets.push((ipv6, ipv6_port).into());
            }
        };

        let ip_mode = config.listen_config.ip_mode();

        let socket_config = socket::SocketConfig {
            executor: config.executor.clone().expect("Executor must exist"),
            filter_config,
            listen_config: config.listen_config.clone(),
            local_node_id: node_id,
            expected_responses: filter_expected_responses.clone(),
            ban_duration: config.ban_duration,
        };

        // Attempt to bind to the socket before spinning up the send/recv tasks.
        let socket = Socket::new::<P>(socket_config).await?;

        let sessions = Sessions::new(
            config.session_cache_capacity,
            config.session_timeout,
            config.unreachable_enr_limit,
        );

        let nat_hole_puncher = NatHolePunchUtils::new(
            listen_sockets.iter(),
            &enr.read(),
            ip_mode,
            config.unused_port_range.clone(),
            config.ban_duration,
            config.session_cache_capacity,
        );

        config
            .executor
            .clone()
            .expect("Executor must be present")
            .spawn(Box::pin(async move {
                let mut handler = Handler {
                    request_retries: config.request_retries,
                    node_id,
                    enr,
                    key,
                    active_requests: ActiveRequests::new(config.request_timeout),
                    pending_requests: HashMap::new(),
                    filter_expected_responses,
                    sessions,
                    one_time_sessions: LruTimeCache::new(
                        Duration::from_secs(ONE_TIME_SESSION_TIMEOUT),
                        Some(ONE_TIME_SESSION_CACHE_CAPACITY),
                    ),
                    active_challenges: HashMapDelay::new(config.request_timeout),
                    service_recv,
                    service_send,
                    listen_sockets,
                    socket,
                    nat_hole_puncher,
                    exit,
                };
                debug!("Handler Starting");
                handler.start::<P>().await;
            }));

        Ok((exit_sender, handler_send, handler_recv))
    }

    /// The main execution loop for the handler.
    async fn start<P: ProtocolIdentity>(&mut self) {
        let mut banned_nodes_check = tokio::time::interval(Duration::from_secs(BANNED_NODES_CHECK));

        loop {
            tokio::select! {
                Some(handler_request) = self.service_recv.recv() => {
                    match handler_request {
                        HandlerIn::Request(contact, request) => {
                            let Request { id, body: request } = *request;
                            if let Err(request_error) =  self.send_request::<P>(contact, HandlerReqId::External(id.clone()), request).await {
                                // If the sending failed report to the application
                                if let Err(e) = self.service_send.send(HandlerOut::RequestFailed(id, request_error)).await {
                                    warn!("Failed to inform that request failed {}", e)
                                }
                            }
                        }
                        HandlerIn::Response(dst, response) => self.send_response::<P>(dst, *response).await,
                        HandlerIn::WhoAreYou(wru_ref, enr) => self.send_challenge::<P>(wru_ref, enr).await,
                        HandlerIn::HolePunchEnr(tgt_enr, relay_msg_notif) => {
                            if let Err(e) = self.send_relay_msg_notif::<P>(tgt_enr, relay_msg_notif).await {
                                warn!("Failed to relay. Error: {}", e);
                            }
                        }
                        HandlerIn::SocketUpdate(old_socket, socket) => {
                            let ip = socket.ip();
                            let port = socket.port();
                            if old_socket.is_none() {
                                // This node goes from being unreachable to being reachable. Remove
                                // its sessions to trigger a WHOAREYOU from peers on next sent
                                // message. If the peer is running this implementation of
                                // discovery, this makes it possible for the local node to be
                                // inserted into its peers' kbuckets before the session they
                                // already had expires. Session duration, in this impl defaults to
                                // 24 hours.
                                self.sessions.cache.clear()
                            }
                            self.nat_hole_puncher.set_is_behind_nat(self.listen_sockets.iter(), Some(ip), Some(port));
                        }
                    }
                }
                Some(inbound_packet) = self.socket.recv.recv() => {
                    self.process_inbound_packet::<P>(inbound_packet).await;
                }
                Some(Ok((node_address, pending_request))) = self.active_requests.next() => {
                    self.handle_request_timeout::<P>(node_address, pending_request).await;
                }
                Some(Ok((node_address, _challenge))) = self.active_challenges.next() => {
                    // A challenge has expired. There could be pending requests awaiting this
                    // challenge. We process them here
                    self.send_next_request::<P>(node_address).await;
                }
                Some(peer_socket) = self.nat_hole_puncher.next() => {
                    if let Err(e) = self.on_hole_punch_expired(peer_socket).await {
                        warn!("Failed to keep hole punched for peer, error: {}", e);
                    }
                }
                _ = banned_nodes_check.tick() => self.unban_nodes_check(), // Unban nodes that are past the timeout
                _ = &mut self.exit => {
                    return;
                }
            }
        }
    }

    /// Processes an inbound decoded packet.
    async fn process_inbound_packet<P: ProtocolIdentity>(
        &mut self,
        inbound_packet: socket::InboundPacket,
    ) {
        let message_nonce = inbound_packet.header.message_nonce;
        match inbound_packet.header.kind {
            PacketKind::WhoAreYou { enr_seq, .. } => {
                let challenge_data =
                    ChallengeData::try_from(inbound_packet.authenticated_data.as_slice())
                        .expect("Must be correct size");
                self.handle_challenge::<P>(
                    inbound_packet.src_address,
                    message_nonce,
                    enr_seq,
                    challenge_data,
                )
                .await
            }
            PacketKind::Handshake {
                src_id,
                id_nonce_sig,
                ephem_pubkey,
                enr_record,
            } => {
                let node_address = NodeAddress {
                    socket_addr: inbound_packet.src_address,
                    node_id: src_id,
                };
                self.handle_auth_message::<P>(
                    node_address,
                    message_nonce,
                    &id_nonce_sig,
                    &ephem_pubkey,
                    enr_record,
                    &inbound_packet.message,
                    &inbound_packet.authenticated_data, // This is required for authenticated data in decryption.
                )
                .await
            }
            PacketKind::Message { src_id } => {
                let node_address = NodeAddress {
                    socket_addr: inbound_packet.src_address,
                    node_id: src_id,
                };
                self.handle_message::<P>(
                    node_address,
                    message_nonce,
                    &inbound_packet.message,
                    &inbound_packet.authenticated_data,
                )
                .await
            }
            PacketKind::SessionMessage { src_id } => {
                let node_address = NodeAddress {
                    socket_addr: inbound_packet.src_address,
                    node_id: src_id,
                };
                self.handle_session_message::<P>(
                    node_address,
                    message_nonce,
                    &inbound_packet.message,
                    &inbound_packet.authenticated_data,
                )
                .await
            }
        }
    }

    fn remove_expected_response(&mut self, socket_addr: SocketAddr) {
        if let std::collections::hash_map::Entry::Occupied(mut entry) =
            self.filter_expected_responses.write().entry(socket_addr)
        {
            let count = entry.get_mut();
            *count = count.saturating_sub(1);
            if count == &0 {
                entry.remove();
            }
        }
    }

    fn add_expected_response(&mut self, socket_addr: SocketAddr) {
        *self
            .filter_expected_responses
            .write()
            .entry(socket_addr)
            .or_default() += 1;
    }

    /// A request has timed out.
    async fn handle_request_timeout<P: ProtocolIdentity>(
        &mut self,
        node_address: NodeAddress,
        mut request_call: RequestCall,
    ) {
        if request_call.retries() >= self.request_retries {
            trace!("Request timed out with {}", node_address);
            if let Some(relay) = self
                .nat_hole_puncher
                .new_peer_latest_relay_cache
                .pop(&node_address.node_id)
            {
                // The request might be timing out because the peer is behind a NAT. If we
                // have a relay to the peer, attempt NAT hole punching.
                let target = request_call.contact().node_address();
                trace!("Trying to hole punch target {target} with relay {relay}");
                let local_enr = self.enr.read().clone();
                let nonce = request_call.packet().header.message_nonce;
                match self
                    .on_request_time_out::<P>(relay, local_enr, nonce, target)
                    .await
                {
                    Err(e) => {
                        warn!("Failed to start hole punching. Error: {:?}", e);
                    }
                    Ok(()) => {
                        self.active_requests.insert(node_address, request_call);
                        return;
                    }
                }
            }
            // Remove the request from the awaiting packet_filter
            self.remove_expected_response(node_address.socket_addr);
            // The request has timed out. We keep any established session for future use.
            self.fail_request(request_call, RequestError::Timeout, false)
                .await;
        } else {
            // increment the request retry count and restart the timeout
            trace!(
                "Resending message: {} to {}",
                request_call.body(),
                node_address
            );
            self.send(node_address.clone(), request_call.packet().clone())
                .await;
            request_call.increment_retries();
            self.active_requests.insert(node_address, request_call);
        }
    }

    /// Sends a `Request` to a node.
    async fn send_request<P: ProtocolIdentity>(
        &mut self,
        contact: NodeContact,
        request_id: HandlerReqId,
        request: RequestBody,
    ) -> Result<(), RequestError> {
        let node_address = contact.node_address();

        if self.listen_sockets.contains(&node_address.socket_addr) {
            debug!("Filtered request to self");
            return Err(RequestError::SelfRequest);
        }

        // If there is already an active request or an active challenge (WHOAREYOU sent) for this
        // node, add to pending requests
        if self.active_requests.get(&node_address).is_some()
            || self.active_challenges.get(&node_address).is_some()
        {
            trace!("Request queued for node: {}", node_address);
            self.pending_requests
                .entry(node_address)
                .or_default()
                .push(PendingRequest {
                    contact,
                    request_id,
                    request,
                });
            return Ok(());
        }

        let (packet, initiating_session) = {
            if let Some(session) = self.sessions.cache.get_mut(&node_address) {
                // Encrypt the message and send
                let request = match &request_id {
                    HandlerReqId::Internal(id) | HandlerReqId::External(id) => Request {
                        id: id.clone(),
                        body: request.clone(),
                    },
                };
                let packet = session
                    .encrypt_message::<P>(self.node_id, &request.encode())
                    .map_err(|e| RequestError::EncryptionFailed(format!("{e:?}")))?;
                (packet, false)
            } else {
                // No session exists, start a new handshake
                trace!(
                    "Starting session. Sending random packet to: {}",
                    node_address
                );
                let packet =
                    Packet::new_random(&self.node_id).map_err(RequestError::EntropyFailure)?;
                // We are initiating a new session
                (packet, true)
            }
        };

        let call = RequestCall::new(
            contact,
            packet.clone(),
            request_id,
            request,
            initiating_session,
        );
        // let the filter know we are expecting a response
        self.add_expected_response(node_address.socket_addr);
        self.send(node_address.clone(), packet).await;

        self.active_requests.insert(node_address, call);
        Ok(())
    }

    /// Sends an RPC Response.
    async fn send_response<P: ProtocolIdentity>(
        &mut self,
        node_address: NodeAddress,
        response: Response,
    ) {
        // Check for an established session
        let packet = if let Some(session) = self.sessions.cache.get_mut(&node_address) {
            session.encrypt_session_message::<P>(self.node_id, &response.encode())
        } else if let Some(mut session) = self.remove_one_time_session(&node_address, &response.id)
        {
            session.encrypt_session_message::<P>(self.node_id, &response.encode())
        } else {
            // Either the session is being established or has expired. We simply drop the
            // response in this case.
            return warn!(
                "Session is not established. Dropping response {} for node: {}",
                response, node_address.node_id
            );
        };

        match packet {
            Ok(packet) => self.send(node_address, packet).await,
            Err(e) => warn!("Could not encrypt response: {:?}", e),
        }
    }

    /// This is called in response to a `HandlerOut::WhoAreYou` event. The applications finds the
    /// highest known ENR for a node then we respond to the node with a WHOAREYOU packet.
    async fn send_challenge<P: ProtocolIdentity>(
        &mut self,
        wru_ref: WhoAreYouRef,
        remote_enr: Option<Enr>,
    ) {
        let node_address = wru_ref.0;
        let message_nonce = wru_ref.1;

        if self.active_challenges.get(&node_address).is_some() {
            warn!("WHOAREYOU already sent. {}", node_address);
            return;
        }

        // NOTE: We do not check if we have an active session here. This was checked before
        // requesting the ENR from the service. It could be the case we have established a session
        // in the meantime, we allow this challenge to establish a second session in the event this
        // race occurs. The nodes will decide amongst themselves which session keys to use (the
        // most recent).

        // It could be the case we have sent an ENR with an active request, however we consider
        // these independent as this is in response to an unknown packet. If the ENR it not in our
        // table (remote_enr is None) then we re-request the ENR to keep the session up to date.

        // send the challenge
        let enr_seq = remote_enr.clone().map_or_else(|| 0, |enr| enr.seq());
        let id_nonce: IdNonce = rand::random();
        let packet = Packet::new_whoareyou(message_nonce, id_nonce, enr_seq);
        let challenge_data = ChallengeData::try_from(packet.authenticated_data::<P>().as_slice())
            .expect("Must be the correct challenge size");
        debug!("Sending WHOAREYOU to {}", node_address);
        self.add_expected_response(node_address.socket_addr);
        self.send(node_address.clone(), packet).await;
        self.active_challenges.insert(
            node_address,
            Challenge {
                data: challenge_data,
                remote_enr,
            },
        );
    }

    /* Packet Handling */

    /// Handles a WHOAREYOU packet that was received from the network.
    async fn handle_challenge<P: ProtocolIdentity>(
        &mut self,
        src_address: SocketAddr,
        request_nonce: MessageNonce,
        enr_seq: u64,
        challenge_data: ChallengeData,
    ) {
        // Check that this challenge matches a known active request.
        // If this message passes all the requisite checks, a request call is returned.
        let mut request_call = match self.active_requests.remove_by_nonce(&request_nonce) {
            Some((node_address, request_call)) => {
                // Verify that the src_addresses match
                if node_address.socket_addr != src_address {
                    debug!("Received a WHOAREYOU packet for a message with a non-expected source. Source {}, expected_source: {} message_nonce {}", src_address, node_address.socket_addr, hex::encode(request_nonce));
                    // Add the request back if src_address doesn't match
                    self.active_requests.insert(node_address, request_call);
                    return;
                }
                request_call
            }
            None => {
                trace!("Received a WHOAREYOU packet that references an unknown or expired request. Source {}, message_nonce {}", src_address, hex::encode(request_nonce));
                return;
            }
        };

        // double check the message nonces match
        if request_call.packet().message_nonce() != &request_nonce {
            // This could theoretically happen if a peer uses the same node id across
            // different connections.
            warn!("Received a WHOAREYOU from a non expected source. Source: {}, message_nonce {} , expected_nonce: {}", request_call.contact(), hex::encode(request_call.packet().message_nonce()), hex::encode(request_nonce));
            // NOTE: Both mappings are removed in this case.
            return;
        }

        trace!(
            "Received a WHOAREYOU packet response. Source: {}",
            request_call.contact()
        );

        // We do not allow multiple WHOAREYOU packets for a single challenge request. If we have
        // already sent a WHOAREYOU ourselves, we drop sessions who send us a WHOAREYOU in
        // response.
        if request_call.handshake_sent() {
            warn!(
                "Authentication response already sent. Dropping session. Node: {}",
                request_call.contact()
            );
            self.fail_request(request_call, RequestError::InvalidRemotePacket, true)
                .await;
            return;
        }

        // Encrypt the message with an auth header and respond

        // First if a new version of our ENR is requested, obtain it for the header
        let updated_enr = if enr_seq < self.enr.read().seq() {
            Some(self.enr.read().clone())
        } else {
            None
        };

        // Generate a new session and authentication packet
        let (auth_packet, mut session) = match Session::encrypt_with_header::<P>(
            request_call.contact(),
            self.key.clone(),
            updated_enr,
            &self.node_id,
            &challenge_data,
            &request_call.encode(),
        ) {
            Ok(v) => v,
            Err(e) => {
                error!("Could not generate a session. Error: {:?}", e);
                self.fail_request(request_call, RequestError::InvalidRemotePacket, true)
                    .await;
                return;
            }
        };

        // There are two quirks with an established session at this point.
        // 1. We may not know the ENR. In this case we need to set up a request to find the ENR and
        //    wait for a response before we officially call this node established.
        // 2. The challenge here could be to an already established session. If so, we need to
        //    update the existing session to attempt to decrypt future messages with the new keys
        //    and update the keys internally upon successful decryption.
        //
        // We handle both of these cases here.

        // Check if we know the ENR, if not request it and flag the session as awaiting an ENR.
        //
        // All sent requests must have an associated node_id. Therefore the following
        // must not panic.
        let node_address = request_call.contact().node_address();
        match request_call.contact().enr() {
            Some(enr) => {
                // NOTE: Here we decide if the session is outgoing or ingoing. The condition for an
                // outgoing session is that we originally sent a RANDOM packet (signifying we did
                // not have a session for a request) and the packet is not a PING (we are not
                // trying to update an old session that may have expired.
                let connection_direction = if request_call.initiating_session() {
                    ConnectionDirection::Outgoing
                } else {
                    ConnectionDirection::Incoming
                };

                // We already know the ENR. Send the handshake response packet
                trace!("Sending Authentication response to node: {}", node_address);
                request_call.update_packet(auth_packet.clone());
                request_call.set_handshake_sent();
                request_call.set_initiating_session(false);
                // Reinsert the request_call
                self.insert_active_request(request_call);
                // Send the actual packet to the send task.
                self.send(node_address.clone(), auth_packet).await;

                // Notify the application that the session has been established
                self.new_connection(enr, node_address.socket_addr, connection_direction)
                    .await;
            }
            None => {
                // Don't know the ENR. Establish the session, but request an ENR also

                // Send the Auth response
                let contact = request_call.contact().clone();
                trace!("Sending Authentication response to node: {}", node_address);
                request_call.update_packet(auth_packet.clone());
                request_call.set_handshake_sent();
                // Reinsert the request_call
                self.insert_active_request(request_call);
                self.send(node_address.clone(), auth_packet).await;

                let id = RequestId::random();
                let request = RequestBody::FindNode { distances: vec![0] };
                session.awaiting_enr = Some(id.clone());
                if let Err(e) = self
                    .send_request::<P>(contact, HandlerReqId::Internal(id), request)
                    .await
                {
                    warn!("Failed to send Enr request {}", e)
                }
            }
        }
        self.new_session(node_address, session);
    }

    /// Verifies a Node ENR to it's observed address. If it fails, any associated session is also
    /// considered failed. If it succeeds, we notify the application.
    fn verify_enr(&self, enr: &Enr, node_address: &NodeAddress) -> bool {
        // If the ENR does not match the observed IP addresses, we consider the Session
        // failed.
        enr.node_id() == node_address.node_id
            && match node_address.socket_addr {
                SocketAddr::V4(socket_addr) => enr
                    .udp4_socket()
                    .map_or(true, |advertized_addr| socket_addr == advertized_addr),
                SocketAddr::V6(socket_addr) => enr
                    .udp6_socket()
                    .map_or(true, |advertized_addr| socket_addr == advertized_addr),
            }
    }

    /// Handle a message that contains an authentication header.
    #[allow(clippy::too_many_arguments)]
    async fn handle_auth_message<P: ProtocolIdentity>(
        &mut self,
        node_address: NodeAddress,
        message_nonce: MessageNonce,
        id_nonce_sig: &[u8],
        ephem_pubkey: &[u8],
        enr_record: Option<Enr>,
        message: &[u8],
        authenticated_data: &[u8],
    ) {
        // Needs to match an outgoing challenge packet (so we have the required nonce to be signed). If it doesn't we drop the packet.
        // This will lead to future outgoing challenges if they proceed to send further encrypted
        // packets.
        trace!(
            "Received an Authentication header message from: {}",
            node_address
        );

        if let Some(challenge) = self.active_challenges.remove(&node_address) {
            match Session::establish_from_challenge(
                self.key.clone(),
                &self.node_id,
                challenge,
                id_nonce_sig,
                ephem_pubkey,
                enr_record,
                &node_address,
                &mut self.sessions,
            ) {
                Ok((mut session, enr)) => {
                    // Receiving an AuthResponse must give us an up-to-date view of the node ENR.
                    // Verify the ENR is valid
                    if self.verify_enr(&enr, &node_address) {
                        // Session is valid
                        // Notify the application
                        // The session established here are from WHOAREYOU packets that we sent.
                        // This occurs when a node established a connection with us.
                        self.new_connection(
                            enr,
                            node_address.socket_addr,
                            ConnectionDirection::Incoming,
                        )
                        .await;
                        self.new_session(node_address.clone(), session);
                        self.nat_hole_puncher
                            .new_peer_latest_relay_cache
                            .pop(&node_address.node_id);
                        self.handle_message::<P>(
                            node_address.clone(),
                            message_nonce,
                            message,
                            authenticated_data,
                        )
                        .await;
                        // We could have pending messages that were awaiting this session to be
                        // established. If so process them.
                        self.send_next_request::<P>(node_address).await;
                    } else {
                        // IP's or NodeAddress don't match. Drop the session.
                        warn!(
                            "Session has invalid ENR. Enr sockets: {:?}, {:?}. Expected: {}",
                            enr.udp4_socket(),
                            enr.udp6_socket(),
                            node_address
                        );
                        self.fail_session(&node_address, RequestError::InvalidRemoteEnr, true)
                            .await;

                        // Respond to PING request even if the ENR or NodeAddress don't match
                        // so that the source node can notice its external IP address has been changed.
                        let maybe_ping_request = match session.decrypt_message(
                            message_nonce,
                            message,
                            authenticated_data,
                        ) {
                            Ok(m) => match Message::decode(&m) {
                                Ok(Message::Request(request)) if request.msg_type() == 1 => {
                                    Some(request)
                                }
                                _ => None,
                            },
                            _ => None,
                        };
                        if let Some(request) = maybe_ping_request {
                            debug!(
                                "Responding to a PING request using a one-time session. node_address: {}",
                                node_address
                            );
                            self.one_time_sessions
                                .insert(node_address.clone(), (request.id.clone(), session));
                            if let Err(e) = self
                                .service_send
                                .send(HandlerOut::Request(node_address.clone(), Box::new(request)))
                                .await
                            {
                                warn!("Failed to report request to application {}", e);
                                self.one_time_sessions.remove(&node_address);
                            }
                        }
                    }
                }
                Err(Discv5Error::InvalidChallengeSignature(challenge)) => {
                    warn!(
                        "Authentication header contained invalid signature. Ignoring packet from: {}",
                        node_address
                    );
                    // insert back the challenge
                    self.active_challenges.insert(node_address, challenge);
                }
                Err(Discv5Error::LimitSessionsUnreachableEnr) => {
                    warn!("Limit reached for sessions with unreachable ENRs. Dropping session.");
                }
                Err(e) => {
                    warn!(
                        "Invalid Authentication header. Dropping session. Error: {:?}",
                        e
                    );
                    self.fail_session(&node_address, RequestError::InvalidRemotePacket, true)
                        .await;
                }
            }
        } else {
            warn!(
                "Received an authenticated header without a matching WHOAREYOU request. {}",
                node_address
            );
        }
    }

    async fn send_next_request<P: ProtocolIdentity>(&mut self, node_address: NodeAddress) {
        // ensure we are not over writing any existing requests
        if self.active_requests.get(&node_address).is_none() {
            if let std::collections::hash_map::Entry::Occupied(mut entry) =
                self.pending_requests.entry(node_address)
            {
                // If it exists, there must be a request here
                let PendingRequest {
                    contact,
                    request_id,
                    request,
                } = entry.get_mut().remove(0);
                if entry.get().is_empty() {
                    entry.remove();
                }
                trace!("Sending next awaiting message. Node: {}", contact);
                if let Err(request_error) = self
                    .send_request::<P>(contact, request_id.clone(), request)
                    .await
                {
                    warn!("Failed to send next awaiting request {}", request_error);
                    // Inform the service that the request failed
                    match request_id {
                        HandlerReqId::Internal(_) => {
                            // An internal request could not be sent. For now we do nothing about
                            // this.
                        }
                        HandlerReqId::External(id) => {
                            if let Err(e) = self
                                .service_send
                                .send(HandlerOut::RequestFailed(id, request_error))
                                .await
                            {
                                warn!("Failed to inform that request failed {}", e);
                            }
                        }
                    }
                }
            }
        }
    }

    /// Handle a session message packet, that is dropped if it can't be decrypted.
    async fn handle_session_message<P: ProtocolIdentity>(
        &mut self,
        node_address: NodeAddress, // session message sender
        message_nonce: MessageNonce,
        message: &[u8],
        authenticated_data: &[u8],
    ) {
        // check if we have an available session
        let Some(session) = self.sessions.cache.get_mut(&node_address) else {
            warn!(
                "Dropping message. Error: {}, {}",
                Discv5Error::SessionNotEstablished,
                node_address
            );
            return;
        };
        // attempt to decrypt notification (same decryption as for a message)
        let message = match session.decrypt_message(message_nonce, message, authenticated_data) {
            Err(e) => {
                // We have a session, but the session message could not be decrypted. It is
                // likely the node sending this message has dropped their session. Since
                // this is a session message that assumes an established session, we do
                // not reply with a WHOAREYOU to this random packet. This means we drop
                // the packet.
                warn!(
                    "Dropping message that should have been part of a session. Error: {}",
                    e
                );
                return;
            }
            Ok(ref bytes) => match Message::decode(bytes) {
                Ok(message) => message,
                Err(err) => {
                    warn!(
                        "Failed to decode message. Error: {:?}, {}",
                        err, node_address
                    );
                    return;
                }
            },
        };

        match message {
            Message::Response(response) => self.handle_response::<P>(node_address, response).await,
            Message::Notification(notif) => match notif {
                Notification::RelayInit(initr, tgt, timed_out_nonce) => {
                    let initr_node_id = initr.node_id();
                    if initr_node_id != node_address.node_id {
                        warn!("peer {node_address} tried to initiate hole punch attempt for another node {initr_node_id}, banning peer {node_address}");
                        self.fail_session(&node_address, RequestError::MaliciousRelayInit, true)
                            .await;
                        let ban_timeout = self
                            .nat_hole_puncher
                            .ban_duration
                            .map(|v| Instant::now() + v);
                        PERMIT_BAN_LIST.write().ban(node_address, ban_timeout);
                    } else if let Err(e) = self.on_relay_init(initr, tgt, timed_out_nonce).await {
                        warn!("failed handling notification to relay for {node_address}, {e}");
                    }
                }
                Notification::RelayMsg(initr, timed_out_nonce) => {
                    match self.nat_hole_puncher.is_behind_nat {
                        Some(false) => {
                            // initr may not be malicious and initiated a hole punch attempt when
                            // a request to this node timed out for another reason
                            debug!("peer {node_address} relayed a hole punch notification but we are not behind nat");
                        }
                        _ => {
                            if let Err(e) = self.on_relay_msg(initr, timed_out_nonce).await {
                                warn!(
                                    "failed handling notification relayed from {node_address}, {e}"
                                );
                            }
                        }
                    }
                }
            },
            Message::Request(_) => {
                warn!(
                    "Peer sent message type {} that shouldn't be sent in packet type `Session Message`, {}",
                    message.msg_type(),
                    node_address,
                );
            }
        }
    }

    /// Handle a standard message that does not contain an authentication header.
    async fn handle_message<P: ProtocolIdentity>(
        &mut self,
        node_address: NodeAddress,
        message_nonce: MessageNonce,
        message: &[u8],
        authenticated_data: &[u8],
    ) {
        // check if we have an available session
        if let Some(session) = self.sessions.cache.get_mut(&node_address) {
            // attempt to decrypt and process the message.
            let message = match session.decrypt_message(message_nonce, message, authenticated_data)
            {
                Ok(m) => match Message::decode(&m) {
                    Ok(p) => p,
                    Err(e) => {
                        warn!("Failed to decode message. Error: {:?}, {}", e, node_address);
                        return;
                    }
                },
                Err(e) => {
                    // We have a session, but the message could not be decrypted. It is likely the node
                    // sending this message has dropped their session. In this case, this message is a
                    // Random packet and we should reply with a WHOAREYOU.
                    // This means we need to drop the current session and re-establish.
                    trace!("Decryption failed. Error {}", e);
                    debug!(
                        "Message from node: {} is not encrypted with known session keys.",
                        node_address
                    );
                    self.fail_session(&node_address, RequestError::InvalidRemotePacket, true)
                        .await;
                    // If we haven't already sent a WhoAreYou,
                    // spawn a WHOAREYOU event to check for highest known ENR
                    if self.active_challenges.get(&node_address).is_none() {
                        let whoareyou_ref = WhoAreYouRef(node_address, message_nonce);
                        if let Err(e) = self
                            .service_send
                            .send(HandlerOut::WhoAreYou(whoareyou_ref))
                            .await
                        {
                            warn!("Failed to send WhoAreYou to the service {}", e)
                        }
                    } else {
                        trace!("WHOAREYOU packet already sent: {}", node_address);
                    }
                    return;
                }
            };

            trace!("Received message from: {}", node_address);

            match message {
                Message::Request(request) => {
                    // report the request to the application
                    if let Err(e) = self
                        .service_send
                        .send(HandlerOut::Request(node_address, Box::new(request)))
                        .await
                    {
                        warn!("Failed to report request to application {}", e)
                    }
                }
                Message::Response(response) => {
                    // Accept response in Message packet for backwards compatibility
                    warn!("Received a response in a `Message` packet, should be sent in a `SessionMessage`");
                    self.handle_response::<P>(node_address, response).await
                }
                Message::Notification(_) => {
                    warn!(
                        "Peer sent message type {} that shouldn't be sent in packet type `Message`, {}",
                        message.msg_type(),
                        node_address
                    );
                }
            }
        } else {
            // no session exists
            trace!("Received a message without a session. {}", node_address);
            trace!("Requesting a WHOAREYOU packet to be sent.");
            // spawn a WHOAREYOU event to check for highest known ENR
            let whoareyou_ref = WhoAreYouRef(node_address, message_nonce);
            if let Err(e) = self
                .service_send
                .send(HandlerOut::WhoAreYou(whoareyou_ref))
                .await
            {
                warn!(
                    "Spawn a WHOAREYOU event to check for highest known ENR failed {}",
                    e
                )
            }
        }
    }

    /// Handles a response to a request. Re-inserts the request call if the response is a multiple
    /// Nodes response.
    async fn handle_response<P: ProtocolIdentity>(
        &mut self,
        node_address: NodeAddress,
        response: Response,
    ) {
        // Sessions could be awaiting an ENR response. Check if this response matches
        // this
        // check if we have an available session
        let Some(session) = self.sessions.cache.get_mut(&node_address) else {
            warn!(
                "Dropping response. Error: {}, {}",
                Discv5Error::SessionNotEstablished,
                node_address
            );
            return;
        };

        if let Some(request_id) = session.awaiting_enr.as_ref() {
            if &response.id == request_id {
                session.awaiting_enr = None;
                if let ResponseBody::Nodes { mut nodes, .. } = response.body {
                    // Received the requested ENR
                    let Some(enr) = nodes.pop() else {
                        return;
                    };
                    if self.verify_enr(&enr, &node_address) {
                        // Notify the application
                        // This can occur when we try to dial a node without an
                        // ENR. In this case we have attempted to establish the
                        // connection, so this is an outgoing connection.
                        self.new_connection(
                            enr,
                            node_address.socket_addr,
                            ConnectionDirection::Outgoing,
                        )
                        .await;
                        return;
                    }
                }
                debug!("Session failed invalid ENR response");
                self.fail_session(&node_address, RequestError::InvalidRemoteEnr, true)
                    .await;
                return;
            }
        }

        // Handle standard responses

        // Find a matching request, if any
        if let Some(mut request_call) = self.active_requests.remove(&node_address) {
            let id = match request_call.id() {
                HandlerReqId::Internal(id) | HandlerReqId::External(id) => id,
            };
            if id != &response.id {
                trace!(
                    "Received an RPC Response to an unknown request. Likely late response. {}",
                    node_address
                );
                // add the request back and reset the timer
                self.active_requests.insert(node_address, request_call);
                return;
            }

            // The response matches a request

            // Check to see if this is a Nodes response, in which case we may require to wait for
            // extra responses
            if let ResponseBody::Nodes { total, ref nodes } = response.body {
                for node in nodes {
                    if let Some(socket_addr) =
                        self.nat_hole_puncher.ip_mode.get_contactable_addr(node)
                    {
                        let node_id = node.node_id();
                        let new_peer_node_address = NodeAddress {
                            socket_addr,
                            node_id,
                        };
                        if self.sessions.cache.peek(&new_peer_node_address).is_none() {
                            self.nat_hole_puncher
                                .new_peer_latest_relay_cache
                                .put(node_id, node_address.clone());
                        }
                    }
                }
                if total > 1 {
                    // This is a multi-response Nodes response
                    if let Some(remaining_responses) = request_call.remaining_responses_mut() {
                        *remaining_responses -= 1;
                        if remaining_responses != &0 {
                            // more responses remaining, add back the request and send the response
                            // add back the request and send the response
                            self.active_requests
                                .insert(node_address.clone(), request_call);
                            if let Err(e) = self
                                .service_send
                                .send(HandlerOut::Response(node_address, Box::new(response)))
                                .await
                            {
                                warn!("Failed to inform of response {}", e)
                            }
                            return;
                        }
                    } else {
                        // This is the first instance
                        *request_call.remaining_responses_mut() = Some(total - 1);
                        // add back the request and send the response
                        self.active_requests
                            .insert(node_address.clone(), request_call);
                        if let Err(e) = self
                            .service_send
                            .send(HandlerOut::Response(node_address, Box::new(response)))
                            .await
                        {
                            warn!("Failed to inform of response {}", e)
                        }
                        return;
                    }
                }
            }

            // Remove the expected response
            self.remove_expected_response(node_address.socket_addr);

            // The request matches report the response
            if let Err(e) = self
                .service_send
                .send(HandlerOut::Response(
                    node_address.clone(),
                    Box::new(response),
                ))
                .await
            {
                warn!("Failed to inform of response {}", e)
            }
            self.send_next_request::<P>(node_address).await;
        } else {
            // This is likely a late response and we have already failed the request. These get
            // dropped here.
            trace!("Late response from node: {}", node_address);
        }
    }

    /// Inserts a request and associated auth_tag mapping.
    fn insert_active_request(&mut self, request_call: RequestCall) {
        let node_address = request_call.contact().node_address();

        // adds the mapping of message nonce to node address
        self.active_requests.insert(node_address, request_call);
    }

    fn new_session(&mut self, node_address: NodeAddress, session: Session) {
        if let Some(current_session) = self.sessions.cache.get_mut(&node_address) {
            current_session.update(session);
        } else {
            self.sessions.cache.insert(node_address, session);
            METRICS
                .active_sessions
                .store(self.sessions.cache.len(), Ordering::Relaxed);
        }
    }

    /// Remove one-time session by the given NodeAddress and RequestId if exists.
    fn remove_one_time_session(
        &mut self,
        node_address: &NodeAddress,
        request_id: &RequestId,
    ) -> Option<Session> {
        match self.one_time_sessions.peek(node_address) {
            Some((id, _)) if id == request_id => {
                let (_, session) = self
                    .one_time_sessions
                    .remove(node_address)
                    .expect("one-time session must exist");
                Some(session)
            }
            _ => None,
        }
    }

    /// A request has failed.
    async fn fail_request(
        &mut self,
        request_call: RequestCall,
        error: RequestError,
        remove_session: bool,
    ) {
        // The Request has expired, remove the session.
        // Fail the current request
        match request_call.id() {
            HandlerReqId::Internal(_) => {
                // Do not report failures on requests belonging to the handler.
            }
            HandlerReqId::External(id) => {
                if let Err(e) = self
                    .service_send
                    .send(HandlerOut::RequestFailed(id.clone(), error.clone()))
                    .await
                {
                    warn!("Failed to inform request failure {}", e)
                }
            }
        }
        let node_address = request_call.contact().node_address();
        self.nat_hole_puncher
            .new_peer_latest_relay_cache
            .pop(&node_address.node_id);
        self.fail_session(&node_address, error, remove_session)
            .await;
    }

    /// Removes a session and updates associated metrics and fields.
    async fn fail_session(
        &mut self,
        node_address: &NodeAddress,
        error: RequestError,
        remove_session: bool,
    ) {
        if remove_session {
            self.sessions.cache.remove(node_address);
            METRICS
                .active_sessions
                .store(self.sessions.cache.len(), Ordering::Relaxed);
            // stop keeping hole punched for peer
            self.nat_hole_puncher.untrack(&node_address.socket_addr);
        }
        if let Some(to_remove) = self.pending_requests.remove(node_address) {
            for PendingRequest { request_id, .. } in to_remove {
                match request_id {
                    HandlerReqId::Internal(_) => {
                        // Do not report failures on requests belonging to the handler.
                    }
                    HandlerReqId::External(id) => {
                        if let Err(e) = self
                            .service_send
                            .send(HandlerOut::RequestFailed(id, error.clone()))
                            .await
                        {
                            warn!("Failed to inform request failure {}", e)
                        }
                    }
                }
            }
        }
    }

    /// Assembles and sends a [`Packet`].
    async fn send(&mut self, node_address: NodeAddress, packet: Packet) {
        let outbound_packet = socket::OutboundPacket {
            node_address,
            packet,
        };
        self.send_outbound(outbound_packet.into()).await;
    }

    /// Sends a packet to the send handler to be encoded and sent.
    async fn send_outbound(&mut self, packet: Outbound) {
        let dst = *packet.dst();
        if let Err(e) = self.socket.send.send(packet).await {
            warn!("Failed to send outbound packet {}", e)
        }
        self.nat_hole_puncher.track(dst);
    }

    /// Check if any banned nodes have served their time and unban them.
    fn unban_nodes_check(&self) {
        PERMIT_BAN_LIST
            .write()
            .ban_ips
            .retain(|_, time| time.is_none() || Some(Instant::now()) < *time);
        PERMIT_BAN_LIST
            .write()
            .ban_nodes
            .retain(|_, time| time.is_none() || Some(Instant::now()) < *time);
    }

    async fn new_connection(
        &mut self,
        enr: Enr,
        socket_addr: SocketAddr,
        conn_dir: ConnectionDirection,
    ) {
        if let Err(e) = self
            .service_send
            .send(HandlerOut::Established(enr, socket_addr, conn_dir))
            .await
        {
            warn!(
                "Failed to inform of established connection {}, {}",
                conn_dir, e
            )
        }
    }
}

#[async_trait::async_trait]
impl HolePunchNat for Handler {
    async fn on_request_time_out<P: ProtocolIdentity>(
        &mut self,
        relay: NodeAddress,
        local_enr: Enr, // initiator-enr
        timed_out_nonce: MessageNonce,
        target_node_address: NodeAddress,
    ) -> Result<(), NatHolePunchError> {
        // Another hole punch process with this target may have just completed.
        if self.sessions.cache.get(&target_node_address).is_some() {
            return Ok(());
        }
        if let Some(session) = self.sessions.cache.get_mut(&relay) {
            let relay_init_notif =
                Notification::RelayInit(local_enr, target_node_address.node_id, timed_out_nonce);
            trace!(
                "Sending notif to relay {}. relay init: {}",
                relay.node_id,
                relay_init_notif,
            );
            // Encrypt the message and send
            let packet = match session
                .encrypt_session_message::<P>(self.node_id, &relay_init_notif.encode())
            {
                Ok(packet) => packet,
                Err(e) => {
                    return Err(NatHolePunchError::Initiator(e));
                }
            };
            self.send(relay, packet).await;
        } else {
            // Drop hole punch attempt with this relay, to ensure hole punch round-trip time stays
            // within the time out of the udp entrypoint for the target peer in the initiator's
            // router, set by the original timed out FINDNODE request from the initiator, as the
            // initiator may also be behind a NAT.
            warn!(
                "Session is not established. Dropping relay notification for relay: {}",
                relay.node_id
            );
        }
        Ok(())
    }

    async fn on_relay_init(
        &mut self,
        initr: Enr,
        tgt: NodeId,
        timed_out_nonce: MessageNonce,
    ) -> Result<(), NatHolePunchError> {
        // Assemble the notification for the target
        let relay_msg_notif = Notification::RelayMsg(initr, timed_out_nonce);

        // Check for target peer in our kbuckets otherwise drop notification.
        if let Err(e) = self
            .service_send
            .send(HandlerOut::FindHolePunchEnr(tgt, relay_msg_notif))
            .await
        {
            return Err(NatHolePunchError::Relay(e.into()));
        }
        Ok(())
    }

    async fn on_relay_msg(
        &mut self,
        initr: Enr,
        timed_out_nonce: MessageNonce,
    ) -> Result<(), NatHolePunchError> {
        let initiator_node_address =
            match NodeContact::try_from_enr(initr, self.nat_hole_puncher.ip_mode) {
                Ok(contact) => contact.node_address(),
                Err(e) => return Err(NatHolePunchError::Target(e.into())),
            };

        // A session may already have been established.
        if self.sessions.cache.get(&initiator_node_address).is_some() {
            trace!(
                "Session already established with initiator: {}",
                initiator_node_address
            );
            return Ok(());
        }
        // Possibly, an attempt to punch this hole, using another relay, is in progress.
        if self
            .active_challenges
            .get(&initiator_node_address)
            .is_some()
        {
            trace!(
                "WHOAREYOU packet already sent to initiator: {}",
                initiator_node_address
            );
            return Ok(());
        }
        // If not hole punch attempts are in progress, spawn a WHOAREYOU event to punch a hole in
        // our NAT for initiator.
        let whoareyou_ref = WhoAreYouRef(initiator_node_address, timed_out_nonce);
        if let Err(e) = self
            .service_send
            .send(HandlerOut::WhoAreYou(whoareyou_ref))
            .await
        {
            return Err(NatHolePunchError::Target(e.into()));
        }
        Ok(())
    }

    async fn send_relay_msg_notif<P: ProtocolIdentity>(
        &mut self,
        tgt_enr: Enr,
        relay_msg_notif: Notification,
    ) -> Result<(), NatHolePunchError> {
        let tgt_node_address =
            match NodeContact::try_from_enr(tgt_enr, self.nat_hole_puncher.ip_mode) {
                Ok(contact) => contact.node_address(),
                Err(e) => return Err(NatHolePunchError::Relay(e.into())),
            };
        if let Some(session) = self.sessions.cache.get_mut(&tgt_node_address) {
            trace!(
                "Sending notif to target {}. relay msg: {}",
                tgt_node_address.node_id,
                relay_msg_notif,
            );
            // Encrypt the notification and send
            let packet = match session
                .encrypt_session_message::<P>(self.node_id, &relay_msg_notif.encode())
            {
                Ok(packet) => packet,
                Err(e) => {
                    return Err(NatHolePunchError::Relay(e));
                }
            };
            self.send(tgt_node_address, packet).await;
            Ok(())
        } else {
            // Either the session is being established or has expired. We simply drop the
            // notification in this case to ensure hole punch round-trip time stays within the
            // time out of the udp entrypoint for the target peer in the initiator's NAT, set by
            // the original timed out FINDNODE request from the initiator, as the initiator may
            // also be behind a NAT.
            Err(NatHolePunchError::Relay(Discv5Error::SessionNotEstablished))
        }
    }

    #[inline]
    async fn on_hole_punch_expired(&mut self, peer: SocketAddr) -> Result<(), NatHolePunchError> {
        self.send_outbound(peer.into()).await;
        Ok(())
    }
}
