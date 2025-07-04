use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::Arc,
};

use anyhow::Context;
use librqbit_dualstack_sockets::{BindOpts, TcpListener};
use librqbit_utp::{BindDevice, UtpSocketUdp, UtpSocketUdpOpts};
use tokio::io::AsyncWrite;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::{stream_connect::ConnectionKind, vectored_traits::AsyncReadVectored};

pub(crate) struct ListenResult {
    pub tcp_socket: Option<TcpListener>,
    pub utp_socket: Option<Arc<UtpSocketUdp>>,
    pub enable_upnp_port_forwarding: bool,
    pub addr: SocketAddr,
    pub announce_port: Option<u16>,
}

#[derive(Debug, Clone, Copy)]
pub enum ListenerMode {
    TcpOnly,
    UtpOnly,
    TcpAndUtp,
}

impl ListenerMode {
    pub fn tcp_enabled(&self) -> bool {
        match self {
            ListenerMode::TcpOnly => true,
            ListenerMode::UtpOnly => false,
            ListenerMode::TcpAndUtp => true,
        }
    }

    pub fn utp_enabled(&self) -> bool {
        match self {
            ListenerMode::TcpOnly => false,
            ListenerMode::UtpOnly => true,
            ListenerMode::TcpAndUtp => true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ListenerOptions {
    pub mode: ListenerMode,
    pub listen_addr: SocketAddr,
    pub enable_upnp_port_forwarding: bool,
    pub utp_opts: Option<librqbit_utp::SocketOpts>,
}

impl Default for ListenerOptions {
    fn default() -> Self {
        Self {
            // TODO: once uTP is stable upgrade default to both
            mode: ListenerMode::TcpOnly,
            listen_addr: (Ipv4Addr::LOCALHOST, 0).into(),
            enable_upnp_port_forwarding: false,
            utp_opts: None,
        }
    }
}

impl ListenerOptions {
    pub(crate) async fn start(
        mut self,
        parent_span: Option<tracing::Id>,
        cancellation_token: CancellationToken,
        bind_device: Option<&BindDevice>,
    ) -> anyhow::Result<ListenResult> {
        if self.listen_addr.port() == 0 {
            anyhow::bail!("you must set the listen port explicitly")
        }
        let mut utp_opts = self.utp_opts.take().unwrap_or_default();
        utp_opts.cancellation_token = cancellation_token.clone();
        utp_opts.parent_span = parent_span;
        utp_opts.dont_wait_for_lastack = true;

        let tcp = async {
            if !self.mode.tcp_enabled() {
                return Ok::<_, anyhow::Error>(None);
            }
            let listener = TcpListener::bind_tcp(
                self.listen_addr,
                BindOpts {
                    request_dualstack: true,
                    reuseport: true,
                    device: bind_device,
                },
            )
            .context("error starting TCP listener")?;
            info!(
                "Listening on TCP {:?} for incoming peer connections",
                self.listen_addr
            );
            Ok(Some(listener))
        };

        let utp = async {
            if !self.mode.utp_enabled() {
                return Ok::<_, anyhow::Error>(None);
            }
            let socket = UtpSocketUdp::new_udp_with_opts(
                self.listen_addr,
                utp_opts,
                UtpSocketUdpOpts { bind_device },
            )
            .await
            .context("error starting uTP listener")?;
            info!(
                "Listening on UDP {:?} for incoming uTP peer connections",
                self.listen_addr
            );
            Ok(Some(socket))
        };

        let announce_port = if self.listen_addr.ip().is_loopback() {
            None
        } else {
            Some(self.listen_addr.port())
        };
        let (tcp_socket, utp_socket) = tokio::try_join!(tcp, utp)?;
        Ok(ListenResult {
            tcp_socket,
            utp_socket,
            announce_port,
            addr: self.listen_addr,
            enable_upnp_port_forwarding: self.enable_upnp_port_forwarding,
        })
    }
}

pub(crate) trait Accept {
    const KIND: ConnectionKind;

    async fn accept(
        &self,
    ) -> anyhow::Result<(
        SocketAddr,
        (
            impl AsyncReadVectored + Send + 'static,
            impl AsyncWrite + Unpin + Send + 'static,
        ),
    )>;
}

impl Accept for TcpListener {
    const KIND: ConnectionKind = ConnectionKind::Tcp;
    async fn accept(
        &self,
    ) -> anyhow::Result<(
        SocketAddr,
        (
            impl AsyncReadVectored + Send + 'static,
            (impl AsyncWrite + Send + 'static),
        ),
    )> {
        let (stream, addr) = self.accept().await.context("error accepting TCP")?;
        let (read, write) = stream.into_split();
        Ok((addr, (read, write)))
    }
}

impl Accept for Arc<UtpSocketUdp> {
    const KIND: ConnectionKind = ConnectionKind::Utp;
    async fn accept(
        &self,
    ) -> anyhow::Result<(
        SocketAddr,
        (
            impl AsyncReadVectored + Send + 'static,
            impl AsyncWrite + Unpin + Send + 'static,
        ),
    )> {
        let stream = self.accept().await.context("error accepting uTP")?;
        let addr = stream.remote_addr();
        let (read, write) = stream.split();
        Ok((addr, (read, write)))
    }
}
