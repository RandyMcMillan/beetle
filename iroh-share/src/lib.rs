use libp2p::{Multiaddr, PeerId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ticket {
    pub peer_id: PeerId,
    pub addrs: Vec<Multiaddr>,
    pub topic: String,
}

pub mod sender {
    use std::path::Path;
    use std::sync::atomic::AtomicU64;

    use anyhow::{Context, Result};
    use async_channel::{bounded, Receiver};
    use bytes::Bytes;
    use cid::Cid;
    use futures::channel::oneshot::{channel as oneshot, Receiver as OneShotReceiver};
    use futures::StreamExt;
    use iroh_metrics::store::Metrics;
    use iroh_p2p::{config, GossipsubEvent, Keychain, MemoryStorage, NetworkEvent, Node};
    use iroh_rpc_client::Client;
    use libp2p::gossipsub::{Sha256Topic, TopicHash};
    use libp2p::PeerId;
    use prometheus_client::registry::Registry;
    use tokio::task::JoinHandle;
    use tracing::error;

    use super::Ticket;

    /// The sending part of the data transfer.
    pub struct Sender {
        p2p_task: JoinHandle<()>,
        rpc: Client,
        next_id: AtomicU64,
        gossip_events: Receiver<GossipsubEvent>,
        store: iroh_store::Store,
    }

    impl Drop for Sender {
        fn drop(&mut self) {
            self.p2p_task.abort();
        }
    }

    impl Sender {
        pub async fn new(
            port: u16,
            rpc_p2p_port: u16,
            rpc_store_port: u16,
            db_path: &Path,
        ) -> Result<Self> {
            let rpc_p2p_addr = format!("0.0.0.0:{rpc_p2p_port}").parse().unwrap();
            let config = config::Libp2pConfig {
                listening_multiaddr: format!("/ip4/0.0.0.0/tcp/{port}").parse().unwrap(),
                mdns: true,
                rpc_addr: rpc_p2p_addr,
                rpc_client: iroh_rpc_client::Config {
                    p2p_addr: rpc_p2p_addr,
                    ..Default::default()
                },
                ..Default::default()
            };

            let rpc = Client::new(&config.rpc_client).await?;
            let rpc_store_addr = format!("0.0.0.0:{rpc_store_port}").parse().unwrap();
            let store_config = iroh_store::Config {
                path: db_path.to_path_buf(),
                rpc_addr: rpc_store_addr,
                rpc_client: iroh_rpc_client::Config {
                    p2p_addr: rpc_store_addr,
                    ..Default::default()
                },
                metrics: Default::default(),
            };
            let mut prom_registry = Registry::default();
            let store_metrics = Metrics::new(&mut prom_registry);
            let store = if store_config.path.exists() {
                iroh_store::Store::open(store_config, store_metrics).await?
            } else {
                iroh_store::Store::create(store_config, store_metrics).await?
            };

            let kc = Keychain::<MemoryStorage>::new();
            let mut p2p = Node::new(config, kc, &mut prom_registry).await?;
            let events = p2p.network_events();
            let (s, r) = bounded(1024);

            tokio::task::spawn(async move {
                while let Ok(event) = events.recv().await {
                    match event {
                        NetworkEvent::Gossipsub(e) => {
                            // drop events if they are not processed
                            s.try_send(e).ok();
                        }
                        _ => {}
                    }
                }
            });

            let p2p_task = tokio::task::spawn(async move {
                if let Err(err) = p2p.run().await {
                    error!("{:?}", err);
                }
            });

            Ok(Sender {
                p2p_task,
                rpc,
                next_id: 0.into(),
                gossip_events: r,
                store,
            })
        }

        pub async fn transfer_from_data(
            &self,
            name: impl Into<String>,
            data: Bytes,
        ) -> Result<Transfer<'_>> {
            let id = self.next_id();
            let t = Sha256Topic::new(format!("iroh-share-{}", id));
            let name = name.into();

            let (s, r) = oneshot();

            let root = {
                let mut file = iroh_resolver::unixfs_builder::FileBuilder::new();
                file.name(&name).content_bytes(data);
                let file = file.build().await?;
                let parts = file.encode();
                tokio::pin!(parts);
                let mut root_cid = None;
                while let Some(part) = parts.next().await {
                    // TODO: store links in the store
                    let (cid, bytes) = part?;
                    root_cid = Some(cid);
                    self.store.put(cid, bytes, []).await?;
                }
                root_cid.unwrap()
            };

            let gossip_events = self.gossip_events.clone();
            let topic_hash = t.hash();
            let th = topic_hash.clone();
            tokio::task::spawn(async move {
                while let Ok(event) = gossip_events.recv().await {
                    match event {
                        GossipsubEvent::Subscribed { peer_id, topic } => {
                            if topic == th {
                                s.send(peer_id).ok();
                                break;
                            }
                        }
                        _ => {}
                    }
                }
            });

            Ok(Transfer {
                id,
                topic: topic_hash,
                sender: self,
                name,
                root,
                peer: r,
            })
        }

        fn next_id(&self) -> u64 {
            self.next_id
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        }
    }

    pub struct Transfer<'a> {
        id: u64,
        name: String,
        root: Cid,
        sender: &'a Sender,
        peer: OneShotReceiver<PeerId>,
        topic: TopicHash,
    }

    impl Transfer<'_> {
        pub async fn ticket(self) -> Result<Ticket> {
            let (peer_id, addrs) = self
                .sender
                .rpc
                .p2p
                .get_listening_addrs()
                .await
                .context("getting p2p info")?;

            let root = self.root.to_bytes().to_vec(); // TODO: actual root hash.
            let peer = self.peer;
            let topic = self.topic;
            let topic_string = topic.to_string();
            let rpc = self.sender.rpc.clone();

            tokio::task::spawn(async move {
                match peer.await {
                    Ok(peer_id) => {
                        println!("S: {:?} subscribed, publishing root", peer_id);
                        rpc.p2p.gossipsub_publish(topic, root.into()).await.unwrap();
                    }
                    Err(e) => {
                        error!("failed to receive root, transfer aborted: {:?}", e);
                    }
                }
            });

            Ok(Ticket {
                peer_id,
                addrs,
                topic: topic_string,
            })
        }
    }
}

