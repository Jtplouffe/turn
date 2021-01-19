#[cfg(test)]
mod relay_conn_test;

// client implements the API for a TURN client
use super::binding::*;
use super::periodic_timer::*;
use super::permission::*;
use super::transaction::*;
use crate::proto;

use crate::errors::*;

use stun::agent::*;
use stun::attributes::*;
use stun::error_code::*;
use stun::fingerprint::*;
use stun::integrity::*;
use stun::message::*;
use stun::textattrs::*;

use util::{Conn, Error};

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::sync::{mpsc, Mutex};
use tokio::time::{Duration, Instant};

use async_trait::async_trait;

const MAX_READ_QUEUE_SIZE: usize = 1024;
const PERM_REFRESH_INTERVAL: Duration = Duration::from_secs(120);
const MAX_RETRY_ATTEMPTS: u16 = 3;

struct InboundData {
    data: Vec<u8>,
    from: SocketAddr,
}

// UDPConnObserver is an interface to UDPConn observer
#[async_trait]
pub trait RelayConnObserver {
    fn turn_server_addr(&self) -> SocketAddr;
    fn username(&self) -> Username;
    fn realm(&self) -> Realm;
    async fn write_to(&self, data: &[u8], to: SocketAddr) -> Result<usize, Error>;
    async fn perform_transaction(
        &mut self,
        msg: &Message,
        to: SocketAddr,
        dont_wait: bool,
    ) -> Result<TransactionResult, Error>;
    async fn on_deallocated(&self, relayed_addr: SocketAddr);
}

// RelayConnConfig is a set of configuration params use by NewUDPConn
pub struct RelayConnConfig {
    observer: Arc<Mutex<Box<dyn RelayConnObserver + Send + Sync>>>,
    relayed_addr: SocketAddr,
    integrity: MessageIntegrity,
    nonce: Nonce,
    lifetime: Duration,
}

pub struct RelayConnInternal {
    obs: Arc<Mutex<Box<dyn RelayConnObserver + Send + Sync>>>,
    relayed_addr: SocketAddr,
    perm_map: PermissionMap,
    binding_mgr: Arc<Mutex<BindingManager>>,
    integrity: MessageIntegrity,
    nonce: Nonce,
    lifetime: Duration,
}

// RelayConn is the implementation of the Conn interfaces for UDP Relayed network connections.
pub struct RelayConn {
    relayed_addr: SocketAddr,
    read_ch_tx: Option<mpsc::Sender<InboundData>>,
    read_ch_rx: Arc<Mutex<mpsc::Receiver<InboundData>>>,
    relay_conn: Arc<Mutex<RelayConnInternal>>,
    refresh_alloc_timer: PeriodicTimer,
    refresh_perms_timer: PeriodicTimer,
}

impl RelayConn {
    // new creates a new instance of UDPConn
    pub fn new(config: RelayConnConfig) -> Self {
        let (read_ch_tx, read_ch_rx) = mpsc::channel(MAX_READ_QUEUE_SIZE);
        let mut c = RelayConn {
            refresh_alloc_timer: PeriodicTimer::new(TimerIdRefresh::Alloc, config.lifetime / 2),
            refresh_perms_timer: PeriodicTimer::new(TimerIdRefresh::Perms, PERM_REFRESH_INTERVAL),
            relayed_addr: config.relayed_addr,
            read_ch_tx: Some(read_ch_tx),
            read_ch_rx: Arc::new(Mutex::new(read_ch_rx)),
            relay_conn: Arc::new(Mutex::new(RelayConnInternal::new(config))),
        };

        let rci1 = Arc::clone(&c.relay_conn);
        let rci2 = Arc::clone(&c.relay_conn);

        if c.refresh_alloc_timer.start(rci1) {
            log::debug!("refresh_alloc_timer started");
        }
        if c.refresh_perms_timer.start(rci2) {
            log::debug!("refresh_perms_timer started");
        }

        c
    }

    // handle_inbound passes inbound data in UDPConn
    pub fn handle_inbound(&self, data: &[u8], from: SocketAddr) -> Result<(), Error> {
        if let Some(read_ch_tx) = &self.read_ch_tx {
            if read_ch_tx
                .try_send(InboundData {
                    data: data.to_vec(),
                    from,
                })
                .is_err()
            {
                log::warn!("receive buffer full");
            }
            Ok(())
        } else {
            Err(ERR_ALREADY_CLOSED.to_owned())
        }
    }

