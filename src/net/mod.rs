pub mod tcp;

use std::io::{Error, Result};
use std::os::unix::io::{RawFd, AsRawFd, FromRawFd, IntoRawFd};
use std::net::{TcpListener as StdTcpListener, TcpStream as StdTcpStream};
use std::net::{ToSocketAddrs, SocketAddr};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::rc::Rc;
use std::cell::RefCell;

enum TcpStreamState {
    Idle,
    PollRegistered(crate::system::EventFuture),
}

struct Connect {
    address: Rc<iou::sqe::SockAddr>,
    event_id: usize,
    future: crate::system::EventFuture, // XXX if I box it, I could move it into a task without breaking pin or knowing event_id...
    completed: bool,
}

impl<'a> Future for Connect {
    type Output = std::result::Result<u32, std::io::Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut future = unsafe { Pin::new_unchecked(&mut self.future) };
        // let poll = unsafe { self.map_unchecked_mut(|s| &mut s.future).poll(cx) }; // XXX need to read up on pin
        match future.poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                self.completed = true;
                Poll::Ready(result)
            }
        }
    }
}

pub struct TcpStream {
    fd: RawFd,
    state: TcpStreamState,
}

impl TcpStream {
    pub async fn connect<A: ToSocketAddrs>(addr: A) -> Result<TcpStream> {
        // let address_storage = Rc::new(RefCell::new(iou::sqe::SockAddrStorage::uninit()));
        let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
        let address = Rc::new(to_iou_sock_addr(addr)?);
        let connect = unsafe { crate::system::connect(fd, address.as_ref()) };
        let connect = Connect {
            address: address.clone(),
            event_id: connect.event_id,
            future: connect,
            completed: false,
        };
        connect.await?; // XXX need to check return value?
        let stream = unsafe { TcpStream::from_raw_fd(fd as i32) };
        Ok(stream)
    }
}

impl AsRawFd for TcpStream {
    fn as_raw_fd(&self) -> RawFd {
        self.fd
    }
}

impl FromRawFd for TcpStream {
    unsafe fn from_raw_fd(fd: RawFd) -> Self {
        TcpStream {fd, state: TcpStreamState::Idle}
    }
}

impl IntoRawFd for TcpStream {
    fn into_raw_fd(self) -> RawFd {
        self.fd
    }
}

impl futures::io::AsyncRead for TcpStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<usize>> {
        use TcpStreamState::*;
        let fd = self.fd;
        match &mut self.state {
            Idle => {
                println!("adding fd = {} to poll", fd);
                let event = unsafe {
                    crate::system::poll_add(
                        fd,
                        iou::sqe::PollFlags::POLLIN,
                    )
                };
                self.state = PollRegistered(event);
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            PollRegistered(event) => {
                let mut event = unsafe {
                    Pin::new_unchecked(event)
                };
                match event.poll(cx) {
                    Poll::Pending => {
                        Poll::Pending
                    }
                    Poll::Ready(Ok(v)) => {
                        println!("poll ready: fd = {}, {}", fd, v);
                        let mut stream = unsafe {
                            StdTcpStream::from_raw_fd(fd) // XXX don't use std TcpStream
                        };
                        use std::io::Read;
                        let res = stream.read(buf);
                        let _ = stream.into_raw_fd(); // To prevent close call
                        self.state = Idle;
                        Poll::Ready(res)
                    }
                    Poll::Ready(Err(e)) => {
                        eprintln!("poll failed: {}", e);
                        self.state = Idle;
                        Poll::Ready(Err(e))
                    }
                }
            }
        }
    }
}

