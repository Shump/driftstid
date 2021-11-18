use std::io::Result;
use std::os::unix::io::{RawFd, FromRawFd, IntoRawFd};
use futures::io::AsyncRead;
use std::task::{Context, Poll};
use std::pin::Pin;
use std::net::{TcpStream as StdTcpStream};
use std::future::Future;

enum TcpStreamState {
    Idle,
    PollRegistered(crate::system::EventFuture),
}

struct ReadHalf<'a> {
    fd: RawFd,
    state: TcpStreamState,
    _stream: &'a crate::net::TcpStream,
}

impl<'a> AsyncRead for ReadHalf<'a> {
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
