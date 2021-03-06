// Copyright 2016 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under (1) the MaidSafe.net Commercial License,
// version 1.0 or later, or (2) The General Public License (GPL), version 3, depending on which
// licence you accepted on initial access to the Software (the "Licences").
//
// By contributing code to the SAFE Network Software, or to this project generally, you agree to be
// bound by the terms of the MaidSafe Contributor Agreement, version 1.1.  This, along with the
// Licenses can be found in the root directory of this project at LICENSE, COPYING and CONTRIBUTOR.
//
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.
//
// Please review the Licences for the specific language governing permissions and limitations
// relating to use of the SAFE Network Software.

use ack_manager::{Ack, AckManager};
use action::Action;
use crust::{PeerId, Service};
use crust::Event as CrustEvent;
use error::{InterfaceError, RoutingError};
use event::Event;
use evented::{Evented, ToEvented};
use id::{FullId, PublicId};
use maidsafe_utilities::serialisation;
use messages::{HopMessage, Message, MessageContent, RoutingMessage, SignedMessage, UserMessage,
               UserMessageCache};
use routing_message_filter::{FilteringResult, RoutingMessageFilter};
use routing_table::Authority;
use state_machine::Transition;
use stats::Stats;
use std::collections::BTreeSet;
use std::fmt::{self, Debug, Formatter};
use std::time::Duration;
use super::common::{Base, Bootstrapped, USER_MSG_CACHE_EXPIRY_DURATION_SECS};
use timer::Timer;
use xor_name::XorName;

/// A node connecting a user to the network, as opposed to a routing / data storage node.
///
/// Each client has a _proxy_: a node through which all requests are routed.
pub struct Client {
    ack_mgr: AckManager,
    crust_service: Service,
    full_id: FullId,
    min_group_size: usize,
    proxy_peer_id: PeerId,
    proxy_public_id: PublicId,
    routing_msg_filter: RoutingMessageFilter,
    stats: Stats,
    timer: Timer,
    user_msg_cache: UserMessageCache,
}

impl Client {
    #[cfg_attr(feature = "cargo-clippy", allow(too_many_arguments))]
    pub fn from_bootstrapping(crust_service: Service,
                              full_id: FullId,
                              min_group_size: usize,
                              proxy_peer_id: PeerId,
                              proxy_public_id: PublicId,
                              stats: Stats,
                              timer: Timer)
                              -> Evented<Self> {
        let client = Client {
            ack_mgr: AckManager::new(),
            crust_service: crust_service,
            full_id: full_id,
            min_group_size: min_group_size,
            proxy_peer_id: proxy_peer_id,
            proxy_public_id: proxy_public_id,
            routing_msg_filter: RoutingMessageFilter::new(),
            stats: stats,
            timer: timer,
            user_msg_cache: UserMessageCache::with_expiry_duration(
                Duration::from_secs(USER_MSG_CACHE_EXPIRY_DURATION_SECS)),
        };

        debug!("{:?} - State changed to client.", client);

        Evented::single(Event::Connected, client)
    }

    pub fn handle_action(&mut self, action: Action) -> Evented<Transition> {
        let mut events = Evented::empty();
        match action {
            Action::ClientSendRequest { content, dst, priority, result_tx } => {
                let src = Authority::Client {
                    client_key: *self.full_id.public_id().signing_public_key(),
                    proxy_node_name: *self.proxy_public_id.name(),
                    peer_id: self.crust_service.id(),
                };

                let user_msg = UserMessage::Request(content);
                let result = match self.send_user_message(src, dst, user_msg, priority)
                    .extract(&mut events) {
                    Err(RoutingError::Interface(err)) => Err(err),
                    Err(_) | Ok(_) => Ok(()),
                };

                let _ = result_tx.send(result);
            }
            Action::NodeSendMessage { result_tx, .. } => {
                let _ = result_tx.send(Err(InterfaceError::InvalidState));
            }
            Action::Name { result_tx } => {
                let _ = result_tx.send(*self.name());
            }
            Action::Timeout(token) => self.handle_timeout(token).extract(&mut events),
            Action::Terminate => {
                return Transition::Terminate.to_evented();
            }
        }

        events.with_value(Transition::Stay)
    }

    pub fn handle_crust_event(&mut self, crust_event: CrustEvent) -> Evented<Transition> {
        match crust_event {
            CrustEvent::LostPeer(peer_id) => self.handle_lost_peer(peer_id),
            CrustEvent::NewMessage(peer_id, bytes) => self.handle_new_message(peer_id, bytes),
            _ => {
                debug!("{:?} Unhandled crust event {:?}", self, crust_event);
                Transition::Stay.to_evented()
            }
        }
    }

