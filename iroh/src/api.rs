use std::path::Path;

use crate::getadd::{add, get};
#[cfg(feature = "mock")]
use crate::p2p::MockP2p;
use crate::p2p::{ClientP2p, P2p};
#[cfg(feature = "mock")]
use crate::store::MockStore;
use crate::store::{ClientStore, Store};
use anyhow::Result;
use async_trait::async_trait;
use cid::Cid;
use futures::stream::LocalBoxStream;
use futures::StreamExt;
use iroh_resolver::resolver::Path as IpfsPath;
use iroh_rpc_client::Client;
use iroh_rpc_client::StatusTable;
#[cfg(feature = "mock")]
use mockall::automock;

pub struct Iroh<'a> {
    client: &'a Client,
}

#[cfg_attr(feature= "mock", automock(type P = MockP2p; type S = MockStore;))]
#[async_trait(?Send)]
pub trait Api {
    type P: P2p;
    type S: Store;

    fn p2p(&self) -> Result<Self::P>;
    fn store(&self) -> Result<Self::S>;
    async fn get<'a>(&self, ipfs_path: &IpfsPath, output: Option<&'a Path>) -> Result<()>;
    async fn add(&self, path: &Path, recursive: bool, no_wrap: bool) -> Result<Cid>;
    async fn check(&self) -> StatusTable;
    async fn watch<'a>(&self) -> LocalBoxStream<'a, StatusTable>;
}

impl<'a> Iroh<'a> {
    pub fn new(client: &'a Client) -> Self {
        Self { client }
    }
}

#[async_trait(?Send)]
impl<'a> Api for Iroh<'a> {
    type P = ClientP2p<'a>;
    type S = ClientStore<'a>;

    fn p2p(&self) -> Result<ClientP2p<'a>> {
        let p2p_client = self.client.try_p2p()?;
        Ok(ClientP2p::new(p2p_client))
    }

    fn store(&self) -> Result<ClientStore<'a>> {
        let store_client = self.client.try_store()?;
        Ok(ClientStore::new(store_client))
    }

    async fn get<'b>(&self, ipfs_path: &IpfsPath, output: Option<&'b Path>) -> Result<()> {
        get(self.client, ipfs_path, output).await
    }

    async fn add(&self, path: &Path, recursive: bool, no_wrap: bool) -> Result<Cid> {
        add(self.client, path, recursive, no_wrap).await
    }

    async fn check(&self) -> StatusTable {
        self.client.check().await
    }

    async fn watch<'b>(&self) -> LocalBoxStream<'b, iroh_rpc_client::StatusTable> {
        self.client.clone().watch().await.boxed()
    }
}