//! Tests for the client. These tests are not intended to walk through all the API possibilites (as
//! that is taken care of in the API tests), but instead focus on entire user workflows

use std::convert::TryInto;
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};

use bindle::client::Client;
use bindle::testing;

use tokio::stream::StreamExt;

struct TestController {
    pub client: Client,
    server_handle: std::process::Child,
    // Keep a handle to the tempdir so it doesn't drop until the controller drops
    _tempdir: tempfile::TempDir,
}

impl TestController {
    async fn new() -> TestController {
        let build_result = tokio::task::spawn_blocking(|| {
            std::process::Command::new("cargo")
                .args(&["build", "--all-features"])
                .output()
        })
        .await
        .unwrap()
        .expect("unable to run build command");

        assert!(
            build_result.status.success(),
            "Error trying to build server {}",
            String::from_utf8(build_result.stderr).unwrap()
        );

        let tempdir = tempfile::tempdir().expect("unable to create tempdir");

        let address = format!("127.0.0.1:{}", get_random_port());

        let base_url = format!("http://{}/v1/", address);

        let server_handle = std::process::Command::new("cargo")
            .args(&[
                "run",
                "--features",
                "cli",
                "--bin",
                "bindle-server",
                "--",
                "-d",
                tempdir.path().to_string_lossy().to_string().as_str(),
                "-i",
                address.as_str(),
            ])
            .spawn()
            .expect("unable to start bindle server");

        // Give things some time to start up
        tokio::time::delay_for(std::time::Duration::from_secs(2)).await;

        let client = Client::new(&base_url).expect("unable to setup bindle client");
        TestController {
            client,
            server_handle,
            _tempdir: tempdir,
        }
    }
}

impl Drop for TestController {
    fn drop(&mut self) {
        // Not much we can do here if we error, so just ignore
        let _ = self.server_handle.kill();
    }
}

fn get_random_port() -> u16 {
    TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
        .expect("Unable to bind to check for port")
        .local_addr()
        .unwrap()
        .port()
}

#[tokio::test]
async fn test_successful() {
    // This first creates some invoices/parcels and then tries fetching them to see that they work.
    // Once we confirm that works, we test yank
    let controller = TestController::new().await;

    let scaffold = testing::Scaffold::load("valid_v1").await;

    let inv = controller
        .client
        .create_invoice(scaffold.invoice)
        .await
        .expect("unable to create invoice")
        .invoice;

    controller
        .client
        .get_invoice(&inv.bindle.id)
        .await
        .expect("Should be able to fetch newly created invoice");

    for parcel in scaffold.parcel_files.values() {
        controller
            .client
            .create_parcel(&inv.bindle.id, &parcel.sha, parcel.data.clone())
            .await
            .expect("Unable to create parcel");
    }

    // Now check that we can get all the parcels
    for parcel in scaffold.parcel_files.values() {
        let data = controller
            .client
            .get_parcel(&inv.bindle.id, &parcel.sha)
            .await
            .expect("unable to get parcel");
        let expected_len = parcel.data.len();
        assert_eq!(
            data.len(),
            expected_len,
            "Expected file to be {} bytes, got {} bytes",
            expected_len,
            data.len()
        );
    }

    controller
        .client
        .yank_invoice(&inv.bindle.id)
        .await
        .expect("unable to yank invoice");

    match controller.client.get_invoice(inv.bindle.id).await {
        Ok(_) => panic!("getting a yanked invoice should have errored"),
        Err(e) => {
            if !matches!(e, bindle::client::ClientError::InvoiceNotFound) {
                panic!("Expected an invoice not found error, got: {:?}", e)
            }
        }
    }
}

#[tokio::test]
async fn test_streaming_successful() {
    let controller = TestController::new().await;

    // Use raw paths instead of scaffolds so we can test the stream
    let root = std::env::var("CARGO_MANIFEST_DIR").expect("Unable to get project directory");
    let base = std::path::PathBuf::from(root).join("tests/scaffolds/valid_v1");

    let inv = controller
        .client
        .create_invoice_from_file(base.join("invoice.toml"))
        .await
        .expect("unable to create invoice")
        .invoice;

    controller
        .client
        .get_invoice(&inv.bindle.id)
        .await
        .expect("Should be able to fetch newly created invoice");

    // Load the label from disk
    let parcel_path = base.join("parcels/parcel.dat");
    let parcel_sha = inv.parcel.expect("Should have parcels in invoice")[0]
        .label
        .sha256
        .to_owned();
    controller
        .client
        .create_parcel_from_file(&inv.bindle.id, &parcel_sha, &parcel_path)
        .await
        .expect("Unable to create parcel");

    // Now check that we can get the parcel and read data from the stream
    let mut stream = controller
        .client
        .get_parcel_stream(&inv.bindle.id, &parcel_sha)
        .await
        .expect("unable to get parcel");

    let mut data = Vec::new();
    while let Some(res) = stream.next().await {
        let bytes = res.expect("Shouldn't get an error in stream");
        data.extend(bytes);
    }

    let on_disk_len = tokio::fs::metadata(parcel_path)
        .await
        .expect("Unable to get file info")
        .len() as usize;
    assert_eq!(
        data.len(),
        on_disk_len,
        "Expected file to be {} bytes, got {} bytes",
        on_disk_len,
        data.len()
    );
}

#[tokio::test]
async fn test_already_created() {
    let controller = TestController::new().await;

    let scaffold = testing::Scaffold::load("valid_v2").await;

    controller
        .client
        .create_invoice(scaffold.invoice.clone())
        .await
        .expect("Invoice creation should not error");

    // Upload parcels for this bindle
    for parcel in scaffold.parcel_files.values() {
        controller
            .client
            .create_parcel(
                &scaffold.invoice.bindle.id,
                &parcel.sha,
                parcel.data.clone(),
            )
            .await
            .expect("Unable to create parcel");
    }

    // Make sure we can create an invoice where all parcels already exist
    let mut other_inv = scaffold.invoice.clone();
    other_inv.bindle.id = "another.com/bindle/1.0.0".try_into().unwrap();
    controller
        .client
        .create_invoice(other_inv)
        .await
        .expect("invoice creation should not error");
}

#[tokio::test]
async fn test_missing() {
    let controller = TestController::new().await;

    // Create a bindle with missing invoices
    let scaffold = testing::Scaffold::load("lotsa_parcels").await;

    let inv = controller
        .client
        .create_invoice(scaffold.invoice)
        .await
        .expect("unable to create invoice")
        .invoice;

    // Check we get the right amount of missing parcels
    let missing = controller
        .client
        .get_missing_parcels(&inv.bindle.id)
        .await
        .expect("Should be able to fetch list of missing parcels");
    assert_eq!(
        missing.len(),
        scaffold.parcel_files.len(),
        "Expected {} missing parcels, found {}",
        scaffold.parcel_files.len(),
        missing.len()
    );

    // Yank the invoice
    controller
        .client
        .yank_invoice(&inv.bindle.id)
        .await
        .expect("unable to yank invoice");

    // Make sure we can't get missing
    match controller.client.get_missing_parcels(&inv.bindle.id).await {
        Ok(_) => panic!("getting a yanked invoice should have errored"),
        Err(e) => {
            if !matches!(e, bindle::client::ClientError::InvoiceNotFound) {
                panic!("Expected an invoice not found error, got: {:?}", e)
            }
        }
    }
}
