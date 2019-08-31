use std::net::{UdpSocket, SocketAddr};
use std::collections::{VecDeque, HashMap};
use std::cell::RefCell;
use std::cmp::min;
use std::io::Error;
use std::rc::Rc;
use std::str::FromStr;
use std::time::Duration;
use std::vec::Vec;
use crc::crc32;
use rand::random;
use time::{Timespec, get_time};

const CMD_SYN: u8 = 128;
const CMD_SYN_ACK: u8 = 129;
const CMD_ACK: u8 = 130;
const CMD_DATA: u8 = 131;
const CMD_HEARTBEAT: u8 = 132;
const CMD_HEARTBEAT_ACK: u8 = 133;
const UCP_PACKET_META_SIZE: usize = 29;
const DEFAULT_WINDOW: u32 = 512;
const DEFAULT_RTO: u32 = 100;
const HEARTBEAT_INTERVAL_MILLIS: i64 = 2500;
const UCP_STREAM_BROKEN_MILLIS: i64 = 20000;
const SKIP_RESEND_TIMES: u32 = 2;

struct UcpPacket {
    buf: [u8; 1400],
    size: usize,
    payload: u16,
    read_pos: usize,
    skip_times: u32,

    session_id: u32,
    timestamp: u32,
    window: u32,
    xmit: u32,
    una: u32,
    seq: u32,
    cmd: u8,
}

impl UcpPacket {
    fn new() -> UcpPacket {
        UcpPacket {
            buf: [0; 1400],
            size: 0,
            payload: 0,
            read_pos: 0,
            skip_times: 0,
            session_id: 0,
            timestamp: 0,
            window: 0,
            xmit: 0,
            una: 0,
            seq: 0,
            cmd: 0
        }
    }

    fn parse(&mut self) -> bool {
        if !self.is_legal() {
            return false
        }

        self.payload = (self.size - UCP_PACKET_META_SIZE) as u16;
        self.read_pos = UCP_PACKET_META_SIZE;

        let mut offset = 4;
        self.session_id = self.parse_u32(&mut offset);
        self.timestamp = self.parse_u32(&mut offset);
        self.window = self.parse_u32(&mut offset);
        self.xmit = self.parse_u32(&mut offset);
        self.una = self.parse_u32(&mut offset);
        self.seq = self.parse_u32(&mut offset);
        self.cmd = self.parse_u8(&mut offset);

        self.cmd >= CMD_SYN && self.cmd <= CMD_HEARTBEAT_ACK
    }

    fn pack(&mut self) {
        let mut offset = 4;
        let session_id = self.session_id;
        let timestamp = self.timestamp;
        let window = self.window;
        let xmit = self.xmit;
        let una = self.una;
        let seq = self.seq;
        let cmd = self.cmd;

        self.write_u32(&mut offset, session_id);
        self.write_u32(&mut offset, timestamp);
        self.write_u32(&mut offset, window);
        self.write_u32(&mut offset, xmit);
        self.write_u32(&mut offset, una);
        self.write_u32(&mut offset, seq);
        self.write_u8(&mut offset, cmd);

        offset = 0;
        self.size = self.payload as usize + UCP_PACKET_META_SIZE;

        let digest = crc32::checksum_ieee(&self.buf[4..self.size]);
        self.write_u32(&mut offset, digest);
    }

    fn packed_buffer(&self) -> &[u8] {
        &self.buf[..self.size]
    }

    fn parse_u32(&self, offset: &mut isize) -> u32 {
        let u = unsafe {
            *(self.buf.as_ptr().offset(*offset) as *const u32)
        };

        *offset += 4;
        u32::from_be(u)
    }

    fn parse_u8(&self, offset: &mut isize) -> u8 {
        let u = self.buf[*offset as usize];
        *offset += 1;
        u
    }

    fn write_u32(&mut self, offset: &mut isize, u: u32) {
        unsafe {
            *(self.buf.as_ptr().offset(*offset) as *mut u32)
                = u.to_be();
        }

        *offset += 4;
    }

    fn write_u8(&mut self, offset: &mut isize, u: u8) {
        self.buf[*offset as usize] = u;
        *offset += 1;
    }

    fn is_legal(&self) -> bool {
        self.size >= UCP_PACKET_META_SIZE && self.is_crc32_correct()
    }

