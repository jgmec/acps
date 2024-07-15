use anyhow::Result;
use prost::Message;

use crate::protobuf::{AppleWLoc, WifiDevice};

pub async fn fetch(bssid: String) -> Result<Vec<WifiDevice>> {
    let x = AppleWLoc {
        wifi_devices: vec![WifiDevice {
            bssid,
            location: None,
        }],
        ..Default::default()
    };

    let mut payload = Vec::new();

    // header
    payload.extend([0x00, 0x01, 0x00, 0x05]);
    payload.extend("en_US".as_bytes());
    payload.extend([0x00, 0x13]);
    payload.extend("com.apple.locationd".as_bytes());
    payload.extend([0x00, 0x0a]);
    payload.extend("17.5.21F79".as_bytes());
    payload.extend([0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00]);

    // request
    x.encode_length_delimited(&mut payload)?;
    x.encode(&mut payload)?;

    let client = reqwest::Client::new();
    let resp = client
        .post("https://gs-loc.apple.com/clls/wloc")
        .header("content-type", "application/x-www-form-urlencoded")
        .header("accept", "*/*")
        .header("accept-charset", "utf-8")
        .header("accept-language", "en-us")
        .header(
            "user-agent",
            "locationd/1753.17 CFNetwork/711.1.12 Darwin/14.0.0",
        )
        .body(payload)
        .send()
        .await?
        .error_for_status()?;

    let x = AppleWLoc::decode(&resp.bytes().await?[10..])?;
    Ok(x.wifi_devices)
}