    // Close closes the connection.
    // Any blocked ReadFrom or write_to operations will be unblocked and return errors.
    pub async fn close(&mut self) -> Result<(), Error> {
        if self.read_ch_tx.is_none() {
            return Err(ERR_ALREADY_CLOSED.to_owned());
        }
        self.refresh_alloc_timer.stop();
        self.refresh_perms_timer.stop();
        self.read_ch_tx.take();

        let mut relay_conn = self.relay_conn.lock().await;
        relay_conn.close().await
    }
}

#[async_trait]
impl Conn for RelayConn {
    async fn connect(&self, _addr: SocketAddr) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Other, "Not applicable"))
    }

    async fn recv(&self, _buf: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::new(io::ErrorKind::Other, "Not applicable"))
    }

    // ReadFrom reads a packet from the connection,
    // copying the payload into p. It returns the number of
    // bytes copied into p and the return address that
    // was on the packet.
    // It returns the number of bytes read (0 <= n <= len(p))
    // and any error encountered. Callers should always process
    // the n > 0 bytes returned before considering the error err.
    // ReadFrom can be made to time out and return
    // an Error with Timeout() == true after a fixed time limit;
    // see SetDeadline and SetReadDeadline.
    async fn recv_from(&self, p: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        let mut read_ch_rx = self.read_ch_rx.lock().await;

        if let Some(ib_data) = read_ch_rx.recv().await {
            let n = ib_data.data.len();
            if p.len() < n {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    ERR_SHORT_BUFFER.to_string(),
                ));
            }
            p[..n].copy_from_slice(&ib_data.data);
            Ok((n, ib_data.from))
        } else {
            Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                ERR_ALREADY_CLOSED.to_string(),
            ))
        }
    }

    async fn send(&self, _buf: &[u8]) -> io::Result<usize> {
        Err(io::Error::new(io::ErrorKind::Other, "Not applicable"))
    }

    // write_to writes a packet with payload p to addr.
    // write_to can be made to time out and return
    // an Error with Timeout() == true after a fixed time limit;
    // see SetDeadline and SetWriteDeadline.
    // On packet-oriented connections, write timeouts are rare.
    async fn send_to(&self, p: &[u8], addr: SocketAddr) -> io::Result<usize> {
        let mut relay_conn = self.relay_conn.lock().await;
        match relay_conn.send_to(p, addr).await {
            Ok(n) => Ok(n),
            Err(err) => Err(io::Error::new(io::ErrorKind::Other, err.to_string())),
        }
    }

    // LocalAddr returns the local network address.
    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.relayed_addr)
    }
}

impl RelayConnInternal {
    // new creates a new instance of UDPConn
    pub fn new(config: RelayConnConfig) -> Self {
        RelayConnInternal {
            obs: config.observer,
            relayed_addr: config.relayed_addr,
            perm_map: PermissionMap::new(),
            binding_mgr: Arc::new(Mutex::new(BindingManager::new())),
            integrity: config.integrity,
            nonce: config.nonce,
            lifetime: config.lifetime,
        }
    }

