use std::{
    io,
    net::{Ipv4Addr, Ipv6Addr},
    sync::Arc,
};

use crate::{
    app::{dispatcher::BoxedChainedStream, dns::ThreadSafeDNSResolver},
    session::{Session, SocksAddr},
};

use super::{
    AnyOutboundDatagram, AnyOutboundHandler, AnyStream, CommonOption, OutboundHandler, OutboundType,
};

use async_trait::async_trait;
pub use netstack_lwip as netstack;

pub struct Opts {
    pub name: String,
    pub common_opts: CommonOption,
    pub server: String,
    pub port: u16,
    pub ip: Ipv4Addr,
    pub ipv6: Option<Ipv6Addr>,
    pub private_key: String,
    pub public_key: String,
    pub preshared_key: Option<String>,
    pub remote_dns_resolve: bool,
    pub dns: Option<Vec<String>>,
    pub mtu: Option<u16>,
    pub udp: bool,
}

pub struct Handler {
    opts: Opts,

    device: boringtun::device::Device,
}

impl Handler {
    pub fn new(opts: Opts) -> AnyOutboundHandler {
        let device_cfg = boringtun::device::DeviceConfig::default();
        let device = boringtun::device::Device::new("utun", device_cfg).unwrap();
        Arc::new(Self { opts, device })
    }
}

#[async_trait]
impl OutboundHandler for Handler {
    fn name(&self) -> &str {
        &self.opts.name
    }

    fn proto(&self) -> OutboundType {
        OutboundType::WireGuard
    }

    async fn remote_addr(&self) -> Option<SocksAddr> {
        None
    }

    async fn support_udp(&self) -> bool {
        self.opts.udp
    }

    /// connect to remote target via TCP
    async fn connect_stream(
        &self,
        sess: &Session,
        resolver: ThreadSafeDNSResolver,
    ) -> io::Result<BoxedChainedStream> {
        todo!()
    }

    /// wraps a stream with outbound handler
    async fn proxy_stream(
        &self,
        s: AnyStream,
        sess: &Session,
        resolver: ThreadSafeDNSResolver,
    ) -> io::Result<AnyStream> {
        todo!()
    }

    /// connect to remote target via UDP
    async fn connect_datagram(
        &self,
        sess: &Session,
        resolver: ThreadSafeDNSResolver,
    ) -> io::Result<AnyOutboundDatagram> {
        todo!()
    }
}
