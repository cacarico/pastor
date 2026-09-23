use pastor::herdr::transport::*;

#[tokio::test]
async fn command_transport_talks_to_fake_herdr() {
    let ep = Endpoint::Command {
        argv: vec![env!("CARGO_BIN_EXE_fake-herdr").to_string()],
    };
    let mut c = connect(&ep).await.ok().unwrap();
    assert_eq!(c.ping().await.unwrap().protocol, 22);
}
