pub mod foreign;
pub mod local;
pub mod native;
pub mod remote;

mod utils;

use crate::{
    error::Result,
    protocol::{Request, Response},
    state::ServerState,
};
use async_trait::async_trait;
use std::sync::Arc;

#[async_trait]
pub trait Server {
    async fn run(self, state: Arc<ServerState>);
}

#[async_trait]
pub trait Client {
    async fn invoke(&self, request: Request) -> Result<Response>;
}
