// A fake DNS resolver, standing in for the host's, and the DNS messages it
// hears and answers.
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::{Arc, Mutex};
use std::thread;

/// A query for `name`'s A record, as a stub resolver asks it.
pub fn query(id: u16, name: &str) -> Vec<u8> {
    let mut q = id.to_be_bytes().to_vec();
    // Recursion desired; one question.
    q.extend([0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0]);
    for label in name.split('.') {
        q.push(label.len() as u8);
        q.extend(label.as_bytes());
    }
    q.extend([0, 0, 1, 0, 1]);
    q
}

/// What the fake resolver answers: the query as a response, with one A
/// record, 192.0.2.1.
pub fn answer_to(query: &[u8]) -> Vec<u8> {
    let mut a = query.to_vec();
    a[2] |= 0x80;
    a[7] = 1;
    a.extend([0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 192, 0, 2, 1]);
    a
}

/// A fake resolver, on one port over UDP and TCP. It notes each query it
/// hears, and answers it unless it's silent: then it never does, and leaves
/// each TCP connection open.
pub struct Resolver {
    pub at: SocketAddr,
    heard: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl Resolver {
    /// One on a free port of localhost.
    pub fn start(answers: bool) -> Resolver {
        let (udp, tcp) = same_port("127.0.0.1");
        Resolver::on(udp, tcp, answers)
    }

    /// One on these sockets, bound to the same address.
    pub fn on(udp: UdpSocket, tcp: TcpListener, answers: bool) -> Resolver {
        let at = udp.local_addr().unwrap();
        let heard = Arc::new(Mutex::new(Vec::new()));
        let noted = heard.clone();
        thread::spawn(move || {
            let mut buf = [0u8; 65536];
            while let Ok((n, from)) = udp.recv_from(&mut buf) {
                noted.lock().unwrap().push(buf[..n].to_vec());
                if answers {
                    let _ = udp.send_to(&answer_to(&buf[..n]), from);
                }
            }
        });
        let noted = heard.clone();
        thread::spawn(move || {
            for conn in tcp.incoming() {
                let Ok(mut conn) = conn else { return };
                let noted = noted.clone();
                thread::spawn(move || {
                    while let Ok(q) = read_framed(&mut conn) {
                        noted.lock().unwrap().push(q.clone());
                        if answers && write_framed(&mut conn, &answer_to(&q)).is_err() {
                            return;
                        }
                    }
                });
            }
        });
        Resolver { at, heard }
    }

    pub fn heard(&self) -> Vec<Vec<u8>> {
        self.heard.lock().unwrap().clone()
    }
}

/// A UDP socket and a TCP listener on the same free port of `ip`.
pub fn same_port(ip: &str) -> (UdpSocket, TcpListener) {
    for _ in 0..50 {
        let udp = UdpSocket::bind((ip, 0)).unwrap();
        let port = udp.local_addr().unwrap().port();
        if let Ok(tcp) = TcpListener::bind((ip, port)) {
            return (udp, tcp);
        }
    }
    panic!("no port free for both UDP and TCP on {ip}");
}

pub fn read_framed(conn: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut len = [0u8; 2];
    conn.read_exact(&mut len)?;
    let mut msg = vec![0u8; usize::from(u16::from_be_bytes(len))];
    conn.read_exact(&mut msg)?;
    Ok(msg)
}

pub fn write_framed(conn: &mut TcpStream, msg: &[u8]) -> std::io::Result<()> {
    let len = u16::try_from(msg.len()).unwrap().to_be_bytes();
    conn.write_all(&[&len[..], msg].concat())
}
