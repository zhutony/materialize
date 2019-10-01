// Copyright 2019 Materialize, Inc. All rights reserved.
//
// This file is part of Materialize. Materialize may not be used or
// distributed without the express permission of Materialize, Inc.

//! The guts of the underlying network communication protocol.

use futures::{try_ready, Async, Future, Poll, Sink, Stream};
use ore::netio::{SniffedStream, SniffingStream};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::net::SocketAddr;
use tokio::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};
use tokio::io::{self, AsyncRead, AsyncWrite};
use tokio::net::unix::UnixStream;
use tokio::net::TcpStream;
use tokio_serde_bincode::{ReadBincode, WriteBincode};
use uuid::Uuid;

/// A magic number that is sent along at the beginning of each network
/// connection. The intent is to make it easy to sniff out `comm` traffic when
/// multiple protocols are multiplexed on the same port.
pub const PROTOCOL_MAGIC: [u8; 8] = [0x5f, 0x65, 0x44, 0x90, 0xaf, 0x4b, 0x3c, 0xfc];

/// Reports whether the connection handshake is `comm` traffic by sniffing out
/// whether the first bytes of `buf` match [`PROTOCOL_MAGIC`].
///
/// See [`crate::Switchboard::handle_connection`] for a usage example.
pub fn match_handshake(buf: &[u8]) -> bool {
    if buf.len() < 8 {
        return false;
    }
    buf[..8] == PROTOCOL_MAGIC
}

/// A trait for objects that can serve as the underlying transport layer for
/// `comm` traffic.
///
/// Only [`TcpStream`] and [`SniffedStream`] support is provided at the moment,
/// but support for any owned, thread-safe type which implements [`AsyncRead`]
/// and [`AsyncWrite`] can be added trivially, i.e., by implementing this trait.
pub trait Connection: AsyncRead + AsyncWrite + Send + 'static {
    /// The type that identifies the endpoint when establishing a connection of
    /// this type.
    type Addr: fmt::Debug
        + Eq
        + PartialEq
        + Send
        + Sync
        + Clone
        + Serialize
        + for<'de> Deserialize<'de>
        + Into<Addr>;

    /// Connects to the specified `addr`.
    fn connect(addr: &Self::Addr) -> Box<dyn Future<Item = Self, Error = io::Error> + Send>;
}

impl Connection for TcpStream {
    type Addr = SocketAddr;

    fn connect(addr: &Self::Addr) -> Box<dyn Future<Item = Self, Error = io::Error> + Send> {
        Box::new(TcpStream::connect(&addr).map(|conn| {
            conn.set_nodelay(true).expect("set_nodelay call failed");
            conn
        }))
    }
}

impl<C> Connection for SniffedStream<C>
where
    C: Connection,
{
    type Addr = C::Addr;

    fn connect(addr: &Self::Addr) -> Box<dyn Future<Item = Self, Error = io::Error> + Send> {
        Box::new(C::connect(addr).map(|conn| SniffingStream::new(conn).into_sniffed()))
    }
}

impl Connection for UnixStream {
    type Addr = std::path::PathBuf;

    fn connect(addr: &Self::Addr) -> Box<dyn Future<Item = Self, Error = io::Error> + Send> {
        Box::new(UnixStream::connect(addr))
    }
}

/// All known address types for [`Connection`]s.
///
/// The existence of this type is a bit unfortunate. It exists so that
/// [`mpsc::Sender`] does not need to be generic over [`Connection`], as
/// MPSC transmitters are meant to be lightweight and easy to stash in places
/// where a generic parameter might be a hassle. Ideally we'd make an `Addr`
/// trait and store a `Box<dyn Addr>`, but Rust does not currently permit
/// serializing and deserializing trait objects.
#[derive(Serialize, Deserialize, Eq, PartialEq, Clone, Debug)]
pub enum Addr {
    /// The address type for [`TcpStream`].
    Tcp(<TcpStream as Connection>::Addr),
    /// The address type for [`UnixStream`].
    Unix(<UnixStream as Connection>::Addr),
}

impl From<<TcpStream as Connection>::Addr> for Addr {
    fn from(addr: <TcpStream as Connection>::Addr) -> Addr {
        Addr::Tcp(addr)
    }
}

impl From<<UnixStream as Connection>::Addr> for Addr {
    fn from(addr: <UnixStream as Connection>::Addr) -> Addr {
        Addr::Unix(addr)
    }
}

pub(crate) fn send_handshake<C>(conn: C, uuid: Uuid, is_rendezvous: bool) -> SendHandshakeFuture<C>
where
    C: Connection,
{
    let mut buf = [0; 25];
    (&mut buf[..8]).copy_from_slice(&PROTOCOL_MAGIC);
    (&mut buf[8..24]).copy_from_slice(uuid.as_bytes());
    buf[24] = is_rendezvous.into();
    SendHandshakeFuture {
        inner: io::write_all(conn, buf),
    }
}

pub(crate) struct SendHandshakeFuture<C> {
    inner: io::WriteAll<C, [u8; 25]>,
}

impl<C> Future for SendHandshakeFuture<C>
where
    C: Connection,
{
    type Item = C;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let (stream, _buf) = try_ready!(self.inner.poll());
        Ok(Async::Ready(stream))
    }
}

pub(crate) fn recv_handshake<C>(conn: C) -> RecvHandshakeFuture<C>
where
    C: Connection,
{
    RecvHandshakeFuture {
        inner: io::read_exact(conn, [0; 25]),
    }
}

pub(crate) struct RecvHandshakeFuture<C>
where
    C: Connection,
{
    inner: io::ReadExact<C, [u8; 25]>,
}

impl<C> Future for RecvHandshakeFuture<C>
where
    C: Connection,
{
    type Item = (C, Uuid, bool);
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let (stream, buf) = try_ready!(self.inner.poll());
        let uuid_bytes = &buf[8..24];
        debug_assert_eq!(uuid_bytes.len(), 16);
        // Parsing a UUID only fails if the slice is not exactly 16 bytes, so
        // it's safe to unwrap here.
        let uuid = Uuid::from_slice(uuid_bytes).unwrap();
        let is_rendezvous = buf[24] > 0;
        Ok(Async::Ready((stream, uuid, is_rendezvous)))
    }
}

/// Constructs a [`Sink`] which encodes incoming `D`s using [bincode] and sends
/// them over the connection `conn` with a length prefix. Its dual is
/// [`decoder`].
///
/// [bincode]: https://crates.io/crates/bincode
pub(crate) fn encoder<C, D>(conn: C) -> impl Sink<SinkItem = D, SinkError = bincode::Error>
where
    C: Connection,
    D: Serialize + for<'de> Deserialize<'de> + Send,
{
    WriteBincode::new(FramedWrite::new(conn, LengthDelimitedCodec::new()).sink_from_err())
}

/// Constructs a [`Stream`] which decodes bincoded, length-prefixed `D`s from
/// the connection `conn`. Its dual is [`encoder`].
pub(crate) fn decoder<C, D>(conn: C) -> impl Stream<Item = D, Error = bincode::Error>
where
    C: Connection,
    D: Serialize + for<'de> Deserialize<'de> + Send,
{
    ReadBincode::new(FramedRead::new(conn, LengthDelimitedCodec::new()).from_err())
}
