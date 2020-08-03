// Copyright 2020 Parity Technologies (UK) Ltd.
// This file is part of Substrate.
//
// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with Substrate.  If not, see <http://www.gnu.org/licenses/>.

//! `NetworkBehaviour` implementation which handles incoming finality proof requests.
//!
//! Every request is coming in on a separate connection substream which gets
//! closed after we have sent the response back. Incoming requests are encoded
//! as protocol buffers (cf. `finality.v1.proto`).

#![allow(unused)]

use bytes::Bytes;
use codec::{Encode, Decode};
use crate::{
	chain::FinalityProofProvider,
	config::ProtocolId,
	protocol::message,
	schema,
};
use futures::{future::BoxFuture, prelude::*, stream::FuturesUnordered};
use libp2p::{
	core::{
		ConnectedPoint,
		Multiaddr,
		PeerId,
		connection::ConnectionId,
		upgrade::{InboundUpgrade, OutboundUpgrade, ReadOneError, UpgradeInfo, Negotiated},
		upgrade::{DeniedUpgrade, read_one, write_one}
	},
	swarm::{
		NegotiatedSubstream,
		NetworkBehaviour,
		NetworkBehaviourAction,
		NotifyHandler,
		OneShotHandler,
		OneShotHandlerConfig,
		PollParameters,
		SubstreamProtocol
	}
};
use prost::Message;
use sp_runtime::{generic::BlockId, traits::{Block, Header, One, Zero}};
use std::{
	cmp::min,
	collections::VecDeque,
	io,
	iter,
	marker::PhantomData,
	sync::Arc,
	time::Duration,
	task::{Context, Poll}
};
use void::{Void, unreachable};

// Type alias for convenience.
pub type Error = Box<dyn std::error::Error + 'static>;

/// Event generated by the finality proof requests behaviour.
#[derive(Debug)]
pub enum Event<B: Block> {
	/// A response to a finality proof request has arrived.
	Response {
		peer: PeerId,
		/// Block hash originally passed to `send_request`.
		block_hash: B::Hash,
		/// Finality proof returned by the remote.
		proof: Vec<u8>,
	},
}

/// Configuration options for `FinalityProofRequests`.
#[derive(Debug, Clone)]
pub struct Config {
	max_request_len: usize,
	max_response_len: usize,
	inactivity_timeout: Duration,
	protocol: Bytes,
}

impl Config {
	/// Create a fresh configuration with the following options:
	///
	/// - max. request size = 1 MiB
	/// - max. response size = 1 MiB
	/// - inactivity timeout = 15s
	pub fn new(id: &ProtocolId) -> Self {
		let mut c = Config {
			max_request_len: 1024 * 1024,
			max_response_len: 1024 * 1024,
			inactivity_timeout: Duration::from_secs(15),
			protocol: Bytes::new(),
		};
		c.set_protocol(id);
		c
	}

	/// Limit the max. length of incoming finality proof request bytes.
	pub fn set_max_request_len(&mut self, v: usize) -> &mut Self {
		self.max_request_len = v;
		self
	}

	/// Limit the max. length of incoming finality proof response bytes.
	pub fn set_max_response_len(&mut self, v: usize) -> &mut Self {
		self.max_response_len = v;
		self
	}

	/// Limit the max. duration the substream may remain inactive before closing it.
	pub fn set_inactivity_timeout(&mut self, v: Duration) -> &mut Self {
		self.inactivity_timeout = v;
		self
	}

	/// Set protocol to use for upgrade negotiation.
	pub fn set_protocol(&mut self, id: &ProtocolId) -> &mut Self {
		let mut v = Vec::new();
		v.extend_from_slice(b"/");
		v.extend_from_slice(id.as_bytes());
		v.extend_from_slice(b"/finality-proof/1");
		self.protocol = v.into();
		self
	}
}

/// The finality proof request handling behaviour.
pub struct FinalityProofRequests<B: Block> {
	/// This behaviour's configuration.
	config: Config,
	/// How to construct finality proofs.
	finality_proof_provider: Option<Arc<dyn FinalityProofProvider<B>>>,
	/// Futures sending back the finality proof request responses.
	outgoing: FuturesUnordered<BoxFuture<'static, ()>>,
	/// Events to return as soon as possible from `poll`.
	pending_events: VecDeque<NetworkBehaviourAction<OutboundProtocol<B>, Event<B>>>,
}

