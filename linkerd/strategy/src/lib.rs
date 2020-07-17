#![recursion_limit = "256"]

use futures::prelude::*;
use http_body::Body as HttpBody;
use linkerd2_error::{Error, Recover};
use linkerd2_proxy_api::destination::{self as api, destination_client::DestinationClient};
use std::{collections::HashMap, convert::TryFrom, net::SocketAddr};
use tokio::sync::watch;
use tonic::{
    self as grpc,
    body::{Body, BoxBody},
    client::GrpcService,
};
use tracing::trace;

#[derive(Clone, Debug)]
pub struct Client<S, R> {
    service: DestinationClient<S>,
    recover: R,
    context_token: String,
}

#[derive(Clone, Debug)]
pub struct Strategy {
    addr: SocketAddr,
    detect: Detect,
    target: Target,
}

#[derive(Copy, Clone, Debug)]
pub enum Detect {
    Client,
    Opaque,
}

#[derive(Clone, Debug)]
pub enum Target {
    Endpoint {
        addr: SocketAddr,
    },
    Concrete {
        authority: String,
        metric_labels: HashMap<String, String>,
        profile: (),
    },
    Logical {},
}

pub type Init = (Strategy, grpc::codec::Streaming<api::StrategyResponse>);

impl<S, R> Client<S, R>
where
    S: GrpcService<BoxBody> + Clone + Send + 'static,
    S::ResponseBody: Send,
    <S::ResponseBody as Body>::Data: Send,
    <S::ResponseBody as HttpBody>::Error:
        Into<Box<dyn std::error::Error + Send + Sync + 'static>> + Send,
    S::Future: Send,
    R: Recover<grpc::Status> + Clone + Send + 'static,
    R::Backoff: Send + Unpin,
{
    pub async fn watch(&mut self, addr: SocketAddr) -> Result<watch::Receiver<Strategy>, Error> {
        let req = api::StrategyRequest {
            target: Some(addr.into()),
            context_token: self.context_token.clone(),
        };

        let (strategy, stream) = match Self::init(addr, &mut self.service, req.clone()).await {
            Ok(rsp) => rsp,
            Err(status) => {
                Self::recover(
                    addr,
                    &mut self.service,
                    req.clone(),
                    &mut self.recover,
                    status,
                )
                .await?
            }
        };

        let (tx, rx) = watch::channel(strategy);
        tokio::spawn(Self::daemon(
            addr,
            self.service.clone(),
            req,
            self.recover.clone(),
            tx,
            stream,
        ));

        Ok(rx)
    }

    /// Processes an initialized stream/watch, recovering as permitted.
    async fn daemon(
        addr: SocketAddr,
        mut service: DestinationClient<S>,
        req: api::StrategyRequest,
        mut recover: R,
        mut tx: watch::Sender<Strategy>,
        mut responses: grpc::codec::Streaming<api::StrategyResponse>,
    ) {
        loop {
            match Self::broadcast(addr, &mut tx, &mut responses).await {
                Ok(()) => {
                    trace!("Shutting down; all receivers dropped");
                    return;
                }
                Err(status) => {
                    futures::select_biased! {
                        () = tx.closed().fuse() => { return; }
                        res = Self::recover(addr, &mut service, req.clone(), &mut recover, status).fuse() => {
                            match res {
                                Err(error) => {
                                    trace!(%error, "Watch failed");
                                    return;
                                }
                                Ok((strategy, stream)) => {
                                    // Broadcast the first update from the stream
                                    let _ = tx.broadcast(strategy);
                                    responses = stream;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// Retrieves the initial strategy.
    async fn init(
        addr: SocketAddr,
        service: &mut DestinationClient<S>,
        req: api::StrategyRequest,
    ) -> Result<Init, grpc::Status> {
        let mut stream = service.strategy(req.clone()).await?.into_inner();
        match stream.try_next().await {
            Ok(Some(rsp)) => Ok((Strategy::new(addr, rsp), stream)),
            Ok(None) => Err(grpc::Status::new(grpc::Code::Ok, "server closed stream")),
            Err(status) => Err(status),
        }
    }

    /// Publishes updates from `responses` to `tx` until either close.
    ///
    /// An error is returned if the `responses` stream terminates. Success is
    /// returned if `tx` is closed.
    async fn recover(
        addr: SocketAddr,
        service: &mut DestinationClient<S>,
        req: api::StrategyRequest,
        recover: &mut R,
        status: grpc::Status,
    ) -> Result<Init, Error> {
        let mut backoff = recover.recover(status)?;

        loop {
            if backoff.try_next().err_into::<Error>().await?.is_none() {
                return Err(grpc::Status::new(grpc::Code::Ok, "Backoff exhausted").into());
            }

            match Self::init(addr, service, req.clone()).await {
                Ok(rsp) => return Ok(rsp),
                Err(status) => {
                    // Reuse the existing backoff instead of resetting.
                    let _ = recover.recover(status)?;
                }
            }
        }
    }

    /// Publishes updates from `responses` to `tx` until either close.
    ///
    /// An error is returned if the `responses` stream terminates. Success is
    /// returned if `tx` is closed.
    async fn broadcast(
        addr: SocketAddr,
        tx: &mut watch::Sender<Strategy>,
        responses: &mut grpc::codec::Streaming<api::StrategyResponse>,
    ) -> Result<(), grpc::Status> {
        loop {
            futures::select_biased! {
                () = tx.closed().fuse() => {
                    return Ok(());
                }
                res = responses.try_next().fuse() => {
                    match res? {
                        Some(s) => {
                            let _ = tx.broadcast(Strategy::new(addr, s));
                        }
                        None => {
                            return Err(grpc::Status::new(grpc::Code::Ok, "server closed stream"));
                        }
                    }
                }
            }
        }
    }
}

impl Strategy {
    fn new(
        addr: SocketAddr,
        api::StrategyResponse { detect, target }: api::StrategyResponse,
    ) -> Self {
        use api::protocol_detection::Protocol;

        let detect = detect
            .and_then(|d| {
                d.protocol.map(|p| match p {
                    Protocol::Client(_) => Detect::Client,
                    Protocol::Opaque(_) => Detect::Opaque,
                })
            })
            .unwrap_or(Detect::Client);

        let target = target
            .and_then(|api::Target { kind }| {
                kind.and_then(|k| match k {
                    api::target::Kind::Endpoint(api::target::Endpoint { addr }) => {
                        addr.and_then(|wa| {
                            wa.addr
                                .and_then(|a| SocketAddr::try_from(a).ok())
                                .map(|addr| Target::Endpoint { addr })
                        })
                    }
                    api::target::Kind::Concrete(api::target::Concrete {
                        authority,
                        metric_labels,
                        profile: _,
                    }) => Some(Target::Concrete {
                        authority,
                        metric_labels,
                        profile: (),
                    }),
                    api::target::Kind::Logical(api::target::Logical {
                        metric_labels: _,
                        inner: _,
                    }) => Some(Target::Logical {}),
                })
            })
            .unwrap_or_else(|| Target::Endpoint { addr });

        Strategy {
            addr,
            detect,
            target,
        }
    }
}
