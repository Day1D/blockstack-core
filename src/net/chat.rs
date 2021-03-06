/*
 copyright: (c) 2013-2019 by Blockstack PBC, a public benefit corporation.

 This file is part of Blockstack.

 Blockstack is free software. You may redistribute or modify
 it under the terms of the GNU General Public License as published by
 the Free Software Foundation, either version 3 of the License or
 (at your option) any later version.

 Blockstack is distributed in the hope that it will be useful,
 but WITHOUT ANY WARRANTY, including without the implied warranty of
 MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
 GNU General Public License for more details.

 You should have received a copy of the GNU General Public License
 along with Blockstack. If not, see <http://www.gnu.org/licenses/>.
*/

use net::PeerAddress;
use net::Neighbor;
use net::NeighborKey;
use net::Error as net_error;
use net::db::PeerDB;
use net::asn::ASEntry4;

use net::*;
use net::codec::*;

use net::StacksMessage;
use net::StacksP2P;
use net::connection::ConnectionP2P;
use net::connection::ReplyHandleP2P;
use net::connection::ConnectionOptions;

use net::poll::NetworkState;
use net::poll::NetworkPollState;

use net::neighbors::MAX_NEIGHBOR_BLOCK_DELAY;

use net::p2p::PeerNetwork;
use net::p2p::NetworkHandle;

use net::db::*;

use util::db::Error as db_error;
use util::db::DBConn;
use util::secp256k1::Secp256k1PublicKey;
use util::secp256k1::Secp256k1PrivateKey;

use chainstate::burn::db::burndb;
use chainstate::burn::db::burndb::BurnDB;

use burnchains::Burnchain;
use burnchains::BurnchainView;

use std::net::SocketAddr;

use std::collections::HashMap;
use std::collections::VecDeque;

use std::io::Read;
use std::io::Write;

use std::convert::TryFrom;

use util::log;
use util::get_epoch_time_secs;
use util::hash::to_hex;

use mio::net as mio_net;

use rusqlite::Transaction;

// did we or did we not successfully send a message?
#[derive(Debug, Clone)]
pub struct NeighborHealthPoint {
    pub success: bool,
    pub time: u64
}

impl Default for NeighborHealthPoint {
    fn default() -> NeighborHealthPoint {
        NeighborHealthPoint {
            success: false,
            time: 0
        }
    }
}

pub const NUM_HEALTH_POINTS : usize = 32;
pub const HEALTH_POINT_LIFETIME : u64 = 12 * 3600;  // 12 hours
    
#[derive(Debug, Clone)]
pub struct NeighborStats {
    pub outbound: bool,
    pub first_contact_time: u64,
    pub last_contact_time: u64,
    pub last_send_time: u64,
    pub last_recv_time: u64,
    pub last_handshake_time: u64,
    pub bytes_tx: u64,
    pub bytes_rx: u64,
    pub msgs_tx: u64,
    pub msgs_rx: u64,
    pub msgs_rx_unsolicited: u64,
    pub msgs_err: u64,
    pub healthpoints: VecDeque<NeighborHealthPoint>,
    pub peer_resets: u64,
    pub last_reset_time: u64,
    pub msg_rx_counts: HashMap<StacksMessageID, u64>,
}

impl NeighborStats {
    pub fn new(outbound: bool) -> NeighborStats {
        NeighborStats {
            outbound: outbound,
            first_contact_time: 0,
            last_contact_time: 0,
            last_send_time: 0,
            last_recv_time: 0,
            last_handshake_time: 0,
            bytes_tx: 0,
            bytes_rx: 0,
            msgs_tx: 0,
            msgs_rx: 0,
            msgs_rx_unsolicited: 0,
            msgs_err: 0,
            healthpoints: VecDeque::new(),
            peer_resets: 0,
            last_reset_time: 0,
            msg_rx_counts: HashMap::new()
        }
    }
    
    pub fn add_healthpoint(&mut self, success: bool) -> () {
        let hp = NeighborHealthPoint {
            success: success,
            time: get_epoch_time_secs()
        };
        self.healthpoints.push_back(hp);
        while self.healthpoints.len() > NUM_HEALTH_POINTS {
            self.healthpoints.pop_front();
        }
    }

    /// Get a peer's perceived health -- the last $NUM_HEALTH_POINTS successful messages divided by
    /// the total.
    pub fn get_health_score(&self) -> f64 {
        // if we don't have enough data, assume 50%
        if self.healthpoints.len() < NUM_HEALTH_POINTS {
            return 0.5;
        }
        
        let mut successful = 0;
        let mut total = 0;
        let now = get_epoch_time_secs();
        for hp in self.healthpoints.iter() {
            // penalize stale data points -- only look at recent data
            if hp.success && now < hp.time + HEALTH_POINT_LIFETIME {
                successful += 1;
            }
            total += 1;
        }
        (successful as f64) / (total as f64)
    }
}

/// P2P ongoing conversation with another Stacks peer
pub struct Conversation {
    pub connection: ConnectionP2P,
    pub conn_id: usize,

    pub burnchain: Burnchain,                   // copy of our burnchain config
    pub seq: u32,                               // our sequence number when talknig to this peer
    pub heartbeat: u32,                         // how often do we send heartbeats?

    pub peer_network_id: u32,
    pub peer_version: u32,
    pub peer_services: u16,
    pub peer_addrbytes: PeerAddress,
    pub peer_port: u16,
    pub peer_heartbeat: u32,                    // how often do we need to ping the remote peer?
    pub peer_expire_block_height: u64,          // when does the peer's key expire?

    pub data_url: UrlString,                   // where does this peer's data live?

    // highest block height and consensus hash this peer has seen
    pub burnchain_tip_height: u64,
    pub burnchain_tip_consensus_hash: ConsensusHash,

    pub stats: NeighborStats
}

impl fmt::Display for Conversation {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "convo:id={},outbound={},peer={:?}", self.conn_id, self.stats.outbound, &self.to_neighbor_key())
    }
}

impl fmt::Debug for Conversation {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "convo:id={},outbound={},peer={:?}", self.conn_id, self.stats.outbound, &self.to_neighbor_key())
    }
}

impl NeighborKey {
    pub fn from_handshake(peer_version: u32, network_id: u32, handshake_data: &HandshakeData) -> NeighborKey {
        NeighborKey {
            peer_version: peer_version, 
            network_id: network_id,
            addrbytes: handshake_data.addrbytes.clone(),
            port: handshake_data.port,
        }
    }

    pub fn from_socketaddr(peer_version: u32, network_id: u32, addr: &SocketAddr) -> NeighborKey {
        NeighborKey {
            peer_version: peer_version,
            network_id: network_id,
            addrbytes: PeerAddress::from_socketaddr(addr),
            port: addr.port(),
        }
    }
}

impl Neighbor {
    /// Update fields in this neighbor from a given handshake.
    /// Also, re-calculate the peer's ASN and organization ID
    pub fn handshake_update(&mut self, conn: &DBConn, handshake_data: &HandshakeData) -> Result<(), net_error> {
        let pubk = handshake_data.node_public_key.to_public_key()?;
        let asn_opt = PeerDB::asn_lookup(conn, &handshake_data.addrbytes)
            .map_err(|_e| net_error::DBError)?;

        let asn = match asn_opt {
            Some(a) => a,
            None => 0
        };

        self.public_key = pubk;
        self.expire_block = handshake_data.expire_block_height;
        self.last_contact_time = get_epoch_time_secs();

        if asn != 0 {
            self.asn = asn;
            self.org = asn;       // TODO; AS number is a place-holder for an organization ID (an organization can own multiple ASs)
        }

        Ok(())
    }

    pub fn from_handshake(conn: &DBConn, peer_version: u32, network_id: u32, handshake_data: &HandshakeData) -> Result<Neighbor, net_error> {
        let addr = NeighborKey::from_handshake(peer_version, network_id, handshake_data);
        let pubk = handshake_data.node_public_key.to_public_key()?;

        let peer_opt = PeerDB::get_peer(conn, network_id, &addr.addrbytes, addr.port)
            .map_err(|_e| net_error::DBError)?;

        let mut neighbor = match peer_opt {
            Some(neighbor) => {
                let mut ret = neighbor.clone();
                ret.addr = addr.clone();
                ret
            },
            None => {
                let ret = Neighbor::empty(&addr, &pubk, handshake_data.expire_block_height);
                ret
            }
        };

        #[cfg(test)]
        {
            // setting BLOCKSTACK_NEIGHBOR_TEST_${PORTNUMBER} will let us select an organization
            // for this peer
            use std::env;
            match env::var(format!("BLOCKSTACK_NEIGHBOR_TEST_{}", addr.port).to_string()) {
                Ok(asn_str) => {
                    neighbor.asn = asn_str.parse().unwrap();
                    neighbor.org = neighbor.asn;
                    test_debug!("Override {:?} to ASN/org {}", &neighbor.addr, neighbor.asn);
                },
                Err(_) => {}
            };
        }

        neighbor.handshake_update(conn, &handshake_data)?;
        Ok(neighbor)
    }

    pub fn from_conversation(conn: &DBConn, convo: &Conversation) -> Result<Option<Neighbor>, net_error> {
        let addr = convo.to_neighbor_key();
        let peer_opt = PeerDB::get_peer(conn, addr.network_id, &addr.addrbytes, addr.port)
            .map_err(|_e| net_error::DBError)?;

        match peer_opt {
            None => {
                Ok(None)
            },
            Some(mut peer) => {
                if peer.asn == 0 {
                    let asn_opt = PeerDB::asn_lookup(conn, &addr.addrbytes)
                        .map_err(|_e| net_error::DBError)?;

                    match asn_opt {
                        Some(a) => {
                            if a != 0 {
                                peer.asn = a;
                            }
                        },
                        None => {}
                    };
                }
                Ok(Some(peer))
            }
        }
    }
}