    // write_to writes a packet with payload p to addr.
    // write_to can be made to time out and return
    // an Error with Timeout() == true after a fixed time limit;
    // see SetDeadline and SetWriteDeadline.
    // On packet-oriented connections, write timeouts are rare.
    async fn send_to(&mut self, p: &[u8], addr: SocketAddr) -> Result<usize, Error> {
        // check if we have a permission for the destination IP addr
        let mut perm = if let Some(perm) = self.perm_map.find(&addr) {
            *perm
        } else {
            let perm = Permission::default();
            self.perm_map.insert(&addr, perm);
            perm
        };

        let mut result = Ok(());
        for _ in 0..MAX_RETRY_ATTEMPTS {
            result = self.create_perm(&mut perm, addr).await;
            if let Err(err) = &result {
                if *err != *ERR_TRY_AGAIN {
                    break;
                }
            }
        }
        if let Err(err) = result {
            return Err(err);
        }

        let number = {
            let (bind_st, bind_at, bind_number, bind_addr) = {
                let mut binding_mgr = self.binding_mgr.lock().await;
                let b = if let Some(b) = binding_mgr.find_by_addr(&addr) {
                    b
                } else {
                    binding_mgr
                        .create(addr)
                        .ok_or_else(|| Error::new("Addr not found".to_owned()))?
                };
                (b.state(), b.refreshed_at(), b.number, b.addr)
            };

            if bind_st == BindingState::Idle
                || bind_st == BindingState::Request
                || bind_st == BindingState::Failed
            {
                // block only callers with the same binding until
                // the binding transaction has been complete
                // binding state may have been changed while waiting. check again.
                if bind_st == BindingState::Idle {
                    let binding_mgr = Arc::clone(&self.binding_mgr);
                    let rc_obs = Arc::clone(&self.obs);
                    let nonce = self.nonce.clone();
                    let integrity = self.integrity.clone();
                    tokio::spawn(async move {
                        {
                            let mut bm = binding_mgr.lock().await;
                            if let Some(b) = bm.get_by_addr(&bind_addr) {
                                b.set_state(BindingState::Request);
                            }
                        }

                        let result = RelayConnInternal::bind(
                            rc_obs,
                            bind_addr,
                            bind_number,
                            nonce,
                            integrity,
                        )
                        .await;

                        {
                            let mut bm = binding_mgr.lock().await;
                            if let Err(err) = result {
                                if err != *ERR_UNEXPECTED_RESPONSE {
                                    bm.delete_by_addr(&bind_addr);
                                } else if let Some(b) = bm.get_by_addr(&bind_addr) {
                                    b.set_state(BindingState::Failed);
                                }

                                // keep going...
                                log::warn!("bind() failed: {}", err);
                            } else if let Some(b) = bm.get_by_addr(&bind_addr) {
                                b.set_state(BindingState::Ready);
                            }
                        }
                    });
                }

                // send data using SendIndication
                let peer_addr = socket_addr2peer_address(&addr);
                let mut msg = Message::new();
                msg.build(&[
                    Box::new(TransactionId::new()),
                    Box::new(MessageType::new(METHOD_SEND, CLASS_INDICATION)),
                    Box::new(proto::data::Data(p.to_vec())),
                    Box::new(peer_addr),
                    Box::new(FINGERPRINT),
                ])?;

                // indication has no transaction (fire-and-forget)
                let obs = self.obs.lock().await;
                return obs.write_to(&msg.raw, obs.turn_server_addr()).await;
            }

            // binding is either ready

            // check if the binding needs a refresh
            if bind_st == BindingState::Ready
                && Instant::now().duration_since(bind_at) > Duration::from_secs(5 * 60)
            {
                let binding_mgr = Arc::clone(&self.binding_mgr);
                let rc_obs = Arc::clone(&self.obs);
                let nonce = self.nonce.clone();
                let integrity = self.integrity.clone();
                tokio::spawn(async move {
                    {
                        let mut bm = binding_mgr.lock().await;
                        if let Some(b) = bm.get_by_addr(&bind_addr) {
                            b.set_state(BindingState::Refresh);
                        }
                    }

                    let result =
                        RelayConnInternal::bind(rc_obs, bind_addr, bind_number, nonce, integrity)
                            .await;

                    {
                        let mut bm = binding_mgr.lock().await;
                        if let Err(err) = result {
                            if err != *ERR_UNEXPECTED_RESPONSE {
                                bm.delete_by_addr(&bind_addr);
                            } else if let Some(b) = bm.get_by_addr(&bind_addr) {
                                b.set_state(BindingState::Failed);
                            }

                            // keep going...
                            log::warn!("bind() for refresh failed: {}", err);
                        } else if let Some(b) = bm.get_by_addr(&bind_addr) {
                            b.set_refreshed_at(Instant::now());
                            b.set_state(BindingState::Ready);
                        }
                    }
                });
            }

            bind_number
        };

        // send via ChannelData
        self.send_channel_data(p, number).await
    }

    // This func-block would block, per destination IP (, or perm), until
    // the perm state becomes "requested". Purpose of this is to guarantee
    // the order of packets (within the same perm).
    // Note that CreatePermission transaction may not be complete before
    // all the data transmission. This is done assuming that the request
    // will be mostly likely successful and we can tolerate some loss of
    // UDP packet (or reorder), inorder to minimize the latency in most cases.
    async fn create_perm(&mut self, perm: &mut Permission, addr: SocketAddr) -> Result<(), Error> {
        if perm.state() == PermState::Idle {
            // punch a hole! (this would block a bit..)
            if let Err(err) = self.create_permissions(&[addr]).await {
                self.perm_map.delete(&addr);
                return Err(err);
            }
            perm.set_state(PermState::Permitted);
        }
        Ok(())
    }