pub mod receiver {
    use anyhow::Result;
    use async_channel::{bounded, Receiver as ChannelReceiver};
    use cid::Cid;
    use iroh_p2p::{config, Keychain, MemoryStorage, NetworkEvent, Node};
    use iroh_rpc_client::Client;
    use libp2p::gossipsub::{GossipsubMessage, MessageId, TopicHash};
    use libp2p::PeerId;
    use prometheus_client::registry::Registry;
    use tokio::io::AsyncReadExt;
    use tokio::task::JoinHandle;
    use tracing::{error, warn};

    use super::Ticket;

    pub struct Receiver {
        p2p_task: JoinHandle<()>,
        rpc: Client,
        gossip_messages: ChannelReceiver<(MessageId, PeerId, GossipsubMessage)>,
        resolver: iroh_resolver::resolver::Resolver<iroh_rpc_client::Client>,
    }

    impl Drop for Receiver {
        fn drop(&mut self) {
            self.p2p_task.abort();
        }
    }

    impl Receiver {
        pub async fn new(port: u16, rpc_port: u16) -> Result<Self> {
            let rpc_addr = format!("0.0.0.0:{rpc_port}").parse().unwrap();
            let config = config::Libp2pConfig {
                listening_multiaddr: format!("/ip4/0.0.0.0/tcp/{port}").parse().unwrap(),
                mdns: true,
                rpc_addr,
                rpc_client: iroh_rpc_client::Config {
                    p2p_addr: rpc_addr,
                    ..Default::default()
                },
                ..Default::default()
            };

            let rpc = Client::new(&config.rpc_client).await?;

            let mut prom_registry = Registry::default();
            let resolver = iroh_resolver::resolver::Resolver::new(rpc.clone(), &mut prom_registry);

            let kc = Keychain::<MemoryStorage>::new();
            let mut p2p = Node::new(config, kc, &mut prom_registry).await?;
            let events = p2p.network_events();

            let p2p_task = tokio::task::spawn(async move {
                if let Err(err) = p2p.run().await {
                    error!("{:?}", err);
                }
            });

            let (s, r) = bounded(1024);

            tokio::task::spawn(async move {
                while let Ok(event) = events.recv().await {
                    match event {
                        NetworkEvent::Gossipsub(iroh_p2p::GossipsubEvent::Message {
                            from,
                            id,
                            message,
                        }) => {
                            s.try_send((id, from, message)).ok();
                        }
                        _ => {}
                    }
                }
            });

            Ok(Receiver {
                p2p_task,
                rpc,
                gossip_messages: r,
                resolver,
            })
        }

