// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Error, Result};
use log::{info, warn};
use protobuf::Message;
use raft::eraftpb::ConfChangeType;
use raft::eraftpb::ConfChangeV2;
use raft::eraftpb::Message as RaftMessage;
use raft::{raw_node::RawNode, storage::MemStorage, Config};
use slog::{o, Drain};
use tokio::sync::Mutex;
use tokio::time::sleep;
use tonic::transport::{Channel, Endpoint};
use tonic::{Request, Response, Status};

use crate::torchftpb::coordinator_service_client::CoordinatorServiceClient;
use crate::torchftpb::coordinator_service_server::CoordinatorService;
use crate::torchftpb::{
    InfoRequest, InfoResponse, NodeInfo, RaftMessageRequest, RaftMessageResponse,
};

pub struct Coordinator {
    rank: u64,
    local_addr: String,

    node: Mutex<RawNode<MemStorage>>,
    peers: Mutex<HashMap<u64, NodeInfo>>,
}

async fn new_coordinator_client(addr: String) -> Result<CoordinatorServiceClient<Channel>> {
    let conn = Endpoint::new(addr)?
        .connect_timeout(Duration::from_secs(10))
        .connect()
        .await?;

    Ok(CoordinatorServiceClient::new(conn))
}

impl Coordinator {
    pub fn new(rank: u64, world_size: u64, local_addr: String) -> Self {
        let config = Config {
            // ids start at 1
            id: rank + 1,
            ..Default::default()
        };
        // Initialize logger.
        let logger = slog::Logger::root(slog_stdlog::StdLog.fuse(), o!());

        // After, make sure it's valid!
        config.validate().unwrap();

        // We don't care about the log so we can use MemStorage
        let storage = MemStorage::new_with_conf_state((vec![1], vec![]));
        let mut node = RawNode::new(&config, storage, &logger).unwrap();

        let steps = (1..world_size + 1)
            .map(|i| raft_proto::new_conf_change_single(i, ConfChangeType::AddNode))
            .collect::<Vec<_>>();
        let mut cc = ConfChangeV2::default();
        cc.set_changes(steps.into());
        node.apply_conf_change(&cc).unwrap();

        Self {
            rank: rank,
            local_addr: local_addr,
            node: Mutex::new(node),
            peers: Mutex::new(HashMap::new()),
        }
    }

    pub async fn run(self: Arc<Self>) -> Result<()> {
        info!("running raft loop...");

        loop {
            self.tick().await?;

            // TODO: account for tick lag
            sleep(Duration::from_millis(100)).await;
        }
    }

    async fn tick(&self) -> Result<()> {
        let mut messages: Vec<RaftMessage> = Vec::new();

        {
            let mut node = self.node.lock().await;
            node.tick();

            if !node.has_ready() {
                return Ok(());
            }
            let mut ready = node.ready();

            // 1. Check whether messages is empty or not. If not, it means that the
            // node will send messages to other nodes:
            if !ready.messages().is_empty() {
                messages.append(&mut ready.take_messages());
            }

            // 2. Check whether snapshot is empty or not. If not empty, it means
            // that the Raft node has received a Raft snapshot from the leader and
            // we must apply the snapshot:

            if !ready.snapshot().is_empty() {
                // This is a snapshot, we need to apply the snapshot at first.
                node.mut_store()
                    .wl()
                    .apply_snapshot(ready.snapshot().clone())?;
            }

            // 3. Check whether committed_entries is empty or not. If not, it means
            // that there are some newly committed log entries which you must apply
            // to the state machine. Of course, after applying, you need to update
            // the applied index and resume apply later:

            // TODO: handle committed entries

            // 4. Check whether entries is empty or not. If not empty, it means that
            // there are newly added entries but have not been committed yet, we
            // must append the entries to the Raft log:
            if !ready.entries().is_empty() {
                // Append entries to the Raft log
                node.mut_store().wl().append(ready.entries()).unwrap();
            }

            // 5. Check whether hs is empty or not. If not empty, it means that the
            // HardState of the node has changed. For example, the node may vote for
            // a new leader, or the commit index has been increased. We must persist
            // the changed HardState:

            if let Some(hs) = ready.hs() {
                // Raft HardState changed, and we need to persist it.
                node.mut_store().wl().set_hardstate(hs.clone());
            }

            // 6. Check whether persisted_messages is empty or not. If not, it means
            // that the node will send messages to other nodes after persisting
            // hardstate, entries and snapshot:

            if !ready.persisted_messages().is_empty() {
                messages.append(&mut ready.take_persisted_messages());
            }

            // 7. Call advance to notify that the previous work is completed. Get
            // the return value LightReady and handle its messages and
            // committed_entries like step 1 and step 3 does. Then call
            // advance_apply to advance the applied index inside.

            let mut light_rd = node.advance(ready);
            // Like step 1 and 3, you can use functions to make them behave the same.
            messages.append(&mut light_rd.take_messages());
            //handle_committed_entries(light_rd.take_committed_entries());
            node.advance_apply();
        }

        self.handle_messages(&messages).await?;

        Ok(())
    }

