use std::{
    collections::HashMap,
    io::{Read, Write},
    net::SocketAddr,
    os::unix::io::AsRawFd,
    sync::Mutex,
};

use frida_gum::interceptor::Interceptor;
use libc::{c_char, sockaddr, socklen_t};
use multi_map::MultiMap;
use os_socketaddr::OsSocketAddr;
use queues::{IsQueue, Queue};
use socketpair::{socketpair_stream, SocketpairStream};
use tracing::{debug, error};

use crate::{
    macros::{hook, try_hook},
    NEW_CONNECTION_SENDER, SOCKETS,
};

pub struct Socket {
    pub read_fd: SockFd,
    pub read_socket: SocketpairStream,
    pub write_socket: SocketpairStream,
}

pub struct ConnectionSocket {
    pub read_fd: SockFd,
    pub read_socket: SocketpairStream,
    pub write_socket: SocketpairStream,
    pub address: SocketAddr,
    pub state: ConnectionState,
}

#[derive(PartialEq)]
pub enum ConnectionState {
    Bound,
    Listening,
}

pub struct DataSocket {
    pub connection_id: ConnectionId,
    #[allow(dead_code)]
    pub read_socket: SocketpairStream, /* Though this is unread, it's necessary to keep the
                                        * socket open */
    pub write_socket: SocketpairStream,
    pub address: SocketAddr,
}

type SockFd = i32;
type Port = u16;
type ConnectionId = u16;
type TCPBuffer = Vec<u8>;

pub struct Sockets {
    new_sockets: Mutex<HashMap<SockFd, Socket>>,
    connections: Mutex<MultiMap<SockFd, Port, ConnectionSocket>>,
    data: Mutex<MultiMap<SockFd, ConnectionId, DataSocket>>,
    connection_queues: Mutex<HashMap<SockFd, Queue<ConnectionId>>>, /* Used to enqueue incoming
                                                                     * connection
                                                                     * ids from the
                                                                     * agent, to be read in the
                                                                     * 'accept'
                                                                     * call */
    pending_data: Mutex<HashMap<ConnectionId, TCPBuffer>>, /* Used to store data that arrived
                                                            * before its
                                                            * connection was opened. When the
                                                            * connection is
                                                            * later opened, pending_data is read
                                                            * and
                                                            * emptied. */
}

impl Default for Sockets {
    fn default() -> Self {
        Self {
            new_sockets: Mutex::new(HashMap::new()),
            connections: Mutex::new(MultiMap::new()),
            data: Mutex::new(MultiMap::new()),
            connection_queues: Mutex::new(HashMap::new()),
            pending_data: Mutex::new(HashMap::new()),
        }
    }
}

impl Sockets {
    pub fn create_socket(&self) -> SockFd {
        let (write_socket, read_socket) = socketpair_stream().unwrap();
        let read_fd = read_socket.as_raw_fd();
        let socket = Socket {
            read_fd,
            read_socket,
            write_socket,
        };

        self.new_sockets.lock().unwrap().insert(read_fd, socket);

        read_fd
    }

    pub fn convert_to_connection_socket(&self, sockfd: SockFd, address: SocketAddr) {
        let mut sockets = self.new_sockets.lock().unwrap();

        // let mut sockets = self.connections.lock().unwrap();
        if let Some(socket) = sockets.remove(&sockfd) {
            // socket.port = port;
            self.connections.lock().unwrap().insert(
                sockfd,
                address.port(),
                ConnectionSocket {
                    read_fd: socket.read_fd,
                    read_socket: socket.read_socket,
                    write_socket: socket.write_socket,
                    address,
                    state: ConnectionState::Bound,
                },
            );
        } else {
            error!("No socket found for fd: {}", sockfd);
        }
    }

