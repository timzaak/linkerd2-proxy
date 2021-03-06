use super::opaque_transport::OpaqueTransport;
use crate::{target::Endpoint, Outbound};
use futures::future;
use linkerd_app_core::{
    io, svc, tls,
    transport::{ConnectTcp, Remote, ServerAddr},
    transport_header::SessionProtocol,
    Error,
};
use std::task::{Context, Poll};
use tracing::debug_span;

/// Prevents outbound connections on the loopback interface, unless the
/// `allow-loopback` feature is enabled.
#[derive(Clone, Debug)]
pub struct PreventLoopback<S>(S);

// === impl Outbound ===

impl Outbound<()> {
    pub fn to_tcp_connect(&self) -> Outbound<PreventLoopback<ConnectTcp>> {
        let connect = PreventLoopback(ConnectTcp::new(self.config.proxy.connect.keepalive));
        self.clone().with_stack(connect)
    }
}

impl<C> Outbound<C> {
    pub fn push_tcp_endpoint<P>(
        self,
    ) -> Outbound<
        impl svc::Service<
                Endpoint<P>,
                Response = impl io::AsyncRead + io::AsyncWrite + Send + Unpin,
                Error = Error,
                Future = impl Send,
            > + Clone,
    >
    where
        Endpoint<P>: svc::Param<Option<SessionProtocol>>,
        C: svc::Service<Endpoint<P>, Error = io::Error> + Clone + Send + 'static,
        C::Response: tls::HasNegotiatedProtocol,
        C::Response: io::AsyncRead + io::AsyncWrite + Send + Unpin + 'static,
        C::Future: Send + 'static,
    {
        let Self {
            config,
            runtime: rt,
            stack: connect,
        } = self;
        let identity_disabled = rt.identity.is_none();

        let stack = connect
            // Initiates mTLS if the target is configured with identity. The
            // endpoint configures ALPN when there is an opaque transport hint OR
            // when an authority override is present (indicating the target is a
            // remote cluster gateway).
            .push(tls::Client::layer(rt.identity.clone()))
            // Encodes a transport header if the established connection is TLS'd and
            // ALPN negotiation indicates support.
            .push(OpaqueTransport::layer())
            // Limits the time we wait for a connection to be established.
            .push_timeout(config.proxy.connect.timeout)
            .push(svc::stack::BoxFuture::layer())
            .push(rt.metrics.transport.layer_connect())
            .push_map_target(move |e: Endpoint<P>| {
                if identity_disabled {
                    e.identity_disabled()
                } else {
                    e
                }
            });

        Outbound {
            config,
            runtime: rt,
            stack,
        }
    }

    pub fn push_tcp_forward<I>(
        self,
    ) -> Outbound<
        impl svc::NewService<
                super::Endpoint,
                Service = impl svc::Service<I, Response = (), Error = Error, Future = impl Send> + Clone,
            > + Clone,
    >
    where
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + std::fmt::Debug + Send + Unpin + 'static,
        C: svc::Service<super::Endpoint> + Clone + Send + 'static,
        C::Response: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin,
        C::Error: Into<Error>,
        C::Future: Send,
    {
        let Self {
            config,
            runtime,
            stack: connect,
        } = self;

        let stack = connect
            .push_make_thunk()
            .push_on_response(super::Forward::layer())
            .instrument(|_: &_| debug_span!("tcp.forward"))
            .check_new_service::<super::Endpoint, I>();

        Outbound {
            config,
            runtime,
            stack,
        }
    }
}

// === impl PreventLoopback ===

impl<S> PreventLoopback<S> {
    #[cfg(not(feature = "allow-loopback"))]
    fn check_loopback(Remote(ServerAddr(addr)): Remote<ServerAddr>) -> io::Result<()> {
        if addr.ip().is_loopback() {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                "Outbound proxy cannot initiate connections on the loopback interface",
            ));
        }

        Ok(())
    }

    #[cfg(feature = "allow-loopback")]
    fn check_loopback(_: Remote<ServerAddr>) -> io::Result<()> {
        Ok(())
    }
}

impl<T, S> svc::Service<T> for PreventLoopback<S>
where
    T: svc::Param<Remote<ServerAddr>>,
    S: svc::Service<T, Error = io::Error>,
{
    type Response = S::Response;
    type Error = io::Error;
    type Future = future::Either<S::Future, future::Ready<io::Result<S::Response>>>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.0.poll_ready(cx)
    }

    fn call(&mut self, ep: T) -> Self::Future {
        if let Err(e) = Self::check_loopback(ep.param()) {
            return future::Either::Right(future::err(e));
        }

        future::Either::Left(self.0.call(ep))
    }
}