    fn is_crc32_correct(&self) -> bool {
        let mut offset = 0;
        let digest = self.parse_u32(&mut offset);
        crc32::checksum_ieee(&self.buf[4..self.size]) == digest
    }

    fn is_syn(&self) -> bool {
        self.cmd == CMD_SYN
    }

    fn remaining_load(&self) -> usize {
        self.buf.len() - self.payload as usize - UCP_PACKET_META_SIZE
    }

    fn payload_offset(&self) -> isize {
        (self.payload as usize + UCP_PACKET_META_SIZE) as isize
    }

    fn payload_write_u32(&mut self, u: u32) -> bool {
        if self.remaining_load() >= 4 {
            let mut offset = self.payload_offset();
            self.write_u32(&mut offset, u);
            self.payload += 4;
            true
        } else {
            false
        }
    }

    fn payload_write_slice(&mut self, buf: &[u8]) -> bool {
        if self.remaining_load() >= buf.len() {
            let offset = self.payload_offset() as usize;
            let end = offset + buf.len();
            self.buf[offset..end].copy_from_slice(buf);
            self.payload += buf.len() as u16;
            true
        } else {
            false
        }
    }

    fn payload_remaining(&self) -> usize {
        self.size - self.read_pos
    }

    fn payload_read_u32(&mut self) -> u32 {
        if self.read_pos + 4 > self.size {
            panic!("Out of range when read u32 from {}", self.read_pos);
        }

        let mut offset = self.read_pos as isize;
        let u = self.parse_u32(&mut offset);
        self.read_pos = offset as usize;
        u
    }

    fn payload_read_slice(&mut self, buf: &mut [u8]) -> usize {
        let size = min(self.payload_remaining(), buf.len());
        let end_pos = self.read_pos + size;

        if size > 0 {
            buf[0..size].copy_from_slice(&self.buf[self.read_pos..end_pos]);
            self.read_pos = end_pos;
        }

        size
    }
}

type UcpPacketQueue = VecDeque<Box<UcpPacket>>;

enum UcpState {
    NONE,
    ACCEPTING,
    CONNECTING,
    ESTABLISHED
}

pub struct UcpStream {
    socket: UdpSocket,
    remote_addr: SocketAddr,
    initial_time: Timespec,
    alive_time: Timespec,
    heartbeat: Timespec,
    state: UcpState,

    send_queue: UcpPacketQueue,
    recv_queue: UcpPacketQueue,
    send_buffer: UcpPacketQueue,

    ack_list: Vec<(u32, u32)>,
    session_id: u32,
    local_window: u32,
    remote_window: u32,
    seq: u32,
    una: u32,
    rto: u32,

    on_update: Rc<RefCell<Option<Box<dyn FnMut(&mut UcpStream) -> bool>>>>,
    on_broken: Rc<RefCell<Option<Box<dyn FnMut(&mut UcpStream)>>>>
}

impl UcpStream {
    fn new(socket: UdpSocket, remote_addr: SocketAddr) -> UcpStream {
        UcpStream {
            socket: socket,
            remote_addr: remote_addr,
            initial_time: get_time(),
            alive_time: get_time(),
            heartbeat: get_time(),
            state: UcpState::NONE,

            send_queue: UcpPacketQueue::new(),
            recv_queue: UcpPacketQueue::new(),
            send_buffer: UcpPacketQueue::new(),

            ack_list: Vec::new(),
            local_window: DEFAULT_WINDOW,
            remote_window: DEFAULT_WINDOW,
            rto: DEFAULT_RTO,
            session_id: 0,
            seq: 0, una: 0,

            on_update: Rc::new(RefCell::new(None)),
            on_broken: Rc::new(RefCell::new(None))
        }
    }

    pub fn is_send_buffer_overflow(&self) -> bool {
        self.send_buffer.len() >= self.remote_window as usize
    }