    fn handle_ack_response(&mut self, ack: Ack) -> Evented<Transition> {
        self.ack_mgr.receive(ack);
        Transition::Stay.to_evented()
    }

    fn handle_timeout(&mut self, token: u64) -> Evented<()> {
        self.resend_unacknowledged_timed_out_msgs(token)
    }

    fn handle_new_message(&mut self, peer_id: PeerId, bytes: Vec<u8>) -> Evented<Transition> {
        let mut result = Evented::empty();

        let transition = match serialisation::deserialise(&bytes) {
            Ok(Message::Hop(hop_msg)) => {
                self.handle_hop_message(hop_msg, peer_id).extract(&mut result)
            }
            Ok(message) => {
                debug!("{:?} - Unhandled new message: {:?}", self, message);
                Ok(Transition::Stay)
            }
            Err(error) => Err(RoutingError::SerialisationError(error)),
        };

        match transition {
            Ok(transition) => result.with_value(transition),
            Err(RoutingError::FilterCheckFailed) => result.with_value(Transition::Stay),
            Err(error) => {
                debug!("{:?} - {:?}", self, error);
                result.with_value(Transition::Stay)
            }
        }
    }

    fn handle_hop_message(&mut self,
                          hop_msg: HopMessage,
                          peer_id: PeerId)
                          -> Evented<Result<Transition, RoutingError>> {
        let mut result = Evented::empty();

        if self.proxy_peer_id == peer_id {
            try_ev!(hop_msg.verify(self.proxy_public_id.signing_public_key()),
                    result);
        } else {
            return result.with_value(Err(RoutingError::UnknownConnection(peer_id)));
        }

        let signed_msg = hop_msg.content;
        try_ev!(signed_msg.check_integrity(self.min_group_size()), result);

        let routing_msg = signed_msg.routing_message();
        let in_authority = self.in_authority(&routing_msg.dst);
        if in_authority {
            self.send_ack(routing_msg, 0).extract(&mut result);
        }

        // Prevents us repeatedly handling identical messages sent by a malicious peer.
        match self.routing_msg_filter.filter_incoming(routing_msg, hop_msg.route) {
            FilteringResult::KnownMessage |
            FilteringResult::KnownMessageAndRoute => {
                return result.with_value(Err(RoutingError::FilterCheckFailed));
            }
            FilteringResult::NewMessage => (),
        }

        if !in_authority {
            return result.with_value(Ok(Transition::Stay));
        }

        result.and(self.dispatch_routing_message(routing_msg.clone()))
            .map(Ok)
    }

    fn dispatch_routing_message(&mut self, routing_msg: RoutingMessage) -> Evented<Transition> {
        match routing_msg.content {
            MessageContent::Ack(ack, _) => {
                let mut result = Evented::empty();
                let transition = self.handle_ack_response(ack).extract(&mut result);
                result.with_value(transition)
            }
            MessageContent::UserMessagePart { hash, part_count, part_index, payload, .. } => {
                trace!("{:?} Got UserMessagePart {:x}, {}/{} from {:?} to {:?}.",
                       self,
                       hash,
                       part_count,
                       part_index,
                       routing_msg.src,
                       routing_msg.dst);
                let mut result = Evented::empty();
                if let Some(msg) = self.user_msg_cache.add(hash, part_count, part_index, payload) {
                    self.stats().count_user_message(&msg);
                    result.add_event(msg.into_event(routing_msg.src, routing_msg.dst));
                }
                result.with_value(Transition::Stay)
            }
            content => {
                debug!("{:?} - Unhandled routing message: {:?} from {:?} to {:?}",
                       self,
                       content,
                       routing_msg.src,
                       routing_msg.dst);
                Transition::Stay.to_evented()
            }
        }
    }

    /// Sends the given message, possibly splitting it up into smaller parts.
    fn send_user_message(&mut self,
                         src: Authority<XorName>,
                         dst: Authority<XorName>,
                         user_msg: UserMessage,
                         priority: u8)
                         -> Evented<Result<(), RoutingError>> {
        self.stats.count_user_message(&user_msg);
        let mut result = Evented::empty();
        for part in try_ev!(user_msg.to_parts(priority), result) {
            let message = RoutingMessage {
                src: src,
                dst: dst,
                content: part,
            };
            try_evx!(self.send_routing_message(message), result);
        }
        result.map(Ok)
    }
}

impl Base for Client {
    fn crust_service(&self) -> &Service {
        &self.crust_service
    }

    fn full_id(&self) -> &FullId {
        &self.full_id
    }