    async fn send_channel_data(&self, data: &[u8], ch_num: u16) -> Result<usize, Error> {
        let mut ch_data = proto::chandata::ChannelData {
            data: data.to_vec(),
            number: proto::channum::ChannelNumber(ch_num),
            ..Default::default()
        };
        ch_data.encode();

        let obs = self.obs.lock().await;
        obs.write_to(&ch_data.raw, obs.turn_server_addr()).await
    }

    async fn create_permissions(&mut self, addrs: &[SocketAddr]) -> Result<(), Error> {
        let res = {
            let msg = {
                let obs = self.obs.lock().await;
                let mut setters: Vec<Box<dyn Setter>> = vec![
                    Box::new(TransactionId::new()),
                    Box::new(MessageType::new(METHOD_CREATE_PERMISSION, CLASS_REQUEST)),
                ];

                for addr in addrs {
                    setters.push(Box::new(socket_addr2peer_address(addr)));
                }

                setters.push(Box::new(obs.username()));
                setters.push(Box::new(obs.realm()));
                setters.push(Box::new(self.nonce.clone()));
                setters.push(Box::new(self.integrity.clone()));
                setters.push(Box::new(FINGERPRINT));

                let mut msg = Message::new();
                msg.build(&setters)?;
                msg
            };

            let mut obs = self.obs.lock().await;
            let turn_server_addr = obs.turn_server_addr();
            let tr_res = obs
                .perform_transaction(&msg, turn_server_addr, false)
                .await?;

            tr_res.msg
        };

        if res.typ.class == CLASS_ERROR_RESPONSE {
            let mut code = ErrorCodeAttribute::default();
            let result = code.get_from(&res);
            if result.is_err() {
                return Err(Error::new(format!("{}", res.typ)));
            } else if code.code == CODE_STALE_NONCE {
                self.set_nonce_from_msg(&res);
                return Err(ERR_TRY_AGAIN.to_owned());
            } else {
                return Err(Error::new(format!("{} (error {})", res.typ, code)));
            }
        }

        Ok(())
    }

    pub fn set_nonce_from_msg(&mut self, msg: &Message) {
        // Update nonce
        match Nonce::get_from_as(msg, ATTR_NONCE) {
            Ok(nonce) => {
                self.nonce = nonce;
                log::debug!("refresh allocation: 438, got new nonce.");
            }
            Err(_) => log::warn!("refresh allocation: 438 but no nonce."),
        }
    }

    // Close closes the connection.
    // Any blocked ReadFrom or write_to operations will be unblocked and return errors.
    pub async fn close(&mut self) -> Result<(), Error> {
        {
            let obs = self.obs.lock().await;
            obs.on_deallocated(self.relayed_addr).await;
        }
        self.refresh_allocation(Duration::from_secs(0), true /* dontWait=true */)
            .await
    }

    // find_addr_by_channel_number returns a peer address associated with the
    // channel number on this UDPConn
    pub async fn find_addr_by_channel_number(&self, ch_num: u16) -> Option<SocketAddr> {
        let binding_mgr = self.binding_mgr.lock().await;
        if let Some(b) = binding_mgr.find_by_number(ch_num) {
            Some(b.addr)
        } else {
            None
        }
    }

    async fn refresh_allocation(
        &mut self,
        lifetime: Duration,
        dont_wait: bool,
    ) -> Result<(), Error> {
        let res = {
            let mut obs = self.obs.lock().await;

            let mut msg = Message::new();
            msg.build(&[
                Box::new(TransactionId::new()),
                Box::new(MessageType::new(METHOD_REFRESH, CLASS_REQUEST)),
                Box::new(proto::lifetime::Lifetime(lifetime)),
                Box::new(obs.username()),
                Box::new(obs.realm()),
                Box::new(self.nonce.clone()),
                Box::new(self.integrity.clone()),
                Box::new(FINGERPRINT),
            ])?;

            log::debug!("send refresh request (dont_wait={})", dont_wait);
            let turn_server_addr = obs.turn_server_addr();
            let tr_res = obs
                .perform_transaction(&msg, turn_server_addr, dont_wait)
                .await?;

            if dont_wait {
                log::debug!("refresh request sent");
                return Ok(());
            }

            log::debug!("refresh request sent, and waiting response");

            tr_res.msg
        };

        if res.typ.class == CLASS_ERROR_RESPONSE {
            let mut code = ErrorCodeAttribute::default();
            let result = code.get_from(&res);
            if result.is_err() {
                return Err(Error::new(format!("{}", res.typ)));
            } else if code.code == CODE_STALE_NONCE {
                self.set_nonce_from_msg(&res);
                return Err(ERR_TRY_AGAIN.to_owned());
            } else {
                return Ok(());
            }
        }

        // Getting lifetime from response
        let mut updated_lifetime = proto::lifetime::Lifetime::default();
        updated_lifetime.get_from(&res)?;

        self.lifetime = updated_lifetime.0;
        log::debug!("updated lifetime: {} seconds", self.lifetime.as_secs());
        Ok(())
    }