impl Conversation {
    /// Create an unconnected conversation
    pub fn new(burnchain: &Burnchain, peer_addr: &SocketAddr, conn_opts: &ConnectionOptions, outbound: bool, conn_id: usize) -> Conversation {
        Conversation {
            connection: ConnectionP2P::new(StacksP2P::new(), conn_opts, None),
            conn_id: conn_id,
            seq: 0,
            heartbeat: conn_opts.heartbeat,
            burnchain: burnchain.clone(),

            peer_network_id: 0,
            peer_version: 0,
            peer_addrbytes: PeerAddress::from_socketaddr(peer_addr),
            peer_port: peer_addr.port(),
            peer_heartbeat: 0,
            peer_services: 0,
            peer_expire_block_height: 0,

            data_url: UrlString::try_from("".to_string()).unwrap(),

            burnchain_tip_height: 0,
            burnchain_tip_consensus_hash: ConsensusHash([0x00; 20]),

            stats: NeighborStats::new(outbound)
        }
    }

    /// Create a conversation from an existing conversation whose underlying network connection had to be
    /// reset.
    pub fn from_peer_reset(convo: &Conversation, conn_opts: &ConnectionOptions) -> Conversation {
        let stats = convo.stats.clone();
        Conversation {
            connection: ConnectionP2P::new(StacksP2P::new(), conn_opts, None),
            conn_id: convo.conn_id,
            seq: 0,
            heartbeat: conn_opts.heartbeat,
            burnchain: convo.burnchain.clone(),

            peer_network_id: convo.peer_network_id,
            peer_version: convo.peer_version,
            peer_addrbytes: convo.peer_addrbytes.clone(),
            peer_port: convo.peer_port,
            peer_heartbeat: convo.peer_heartbeat,
            peer_services: convo.peer_services,
            peer_expire_block_height: convo.peer_expire_block_height,

            data_url: convo.data_url.clone(),

            burnchain_tip_height: convo.burnchain_tip_height,
            burnchain_tip_consensus_hash: convo.burnchain_tip_consensus_hash.clone(),
            
            stats: NeighborStats {
                peer_resets: convo.stats.peer_resets + 1,
                last_reset_time: get_epoch_time_secs(),
                ..stats
            }
        }
    }

    pub fn set_public_key(&mut self, pubkey_opt: Option<Secp256k1PublicKey>) -> () {
        self.connection.set_public_key(pubkey_opt);
    }

    pub fn to_neighbor_key(&self) -> NeighborKey {
        NeighborKey {
            peer_version: self.peer_version,
            network_id: self.peer_network_id,
            addrbytes: self.peer_addrbytes.clone(),
            port: self.peer_port
        }
    }
    
    /// Determine whether or not a given (height, consensus_hash) pair _disagrees_ with our
    /// burnchain view.  If it does, return true.  If it doesn't (including if the given pair is
    /// simply absent from the chain_view), then return False.
    fn check_consensus_hash_disagreement(block_height: u64, their_consensus_hash: &ConsensusHash, chain_view: &BurnchainView) -> bool {
        let ch = match chain_view.last_consensus_hashes.get(&block_height) {
            Some(ref ch) => {
                ch.clone()
            },
            None => {
                // not present; can't prove disagreement
                return false;
            }
        };
        *ch != *their_consensus_hash
    }

    /// Validate an inbound message's preamble against our knowledge of the burn chain.
    /// Return Ok(true) if we can proceed
    /// Return Ok(false) if we can't proceed, but the remote peer is not in violation of the protocol 
    /// Return Err(net_error::InvalidMessage) if the remote peer returns an invalid message in
    ///     violation of the protocol
    pub fn is_preamble_valid(&self, msg: &StacksMessage, chain_view: &BurnchainView) -> Result<bool, net_error> {
        if msg.preamble.network_id != self.burnchain.network_id {
            // not on our network -- potentially blacklist this peer
            test_debug!("wrong network ID: {:x} != {:x}", msg.preamble.network_id, self.burnchain.network_id);
            return Err(net_error::InvalidMessage);
        }
        if (msg.preamble.peer_version & 0xff000000) != (self.burnchain.peer_version & 0xff000000) {
            // major version mismatch -- potentially blacklist this peer
            test_debug!("wrong peer version: {:x} != {:x}", msg.preamble.peer_version, self.burnchain.peer_version);
            return Err(net_error::InvalidMessage);
        }
        if msg.preamble.burn_stable_block_height.checked_add(self.burnchain.stable_confirmations as u64) != Some(msg.preamble.burn_block_height) {
            // invalid message -- potentially blacklist this peer
            test_debug!("wrong stable block height: {:?} != {}", msg.preamble.burn_stable_block_height.checked_add(self.burnchain.stable_confirmations as u64), msg.preamble.burn_block_height);
            return Err(net_error::InvalidMessage);
        }

        if msg.preamble.burn_stable_block_height > chain_view.burn_block_height + MAX_NEIGHBOR_BLOCK_DELAY {
            // this node is too far ahead of us, but otherwise still potentially valid 
            test_debug!("remote peer is too far ahead of us: {} > {}", msg.preamble.burn_stable_block_height, chain_view.burn_block_height);
            return Ok(false);
        }
        else {
            // remote node's unstable burn block height is at or behind ours.
            // if their view is sufficiently fresh, make sure their consensus hash matches our view.
            let res = Conversation::check_consensus_hash_disagreement(msg.preamble.burn_block_height, &msg.preamble.burn_consensus_hash, chain_view);
            if res {
                // our chain tip disagrees with their chain tip -- don't engage
                return Ok(false);
            }
        }

        // must agree on stable consensus hash
        let rules_disagree = Conversation::check_consensus_hash_disagreement(msg.preamble.burn_stable_block_height, &msg.preamble.burn_stable_consensus_hash, chain_view);
        if rules_disagree {
            // remote peer disagrees on stable consensus hash -- follows different rules than us
            test_debug!("Consensus hash mismatch in preamble");
            return Err(net_error::InvalidMessage);
        }

        Ok(true)
    }

    /// Get next message sequence number, and increment.
    fn next_seq(&mut self) -> u32 {
        if self.seq == u32::max_value() {
            self.seq = 0;
        }
        let ret = self.seq;
        self.seq += 1;
        ret
    }

    /// Generate a signed message for this conversation 
    pub fn sign_message(&mut self, chain_view: &BurnchainView, private_key: &Secp256k1PrivateKey, payload: StacksMessageType) -> Result<StacksMessage, net_error> {
        let mut msg = StacksMessage::from_chain_view(self.burnchain.peer_version, self.burnchain.network_id, chain_view, payload);
        msg.sign(self.next_seq(), private_key)?;
        Ok(msg)
    }
    
    /// Generate a signed reply for this conversation 
    pub fn sign_reply(&mut self, chain_view: &BurnchainView, private_key: &Secp256k1PrivateKey, payload: StacksMessageType, seq: u32) -> Result<StacksMessage, net_error> {
        let mut msg = StacksMessage::from_chain_view(self.burnchain.peer_version, self.burnchain.network_id, chain_view, payload);
        msg.sign(seq, private_key)?;
        Ok(msg)
    }

    /// Queue up this message to this peer, and update our stats.
    /// This is a non-blocking operation. The caller needs to call .try_flush() or .flush() on the
    /// returned Write to finish sending.
    pub fn relay_signed_message(&mut self, msg: StacksMessage) -> Result<ReplyHandleP2P, net_error> {
        let mut handle = self.connection.make_relay_handle()?;
        msg.consensus_serialize(&mut handle)?;

        self.stats.msgs_tx += 1;
        Ok(handle)
    }
    
    /// Queue up this message to this peer, and update our stats.  Expect a reply.
    /// This is a non-blocking operation.  The caller needs to call .try_flush() or .flush() on the
    /// returned handle to finish sending.
    pub fn send_signed_request(&mut self, msg: StacksMessage, ttl: u64) -> Result<ReplyHandleP2P, net_error> {
        let mut handle = self.connection.make_request_handle(msg.request_id(), ttl)?;
        msg.consensus_serialize(&mut handle)?;

        self.stats.msgs_tx += 1;
        Ok(handle)
    }

    /// Reply to a ping with a pong.
    /// Called from the p2p network thread.
    pub fn handle_ping(&mut self, chain_view: &BurnchainView, message: &mut StacksMessage) -> Result<Option<StacksMessage>, net_error> {
        let ping_data = match message.payload {
            StacksMessageType::Ping(ref data) => data,
            _ => panic!("Message is not a ping")
        };
        let pong_data = PongData::from_ping(&ping_data);
        Ok(Some(StacksMessage::from_chain_view(self.burnchain.peer_version, self.burnchain.network_id, chain_view, StacksMessageType::Pong(pong_data))))
    }