impl<B> FinalityProofRequests<B>
where
	B: Block,
{
	/// Initializes the behaviour.
	///
	/// If the proof provider is `None`, then the behaviour will not support the finality proof
	/// requests protocol.
	pub fn new(cfg: Config, finality_proof_provider: Option<Arc<dyn FinalityProofProvider<B>>>) -> Self {
		FinalityProofRequests {
			config: cfg,
			finality_proof_provider,
			outgoing: FuturesUnordered::new(),
			pending_events: VecDeque::new(),
		}
	}

	/// Issue a new finality proof request.
	///
	/// If the response doesn't arrive in time, or if the remote answers improperly, the target
	/// will be disconnected.
	pub fn send_request(&mut self, target: &PeerId, block_hash: B::Hash, request: Vec<u8>) {
		let protobuf_rq = schema::v1::finality::FinalityProofRequest {
			block_hash: block_hash.encode(),
			request,
		};

		let mut buf = Vec::with_capacity(protobuf_rq.encoded_len());
		if let Err(err) = protobuf_rq.encode(&mut buf) {
			log::warn!("failed to encode finality proof request {:?}: {:?}", protobuf_rq, err);
			return;
		}

		log::trace!("enqueueing finality proof request to {:?}: {:?}", target, protobuf_rq);
		self.pending_events.push_back(NetworkBehaviourAction::NotifyHandler {
			peer_id: target.clone(),
			handler: NotifyHandler::Any,
			event: OutboundProtocol {
				request: buf,
				block_hash,
				max_response_size: self.config.max_response_len,
				protocol: self.config.protocol.clone(),
			},
		});
	}

	/// Callback, invoked when a new finality request has been received from remote.
	fn on_finality_request(&mut self, peer: &PeerId, request: &schema::v1::finality::FinalityProofRequest)
		-> Result<schema::v1::finality::FinalityProofResponse, Error>
	{
		let block_hash = Decode::decode(&mut request.block_hash.as_ref())?;

		log::trace!(target: "sync", "Finality proof request from {} for {}", peer, block_hash);

		// Note that an empty Vec is sent if no proof is available.
		let finality_proof = if let Some(provider) = &self.finality_proof_provider {
			provider
				.prove_finality(block_hash, &request.request)?
				.unwrap_or(Vec::new())
		} else {
			log::error!("Answering a finality proof request while finality provider is empty");
			return Err(From::from("Empty finality proof provider".to_string()))
		};

		Ok(schema::v1::finality::FinalityProofResponse { proof: finality_proof })
	}
}

impl<B> NetworkBehaviour for FinalityProofRequests<B>
where
	B: Block
{
	type ProtocolsHandler = OneShotHandler<InboundProtocol<B>, OutboundProtocol<B>, NodeEvent<B, NegotiatedSubstream>>;
	type OutEvent = Event<B>;

	fn new_handler(&mut self) -> Self::ProtocolsHandler {
		let p = InboundProtocol {
			max_request_len: self.config.max_request_len,
			protocol: if self.finality_proof_provider.is_some() {
				Some(self.config.protocol.clone())
			} else {
				None
			},
			marker: PhantomData,
		};
		let mut cfg = OneShotHandlerConfig::default();
		cfg.keep_alive_timeout = self.config.inactivity_timeout;
		OneShotHandler::new(SubstreamProtocol::new(p), cfg)
	}

	fn addresses_of_peer(&mut self, _: &PeerId) -> Vec<Multiaddr> {
		Vec::new()
	}

	fn inject_connected(&mut self, _peer: &PeerId) {
	}

	fn inject_disconnected(&mut self, _peer: &PeerId) {
	}

	fn inject_event(
		&mut self,
		peer: PeerId,
		connection: ConnectionId,
		event: NodeEvent<B, NegotiatedSubstream>
	) {
		match event {
			NodeEvent::Request(request, mut stream) => {
				match self.on_finality_request(&peer, &request) {
					Ok(res) => {
						log::trace!("enqueueing finality response for peer {}", peer);
						let mut data = Vec::with_capacity(res.encoded_len());
						if let Err(e) = res.encode(&mut data) {
							log::debug!("error encoding finality response for peer {}: {}", peer, e)
						} else {
							let future = async move {
								if let Err(e) = write_one(&mut stream, data).await {
									log::debug!("error writing finality response: {}", e)
								}
							};
							self.outgoing.push(future.boxed())
						}
					}
					Err(e) => log::debug!("error handling finality request from peer {}: {}", peer, e)
				}
			}
			NodeEvent::Response(response, block_hash) => {
				let ev = Event::Response {
					peer,
					block_hash,
					proof: response.proof,
				};
				self.pending_events.push_back(NetworkBehaviourAction::GenerateEvent(ev));
			}
		}
	}

	fn poll(&mut self, cx: &mut Context, _: &mut impl PollParameters)
		-> Poll<NetworkBehaviourAction<OutboundProtocol<B>, Event<B>>>
	{
		if let Some(ev) = self.pending_events.pop_front() {
			return Poll::Ready(ev);
		}

		while let Poll::Ready(Some(_)) = self.outgoing.poll_next_unpin(cx) {}
		Poll::Pending
	}
}