    async fn refresh_permissions(&mut self) -> Result<(), Error> {
        let addrs = self.perm_map.addrs();
        if addrs.is_empty() {
            log::debug!("no permission to refresh");
            return Ok(());
        }

        if let Err(err) = self.create_permissions(&addrs).await {
            if err != *ERR_TRY_AGAIN {
                log::error!("fail to refresh permissions: {}", err);
            }
            return Err(err);
        }

        log::debug!("refresh permissions successful");
        Ok(())
    }

    async fn bind(
        rc_obs: Arc<Mutex<Box<dyn RelayConnObserver + Send + Sync>>>,
        bind_addr: SocketAddr,
        bind_number: u16,
        nonce: Nonce,
        integrity: MessageIntegrity,
    ) -> Result<(), Error> {
        let (msg, turn_server_addr) = {
            let obs = rc_obs.lock().await;

            let setters: Vec<Box<dyn Setter>> = vec![
                Box::new(TransactionId::new()),
                Box::new(MessageType::new(METHOD_CHANNEL_BIND, CLASS_REQUEST)),
                Box::new(socket_addr2peer_address(&bind_addr)),
                Box::new(proto::channum::ChannelNumber(bind_number)),
                Box::new(obs.username()),
                Box::new(obs.realm()),
                Box::new(nonce),
                Box::new(integrity),
                Box::new(FINGERPRINT),
            ];

            let mut msg = Message::new();
            msg.build(&setters)?;

            (msg, obs.turn_server_addr())
        };

        let tr_res = {
            let mut obs = rc_obs.lock().await;
            obs.perform_transaction(&msg, turn_server_addr, false)
                .await?
        };

        let res = tr_res.msg;

        if res.typ != MessageType::new(METHOD_CHANNEL_BIND, CLASS_SUCCESS_RESPONSE) {
            return Err(ERR_UNEXPECTED_RESPONSE.to_owned());
        }

        log::debug!("channel binding successful: {} {}", bind_addr, bind_number);

        // Success.
        Ok(())
    }
}

#[async_trait]
impl PeriodicTimerTimeoutHandler for RelayConnInternal {
    async fn on_timeout(&mut self, id: TimerIdRefresh) {
        log::debug!("refresh timer {:?} expired", id);
        match id {
            TimerIdRefresh::Alloc => {
                let lifetime = self.lifetime;
                // limit the max retries on errTryAgain to 3
                // when stale nonce returns, sencond retry should succeed
                let mut result = Ok(());
                for _ in 0..MAX_RETRY_ATTEMPTS {
                    result = self.refresh_allocation(lifetime, false).await;
                    if let Err(err) = &result {
                        if *err != *ERR_TRY_AGAIN {
                            break;
                        }
                    }
                }
                if result.is_err() {
                    log::warn!("refresh allocation failed");
                }
            }
            TimerIdRefresh::Perms => {
                let mut result = Ok(());
                for _ in 0..MAX_RETRY_ATTEMPTS {
                    result = self.refresh_permissions().await;
                    if let Err(err) = &result {
                        if *err != *ERR_TRY_AGAIN {
                            break;
                        }
                    }
                }
                if result.is_err() {
                    log::warn!("refresh permissions failed");
                }
            }
        }
    }
}

fn socket_addr2peer_address(addr: &SocketAddr) -> proto::peeraddr::PeerAddress {
    proto::peeraddr::PeerAddress {
        ip: addr.ip(),
        port: addr.port(),
    }
}