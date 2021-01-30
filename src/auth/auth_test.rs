use super::*;
use crate::client::*;
use crate::relay::relay_static::*;
use crate::server::{config::*, *};

use std::net::IpAddr;
use std::str::FromStr;
use std::sync::Arc;

use tokio::net::UdpSocket;
use util::Error;

#[test]
fn test_lt_cred() -> Result<(), Error> {
    let username = "1599491771";
    let shared_secret = "foobar";

    let expected_password = "Tpz/nKkyvX/vMSLKvL4sbtBt8Vs=";
    let actual_password = long_term_credentials(username, shared_secret)?;
    assert_eq!(
        expected_password, actual_password,
        "Expected {}, got {}",
        expected_password, actual_password
    );

    Ok(())
}

#[test]
fn test_generate_auth_key() -> Result<(), Error> {
    let username = "60";
    let password = "HWbnm25GwSj6jiHTEDMTO5D7aBw=";
    let realm = "webrtc.rs";

    let expected_key = vec![
        56, 22, 47, 139, 198, 127, 13, 188, 171, 80, 23, 29, 195, 148, 216, 224,
    ];
    let actual_key = generate_auth_key(username, realm, password);
    assert_eq!(
        expected_key, actual_key,
        "Expected {:?}, got {:?}",
        expected_key, actual_key
    );

    Ok(())
}

#[tokio::test]
async fn test_new_long_term_auth_handler() -> Result<(), Error> {
    // env_logger::init();

    const SHARED_SECRET: &str = "HELLO_WORLD";

    // here, it should use static port, like "0.0.0.0:3478",
    // but, due to different test environment, let's fake it by using "0.0.0.0:0"
    // to auto assign a "static" port
    let conn = Arc::new(UdpSocket::bind("0.0.0.0:0").await?);
    let server_port = conn.local_addr()?.port();

    let server = Server::new(ServerConfig {
        conn_configs: vec![ConnConfig {
            conn,
            relay_addr_generator: Box::new(RelayAddressGeneratorStatic {
                relay_address: IpAddr::from_str("127.0.0.1")?,
                address: "0.0.0.0".to_owned(),
            }),
        }],
        realm: "webrtc.rs".to_owned(),
        auth_handler: Arc::new(Box::new(LongTermAuthHandler::new(
            SHARED_SECRET.to_string(),
        ))),
        channel_bind_timeout: Duration::from_secs(0),
    })
    .await?;

    let (username, password) =
        generate_long_term_credentials(SHARED_SECRET, Duration::from_secs(60))?;

    let conn = Arc::new(UdpSocket::bind("0.0.0.0:0").await?);

    let client = Client::new(ClientConfig {
        stun_serv_addr: format!("0.0.0.0:{}", server_port),
        turn_serv_addr: format!("0.0.0.0:{}", server_port),
        username,
        password,
        realm: "webrtc.rs".to_owned(),
        software: String::new(),
        rto_in_ms: 0,
        conn,
    })
    .await?;

    client.listen().await?;

    let _allocation = client.allocate().await?;

    client.close().await?;
    server.close()?;

    Ok(())
}