/// Output type of inbound and outbound substream upgrades.
#[derive(Debug)]
pub enum NodeEvent<B: Block, T> {
	/// Incoming request from remote and substream to use for the response.
	Request(schema::v1::finality::FinalityProofRequest, T),
	/// Incoming response from remote.
	Response(schema::v1::finality::FinalityProofResponse, B::Hash),
}

/// Substream upgrade protocol.
///
/// We attempt to parse an incoming protobuf encoded request (cf. `Request`)
/// which will be handled by the `FinalityProofRequests` behaviour, i.e. the request
/// will become visible via `inject_node_event` which then dispatches to the
/// relevant callback to process the message and prepare a response.
#[derive(Debug, Clone)]
pub struct InboundProtocol<B> {
	/// The max. request length in bytes.
	max_request_len: usize,
	/// The protocol to use during upgrade negotiation. If `None`, then the incoming protocol
	/// is simply disabled.
	protocol: Option<Bytes>,
	/// Marker to pin the block type.
	marker: PhantomData<B>,
}

impl<B: Block> UpgradeInfo for InboundProtocol<B> {
	type Info = Bytes;
	// This iterator will return either 0 elements if `self.protocol` is `None`, or 1 element if
	// it is `Some`.
	type InfoIter = std::option::IntoIter<Self::Info>;

	fn protocol_info(&self) -> Self::InfoIter {
		self.protocol.clone().into_iter()
	}
}

impl<B, T> InboundUpgrade<T> for InboundProtocol<B>
where
	B: Block,
	T: AsyncRead + AsyncWrite + Unpin + Send + 'static
{
	type Output = NodeEvent<B, T>;
	type Error = ReadOneError;
	type Future = BoxFuture<'static, Result<Self::Output, Self::Error>>;

	fn upgrade_inbound(self, mut s: T, _: Self::Info) -> Self::Future {
		async move {
			let len = self.max_request_len;
			let vec = read_one(&mut s, len).await?;
			match schema::v1::finality::FinalityProofRequest::decode(&vec[..]) {
				Ok(r) => Ok(NodeEvent::Request(r, s)),
				Err(e) => Err(ReadOneError::Io(io::Error::new(io::ErrorKind::Other, e)))
			}
		}.boxed()
	}
}

/// Substream upgrade protocol.
///
/// Sends a request to remote and awaits the response.
#[derive(Debug, Clone)]
pub struct OutboundProtocol<B: Block> {
	/// The serialized protobuf request.
	request: Vec<u8>,
	/// Block hash that has been requested.
	block_hash: B::Hash,
	/// The max. response length in bytes.
	max_response_size: usize,
	/// The protocol to use for upgrade negotiation.
	protocol: Bytes,
}

impl<B: Block> UpgradeInfo for OutboundProtocol<B> {
	type Info = Bytes;
	type InfoIter = iter::Once<Self::Info>;

	fn protocol_info(&self) -> Self::InfoIter {
		iter::once(self.protocol.clone())
	}
}

impl<B, T> OutboundUpgrade<T> for OutboundProtocol<B>
where
	B: Block,
	T: AsyncRead + AsyncWrite + Unpin + Send + 'static
{
	type Output = NodeEvent<B, T>;
	type Error = ReadOneError;
	type Future = BoxFuture<'static, Result<Self::Output, Self::Error>>;

	fn upgrade_outbound(self, mut s: T, _: Self::Info) -> Self::Future {
		async move {
			write_one(&mut s, &self.request).await?;
			let vec = read_one(&mut s, self.max_response_size).await?;

			schema::v1::finality::FinalityProofResponse::decode(&vec[..])
				.map(|r| NodeEvent::Response(r, self.block_hash))
				.map_err(|e| {
					ReadOneError::Io(io::Error::new(io::ErrorKind::Other, e))
				})
		}.boxed()
	}
}