    pub fn set_on_update<CB>(&mut self, cb: CB)
        where CB: 'static + FnMut(&mut UcpStream) -> bool {
        self.on_update = Rc::new(RefCell::new(Some(Box::new(cb))));
    }

    pub fn set_on_broken<CB>(&mut self, cb: CB)
        where CB: 'static + FnMut(&mut UcpStream) {
        self.on_broken = Rc::new(RefCell::new(Some(Box::new(cb))));
    }

    pub fn send(&mut self, buf: &[u8]) {
        let mut pos = 0;

        if let Some(packet) = self.send_buffer.back_mut() {
            let remain = min(packet.remaining_load(), buf.len());
            if remain > 0 {
                packet.payload_write_slice(&buf[0..remain]);
            }

            pos = remain;
        }

        if pos < buf.len() {
            self.make_packet_send(&buf[pos..]);
        }
    }

    pub fn recv(&mut self, buf: &mut [u8]) -> usize {
        let mut size = 0;

        while size < buf.len() && !self.recv_queue.is_empty() {
            if let Some(packet) = self.recv_queue.front_mut() {
                let diff = (packet.seq - self.una) as i32;
                if diff >= 0 {
                    break
                }

                size += packet.payload_read_slice(&mut buf[size..]);
            }

            let no_remain_payload = self.recv_queue.front().map(
                |packet| packet.payload_remaining() == 0).unwrap();

            if no_remain_payload {
                self.recv_queue.pop_front();
            }
        }

        size
    }

    fn update(&mut self) -> bool {
        let mut alive = self.check_if_alive();

        if alive {
            self.do_heartbeat();
            self.send_ack_list();
            self.timeout_resend();
            self.send_pending_packets();
            let on_update = self.on_update.clone();
            alive = (on_update.borrow_mut().as_mut().unwrap())(self);
        }

        alive
    }

    fn check_if_alive(&mut self) -> bool {
        let now = get_time();
        let interval = (now - self.alive_time).num_milliseconds();
        let alive = interval < UCP_STREAM_BROKEN_MILLIS;

        if !alive {
            let on_broken = self.on_broken.clone();
            (on_broken.borrow_mut().as_mut().unwrap())(self);
            error!("ucp alive timeout, remote address: {}, session: {}",
                   self.remote_addr, self.session_id);
        }

        alive
    }

    fn do_heartbeat(&mut self) {
        let now = get_time();
        let interval = (now - self.heartbeat).num_milliseconds();

        if interval >= HEARTBEAT_INTERVAL_MILLIS {
            let mut heartbeat = self.new_noseq_packet(CMD_HEARTBEAT);
            self.send_packet_directly(&mut heartbeat);
            self.heartbeat = now;
        }
    }

    fn send_ack_list(&mut self) {
        if self.ack_list.is_empty() {
            return
        }

        let mut packet = self.new_noseq_packet(CMD_ACK);

        for &(seq, timestamp) in self.ack_list.iter() {
            if packet.remaining_load() < 8 {
                self.send_packet_directly(&mut packet);
                packet = self.new_noseq_packet(CMD_ACK);
            }

            packet.payload_write_u32(seq);
            packet.payload_write_u32(timestamp);
        }

        self.send_packet_directly(&mut packet);
        self.ack_list.clear();
    }

    fn timeout_resend(&mut self) {
        let now = self.timestamp();

        for packet in self.send_queue.iter_mut() {
            let interval = now - packet.timestamp;
            let skip_resend = packet.skip_times >= SKIP_RESEND_TIMES;

            if interval >= self.rto || skip_resend {
                packet.skip_times = 0;
                packet.window = self.local_window;
                packet.una = self.una;
                packet.timestamp = now;
                packet.xmit += 1;
                packet.pack();

                let _ = self.socket.send_to(
                    packet.packed_buffer(), self.remote_addr);
            }
        }
    }

    fn send_pending_packets(&mut self) {
        let now = self.timestamp();
        let window = self.remote_window as usize;

        while self.send_queue.len() < window {
            if let Some(q) = self.send_queue.front() {
                if let Some(p) = self.send_buffer.front() {
                    let seq_diff = (p.seq - q.seq) as usize;
                    if seq_diff >= window {
                        break
                    }
                }
            }

            if let Some(mut packet) = self.send_buffer.pop_front() {
                packet.window = self.local_window;
                packet.una = self.una;
                packet.timestamp = now;

                self.send_packet_directly(&mut packet);
                self.send_queue.push_back(packet);
            } else {
                break
            }
        }
    }

    fn process_packet(&mut self, packet: Box<UcpPacket>,
                      remote_addr: SocketAddr) {
        if self.remote_addr != remote_addr {
            error!("unexpect packet from {}, expect from {}",
                   remote_addr, self.remote_addr);
            return
        }

        match self.state {
            UcpState::NONE => if packet.is_syn() {
                self.accepting(packet);
            },
            _ => {
                self.processing(packet)
            }
        }
    }

    fn connecting(&mut self) {
        self.state = UcpState::CONNECTING;
        self.session_id = random::<u32>();

        let syn = self.new_packet(CMD_SYN);
        self.send_packet(syn);
        info!("connecting ucp server {}, session: {}",
              self.remote_addr, self.session_id);
    }

    fn accepting(&mut self, packet: Box<UcpPacket>) {
        self.state = UcpState::ACCEPTING;
        self.session_id = packet.session_id;
        self.remote_window = packet.window;
        self.una = packet.seq + 1;

        let mut syn_ack = self.new_packet(CMD_SYN_ACK);
        syn_ack.payload_write_u32(packet.seq);
        syn_ack.payload_write_u32(packet.timestamp);
        self.send_packet(syn_ack);
        info!("accepting ucp client {}, session: {}",
              self.remote_addr, self.session_id);
    }

    fn processing(&mut self, packet: Box<UcpPacket>) {
        if self.session_id != packet.session_id {
            error!("unexpect session_id: {}, expect {}",
                   packet.session_id, self.session_id);
            return
        }

        self.alive_time = get_time();
        self.remote_window = packet.window;

        match self.state {
            UcpState::ACCEPTING => {
                self.process_state_accepting(packet);
            },
            UcpState::CONNECTING => {
                self.process_state_connecting(packet);
            },
            UcpState::ESTABLISHED => {
                self.process_state_established(packet);
            },
            UcpState::NONE => {}
        }
    }

    fn process_state_accepting(&mut self, mut packet: Box<UcpPacket>) {
        if packet.cmd == CMD_ACK && packet.payload == 8 {
            let seq = packet.payload_read_u32();
            let timestamp = packet.payload_read_u32();

            if self.process_an_ack(seq, timestamp) {
                self.state = UcpState::ESTABLISHED;
                info!("{} established, session: {}",
                      self.remote_addr, self.session_id);
            }
        }
    }

    fn process_state_connecting(&mut self, packet: Box<UcpPacket>) {
        self.process_syn_ack(packet);
    }

    fn process_state_established(&mut self, packet: Box<UcpPacket>) {
        self.process_una(packet.una);

        match packet.cmd {
            CMD_ACK => {
                self.process_ack(packet);
            },
            CMD_DATA => {
                self.process_data(packet);
            },
            CMD_SYN_ACK => {
                self.process_syn_ack(packet);
            },
            CMD_HEARTBEAT => {
                self.process_heartbeat();
            },
            CMD_HEARTBEAT_ACK => {
                self.process_heartbeat_ack();
            }
            _ => {}
        }
    }

    fn process_una(&mut self, una: u32) {
        while !self.send_queue.is_empty() {
            let diff = self.send_queue.front().map(
                |packet| (packet.seq - una) as i32).unwrap();

            if diff < 0 {
                self.send_queue.pop_front();
            } else {
                break
            }
        }
    }

    fn process_ack(&mut self, mut packet: Box<UcpPacket>) {
        if packet.cmd == CMD_ACK && packet.payload % 8 == 0 {
            while packet.payload_remaining() > 0 {
                let seq = packet.payload_read_u32();
                let timestamp = packet.payload_read_u32();
                self.process_an_ack(seq, timestamp);
            }
        }
    }

    fn process_data(&mut self, packet: Box<UcpPacket>) {
        self.ack_list.push((packet.seq, packet.timestamp));

        let una_diff = (packet.seq - self.una) as i32;
        if una_diff < 0 {
            return
        }

        let mut pos = 0;
        for i in 0..self.recv_queue.len() {
            let seq_diff = (packet.seq - self.recv_queue[i].seq) as i32;

            if seq_diff == 0 {
                return
            } else if seq_diff < 0 {
                break
            } else {
                pos += 1;
            }
        }

        self.recv_queue.insert(pos, packet);

        for i in pos..self.recv_queue.len() {
            if self.recv_queue[i].seq == self.una {
                self.una += 1;
            } else {
                break
            }
        }
    }

    fn process_syn_ack(&mut self, mut packet: Box<UcpPacket>) {
        if packet.cmd == CMD_SYN_ACK && packet.payload == 8 {
            let seq = packet.payload_read_u32();
            let timestamp = packet.payload_read_u32();

            let mut ack = self.new_noseq_packet(CMD_ACK);
            ack.payload_write_u32(packet.seq);
            ack.payload_write_u32(packet.timestamp);
            self.send_packet_directly(&mut ack);

            match self.state {
                UcpState::CONNECTING => {
                    if self.process_an_ack(seq, timestamp) {
                        self.state = UcpState::ESTABLISHED;
                        self.una = packet.seq + 1;
                        info!("{} established, session: {}",
                              self.remote_addr, self.session_id);
                    }
                },
                _ => {}
            }
        }
    }

    fn process_heartbeat(&mut self) {
        let mut heartbeat_ack = self.new_noseq_packet(CMD_HEARTBEAT_ACK);
        self.send_packet_directly(&mut heartbeat_ack);
    }

    fn process_heartbeat_ack(&mut self) {
        self.alive_time = get_time();
    }

    fn process_an_ack(&mut self, seq: u32, timestamp: u32) -> bool {
        let rtt = self.timestamp() - timestamp;
        self.rto = (self.rto + rtt) / 2;

        for i in 0..self.send_queue.len() {
            if self.send_queue[i].seq == seq {
                self.send_queue.remove(i);
                return true
            } else {
                if self.send_queue[i].timestamp <= timestamp {
                    self.send_queue[i].skip_times += 1;
                }
            }
        }

        false
    }

    fn new_packet(&mut self, cmd: u8) -> Box<UcpPacket> {
        let mut packet = Box::new(UcpPacket::new());

        packet.session_id = self.session_id;
        packet.timestamp = self.timestamp();
        packet.window = self.local_window;
        packet.seq = self.next_seq();
        packet.una = self.una;
        packet.cmd = cmd;

        packet
    }

    fn new_noseq_packet(&self, cmd: u8) -> Box<UcpPacket> {
        let mut packet = Box::new(UcpPacket::new());

        packet.session_id = self.session_id;
        packet.timestamp = self.timestamp();
        packet.window = self.local_window;
        packet.una = self.una;
        packet.cmd = cmd;

        packet
    }

    fn timestamp(&self) -> u32 {
        (get_time() - self.initial_time).num_milliseconds() as u32
    }

    fn next_seq(&mut self) -> u32 {
        self.seq += 1;
        self.seq
    }

    fn make_packet_send(&mut self, buf: &[u8]) {
        let buf_len = buf.len();

        let mut pos = 0;
        while pos < buf_len {
            let mut packet = self.new_packet(CMD_DATA);
            let size = min(packet.remaining_load(), buf_len - pos);
            let end_pos = pos + size;

            packet.payload_write_slice(&buf[pos..end_pos]);
            self.send_packet(packet);

            pos = end_pos;
        }
    }

    fn send_packet(&mut self, packet: Box<UcpPacket>) {
        self.send_buffer.push_back(packet);
    }

    fn send_packet_directly(&self, packet: &mut Box<UcpPacket>) {
        packet.pack();
        let _ = self.socket.send_to(packet.packed_buffer(), self.remote_addr);
    }
}