    /// Validate a handshake request.
    /// Return Err(...) if the handshake request was invalid.
    pub fn validate_handshake(&mut self, local_peer: &LocalPeer, chain_view: &BurnchainView, message: &mut StacksMessage) -> Result<(), net_error> {
        let handshake_data = match message.payload {
            StacksMessageType::Handshake(ref mut data) => data.clone(),
            _ => panic!("Message is not a handshake")
        };

        match self.connection.get_public_key() {
            None => {
                // if we don't yet have a public key for this node, verify the message.
                // if it's improperly signed, it's probably a poorly-timed re-key request (but either way the message should be rejected)
                message.verify_secp256k1(&handshake_data.node_public_key)
                    .map_err(|_e| {
                        test_debug!("{:?}: invalid handshake: not signed with given public key", &self);
                        net_error::InvalidMessage
                    })?;
            },
            Some(_) => {
                // for outbound connections, the self-reported address must match socket address if we already have a public key.
                // (not the case for inbound connections, since the peer socket address we see may
                // not be the same as the address the remote peer thinks it has).
                if self.stats.outbound && (self.peer_addrbytes != handshake_data.addrbytes || self.peer_port != handshake_data.port) {
                    // wrong peer address
                    test_debug!("{:?}: invalid handshake -- wrong addr/port ({:?}:{:?})", &self, &handshake_data.addrbytes, handshake_data.port);
                    return Err(net_error::InvalidHandshake);
                }
            }
        };

        let their_public_key_res = handshake_data.node_public_key.to_public_key();
        match their_public_key_res {
            Ok(_) => {},
            Err(_e) => {
                // bad public key
                test_debug!("{:?}: invalid handshake -- invalid public key", &self);
                return Err(net_error::InvalidMessage);
            }
        };

        if handshake_data.expire_block_height <= chain_view.burn_block_height {
            // already stale
            test_debug!("{:?}: invalid handshake -- stale public key (expired at {})", &self, handshake_data.expire_block_height);
            return Err(net_error::InvalidHandshake);
        }

        // the handshake cannot come from us 
        if handshake_data.node_public_key == StacksPublicKeyBuffer::from_public_key(&Secp256k1PublicKey::from_private(&local_peer.private_key)) {
            test_debug!("{:?}: invalid handshake -- got a handshake from myself", &self);
            return Err(net_error::InvalidHandshake);
        }

        Ok(())
    }

    /// Update connection state from handshake data
    pub fn update_from_handshake_data(&mut self, preamble: &Preamble, handshake_data: &HandshakeData) -> Result<(), net_error> {
        let pubk = handshake_data.node_public_key.to_public_key()?;

        self.peer_version = preamble.peer_version;
        self.peer_network_id = preamble.network_id;
        self.peer_services = handshake_data.services;
        self.peer_expire_block_height = handshake_data.expire_block_height;
        self.data_url = handshake_data.data_url.clone();

        let cur_pubk_opt = self.connection.get_public_key();
        if let Some(cur_pubk) = cur_pubk_opt {
            if pubk != cur_pubk {
                test_debug!("{:?}: Upgrade key {:?} to {:?} expires {:?}", &self, &to_hex(&cur_pubk.to_bytes_compressed()), &to_hex(&pubk.to_bytes_compressed()), self.peer_expire_block_height);
            }
        }
        
        self.connection.set_public_key(Some(pubk.clone()));

        Ok(())
    }

    /// Handle a handshake request, and generate either a HandshakeAccept or a HandshakeReject
    /// payload to send back.
    /// A handshake will only be accepted if we do not yet know the public key of this remote peer,
    /// or if it is signed by the current public key.
    /// Called from the p2p network thread.
    /// Panics if this message is not a handshake (caller should check)
    pub fn handle_handshake(&mut self, local_peer: &LocalPeer, chain_view: &BurnchainView, message: &mut StacksMessage) -> Result<Option<StacksMessage>, net_error> {
        let res = self.validate_handshake(local_peer, chain_view, message);
        match res {
            Ok(_) => {},
            Err(net_error::InvalidHandshake) => {
                let reject = StacksMessage::from_chain_view(self.burnchain.peer_version, self.burnchain.network_id, chain_view, StacksMessageType::HandshakeReject);
                debug!("{:?}: invalid handshake", &self);
                return Ok(Some(reject));
            },
            Err(e) => {
                return Err(e);
            }
        };
        
        let handshake_data = match message.payload {
            StacksMessageType::Handshake(ref mut data) => data.clone(),
            _ => panic!("Message is not a handshake")
        };

        let old_pubkey_opt = self.connection.get_public_key();
        self.update_from_handshake_data(&message.preamble, &handshake_data)?;
       
        let new_pubkey_opt = self.connection.get_public_key();

        let _authentic_msg = if old_pubkey_opt == new_pubkey_opt { "same" } else if old_pubkey_opt.is_none() { "new" } else { "upgraded" };

        test_debug!("Handshake from {:?} {} public key {:?} expires at {:?}", &self, _authentic_msg,
                    &to_hex(&handshake_data.node_public_key.to_public_key().unwrap().to_bytes_compressed()), handshake_data.expire_block_height);

        let accept_data = HandshakeAcceptData::new(local_peer, self.heartbeat);
        let accept = StacksMessage::from_chain_view(self.burnchain.peer_version, self.burnchain.network_id, chain_view, StacksMessageType::HandshakeAccept(accept_data));
        Ok(Some(accept))
    }

    /// Update conversation state based on a HandshakeAccept
    /// Called from the p2p network thread.
    pub fn handle_handshake_accept(&mut self, preamble: &Preamble, handshake_accept: &HandshakeAcceptData) -> Result<Option<StacksMessage>, net_error> {
        self.update_from_handshake_data(preamble, &handshake_accept.handshake)?;
        self.peer_heartbeat = handshake_accept.heartbeat_interval;
        self.stats.last_handshake_time = get_epoch_time_secs();

        test_debug!("HandshakeAccept from {:?}: set public key to {:?} expiring at {:?} heartbeat {}s", &self,
                    &to_hex(&handshake_accept.handshake.node_public_key.to_public_key().unwrap().to_bytes_compressed()), handshake_accept.handshake.expire_block_height, self.peer_heartbeat);
        Ok(None)
    }

    /// Load data into our connection 
    pub fn recv<R: Read>(&mut self, r: &mut R) -> Result<usize, net_error> {
        let res = self.connection.recv_data(r);
        match res {
            Ok(num_recved) => {
                self.stats.last_recv_time = get_epoch_time_secs();
                self.stats.bytes_rx += num_recved as u64;
            },
            Err(_) => {}
        };
        res
    }

    /// Write data out of our conversation 
    pub fn send<W: Write>(&mut self, w: &mut W) -> Result<usize, net_error> {
        let res = self.connection.send_data(w);
        match res {
            Ok(num_sent) => {
                self.stats.last_send_time = get_epoch_time_secs();
                self.stats.bytes_tx += num_sent as u64;
            },
            Err(_) => {}
        };
        res
    }

