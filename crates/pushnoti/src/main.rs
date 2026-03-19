use anyhow::{Context, Result};
use clap::Parser;
use fcm_push_listener::*;
use log::*;
use std::{fs, path::PathBuf};
use tokio::io::AsyncWriteExt;
use tokio_stream::StreamExt;
use validator::Validate;

mod config;
mod opt;
mod utils;

use config::Config;
use opt::Opt;
use utils::find_and_connect;

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let opt = Opt::parse();

    let conf_path = opt.config.context("Must supply --config file")?;
    let config: Config = toml::from_str(
        &fs::read_to_string(&conf_path)
            .with_context(|| format!("Failed to read {:?}", conf_path))?,
    )
    .with_context(|| format!("Failed to parse the {:?} config file", conf_path))?;

    config
        .validate()
        .with_context(|| format!("Failed to validate the {:?} config file", conf_path))?;

    let camera = find_and_connect(&config, &opt.camera).await?;

    let http = reqwest::Client::new();

    // Firebase credentials for Reolink FCM registration
    let firebase_app_id = "1:743639030586:android:86f60a4fb7143876";
    let firebase_project_id = "reolink-login";
    let firebase_api_key = "AIzaSyBEUIuWHnnOEwFahxWgQB4Yt4NsgOmkPyE";
    let vapid_key = None;

    let token_path = PathBuf::from("./token.toml");
    let registration = if let Ok(Ok(registration)) =
        fs::read_to_string(&token_path).map(|v| toml::from_str::<Registration>(&v))
    {
        info!("Loaded token");
        registration
    } else {
        info!("Registering new token");
        let registration = fcm_push_listener::register(
            &http,
            firebase_app_id,
            firebase_project_id,
            firebase_api_key,
            vapid_key,
        )
        .await?;
        let new_token = toml::to_string(&registration)?;
        fs::write(token_path, new_token)?;
        registration
    };

    // Send registration.fcm_token to the server to allow it to send push messages to you.
    info!("registration.fcm_token: {}", registration.fcm_token);
    let uid = "6A5443E486511B0D828543445DC55A7D"; // MD5 Hash of "WHY_REOLINK"
    camera
        .send_pushinfo_android(&registration.fcm_token, uid)
        .await?;

    info!("Listening");
    let session = registration.gcm.checkin(&http).await?;
    let connection = session.new_connection(vec![]).await?;
    let mut stream = MessageStream::wrap(connection, &registration.keys);

    while let Some(message) = stream.next().await {
        match message? {
            Message::Data(data) => {
                let payload = String::from_utf8_lossy(&data.body);
                info!("Message JSON: {}", payload);
                info!("Persistent ID: {:?}", data.persistent_id);
            }
            Message::HeartbeatPing => {
                debug!("Heartbeat ping, sending ack");
                stream.write_all(&new_heartbeat_ack()).await?;
            }
            Message::Other(tag, bytes) => {
                debug!("Non-data message: tag={}, {} bytes", tag, bytes.len());
            }
        }
    }

    Ok(())
}