impl futures::io::AsyncWrite for TcpStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize>> {
        use TcpStreamState::*;
        let fd = self.fd;
        match &mut self.state {
            Idle => {
                println!("adding fd = {} to poll", fd);
                let event = unsafe {
                    crate::system::poll_add(
                        fd,
                        iou::sqe::PollFlags::POLLOUT, // XXX Writing is now possible, though a
                                                      // write larger that the available space in a socket or pipe will still
                                                      // block (unless O_NONBLOCK is set).
                    )
                };
                self.state = PollRegistered(event);
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            PollRegistered(event) => {
                let mut event = unsafe {
                    Pin::new_unchecked(event)
                };
                match event.poll(cx) {
                    Poll::Pending => {
                        println!("fd = {} still pending", fd);
                        Poll::Pending
                    }
                    Poll::Ready(Ok(v)) => {
                        println!("fd = {} ready", fd);
                        println!("poll ready: {}", v);
                        let mut stream = unsafe {
                            StdTcpStream::from_raw_fd(fd) // XXX don't use std TcpStream
                        };
                        use std::io::Write;
                        let res = stream.write(buf);
                        let _ = stream.into_raw_fd(); // To prevent close call
                        use std::io::ErrorKind::*;
                        match res.as_ref().map_err(Error::kind) {
                            Ok(_) => {
                                self.state = Idle;
                                Poll::Ready(res)
                            },
                            // https://docs.rs/futures/0.3.15/futures/io/trait.AsyncWrite.html#tymethod.poll_write
                            // This function may not return errors of kind WouldBlock or
                            // Interrupted. Implementations must convert WouldBlock into
                            // Poll::Pending and either internally retry or convert
                            // Interrupted into another error kind.
                            Err(WouldBlock) => {
                                println!("write would block");
                                // Mark as idle and wake up to retry.
                                self.state = Idle;
                                cx.waker().wake_by_ref();
                                Poll::Pending
                            }
                            Err(Interrupted) => {
                                println!("write interrupted");
                                // Mark as idle and wake up to retry.
                                self.state = Idle;
                                cx.waker().wake_by_ref();
                                Poll::Pending
                            }
                            Err(_) => {
                                self.state = Idle;
                                Poll::Ready(res)
                            },

                        }
                    }
                    Poll::Ready(Err(e)) => {
                        eprintln!("poll failed: {}", e);
                        self.state = Idle;
                        Poll::Ready(Err(e))
                    }
                }
            }
        }
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<()>> {
        todo!()
    }

    fn poll_close(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<()>> {
        todo!()
    }
}

// impl TcpStream {
//     pub async fn connect<A: ToSocketAddrs>(addr: A) -> Result<TcpStream> {
//         unsafe {
//             let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
//             let socket_addr: std::net::SocketAddr = "127.0.0.1:12345".parse().unwrap();
//             let sock_addr = iou::sqe::SockAddr::Inet(nix::sys::socket::InetAddr::from_std(&socket_addr));
//             let server = unsafe { system::connect(fd, &sock_addr) }.await.unwrap();
//             println!("connected to server: {}", server);
//             let bytes_sent = unsafe { system::send(fd, b"ping", MsgFlags::empty()) }.await.unwrap();
//             assert_eq!(4, bytes_sent);
//             let mut buf = [0; 32];
//             let bytes_received = unsafe { system::recv(fd, &mut buf[..], MsgFlags::empty()) }.await.unwrap();
//             println!("received bytes: {}", bytes_received);
//             assert_eq!(b"pong"[..], buf[..bytes_received as usize]);
//         }
//     }
// }

pub struct TcpListener {
    fd: RawFd,
}

impl TcpListener {
    pub async fn bind<A: ToSocketAddrs>(addr: A) -> Result<TcpListener> {
        let listener = StdTcpListener::bind(addr)?;
        let fd = listener.into_raw_fd();
        let listener = TcpListener {fd};
        Ok(listener)
    }

    pub async fn accept(&self) -> Result<(TcpStream, SocketAddr)> {
        let address_storage = Rc::new(RefCell::new(iou::sqe::SockAddrStorage::uninit()));
        let accept = unsafe { crate::system::accept(self.fd, Some(&mut *address_storage.as_ptr())) };
        let accept = Accept {
            listener: self,
            address_storage: address_storage.clone(),
            event_id: accept.event_id,
            future: accept,
            completed: false,
        };
        let fd = accept.await?;
        let stream = unsafe { TcpStream::from_raw_fd(fd as i32) };
        let addr = unsafe { address_storage.borrow().as_socket_addr() }?;
        let addr = to_std_socket_addr(addr)?;
        Ok((stream, addr))
    }
}

impl AsRawFd for TcpListener {
    fn as_raw_fd(&self) -> RawFd {
        self.fd
    }
}

impl FromRawFd for TcpListener {
    unsafe fn from_raw_fd(fd: RawFd) -> Self {
        TcpListener { fd }
    }
}

impl IntoRawFd for TcpListener {
    fn into_raw_fd(self) -> RawFd {
        self.fd
    }
}

struct Accept<'a> {
    listener: &'a TcpListener,
    address_storage: Rc<RefCell<iou::sqe::SockAddrStorage>>,
    event_id: usize,
    future: crate::system::EventFuture, // XXX if I box it, I could move it into a task without breaking pin or knowing event_id...
    completed: bool,
}

impl<'a> Future for Accept<'a> {
    type Output = std::result::Result<u32, std::io::Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let (mut future, mut completed) = unsafe {
            let this = self.get_unchecked_mut();
            (
                Pin::new_unchecked(&mut this.future),
                Pin::new_unchecked(&mut this.completed),
            )
        };
        // let poll = unsafe { self.map_unchecked_mut(|s| &mut s.future).poll(cx) }; // XXX need to read up on pin
        match future.poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                completed.set(true);
                Poll::Ready(result)
            }
        }
    }
}

impl<'a> Drop for Accept<'a> {
    fn drop(&mut self) {
        if !self.completed {
            let event_id = self.event_id;
            let address_storage = self.address_storage.clone();
            crate::spawn(async move {
                let _ = address_storage;
                crate::EventCompletion { target_event_id: event_id, primed: false }.await;
            });
        }
    }
}

fn to_iou_sock_addr<A: ToSocketAddrs>(addr: A) -> Result<iou::sqe::SockAddr> {
    use nix::sys::socket::InetAddr;
    let addr = addr.to_socket_addrs()?.next().unwrap(); // XXX
    let addr = InetAddr::from_std(&addr);
    let addr = iou::sqe::SockAddr::new_inet(addr);
    Ok(addr)
}

fn to_std_socket_addr(addr: iou::sqe::SockAddr) -> Result<SocketAddr> {
    use iou::sqe::SockAddr::*;
    match addr {
        Inet(addr) => {
            Ok(addr.to_std())
        }
        _ => todo!(),
    }
}