    /// Carry on a conversation with the remote peer.
    /// Called from the p2p network thread, so no need for a network handle.
    /// Attempts to fulfill requests in other threads as a result of processing a message.
    /// Returns the list of unfulfilled Stacks messages we received -- messages not destined for
    /// any other thread in this program (i.e. "unsolicited messages"), but originating from this
    /// peer.
    /// Also returns reply handles for each message sent.  The caller will need to call
    /// .try_flush() or .flush() on the handles to make sure their data is sent along.
    /// If the peer violates the protocol, returns net_error::InvalidMessage. The caller should
    /// cease talking to this peer.
    pub fn chat(&mut self, local_peer: &LocalPeer, burnchain_view: &BurnchainView) -> Result<(Vec<StacksMessage>, Vec<ReplyHandleP2P>), net_error> {
        let num_inbound = self.connection.inbox_len();
        test_debug!("{:?}: {} messages pending", &self, num_inbound);

        let mut unsolicited = vec![];
        let mut responses = vec![];
        for _ in 0..num_inbound {
            let mut solicited = true;
            let mut consume_unsolicited = false;

            let mut msg = match self.connection.next_inbox_message() {
                None => {
                    continue;
                },
                Some(m) => m
            };

            // validate message preamble
            match self.is_preamble_valid(&msg, burnchain_view) {
                Ok(res) => {
                    if !res {
                        info!("{:?}: Received message with stale preamble; ignoring", &self);
                        self.stats.msgs_err += 1;
                        self.stats.add_healthpoint(false);
                        continue;
                    }
                },
                Err(e) => {
                    match e {
                        net_error::InvalidMessage => {
                            // Disconnect from this peer.  If it thinks nothing's wrong, it'll
                            // reconnect on its own.
                            // However, only count this message as error.  Drop all other queued
                            // messages.
                            info!("{:?}: Received invalid preamble; dropping connection", &self);
                            self.stats.msgs_err += 1;
                            self.stats.add_healthpoint(false);
                            return Err(e);
                        },
                        _ => {
                            // skip this message 
                            info!("{:?}: Failed to process message: {:?}", &self, &e);
                            self.stats.msgs_err += 1;
                            self.stats.add_healthpoint(false);
                            continue;
                        }
                    }
                }
            };
            
            let reply_opt_res = 
                if self.connection.has_public_key() {
                    // already have public key; match payload
                    match msg.payload {
                        StacksMessageType::Handshake(_) => {
                            test_debug!("{:?}: Got Handshake", &self);

                            let cur_public_key_opt = self.connection.get_public_key();
                            let handshake_res = self.handle_handshake(local_peer, burnchain_view, &mut msg);
                            if handshake_res.is_ok() {
                                // did we re-key?
                                consume_unsolicited = match (cur_public_key_opt, self.connection.get_public_key()) {
                                    (Some(old_public_key), Some(new_public_key)) => {
                                        if old_public_key.to_bytes_compressed() != new_public_key.to_bytes_compressed() {
                                            // remote peer re-keyed. 
                                            // pass along this message to the peer network, even if unsolicited, so we can
                                            // store the new key data.
                                            false       // do not consume
                                        }
                                        else {
                                            // no need to forward along if not solicited, since we
                                            // learned nothing new.
                                            true        // consume if unsolicited
                                        }
                                    },
                                    (None, Some(_)) => {
                                        // learned the initial key, so forward back if not solicited 
                                        false
                                    },
                                    (_, _) => false     // not a re-key -- do not consume if not solicited
                                }
                            }
                            handshake_res
                        },
                        StacksMessageType::HandshakeAccept(ref data) => {
                            test_debug!("{:?}: Got HandshakeAccept", &self);
                            self.handle_handshake_accept(&msg.preamble, data)
                        },
                        StacksMessageType::Ping(_) => {
                            test_debug!("{:?}: Got Ping", &self);

                            // consume here if unsolicited
                            consume_unsolicited = true;
                            self.handle_ping(burnchain_view, &mut msg)
                        },
                        StacksMessageType::Pong(_) => {
                            test_debug!("{:?}: Got Pong", &self);

                            // consume here if unsolicited
                            consume_unsolicited = true;
                            Ok(None)
                        },
                        _ => {
                            test_debug!("{:?}: Got a message (type {})", &self, msg.payload.get_message_name());
                            Ok(None)       // nothing to reply to at this time
                        }
                    }
                }
                else {
                    // don't count unauthenticated messages we didn't ask for
                    solicited = self.connection.is_solicited(&msg);

                    // only thing we'll take right now is a handshake, as well as handshake
                    // accept/rejects and nacks.
                    //
                    // Anything else will be nack'ed -- the peer will first need to handshake.
                    match msg.payload {
                        StacksMessageType::Handshake(_) => {
                            test_debug!("{:?}: Got unauthenticated Handshake", &self);
                            self.handle_handshake(local_peer, burnchain_view, &mut msg)
                        },
                        StacksMessageType::HandshakeAccept(ref data) => {
                            if solicited {
                                test_debug!("{:?}: Got unauthenticated HandshakeAccept", &self);
                                self.handle_handshake_accept(&msg.preamble, data)
                            }
                            else {
                                test_debug!("{:?}: Unsolicited unauthenticated HandshakeAccept", &self);

                                // don't update state and don't pass back 
                                consume_unsolicited = true;
                                Ok(None)
                            }
                        },
                        StacksMessageType::HandshakeReject => {
                            test_debug!("{:?}: Got unauthenticated HandshakeReject", &self);

                            // don't NACK this back just because we were rejected
                            Ok(None)
                        },
                        StacksMessageType::Nack(_) => {
                            test_debug!("{:?}: Got unauthenticated Nack", &self);
                            
                            // don't NACK back
                            Ok(None)
                        }
                        _ => {
                            test_debug!("{:?}: Got unauthenticated message (type {})", &self, msg.payload.get_message_name());
                            let nack_payload = StacksMessageType::Nack(NackData::new(NackErrorCodes::HandshakeRequired));
                            let nack = StacksMessage::from_chain_view(self.burnchain.peer_version, self.burnchain.network_id, burnchain_view, nack_payload);

                            // unauthenticated, so don't forward 
                            consume_unsolicited = true;
                            Ok(Some(nack))
                        }
                    }
                };

            let now = get_epoch_time_secs();
            let reply_opt = reply_opt_res?;
            match reply_opt {
                None => {}
                Some(mut reply) => {
                    // send back this message to the remote peer
                    test_debug!("{:?}: Send automatic reply type {}", &self, reply.payload.get_message_name());
                    reply.sign(msg.preamble.seq, &local_peer.private_key)?;
                    let reply_handle = self.relay_signed_message(reply)?;
                    responses.push(reply_handle);
                }
            }

            if solicited {
                // successfully got a message -- update stats
                if self.stats.first_contact_time == 0 {
                    self.stats.first_contact_time = now;
                }

                let msg_id = msg.payload.get_message_id();
                let count = match self.stats.msg_rx_counts.get(&msg_id) {
                    None => 1,
                    Some(c) => c + 1
                };
                self.stats.msg_rx_counts.insert(msg_id, count);

                self.stats.msgs_rx += 1;
                self.stats.last_recv_time = now;
                self.stats.last_contact_time = get_epoch_time_secs();
                self.stats.add_healthpoint(true);
            }
            else {
                // got an unauthenticated message we didn't ask for
                self.stats.msgs_rx_unsolicited += 1;
            }

            let _msgtype = msg.payload.get_message_name().to_owned();

            // Is there someone else waiting for this message?  If so, pass it along.
            let fulfill_opt = self.connection.fulfill_request(msg);
            match fulfill_opt {
                None => {
                    test_debug!("{:?}: Fulfilled pending message request (type {})", &self, _msgtype);
                },
                Some(m) => {
                    if consume_unsolicited {
                        test_debug!("{:?}: Consuming unsolicited message (type {})", &self, _msgtype);
                    }
                    else {
                        test_debug!("{:?}: Forwarding along unsolicited message (type {})", &self, _msgtype);
                        unsolicited.push(m);
                    }
                }
            };
        }

        Ok((unsolicited, responses))
    }

