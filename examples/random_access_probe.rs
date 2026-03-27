use log::{info, LevelFilter};
use russh::*;
use russh_sftp::{client::SftpSession, protocol::OpenFlags};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;

struct Client;

impl client::Handler for Client {
    type Error = anyhow::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &russh::keys::PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

#[tokio::main]
async fn main() {
    env_logger::builder().filter_level(LevelFilter::Info).init();

    let config = russh::client::Config::default();
    let mut session = russh::client::connect(Arc::new(config), ("localhost", 22), Client)
        .await
        .unwrap();

    if !session
        .authenticate_password("root", "password")
        .await
        .unwrap()
        .success()
    {
        return;
    }

    let channel = session.channel_open_session().await.unwrap();
    channel.request_subsystem(true, "sftp").await.unwrap();
    let sftp = SftpSession::new(channel.into_stream()).await.unwrap();

    let path = "random-access-probe.bin";
    let mut file = sftp
        .open_random_access_with_flags(
            path,
            OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::READ | OpenFlags::WRITE,
        )
        .await
        .unwrap();

    file.write_at(0, b"alpha").await.unwrap();
    file.write_at(10, b"omega").await.unwrap();

    let prefix = file.read_at(0, 5).await.unwrap();
    let whole = file.read_at(0, 15).await.unwrap();

    info!("prefix: {:?}", String::from_utf8_lossy(&prefix));
    info!("whole: {:?}", whole);

    file.shutdown().await.unwrap();
    sftp.remove_file(path).await.unwrap();
}