    /// Does the given authority represent us?
    fn in_authority(&self, auth: &Authority<XorName>) -> bool {
        if let Authority::Client { ref client_key, .. } = *auth {
            client_key == self.full_id.public_id().signing_public_key()
        } else {
            false
        }
    }

    fn handle_lost_peer(&mut self, peer_id: PeerId) -> Evented<Transition> {
        if peer_id == self.crust_service().id() {
            error!("{:?} LostPeer fired with our crust peer id", self);
            return Transition::Stay.to_evented();
        }

        debug!("{:?} Received LostPeer - {:?}", self, peer_id);

        if self.proxy_peer_id == peer_id {
            debug!("{:?} Lost bootstrap connection to {:?} ({:?}).",
                   self,
                   self.proxy_public_id.name(),
                   peer_id);
            Evented::single(Event::Terminate, Transition::Terminate)
        } else {
            Transition::Stay.to_evented()
        }
    }

    fn stats(&mut self) -> &mut Stats {
        &mut self.stats
    }
}

impl Bootstrapped for Client {
    fn ack_mgr(&self) -> &AckManager {
        &self.ack_mgr
    }

    fn ack_mgr_mut(&mut self) -> &mut AckManager {
        &mut self.ack_mgr
    }

    fn min_group_size(&self) -> usize {
        self.min_group_size
    }

    fn resend_unacknowledged_timed_out_msgs(&mut self, token: u64) -> Evented<()> {
        let mut result = Evented::empty();
        if let Some((unacked_msg, ack)) = self.ack_mgr.find_timed_out(token) {
            trace!("{:?} - Timed out waiting for ack({}) {:?}",
                   self,
                   ack,
                   unacked_msg);

            if unacked_msg.route as usize == self.min_group_size {
                debug!("{:?} - Message unable to be acknowledged - giving up. {:?}",
                       self,
                       unacked_msg);
                self.stats.count_unacked();
            } else if let Err(error) =
                self.send_routing_message_via_route(unacked_msg.routing_msg, unacked_msg.route)
                    .extract(&mut result) {
                debug!("{:?} Failed to send message: {:?}", self, error);
            }
        }
        result
    }

    fn send_routing_message_via_route(&mut self,
                                      routing_msg: RoutingMessage,
                                      route: u8)
                                      -> Evented<Result<(), RoutingError>> {
        self.stats.count_route(route);

        if routing_msg.dst.is_client() && self.in_authority(&routing_msg.dst) {
            return Ok(()).to_evented(); // Message is for us.
        }

        // Get PeerId of the proxy node
        let (proxy_peer_id, sending_nodes) = match routing_msg.src {
            Authority::Client { ref proxy_node_name, .. } => {
                if *self.proxy_public_id.name() != *proxy_node_name {
                    error!("{:?} - Unable to find connection to proxy node in proxy map",
                           self);
                    return Err(RoutingError::ProxyConnectionNotFound).to_evented();
                }
                (self.proxy_peer_id, vec![])
            }
            _ => {
                error!("{:?} - Source should be client if our state is a Client",
                       self);
                return Err(RoutingError::InvalidSource).to_evented();
            }
        };

        let signed_msg = try_ev!(SignedMessage::new(routing_msg, self.full_id(), sending_nodes),
                                 Evented::empty());

        if self.add_to_pending_acks(&signed_msg, route) &&
           !self.filter_outgoing_routing_msg(signed_msg.routing_message(), &proxy_peer_id, route) {
            let bytes = try_ev!(self.to_hop_bytes(signed_msg.clone(), route, BTreeSet::new()),
                                Evented::empty());
            self.send_or_drop(&proxy_peer_id, bytes, signed_msg.priority());
        }

        Ok(()).to_evented()
    }

    fn routing_msg_filter(&mut self) -> &mut RoutingMessageFilter {
        &mut self.routing_msg_filter
    }

    fn timer(&mut self) -> &mut Timer {
        &mut self.timer
    }
}

#[cfg(feature = "use-mock-crust")]
impl Client {
    /// Resends all unacknowledged messages.
    pub fn resend_unacknowledged(&mut self) -> Evented<bool> {
        let mut result = Evented::empty();
        let timer_tokens = self.ack_mgr.timer_tokens();
        for timer_token in &timer_tokens {
            self.resend_unacknowledged_timed_out_msgs(*timer_token).extract(&mut result);
        }
        result.with_value(!timer_tokens.is_empty())
    }

    /// Are there any unacknowledged messages?
    pub fn has_unacknowledged(&self) -> bool {
        self.ack_mgr.has_pending()
    }
}

impl Debug for Client {
    fn fmt(&self, formatter: &mut Formatter) -> fmt::Result {
        write!(formatter, "Client({})", self.name())
    }
}