    pub fn set_connection_state(&self, sockfd: SockFd, state: ConnectionState) -> Result<(), ()> {
        let mut connections = self.connections.lock().unwrap();
        if let Some(connection) = connections.get_mut(&sockfd) {
            if connection.state != state {
                connection.state = state;
                Ok(())
            } else {
                error!("No connection found for fd: {}", sockfd);
                Err(())
            }
        } else {
            error!("No connection found for fd: {}", sockfd);
            Err(())
        }
    }

    pub fn get_connection_socket_address(&self, sockfd: SockFd) -> Option<SocketAddr> {
        let sockets = self.connections.lock().unwrap();
        sockets.get(&sockfd).map(|socket| socket.address)
    }

    pub fn get_data_socket_address(&self, sockfd: SockFd) -> Option<SocketAddr> {
        let sockets = self.data.lock().unwrap();
        sockets.get(&sockfd).map(|socket| socket.address)
    }

    pub fn read_single_connection(&self, sockfd: SockFd) -> ConnectionId {
        let mut sockets = self.connections.lock().unwrap();

        if let Some(mut socket) = sockets.remove(&sockfd) {
            let mut buffer = [0; 1];
            socket.read_socket.read_exact(&mut buffer).unwrap();
            sockets.insert(sockfd, socket.address.port(), socket);
            let mut queues = self.connection_queues.lock().unwrap();
            let queue = queues.get_mut(&sockfd).unwrap();
            queue.remove().unwrap()
        } else {
            error!("No socket found for fd: {}", sockfd);
            0
        }
    }

    pub fn open_connection(&self, connection_id: ConnectionId, port: Port) {
        let mut connections = self.connections.lock().unwrap();
        if let Some(mut socket) = connections.remove_alt(&port) {
            debug!("new connection id: {:?}", connection_id);
            let mut queues = self.connection_queues.lock().unwrap();
            match queues.get_mut(&socket.read_fd) {
                Some(queue) => {
                    queue.add(connection_id).unwrap();
                }
                None => {
                    let mut queue = Queue::new();
                    queue.add(connection_id).unwrap();
                    queues.insert(socket.read_fd, queue);
                }
            }

            write!(socket.write_socket, "a").unwrap(); // Need to write one byte per incoming connection, hence "a"
            connections.insert(socket.read_fd, socket.address.port(), socket);
        } else {
            error!("No socket found for port: {}", port);
        }
    }

    pub fn create_data_socket(&self, connection_id: ConnectionId, address: SocketAddr) -> SockFd {
        let (read_socket, mut write_socket) = socketpair_stream().unwrap();
        let read_fd = read_socket.as_raw_fd();
        debug!(
            "Accepted connection from read_fd:{:?}, write_sock:{:?}",
            read_fd, write_socket
        );

        if let Some(data) = self.pending_data.lock().unwrap().remove(&connection_id) {
            debug!("writing pending data for connection_id: {}", connection_id);
            write_socket.write_all(&data).unwrap();
        }
        let read_fd = read_socket.as_raw_fd();
        let data_socket = DataSocket {
            connection_id,
            read_socket,
            write_socket,
            address,
        };
        self.data
            .lock()
            .unwrap()
            .insert(read_fd, connection_id, data_socket);
        read_fd
    }

    pub fn write_data(&self, connection_id: ConnectionId, data: TCPBuffer) {
        let mut sockets = self.data.lock().unwrap();
        if let Some(mut socket) = sockets.remove_alt(&connection_id) {
            socket.write_socket.write_all(&data).unwrap();
            clear_data(&mut socket.write_socket); // clear HTTP response data that the app wrote to this socket
            sockets.insert(socket.read_socket.as_raw_fd(), socket.connection_id, socket);
        } else {
            // Not necessarily an error - sometime the TCPData message is handled before
            // NewTcpConnection
            debug!("No socket found for connection_id: {}", connection_id);
            self.pending_data
                .lock()
                .unwrap()
                .insert(connection_id, data);
        }
    }

    pub fn close_connection(&self, connection_id: ConnectionId) {
        self.data
            .lock()
            .unwrap()
            .remove_alt(&connection_id)
            .unwrap();
    }
}