    async fn get_client(&self, rank: u64) -> Result<CoordinatorServiceClient<Channel>> {
        let addr: String = {
            let peers = self.peers.lock().await;
            if !peers.contains_key(&rank) {
                return Err(Error::msg("rank not found".to_string()));
            }
            peers[&rank].address.clone()
        };

        return new_coordinator_client(addr).await;
    }

    async fn handle_messages(&self, messages: &[RaftMessage]) -> Result<()> {
        for message in messages {
            let rank = message.to - 1;
            let client = self.get_client(rank).await;
            if client.is_err() {
                warn!(
                    "failed to get client for rank {}, err {:?}",
                    rank,
                    client.err()
                );
                continue;
            }

            let request = tonic::Request::new(RaftMessageRequest {
                message: message.write_to_bytes()?,
            });

            client.unwrap().raft_message(request).await?;
        }

        Ok(())
    }

    fn node(&self) -> NodeInfo {
        NodeInfo {
            rank: self.rank,
            address: self.local_addr.clone(),
        }
    }

    pub async fn bootstrap(self: Arc<Self>, addrs: Vec<String>) -> Result<()> {
        let mut addresses: Vec<String> = addrs.clone();

        while addresses.len() > 0 {
            let addr = addresses.pop().unwrap();
            if addr == "" {
                continue;
            }

            info!("bootstrapping from {}", addr);

            let mut client = new_coordinator_client(addr.clone()).await?;

            let request = tonic::Request::new(InfoRequest {
                requester: Some(self.node()),
            });

            let response = client.info(request).await?;

            let info_response = response.into_inner();

            info!("info response {:?}", info_response);

            let mut peers = self.peers.lock().await;

            for peer in info_response.peers {
                if peer.rank == self.rank {
                    continue;
                }
                if !peers.contains_key(&peer.rank) {
                    info!("adding peer {:?}", peer);
                    peers.insert(peer.rank, peer.clone());
                    if addr != peer.address {
                        addresses.push(peer.address.clone());
                    }
                }
            }
        }

        Ok(())
    }
}

#[tonic::async_trait]
impl CoordinatorService for Arc<Coordinator> {
    async fn raft_message(
        &self,
        request: Request<RaftMessageRequest>,
    ) -> Result<Response<RaftMessageResponse>, Status> {
        let reply = RaftMessageResponse {};

        let message = RaftMessage::parse_from_bytes(request.into_inner().message.as_slice())
            .map_err(|e| Status::internal(format!("Failed to parse message: {}", e)))?;

        let mut node = self.node.lock().await;
        node.step(message)
            .map_err(|e| Status::internal(format!("Failed to step state machine: {}", e)))?;

        Ok(Response::new(reply))
    }

    async fn info(&self, request: Request<InfoRequest>) -> Result<Response<InfoResponse>, Status> {
        info!("got info request: {:?}", request);

        let mut peers = self.peers.lock().await;

        let requester = request.into_inner().requester.unwrap();
        info!("adding peer {:?}", requester);
        peers.insert(requester.rank, requester);

        let mut peers_vec: Vec<NodeInfo> = peers.values().cloned().collect();
        peers_vec.push(self.node());

        let reply = InfoResponse { peers: peers_vec };

        Ok(Response::new(reply)) // Send back our formatted greeting
    }
}