pub struct UcpClient {
    socket: UdpSocket,
    ucp: UcpStream,
    update_time: Timespec
}

impl UcpClient {
    pub fn connect(server_addr: &str) -> UcpClient {
        let socket = UdpSocket::bind("0.0.0.0:0").unwrap();
        let remote_addr = SocketAddr::from_str(server_addr).unwrap();

        let socket2 = socket.try_clone().unwrap();
        let mut ucp = UcpStream::new(socket2, remote_addr);
        ucp.connecting();

        socket.set_read_timeout(Some(Duration::from_millis(10))).unwrap();
        UcpClient { socket: socket, ucp: ucp, update_time: get_time() }
    }

    pub fn set_on_update<CB>(&mut self, cb: CB)
        where CB: 'static + FnMut(&mut UcpStream) -> bool {
        self.ucp.set_on_update(cb);
    }

    pub fn set_on_broken<CB>(&mut self, cb: CB)
        where CB: 'static + FnMut(&mut UcpStream) {
        self.ucp.set_on_broken(cb);
    }

    pub fn run(&mut self) {
        loop {
            let mut packet = Box::new(UcpPacket::new());
            let result = self.socket.recv_from(&mut packet.buf);

            if let Ok((size, remote_addr)) = result {
                packet.size = size;
                self.process_packet(packet, remote_addr);
            }

            if !self.update() {
                break
            }
        }
    }

    fn update(&mut self) -> bool {
        let now = get_time();
        if (now - self.update_time).num_milliseconds() < 10 {
            return true
        }

        self.update_time = now;
        self.ucp.update()
    }

    fn process_packet(&mut self, mut packet: Box<UcpPacket>,
                      remote_addr: SocketAddr) {
        if !packet.parse() {
            error!("recv illgal packet from {}", remote_addr);
            return
        }

        self.ucp.process_packet(packet, remote_addr);
    }
}

