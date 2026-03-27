use log::{info, LevelFilter};
use russh::*;
use russh_sftp::{client::SftpSession, protocol::OpenFlags};
use std::sync::Arc;

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

fn validate_round_trip(
    prefix: &[u8],
    whole: &[u8],
    expected_prefix: &[u8],
    expected_whole: &[u8],
) -> anyhow::Result<()> {
    anyhow::ensure!(prefix == expected_prefix, "prefix mismatch");
    anyhow::ensure!(whole == expected_whole, "whole-file mismatch");
    Ok(())
}

fn probe_success_message(prefix: &[u8], whole: &[u8]) -> anyhow::Result<&'static str> {
    validate_round_trip(prefix, whole, b"alpha", b"alpha\0\0\0\0\0omega")?;
    Ok("random-access probe round trip succeeded")
}

#[cfg(test)]
mod tests {
    use super::probe_success_message;

    #[test]
    fn probe_success_message_requires_a_validated_round_trip() {
        let message = probe_success_message(b"alpha", b"alpha\0\0\0\0\0omega").unwrap();
        assert_eq!(message, "random-access probe round trip succeeded");
    }

    #[test]
    fn probe_success_message_rejects_mismatched_bytes() {
        assert!(probe_success_message(b"alpha", b"beta").is_err());
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
    let file = sftp
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

    let success = probe_success_message(&prefix, &whole).unwrap();
    info!("prefix: {:?}", String::from_utf8_lossy(&prefix));
    info!("whole: {:?}", whole);
    info!("{success}");

    file.shutdown().await.unwrap();
    sftp.remove_file(path).await.unwrap();
}