fn clear_data(socket: &mut SocketpairStream) {
    let num_ready_bytes = socket.num_ready_bytes().unwrap();
    let mut buffer = Vec::with_capacity(num_ready_bytes as usize);
    socket
        .take(num_ready_bytes)
        .read_to_end(&mut buffer)
        .unwrap();
}

unsafe extern "C" fn socket_detour(_domain: i32, _socket_type: i32, _protocol: i32) -> i32 {
    debug!("socket called");
    SOCKETS.create_socket()
}

unsafe extern "C" fn bind_detour(sockfd: i32, addr: *const sockaddr, addrlen: socklen_t) -> i32 {
    debug!("bind called");
    let parsed_addr = OsSocketAddr::from_raw_parts(addr as *const u8, addrlen as usize)
        .into_addr()
        .unwrap();

    SOCKETS.convert_to_connection_socket(sockfd, parsed_addr);
    0
}

unsafe extern "C" fn listen_detour(sockfd: i32, _backlog: i32) -> i32 {
    debug!("listen called");

    match SOCKETS.set_connection_state(sockfd, ConnectionState::Listening) {
        Ok(()) => {
            let sender = NEW_CONNECTION_SENDER.lock().unwrap();
            sender.as_ref().unwrap().blocking_send(sockfd).unwrap(); // Tell main thread to subscribe to agent
            0
        }
        Err(()) => {
            error!("Failed to set connection state to listening");
            -1
        }
    }
}

unsafe extern "C" fn getpeername_detour(
    sockfd: i32,
    addr: *mut sockaddr,
    addrlen: *mut socklen_t,
) -> i32 {
    let socket_addr = SOCKETS.get_data_socket_address(sockfd).unwrap();
    let os_addr: OsSocketAddr = socket_addr.into();
    let len = std::cmp::min(*addrlen as usize, os_addr.len() as usize);
    std::ptr::copy_nonoverlapping(os_addr.as_ptr() as *const u8, addr as *mut u8, len);

    *addrlen = os_addr.len();
    0
}

unsafe extern "C" fn setsockopt_detour(
    _sockfd: i32,
    _level: i32,
    _optname: i32,
    _optval: *mut c_char,
    _optlen: socklen_t,
) -> i32 {
    0
}

unsafe extern "C" fn accept_detour(
    sockfd: i32,
    addr: *mut sockaddr,
    addrlen: *mut socklen_t,
) -> i32 {
    debug!(
        "Accept called with sockfd {:?}, addr {:?}, addrlen {:?}",
        &sockfd, &addr, &addrlen
    );
    let socket_addr = SOCKETS.get_connection_socket_address(sockfd).unwrap();

    if !addr.is_null() {
        debug!("received non-null address in accept");
        let os_addr: OsSocketAddr = socket_addr.into();
        std::ptr::copy_nonoverlapping(os_addr.as_ptr(), addr, os_addr.len() as usize);
    }

    let connection_id = SOCKETS.read_single_connection(sockfd);
    SOCKETS.create_data_socket(connection_id, socket_addr)
}

unsafe extern "C" fn accept4_detour(
    sockfd: i32,
    addr: *mut sockaddr,
    addrlen: *mut socklen_t,
    _flags: i32,
) -> i32 {
    accept_detour(sockfd, addr, addrlen)
}

pub fn enable_hooks(mut interceptor: Interceptor) {
    hook!(interceptor, "socket", socket_detour);
    hook!(interceptor, "bind", bind_detour);
    hook!(interceptor, "listen", listen_detour);
    hook!(interceptor, "getpeername", getpeername_detour);
    hook!(interceptor, "setsockopt", setsockopt_detour);
    try_hook!(interceptor, "uv__accept4", accept4_detour);
    try_hook!(interceptor, "accept4", accept4_detour);
    try_hook!(interceptor, "accept", accept_detour);
}