type UcpStreamMap = HashMap<SocketAddr, Rc<RefCell<UcpStream>>>;

pub struct UcpServer {
    socket: UdpSocket,
    ucp_map: UcpStreamMap,
    broken_ucp: Vec<SocketAddr>,
    on_new_ucp: Option<Box<dyn FnMut(&mut UcpStream)>>,
    update_time: Timespec
}

impl UcpServer {
    pub fn listen(listen_addr: &str) -> Result<UcpServer, Error> {
        match UdpSocket::bind(listen_addr) {
            Ok(socket) => {
                socket.set_read_timeout(
                    Some(Duration::from_millis(10))).unwrap();
                Ok(UcpServer { socket: socket,
                    ucp_map: UcpStreamMap::new(),
                    broken_ucp: Vec::new(),
                    on_new_ucp: None,
                    update_time: get_time() })
            },
            Err(e) => Err(e)
        }
    }

    pub fn set_on_new_ucp_stream<CB>(&mut self, cb: CB)
        where CB: 'static + FnMut(&mut UcpStream) {
        self.on_new_ucp = Some(Box::new(cb));
    }

    pub fn run(&mut self) {
        loop {
            let mut packet = Box::new(UcpPacket::new());
            let result = self.socket.recv_from(&mut packet.buf);

            if let Ok((size, remote_addr)) = result {
                packet.size = size;
                self.process_packet(packet, remote_addr);
            }

            self.update();
        }
    }

    fn update(&mut self) {
        let now = get_time();
        if (now - self.update_time).num_milliseconds() < 10 {
            return
        }

        for (key, ucp) in self.ucp_map.iter() {
            if !ucp.borrow_mut().update() {
                self.broken_ucp.push(key.clone());
            }
        }

        for key in self.broken_ucp.iter() {
            self.ucp_map.remove(key);
        }

        self.broken_ucp.clear();
        self.update_time = now;
    }

    fn process_packet(&mut self, mut packet: Box<UcpPacket>,
                      remote_addr: SocketAddr) {
        if !packet.parse() {
            error!("recv illgal packet from {}", remote_addr);
            return
        }

        if let Some(ucp) = self.ucp_map.get_mut(&remote_addr) {
            ucp.borrow_mut().process_packet(packet, remote_addr);
            return
        }

        if packet.is_syn() {
            info!("new ucp client from {}", remote_addr);
            self.new_ucp_stream(packet, remote_addr);
        } else {
            error!("no session ucp packet from {}", remote_addr);
        }
    }

    fn new_ucp_stream(&mut self, packet: Box<UcpPacket>,
                      remote_addr: SocketAddr) {
        let socket = self.socket.try_clone().unwrap();
        let mut ucp = UcpStream::new(socket, remote_addr);

        if let Some(ref mut on_new_ucp) = self.on_new_ucp {
            on_new_ucp(&mut ucp);
        }

        let ucp_impl = Rc::new(RefCell::new(ucp));
        let _ = self.ucp_map.insert(remote_addr, ucp_impl.clone());
        ucp_impl.borrow_mut().process_packet(packet, remote_addr);
    }
}