    /// Remove all timed-out messages, and ding the remote peer as unhealthy
    pub fn clear_timeouts(&mut self) -> () {
       let num_drained = self.connection.drain_timeouts();
       for _ in 0..num_drained {
           self.stats.add_healthpoint(false);
       }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use net::*;
    use net::connection::*;
    use net::db::*;
    use net::p2p::*;
    use util::secp256k1::*;
    use util::uint::*;
    use util::pipe::*;
    use burnchains::*;
    use burnchains::burnchain::*;
    use chainstate::*;
    use chainstate::burn::*;
    use chainstate::burn::db::burndb::*;

    use burnchains::bitcoin::address::BitcoinAddress;
    use burnchains::bitcoin::keys::BitcoinPublicKey;

    use std::net::SocketAddr;
    use std::net::SocketAddrV4;

    use std::io::prelude::*;
    use std::io::Read;
    use std::io::Write;

    use net::test::*;

    use core::PEER_VERSION;

    fn convo_send_recv(sender: &mut Conversation, mut sender_handles: Vec<&mut ReplyHandleP2P>, receiver: &mut Conversation, mut relay_handles: Vec<ReplyHandleP2P>) -> () {
        let (mut pipe_read, mut pipe_write) = Pipe::new();
        pipe_read.set_nonblocking(true);

        loop {
            let mut res = true;
            for i in 0..sender_handles.len() {
                let r = sender_handles[i].try_flush().unwrap();
                res = r && res;
            }
            
            let mut all_relays_flushed = true;
            for h in relay_handles.iter_mut() {
                let f = h.try_flush().unwrap();
                all_relays_flushed = f && all_relays_flushed;
            }
            
            let nw = sender.send(&mut pipe_write).unwrap();
            let nr = receiver.recv(&mut pipe_read).unwrap();

            test_debug!("res = {}, all_relays_flushed = {}, nr = {}, nw = {}", res, all_relays_flushed, nr, nw);
            if res && all_relays_flushed && nr == 0 && nw == 0 {
                break;
            }
        }
    }

    fn db_setup(peerdb: &mut PeerDB, burndb: &mut BurnDB, socketaddr: &SocketAddr, chain_view: &BurnchainView) -> () {
        {
            let mut tx = peerdb.tx_begin().unwrap();
            PeerDB::set_local_ipaddr(&mut tx, &PeerAddress::from_socketaddr(socketaddr), socketaddr.port()).unwrap();
            tx.commit().unwrap();
        }
        let mut tx = burndb.tx_begin().unwrap();
        let mut prev_snapshot = BurnDB::get_first_block_snapshot(&tx).unwrap();
        for i in prev_snapshot.block_height..chain_view.burn_block_height+1 {
            let mut next_snapshot = prev_snapshot.clone();

            next_snapshot.block_height += 1;
            if i > chain_view.burn_stable_block_height {
                next_snapshot.consensus_hash = chain_view.burn_consensus_hash.clone();
            }
            else {
                next_snapshot.consensus_hash = chain_view.burn_stable_consensus_hash.clone();
            }

            let big_i = Uint256::from_u64(i as u64);
            let mut big_i_bytes_32 = [0u8; 32];
            big_i_bytes_32.copy_from_slice(&big_i.to_u8_slice());

            next_snapshot.parent_burn_header_hash = next_snapshot.burn_header_hash.clone();
            next_snapshot.burn_header_hash = BurnchainHeaderHash(big_i_bytes_32.clone());
            next_snapshot.ops_hash = OpsHash::from_bytes(&big_i_bytes_32).unwrap();
            next_snapshot.total_burn += 1;
            next_snapshot.sortition = true;
            next_snapshot.sortition_hash = next_snapshot.sortition_hash.mix_burn_header(&BurnchainHeaderHash(big_i_bytes_32.clone()));
            next_snapshot.num_sortitions += 1;

            let next_index_root = BurnDB::append_chain_tip_snapshot(&mut tx, &prev_snapshot, &next_snapshot, &vec![], &vec![]).unwrap();
            next_snapshot.index_root = next_index_root;

            test_debug!("i = {}, chain_view.burn_block_height = {}, ch = {}", i, chain_view.burn_block_height, next_snapshot.consensus_hash);
            
            prev_snapshot = next_snapshot;
        }
        tx.commit().unwrap();
    }

    #[test]
    fn convo_handshake_accept() {
        let conn_opts = ConnectionOptions::default();

        let socketaddr_1 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let socketaddr_2 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)), 8081);
        
        let first_burn_hash = BurnchainHeaderHash::from_hex("0000000000000000000000000000000000000000000000000000000000000000").unwrap();
        
        let burnchain = Burnchain {
            peer_version: PEER_VERSION,
            network_id: 0x9abcdef0,
            chain_name: "bitcoin".to_string(),
            network_name: "testnet".to_string(),
            working_dir: "/nope".to_string(),
            consensus_hash_lifetime: 24,
            stable_confirmations: 7,
            first_block_height: 12300,
            first_block_hash: first_burn_hash.clone(),
        };

        let mut chain_view = BurnchainView {
            burn_block_height: 12348,
            burn_consensus_hash: ConsensusHash::from_hex("1111111111111111111111111111111111111111").unwrap(),
            burn_stable_block_height: 12341,
            burn_stable_consensus_hash: ConsensusHash::from_hex("2222222222222222222222222222222222222222").unwrap(),
            last_consensus_hashes: HashMap::new()
        };
        chain_view.make_test_data();

        let mut peerdb_1 = PeerDB::connect_memory(0x9abcdef0, 12350, "http://peer1.com".into(), &vec![], &vec![]).unwrap();
        let mut peerdb_2 = PeerDB::connect_memory(0x9abcdef0, 12351, "http://peer2.com".into(), &vec![], &vec![]).unwrap();
        
        let mut burndb_1 = BurnDB::connect_memory(12300, &first_burn_hash).unwrap();
        let mut burndb_2 = BurnDB::connect_memory(12300, &first_burn_hash).unwrap();

        db_setup(&mut peerdb_1, &mut burndb_1, &socketaddr_1, &chain_view);
        db_setup(&mut peerdb_2, &mut burndb_2, &socketaddr_2, &chain_view);

        let local_peer_1 = PeerDB::get_local_peer(&peerdb_1.conn()).unwrap();
        let local_peer_2 = PeerDB::get_local_peer(&peerdb_2.conn()).unwrap();

        let mut convo_1 = Conversation::new(&burnchain, &socketaddr_2, &conn_opts, true, 0);
        let mut convo_2 = Conversation::new(&burnchain, &socketaddr_1, &conn_opts, true, 0);
       
        // no peer public keys known yet
        assert!(convo_1.connection.get_public_key().is_none());
        assert!(convo_2.connection.get_public_key().is_none());
        
        // convo_1 sends a handshake to convo_2
        let handshake_data_1 = HandshakeData::from_local_peer(&local_peer_1);
        let handshake_1 = convo_1.sign_message(&chain_view, &local_peer_1.private_key, StacksMessageType::Handshake(handshake_data_1.clone())).unwrap();
        let mut rh_1 = convo_1.send_signed_request(handshake_1, 1000000).unwrap();

        // convo_2 receives it and processes it, and since no one is waiting for it, will forward
        // it along to the chat caller (us)
        test_debug!("send handshake");
        convo_send_recv(&mut convo_1, vec![&mut rh_1], &mut convo_2, vec![]);
        let (unhandled_2, handles_2) = convo_2.chat(&local_peer_2, &chain_view).unwrap();

        // convo_1 has a handshakeaccept 
        test_debug!("send handshake-accept");
        convo_send_recv(&mut convo_2, vec![&mut rh_1], &mut convo_1, handles_2);
        let (unhandled_1, handles_1) = convo_1.chat(&local_peer_1, &chain_view).unwrap();

        let reply_1 = rh_1.recv(0).unwrap();

        assert_eq!(unhandled_1.len(), 0);
        assert_eq!(unhandled_2.len(), 1);

        // convo 2 returns the handshake from convo 1
        match unhandled_2[0].payload {
            StacksMessageType::Handshake(ref data) => {
                assert_eq!(handshake_data_1, *data);
            },
            _ => {
                assert!(false);
            }
        };

        // received a valid HandshakeAccept from peer 2 
        match reply_1.payload {
            StacksMessageType::HandshakeAccept(ref data) => {
                assert_eq!(data.handshake.addrbytes, local_peer_2.addrbytes);
                assert_eq!(data.handshake.port, local_peer_2.port);
                assert_eq!(data.handshake.services, local_peer_2.services);
                assert_eq!(data.handshake.node_public_key, StacksPublicKeyBuffer::from_public_key(&Secp256k1PublicKey::from_private(&local_peer_2.private_key)));
                assert_eq!(data.handshake.expire_block_height, local_peer_2.private_key_expire); 
                assert_eq!(data.handshake.data_url, "http://peer2.com".into());
                assert_eq!(data.heartbeat_interval, conn_opts.heartbeat);
            },
            _ => {
                assert!(false);
            }
        };

        // convo_2 got updated with convo_1's peer info, but no heartbeat info 
        assert_eq!(convo_2.peer_heartbeat, 0);
        assert_eq!(convo_2.connection.get_public_key().unwrap(), Secp256k1PublicKey::from_private(&local_peer_1.private_key));
        assert_eq!(convo_2.data_url, "http://peer1.com".into());

        // convo_1 got updated with convo_2's peer info, as well as heartbeat
        assert_eq!(convo_1.peer_heartbeat, conn_opts.heartbeat);
        assert_eq!(convo_1.connection.get_public_key().unwrap(), Secp256k1PublicKey::from_private(&local_peer_2.private_key));
        assert_eq!(convo_1.data_url, "http://peer2.com".into());
    }
    
    #[test]
    fn convo_handshake_reject() {
        let conn_opts = ConnectionOptions::default();
        let socketaddr_1 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let socketaddr_2 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)), 8081);
        
        let first_burn_hash = BurnchainHeaderHash::from_hex("0000000000000000000000000000000000000000000000000000000000000000").unwrap();

        let burnchain = Burnchain {
            peer_version: PEER_VERSION,
            network_id: 0x9abcdef0,
            chain_name: "bitcoin".to_string(),
            network_name: "testnet".to_string(),
            working_dir: "/nope".to_string(),
            consensus_hash_lifetime: 24,
            stable_confirmations: 7,
            first_block_height: 12300,
            first_block_hash: first_burn_hash.clone(),
        };

        let mut chain_view = BurnchainView {
            burn_block_height: 12348,
            burn_consensus_hash: ConsensusHash::from_hex("1111111111111111111111111111111111111111").unwrap(),
            burn_stable_block_height: 12341,
            burn_stable_consensus_hash: ConsensusHash::from_hex("2222222222222222222222222222222222222222").unwrap(),
            last_consensus_hashes: HashMap::new()
        };
        chain_view.make_test_data();
        
        let mut peerdb_1 = PeerDB::connect_memory(0x9abcdef0, 12350, "http://peer1.com".into(), &vec![], &vec![]).unwrap();
        let mut peerdb_2 = PeerDB::connect_memory(0x9abcdef0, 12351, "http://peer2.com".into(), &vec![], &vec![]).unwrap();
        
        let mut burndb_1 = BurnDB::connect_memory(12300, &first_burn_hash).unwrap();
        let mut burndb_2 = BurnDB::connect_memory(12300, &first_burn_hash).unwrap();

        db_setup(&mut peerdb_1, &mut burndb_1, &socketaddr_1, &chain_view);
        db_setup(&mut peerdb_2, &mut burndb_2, &socketaddr_2, &chain_view);

        let local_peer_1 = PeerDB::get_local_peer(&peerdb_1.conn()).unwrap();
        let local_peer_2 = PeerDB::get_local_peer(&peerdb_2.conn()).unwrap();

        let mut convo_1 = Conversation::new(&burnchain, &socketaddr_2, &conn_opts, true, 0);
        let mut convo_2 = Conversation::new(&burnchain, &socketaddr_1, &conn_opts, true, 0);
       
        // no peer public keys known yet
        assert!(convo_1.connection.get_public_key().is_none());
        assert!(convo_2.connection.get_public_key().is_none());
        
        // convo_1 sends a _stale_ handshake to convo_2 (wrong public key)
        let mut handshake_data_1 = HandshakeData::from_local_peer(&local_peer_1);
        handshake_data_1.expire_block_height = 12340;
        let handshake_1 = convo_1.sign_message(&chain_view, &local_peer_1.private_key, StacksMessageType::Handshake(handshake_data_1.clone())).unwrap();

        let mut rh_1 = convo_1.send_signed_request(handshake_1, 1000000).unwrap();

        // convo_2 receives it and processes it, and since no one is waiting for it, will forward
        // it along to the chat caller (us)
        convo_send_recv(&mut convo_1, vec![&mut rh_1], &mut convo_2, vec![]);
        let (unhandled_2, handles_2) = convo_2.chat(&local_peer_2, &chain_view).unwrap();

        // convo_1 has a handshakreject
        convo_send_recv(&mut convo_2, vec![&mut rh_1], &mut convo_1, handles_2);
        let (unhandled_1, handles_1) = convo_1.chat(&local_peer_1, &chain_view).unwrap();

        let reply_1 = rh_1.recv(0).unwrap();

        assert_eq!(unhandled_1.len(), 0);
        assert_eq!(unhandled_2.len(), 1);

        // convo 2 returns the handshake from convo 1
        match unhandled_2[0].payload {
            StacksMessageType::Handshake(ref data) => {
                assert_eq!(handshake_data_1, *data);
            },
            _ => {
                assert!(false);
            }
        };

        // received a valid HandshakeReject from peer 2 
        match reply_1.payload {
            StacksMessageType::HandshakeReject => {},
            _ => {
                assert!(false);
            }
        };

        // neither peer updated their info on one another 
        assert!(convo_1.connection.get_public_key().is_none());
        assert!(convo_2.connection.get_public_key().is_none());
    }

    #[test]
    fn convo_handshake_badsignature() {
        let conn_opts = ConnectionOptions::default();
        let socketaddr_1 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let socketaddr_2 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)), 8081);
        
        let first_burn_hash = BurnchainHeaderHash::from_hex("0000000000000000000000000000000000000000000000000000000000000000").unwrap();
        
        let burnchain = Burnchain {
            peer_version: PEER_VERSION,
            network_id: 0x9abcdef0,
            chain_name: "bitcoin".to_string(),
            network_name: "testnet".to_string(),
            working_dir: "/nope".to_string(),
            consensus_hash_lifetime: 24,
            stable_confirmations: 7,
            first_block_height: 12300,
            first_block_hash: first_burn_hash.clone(),
        };

        let mut chain_view = BurnchainView {
            burn_block_height: 12348,
            burn_consensus_hash: ConsensusHash::from_hex("1111111111111111111111111111111111111111").unwrap(),
            burn_stable_block_height: 12341,
            burn_stable_consensus_hash: ConsensusHash::from_hex("2222222222222222222222222222222222222222").unwrap(),
            last_consensus_hashes: HashMap::new()
        };
        chain_view.make_test_data();
        
        let first_burn_hash = BurnchainHeaderHash::from_hex("0000000000000000000000000000000000000000000000000000000000000000").unwrap();

        let mut peerdb_1 = PeerDB::connect_memory(0x9abcdef0, 12350, "http://peer1.com".into(), &vec![], &vec![]).unwrap();
        let mut peerdb_2 = PeerDB::connect_memory(0x9abcdef0, 12351, "http://peer2.com".into(), &vec![], &vec![]).unwrap();
        
        let mut burndb_1 = BurnDB::connect_memory(12300, &first_burn_hash).unwrap();
        let mut burndb_2 = BurnDB::connect_memory(12300, &first_burn_hash).unwrap();

        db_setup(&mut peerdb_1, &mut burndb_1, &socketaddr_1, &chain_view);
        db_setup(&mut peerdb_2, &mut burndb_2, &socketaddr_2, &chain_view);

        let local_peer_1 = PeerDB::get_local_peer(&peerdb_1.conn()).unwrap();
        let local_peer_2 = PeerDB::get_local_peer(&peerdb_2.conn()).unwrap();

        let mut convo_1 = Conversation::new(&burnchain, &socketaddr_2, &conn_opts, true, 0);
        let mut convo_2 = Conversation::new(&burnchain, &socketaddr_1, &conn_opts, true, 0);
       
        // no peer public keys known yet
        assert!(convo_1.connection.get_public_key().is_none());
        assert!(convo_2.connection.get_public_key().is_none());
        
        // convo_1 sends an _invalid_ handshake to convo_2 (bad signature)
        let handshake_data_1 = HandshakeData::from_local_peer(&local_peer_1);
        let mut handshake_1 = convo_1.sign_message(&chain_view, &local_peer_1.private_key, StacksMessageType::Handshake(handshake_data_1.clone())).unwrap();
        match handshake_1.payload {
            StacksMessageType::Handshake(ref mut data) => {
                data.expire_block_height += 1;
            },
            _ => panic!()
        };

        let mut rh_1 = convo_1.send_signed_request(handshake_1, 1000000).unwrap();

        // convo_2 receives it and processes it, and barfs
        convo_send_recv(&mut convo_1, vec![&mut rh_1], &mut convo_2, vec![]);
        let unhandled_2_err = convo_2.chat(&local_peer_2, &chain_view);

        // convo_1 gets a nack and consumes it
        convo_send_recv(&mut convo_2, vec![&mut rh_1], &mut convo_1, vec![]);
        let (unhandled_1, handles_1) = convo_1.chat(&local_peer_1, &chain_view).unwrap();

        // the waiting reply aborts on disconnect
        let reply_1_err = rh_1.recv(0);

        assert_eq!(unhandled_2_err.unwrap_err(), net_error::InvalidMessage);
        assert_eq!(reply_1_err, Err(net_error::ConnectionBroken));

        assert_eq!(unhandled_1.len(), 0);

        // neither peer updated their info on one another 
        assert!(convo_1.connection.get_public_key().is_none());
        assert!(convo_2.connection.get_public_key().is_none());
    }
    
    #[test]
    fn convo_handshake_self() {
        let conn_opts = ConnectionOptions::default();
        let socketaddr_1 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let socketaddr_2 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)), 8081);
        
        let first_burn_hash = BurnchainHeaderHash::from_hex("0000000000000000000000000000000000000000000000000000000000000000").unwrap();
        
        let burnchain = Burnchain {
            peer_version: PEER_VERSION,
            network_id: 0x9abcdef0,
            chain_name: "bitcoin".to_string(),
            network_name: "testnet".to_string(),
            working_dir: "/nope".to_string(),
            consensus_hash_lifetime: 24,
            stable_confirmations: 7,
            first_block_height: 12300,
            first_block_hash: first_burn_hash.clone(),
        };

        let mut chain_view = BurnchainView {
            burn_block_height: 12348,
            burn_consensus_hash: ConsensusHash::from_hex("1111111111111111111111111111111111111111").unwrap(),
            burn_stable_block_height: 12341,
            burn_stable_consensus_hash: ConsensusHash::from_hex("2222222222222222222222222222222222222222").unwrap(),
            last_consensus_hashes: HashMap::new()
        };
        chain_view.make_test_data();
        
        let first_burn_hash = BurnchainHeaderHash::from_hex("0000000000000000000000000000000000000000000000000000000000000000").unwrap();

        let mut peerdb_1 = PeerDB::connect_memory(0x9abcdef0, 12350, "http://peer1.com".into(), &vec![], &vec![]).unwrap();
        let mut peerdb_2 = PeerDB::connect_memory(0x9abcdef0, 12351, "http://peer2.com".into(), &vec![], &vec![]).unwrap();
        
        let mut burndb_1 = BurnDB::connect_memory(12300, &first_burn_hash).unwrap();
        let mut burndb_2 = BurnDB::connect_memory(12300, &first_burn_hash).unwrap();

        db_setup(&mut peerdb_1, &mut burndb_1, &socketaddr_1, &chain_view);
        db_setup(&mut peerdb_2, &mut burndb_2, &socketaddr_2, &chain_view);

        let local_peer_1 = PeerDB::get_local_peer(&peerdb_1.conn()).unwrap();
        let local_peer_2 = PeerDB::get_local_peer(&peerdb_2.conn()).unwrap();

        let mut convo_1 = Conversation::new(&burnchain, &socketaddr_2, &conn_opts, true, 0);
        let mut convo_2 = Conversation::new(&burnchain, &socketaddr_1, &conn_opts, true, 0);
       
        // no peer public keys known yet
        assert!(convo_1.connection.get_public_key().is_none());
        assert!(convo_2.connection.get_public_key().is_none());
       
        // convo_1 sends a handshake to itself (not allowed)
        let handshake_data_1 = HandshakeData::from_local_peer(&local_peer_2);
        let handshake_1 = convo_1.sign_message(&chain_view, &local_peer_2.private_key, StacksMessageType::Handshake(handshake_data_1.clone())).unwrap();
        let mut rh_1 = convo_1.send_signed_request(handshake_1, 1000000).unwrap();

        // convo_2 receives it and processes it, and give back a handshake reject
        convo_send_recv(&mut convo_1, vec![&mut rh_1], &mut convo_2, vec![]);
        let (unhandled_2, handles_2) = convo_2.chat(&local_peer_2, &chain_view).unwrap();

        // convo_1 gets a handshake reject and consumes it
        convo_send_recv(&mut convo_2, vec![&mut rh_1], &mut convo_1, handles_2);
        let (unhandled_1, handles_1) = convo_1.chat(&local_peer_1, &chain_view).unwrap();

        // get back handshake reject
        let reply_1 = rh_1.recv(0).unwrap();

        assert_eq!(unhandled_1.len(), 0);
        assert_eq!(unhandled_2.len(), 1);

        // convo 2 returns the handshake from convo 1
        match unhandled_2[0].payload {
            StacksMessageType::Handshake(ref data) => {
                assert_eq!(handshake_data_1, *data);
            },
            _ => {
                assert!(false);
            }
        };

        // received a valid HandshakeReject from peer 2 
        match reply_1.payload {
            StacksMessageType::HandshakeReject => {},
            _ => {
                assert!(false);
            }
        };

        // neither peer updated their info on one another 
        assert!(convo_1.connection.get_public_key().is_none());
        assert!(convo_2.connection.get_public_key().is_none());
    }

    #[test]
    fn convo_ping() {
        let conn_opts = ConnectionOptions::default();
        let socketaddr_1 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let socketaddr_2 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)), 8081);
        
        let first_burn_hash = BurnchainHeaderHash::from_hex("0000000000000000000000000000000000000000000000000000000000000000").unwrap();

        let burnchain = Burnchain {
            peer_version: PEER_VERSION,
            network_id: 0x9abcdef0,
            chain_name: "bitcoin".to_string(),
            network_name: "testnet".to_string(),
            working_dir: "/nope".to_string(),
            consensus_hash_lifetime: 24,
            stable_confirmations: 7,
            first_block_height: 12300,
            first_block_hash: first_burn_hash.clone(),
        };

        let mut chain_view = BurnchainView {
            burn_block_height: 12348,
            burn_consensus_hash: ConsensusHash::from_hex("1111111111111111111111111111111111111111").unwrap(),
            burn_stable_block_height: 12341,
            burn_stable_consensus_hash: ConsensusHash::from_hex("2222222222222222222222222222222222222222").unwrap(),
            last_consensus_hashes: HashMap::new()
        };
        chain_view.make_test_data();
        
        let first_burn_hash = BurnchainHeaderHash::from_hex("0000000000000000000000000000000000000000000000000000000000000000").unwrap();

        let mut peerdb_1 = PeerDB::connect_memory(0x9abcdef0, 12350, "http://peer1.com".into(), &vec![], &vec![]).unwrap();
        let mut peerdb_2 = PeerDB::connect_memory(0x9abcdef0, 12350, "http://peer2.com".into(), &vec![], &vec![]).unwrap();
        
        let mut burndb_1 = BurnDB::connect_memory(12300, &first_burn_hash).unwrap();
        let mut burndb_2 = BurnDB::connect_memory(12300, &first_burn_hash).unwrap();

        db_setup(&mut peerdb_1, &mut burndb_1, &socketaddr_1, &chain_view);
        db_setup(&mut peerdb_2, &mut burndb_2, &socketaddr_2, &chain_view);

        let local_peer_1 = PeerDB::get_local_peer(&peerdb_1.conn()).unwrap();
        let local_peer_2 = PeerDB::get_local_peer(&peerdb_2.conn()).unwrap();

        let mut convo_1 = Conversation::new(&burnchain, &socketaddr_2, &conn_opts, true, 0);
        let mut convo_2 = Conversation::new(&burnchain, &socketaddr_1, &conn_opts, true, 0);

        // convo_1 sends a handshake to convo_2
        let handshake_data_1 = HandshakeData::from_local_peer(&local_peer_1);
        let handshake_1 = convo_1.sign_message(&chain_view, &local_peer_1.private_key, StacksMessageType::Handshake(handshake_data_1.clone())).unwrap();
        let mut rh_handshake_1 = convo_1.send_signed_request(handshake_1.clone(), 1000000).unwrap();

        // convo_1 sends a ping to convo_2 
        let ping_data_1 = PingData::new();
        let ping_1 = convo_1.sign_message(&chain_view, &local_peer_1.private_key, StacksMessageType::Ping(ping_data_1.clone())).unwrap();
        let mut rh_ping_1 = convo_1.send_signed_request(ping_1.clone(), 1000000).unwrap();

        // convo_2 receives the handshake and ping and processes both, and since no one is waiting for the handshake, will forward
        // it along to the chat caller (us)
        test_debug!("send handshake {:?}", &handshake_1);
        test_debug!("send ping {:?}", &ping_1);
        convo_send_recv(&mut convo_1, vec![&mut rh_handshake_1, &mut rh_ping_1], &mut convo_2, vec![]);
        let (unhandled_2, handles_2) = convo_2.chat(&local_peer_2, &chain_view).unwrap();

        // convo_1 has a handshakeaccept 
        test_debug!("reply handshake-accept");
        test_debug!("send pong");
        convo_send_recv(&mut convo_2, vec![&mut rh_handshake_1, &mut rh_ping_1], &mut convo_1, handles_2);
        let (unhandled_1, handles_1) = convo_1.chat(&local_peer_1, &chain_view).unwrap();

        let reply_handshake_1 = rh_handshake_1.recv(0).unwrap();
        let reply_ping_1 = rh_ping_1.recv(0).unwrap();

        assert_eq!(unhandled_1.len(), 0);
        assert_eq!(unhandled_2.len(), 1);   // only the handshake is given back.  the ping is consumed

        // convo 2 returns the handshake from convo 1
        match unhandled_2[0].payload {
            StacksMessageType::Handshake(ref data) => {
                assert_eq!(handshake_data_1, *data);
            },
            _ => {
                assert!(false);
            }
        };

        // convo 2 replied to convo 1 with a matching pong
        match reply_ping_1.payload {
            StacksMessageType::Pong(ref data) => {
                assert_eq!(data.nonce, ping_data_1.nonce);
            },
            _ => {
                assert!(false);
            }
        }
    }

    #[test]
    fn convo_handshake_ping_loop() {
        let conn_opts = ConnectionOptions::default();
        let socketaddr_1 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let socketaddr_2 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)), 8081);
       
        let first_burn_hash = BurnchainHeaderHash::from_hex("0000000000000000000000000000000000000000000000000000000000000000").unwrap();
        
        let burnchain = Burnchain {
            peer_version: PEER_VERSION,
            network_id: 0x9abcdef0,
            chain_name: "bitcoin".to_string(),
            network_name: "testnet".to_string(),
            working_dir: "/nope".to_string(),
            consensus_hash_lifetime: 24,
            stable_confirmations: 7,
            first_block_height: 12300,
            first_block_hash: first_burn_hash.clone(),
        };

        let mut chain_view = BurnchainView {
            burn_block_height: 12348,
            burn_consensus_hash: ConsensusHash::from_hex("1111111111111111111111111111111111111111").unwrap(),
            burn_stable_block_height: 12341,
            burn_stable_consensus_hash: ConsensusHash::from_hex("2222222222222222222222222222222222222222").unwrap(),
            last_consensus_hashes: HashMap::new()
        };
        chain_view.make_test_data();
        
        let first_burn_hash = BurnchainHeaderHash::from_hex("0000000000000000000000000000000000000000000000000000000000000000").unwrap();

        let mut peerdb_1 = PeerDB::connect_memory(0x9abcdef0, 12350, "http://peer1.com".into(), &vec![], &vec![]).unwrap();
        let mut peerdb_2 = PeerDB::connect_memory(0x9abcdef0, 12350, "http://peer2.com".into(), &vec![], &vec![]).unwrap();
        
        let mut burndb_1 = BurnDB::connect_memory(12300, &first_burn_hash).unwrap();
        let mut burndb_2 = BurnDB::connect_memory(12300, &first_burn_hash).unwrap();

        db_setup(&mut peerdb_1, &mut burndb_1, &socketaddr_1, &chain_view);
        db_setup(&mut peerdb_2, &mut burndb_2, &socketaddr_2, &chain_view);

        let local_peer_1 = PeerDB::get_local_peer(&peerdb_1.conn()).unwrap();
        let local_peer_2 = PeerDB::get_local_peer(&peerdb_2.conn()).unwrap();

        let mut convo_1 = Conversation::new(&burnchain, &socketaddr_2, &conn_opts, true, 0);
        let mut convo_2 = Conversation::new(&burnchain, &socketaddr_1, &conn_opts, true, 0);

        for i in 0..5 {
            // do handshake/ping over and over, with different keys.
            // tests re-keying.

            // convo_1 sends a handshake to convo_2
            let handshake_data_1 = HandshakeData::from_local_peer(&local_peer_1);
            let handshake_1 = convo_1.sign_message(&chain_view, &local_peer_1.private_key, StacksMessageType::Handshake(handshake_data_1.clone())).unwrap();
            let mut rh_handshake_1 = convo_1.send_signed_request(handshake_1, 1000000).unwrap();

            // convo_1 sends a ping to convo_2 
            let ping_data_1 = PingData::new();
            let ping_1 = convo_1.sign_message(&chain_view, &local_peer_1.private_key, StacksMessageType::Ping(ping_data_1.clone())).unwrap();
            let mut rh_ping_1 = convo_1.send_signed_request(ping_1, 1000000).unwrap();

            // convo_2 receives the handshake and ping and processes both, and since no one is waiting for the handshake, will forward
            // it along to the chat caller (us)
            convo_send_recv(&mut convo_1, vec![&mut rh_handshake_1, &mut rh_ping_1], &mut convo_2, vec![]);
            let (unhandled_2, handles_2) = convo_2.chat(&local_peer_2, &chain_view).unwrap();

            // convo_1 has a handshakeaccept 
            convo_send_recv(&mut convo_2, vec![&mut rh_handshake_1, &mut rh_ping_1], &mut convo_1, handles_2);
            let (unhandled_1, handles_1) = convo_1.chat(&local_peer_1, &chain_view).unwrap();

            let reply_handshake_1 = rh_handshake_1.recv(0).unwrap();
            let reply_ping_1 = rh_ping_1.recv(0).unwrap();

            assert_eq!(unhandled_1.len(), 0);

            if i == 0 {
                // initial key -- will get back the handshake
                assert_eq!(unhandled_2.len(), 1);   // only the handshake is given back.  the ping is consumed

                // convo 2 returns the handshake from convo 1
                match unhandled_2[0].payload {
                    StacksMessageType::Handshake(ref data) => {
                        assert_eq!(handshake_data_1, *data);
                    },
                    _ => {
                        assert!(false);
                    }
                };
            }
            else {
                // same key -- will NOT get back the handshake
                assert_eq!(unhandled_2.len(), 0);
            }

            // convo 2 replied to convo 1 with a matching pong
            match reply_ping_1.payload {
                StacksMessageType::Pong(ref data) => {
                    assert_eq!(data.nonce, ping_data_1.nonce);
                },
                _ => {
                    assert!(false);
                }
            }

            // received a valid HandshakeAccept from peer 2 
            match reply_handshake_1.payload {
                StacksMessageType::HandshakeAccept(ref data) => {
                    assert_eq!(data.handshake.addrbytes, local_peer_2.addrbytes);
                    assert_eq!(data.handshake.port, local_peer_2.port);
                    assert_eq!(data.handshake.services, local_peer_2.services);
                    assert_eq!(data.handshake.node_public_key, StacksPublicKeyBuffer::from_public_key(&Secp256k1PublicKey::from_private(&local_peer_2.private_key)));
                    assert_eq!(data.handshake.expire_block_height, local_peer_2.private_key_expire); 
                    assert_eq!(data.heartbeat_interval, conn_opts.heartbeat);
                },
                _ => {
                    assert!(false);
                }
            };

            // confirm that sequence numbers are increasing
            assert_eq!(reply_handshake_1.preamble.seq, 2*i);
            assert_eq!(reply_ping_1.preamble.seq, 2*i + 1);
            assert_eq!(convo_1.seq, 2*i + 2);

            // convo_2 got updated with convo_1's peer info, but no heartbeat info 
            assert_eq!(convo_2.peer_heartbeat, 0);
            assert_eq!(convo_2.connection.get_public_key().unwrap(), Secp256k1PublicKey::from_private(&local_peer_1.private_key));

            // convo_1 got updated with convo_2's peer info, as well as heartbeat
            assert_eq!(convo_1.peer_heartbeat, conn_opts.heartbeat);
            assert_eq!(convo_1.connection.get_public_key().unwrap(), Secp256k1PublicKey::from_private(&local_peer_2.private_key));

            // regenerate keys and expiries in peer 1
            let new_privkey = Secp256k1PrivateKey::new();
            {
                let mut tx = peerdb_1.tx_begin().unwrap();
                PeerDB::set_local_private_key(&mut tx, &new_privkey, (12350 + i) as u64).unwrap();
                tx.commit().unwrap();
            }
        }
    }

    #[test]
    fn convo_nack_unsolicited() {

        let conn_opts = ConnectionOptions::default();
        let socketaddr_1 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let socketaddr_2 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)), 8081);
        
        let first_burn_hash = BurnchainHeaderHash::from_hex("0000000000000000000000000000000000000000000000000000000000000000").unwrap();

        let burnchain = Burnchain {
            peer_version: PEER_VERSION,
            network_id: 0x9abcdef0,
            chain_name: "bitcoin".to_string(),
            network_name: "testnet".to_string(),
            working_dir: "/nope".to_string(),
            consensus_hash_lifetime: 24,
            stable_confirmations: 7,
            first_block_height: 12300,
            first_block_hash: first_burn_hash.clone(),
        };

        let mut chain_view = BurnchainView {
            burn_block_height: 12348,
            burn_consensus_hash: ConsensusHash::from_hex("1111111111111111111111111111111111111111").unwrap(),
            burn_stable_block_height: 12341,
            burn_stable_consensus_hash: ConsensusHash::from_hex("2222222222222222222222222222222222222222").unwrap(),
            last_consensus_hashes: HashMap::new()
        };
        chain_view.make_test_data();
        
        let first_burn_hash = BurnchainHeaderHash::from_hex("0000000000000000000000000000000000000000000000000000000000000000").unwrap();

        let mut peerdb_1 = PeerDB::connect_memory(0x9abcdef0, 12350, "http://peer1.com".into(), &vec![], &vec![]).unwrap();
        let mut peerdb_2 = PeerDB::connect_memory(0x9abcdef0, 12351, "http://peer2.com".into(), &vec![], &vec![]).unwrap();
        
        let mut burndb_1 = BurnDB::connect_memory(12300, &first_burn_hash).unwrap();
        let mut burndb_2 = BurnDB::connect_memory(12300, &first_burn_hash).unwrap();

        db_setup(&mut peerdb_1, &mut burndb_1, &socketaddr_1, &chain_view);
        db_setup(&mut peerdb_2, &mut burndb_2, &socketaddr_2, &chain_view);

        let local_peer_1 = PeerDB::get_local_peer(&peerdb_1.conn()).unwrap();
        let local_peer_2 = PeerDB::get_local_peer(&peerdb_2.conn()).unwrap();

        let mut convo_1 = Conversation::new(&burnchain, &socketaddr_2, &conn_opts, true, 0);
        let mut convo_2 = Conversation::new(&burnchain, &socketaddr_1, &conn_opts, true, 0);
       
        // no peer public keys known yet
        assert!(convo_1.connection.get_public_key().is_none());
        assert!(convo_2.connection.get_public_key().is_none());
        
        // convo_1 sends a ping to convo_2
        let ping_data_1 = PingData::new();
        let ping_1 = convo_1.sign_message(&chain_view, &local_peer_1.private_key, StacksMessageType::Ping(ping_data_1.clone())).unwrap();
        let mut rh_ping_1 = convo_1.send_signed_request(ping_1, 1000000).unwrap();

        // convo_2 will reply with a nack since peer_1 hasn't authenticated yet
        convo_send_recv(&mut convo_1, vec![&mut rh_ping_1], &mut convo_2, vec![]);
        let (unhandled_2, handles_2) = convo_2.chat(&local_peer_2, &chain_view).unwrap();

        // convo_1 has a nack 
        convo_send_recv(&mut convo_2, vec![&mut rh_ping_1], &mut convo_1, handles_2);
        let (unhandled_1, handles_1) = convo_1.chat(&local_peer_1, &chain_view).unwrap();

        let reply_1 = rh_ping_1.recv(0).unwrap();
       
        // convo_2 gives back nothing
        assert_eq!(unhandled_1.len(), 0);
        assert_eq!(unhandled_2.len(), 0);

        // convo_1 got a NACK 
        match reply_1.payload {
            StacksMessageType::Nack(ref data) => {
                assert_eq!(data.error_code, NackErrorCodes::HandshakeRequired);
            },
            _ => {
                assert!(false);
            }
        };

        // convo_2 did NOT get updated with convo_1's peer info
        assert_eq!(convo_2.peer_heartbeat, 0);
        assert!(convo_2.connection.get_public_key().is_none());

        // convo_1 did NOT get updated
        assert_eq!(convo_1.peer_heartbeat, 0);
        assert!(convo_2.connection.get_public_key().is_none());
    }

    #[test]
    fn convo_is_preamble_valid() {
        let conn_opts = ConnectionOptions::default();
        let socketaddr_1 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let socketaddr_2 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)), 8081);
        
        let first_burn_hash = BurnchainHeaderHash::from_hex("0000000000000000000000000000000000000000000000000000000000000000").unwrap();

        let burnchain = Burnchain {
            peer_version: PEER_VERSION,
            network_id: 0x9abcdef0,
            chain_name: "bitcoin".to_string(),
            network_name: "testnet".to_string(),
            working_dir: "/nope".to_string(),
            consensus_hash_lifetime: 24,
            stable_confirmations: 7,
            first_block_height: 12300,
            first_block_hash: first_burn_hash.clone(),
        };

        let mut chain_view = BurnchainView {
            burn_block_height: 12348,
            burn_consensus_hash: ConsensusHash::from_hex("1111111111111111111111111111111111111111").unwrap(),
            burn_stable_block_height: 12341,
            burn_stable_consensus_hash: ConsensusHash::from_hex("2222222222222222222222222222222222222222").unwrap(),
            last_consensus_hashes: HashMap::new()
        };
        chain_view.make_test_data();

        let mut peerdb_1 = PeerDB::connect_memory(0x9abcdef0, 12350, "http://peer1.com".into(), &vec![], &vec![]).unwrap();
        let mut burndb_1 = BurnDB::connect_memory(12300, &first_burn_hash).unwrap();
        let mut burndb_2 = BurnDB::connect_memory(12300, &first_burn_hash).unwrap();
        
        db_setup(&mut peerdb_1, &mut burndb_1, &socketaddr_1, &chain_view);
        
        let local_peer_1 = PeerDB::get_local_peer(&peerdb_1.conn()).unwrap();
        
        // network ID check
        {
            let mut convo_bad = Conversation::new(&burnchain, &socketaddr_2, &conn_opts, true, 0);

            let ping_data = PingData::new();
            convo_bad.burnchain.network_id += 1;
            let ping_bad = convo_bad.sign_message(&chain_view, &local_peer_1.private_key, StacksMessageType::Ping(ping_data.clone())).unwrap();
            convo_bad.burnchain.network_id -= 1;

            assert_eq!(convo_bad.is_preamble_valid(&ping_bad, &chain_view), Err(net_error::InvalidMessage));
        }

        // stable block height check
        {
            let mut convo_bad = Conversation::new(&burnchain, &socketaddr_2, &conn_opts, true, 0);

            let ping_data = PingData::new();
            
            let mut chain_view_bad = chain_view.clone();
            chain_view_bad.burn_stable_block_height -= 1;

            let ping_bad = convo_bad.sign_message(&chain_view_bad, &local_peer_1.private_key, StacksMessageType::Ping(ping_data.clone())).unwrap();

            assert_eq!(convo_bad.is_preamble_valid(&ping_bad, &chain_view), Err(net_error::InvalidMessage));
        }

        // node is too far ahead of us
        {
            let mut convo_bad = Conversation::new(&burnchain, &socketaddr_2, &conn_opts, true, 0);

            let ping_data = PingData::new();
            
            let mut chain_view_bad = chain_view.clone();
            chain_view_bad.burn_stable_block_height += MAX_NEIGHBOR_BLOCK_DELAY + 1 + burnchain.stable_confirmations as u64;
            chain_view_bad.burn_block_height += MAX_NEIGHBOR_BLOCK_DELAY + 1 + burnchain.stable_confirmations as u64;

            let ping_bad = convo_bad.sign_message(&chain_view_bad, &local_peer_1.private_key, StacksMessageType::Ping(ping_data.clone())).unwrap();
            
            chain_view_bad.burn_stable_block_height -= MAX_NEIGHBOR_BLOCK_DELAY + 1 + burnchain.stable_confirmations as u64;
            chain_view_bad.burn_block_height -= MAX_NEIGHBOR_BLOCK_DELAY + 1 + burnchain.stable_confirmations as u64;
            
            db_setup(&mut peerdb_1, &mut burndb_2, &socketaddr_2, &chain_view_bad);
            
            assert_eq!(convo_bad.is_preamble_valid(&ping_bad, &chain_view), Ok(false));
        }

        // unstable consensus hash mismatch
        {
            let mut convo_bad = Conversation::new(&burnchain, &socketaddr_2, &conn_opts, true, 0);

            let ping_data = PingData::new();
            
            let mut chain_view_bad = chain_view.clone();
            let old = chain_view_bad.burn_consensus_hash.clone();
            chain_view_bad.burn_consensus_hash = ConsensusHash::from_hex("3333333333333333333333333333333333333333").unwrap();
            chain_view_bad.last_consensus_hashes.insert(chain_view_bad.burn_block_height, chain_view_bad.burn_consensus_hash.clone());

            let ping_bad = convo_bad.sign_message(&chain_view_bad, &local_peer_1.private_key, StacksMessageType::Ping(ping_data.clone())).unwrap();
            
            assert_eq!(convo_bad.is_preamble_valid(&ping_bad, &chain_view), Ok(false));
        }

        // stable consensus hash mismatch 
        {
            let mut convo_bad = Conversation::new(&burnchain, &socketaddr_2, &conn_opts, true, 0);

            let ping_data = PingData::new();
            
            let mut chain_view_bad = chain_view.clone();
            let old = chain_view_bad.burn_stable_consensus_hash.clone();
            chain_view_bad.burn_stable_consensus_hash = ConsensusHash::from_hex("1111111111111111111111111111111111111112").unwrap();
            chain_view_bad.last_consensus_hashes.insert(chain_view_bad.burn_stable_block_height, chain_view_bad.burn_stable_consensus_hash.clone());

            let ping_bad = convo_bad.sign_message(&chain_view_bad, &local_peer_1.private_key, StacksMessageType::Ping(ping_data.clone())).unwrap();
            
            assert_eq!(convo_bad.is_preamble_valid(&ping_bad, &chain_view), Err(net_error::InvalidMessage));
        }
    }
}