        pub async fn transfer_from_ticket(&self, ticket: Ticket) -> Result<Transfer<'_>> {
            // Connect to the sender
            self.rpc
                .p2p
                .connect(ticket.peer_id, ticket.addrs.clone())
                .await?;
            self.rpc
                .p2p
                .gossipsub_add_explicit_peer(ticket.peer_id)
                .await?;
            let topic = TopicHash::from_raw(&ticket.topic);
            let rpc = self.rpc.clone();
            self.rpc.p2p.gossipsub_subscribe(topic.clone()).await?;
            let gossip_messages = self.gossip_messages.clone();
            let expected_sender = ticket.peer_id;
            let resolver = self.resolver.clone();
            let (s, r) = bounded(1024);

            tokio::task::spawn(async move {
                while let Ok((_id, from, message)) = gossip_messages.recv().await {
                    if from == expected_sender {
                        match Cid::try_from(message.data) {
                            Ok(root) => {
                                println!("R: got roto {:?}, from: {:?}", root, from);
                                // TODO: resolve recursively
                                let res = resolver
                                    .resolve(iroh_resolver::resolver::Path::from_cid(root))
                                    .await;
                                s.send(res).await.unwrap();
                            }
                            Err(err) => {
                                warn!("got unexpected message from {}: {:?}", from, err);
                            }
                        }
                    } else {
                        warn!("got message from unexpected sender: {:?}", from);
                    }
                }
            });

            Ok(Transfer {
                receiver: self,
                ticket,
                topic,
                data_receiver: r,
            })
        }
    }

    pub struct Transfer<'a> {
        ticket: Ticket,
        receiver: &'a Receiver,
        topic: TopicHash,
        data_receiver: ChannelReceiver<Result<iroh_resolver::resolver::Out>>,
    }

    impl Transfer<'_> {
        pub async fn recv(&self) -> Result<Data> {
            let res = self.data_receiver.recv().await??;
            // TODO: notification
            let mut reader = res.pretty(self.receiver.rpc.clone(), Default::default());
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes).await?;

            Ok(Data {
                name: "".into(), // TODO,
                bytes,
            })
        }
    }

    pub struct Data {
        name: String,
        bytes: Vec<u8>,
    }

    impl Data {
        pub fn name(&self) -> &str {
            &self.name
        }

        pub fn bytes(&self) -> &[u8] {
            &self.bytes
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use anyhow::{Context, Result};
    use bytes::Bytes;

    use receiver as r;
    use sender as s;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_transfer() -> Result<()> {
        let sender_dir = tempfile::tempdir().unwrap();
        let sender_db = sender_dir.path().join("db");

        let sender = s::Sender::new(9990, 5550, 5560, &sender_db)
            .await
            .context("s:new")?;
        let bytes = Bytes::from(vec![1u8; 5 * 1024]);
        let sender_transfer = sender
            .transfer_from_data("foo.jpg", bytes.clone())
            .await
            .context("s: transfer")?;
        let ticket = sender_transfer.ticket().await.context("s: ticket")?;

        // the ticket is serialized, shared with the receiver and deserialized there

        let receiver = r::Receiver::new(9991, 5551).await.context("r: new")?;

        // tries to discover the sender, and receive the root
        let receiver_transfer = receiver
            .transfer_from_ticket(ticket)
            .await
            .context("r: transfer")?;

        tokio::time::sleep(Duration::from_secs(1)).await;

        let data = receiver_transfer.recv().await.context("r: recv")?;
        assert_eq!(data.name(), "foo.jpg");
        assert_eq!(data.bytes(), &bytes);

        Ok(())
    }
}